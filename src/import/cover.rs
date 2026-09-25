//! 画像を artwork の置き場に置く共通処理（`POST /api/artwork/upload` / `from-caa`、D-86）と、
//! CD の取り込みの表の画像を Cover Art Archive から一度だけ取る処理（D-91）。
//!
//! 取得は inbox ジョブの走査の後に行う（承認画面を開くたび・`GET /api/inbox` のたびには外へ出ない）。
//! 対象は承認前（pending / failed）で画像がまだ無く、試行が上限（[`CAA_MAX_TRIES`]）未満の取り込み。
//! サイドカーの `rip.metadata.release_id` があるものだけ取りに行き、無いもの（CD でない・候補を選ばずに
//! 吸い出した）と画像の無い盤（404）は 1 回で打ち止めにする。上流の失敗は回数を 1 つ進め、次の走査で
//! もう 1 回だけ試す。どれも取り込み自体は失敗させない

use std::sync::Arc;

use crate::cd::coverart::CoverArtClient;
use crate::db::artwork as dbart;
use crate::db::inbox::{self as dbinbox, Item, CAA_MAX_TRIES};
use crate::db::Db;
use crate::domain::relpath::RelPath;
use crate::fsroot::RootDir;
use crate::import::sidecar::Sidecar;
use crate::jobs::handlers::thumbnail::new_thumbnail_job;
use crate::jobs::Jobs;
use crate::media::artwork::{sniff, ArtworkStore};

/// 埋め込みに使う形式（[`sniff`] が読める形式のうち、lofty で書けて主要プレイヤーが表示するもの）
pub const EMBED_MIMES: [&str; 3] = ["image/jpeg", "image/png", "image/webp"];

/// 置いた画像
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredImage {
    pub hash: [u8; 32],
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
    /// 投入した thumbnail ジョブ（既にサムネイルがあれば None）
    pub thumbnail_job: Option<i64>,
}

impl StoredImage {
    /// 下書きの `picture` の値（`<mime>:<sha256hex>`）
    pub fn picture_value(&self) -> String {
        format!("{}:{}", self.mime, ArtworkStore::hex(&self.hash))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreImageError {
    #[error("JPEG / PNG / WebP の画像だけを受け付ける")]
    Unsupported,
    #[error("原画像を置けない: {0}")]
    Io(String),
    #[error(transparent)]
    Db(#[from] crate::db::DbError),
}

/// 画像のバイト列を store と `artwork` 行に置き、サムネイルが無ければ thumbnail ジョブを投入する。
/// 同じ画像は 1 回だけ置かれる。ジョブの通知（`Jobs::notify_enqueued`）は呼び出し側が行う
pub async fn store_image(
    db: &Db,
    store: &Arc<ArtworkStore>,
    body: Vec<u8>,
) -> Result<StoredImage, StoreImageError> {
    let info = sniff(&body)
        .filter(|i| EMBED_MIMES.contains(&i.mime))
        .ok_or(StoreImageError::Unsupported)?;
    let bytes = body.len();
    let hash = ArtworkStore::hash_of(&body);
    {
        // 既にある画像でも put_original が dir の mtime を今にするので、GC 区分 E の 24 時間の猶予は
        // この時点から数え直される（参照が付くまでの窓を守る）
        let store = Arc::clone(store);
        tokio::task::spawn_blocking(move || store.put_original(&hash, info.mime, &body))
            .await
            .map_err(|e| StoreImageError::Io(e.to_string()))?
            .map_err(|e| StoreImageError::Io(e.to_string()))?;
    }
    let needs = !store.missing_thumbs(&hash).is_empty();
    let thumbnail_job = db
        .write(move |c| {
            let tx = c.transaction()?;
            let id = dbart::upsert(
                &tx,
                &hash,
                info.mime,
                Some(info.width),
                Some(info.height),
                bytes,
                "embedded",
            )?;
            let job = if needs {
                Some(
                    crate::db::jobs::enqueue(&tx, &new_thumbnail_job(id), crate::db::now_epoch())?
                        .id(),
                )
            } else {
                None
            };
            tx.commit()?;
            Ok(job)
        })
        .await?;
    Ok(StoredImage {
        hash,
        mime: info.mime,
        width: info.width,
        height: info.height,
        bytes,
        thumbnail_job,
    })
}

/// [`fetch_cd_covers`] の集計
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CoverReport {
    /// 画像を置いた件
    pub found: usize,
    /// 画像の無い盤（404）
    pub absent: usize,
    /// CD の吸い出しでない・リリースが決まっていない（取りに行かない）
    pub skipped: usize,
    /// 上流の失敗・置けなかった（回数を進めた）
    pub failed: usize,
}

/// 件のサイドカーの `rip.metadata.release_id`（MBID を小文字にしたもの）。無ければ None
fn rip_release_id(inbox: &RootDir, item: &Item) -> Option<String> {
    if item.rel_dir.is_empty() {
        return None;
    }
    let dir = RelPath::parse(&item.rel_dir).ok()?;
    let sidecar = match Sidecar::read(inbox, &dir) {
        Ok(s) => s?,
        Err(e) => {
            tracing::warn!(item_id = item.id, error = %e, "サイドカーを読めないので表の画像を取りに行かない");
            return None;
        }
    };
    let id = sidecar.rip?.metadata.release_id?;
    let id = id.trim().to_ascii_lowercase();
    crate::import::inbox::is_mbid(&id).then_some(id)
}

/// 承認前の CD の取り込みの表の画像を Cover Art Archive から取り、置いて件に記録する（D-91）。
/// 失敗は件ごとにログへ出して続ける（取り込みも走査も失敗させない）。`cancel` が立てば途中で止める
pub async fn fetch_cd_covers(
    db: &Db,
    inbox: &Arc<RootDir>,
    store: &Arc<ArtworkStore>,
    client: &CoverArtClient,
    jobs: &Jobs,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<CoverReport, crate::db::DbError> {
    let items = db.read(dbinbox::caa_candidates).await?;
    let mut report = CoverReport::default();
    for item in items {
        if cancel.is_cancelled() {
            break;
        }
        let id = item.id;
        let release_id = {
            let (inbox, item) = (Arc::clone(inbox), item.clone());
            tokio::task::spawn_blocking(move || rip_release_id(&inbox, &item))
                .await
                .ok()
                .flatten()
        };
        let Some(release_id) = release_id else {
            db.write(move |c| dbinbox::record_caa(c, id, None, CAA_MAX_TRIES))
                .await?;
            report.skipped += 1;
            continue;
        };
        let tries = item.caa_tries + 1;
        match client.front(&release_id).await {
            Ok(Some((_content_type, bytes))) => match store_image(db, store, bytes).await {
                Ok(img) => {
                    let value = img.picture_value();
                    let recorded = {
                        let value = value.clone();
                        db.write(move |c| dbinbox::record_caa(c, id, Some(&value), tries))
                            .await?
                    };
                    if let Some(job) = img.thumbnail_job {
                        jobs.notify_enqueued(&[job]).await;
                    }
                    if recorded {
                        tracing::info!(item_id = id, release_id, picture = %value, "取り込みの表の画像を Cover Art Archive から取った");
                        report.found += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!(item_id = id, release_id, error = %e, "取り込みの表の画像を置けない");
                    db.write(move |c| dbinbox::record_caa(c, id, None, tries))
                        .await?;
                    report.failed += 1;
                }
            },
            Ok(None) => {
                tracing::info!(
                    item_id = id,
                    release_id,
                    "Cover Art Archive にこの盤の表の画像が無い"
                );
                db.write(move |c| dbinbox::record_caa(c, id, None, CAA_MAX_TRIES))
                    .await?;
                report.absent += 1;
            }
            Err(e) => {
                let detail = crate::cd::error_chain(&e);
                tracing::warn!(item_id = id, release_id, tries, error = %detail, "取り込みの表の画像を取れない（次の走査で上限まで試す）");
                db.write(move |c| dbinbox::record_caa(c, id, None, tries))
                    .await?;
                report.failed += 1;
            }
        }
    }
    Ok(report)
}

//! 画像を artwork の置き場に置く共通処理（`POST /api/artwork/upload` / `from-caa`、D-86）と、
//! CD の取り込みの表の画像を Cover Art Archive から一度だけ取る処理（D-91）。
//!
//! 取得は inbox ジョブの走査の後に行う（承認画面を開くたび・`GET /api/inbox` のたびには外へ出ない）。
//! 対象は承認前（pending / failed）で画像がまだ無く、試行が上限（[`CAA_MAX_TRIES`]）未満の取り込み。
//! サイドカーの `rip.metadata.release_id` があるものだけ取りに行き、無いもの（CD でない・候補を選ばずに
//! 吸い出した）と画像の無い盤（404）は 1 回で打ち止めにする。上流の失敗は次の走査でもう 1 回だけ試す。
//! 回数は外へ出る前に claim して永続化する（途中で落ちても上限を超えない）。配置の後に、1 回の実行で
//! [`CAA_PER_RUN`] 件・[`CAA_RUN_BUDGET`] までに限って行う（Inbox の配置を外部の障害で待たせない）。
//! どれも取り込み自体は失敗させない

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

/// 件のサイドカーの `rip.metadata.release_id` と `release_group_id`（MBID を小文字にしたもの）。
/// リリースが無ければ None（グループだけでは取りに行かない）。グループは形が MBID でなければ無いものとする
fn rip_release_ids(inbox: &RootDir, item: &Item) -> Option<(String, Option<String>)> {
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
    let metadata = sidecar.rip?.metadata;
    let normalize = |id: String| {
        let id = id.trim().to_ascii_lowercase();
        crate::import::inbox::is_mbid(&id).then_some(id)
    };
    let release = normalize(metadata.release_id?)?;
    Some((release, metadata.release_group_id.and_then(normalize)))
}

/// リリースの表の画像を取り、無ければ（404）リリースグループの表の画像を 1 度だけ試す（D-93）。
/// 通常盤に画像が無くても、同じグループの初回限定盤や BD 付きの版にあることが多い。
/// 取れたら出どころ（ログ用に `release` / `release-group`）と本文を返す
async fn fetch_front(
    client: &CoverArtClient,
    release_id: &str,
    group_id: Option<&str>,
) -> Result<Option<(&'static str, Vec<u8>)>, crate::cd::LookupError> {
    if let Some((_content_type, bytes)) = client.front(release_id).await? {
        return Ok(Some(("release", bytes)));
    }
    let Some(group_id) = group_id else {
        return Ok(None);
    };
    Ok(client
        .group_front(group_id)
        .await?
        .map(|(_content_type, bytes)| ("release-group", bytes)))
}

/// 1 回の inbox ジョブで表の画像を取りに行く件数の上限（D-91）。残りは次の周回で続ける（`needs_attention`）
pub const CAA_PER_RUN: usize = 4;

/// 1 回の inbox ジョブで表の画像の取得に使う時間の上限（D-91）。これを過ぎたら新しい件に手を付けない
/// （走行中の 1 件は `CoverArtClient` の全体 timeout で終わる）
pub const CAA_RUN_BUDGET: std::time::Duration = std::time::Duration::from_secs(90);

/// 承認前の CD の取り込みの表の画像を Cover Art Archive から取り、置いて件に記録する（D-91）。
/// リリースに無ければリリースグループの表の画像を同じ回で試す（D-93。回数は 1 回と数える）。
/// 1 回に [`CAA_PER_RUN`] 件・[`CAA_RUN_BUDGET`] まで。失敗は件ごとにログへ出して続ける（取り込みも走査も
/// 失敗させない）。**外へ出る前に回数を claim して永続化する**（通信の途中で落ちても上限を超えない）。
/// `cancel` は通信の待ちの間も見る（立てば取りやめ、claim した回数はそのまま = 1 回と数える）
pub async fn fetch_cd_covers(
    db: &Db,
    inbox: &Arc<RootDir>,
    store: &Arc<ArtworkStore>,
    client: &CoverArtClient,
    jobs: &Jobs,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<CoverReport, crate::db::DbError> {
    let started = std::time::Instant::now();
    let items = db.read(dbinbox::caa_candidates).await?;
    let mut report = CoverReport::default();
    let mut attempted = 0usize;
    for item in items {
        if cancel.is_cancelled() || attempted >= CAA_PER_RUN || started.elapsed() >= CAA_RUN_BUDGET
        {
            break;
        }
        let id = item.id;
        let ids = {
            let (inbox, item) = (Arc::clone(inbox), item.clone());
            tokio::task::spawn_blocking(move || rip_release_ids(&inbox, &item))
                .await
                .ok()
                .flatten()
        };
        let Some((release_id, group_id)) = ids else {
            // 外へ出ないので claim は要らない
            db.write(move |c| dbinbox::record_caa(c, id, None, CAA_MAX_TRIES))
                .await?;
            report.skipped += 1;
            continue;
        };
        // 1 回分を先に claim（CAS）。取れなければ別の実行が進めた・状態が変わった
        let expected = item.caa_tries;
        let Some(tries) = db
            .write(move |c| dbinbox::claim_caa(c, id, expected))
            .await?
        else {
            continue;
        };
        attempted += 1;
        let fetched = tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!(item_id = id, release_id, "取り消されたので表の画像の取得をやめる");
                break;
            }
            r = fetch_front(client, &release_id, group_id.as_deref()) => r,
        };
        match fetched {
            Ok(Some((source, bytes))) => match store_image(db, store, bytes).await {
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
                        tracing::info!(item_id = id, release_id, group_id, source, picture = %value, "取り込みの表の画像を Cover Art Archive から取った");
                        report.found += 1;
                    }
                }
                Err(e) => {
                    // 回数は claim で進めてある
                    tracing::warn!(item_id = id, release_id, tries, error = %e, "取り込みの表の画像を置けない");
                    report.failed += 1;
                }
            },
            Ok(None) => {
                tracing::info!(
                    item_id = id,
                    release_id,
                    group_id,
                    "Cover Art Archive にこの盤（とリリースグループ）の表の画像が無い"
                );
                db.write(move |c| dbinbox::record_caa(c, id, None, CAA_MAX_TRIES))
                    .await?;
                report.absent += 1;
            }
            Err(e) => {
                // 回数は claim で進めてある。上限未満なら次の周回でもう 1 回
                let detail = crate::cd::error_chain(&e);
                tracing::warn!(item_id = id, release_id, tries, error = %detail, "取り込みの表の画像を取れない（次の走査で上限まで試す）");
                report.failed += 1;
            }
        }
    }
    Ok(report)
}

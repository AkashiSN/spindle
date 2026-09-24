//! 埋め込み画像の差し替え（P1-3 書き側、SPEC §7.5、D-60）。
//!
//! 画像はトラックへの埋め込みに統一する（spindle は同梱の cover ファイルを書かない）。差し替えは
//! `kind = 'tags'` の op で、キー `PICTURE`（値は `tag_hash` と同じ `<mime>:<sha256hex>` の配列）を
//! 「全画像を捨てて、上げた 1 枚を front cover にする」変更として記録する。`track_tags` は既に
//! この形で `PICTURE` を持つので、旧値・overlay・`tag_version`・差分・書き戻し確認・巻き戻しは
//! 通常のタグ編集と同じ機構がそのまま効く。
//!
//! 画像の実体は [`ArtworkStore`]（`thumbs/<hex>/orig.<ext>`）と `artwork` 行で持つ。新画像は
//! アップロード（`POST /api/artwork/upload`）が置き、tagwrite は書く前に捨てる旧画像を同じ store へ
//! 退避する（[`super::mod`] の `stage_tags`）。

use std::collections::BTreeMap;
use std::sync::Arc;

use lofty::picture::{MimeType, Picture, PictureType};
use rusqlite::Connection;
use serde::Serialize;

use crate::db::artwork as dbart;
use crate::db::history;
use crate::db::now_epoch;
use crate::db::replaygain as dbrg;
use crate::domain::tags::TagSet;
use crate::jobs::{BatchEvent, Event};
use crate::media::artwork::{sniff, ArtworkStore};

use super::{prepare_tags_in, with_replaced, EditError, Editor, PlanTarget};

/// edits のキー（`TagSet` の擬似キーと同じ）
pub const PICTURE_KEY: &str = "PICTURE";

/// `prepare_picture` の結果
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PicturePrepared {
    /// 記録したバッチ。書く行が無ければ None
    pub batch_id: Option<i64>,
    /// 記録した op 数
    pub affected: usize,
    /// 既にその 1 枚だけを持っていて op にならなかった行数
    pub unchanged: usize,
    /// missing で対象外の行数（ファイルが無いので書けない）
    pub missing: usize,
    pub job_ids: Vec<i64>,
    #[serde(skip)]
    pub event: Option<BatchEvent>,
}

/// `PICTURE` の値 `<mime>:<sha256hex>` を分解する
pub fn parse_picture_value(value: &str) -> Option<(&str, [u8; 32])> {
    let (mime, hex) = value.rsplit_once(':')?;
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some((mime, out))
}

/// `PICTURE` の値を組み立てる
pub fn picture_value(mime: &str, sha256: &[u8]) -> String {
    format!("{mime}:{}", ArtworkStore::hex(sha256))
}

/// store から `PICTURE` の値に対応する画像を front cover として読む。無ければ `None`。
/// 上限は設けない（退避した旧画像はアップロードの上限に縛られず、巻き戻しには全体が要る）
pub(crate) fn load_picture(store: &ArtworkStore, value: &str) -> std::io::Result<Option<Picture>> {
    let Some((mime, hash)) = parse_picture_value(value) else {
        return Ok(None);
    };
    let Some(bytes) = store.read_original(&hash, mime)? else {
        return Ok(None);
    };
    Ok(Some(
        Picture::unchecked(bytes)
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::from_str(mime))
            .build(),
    ))
}

/// 退避した旧画像（`artwork` 行にする分）
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StashedPicture {
    pub sha256: [u8; 32],
    pub mime: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bytes: usize,
}

/// 捨てる画像を store へ退避する（ハッシュアドレスなので同じ画像は 1 回）。1 枚でも置けなければ
/// エラー（何も書かずに op を failed にする）
pub(super) fn stash_pictures(
    store: &ArtworkStore,
    pictures: &[Picture],
) -> std::io::Result<Vec<StashedPicture>> {
    let mut out = Vec::with_capacity(pictures.len());
    for pic in pictures {
        let mime = pic.mime_type().map(|m| m.as_str()).unwrap_or("").to_owned();
        let hash = ArtworkStore::hash_of(pic.data());
        store.put_original(&hash, &mime, pic.data())?;
        let info = sniff(pic.data());
        out.push(StashedPicture {
            sha256: hash,
            mime,
            width: info.map(|i| i.width),
            height: info.map(|i| i.height),
            bytes: pic.data().len(),
        });
    }
    Ok(out)
}

pub(super) fn upsert_stashed(
    conn: &Connection,
    stashed: &[StashedPicture],
) -> crate::db::Result<()> {
    for s in stashed {
        dbart::upsert(
            conn, &s.sha256, &s.mime, s.width, s.height, s.bytes, "embedded",
        )?;
    }
    Ok(())
}

impl Editor {
    /// アートワークのキャッシュを付ける（画像の差し替えに必須。無ければ `PICTURE` の op は
    /// 記録できず、反映も failed になる）
    pub fn with_artwork(mut self, store: Arc<ArtworkStore>) -> Self {
        self.artwork = Some(store);
        self
    }

    pub fn artwork_store(&self) -> Option<&Arc<ArtworkStore>> {
        self.artwork.as_ref()
    }

    /// `track_ids` の埋め込み画像を `sha256` の 1 枚（front cover）に差し替える編集バッチを記録する
    /// （D-60）。通常の tags バッチと同じ機構に乗る（旧値の記録、overlay、track 単位の tagwrite、
    /// 巻き戻し）。
    ///
    /// - `sha256` の `artwork` 行と原画像が無ければ [`EditError::ArtworkNotFound`]
    /// - missing の行は対象外（`missing`）。既にその 1 枚だけを持つ行は op にしない（`unchanged`）。
    ///   op になる行が無ければ [`EditError::NoChanges`]
    /// - 対象トラックに pending の op があれば何も記録せず [`EditError::Pending`]
    pub async fn prepare_picture(
        &self,
        description: Option<&str>,
        track_ids: Vec<i64>,
        sha256: [u8; 32],
    ) -> Result<PicturePrepared, EditError> {
        let store = self.artwork.clone().ok_or(EditError::ArtworkUnavailable)?;
        let description = description.map(str::to_owned);
        let prepared = self
            .db
            .write(move |c| {
                Ok(prepare_picture_tx(
                    c,
                    &store,
                    description.as_deref(),
                    &track_ids,
                    sha256,
                    now_epoch(),
                ))
            })
            .await??;
        self.jobs.notify_enqueued(&prepared.job_ids).await;
        if let Some(ev) = &prepared.event {
            self.jobs.publish(Event::Batch(ev.clone()));
        }
        Ok(prepared)
    }
}

/// [`Editor::prepare_picture`] のトランザクション本体
fn prepare_picture_tx(
    conn: &mut Connection,
    store: &ArtworkStore,
    description: Option<&str>,
    track_ids: &[i64],
    sha256: [u8; 32],
    now: i64,
) -> Result<PicturePrepared, EditError> {
    let tx = conn.transaction()?;
    let art = dbart::get_by_sha256(&tx, &sha256)?.ok_or(EditError::ArtworkNotFound)?;
    if !store.has_original(&sha256, &art.mime) {
        return Err(EditError::ArtworkNotFound);
    }
    let value = picture_value(&art.mime, &sha256);

    let mut ids: Vec<i64> = track_ids.to_vec();
    ids.sort_unstable();
    if let Some(w) = ids.windows(2).find(|w| w[0] == w[1]) {
        return Err(EditError::DuplicateTrack(w[0]));
    }
    let pending = history::pending_track_ids(&tx, &ids)?;
    if !pending.is_empty() {
        return Err(EditError::Pending { track_ids: pending });
    }
    // missing の判定は RG 書き込みと同じ行取得を使う（存在しない id はここで分かる）
    let rows = dbrg::write_rows(&tx, &ids)?;
    if let Some(unknown) = ids.iter().find(|id| !rows.iter().any(|r| r.id == **id)) {
        return Err(EditError::TrackNotFound(*unknown));
    }
    let mut missing = 0usize;
    let mut targets: BTreeMap<i64, ()> = BTreeMap::new();
    for row in &rows {
        if row.missing {
            missing += 1;
            continue;
        }
        targets.insert(row.id, ());
    }
    let targets: Vec<PlanTarget> = targets
        .keys()
        .copied()
        .enumerate()
        .map(|(index, track_id)| PlanTarget {
            track_id,
            expected_tag_version: None,
            index,
            expected: None,
            conflict: None,
        })
        .collect();
    let mut prepared = PicturePrepared {
        batch_id: None,
        affected: 0,
        unchanged: 0,
        missing,
        job_ids: Vec::new(),
        event: None,
    };
    if targets.is_empty() {
        return Err(EditError::NoChanges);
    }
    let eval = move |_t: &PlanTarget, current: &TagSet| -> Result<TagSet, String> {
        Ok(with_replaced(
            current,
            [(PICTURE_KEY.to_owned(), vec![value.clone()])],
        ))
    };
    let p = prepare_tags_in(&tx, description, &targets, &eval, None, now)?;
    prepared.batch_id = Some(p.batch_id);
    prepared.affected = p.affected;
    prepared.unchanged = p.unchanged;
    prepared.job_ids = p.job_ids;
    prepared.event = p.event;
    tx.commit()?;
    tracing::info!(
        batch_id = ?prepared.batch_id,
        affected = prepared.affected,
        unchanged = prepared.unchanged,
        missing,
        sha256 = %ArtworkStore::hex(&sha256),
        "埋め込み画像の差し替えバッチを記録した"
    );
    Ok(prepared)
}

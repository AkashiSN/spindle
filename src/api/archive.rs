//! `GET /api/archive`（設定画面 SPEC §12.6、P1-12、D-58）。`archived_files` の台帳を新しい順に返す。
//! 退避した op の `batch_id` を添える（復元はそのバッチを履歴から巻き戻す。SPEC §7.4）。
//! 読むだけで Archive のファイルには触らない

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::db::archive::{self, ArchivedFile};

use super::error::ApiError;
use super::AppState;

#[derive(Serialize)]
pub struct ArchivedEntry {
    #[serde(flatten)]
    pub file: ArchivedFile,
    /// 退避した op のバッチ。op が消えていれば null
    pub batch_id: Option<i64>,
}

#[derive(Serialize)]
pub struct ArchiveList {
    pub items: Vec<ArchivedEntry>,
}

pub async fn list(State(state): State<AppState>) -> Result<Json<ArchiveList>, ApiError> {
    let items = state
        .db
        .read(|c| {
            archive::list_with_batch(c).map(|rows| {
                rows.into_iter()
                    .map(|(file, batch_id)| ArchivedEntry { file, batch_id })
                    .collect()
            })
        })
        .await?;
    Ok(Json(ArchiveList { items }))
}

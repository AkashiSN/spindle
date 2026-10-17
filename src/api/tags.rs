//! `GET /api/tags/keys`（SPEC §9、D-113）。ライブラリにあるタグのキーと曲数。タグのキーの入力欄
//! （Inbox のタグの追加・プロパティのフィールドの追加・一括編集）が前方一致の候補に使う

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::db::tracks::{self, TagKeyUsage};

use super::error::ApiError;
use super::AppState;

#[derive(Serialize)]
pub struct TagKeyList {
    pub items: Vec<TagKeyUsage>,
}

pub async fn keys(State(state): State<AppState>) -> Result<Json<TagKeyList>, ApiError> {
    let items = state.db.read(tracks::tag_keys).await?;
    Ok(Json(TagKeyList { items }))
}

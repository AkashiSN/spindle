//! `GET /api/archive`（記録画面の「退避ファイル」タブ。SPEC §12.4、P1-12、D-58、D-106）。`archived_files` の
//! 台帳を状態で絞って新しい順に 1 ページずつ返し、状態ごとの件数を添える。退避した op の `batch_id` を
//! 添える（復元はそのバッチを履歴から巻き戻す。SPEC §7.4）。読むだけで Archive のファイルには触らない

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db::archive::{self, ArchiveCounts, ArchiveState, ArchivedFile};

use super::error::{error_response, ApiError};
use super::AppState;

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;

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
    /// 続きの `cursor`。無ければ null
    pub next_cursor: Option<i64>,
    pub counts: ArchiveCounts,
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub state: Option<String>,
    pub limit: Option<usize>,
    pub cursor: Option<i64>,
}

pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, ApiError> {
    // 省略は保持中（見るべきものだけ。削除済みは GC で実体を消した記録）
    let filter = match q.state.as_deref().unwrap_or("held") {
        "all" => None,
        s => match ArchiveState::parse(s) {
            Some(st) => Some(st),
            None => return Ok(error_response(StatusCode::BAD_REQUEST, "bad_request")),
        },
    };
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Ok(error_response(StatusCode::BAD_REQUEST, "bad_request"));
    }
    let cursor = q.cursor;
    let (mut rows, counts) = state
        .db
        .read(move |c| {
            Ok((
                archive::list_page(c, filter, limit + 1, cursor)?,
                archive::counts(c)?,
            ))
        })
        .await?;
    // 1 件多く取って続きの有無を見る（limit ちょうどで終わるページに空の続きを出さない）
    let next_cursor = if rows.len() > limit {
        rows.truncate(limit);
        rows.last().map(|(f, _)| f.id)
    } else {
        None
    };
    let items = rows
        .into_iter()
        .map(|(file, batch_id)| ArchivedEntry { file, batch_id })
        .collect();
    Ok(Json(ArchiveList {
        items,
        next_cursor,
        counts,
    })
    .into_response())
}

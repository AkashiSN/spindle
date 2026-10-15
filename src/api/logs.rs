//! `GET /api/logs`・`GET /api/logs/{key}/text`（記録画面の「ログ」タブ。SPEC §9、§12.4、D-106）。
//! ログはファイルのまま残し、画面は API で読む。照合ログは `data/verify/<album_id>.log`（album ごとに最新
//! 1 回分）、吸い出しログはいまの曲のディレクトリの `rip.log` / `rip<N>.log`

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db::logs::{self, LogRun};

use super::error::{error_response, ApiError};
use super::AppState;

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;

#[derive(Deserialize)]
pub struct ListQuery {
    pub kind: Option<String>,
    pub album_id: Option<i64>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

#[derive(Serialize)]
pub struct LogList {
    pub items: Vec<LogRun>,
    /// 続きの `cursor`（不透明。中身は読み飛ばす件数）。無ければ null
    pub next_cursor: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, ApiError> {
    let source: Option<&'static str> = match q.kind.as_deref().unwrap_or("all") {
        "all" => None,
        "retro" => Some("retro"),
        "rip" => Some("rip"),
        _ => return Ok(error_response(StatusCode::BAD_REQUEST, "bad_request")),
    };
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Ok(error_response(StatusCode::BAD_REQUEST, "bad_request"));
    }
    let offset = match q.cursor.as_deref().map(str::parse::<usize>) {
        None => 0,
        Some(Ok(n)) => n,
        Some(Err(_)) => return Ok(error_response(StatusCode::BAD_REQUEST, "bad_request")),
    };
    let album_id = q.album_id;
    let mut items = state
        .db
        .read(move |c| logs::list_runs(c, source, album_id, limit + 1, offset))
        .await?;
    let next_cursor = (items.len() > limit).then(|| {
        items.truncate(limit);
        (offset + limit).to_string()
    });
    Ok(Json(LogList { items, next_cursor }).into_response())
}

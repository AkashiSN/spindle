//! `GET /api/logs`・`GET /api/logs/{key}/text`（記録画面の「ログ」タブ。SPEC §9、§12.4、D-106）。
//! ログはファイルのまま残し、画面は API で読む。照合ログは `data/verify/<album_id>.log`（album ごとに最新
//! 1 回分）、吸い出しログはいまの曲のディレクトリの `rip.log` / `rip<N>.log`

use std::io::Read as _;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::cd::riplog::is_rip_log_name;
use crate::db::logs::{self, LogAvail, LogRun, RunKey};
use crate::domain::relpath::RelPath;
use crate::fsroot::{FileKind, FsError, RootDir};

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

/// ログ本文の上限（本番の照合ログの最大は 27 KB）
pub const LOG_TEXT_LIMIT: u64 = 1 << 20;

pub(crate) enum TextRead {
    Text(String),
    TooLarge,
    NotFound,
}

/// root の dirfd から `openat2` で開いて上限まで読む。UTF-8 でなければ置換文字で返す
pub(crate) async fn read_text_capped(
    root: Arc<RootDir>,
    rel: RelPath,
) -> Result<TextRead, ApiError> {
    tokio::task::spawn_blocking(move || -> Result<TextRead, ApiError> {
        // O_NONBLOCK: FIFO に差し替わっていても止まらず、fstat の種別で弾く
        let file = match root.open_file_nonblocking(&rel) {
            Ok(f) => f,
            Err(FsError::NotFound | FsError::Symlink | FsError::Escaped) => {
                return Ok(TextRead::NotFound)
            }
            Err(e) => return Err(ApiError::Internal(format!("ログを開けない: {e}"))),
        };
        let st = crate::fsroot::fstat(&file)
            .map_err(|e| ApiError::Internal(format!("ログを調べられない: {e}")))?;
        if st.kind != FileKind::File {
            return Ok(TextRead::NotFound);
        }
        let mut buf = Vec::new();
        file.take(LOG_TEXT_LIMIT + 1)
            .read_to_end(&mut buf)
            .map_err(|e| ApiError::Internal(format!("ログを読めない: {e}")))?;
        if buf.len() as u64 > LOG_TEXT_LIMIT {
            return Ok(TextRead::TooLarge);
        }
        Ok(TextRead::Text(String::from_utf8_lossy(&buf).into_owned()))
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?
}

pub(crate) fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "log_not_found")
}

pub(crate) fn text_response(r: TextRead) -> Response {
    match r {
        TextRead::Text(s) => {
            ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], s).into_response()
        }
        TextRead::TooLarge => error_response(StatusCode::PAYLOAD_TOO_LARGE, "log_too_large"),
        TextRead::NotFound => not_found(),
    }
}

#[derive(Deserialize)]
pub struct TextQuery {
    pub disc: Option<i64>,
}

pub async fn text(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(q): Query<TextQuery>,
) -> Result<Response, ApiError> {
    let Some(key) = RunKey::parse(&key) else {
        return Ok(error_response(StatusCode::BAD_REQUEST, "bad_request"));
    };
    let k = key.clone();
    let (rows, avail) = state
        .db
        .read(move |c| {
            let rows = logs::run_rows(c, &k)?;
            let avail = logs::availability(c, &k, &rows)?;
            Ok((rows, avail))
        })
        .await?;
    if rows.is_empty() || avail != LogAvail::Latest {
        return Ok(not_found());
    }
    if key.source == "retro" {
        let Some(root) = state.verify_logs.clone() else {
            return Ok(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "logs_unavailable",
            ));
        };
        // DB の log_path（絶対パス）は使わず、名前を組み立てて data/verify の dirfd から開く
        let Ok(rel) = RelPath::parse(&format!("{}.log", key.album_id)) else {
            return Ok(not_found());
        };
        return Ok(text_response(read_text_capped(root, rel).await?));
    }
    // 吸い出し: log_path は配置時の Library 相対でリネームに追随しない。名前だけ使い、場所はいまの曲から引く
    let Some(row) = rows
        .iter()
        .filter(|r| r.log_path.is_some())
        .find(|r| q.disc.is_none() || r.disc_no == q.disc)
    else {
        return Ok(not_found());
    };
    let name = row
        .log_path
        .as_deref()
        .and_then(|p| p.rsplit('/').next())
        .unwrap_or_default()
        .to_owned();
    if !is_rip_log_name(&name) {
        return Ok(not_found());
    }
    let Some(root) = state.library.clone() else {
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "logs_unavailable",
        ));
    };
    let (album_id, disc_no) = (key.album_id, row.disc_no);
    let dir = state
        .db
        .read(move |c| logs::track_dir_for_disc(c, album_id, disc_no))
        .await?;
    let Some(rel) = dir
        .and_then(|d| RelPath::parse(&d).ok())
        .and_then(|d| d.join(&name).ok())
    else {
        return Ok(not_found());
    };
    Ok(text_response(read_text_capped(root, rel).await?))
}

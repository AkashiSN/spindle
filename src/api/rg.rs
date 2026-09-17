//! ReplayGain の API（SPEC §9、P1-1 / P1-2）。
//!
//! - `POST /api/rg { selection }`: selection に含まれる active なトラックの album ごとに `rg`
//!   ジョブを投入する（album を持たないトラックは track 単位）。解析は DB にしか書かないので
//!   preview 段階は無い。対象が無ければ 409 `no_changes`。既に queued / running の album は
//!   `duplicates` に数える
//! - `POST /api/rg/write { selection, description?, skip_pending? }`: selection の解析済み
//!   トラックへ解析値をタグとして書く編集バッチを記録する（`Editor::prepare_rg_write`。
//!   通常の tags バッチと同じく巻き戻せる）。値は DB から決まるので preview 段階は無い。
//!   反映待ちがあれば 409 `pending`、`skip_pending` で除外。書く行が無く既に一致した行も
//!   無ければ 409 `no_changes`。`[replaygain].write_tags = false` なら 409 `rg_write_disabled`

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db::{history, replaygain as dbrg, tracks};
use crate::domain::selection::SelectionBody;
use crate::edit::{EditError, Editor};
use crate::jobs::handlers::rg::{new_album_job, new_track_job};
use crate::jobs::EnqueueResult;

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;

#[derive(Debug, Deserialize)]
pub struct StartBody {
    pub selection: SelectionBody,
}

#[derive(Debug, Serialize)]
pub struct StartResponse {
    /// 投入した album 単位のジョブ数
    pub albums: usize,
    /// 投入した track 単位のジョブ数（album を持たないトラック）
    pub tracks: usize,
    /// 既に queued / running で投入しなかった数
    pub duplicates: usize,
    pub job_ids: Vec<i64>,
}

pub async fn start(
    State(state): State<AppState>,
    body: Result<Json<StartBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                e.body_text(),
            ))
        }
    };
    let sel = match body.selection.parse() {
        Ok(s) => s,
        Err(e) => {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                e.to_string(),
            ))
        }
    };
    let (albums, track_ids) = state
        .db
        .read(move |c| {
            let rows = tracks::resolve_selection(c, &sel)?;
            let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
            dbrg::scopes_of(c, &ids)
        })
        .await?;
    if albums.is_empty() && track_ids.is_empty() {
        return Ok(error_response(StatusCode::CONFLICT, "no_changes"));
    }
    let mut res = StartResponse {
        albums: 0,
        tracks: 0,
        duplicates: 0,
        job_ids: Vec::new(),
    };
    for id in albums {
        match state.jobs.enqueue(new_album_job(id)).await? {
            EnqueueResult::Inserted(job_id) => {
                res.albums += 1;
                res.job_ids.push(job_id);
            }
            EnqueueResult::Duplicate(_) => res.duplicates += 1,
        }
    }
    for id in track_ids {
        match state.jobs.enqueue(new_track_job(id)).await? {
            EnqueueResult::Inserted(job_id) => {
                res.tracks += 1;
                res.job_ids.push(job_id);
            }
            EnqueueResult::Duplicate(_) => res.duplicates += 1,
        }
    }
    Ok((StatusCode::ACCEPTED, Json(res)).into_response())
}

// ---------------------------------------------------------------- タグ書き込み（P1-2）

#[derive(Debug, Deserialize)]
pub struct WriteBody {
    pub selection: SelectionBody,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub skip_pending: bool,
}

#[derive(Debug, Serialize)]
pub struct WriteResponse {
    /// 記録したバッチ。書く行が無ければ null
    pub batch_id: Option<i64>,
    /// 記録した op 数
    pub affected: usize,
    /// ファイルが既に解析値を持っていたので `rg_written_at` だけ立てた行数
    pub unchanged: usize,
    /// 未解析で対象外の行数
    pub unscanned: usize,
    /// missing で対象外の行数
    pub missing: usize,
    /// `skip_pending` で除外した行数
    pub pending_excluded: usize,
}

/// 書き込み API が使える条件（設定で有効、Editor あり）。使えなければ返す応答
fn editor_of(state: &AppState) -> Result<Arc<Editor>, Box<Response>> {
    if !state.config.replaygain.write_tags {
        return Err(Box::new(error_response(
            StatusCode::CONFLICT,
            "rg_write_disabled",
        )));
    }
    state.editor.clone().ok_or_else(|| {
        Box::new(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "editor_unavailable",
        ))
    })
}

fn pending_response(track_ids: Vec<i64>) -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "error": "pending",
            "count": track_ids.len(),
            "track_ids": track_ids,
        })),
    )
        .into_response()
}

pub async fn write(
    State(state): State<AppState>,
    body: Result<Json<WriteBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                e.body_text(),
            ))
        }
    };
    let editor = match editor_of(&state) {
        Ok(e) => e,
        Err(r) => return Ok(*r),
    };
    let sel = match body.selection.parse() {
        Ok(s) => s,
        Err(e) => {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                e.to_string(),
            ))
        }
    };
    let (ids, pending) = state
        .db
        .read(move |c| {
            let tx = c.unchecked_transaction()?;
            let rows = tracks::resolve_selection(&tx, &sel)?;
            let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
            let pending = history::pending_track_ids(&tx, &ids)?;
            tx.finish()?;
            Ok((ids, pending))
        })
        .await?;
    if !pending.is_empty() && !body.skip_pending {
        return Ok(pending_response(pending));
    }
    let ids: Vec<i64> = ids
        .into_iter()
        .filter(|id| pending.binary_search(id).is_err())
        .collect();
    let pending_excluded = pending.len();
    match editor
        .prepare_rg_write(body.description.as_deref(), ids)
        .await
    {
        Ok(p) => {
            if p.batch_id.is_none() && p.unchanged == 0 {
                return Ok(error_response(StatusCode::CONFLICT, "no_changes"));
            }
            let status = if p.batch_id.is_some() {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            Ok((
                status,
                Json(WriteResponse {
                    batch_id: p.batch_id,
                    affected: p.affected,
                    unchanged: p.unchanged,
                    unscanned: p.unscanned,
                    missing: p.missing,
                    pending_excluded,
                }),
            )
                .into_response())
        }
        Err(EditError::Pending { track_ids }) => Ok(pending_response(track_ids)),
        Err(e) => Err(ApiError::Internal(e.to_string())),
    }
}

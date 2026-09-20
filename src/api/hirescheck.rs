//! `POST /api/hirescheck { selection }`（SPEC §9、P3-5、D-71）。selection の対象（可逆かつ
//! 48 kHz 超または 16 bit 超、active）を track 単位の `hirescheck` ジョブに投入する（flaccheck と同型。
//! DB にしか書かないので preview は無い）。対象が無ければ 409 `no_changes`、
//! 既に queued / running は `duplicates` に数える

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db::{hires as dbh, now_epoch, tracks};
use crate::domain::selection::SelectionBody;

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;

#[derive(Debug, Deserialize)]
pub struct StartBody {
    pub selection: SelectionBody,
}

#[derive(Debug, Serialize)]
pub struct StartResponse {
    /// 投入したジョブ数
    pub tracks: usize,
    /// 対象外（非可逆・44.1/48 kHz かつ 16 bit・missing）で飛ばした数
    pub skipped: usize,
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
    let (job_ids, skipped, duplicates) = state
        .db
        .write(move |c| {
            let rows = tracks::resolve_selection(c, &sel)?;
            let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
            dbh::enqueue_selection(c, &ids, now_epoch())
        })
        .await?;
    if job_ids.is_empty() && duplicates == 0 {
        return Ok(error_response(StatusCode::CONFLICT, "no_changes"));
    }
    state.jobs.notify_enqueued(&job_ids).await;
    Ok((
        StatusCode::ACCEPTED,
        Json(StartResponse {
            tracks: job_ids.len(),
            skipped,
            duplicates,
            job_ids,
        }),
    )
        .into_response())
}

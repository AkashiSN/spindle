//! `POST /api/verify { selection }`（SPEC §9、P2-9、D-13 / D-63）。selection のトラックが属する
//! アルバムを album 単位の `verify` ジョブ（遡及照合）に投入する。適格判定（形式・完全性）は
//! ジョブ側で行い、対象外は `unverifiable` として記録されるので、ここでは絞らない。
//! 対象が無ければ 409 `no_changes`、既に queued / running は `duplicates` に数える

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db::{now_epoch, tracks, verify as dbv};
use crate::domain::selection::SelectionBody;

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;

#[derive(Debug, Deserialize)]
pub struct StartBody {
    pub selection: SelectionBody,
}

#[derive(Debug, Serialize)]
pub struct StartResponse {
    /// 投入したアルバム（= ジョブ）数
    pub albums: usize,
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
    let (job_ids, duplicates) = state
        .db
        .write(move |c| {
            let rows = tracks::resolve_selection(c, &sel)?;
            let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
            dbv::enqueue_selection(c, &ids, now_epoch())
        })
        .await?;
    if job_ids.is_empty() {
        return Ok(error_response(StatusCode::CONFLICT, "no_changes"));
    }
    state.jobs.notify_enqueued(&job_ids).await;
    Ok((
        StatusCode::ACCEPTED,
        Json(StartResponse {
            albums: job_ids.len(),
            duplicates,
            job_ids,
        }),
    )
        .into_response())
}

//! `POST /api/scan`（SPEC §9、D-38）。スキャンジョブを投入する。既に queued / running なら 409

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::jobs::handlers::scan::{new_scan_job, parse_kind};
use crate::jobs::EnqueueResult;

use super::error::{error_response, ApiError};
use super::AppState;

#[derive(Deserialize)]
pub struct ScanBody {
    /// `"incremental"`（既定）| `"deep"`
    pub kind: Option<String>,
}

#[derive(Serialize)]
pub struct ScanAccepted {
    pub job_id: i64,
}

pub async fn start(
    State(state): State<AppState>,
    body: Option<Json<ScanBody>>,
) -> Result<Response, ApiError> {
    let kind_str = body
        .and_then(|Json(b)| b.kind)
        .unwrap_or_else(|| "incremental".to_owned());
    let Some(kind) = parse_kind(&kind_str) else {
        return Ok(error_response(StatusCode::BAD_REQUEST, "bad_request"));
    };
    Ok(match state.jobs.enqueue(new_scan_job(kind)).await? {
        EnqueueResult::Inserted(job_id) => {
            (StatusCode::ACCEPTED, Json(ScanAccepted { job_id })).into_response()
        }
        EnqueueResult::Duplicate(_) => error_response(StatusCode::CONFLICT, "duplicate"),
    })
}

//! `GET /api/gc/preview`（dry-run。`gc::plan` を同期で呼び、何も消さない）と
//! `POST /api/gc`（`gc` ジョブの投入。未完了があれば 409）。SPEC §9、P1-11、D-56

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::db::now_epoch;
use crate::gc::{self, Preview};
use crate::jobs::handlers::gc::new_gc_job;
use crate::jobs::EnqueueResult;

use super::error::{error_response, ApiError};
use super::AppState;

#[derive(Serialize)]
pub struct GcAccepted {
    pub job_id: i64,
}

pub async fn preview(State(state): State<AppState>) -> Result<Response, ApiError> {
    let Some(roots) = state.gc.as_ref() else {
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "gc_unavailable",
        ));
    };
    let retention = i64::from(state.config.gc.retention_days) * 86_400;
    let plan = gc::plan(&state.db, roots, retention, now_epoch())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(Preview::of(&plan)).into_response())
}

pub async fn start(State(state): State<AppState>) -> Result<Response, ApiError> {
    Ok(match state.jobs.enqueue(new_gc_job()).await? {
        EnqueueResult::Inserted(job_id) => {
            (StatusCode::ACCEPTED, Json(GcAccepted { job_id })).into_response()
        }
        EnqueueResult::Duplicate(_) => error_response(StatusCode::CONFLICT, "duplicate"),
    })
}

//! `GET /api/jobs`、`POST /api/jobs/:id/cancel` / `retry`（SPEC §9、§12.5）

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::jobs::{CancelOutcome, Job, RetryOutcome, Summary};

use super::error::{error_response, ApiError};
use super::AppState;

#[derive(Serialize)]
pub struct JobList {
    pub items: Vec<Job>,
    pub summary: Summary,
}

pub async fn list(State(state): State<AppState>) -> Result<Json<JobList>, ApiError> {
    let (items, summary) = state.jobs.list().await?;
    Ok(Json(JobList { items, summary }))
}

pub async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    Ok(match state.jobs.cancel(id).await? {
        CancelOutcome::Cancelled | CancelOutcome::Requested => StatusCode::ACCEPTED.into_response(),
        CancelOutcome::NotFound => error_response(StatusCode::NOT_FOUND, "not_found"),
        CancelOutcome::NotCancellable => error_response(StatusCode::CONFLICT, "not_cancellable"),
    })
}

pub async fn retry(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    Ok(match state.jobs.retry(id).await? {
        RetryOutcome::Requeued => StatusCode::ACCEPTED.into_response(),
        RetryOutcome::NotFound => error_response(StatusCode::NOT_FOUND, "not_found"),
        RetryOutcome::NotRetryable => error_response(StatusCode::CONFLICT, "not_retryable"),
        RetryOutcome::Duplicate => error_response(StatusCode::CONFLICT, "duplicate"),
    })
}

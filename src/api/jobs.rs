//! `GET /api/jobs`、`POST /api/jobs/:id/cancel` / `retry`（SPEC §9、§12.5）

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::jobs::ListLimits;
use crate::jobs::{CancelOutcome, Job, JobType, RetryOutcome, Summary, TypeCounts, LIST_LIMITS};

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;

#[derive(Serialize)]
pub struct JobList {
    pub items: Vec<Job>,
    pub summary: Summary,
    /// 種別ごとの並列度（SPEC §8。ジョブ画面 §12.5 が待ち行列と並べて出す）
    pub concurrency: BTreeMap<&'static str, usize>,
    /// CPU 系（rg / transcode / flaccheck / hirescheck）が共有する並列予算（= コア数。D-73）
    pub cpu_budget: usize,
    /// 種別ごとの queued / running / failed（全件の集計。`items` は上限付きなので web で数えない）
    pub by_type: BTreeMap<String, TypeCounts>,
    /// `items` の状態ごとの上限（達していれば画面が「表示は最新 N 件まで」と添える）
    pub limits: ListLimits,
}

#[derive(Deserialize)]
pub struct ListParams {
    /// 種別で一覧を絞る（`ytdl` 等。YouTube 画面。上限も種別内で数える）。summary / by_type は全件
    #[serde(rename = "type")]
    pub job_type: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> Result<Response, ApiError> {
    let job_type = match params.job_type.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(s) => match s.parse::<JobType>() {
            Ok(t) => Some(t),
            Err(e) => {
                return Ok(error_response_with_message(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    e,
                ))
            }
        },
    };
    let (items, summary, by_type) = state.jobs.list(job_type).await?;
    let cpus = state.jobs.cpus();
    let concurrency = JobType::ALL
        .iter()
        .map(|t| (t.as_str(), t.concurrency(cpus)))
        .collect();
    Ok(Json(JobList {
        items,
        summary,
        concurrency,
        cpu_budget: state.jobs.cpu_budget(),
        by_type,
        limits: LIST_LIMITS,
    })
    .into_response())
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

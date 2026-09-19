//! `POST /api/ytmusic/download`（SPEC §9、D-70、P3-3）。URL ごとに `ytdl` ジョブを投入する。
//! `[ytmusic].enabled` でなければ 404（ハンドラが登録されないので投入しても動かない）

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::import::ytmusic::downloader::new_ytdl_job;

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;

/// URL 1 件の上限（文字）
const MAX_URL_CHARS: usize = 2048;

#[derive(Deserialize)]
pub struct DownloadBody {
    #[serde(default)]
    pub urls: Vec<String>,
}

#[derive(Serialize)]
pub struct DownloadAccepted {
    /// `urls` と同じ順。同じ URL が既に queued / running ならそのジョブの id
    pub job_ids: Vec<i64>,
}

pub async fn download(
    State(state): State<AppState>,
    Json(body): Json<DownloadBody>,
) -> Result<Response, ApiError> {
    if !state.config.ytmusic.enabled {
        return Ok(error_response(StatusCode::NOT_FOUND, "not_found"));
    }
    let urls: Vec<String> = body.urls.iter().map(|u| u.trim().to_owned()).collect();
    if urls.is_empty() {
        return Ok(error_response_with_message(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "urls が空",
        ));
    }
    for u in &urls {
        if u.is_empty() || u.chars().count() > MAX_URL_CHARS {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("URL が空か長すぎる（{MAX_URL_CHARS} 文字まで）"),
            ));
        }
        if !(u.starts_with("https://") || u.starts_with("http://"))
            || u.chars().any(char::is_control)
        {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("URL の形が不正: {u}"),
            ));
        }
    }
    let mut job_ids = Vec::with_capacity(urls.len());
    for u in &urls {
        job_ids.push(state.jobs.enqueue(new_ytdl_job(u)).await?.id());
    }
    Ok((StatusCode::ACCEPTED, Json(DownloadAccepted { job_ids })).into_response())
}

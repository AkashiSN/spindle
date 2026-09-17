//! `POST /api/rg { selection }`（SPEC §9、P1-1）。selection に含まれる active なトラックの
//! album ごとに `rg` ジョブを投入する（album を持たないトラックは track 単位）。
//! 解析は DB にしか書かないので preview 段階は無い。対象が無ければ 409 `no_changes`。
//! 既に queued / running の album は `duplicates` に数える

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db::{replaygain as dbrg, tracks};
use crate::domain::selection::SelectionBody;
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

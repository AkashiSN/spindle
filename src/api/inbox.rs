//! Inbox の承認キュー（SPEC §7.8 / §9、D-68、P2-10）。
//! `GET /api/inbox`（件と下書き・警告）、`POST /api/inbox/scan`、`POST /api/inbox/:id/approve`
//! （検証して approved にし、inbox ジョブを投入）、`/reject`、`/reopen`

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::db::inbox::{self as dbinbox, FileRow, Item, ItemState};
use crate::db::now_epoch;
use crate::db::scans;
use crate::import::inbox::{proposal, warnings, InboxDraft};
use crate::jobs::handlers::inbox::new_inbox_job;
use crate::jobs::EnqueueResult;

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;

#[derive(Serialize)]
pub struct ItemView {
    #[serde(flatten)]
    pub item: Item,
    pub tracks: Vec<FileRow>,
    pub proposal: InboxDraft,
    pub warnings: Vec<String>,
}

#[derive(Serialize)]
pub struct ItemList {
    pub items: Vec<ItemView>,
}

#[derive(Serialize)]
pub struct Accepted {
    pub job_id: i64,
}

fn unavailable(state: &AppState) -> Option<Response> {
    state
        .inbox
        .is_none()
        .then(|| error_response(StatusCode::SERVICE_UNAVAILABLE, "inbox_unavailable"))
}

pub async fn list(State(state): State<AppState>) -> Result<Response, ApiError> {
    if let Some(r) = unavailable(&state) {
        return Ok(r);
    }
    let items = state
        .db
        .read(|c| {
            // 表示名（`scans::load_categories` は照合用の canonical key を返す）
            let categories: Vec<(i64, String)> = crate::db::categories::list(c)?
                .into_iter()
                .map(|cat| (cat.id, cat.name))
                .collect();
            let genre_map = scans::load_genre_map(c)?;
            let mut out = Vec::new();
            for item in dbinbox::list(c)? {
                let tracks = dbinbox::files(c, item.id)?;
                let p = proposal(&tracks, &categories, &genre_map);
                let w = warnings(&tracks, &p);
                out.push(ItemView {
                    item,
                    tracks,
                    proposal: p,
                    warnings: w,
                });
            }
            Ok(out)
        })
        .await?;
    Ok(Json(ItemList { items }).into_response())
}

pub async fn scan(State(state): State<AppState>) -> Result<Response, ApiError> {
    if let Some(r) = unavailable(&state) {
        return Ok(r);
    }
    Ok(match state.jobs.enqueue(new_inbox_job()).await? {
        EnqueueResult::Inserted(job_id) => {
            (StatusCode::ACCEPTED, Json(Accepted { job_id })).into_response()
        }
        EnqueueResult::Duplicate(_) => error_response(StatusCode::CONFLICT, "duplicate"),
    })
}

pub async fn approve(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(draft): Json<InboxDraft>,
) -> Result<Response, ApiError> {
    if let Some(r) = unavailable(&state) {
        return Ok(r);
    }
    let (item, files) = state
        .db
        .read(move |c| {
            let item = dbinbox::get(c, id)?;
            let files = match &item {
                Some(_) => dbinbox::files(c, id)?,
                None => Vec::new(),
            };
            Ok((item, files))
        })
        .await?;
    let Some(item) = item else {
        return Ok(error_response(StatusCode::NOT_FOUND, "not_found"));
    };
    if !matches!(item.state, ItemState::Pending | ItemState::Failed) {
        return Ok(error_response(StatusCode::CONFLICT, "state"));
    }
    let names: Vec<String> = files.iter().map(|f| f.rel_path.clone()).collect();
    if let Err(e) = draft.validate(&names) {
        return Ok(error_response_with_message(
            StatusCode::BAD_REQUEST,
            "bad_request",
            e.to_string(),
        ));
    }
    let value = serde_json::to_value(&draft)
        .map_err(|e| ApiError::Internal(format!("下書きを JSON にできない: {e}")))?;
    state
        .db
        .transaction(move |c| {
            dbinbox::set_draft(c, id, &value)?;
            dbinbox::set_state(c, id, ItemState::Approved, None, now_epoch())?;
            Ok(())
        })
        .await?;
    let job_id = state.jobs.enqueue(new_inbox_job()).await?.id();
    Ok((StatusCode::ACCEPTED, Json(Accepted { job_id })).into_response())
}

async fn transition(
    state: &AppState,
    id: i64,
    from: &[ItemState],
    to: ItemState,
) -> Result<Response, ApiError> {
    if let Some(r) = unavailable(state) {
        return Ok(r);
    }
    let item = state.db.read(move |c| dbinbox::get(c, id)).await?;
    let Some(item) = item else {
        return Ok(error_response(StatusCode::NOT_FOUND, "not_found"));
    };
    if !from.contains(&item.state) {
        return Ok(error_response(StatusCode::CONFLICT, "state"));
    }
    state
        .db
        .write(move |c| dbinbox::set_state(c, id, to, None, now_epoch()))
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn reject(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    transition(
        &state,
        id,
        &[ItemState::Pending, ItemState::Failed, ItemState::Approved],
        ItemState::Rejected,
    )
    .await
}

pub async fn reopen(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    transition(
        &state,
        id,
        &[ItemState::Approved, ItemState::Rejected, ItemState::Failed],
        ItemState::Pending,
    )
    .await
}

//! `GET /api/albums`、`GET /api/albums/:id`（SPEC §9、P0-7）、`PATCH /api/albums/:id`（P4-5、D-74）。
//! アルバムは数千件なので一覧はページングしない（ツリーの構築に全件を使う）

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db::tracks::{self, AlbumRow};
use crate::db::{jobs as dbjobs, now_epoch, replaygain as dbrg};
use crate::jobs::handlers::rg::new_album_job;

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;

#[derive(Serialize)]
pub struct AlbumList {
    pub items: Vec<AlbumRow>,
}

pub async fn list(State(state): State<AppState>) -> Result<Json<AlbumList>, ApiError> {
    let items = state.db.read(tracks::list_albums).await?;
    Ok(Json(AlbumList { items }))
}

pub async fn get(State(state): State<AppState>, Path(id): Path<i64>) -> Result<Response, ApiError> {
    let row = state.db.read(move |c| tracks::get_album(c, id)).await?;
    Ok(match row {
        Some(row) => Json(row).into_response(),
        None => error_response(StatusCode::NOT_FOUND, "not_found"),
    })
}

#[derive(Debug, Deserialize)]
pub struct PatchBody {
    pub album_gain: bool,
}

#[derive(Serialize)]
pub struct PatchResponse {
    pub album: AlbumRow,
    /// true にして投入した album 単位の rg。投入しなければ null
    pub job_id: Option<i64>,
    /// false にして投入した Derived の追随（タグ上書き）
    pub derived_jobs: Vec<i64>,
}

/// `PATCH /api/albums/:id { album_gain }`（D-74）。true → album 単位の rg を投入、false →
/// `rg_album_*` を NULL にして未書込に戻し、Derived の追随を投入。同じ値なら何もしない
pub async fn patch(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    body: Result<Json<PatchBody>, JsonRejection>,
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
    let on = body.album_gain;
    let outcome = state
        .db
        .transaction(move |c| {
            let now = now_epoch();
            let Some(change) = dbrg::set_album_gain(c, id, on, now)? else {
                return Ok(None);
            };
            let mut derived_jobs = Vec::new();
            let mut job_id = None;
            if change.changed {
                if on {
                    job_id = Some(dbjobs::enqueue(c, &new_album_job(id), now)?.id());
                } else {
                    for track_id in &change.cleared {
                        if let Some(j) = crate::db::derived::enqueue_if_stale(c, *track_id, now)? {
                            derived_jobs.push(j);
                        }
                    }
                }
            }
            Ok(Some((job_id, derived_jobs)))
        })
        .await?;
    let Some((job_id, derived_jobs)) = outcome else {
        return Ok(error_response(StatusCode::NOT_FOUND, "not_found"));
    };
    let mut all = derived_jobs.clone();
    all.extend(job_id);
    state.jobs.notify_enqueued(&all).await;
    let Some(album) = state.db.read(move |c| tracks::get_album(c, id)).await? else {
        return Ok(error_response(StatusCode::NOT_FOUND, "not_found"));
    };
    Ok(Json(PatchResponse {
        album,
        job_id,
        derived_jobs,
    })
    .into_response())
}

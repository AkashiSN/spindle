//! `GET /api/albums`、`GET /api/albums/:id`（SPEC §9、P0-7）。
//! アルバムは数千件なので一覧はページングしない（ツリーの構築に全件を使う）

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::db::tracks::{self, AlbumRow};

use super::error::{error_response, ApiError};
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

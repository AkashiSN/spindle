//! `GET /api/categories`、`POST /api/categories { name }`（SPEC §9、D-67）。
//! category は `[layout]` テンプレートの先頭要素になるので、ファイル名として使えない名前は 400

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db::categories::{self, Category};
use crate::domain::pathgen::sanitize_component;

use super::error::{error_response, ApiError};
use super::AppState;

#[derive(Serialize)]
pub struct CategoryList {
    pub items: Vec<Category>,
}

#[derive(Deserialize)]
pub struct CreateBody {
    pub name: String,
}

pub async fn list(State(state): State<AppState>) -> Result<Json<CategoryList>, ApiError> {
    let items = state.db.read(categories::list).await?;
    Ok(Json(CategoryList { items }))
}

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateBody>,
) -> Result<Response, ApiError> {
    let name = body.name.trim().to_owned();
    if name.is_empty() || sanitize_component(&name) != name {
        return Ok(error_response(StatusCode::BAD_REQUEST, "bad_request"));
    }
    let inserted = {
        let name = name.clone();
        state
            .db
            .write(move |c| categories::insert(c, &name))
            .await?
    };
    Ok(match inserted {
        Some(id) => (StatusCode::CREATED, Json(Category { id, name })).into_response(),
        None => error_response(StatusCode::CONFLICT, "duplicate"),
    })
}

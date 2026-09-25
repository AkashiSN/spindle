//! `GET /api/categories`、`POST /api/categories { name }`、`DELETE /api/categories/:id`（SPEC §9、D-67、D-92）。
//! category は `[layout]` テンプレートの先頭要素になるので、ファイル名として使えない名前は 400

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db::categories::{self, Category};
use crate::domain::pathgen::sanitize_component;

use super::error::{error_response, error_response_with_message, ApiError};
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

/// `DELETE /api/categories/:id`: 使われていない語彙だけを消す（D-92）。204、無ければ 404、使われていれば
/// 409 `in_use`（`message` に理由）
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let outcome = state
        .db
        .write(move |c| {
            let tx = c.transaction()?;
            let out = categories::delete_if_unused(&tx, id)?;
            tx.commit()?;
            Ok(out)
        })
        .await?;
    Ok(match outcome {
        categories::DeleteOutcome::Deleted => StatusCode::NO_CONTENT.into_response(),
        categories::DeleteOutcome::NotFound => error_response(StatusCode::NOT_FOUND, "not_found"),
        categories::DeleteOutcome::InUse(reason) => error_response_with_message(
            StatusCode::CONFLICT,
            "in_use",
            format!("使われているので消せない: {reason}"),
        ),
    })
}

//! HTTP API。P0-1 では `/health` と SPA 配信の骨格のみ。
//! 認証ミドルウェア（`/health` 以外の全ルート）は P0-3 でここに載せる

pub mod health;
pub mod spa;

use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

/// ルータを組み立てる
pub fn router() -> Router {
    Router::new()
        .route("/health", get(health::get))
        .nest("/api", Router::new().fallback(api_not_found))
        .fallback(spa::serve)
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
}

/// `/api/*` に一致しないパスは SPA の index.html に倒さず JSON の 404 を返す
async fn api_not_found() -> (StatusCode, Json<ErrorBody>) {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorBody { error: "not_found" }),
    )
}

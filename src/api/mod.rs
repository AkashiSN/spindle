//! HTTP API。`/health` 以外の全ルート（SPA 配信を含む）は `auth::guard` の配下に置く

pub mod auth;
pub mod error;
pub mod health;
pub mod spa;
mod state;

use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{middleware, Router};

pub use state::AppState;

/// ルータを組み立てる
pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/session", get(auth::session))
        .fallback(api_not_found);

    let protected = Router::new()
        .nest("/api", api)
        .fallback(spa::serve)
        .layer(middleware::from_fn_with_state(state.clone(), auth::guard));

    Router::new()
        .route("/health", get(health::get))
        .merge(protected)
        .with_state(state)
}

/// `/api/*` に一致しないパスは SPA の index.html に倒さず JSON の 404 を返す
async fn api_not_found() -> Response {
    error::error_response(StatusCode::NOT_FOUND, "not_found")
}

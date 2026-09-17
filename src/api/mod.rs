//! HTTP API。`/health` 以外の全ルート（SPA 配信を含む）は `auth::guard` の配下に置く

pub mod albums;
pub mod artwork;
pub mod auth;
pub mod batch;
pub mod error;
pub mod events;
pub mod health;
pub mod history;
pub mod jobs;
pub mod normalize;
pub mod rename;
pub mod rg;
pub mod scan;
pub mod selection;
pub mod spa;
mod state;
pub mod stream;
pub mod tracks;

use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, patch, post};
use axum::{middleware, Router};

pub use state::AppState;

/// ルータを組み立てる
pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/session", get(auth::session))
        .route("/tracks", get(tracks::list))
        .route("/tracks/{id}", get(tracks::get))
        .route("/tracks/batch", patch(batch::apply))
        .route("/tracks/batch/preview", post(batch::preview))
        .route("/rename/preview", post(rename::preview))
        .route("/rename/apply", post(rename::apply))
        .route("/normalize/preview", post(normalize::preview))
        .route("/normalize/apply", post(normalize::apply))
        .route("/rg", post(rg::start))
        .route("/rg/write", post(rg::write))
        .route("/search", get(tracks::search))
        .route("/albums", get(albums::list))
        .route("/albums/{id}", get(albums::get))
        .route("/artwork/{hash}", get(artwork::get))
        .route("/stream/{id}", get(stream::get))
        .route("/history", get(history::list))
        .route("/history/{id}", get(history::get))
        .route("/history/{id}/revert", post(history::revert))
        .route("/history/{id}/cancel", post(history::cancel))
        .route("/jobs", get(jobs::list))
        .route("/jobs/{id}/cancel", post(jobs::cancel))
        .route("/jobs/{id}/retry", post(jobs::retry))
        .route("/events", get(events::stream))
        .route("/scan", post(scan::start))
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

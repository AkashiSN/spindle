//! HTTP API。`/health` 以外の全ルート（SPA 配信を含む）は `auth::guard` の配下に置く

pub mod albums;
pub mod archive;
pub mod artwork;
pub mod auth;
pub mod batch;
pub mod categories;
pub mod cd;
pub mod config;
pub mod error;
pub mod events;
pub mod flaccheck;
pub mod gc;
pub mod health;
pub mod hirescheck;
pub mod history;
pub mod inbox;
pub mod jobs;
pub mod md5fill;
pub mod normalize;
pub mod playlists;
pub mod rename;
pub mod rg;
pub mod scan;
pub mod selection;
pub mod spa;
mod state;
pub mod stream;
pub mod tracks;
pub mod verify;
pub mod ytmusic;

use axum::extract::DefaultBodyLimit;
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
        .route("/flaccheck", post(flaccheck::start))
        .route("/hirescheck", post(hirescheck::start))
        .route("/md5fill", post(md5fill::start))
        .route("/verify", post(verify::start))
        .route("/cd/lookup", post(cd::lookup))
        .route("/search", get(tracks::search))
        .route("/albums", get(albums::list))
        .route(
            "/categories",
            get(categories::list).post(categories::create),
        )
        .route("/albums/{id}", get(albums::get))
        .route("/artwork/{hash}", get(artwork::get))
        .route(
            "/artwork/upload",
            post(artwork::upload).layer(DefaultBodyLimit::max(artwork::UPLOAD_LIMIT)),
        )
        .route("/artwork/embed", post(artwork::embed))
        .route("/playlists", get(playlists::list).post(playlists::create))
        .route(
            "/playlists/import",
            get(playlists::import_list).post(playlists::import_create),
        )
        .route("/playlists/preview", post(playlists::preview))
        .route("/playlists/{id}/refresh", post(playlists::refresh))
        .route("/playlists/{id}/fb2k_query", get(playlists::fb2k_query))
        .route(
            "/playlists/{id}",
            get(playlists::get)
                .patch(playlists::patch)
                .delete(playlists::delete),
        )
        .route(
            "/playlists/{id}/items",
            post(playlists::append).delete(playlists::remove),
        )
        .route("/playlists/{id}/items/move", post(playlists::move_items))
        .route(
            "/playlists/{id}/export",
            get(playlists::export_get).post(playlists::export_post),
        )
        .route("/stream/{id}", get(stream::get))
        .route("/history", get(history::list))
        .route("/history/{id}", get(history::get))
        .route("/history/{id}/revert", post(history::revert))
        .route("/history/{id}/cancel", post(history::cancel))
        .route("/jobs", get(jobs::list))
        .route("/jobs/{id}/cancel", post(jobs::cancel))
        .route("/jobs/{id}/retry", post(jobs::retry))
        .route("/events", get(events::stream))
        .route("/config", get(config::get))
        .route("/archive", get(archive::list))
        .route("/scan", post(scan::start))
        .route("/gc", post(gc::start))
        .route("/gc/preview", get(gc::preview))
        .route("/inbox", get(inbox::list))
        .route("/inbox/scan", post(inbox::scan))
        .route("/inbox/{id}/approve", post(inbox::approve))
        .route("/inbox/{id}/reject", post(inbox::reject))
        .route("/inbox/{id}/reopen", post(inbox::reopen))
        .route("/ytmusic/download", post(ytmusic::download))
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

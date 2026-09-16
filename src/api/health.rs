//! `GET /health`。認証の外に置く唯一のルート。ロックモードでは `{"status":"locked"}`

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use super::auth::Mode;
use super::AppState;

#[derive(Serialize)]
pub struct Health {
    pub status: &'static str,
}

pub async fn get(State(state): State<AppState>) -> Json<Health> {
    let status = match state.auth.mode {
        Mode::Unlocked => "ok",
        Mode::Locked => "locked",
    };
    Json(Health { status })
}

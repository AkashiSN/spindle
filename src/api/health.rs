//! `GET /health`。認証の外に置く唯一のルート。ロックモードでは `{"status":"locked"}`。
//! `version`（ビルド時に焼いた版。P4-12）と `ytdlp`（起動時診断で取った yt-dlp の版。無ければ null）で
//! 「いまどの版が動いているか」に答える

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use super::auth::Mode;
use super::AppState;

#[derive(Serialize)]
pub struct Health {
    pub status: &'static str,
    pub version: &'static str,
    pub ytdlp: Option<String>,
}

pub async fn get(State(state): State<AppState>) -> Json<Health> {
    let status = match state.auth.mode {
        Mode::Unlocked => "ok",
        Mode::Locked => "locked",
    };
    Json(Health {
        status,
        version: crate::version::VERSION,
        ytdlp: state.ytdlp_version.as_deref().map(str::to_owned),
    })
}

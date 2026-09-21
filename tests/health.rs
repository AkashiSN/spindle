//! `GET /health` が 200 を返すこと（P0-1 の受け入れ条件）と、ルーティングの骨格。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::Db;

struct TestApp {
    router: Router,
    _dir: tempfile::TempDir,
}

async fn app() -> TestApp {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
    let config = Arc::new(Config::parse(include_str!("../deploy/config.example.toml")).unwrap());
    let mode = auth::bootstrap(&db, Some("pw".into())).await.unwrap();
    TestApp {
        router: api::router(AppState::new(config, db, mode)),
        _dir: dir,
    }
}

#[tokio::test]
async fn health_returns_200_with_status_ok() {
    let app = app().await;
    let res = app
        .router
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let ct = res
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(ct.starts_with("application/json"), "content-type: {ct}");
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // P4-12: いまどの版が動いているかを /health で答える（version はビルド時に焼く。yt-dlp は起動時診断）
    assert_eq!(json["status"], "ok");
    assert_eq!(json["version"], spindle::version::VERSION);
    assert!(!spindle::version::VERSION.is_empty());
    assert_eq!(json["ytdlp"], serde_json::Value::Null, "診断前は null");
    assert_eq!(json.as_object().unwrap().len(), 3, "{json}");
}

/// P4-12: yt-dlp の版は起動時診断で載せる
#[tokio::test]
async fn health_reports_the_ytdlp_version_when_probed() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
    let config = Arc::new(Config::parse(include_str!("../deploy/config.example.toml")).unwrap());
    let mode = auth::bootstrap(&db, Some("pw".into())).await.unwrap();
    let state = AppState::new(config, db, mode).with_ytdlp_version(Some("2026.08.19".to_owned()));
    let res = api::router(state)
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ytdlp"], "2026.08.19");
}

/// P4-12: `spindle --version` は版だけを出して終わる（設定を読まない）
#[test]
fn version_flag_prints_the_version_and_exits() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_spindle"))
        .arg("--version")
        .env_remove("SPINDLE_CONFIG")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("spindle {}", spindle::version::VERSION)
    );
}

/// ビルド時の版: `SPINDLE_VERSION` が無ければ git describe か `dev`。空にはならない
#[test]
fn version_is_never_empty_and_has_no_whitespace() {
    let v = spindle::version::VERSION;
    assert!(!v.is_empty());
    assert!(!v.chars().any(char::is_whitespace), "{v:?}");
}

#[tokio::test]
async fn unknown_api_route_returns_404_json_not_spa() {
    let app = app().await;
    let res = app
        .router
        .oneshot(
            Request::builder()
                .uri("/api/nope")
                .header("cookie", "x=y")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // 未認証なので 401（認証前に SPA の index.html へ倒れないことが要点）
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unknown_static_asset_returns_404() {
    let app = app().await;
    let res = app
        .router
        .oneshot(
            Request::builder()
                .uri("/assets/nope.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

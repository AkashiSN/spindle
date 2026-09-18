//! `GET /api/config`（設定画面 SPEC §12.6、P1-12、D-58）: 読み込んだ `config.toml` の原文を返す。
//! セッション必須（trusted_cidrs からでも 401）

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::Db;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

struct App {
    router: Router,
    _dir: tempfile::TempDir,
}

async fn build(text: &str) -> App {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
    let config = Arc::new(Config::parse(text).unwrap());
    let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
        .await
        .unwrap();
    let state = AppState::new(config, db, mode);
    App {
        router: api::router(state),
        _dir: dir,
    }
}

fn req(method: Method, uri: &str) -> axum::http::request::Builder {
    let peer: SocketAddr = LAN.parse().unwrap();
    let mut b = Request::builder().method(method).uri(uri);
    b.extensions_mut().unwrap().insert(ConnectInfo(peer));
    b
}

async fn send(app: &App, r: Request<Body>) -> axum::response::Response {
    app.router.clone().oneshot(r).await.unwrap()
}

async fn cookie(app: &App) -> String {
    let r = req(Method::POST, "/api/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .header("sec-fetch-site", "same-origin")
        .body(Body::from(r#"{"password":"correct horse"}"#))
        .unwrap();
    let res = send(app, r).await;
    assert_eq!(res.status(), StatusCode::OK);
    let set = res
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    set.split(';').next().unwrap().to_string()
}

#[tokio::test]
async fn config_returns_the_loaded_toml_verbatim_and_requires_session() {
    // 原文をそのまま返す（コメントも含む。パースし直した TOML ではない）
    let text = format!("{EXAMPLE}\n# 末尾のコメント（原文の証拠）\n");
    let trusted = text.replace(
        "trusted_cidrs = []",
        r#"trusted_cidrs = ["192.168.1.0/24"]"#,
    );
    let app = build(&trusted).await;
    let c = cookie(&app).await;

    let res = send(
        &app,
        req(Method::GET, "/api/config")
            .header(header::COOKIE, &c)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["text"], trusted);
    assert!(v["path"].is_string() || v["path"].is_null());

    // trusted_cidrs 内でもセッション無しは 401（設定は公開しない）
    let res = send(
        &app,
        req(Method::GET, "/api/config").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

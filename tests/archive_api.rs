//! `GET /api/archive`（設定画面 SPEC §12.6、P1-12、D-58）: `archived_files` の台帳を新しい順に返し、
//! 退避した op の `batch_id` を添える（復元はそのバッチの巻き戻し）

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use rusqlite::{params, Connection};
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::Db;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

struct App {
    router: Router,
    dir: tempfile::TempDir,
}

impl App {
    fn conn(&self) -> Connection {
        Connection::open(self.dir.path().join("spindle.db")).unwrap()
    }
}

async fn build() -> App {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
    let config = Arc::new(Config::parse(EXAMPLE).unwrap());
    let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
        .await
        .unwrap();
    let state = AppState::new(config, db, mode);
    App {
        router: api::router(state),
        dir,
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
async fn archive_lists_newest_first_with_batch_id_and_requires_session() {
    let app = build().await;
    let c = cookie(&app).await;
    let conn = app.conn();
    conn.execute(
        "INSERT INTO edit_batches (id, created_at, state, affected) VALUES (7, 1, 'applied', 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edit_ops (id, batch_id, ordinal, track_id, kind, result) VALUES (70, 7, 0, 5, 'archive', 'applied')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO archived_files (id, track_id, op_id, rel_path, rel_path_key, source_rel_path, reason,
                                     archived_at, eligible_after, state, state_at)
         VALUES (1, 5, 70, 'A/x.wav', 'a/x.wav', 'A/x.wav', 'normalize', 100, 200, 'held', NULL),
                (2, 6, NULL, 'B/y.flac', 'b/y.flac', 'B/y.flac', 'restore', 150, 250, 'deleted', 300)",
        params![],
    )
    .unwrap();

    let res = send(
        &app,
        req(Method::GET, "/api/archive")
            .header(header::COOKIE, &c)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], 2, "新しい順");
    assert_eq!(items[0]["reason"], "restore");
    assert_eq!(items[0]["state"], "deleted");
    assert_eq!(items[0]["state_at"], 300);
    assert!(
        items[0]["batch_id"].is_null(),
        "op の無い行は batch_id 無し"
    );
    assert_eq!(items[1]["id"], 1);
    assert_eq!(items[1]["rel_path"], "A/x.wav");
    assert_eq!(items[1]["source_rel_path"], "A/x.wav");
    assert_eq!(items[1]["archived_at"], 100);
    assert_eq!(items[1]["eligible_after"], 200);
    assert_eq!(items[1]["state"], "held");
    assert_eq!(
        items[1]["batch_id"], 7,
        "op から batch を引く（復元はその巻き戻し）"
    );

    let res = send(
        &app,
        req(Method::GET, "/api/archive")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

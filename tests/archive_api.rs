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
        req(Method::GET, "/api/archive?state=all")
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

async fn get_json(app: &App, c: &str, uri: &str) -> (StatusCode, serde_json::Value) {
    let res = send(
        app,
        req(Method::GET, uri)
            .header(header::COOKIE, c)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

fn seed(conn: &Connection, n_held: i64, n_deleted: i64, n_restored: i64) {
    let mut id = 0;
    for (state, n) in [
        ("deleted", n_deleted),
        ("restored", n_restored),
        ("held", n_held),
    ] {
        for _ in 0..n {
            id += 1;
            let rel = format!("A/{id}.wav");
            conn.execute(
                "INSERT INTO archived_files (id, track_id, op_id, rel_path, rel_path_key, source_rel_path, reason,
                                             archived_at, eligible_after, state, state_at)
                 VALUES (?1, NULL, NULL, ?2, lower(?2), ?2, 'normalize', ?1, ?1, ?3, NULL)",
                params![id, rel, state],
            )
            .unwrap();
        }
    }
}

#[tokio::test]
async fn archive_defaults_to_held_and_counts_every_state() {
    let app = build().await;
    let c = cookie(&app).await;
    seed(&app.conn(), 2, 3, 1);
    let (st, v) = get_json(&app, &c, "/api/archive").await;
    assert_eq!(st, StatusCode::OK);
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "既定は保持中だけ");
    assert!(items.iter().all(|i| i["state"] == "held"));
    assert_eq!(
        v["counts"],
        serde_json::json!({"held": 2, "restored": 1, "deleted": 3})
    );
    assert!(v["next_cursor"].is_null());

    let (_, v) = get_json(&app, &c, "/api/archive?state=deleted").await;
    assert_eq!(v["items"].as_array().unwrap().len(), 3);
    assert_eq!(v["counts"]["held"], 2, "counts は state に関係なく全体");
    let (_, v) = get_json(&app, &c, "/api/archive?state=all").await;
    assert_eq!(v["items"].as_array().unwrap().len(), 6);
}

#[tokio::test]
async fn archive_pages_by_cursor_without_gaps_or_duplicates() {
    let app = build().await;
    let c = cookie(&app).await;
    seed(&app.conn(), 0, 5, 0);
    let (_, v) = get_json(&app, &c, "/api/archive?state=deleted&limit=2").await;
    let ids: Vec<i64> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![5, 4]);
    assert_eq!(v["next_cursor"], 4);
    let (_, v) = get_json(&app, &c, "/api/archive?state=deleted&limit=2&cursor=4").await;
    let ids: Vec<i64> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![3, 2]);
    let (_, v) = get_json(&app, &c, "/api/archive?state=deleted&limit=1&cursor=2").await;
    assert_eq!(v["items"][0]["id"], 1);
    assert!(
        v["next_cursor"].is_null(),
        "最後のページ（limit ちょうど）は続き無し"
    );
}

#[tokio::test]
async fn archive_rejects_bad_state_and_limit() {
    let app = build().await;
    let c = cookie(&app).await;
    for uri in [
        "/api/archive?state=gone",
        "/api/archive?limit=0",
        "/api/archive?limit=501",
    ] {
        let (st, v) = get_json(&app, &c, uri).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(v["error"], "bad_request");
    }
}

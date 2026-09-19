//! Inbox の API（`GET /api/inbox`、`POST /api/inbox/scan`、`/:id/approve` / `reject` / `reopen`。
//! SPEC §9、D-68、P2-10）。件は DB に直接作る（走査と配置は tests/inbox_job.rs）

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::inbox::{self, FileRow, ItemState};
use spindle::db::Db;
use spindle::fsroot::RootDir;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

struct App {
    router: Router,
    db: Arc<Db>,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Inbox")).unwrap();
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let config = Arc::new(Config::parse(EXAMPLE).unwrap());
        let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
            .await
            .unwrap();
        let inbox = Arc::new(RootDir::open(&dir.path().join("Inbox")).unwrap());
        let state = AppState::new(config, db.clone(), mode).with_inbox(inbox);
        Self {
            router: api::router(state),
            db,
            dir,
        }
    }

    async fn cookie(&self) -> String {
        let r = req(Method::POST, "/api/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(r#"{"password":"correct horse"}"#))
            .unwrap();
        let res = self.router.clone().oneshot(r).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let set = res
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        set.split(';').next().unwrap().to_string()
    }

    async fn get(&self, c: &str, uri: &str) -> (StatusCode, Value) {
        let r = req(Method::GET, uri)
            .header(header::COOKIE, c)
            .body(Body::empty())
            .unwrap();
        self.send(r).await
    }

    async fn post(&self, c: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        let r = req(Method::POST, uri)
            .header(header::COOKIE, c)
            .header(header::CONTENT_TYPE, "application/json")
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(body.to_string()))
            .unwrap();
        self.send(r).await
    }

    async fn send(&self, r: Request<Body>) -> (StatusCode, Value) {
        let res = self.router.clone().oneshot(r).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// 件を作る（2 曲。1 曲目は TITLE あり、2 曲目は TITLE 無し）
    async fn item(&self, rel_dir: &str) -> i64 {
        let rel_dir = rel_dir.to_owned();
        self.db
            .write(move |c| {
                let id = inbox::insert_item(
                    c,
                    &rel_dir,
                    &spindle::domain::relpath::canonical_key(&rel_dir),
                    1000,
                )?;
                let f = |rel: &str, tags: Vec<(&str, &str)>| FileRow {
                    rel_path: format!("{rel_dir}/{rel}"),
                    inode: 1,
                    size: 1,
                    mtime_ns: 0,
                    ctime_ns: 0,
                    codec: "flac".into(),
                    lossless: true,
                    sample_rate: Some(44100),
                    bit_depth: Some(16),
                    channels: Some(2),
                    duration_ms: Some(1000),
                    tags: tags
                        .into_iter()
                        .map(|(k, v)| (k.to_owned(), v.to_owned()))
                        .collect(),
                };
                inbox::replace_files(
                    c,
                    id,
                    &[
                        f(
                            "01.flac",
                            vec![
                                ("TITLE", "One"),
                                ("ALBUM", "Album"),
                                ("ALBUMARTIST", "Artist"),
                                ("TRACKNUMBER", "1"),
                                ("GENRE", "Rock"),
                            ],
                        ),
                        f("02.flac", vec![("ALBUM", "Album"), ("TRACKNUMBER", "2")]),
                    ],
                )?;
                Ok(id)
            })
            .await
            .unwrap()
    }

    async fn state(&self, id: i64) -> ItemState {
        self.db
            .read(move |c| inbox::get(c, id))
            .await
            .unwrap()
            .unwrap()
            .state
    }
}

fn req(method: Method, uri: &str) -> axum::http::request::Builder {
    let peer: SocketAddr = LAN.parse().unwrap();
    let mut b = Request::builder().method(method).uri(uri);
    b.extensions_mut().unwrap().insert(ConnectInfo(peer));
    b
}

fn draft(rel_dir: &str) -> Value {
    json!({
        "category": null,
        "albumartist": "Artist",
        "album": "Album",
        "date": "2024",
        "tracks": [
            { "rel_path": format!("{rel_dir}/01.flac"), "disc_no": 1, "track_no": 1, "title": "One", "artist": "" },
            { "rel_path": format!("{rel_dir}/02.flac"), "disc_no": 1, "track_no": 2, "title": "Two", "artist": "" }
        ]
    })
}

#[tokio::test]
async fn list_returns_items_with_proposal_tracks_and_warnings() {
    let app = App::new().await;
    let c = app.cookie().await;
    app.db
        .write(|c| {
            c.execute("INSERT INTO categories (name) VALUES ('Rock')", [])?;
            c.execute(
                "INSERT INTO genre_category_map (genre, category_id) VALUES ('Rock', 1)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let id = app.item("AlbumA").await;
    let (st, body) = app.get(&c, "/api/inbox").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let it = &items[0];
    assert_eq!(it["id"], id);
    assert_eq!(it["rel_dir"], "AlbumA");
    assert_eq!(it["state"], "pending");
    assert_eq!(it["tracks"].as_array().unwrap().len(), 2);
    assert_eq!(it["tracks"][0]["rel_path"], "AlbumA/01.flac");
    assert_eq!(it["tracks"][0]["codec"], "flac");
    assert_eq!(it["proposal"]["albumartist"], "Artist");
    assert_eq!(it["proposal"]["album"], "Album");
    assert_eq!(it["proposal"]["category"], "Rock");
    assert_eq!(it["proposal"]["tracks"][1]["title"], "");
    let w = it["warnings"].as_array().unwrap();
    assert_eq!(w.len(), 1);
    assert!(w[0].as_str().unwrap().contains("02.flac"));
    assert!(it["draft"].is_null());
}

#[tokio::test]
async fn approve_validates_and_enqueues_job() {
    let app = App::new().await;
    let c = app.cookie().await;
    let id = app.item("AlbumA").await;
    // 400: タイトルが空
    let mut bad = draft("AlbumA");
    bad["tracks"][1]["title"] = json!("");
    let (st, body) = app.post(&c, &format!("/api/inbox/{id}/approve"), bad).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "bad_request");
    assert!(body["message"].as_str().unwrap().contains("02.flac"));
    // 400: 番号の重複
    let mut bad = draft("AlbumA");
    bad["tracks"][1]["track_no"] = json!(1);
    let (st, _) = app.post(&c, &format!("/api/inbox/{id}/approve"), bad).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // 400: 件に無いファイル
    let mut bad = draft("AlbumA");
    bad["tracks"][1]["rel_path"] = json!("AlbumA/09.flac");
    let (st, _) = app.post(&c, &format!("/api/inbox/{id}/approve"), bad).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(app.state(id).await, ItemState::Pending);
    // 404
    let (st, _) = app
        .post(&c, "/api/inbox/9999/approve", draft("AlbumA"))
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // 成功: approved + draft + inbox ジョブ
    let (st, body) = app
        .post(&c, &format!("/api/inbox/{id}/approve"), draft("AlbumA"))
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    let job_id = body["job_id"].as_i64().unwrap();
    assert_eq!(app.state(id).await, ItemState::Approved);
    let (_, body) = app.get(&c, "/api/inbox").await;
    assert_eq!(body["items"][0]["draft"]["album"], "Album");
    assert!(!body["items"][0]["approved_at"].is_null());
    let ty: String = app
        .db
        .read(move |c| {
            Ok(
                c.query_row("SELECT type FROM jobs WHERE id = ?1", [job_id], |r| {
                    r.get(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(ty, "inbox");
    // 409: approved の件をもう一度 approve
    let (st, body) = app
        .post(&c, &format!("/api/inbox/{id}/approve"), draft("AlbumA"))
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["error"], "state");
}

#[tokio::test]
async fn reject_reopen_and_scan() {
    let app = App::new().await;
    let c = app.cookie().await;
    let id = app.item("AlbumA").await;
    let (st, _) = app
        .post(&c, &format!("/api/inbox/{id}/reject"), json!({}))
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(app.state(id).await, ItemState::Rejected);
    // rejected は approve できない
    let (st, _) = app
        .post(&c, &format!("/api/inbox/{id}/approve"), draft("AlbumA"))
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, _) = app
        .post(&c, &format!("/api/inbox/{id}/reopen"), json!({}))
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(app.state(id).await, ItemState::Pending);
    // pending の reopen は 409、failed の approve は通る
    let (st, _) = app
        .post(&c, &format!("/api/inbox/{id}/reopen"), json!({}))
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    app.db
        .write(move |c| inbox::set_state(c, id, ItemState::Failed, Some("x"), 2000))
        .await
        .unwrap();
    let (st, _) = app
        .post(&c, &format!("/api/inbox/{id}/approve"), draft("AlbumA"))
        .await;
    assert_eq!(st, StatusCode::ACCEPTED);
    // approved → reopen で pending に戻る
    let (st, _) = app
        .post(&c, &format!("/api/inbox/{id}/reopen"), json!({}))
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(app.state(id).await, ItemState::Pending);
    // scan: 202、未完了があれば 409
    let (st, body) = app.post(&c, "/api/inbox/scan", json!({})).await;
    assert!(
        st == StatusCode::ACCEPTED || st == StatusCode::CONFLICT,
        "{st} {body}"
    );
    let (st, body) = app.post(&c, "/api/inbox/scan", json!({})).await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "duplicate");
}

#[tokio::test]
async fn inbox_requires_login() {
    let app = App::new().await;
    let r = req(Method::GET, "/api/inbox").body(Body::empty()).unwrap();
    let res = app.router.clone().oneshot(r).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

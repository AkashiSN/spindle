//! `GET /api/tags/keys`（SPEC §9、D-113）。ライブラリにあるタグのキーと、そのキーを持つ曲数。
//! タグのキーの入力欄（Inbox のタグの追加・プロパティのフィールドの追加・一括編集）の候補に使う

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
use spindle::db::Db;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

struct App {
    router: Router,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let config = Arc::new(Config::parse(EXAMPLE).unwrap());
        let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
            .await
            .unwrap();
        let state = AppState::new(config, db, mode);
        Self {
            router: api::router(state),
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

    #[allow(dead_code)]
    async fn post(&self, c: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        let r = req(Method::POST, uri)
            .header(header::COOKIE, c)
            .header(header::CONTENT_TYPE, "application/json")
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(body.to_string()))
            .unwrap();
        self.send(r).await
    }

    #[allow(dead_code)]
    async fn delete(&self, c: &str, uri: &str) -> (StatusCode, Value) {
        let r = req(Method::DELETE, uri)
            .header(header::COOKIE, c)
            .header("sec-fetch-site", "same-origin")
            .body(Body::empty())
            .unwrap();
        self.send(r).await
    }

    fn conn(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.dir.path().join("spindle.db")).unwrap()
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
}

fn req(method: Method, uri: &str) -> axum::http::request::Builder {
    let peer: SocketAddr = LAN.parse().unwrap();
    let mut b = Request::builder().method(method).uri(uri);
    b.extensions_mut().unwrap().insert(ConnectInfo(peer));
    b
}

fn insert_track(c: &rusqlite::Connection, id: i64, missing: Option<i64>, tags: &[(&str, &str)]) {
    c.execute(
        "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                             audio_version, tag_version, seen_at, missing_since)
         VALUES (?1, ?2, lower(?2), 1, 0, 0, 'flac', 1, 1, 1, 0, ?3)",
        rusqlite::params![id, format!("A/{id:02}.flac"), missing],
    )
    .unwrap();
    for (i, (k, v)) in tags.iter().enumerate() {
        c.execute(
            "INSERT INTO track_tags (track_id, key, idx, value) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, k, i as i64, v],
        )
        .unwrap();
    }
}

#[tokio::test]
async fn tag_keys_lists_keys_by_track_count_without_picture_or_missing() {
    let app = App::new().await;
    let c = app.cookie().await;
    {
        let db = app.conn();
        insert_track(
            &db,
            1,
            None,
            &[
                ("TITLE", "a"),
                ("ARTIST", "x"),
                ("ARTIST", "y"),
                ("TRACK_TYPE", "karaoke"),
            ],
        );
        insert_track(
            &db,
            2,
            None,
            &[("TITLE", "b"), ("ARTIST", "x"), ("PICTURE", "image/png:00")],
        );
        insert_track(&db, 3, None, &[("TITLE", "c"), ("LYRICIST", "z")]);
        // missing の曲のキーは数えない（その曲だけが持つキーは候補に出ない）
        insert_track(&db, 4, Some(5), &[("TITLE", "d"), ("OLDKEY", "o")]);
    }
    let (st, body) = app.get(&c, "/api/tags/keys").await;
    assert_eq!(st, StatusCode::OK);
    // 曲数の多い順、同数はキーの順。多値は 1 曲と数える。PICTURE（疑似キー）は出さない
    assert_eq!(
        body,
        json!({ "items": [
            { "key": "TITLE", "tracks": 3 },
            { "key": "ARTIST", "tracks": 2 },
            { "key": "LYRICIST", "tracks": 1 },
            { "key": "TRACK_TYPE", "tracks": 1 },
        ]})
    );
}

#[tokio::test]
async fn tag_keys_require_login() {
    let app = App::new().await;
    let r = req(Method::GET, "/api/tags/keys")
        .body(Body::empty())
        .unwrap();
    let (st, _) = app.send(r).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

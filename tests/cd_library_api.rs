//! `POST /api/cd/library { toc, release_id? }`（SPEC §9、§12.6 CD）。いまドライブに入っている盤が
//! ライブラリにあるか。
//!
//! 見るもの: DiscID（TOC からサーバが計算）が active なトラックの `MUSICBRAINZ_DISCID` に当たれば
//! `disc`、当たらず選択中の候補のリリース ID が active な album の `mb_release_id` に当たれば
//! `release`（大文字小文字は区別しない。D-84）、missing のトラック・album は数えない、TOC が壊れて
//! いれば 400

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::Db;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";
const NEVERMIND_TOC: &str =
    "0:22593:41700:58133:71920:91198:104468:115188:131988:143758:159678:174415:191880";
const NEVERMIND_ID: &str = "y6Br7t4P.bldLe_6Im2d9Z42IU4-";
const RELEASE: &str = "f1223d63-f359-457d-b935-fc27eb24a6de";

struct TestApp {
    router: Router,
    dir: tempfile::TempDir,
}

impl TestApp {
    fn raw(&self) -> Connection {
        Connection::open(self.dir.path().join("spindle.db")).unwrap()
    }
}

async fn app() -> TestApp {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
    let config = Arc::new(Config::parse(EXAMPLE).unwrap());
    let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
        .await
        .unwrap();
    let state = AppState::new(config, db, mode);
    TestApp {
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

async fn cookie(app: &TestApp) -> String {
    let r = req(Method::POST, "/api/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .header("sec-fetch-site", "same-origin")
        .body(Body::from(r#"{"password":"correct horse"}"#))
        .unwrap();
    let res = app.router.clone().oneshot(r).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let set = res
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    set.split(';').next().unwrap().to_string()
}

async fn post(app: &TestApp, c: &str, body: Value) -> (StatusCode, Value) {
    let r = req(Method::POST, "/api/cd/library")
        .header(header::COOKIE, c)
        .header(header::CONTENT_TYPE, "application/json")
        .header("sec-fetch-site", "same-origin")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.router.clone().oneshot(r).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn insert_album(conn: &Connection, rel_dir: &str, album: &str, mb: Option<&str>) -> i64 {
    conn.execute(
        "INSERT INTO albums (rel_dir, rel_dir_key, albumartist, album, date, mb_release_id)
         VALUES (?1, ?1, 'Nirvana', ?2, '1991', ?3)",
        params![rel_dir, album, mb],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn insert_track(conn: &Connection, album_id: i64, rel: &str, discid: Option<&str>) -> i64 {
    conn.execute(
        "INSERT INTO tracks (album_id, rel_path, rel_path_key, dev, inode, size, mtime_ns, ctime_ns, codec, lossless,
           title, artist_display, album, albumartist, track_no, disc_no, seen_at)
         VALUES (?1, ?2, ?2, 1, abs(random()), 1, 1, 1, 'flac', 1, 't', 'Nirvana', 'Nevermind', 'Nirvana', 1, 1, 1)",
        params![album_id, rel],
    )
    .unwrap();
    let id = conn.last_insert_rowid();
    if let Some(d) = discid {
        conn.execute(
            "INSERT INTO track_tags (track_id, key, idx, value) VALUES (?1, 'MUSICBRAINZ_DISCID', 0, ?2)",
            params![id, d],
        )
        .unwrap();
    }
    id
}

#[tokio::test]
async fn disc_match_by_discid_tag() {
    let app = app().await;
    let c = cookie(&app).await;
    let alb = {
        let conn = app.raw();
        let alb = insert_album(&conn, "Rock/Nirvana/Nevermind", "Nevermind", None);
        insert_track(
            &conn,
            alb,
            "Rock/Nirvana/Nevermind/01.flac",
            Some(NEVERMIND_ID),
        );
        alb
    };
    let (st, body) = post(&app, &c, json!({ "toc": NEVERMIND_TOC })).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["discid"], NEVERMIND_ID);
    assert_eq!(body["disc"]["album_id"], alb);
    assert_eq!(body["disc"]["album"], "Nevermind");
    assert_eq!(body["disc"]["albumartist"], "Nirvana");
    assert_eq!(body["release"], Value::Null);
}

#[tokio::test]
async fn release_match_when_disc_is_not_in_library() {
    let app = app().await;
    let c = cookie(&app).await;
    let alb = {
        let conn = app.raw();
        // 大文字で書かれた MBID でも当たる（D-84）
        let alb = insert_album(
            &conn,
            "Rock/Nirvana/Nevermind",
            "Nevermind",
            Some(&RELEASE.to_ascii_uppercase()),
        );
        // 別の盤（DiscID 違い）の行
        insert_track(
            &conn,
            alb,
            "Rock/Nirvana/Nevermind/01.flac",
            Some("other-disc-id"),
        );
        alb
    };
    let (st, body) = post(
        &app,
        &c,
        json!({ "toc": NEVERMIND_TOC, "release_id": RELEASE }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["disc"], Value::Null);
    assert_eq!(body["release"]["album_id"], alb);
    // リリース ID を渡さなければ release は引かない
    let (_, body) = post(&app, &c, json!({ "toc": NEVERMIND_TOC })).await;
    assert_eq!(body["release"], Value::Null);
}

#[tokio::test]
async fn missing_tracks_and_albums_are_not_counted() {
    let app = app().await;
    let c = cookie(&app).await;
    {
        let conn = app.raw();
        let a1 = insert_album(&conn, "Rock/Nirvana/Nevermind", "Nevermind", Some(RELEASE));
        let t = insert_track(
            &conn,
            a1,
            "Rock/Nirvana/Nevermind/01.flac",
            Some(NEVERMIND_ID),
        );
        conn.execute("UPDATE tracks SET missing_since = 10 WHERE id = ?1", [t])
            .unwrap();
        conn.execute("UPDATE albums SET missing_since = 10 WHERE id = ?1", [a1])
            .unwrap();
    }
    let (st, body) = post(
        &app,
        &c,
        json!({ "toc": NEVERMIND_TOC, "release_id": RELEASE }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["disc"], Value::Null);
    assert_eq!(body["release"], Value::Null);
}

#[tokio::test]
async fn broken_toc_is_bad_request() {
    let app = app().await;
    let c = cookie(&app).await;
    let (st, body) = post(&app, &c, json!({ "toc": "garbage" })).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "bad_request");
}

//! `PATCH /api/albums/:id { album_gain }`（P4-5、D-74）。仕様: docs/SPEC.md §9

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

async fn send(app: &TestApp, r: Request<Body>) -> axum::response::Response {
    app.router.clone().oneshot(r).await.unwrap()
}

async fn json(res: axum::response::Response) -> serde_json::Value {
    let body = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
}

async fn cookie(app: &TestApp) -> String {
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

async fn patch(
    app: &TestApp,
    c: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let r = req(Method::PATCH, uri)
        .header(header::COOKIE, c)
        .header(header::CONTENT_TYPE, "application/json")
        .header("sec-fetch-site", "same-origin")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = send(app, r).await;
    let status = res.status();
    (status, json(res).await)
}

fn insert_track(conn: &Connection, rel: &str, title: &str, album_id: Option<i64>) -> i64 {
    conn.execute(
        "INSERT INTO tracks (album_id, rel_path, rel_path_key, dev, inode, size, mtime_ns, ctime_ns, codec, lossless,
           title, artist_display, album, albumartist, track_no, disc_no, seen_at)
         VALUES (?1, ?2, ?2, 1, abs(random()), 1, 1, 1, 'flac', 1, ?3, 'Ar', 'Al', 'AA', 1, 1, 1)",
        params![album_id, rel, title],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn insert_album(conn: &Connection, rel_dir: &str) -> i64 {
    conn.execute(
        "INSERT INTO albums (rel_dir, rel_dir_key, albumartist, album, date)
         VALUES (?1, ?1, 'AA', 'Al', '2024')",
        [rel_dir],
    )
    .unwrap();
    conn.last_insert_rowid()
}

#[tokio::test]
async fn patch_album_gain_on_enqueues_album_rg_and_off_clears_values() {
    let app = app().await;
    let c = cookie(&app).await;
    let (alb, t1) = {
        let conn = app.raw();
        let alb = insert_album(&conn, "J-Pop/AA/Al");
        let t1 = insert_track(&conn, "J-Pop/AA/Al/1.flac", "a", Some(alb));
        // 属性は off のまま、album の値を持つ行（0017 より後に手で入れた形）
        conn.execute(
            "UPDATE tracks SET rg_track_gain = 0, rg_track_peak = 0.5, rg_album_gain = -1.0,
                    rg_album_peak = 0.9, rg_scanned_at = 10, rg_written_at = 10 WHERE id = ?1",
            [t1],
        )
        .unwrap();
        (alb, t1)
    };
    // on: album 単位の rg を投入
    let (st, body) = patch(
        &app,
        &c,
        &format!("/api/albums/{alb}"),
        serde_json::json!({ "album_gain": true }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["album"]["album_gain"], true, "{body}");
    assert_eq!(body["album"]["id"], alb);
    let job = body["job_id"].as_i64().expect("rg を投入");
    let (kind, key): (String, String) = app
        .raw()
        .query_row(
            "SELECT type, dedup_key FROM jobs WHERE id = ?1",
            [job],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(kind, "rg");
    assert_eq!(key, format!("rg:album:{alb}"));
    // 同じ値: 何も投入しない
    let (st, body) = patch(
        &app,
        &c,
        &format!("/api/albums/{alb}"),
        serde_json::json!({ "album_gain": true }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(body["job_id"].is_null(), "{body}");
    // off: album 値を消し、未書込に戻す
    let (st, body) = patch(
        &app,
        &c,
        &format!("/api/albums/{alb}"),
        serde_json::json!({ "album_gain": false }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["album"]["album_gain"], false, "{body}");
    assert!(body["job_id"].is_null(), "{body}");
    let (ag, written, scanned): (Option<f64>, Option<i64>, Option<i64>) = app
        .raw()
        .query_row(
            "SELECT rg_album_gain, rg_written_at, rg_scanned_at FROM tracks WHERE id = ?1",
            [t1],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((ag, written), (None, None));
    assert!(
        scanned.unwrap() > 10,
        "rg_scanned_at が進む（Derived の追随）"
    );

    let (st, _) = patch(
        &app,
        &c,
        "/api/albums/999",
        serde_json::json!({ "album_gain": true }),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = patch(
        &app,
        &c,
        &format!("/api/albums/{alb}"),
        serde_json::json!({ "album_gain": "yes" }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn patch_album_requires_session() {
    let app = app().await;
    let r = req(Method::PATCH, "/api/albums/1")
        .header(header::CONTENT_TYPE, "application/json")
        .header("sec-fetch-site", "same-origin")
        .body(Body::from(r#"{"album_gain":true}"#))
        .unwrap();
    assert_eq!(send(&app, r).await.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------- ?filter=（P4-6、D-58 追記）

fn urlenc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn album_ids(app: &TestApp, c: &str, filter: Option<&str>) -> (StatusCode, Vec<i64>) {
    let uri = match filter {
        Some(f) => format!("/api/albums?filter={}", urlenc(f)),
        None => "/api/albums".to_owned(),
    };
    let r = req(Method::GET, &uri)
        .header(header::COOKIE, c)
        .body(Body::empty())
        .unwrap();
    let res = send(app, r).await;
    let status = res.status();
    let body = json(res).await;
    let ids = body["items"]
        .as_array()
        .map(|a| a.iter().map(|i| i["id"].as_i64().unwrap()).collect())
        .unwrap_or_default();
    (status, ids)
}

/// アルバム一覧はトラック一覧と同じフィルタで絞れる（一致する active なトラックを持つ album だけ）
#[tokio::test]
async fn albums_can_be_filtered_like_tracks() {
    let app = app().await;
    let c = cookie(&app).await;
    let (a1, a2, a3, a4, pl_manual, pl_smart) = {
        let conn = app.raw();
        conn.execute("INSERT INTO categories (name) VALUES ('J-Pop')", [])
            .unwrap();
        let cat: i64 = conn
            .query_row("SELECT id FROM categories WHERE name = 'J-Pop'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let a1 = insert_album(&conn, "J-Pop/AA/One");
        let a2 = insert_album(&conn, "J-Pop/AA/Two");
        let a3 = insert_album(&conn, "Rock/BB/Three");
        let a4 = insert_album(&conn, "Rock/BB/Gone");
        conn.execute(
            "UPDATE albums SET category_id = ?1 WHERE id IN (?2, ?3)",
            params![cat, a1, a2],
        )
        .unwrap();
        let t1 = insert_track(&conn, "J-Pop/AA/One/1.flac", "Sunrise", Some(a1));
        let _t2 = insert_track(&conn, "J-Pop/AA/Two/1.flac", "Moon", Some(a2));
        let t3 = insert_track(&conn, "Rock/BB/Three/1.flac", "Sunset", Some(a3));
        // a4 は missing のトラックしか持たない
        let t4 = insert_track(&conn, "Rock/BB/Gone/1.flac", "Sunk", Some(a4));
        conn.execute("UPDATE tracks SET missing_since = 1 WHERE id = ?1", [t4])
            .unwrap();
        // 静的プレイリストは a1 の曲、スマートプレイリストは a3 の曲（どちらも playlist_items に実体化）
        conn.execute(
            "INSERT INTO playlists (id, name, name_key, kind, created_at, updated_at) VALUES (1, 'm', 'm', 'manual', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO playlists (id, name, name_key, kind, rule_source, rule_ast, created_at, updated_at)
             VALUES (2, 's', 's', 'smart', '%title% HAS sun', '{}', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO playlist_items (playlist_id, position, track_id) VALUES (1, 0, ?1), (2, 0, ?2)",
            params![t1, t3],
        )
        .unwrap();
        (a1, a2, a3, a4, 1i64, 2i64)
    };

    // フィルタ無し・空・{} は全件（missing しか持たない album も含む）
    for f in [None, Some(""), Some("{}")] {
        let (st, ids) = album_ids(&app, &c, f).await;
        assert_eq!(st, StatusCode::OK, "{f:?}");
        assert_eq!(ids, [a1, a2, a3, a4], "{f:?}");
    }
    // album_ids
    let (st, ids) = album_ids(&app, &c, Some(&format!(r#"{{"album_ids":[{a2},{a3}]}}"#))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(ids, [a2, a3]);
    // category
    let (_, ids) = album_ids(&app, &c, Some(r#"{"category":"J-Pop"}"#)).await;
    assert_eq!(ids, [a1, a2]);
    // 静的 / スマートプレイリスト
    let (_, ids) = album_ids(&app, &c, Some(&format!(r#"{{"playlist_id":{pl_manual}}}"#))).await;
    assert_eq!(ids, [a1]);
    let (_, ids) = album_ids(&app, &c, Some(&format!(r#"{{"playlist_id":{pl_smart}}}"#))).await;
    assert_eq!(ids, [a3]);
    // 検索語（FTS）と DSL
    let (_, ids) = album_ids(&app, &c, Some(r#"{"q":"Sun"}"#)).await;
    assert_eq!(ids, [a1, a3], "missing の Sunk を持つ a4 は出ない");
    let (_, ids) = album_ids(&app, &c, Some(r#"{"dsl":"%title% IS Moon"}"#)).await;
    assert_eq!(ids, [a2]);
    // 組み合わせは AND
    let (_, ids) = album_ids(&app, &c, Some(r#"{"category":"J-Pop","q":"Sun"}"#)).await;
    assert_eq!(ids, [a1]);
    // 不正なフィルタは 400
    let (st, _) = album_ids(&app, &c, Some(r#"{"nope":1}"#)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = album_ids(&app, &c, Some("not json")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

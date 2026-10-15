//! `GET /api/logs`・`GET /api/logs/{key}/text`（記録画面の「ログ」タブ。D-106）

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
use spindle::fsroot::RootDir;

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
    fn lib(&self) -> std::path::PathBuf {
        self.dir.path().join("Library")
    }
    fn verify_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("data/verify")
    }
}

async fn build() -> App {
    let dir = tempfile::tempdir().unwrap();
    for d in ["Library", "Derived", "data/verify"] {
        std::fs::create_dir_all(dir.path().join(d)).unwrap();
    }
    let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
    let config = Arc::new(Config::parse(EXAMPLE).unwrap());
    let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
        .await
        .unwrap();
    let library = Arc::new(RootDir::open(&dir.path().join("Library")).unwrap());
    let derived = Arc::new(RootDir::open(&dir.path().join("Derived")).unwrap());
    let verify = Arc::new(RootDir::open(&dir.path().join("data/verify")).unwrap());
    let state = AppState::new(config, db, mode)
        .with_roots(library, derived)
        .with_verify_logs(verify);
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

async fn cookie(app: &App) -> String {
    let r = req(Method::POST, "/api/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .header("sec-fetch-site", "same-origin")
        .body(Body::from(r#"{"password":"correct horse"}"#))
        .unwrap();
    let res = app.router.clone().oneshot(r).await.unwrap();
    let set = res
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    set.split(';').next().unwrap().to_string()
}

async fn get(app: &App, c: &str, uri: &str) -> (StatusCode, Vec<u8>) {
    let r = req(Method::GET, uri)
        .header(header::COOKIE, c)
        .body(Body::empty())
        .unwrap();
    let res = app.router.clone().oneshot(r).await.unwrap();
    let st = res.status();
    (
        st,
        res.into_body().collect().await.unwrap().to_bytes().to_vec(),
    )
}

async fn get_json(app: &App, c: &str, uri: &str) -> (StatusCode, serde_json::Value) {
    let (st, b) = get(app, c, uri).await;
    (
        st,
        serde_json::from_slice(&b).unwrap_or(serde_json::Value::Null),
    )
}

fn album(conn: &Connection, id: i64, rel_dir: &str) {
    conn.execute(
        "INSERT INTO albums (id, rel_dir, rel_dir_key, albumartist, album) VALUES (?1, ?2, lower(?2), 'AA', ?3)",
        params![id, rel_dir, format!("Al{id}")],
    )
    .unwrap();
}

fn track(conn: &Connection, album_id: i64, rel: &str, disc: Option<i64>, no: i64) {
    conn.execute(
        "INSERT INTO tracks (album_id, rel_path, rel_path_key, dev, inode, size, mtime_ns, ctime_ns, codec, lossless,
           title, artist_display, album, albumartist, track_no, disc_no, seen_at)
         VALUES (?1, ?2, lower(?2), 1, abs(random()), 1, 1, 1, 'flac', 1, 't', 'Ar', 'Al', 'AA', ?3, ?4, 1)",
        params![album_id, rel, no, disc],
    )
    .unwrap();
}

fn job(conn: &Connection, id: i64, album_id: i64) {
    conn.execute(
        "INSERT INTO jobs (id, type, payload, state, created_at) VALUES (?1, 'verify', json_object('album_id', ?2), 'done', 1)",
        params![id, album_id],
    )
    .unwrap();
}

#[allow(clippy::too_many_arguments)]
fn verification(
    conn: &Connection,
    album_id: i64,
    source: &str,
    job_id: Option<i64>,
    disc: Option<i64>,
    method: &str,
    result: &str,
    at: i64,
    log_path: Option<&str>,
) {
    conn.execute(
        "INSERT INTO album_verifications (album_id, method, result, source, verified_at, log_path, disc_no, job_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![album_id, method, result, source, at, log_path, disc, job_id],
    )
    .unwrap();
}

#[tokio::test]
async fn runs_group_methods_and_discs_into_one_item() {
    let app = build().await;
    let c = cookie(&app).await;
    let conn = app.conn();
    album(&conn, 1, "A/One");
    job(&conn, 10, 1);
    // 遡及照合 1 回（job 10）= 2 disc × 2 手法
    for disc in [1, 2] {
        verification(
            &conn,
            1,
            "retro",
            Some(10),
            Some(disc),
            "ctdb",
            "verified",
            100,
            Some("/x/1.log"),
        );
        verification(
            &conn,
            1,
            "retro",
            Some(10),
            Some(disc),
            "accuraterip",
            "not_found",
            100,
            Some("/x/1.log"),
        );
    }
    // 吸い出し（job_id なし）= 1 disc × 2 手法、同じ verified_at
    verification(
        &conn,
        1,
        "rip",
        None,
        Some(1),
        "ctdb",
        "verified",
        50,
        Some("A/One/rip.log"),
    );
    verification(
        &conn,
        1,
        "rip",
        None,
        Some(1),
        "accuraterip",
        "verified",
        50,
        Some("A/One/rip.log"),
    );

    let (st, v) = get_json(&app, &c, "/api/logs").await;
    assert_eq!(st, StatusCode::OK);
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{v}");
    assert_eq!(items[0]["source"], "retro", "新しい順");
    assert_eq!(items[0]["key"], "retro-1-j10");
    assert_eq!(items[0]["album"], "Al1");
    assert_eq!(items[0]["discs"].as_array().unwrap().len(), 4);
    assert_eq!(items[0]["log"], "latest");
    assert_eq!(items[1]["source"], "rip");
    assert_eq!(items[1]["key"], "rip-1-t50");
    assert_eq!(
        items[1]["discs"].as_array().unwrap().len(),
        2,
        "job_id の無い行も verified_at でまとまる"
    );
    assert_eq!(items[1]["log"], "latest");
    assert!(v["next_cursor"].is_null());
}

#[tokio::test]
async fn runs_filter_by_kind_and_album_and_mark_superseded() {
    let app = build().await;
    let c = cookie(&app).await;
    let conn = app.conn();
    album(&conn, 1, "A/One");
    album(&conn, 2, "A/Two");
    job(&conn, 10, 1);
    job(&conn, 11, 1);
    job(&conn, 12, 2);
    verification(
        &conn,
        1,
        "retro",
        Some(10),
        Some(1),
        "ctdb",
        "mismatch",
        100,
        Some("/x/1.log"),
    );
    verification(
        &conn,
        1,
        "retro",
        Some(11),
        Some(1),
        "ctdb",
        "verified",
        200,
        Some("/x/1.log"),
    );
    verification(
        &conn,
        2,
        "retro",
        Some(12),
        Some(1),
        "ctdb",
        "verified",
        150,
        Some("/x/2.log"),
    );
    verification(&conn, 2, "rip", None, Some(1), "ctdb", "verified", 50, None);

    let (_, v) = get_json(&app, &c, "/api/logs?album_id=1").await;
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["key"], "retro-1-j11");
    assert_eq!(items[0]["log"], "latest");
    assert_eq!(
        items[1]["log"], "superseded",
        "album ごとに最新 1 回分しか残らない"
    );

    let (_, v) = get_json(&app, &c, "/api/logs?kind=rip").await;
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["log"], "none", "log_path の無い吸い出しは本文なし");

    let (_, v) = get_json(&app, &c, "/api/logs?limit=2").await;
    assert_eq!(v["items"].as_array().unwrap().len(), 2);
    let next = v["next_cursor"].as_str().unwrap().to_owned();
    let (_, v) = get_json(&app, &c, &format!("/api/logs?limit=2&cursor={next}")).await;
    assert_eq!(v["items"].as_array().unwrap().len(), 2);
    assert!(v["next_cursor"].is_null());

    let (st, _) = get_json(&app, &c, "/api/logs?kind=other").await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn retro_text_reads_the_album_log_and_404s_when_superseded() {
    let app = build().await;
    let c = cookie(&app).await;
    let conn = app.conn();
    album(&conn, 1, "A/One");
    job(&conn, 10, 1);
    job(&conn, 11, 1);
    verification(
        &conn,
        1,
        "retro",
        Some(10),
        Some(1),
        "ctdb",
        "mismatch",
        100,
        Some("/x/1.log"),
    );
    verification(
        &conn,
        1,
        "retro",
        Some(11),
        Some(1),
        "ctdb",
        "verified",
        200,
        Some("/x/1.log"),
    );
    std::fs::write(app.verify_dir().join("1.log"), "spindle verify.log\n一致\n").unwrap();

    let (st, body) = get(&app, &c, "/api/logs/retro-1-j11/text").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        String::from_utf8(body).unwrap(),
        "spindle verify.log\n一致\n"
    );
    let (st, v) = get_json(&app, &c, "/api/logs/retro-1-j10/text").await;
    assert_eq!(st, StatusCode::NOT_FOUND, "上書きされた古い実行");
    assert_eq!(v["error"], "log_not_found");
    // 記録の無い key（他の album の id を差し込んでも本文を返さない）
    let (st, _) = get(&app, &c, "/api/logs/retro-2-j11/text").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = get(&app, &c, "/api/logs/nonsense/text").await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rip_text_follows_the_album_after_rename() {
    let app = build().await;
    let c = cookie(&app).await;
    let conn = app.conn();
    // 配置時は A/Old、いまは B/New にある（log_path は古いまま）
    album(&conn, 1, "B/New");
    track(&conn, 1, "B/New/01.flac", Some(1), 1);
    verification(
        &conn,
        1,
        "rip",
        None,
        Some(1),
        "ctdb",
        "verified",
        50,
        Some("A/Old/rip.log"),
    );
    std::fs::create_dir_all(app.lib().join("B/New")).unwrap();
    std::fs::write(app.lib().join("B/New/rip.log"), "spindle rip.log\n").unwrap();

    let (st, body) = get(&app, &c, "/api/logs/rip-1-t50/text").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(String::from_utf8(body).unwrap(), "spindle rip.log\n");
}

#[tokio::test]
async fn rip_text_picks_the_disc_and_rejects_other_names() {
    let app = build().await;
    let c = cookie(&app).await;
    let conn = app.conn();
    album(&conn, 1, "A/Set");
    track(&conn, 1, "A/Set/Disc 1/01.flac", Some(1), 1);
    track(&conn, 1, "A/Set/Disc 2/01.flac", Some(2), 1);
    verification(
        &conn,
        1,
        "rip",
        None,
        Some(1),
        "ctdb",
        "verified",
        50,
        Some("A/Set/Disc 1/rip1.log"),
    );
    verification(
        &conn,
        1,
        "rip",
        None,
        Some(2),
        "ctdb",
        "verified",
        50,
        Some("A/Set/Disc 2/rip2.log"),
    );
    verification(
        &conn,
        1,
        "rip",
        None,
        Some(3),
        "ctdb",
        "verified",
        50,
        Some("A/Set/x/secret.txt"),
    );
    for (d, n) in [("Disc 1", "rip1.log"), ("Disc 2", "rip2.log")] {
        std::fs::create_dir_all(app.lib().join("A/Set").join(d)).unwrap();
        std::fs::write(app.lib().join("A/Set").join(d).join(n), n).unwrap();
    }
    let (_, body) = get(&app, &c, "/api/logs/rip-1-t50/text").await;
    assert_eq!(body, b"rip1.log", "省略は最初のディスク");
    let (_, body) = get(&app, &c, "/api/logs/rip-1-t50/text?disc=2").await;
    assert_eq!(body, b"rip2.log");
    let (st, _) = get(&app, &c, "/api/logs/rip-1-t50/text?disc=3").await;
    assert_eq!(st, StatusCode::NOT_FOUND, "rip*.log 以外の名前は読まない");
}

#[tokio::test]
async fn text_rejects_oversize_and_symlinks() {
    let app = build().await;
    let c = cookie(&app).await;
    let conn = app.conn();
    album(&conn, 1, "A/One");
    album(&conn, 2, "A/Two");
    job(&conn, 10, 1);
    job(&conn, 11, 2);
    verification(
        &conn,
        1,
        "retro",
        Some(10),
        Some(1),
        "ctdb",
        "verified",
        100,
        Some("/x/1.log"),
    );
    verification(
        &conn,
        2,
        "retro",
        Some(11),
        Some(1),
        "ctdb",
        "verified",
        100,
        Some("/x/2.log"),
    );
    std::fs::write(app.verify_dir().join("1.log"), vec![b'a'; (1 << 20) + 1]).unwrap();
    let outside = app.dir.path().join("outside.txt");
    std::fs::write(&outside, "secret").unwrap();
    std::os::unix::fs::symlink(&outside, app.verify_dir().join("2.log")).unwrap();

    let (st, v) = get_json(&app, &c, "/api/logs/retro-1-j10/text").await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(v["error"], "log_too_large");
    let (st, _) = get(&app, &c, "/api/logs/retro-2-j11/text").await;
    assert_eq!(st, StatusCode::NOT_FOUND, "symlink は辿らない");
}

/// 照合ログの名前にディレクトリが差し替わっていても 500 にせず 404 にする
/// （FIFO も O_NONBLOCK で開いて fstat の種別で弾くので止まらない）
#[tokio::test]
async fn text_404s_when_the_log_is_not_a_regular_file() {
    let app = build().await;
    let c = cookie(&app).await;
    let conn = app.conn();
    album(&conn, 1, "A/One");
    job(&conn, 10, 1);
    verification(
        &conn,
        1,
        "retro",
        Some(10),
        Some(1),
        "ctdb",
        "verified",
        100,
        Some("/x/1.log"),
    );
    std::fs::create_dir(app.verify_dir().join("1.log")).unwrap();
    let (st, v) = get_json(&app, &c, "/api/logs/retro-1-j10/text").await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["error"], "log_not_found");
}

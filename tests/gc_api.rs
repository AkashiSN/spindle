//! `GET /api/gc/preview`（dry-run。何も消さない）と `POST /api/gc`（ジョブ投入）。SPEC §9、P1-11、D-56

#![cfg(target_os = "linux")]

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use rusqlite::{params, Connection};
use serde_json::Value;
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::{now_epoch, Db};
use spindle::fsroot::RootDir;
use spindle::gc::GcRoots;
use spindle::media::artwork::ArtworkStore;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

struct App {
    router: Router,
    dir: tempfile::TempDir,
    db_path: std::path::PathBuf,
}

async fn build(with_gc: bool) -> App {
    let dir = tempfile::tempdir().unwrap();
    for d in ["Library", "Archive", "Derived", "thumbs"] {
        std::fs::create_dir(dir.path().join(d)).unwrap();
    }
    let db_path = dir.path().join("spindle.db");
    let db = Arc::new(Db::open(&db_path).unwrap());
    let config = Arc::new(Config::parse(EXAMPLE).unwrap());
    let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
        .await
        .unwrap();
    let mut state = AppState::new(config, db, mode);
    if with_gc {
        state = state.with_gc(Arc::new(GcRoots {
            library: Arc::new(RootDir::open(&dir.path().join("Library")).unwrap()),
            archive: Arc::new(RootDir::open(&dir.path().join("Archive")).unwrap()),
            derived: Arc::new(RootDir::open(&dir.path().join("Derived")).unwrap()),
            artwork: Arc::new(ArtworkStore::new(dir.path().join("thumbs"))),
        }));
    }
    App {
        router: api::router(state),
        dir,
        db_path,
    }
}

impl App {
    fn conn(&self) -> Connection {
        Connection::open(&self.db_path).unwrap()
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

async fn body_json(res: axum::response::Response) -> Value {
    let body = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap_or(Value::Null)
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

async fn call(app: &App, c: &str, method: Method, uri: &str) -> (StatusCode, Value) {
    let r = req(method, uri)
        .header(header::COOKIE, c)
        .header("sec-fetch-site", "same-origin")
        .body(Body::empty())
        .unwrap();
    let res = send(app, r).await;
    let status = res.status();
    (status, body_json(res).await)
}

#[tokio::test]
async fn preview_reports_counts_without_deleting_and_post_enqueues_once() {
    let app = build(true).await;
    let c = cookie(&app).await;
    let now = now_epoch();
    app.conn()
        .execute(
            "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                                 audio_version, tag_version, seen_at, missing_since)
             VALUES (1, 'A/gone.flac', 'a/gone.flac', 1, 0, 0, 'flac', 1, 1, 1, 0, ?1)",
            [now - 31 * 86_400],
        )
        .unwrap();
    app.conn()
        .execute(
            "INSERT INTO archived_files (id, rel_path, rel_path_key, source_rel_path, reason, archived_at,
                                         eligible_after, state)
             VALUES (1, 'A/1.m4a', 'a/1.m4a', 'x', 'normalize', ?1, ?2, 'held')",
            params![now - 40 * 86_400, now - 86_400],
        )
        .unwrap();
    std::fs::create_dir_all(app.dir.path().join("Archive/A")).unwrap();
    std::fs::write(app.dir.path().join("Archive/A/1.m4a"), b"12345").unwrap();

    let (status, body) = call(&app, &c, Method::GET, "/api/gc/preview").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tracks"]["count"], 1);
    assert_eq!(body["tracks"]["sample"][0], "A/gone.flac");
    assert_eq!(body["archived"]["count"], 1);
    assert_eq!(body["archived"]["bytes"], 5);
    assert_eq!(body["derived"]["count"], 0);
    assert_eq!(body["artwork_dirs"]["count"], 0);
    assert!(body["cutoff"].as_i64().unwrap() <= now - 30 * 86_400);
    // 何も消えていない
    let n: i64 = app
        .conn()
        .query_row("SELECT count(*) FROM tracks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    assert!(app.dir.path().join("Archive/A/1.m4a").exists());

    // POST は 202 でジョブを投入、未完了の間は 409
    let (status, body) = call(&app, &c, Method::POST, "/api/gc").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let job_id = body["job_id"].as_i64().unwrap();
    let ty: String = app
        .conn()
        .query_row("SELECT type FROM jobs WHERE id = ?1", [job_id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(ty, "gc");
    let (status, body) = call(&app, &c, Method::POST, "/api/gc").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "duplicate");

    // 未認証は 401
    let r = req(Method::GET, "/api/gc/preview")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.status(), StatusCode::UNAUTHORIZED);
    let r = req(Method::POST, "/api/gc")
        .header("sec-fetch-site", "same-origin")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn preview_is_503_without_gc_roots() {
    let app = build(false).await;
    let c = cookie(&app).await;
    let (status, body) = call(&app, &c, Method::GET, "/api/gc/preview").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "gc_unavailable");
}

//! `POST /api/verify { selection }`（SPEC §9、P2-9）。selection のトラックが属するアルバムを
//! album 単位の `verify` ジョブに投入する。ワーカーは起こさない（ジョブ本体は tests/verify_job.rs）

#![cfg(target_os = "linux")]

mod common;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use rusqlite::Connection;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::Db;
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, Scanner};

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

struct App {
    router: Router,
    dir: tempfile::TempDir,
    scanner: Scanner,
    #[allow(dead_code)]
    shutdown: CancellationToken,
}

impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Library");
        std::fs::create_dir(&lib).unwrap();
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let config = Arc::new(Config::parse(EXAMPLE).unwrap());
        let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
            .await
            .unwrap();
        let root = Arc::new(RootDir::open(&lib).unwrap());
        let scanner = Scanner::new(db.clone(), root.clone(), 2);
        let state = AppState::new(config, db, mode);
        Self {
            router: api::router(state.clone()),
            dir,
            scanner,
            shutdown: CancellationToken::new(),
        }
    }

    fn add(&self, rel: &str, ext: &str, album: &str, track: u32) -> PathBuf {
        let p = self.dir.path().join("Library").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        common::make_audio(p.parent().unwrap(), &name, ext, 3).unwrap();
        common::set_basic_tags(&p, "t", "Artist", album, "AlbumArtist", track, 1);
        p
    }

    async fn scan(&self) {
        self.scanner
            .run(
                ScanKind::Incremental,
                Arc::new(|_, _, _| {}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }

    fn conn(&self) -> Connection {
        Connection::open(self.dir.path().join("spindle.db")).unwrap()
    }

    fn track_id(&self, rel: &str) -> i64 {
        self.conn()
            .query_row("SELECT id FROM tracks WHERE rel_path = ?1", [rel], |r| {
                r.get(0)
            })
            .unwrap()
    }

    fn jobs(&self) -> Vec<(String, String)> {
        let conn = self.conn();
        let mut st = conn
            .prepare("SELECT dedup_key, state FROM jobs WHERE type = 'verify' ORDER BY id")
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
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

    async fn post(&self, c: &str, body: Value) -> (StatusCode, Value) {
        let r = req(Method::POST, "/api/verify")
            .header(header::COOKIE, c)
            .header(header::CONTENT_TYPE, "application/json")
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(body.to_string()))
            .unwrap();
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

#[tokio::test]
async fn selection_is_folded_into_album_jobs_and_deduplicated() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let app = App::new().await;
    app.add("A/One/01.flac", "flac", "One", 1);
    app.add("A/One/02.flac", "flac", "One", 2);
    app.add("A/Two/01.flac", "flac", "Two", 1);
    app.scan().await;
    let c = app.cookie().await;
    let ids = vec![
        app.track_id("A/One/01.flac"),
        app.track_id("A/One/02.flac"),
        app.track_id("A/Two/01.flac"),
    ];
    let (st, body) = app.post(&c, json!({ "selection": { "ids": ids } })).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["albums"], 2);
    assert_eq!(body["duplicates"], 0);
    assert_eq!(body["job_ids"].as_array().unwrap().len(), 2);
    let jobs = app.jobs();
    assert_eq!(jobs.len(), 2);
    assert!(jobs
        .iter()
        .all(|(k, s)| k.starts_with("verify:") && s == "queued"));

    // 同じアルバムをもう一度 → 全部重複で 409
    let (st, body) = app
        .post(
            &c,
            json!({ "selection": { "ids": [app.track_id("A/One/01.flac")] } }),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "no_changes");
    assert_eq!(app.jobs().len(), 2);
}

#[tokio::test]
async fn non_flac_and_unknown_tracks_are_still_folded_by_album() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let app = App::new().await;
    // 非 FLAC でもアルバムは投入される（適格判定はジョブ側。unverifiable として記録される）
    app.add("A/Op/01.opus", "opus", "Op", 1);
    app.scan().await;
    let c = app.cookie().await;
    let (st, body) = app
        .post(
            &c,
            json!({ "selection": { "ids": [app.track_id("A/Op/01.opus"), 999_999] } }),
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["albums"], 1);
    let (st, _) = app
        .post(&c, json!({ "selection": { "ids": [999_999] } }))
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
}

#[tokio::test]
async fn requires_a_session_and_a_valid_body() {
    let app = App::new().await;
    let (st, _) = app.post("", json!({ "selection": { "ids": [1] } })).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let c = app.cookie().await;
    let (st, body) = app.post(&c, json!({ "nope": 1 })).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
}

//! `POST /api/rg`（SPEC §9、docs/TASKS.md P1-1）。selection に含まれるトラックの album ごとに
//! `rg` ジョブを投入する。album を持たないトラックは track 単位。合成ファイルは ffmpeg で作る

#![cfg(target_os = "linux")]

mod common;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use rusqlite::Connection;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::{now_epoch, Db};
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, Scanner};
use spindle::jobs::handlers::rg::RgHandler;
use spindle::jobs::{JobType, Registry};
use spindle::media::decode::Decoder;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

struct App {
    state: AppState,
    router: Router,
    dir: tempfile::TempDir,
    scanner: Scanner,
    root: Arc<RootDir>,
    shutdown: CancellationToken,
}

impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Library")).unwrap();
        let lib = dir.path().join("Library");
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
            state,
            dir,
            scanner,
            root,
            shutdown: CancellationToken::new(),
        }
    }

    fn start_worker(&self) {
        let mut reg = Registry::new();
        reg.register(
            JobType::Rg,
            Arc::new(RgHandler::new(
                self.root.clone(),
                Decoder::new("ffmpeg"),
                -18.0,
            )),
        );
        self.state.jobs.start(reg, self.shutdown.clone());
    }

    fn lib(&self) -> PathBuf {
        self.dir.path().join("Library")
    }

    fn add(&self, rel: &str, title: &str) -> PathBuf {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        common::make_audio(p.parent().unwrap(), &name, "flac", 3).unwrap();
        common::set_basic_tags(&p, title, "Artist", "Album", "AlbumArtist", 1, 1);
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
        let c = self.conn();
        let mut st = c
            .prepare("SELECT dedup_key, state FROM jobs WHERE type = 'rg' ORDER BY id")
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn scanned_at(&self, rel: &str) -> Option<i64> {
        self.conn()
            .query_row(
                "SELECT rg_scanned_at FROM tracks WHERE rel_path = ?1",
                [rel],
                |r| r.get(0),
            )
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

    async fn post(&self, c: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        let r = req(Method::POST, "/api/rg")
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
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn wait_all_rg_terminal(&self) {
        // CI のランナーは I/O が遅く回数ベースでは足りないので、経過時間で待つ
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while std::time::Instant::now() < deadline {
            if self
                .jobs()
                .iter()
                .all(|(_, s)| s == "done" || s == "failed" || s == "cancelled")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("rg ジョブが終端にならない: {:?}", self.jobs());
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn req(method: Method, uri: &str) -> axum::http::request::Builder {
    let peer: SocketAddr = LAN.parse().unwrap();
    let mut b = Request::builder().method(method).uri(uri);
    b.extensions_mut().unwrap().insert(ConnectInfo(peer));
    b
}

#[tokio::test]
async fn enqueues_one_job_per_album_in_selection() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let app = App::new().await;
    app.add("A/01.flac", "a1");
    app.add("A/02.flac", "a2");
    app.add("B/01.flac", "b1");
    app.add("C/01.flac", "c1");
    app.scan().await;
    let c = app.cookie().await;
    let ids = vec![
        app.track_id("A/01.flac"),
        app.track_id("A/02.flac"),
        app.track_id("B/01.flac"),
    ];
    let (st, body) = app
        .post(&c, serde_json::json!({ "selection": { "ids": ids } }))
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["albums"], 2, "{body}");
    assert_eq!(body["tracks"], 0, "{body}");
    assert_eq!(body["duplicates"], 0, "{body}");
    assert_eq!(body["job_ids"].as_array().unwrap().len(), 2);
    let keys: Vec<String> = app.jobs().into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys.len(), 2);
    assert!(keys.iter().all(|k| k.starts_with("rg:album:")), "{keys:?}");

    // 同じ album をもう一度投げても queued の間は重複しない
    let (st, body) = app
        .post(
            &c,
            serde_json::json!({ "selection": { "ids": [app.track_id("A/01.flac")] } }),
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["albums"], 0, "{body}");
    assert_eq!(body["duplicates"], 1, "{body}");
    assert_eq!(app.jobs().len(), 2);

    app.start_worker();
    app.wait_all_rg_terminal().await;
    assert!(
        app.jobs().iter().all(|(_, s)| s == "done"),
        "{:?}",
        app.jobs()
    );
    for rel in ["A/01.flac", "A/02.flac", "B/01.flac"] {
        assert!(app.scanned_at(rel).is_some(), "{rel}");
    }
    assert_eq!(app.scanned_at("C/01.flac"), None, "選択外は触らない");
}

#[tokio::test]
async fn filter_selection_with_no_rg_flag_covers_unscanned_albums() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let app = App::new().await;
    app.add("A/01.flac", "a1");
    app.add("B/01.flac", "b1");
    app.scan().await;
    // A は解析済みにしておく
    app.conn()
        .execute(
            "UPDATE tracks SET rg_scanned_at = ?1 WHERE rel_path = 'A/01.flac'",
            [now_epoch()],
        )
        .unwrap();
    let c = app.cookie().await;
    let (st, body) = app
        .post(
            &c,
            serde_json::json!({ "selection": { "filter": r#"{"flags":["no_rg"]}"# } }),
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["albums"], 1, "{body}");
    let keys: Vec<String> = app.jobs().into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        keys,
        vec![format!("rg:album:{}", album_of(&app, "B/01.flac"))]
    );
}

#[tokio::test]
async fn tracks_without_album_get_track_jobs() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let app = App::new().await;
    app.add("A/01.flac", "a1");
    app.scan().await;
    let id = app.track_id("A/01.flac");
    app.conn()
        .execute("UPDATE tracks SET album_id = NULL WHERE id = ?1", [id])
        .unwrap();
    let c = app.cookie().await;
    let (st, body) = app
        .post(&c, serde_json::json!({ "selection": { "ids": [id] } }))
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["albums"], 0, "{body}");
    assert_eq!(body["tracks"], 1, "{body}");
    let keys: Vec<String> = app.jobs().into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys, vec![format!("rg:track:{id}")]);
}

#[tokio::test]
async fn empty_selection_is_no_changes() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let app = App::new().await;
    app.add("A/01.flac", "a1");
    app.scan().await;
    let c = app.cookie().await;
    let (st, body) = app
        .post(&c, serde_json::json!({ "selection": { "ids": [999_999] } }))
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "no_changes");
    assert!(app.jobs().is_empty());
}

#[tokio::test]
async fn bad_body_is_400_and_unauthenticated_is_401() {
    let app = App::new().await;
    let c = app.cookie().await;
    let (st, _) = app
        .post(&c, serde_json::json!({ "selection": { "filter": "{" } }))
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = app.post(&c, serde_json::json!({})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = app
        .post(
            "session=nope",
            serde_json::json!({ "selection": { "ids": [1] } }),
        )
        .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

fn album_of(app: &App, rel: &str) -> i64 {
    app.conn()
        .query_row(
            "SELECT album_id FROM tracks WHERE rel_path = ?1",
            [rel],
            |r| r.get(0),
        )
        .unwrap()
}

//! `POST /api/flaccheck { selection }`（SPEC §9、P1-5、D-57）と、一覧の `flac_check` /
//! フィルタ `flac_unchecked` / `flac_error`。合成ファイルは ffmpeg、検査は flac（無ければ skip）

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
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::Db;
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, Scanner};
use spindle::jobs::handlers::flaccheck::FlaccheckHandler;
use spindle::jobs::{JobType, Registry};

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

fn flac_available() -> bool {
    std::process::Command::new("flac")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

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
            JobType::Flaccheck,
            Arc::new(FlaccheckHandler::new(self.root.clone(), "flac")),
        );
        self.state.jobs.start(reg, self.shutdown.clone());
    }

    fn add(&self, rel: &str, ext: &str) -> PathBuf {
        let p = self.dir.path().join("Library").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        common::make_audio(p.parent().unwrap(), &name, ext, 3).unwrap();
        common::set_basic_tags(&p, "t", "Artist", "Album", "AlbumArtist", 1, 1);
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
        let r = req(Method::POST, "/api/flaccheck")
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

    async fn list(&self, c: &str, filter: Option<&str>) -> Vec<Value> {
        let uri = match filter {
            Some(f) => format!("/api/tracks?filter={}", urlenc(f)),
            None => "/api/tracks".to_owned(),
        };
        let r = req(Method::GET, &uri)
            .header(header::COOKIE, c)
            .body(Body::empty())
            .unwrap();
        let res = self.router.clone().oneshot(r).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        v["items"].as_array().unwrap().clone()
    }

    async fn wait_all_terminal(&self) {
        // CI のランナーは I/O が遅く回数ベースでは足りないので、経過時間で待つ
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while std::time::Instant::now() < deadline {
            let pending: i64 = self
                .conn()
                .query_row(
                    "SELECT count(*) FROM jobs WHERE type = 'flaccheck' AND state IN ('queued','running')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            if pending == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("flaccheck ジョブが終端にならない");
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

#[tokio::test]
async fn selection_enqueues_active_flac_and_list_exposes_results_and_flags() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    if !flac_available() {
        eprintln!("flac が無いので skip");
        return;
    }
    let app = App::new().await;
    app.add("A/a.flac", "flac");
    app.add("A/b.flac", "flac");
    app.add("A/c.opus", "opus");
    app.scan().await;
    let c = app.cookie().await;
    let (a, b, o) = (
        app.track_id("A/a.flac"),
        app.track_id("A/b.flac"),
        app.track_id("A/c.opus"),
    );

    // 検査前: FLAC は flac_unchecked、行の flac_check は null
    let items = app.list(&c, Some(r#"{"flags":["flac_unchecked"]}"#)).await;
    let mut ids: Vec<i64> = items.iter().map(|t| t["id"].as_i64().unwrap()).collect();
    ids.sort_unstable();
    let mut expected = vec![a, b];
    expected.sort_unstable();
    assert_eq!(ids, expected);
    let all = app.list(&c, None).await;
    assert!(all.iter().all(|t| t["flac_check"].is_null()), "{all:?}");

    let (status, body) = app
        .post(&c, json!({ "selection": { "ids": [a, o, b] } }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["tracks"], 2);
    assert_eq!(body["skipped"], 1);
    assert_eq!(body["duplicates"], 0);
    assert_eq!(body["job_ids"].as_array().unwrap().len(), 2);
    // 同じ選択の再投入は重複
    let (status, body) = app.post(&c, json!({ "selection": { "ids": [a] } })).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["duplicates"], 1);

    app.start_worker();
    app.wait_all_terminal().await;
    // b を壊れた扱いにして（結果を直接書く）フィルタとバッジ用の値を確認
    app.conn()
        .execute(
            "UPDATE tracks SET flac_check = 'decode_error', flac_check_error = 'boom' WHERE id = ?1",
            [b],
        )
        .unwrap();
    let all = app.list(&c, None).await;
    let of = |id: i64| all.iter().find(|t| t["id"] == id).unwrap()["flac_check"].clone();
    assert_eq!(of(a)["status"], "ok");
    assert_eq!(of(a)["stale"], false);
    assert!(of(a)["checked_at"].as_i64().unwrap() > 0);
    assert_eq!(of(b)["status"], "decode_error");
    assert!(of(o).is_null());
    let err: Vec<i64> = app
        .list(&c, Some(r#"{"flags":["flac_error"]}"#))
        .await
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    assert_eq!(err, vec![b]);
    assert!(app
        .list(&c, Some(r#"{"flags":["flac_unchecked"]}"#))
        .await
        .is_empty());
    // 版が進むと stale になり flac_unchecked に戻る
    app.conn()
        .execute(
            "UPDATE tracks SET audio_version = audio_version + 1 WHERE id = ?1",
            [a],
        )
        .unwrap();
    let all = app.list(&c, None).await;
    assert_eq!(of_id(&all, a)["stale"], true);
    let un: Vec<i64> = app
        .list(&c, Some(r#"{"flags":["flac_unchecked"]}"#))
        .await
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    assert_eq!(un, vec![a]);

    // 対象が無ければ 409、未認証は 401
    let (status, body) = app.post(&c, json!({ "selection": { "ids": [o] } })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "no_changes");
    let r = req(Method::POST, "/api/flaccheck")
        .header(header::CONTENT_TYPE, "application/json")
        .header("sec-fetch-site", "same-origin")
        .body(Body::from(
            json!({ "selection": { "ids": [a] } }).to_string(),
        ))
        .unwrap();
    assert_eq!(
        app.router.clone().oneshot(r).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
}

fn of_id(all: &[Value], id: i64) -> Value {
    all.iter().find(|t| t["id"] == id).unwrap()["flac_check"].clone()
}

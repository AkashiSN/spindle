//! `POST /api/hirescheck { selection }`（SPEC §9、P3-5、D-71）と、一覧の `hires_check` /
//! 固定フィルタ `hires_unchecked` / `hires_suspect`。合成ファイルは ffmpeg で作る（無ければ skip）

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
use spindle::jobs::handlers::hirescheck::HirescheckHandler;
use spindle::jobs::{JobType, Registry};
use spindle::media::decode::Decoder;
use spindle::media::hires::Thresholds;

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
            JobType::Hirescheck,
            Arc::new(HirescheckHandler::new(
                self.root.clone(),
                Decoder::new("ffmpeg"),
                Thresholds {
                    cutoff_hz: 25_000,
                    cliff_db: 10.0,
                    hard_cutoff_hz: 22_500,
                },
            )),
        );
        self.state.jobs.start(reg, self.shutdown.clone());
    }

    /// 24 bit / `rate` のステレオ白色雑音（`pad` なら下位 8 bit ゼロ、`bits` 16 なら 16 bit）
    fn add(&self, rel: &str, rate: u32, bits: u32, pad: bool) -> PathBuf {
        let p = self.dir.path().join("Library").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let samples: Vec<i32> = (0..rate * 2)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let v = ((x >> 40) as i32 & 0x00ff_ffff) - 0x0080_0000;
                let v = if bits == 16 { v >> 8 } else { v };
                if pad {
                    v & !0xff
                } else {
                    v
                }
            })
            .collect();
        let wav = p.with_extension("src.wav");
        common::write_wav_ex(&wav, &samples, bits, rate, 2);
        common::encode(&wav, &p, &["-c:a", "flac"]).unwrap();
        std::fs::remove_file(&wav).unwrap();
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
        let r = req(Method::POST, "/api/hirescheck")
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

    async fn wait_idle(&self) {
        // CI のランナーは I/O が遅く回数ベースでは足りないので、経過時間で待つ
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
        while std::time::Instant::now() < deadline {
            let pending: i64 = self
                .conn()
                .query_row(
                    "SELECT count(*) FROM jobs WHERE type = 'hirescheck' AND state IN ('queued','running')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            if pending == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("hirescheck ジョブが終端にならない");
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
async fn post_enqueues_targets_and_list_exposes_results_and_flags() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let app = App::new().await;
    app.add("A/hi.flac", 96_000, 24, false);
    app.add("A/padded.flac", 96_000, 24, true);
    app.add("A/cd.flac", 44_100, 16, false);
    app.scan().await;
    let c = app.cookie().await;
    let (hi, padded, cd) = (
        app.track_id("A/hi.flac"),
        app.track_id("A/padded.flac"),
        app.track_id("A/cd.flac"),
    );

    // 未検査フィルタは対象の 2 件（16/44 は対象外なので出ない）
    let un: Vec<i64> = app
        .list(&c, Some(r#"{"flags":["hires_unchecked"]}"#))
        .await
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    let mut un = un;
    un.sort_unstable();
    let mut expected = vec![hi, padded];
    expected.sort_unstable();
    assert_eq!(un, expected, "一覧はアルバム順なので id 順に揃えて比べる");
    assert_eq!(
        of_id(&app.list(&c, None).await, hi),
        Value::Null,
        "未検査は null"
    );

    let (status, body) = app
        .post(&c, json!({ "selection": { "ids": [hi, padded, cd] } }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["tracks"], 2);
    assert_eq!(body["skipped"], 1);
    assert_eq!(body["duplicates"], 0);
    // 投入済みは duplicates
    let (status, body) = app.post(&c, json!({ "selection": { "ids": [hi] } })).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["duplicates"], 1);

    app.start_worker();
    app.wait_idle().await;
    let all = app.list(&c, None).await;
    let h = of_id(&all, hi);
    assert_eq!(h["status"], "ok", "{h}");
    assert_eq!(h["stale"], false);
    assert_eq!(h["cutoff_hz"], 48_000);
    assert_eq!(h["cliff_db"], Value::Null);
    assert_eq!(h["effective_bits"], 24);
    assert!(h["checked_at"].is_i64());
    assert_eq!(h["error"], Value::Null);
    let p = of_id(&all, padded);
    assert_eq!(p["status"], "padded", "{p}");
    assert_eq!(p["effective_bits"], 16);
    assert_eq!(of_id(&all, cd), Value::Null);

    // suspect フィルタは padded だけ。未検査は空
    let sus: Vec<i64> = app
        .list(&c, Some(r#"{"flags":["hires_suspect"]}"#))
        .await
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    assert_eq!(sus, vec![padded]);
    assert!(app
        .list(&c, Some(r#"{"flags":["hires_unchecked"]}"#))
        .await
        .is_empty());

    // 版が進むと stale になり、未検査フィルタに戻る
    app.conn()
        .execute(
            "UPDATE tracks SET audio_version = audio_version + 1 WHERE id = ?1",
            [hi],
        )
        .unwrap();
    assert_eq!(of_id(&app.list(&c, None).await, hi)["stale"], true);
    let un: Vec<i64> = app
        .list(&c, Some(r#"{"flags":["hires_unchecked"]}"#))
        .await
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    assert_eq!(un, vec![hi]);

    // 対象が無ければ 409、未認証は 401
    let (status, body) = app.post(&c, json!({ "selection": { "ids": [cd] } })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "no_changes");
    let r = req(Method::POST, "/api/hirescheck")
        .header(header::CONTENT_TYPE, "application/json")
        .header("sec-fetch-site", "same-origin")
        .body(Body::from(
            json!({ "selection": { "ids": [hi] } }).to_string(),
        ))
        .unwrap();
    assert_eq!(
        app.router.clone().oneshot(r).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
}

fn of_id(all: &[Value], id: i64) -> Value {
    all.iter().find(|t| t["id"] == id).unwrap()["hires_check"].clone()
}

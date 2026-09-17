//! `POST /api/normalize/preview` / `POST /api/normalize/apply`（SPEC §9 / §7.4、docs/TASKS.md P1-4、
//! D-33）。合成ファイルは ffmpeg で作り、エンコードには flac を使う（どちらかが無ければ skip）

#![cfg(target_os = "linux")]

mod common;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
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
use spindle::db::history::{self, BatchState};
use spindle::db::Db;
use spindle::edit::{Editor, NewTagOp, NormalizeEnv, TagChange};
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, ScanReport, Scanner};
use spindle::jobs::handlers::normalize::NormalizeHandler;
use spindle::jobs::{JobType, Registry};
use spindle::media::encode::FlacEncoder;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

fn flac_bin() -> Option<PathBuf> {
    let p = Command::new("flac").arg("--version").output().ok()?;
    p.status.success().then(|| PathBuf::from("flac"))
}

macro_rules! require_tools {
    () => {
        if common::ffmpeg().is_none() || flac_bin().is_none() {
            eprintln!("ffmpeg / flac が無いので skip");
            return;
        }
    };
}

struct App {
    state: AppState,
    router: Router,
    dir: tempfile::TempDir,
    scanner: Scanner,
    editor: Arc<Editor>,
    shutdown: CancellationToken,
}

impl App {
    async fn new() -> Self {
        Self::with_config(EXAMPLE).await
    }

    async fn with_config(toml: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["Library", "Archive", "tmp"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let lib = dir.path().join("Library");
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let config = Arc::new(Config::parse(toml).unwrap());
        let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
            .await
            .unwrap();
        let root = Arc::new(RootDir::open(&lib).unwrap());
        let archive = Arc::new(RootDir::open(&dir.path().join("Archive")).unwrap());
        let scanner = Scanner::new(db.clone(), root.clone(), 2);
        let state = AppState::new(config, db.clone(), mode);
        let editor = Arc::new(Editor::new(db, root, state.jobs.clone()).with_normalize(
            NormalizeEnv {
                archive,
                encoder: FlacEncoder::new("ffmpeg", "flac", 8, dir.path().join("tmp")),
                retention_days: 30,
            },
        ));
        let state = state.with_editor(editor.clone());
        Self {
            router: api::router(state.clone()),
            state,
            dir,
            scanner,
            editor,
            shutdown: CancellationToken::new(),
        }
    }

    fn start_worker(&self) {
        let mut reg = Registry::new();
        reg.register(
            JobType::Normalize,
            Arc::new(NormalizeHandler::new(self.editor.clone())),
        );
        self.state.jobs.start(reg, self.shutdown.clone());
    }

    fn lib(&self) -> PathBuf {
        self.dir.path().join("Library")
    }

    fn add(&self, rel: &str, title: &str) -> PathBuf {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let ext = rel.rsplit_once('.').unwrap().1;
        match ext {
            "wav" => common::write_wav(&p, &common::pcm_samples(1), 16),
            "m4a" => {
                let name = p.file_name().unwrap().to_str().unwrap().to_owned();
                common::make_audio(p.parent().unwrap(), &name, "alac.m4a", 2).unwrap();
            }
            "flac" => {
                let name = p.file_name().unwrap().to_str().unwrap().to_owned();
                common::make_audio(p.parent().unwrap(), &name, "flac", 3).unwrap();
            }
            other => panic!("unsupported {other}"),
        }
        common::set_basic_tags(&p, title, "Artist", "Album", "AlbumArtist", 1, 1);
        p
    }

    async fn scan(&self) -> ScanReport {
        self.scanner
            .run(
                ScanKind::Incremental,
                Arc::new(|_, _| {}),
                CancellationToken::new(),
            )
            .await
            .unwrap()
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

    fn rel_path_and_codec(&self, id: i64) -> (String, String) {
        self.conn()
            .query_row(
                "SELECT rel_path, codec FROM tracks WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
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

    async fn call(
        &self,
        c: &str,
        method: Method,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let r = req(method, uri)
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

    async fn preview(&self, c: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        self.call(c, Method::POST, "/api/normalize/preview", body)
            .await
    }

    async fn apply(&self, c: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        self.call(c, Method::POST, "/api/normalize/apply", body)
            .await
    }

    async fn wait_batch(&self, id: i64) -> BatchState {
        for _ in 0..3000 {
            let st = history::get_batch(&self.conn(), id).unwrap().unwrap().state;
            if st.is_terminal() {
                return st;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("batch {id} が終端にならない");
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

// ---------------------------------------------------------------- preview

#[tokio::test]
async fn preview_returns_token_counts_and_destinations_and_excludes_pending() {
    require_tools!();
    let app = App::new().await;
    app.add("A/01.wav", "a");
    app.add("A/02.m4a", "b");
    app.add("A/03.flac", "c");
    app.add("A/04.wav", "p");
    app.scan().await;
    let c = app.cookie().await;
    let ids: Vec<i64> = ["A/01.wav", "A/02.m4a", "A/03.flac", "A/04.wav"]
        .iter()
        .map(|r| app.track_id(r))
        .collect();
    // 4 件目は反映待ち
    app.editor
        .prepare_tags(
            None,
            vec![NewTagOp {
                track_id: ids[3],
                changes: vec![TagChange {
                    key: "TITLE".into(),
                    values: Some(vec!["pending".into()]),
                }],
            }],
        )
        .await
        .unwrap();

    let (status, body) = app
        .preview(&c, serde_json::json!({ "selection": { "ids": ids } }))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["selection_token"]
        .as_str()
        .is_some_and(|t| !t.is_empty()));
    assert_eq!(body["count"], 4);
    assert_eq!(body["pending_excluded"], 1);
    assert_eq!(body["changed"], 2);
    assert_eq!(body["unchanged"], 1);
    assert_eq!(body["conflict"], 0);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{items:?}");
    let a = items.iter().find(|i| i["id"] == ids[0]).unwrap();
    assert_eq!(a["old"], "A/01.wav");
    assert_eq!(a["codec"], "wav");
    assert_eq!(a["new"], "A/01.flac");
    let b = items.iter().find(|i| i["id"] == ids[1]).unwrap();
    assert_eq!(b["codec"], "alac");
    assert_eq!(b["new"], "A/02.flac");
}

#[tokio::test]
async fn preview_reports_conflict_when_destination_is_taken() {
    require_tools!();
    let app = App::new().await;
    app.add("A/01.wav", "a");
    app.add("A/01.flac", "other");
    app.scan().await;
    let c = app.cookie().await;
    let (status, body) = app
        .preview(&c, serde_json::json!({ "selection": { "filter": "{}" } }))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], 0);
    assert_eq!(body["unchanged"], 1);
    assert_eq!(body["conflict"], 1);
    let item = &body["items"][0];
    assert!(item["new"].is_null());
    assert!(item["reason"].as_str().unwrap().contains("占有"));
}

#[tokio::test]
async fn preview_rejects_bad_selection() {
    require_tools!();
    let app = App::new().await;
    app.add("A/01.wav", "a");
    app.scan().await;
    let c = app.cookie().await;
    for body in [
        serde_json::json!({ "selection": { "filter": "{\"bogus\":1}" } }),
        serde_json::json!({ "selection": { "ids": [1] }, "sort": "sideways" }),
        serde_json::json!({}),
    ] {
        let (status, _) = app.preview(&c, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
}

#[tokio::test]
async fn disabled_by_config_is_409() {
    require_tools!();
    let toml = EXAMPLE.replace("wav_to_flac = true", "wav_to_flac = false");
    let app = App::with_config(&toml).await;
    app.add("A/01.wav", "a");
    app.scan().await;
    let c = app.cookie().await;
    let (status, body) = app
        .preview(&c, serde_json::json!({ "selection": { "filter": "{}" } }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "normalize_disabled");
    let (status, body) = app
        .apply(&c, serde_json::json!({ "selection_token": "x" }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "normalize_disabled");
}

// ---------------------------------------------------------------- apply

#[tokio::test]
async fn apply_records_batch_and_worker_converts() {
    require_tools!();
    let app = App::new().await;
    let wav = app.add("A/01.wav", "a");
    app.add("A/02.m4a", "b");
    app.add("A/03.flac", "c");
    app.scan().await;
    let c = app.cookie().await;
    let (status, body) = app
        .preview(&c, serde_json::json!({ "selection": { "filter": "{}" } }))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let token = body["selection_token"].as_str().unwrap().to_owned();
    let (status, body) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": token, "description": "FLAC 化" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["affected"], 2);
    assert_eq!(body["conflict"], 0);
    let batch_id = body["batch_id"].as_i64().unwrap();
    // token は消費済み
    let (status, body) = app
        .apply(&c, serde_json::json!({ "selection_token": token }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "preview_stale");

    // 履歴に kind=archive で載る
    let (status, body) = app
        .call(&c, Method::GET, "/api/history", serde_json::json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let item = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == batch_id)
        .unwrap();
    assert_eq!(item["kind"], "archive");
    assert_eq!(item["description"], "FLAC 化");

    app.start_worker();
    assert_eq!(app.wait_batch(batch_id).await, BatchState::Applied);
    let a = app.track_id("A/01.flac");
    assert_eq!(
        app.rel_path_and_codec(a),
        ("A/01.flac".to_owned(), "flac".to_owned())
    );
    assert!(!wav.exists());
    assert!(app.dir.path().join("Archive/A/01.wav").exists());
    assert!(app.dir.path().join("Archive/A/02.m4a").exists());

    // op の詳細: edits に rel_path / codec
    let (status, body) = app
        .call(
            &c,
            Method::GET,
            &format!("/api/history/{batch_id}"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ops = body["ops"].as_array().unwrap();
    assert_eq!(ops.len(), 2);
    let op = ops.iter().find(|o| o["track_id"] == a).unwrap();
    assert_eq!(op["kind"], "archive");
    assert_eq!(op["result"], "applied");
    assert_eq!(op["edits"]["rel_path"]["old"], "A/01.wav");
    assert_eq!(op["edits"]["rel_path"]["new"], "A/01.flac");
    assert_eq!(op["edits"]["codec"]["new"], "flac");

    // 巻き戻しも API から
    let (status, body) = app
        .call(
            &c,
            Method::POST,
            &format!("/api/history/{batch_id}/revert"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let reverse = body["batch_id"].as_i64().unwrap();
    assert_eq!(app.wait_batch(reverse).await, BatchState::Applied);
    assert!(wav.exists());
    assert_eq!(
        app.rel_path_and_codec(a),
        ("A/01.wav".to_owned(), "wav".to_owned())
    );
    assert!(app.dir.path().join("Archive/A/01.flac").exists());
}

#[tokio::test]
async fn apply_with_nothing_to_do_is_no_changes() {
    require_tools!();
    let app = App::new().await;
    app.add("A/03.flac", "c");
    app.scan().await;
    let c = app.cookie().await;
    let (_, body) = app
        .preview(&c, serde_json::json!({ "selection": { "filter": "{}" } }))
        .await;
    let token = body["selection_token"].as_str().unwrap().to_owned();
    let (status, body) = app
        .apply(&c, serde_json::json!({ "selection_token": token }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "no_changes");
    // 409 では token を消費しない
    let (status, body) = app
        .apply(&c, serde_json::json!({ "selection_token": token }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "no_changes");
}

#[tokio::test]
async fn apply_rejects_pending_unless_skipped() {
    require_tools!();
    let app = App::new().await;
    app.add("A/01.wav", "a");
    app.add("A/02.wav", "b");
    app.scan().await;
    let c = app.cookie().await;
    let (_, body) = app
        .preview(&c, serde_json::json!({ "selection": { "filter": "{}" } }))
        .await;
    let token = body["selection_token"].as_str().unwrap().to_owned();
    // preview の後に 1 件が反映待ちになった
    let b = app.track_id("A/02.wav");
    app.editor
        .prepare_tags(
            None,
            vec![NewTagOp {
                track_id: b,
                changes: vec![TagChange {
                    key: "TITLE".into(),
                    values: Some(vec!["pending".into()]),
                }],
            }],
        )
        .await
        .unwrap();
    let (status, body) = app
        .apply(&c, serde_json::json!({ "selection_token": token }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "pending");
    assert_eq!(body["count"], 1);
    let (status, body) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": token, "skip_pending": true }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["affected"], 1);
}

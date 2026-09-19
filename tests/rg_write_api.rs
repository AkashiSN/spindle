//! `POST /api/rg/write`（SPEC §9、docs/TASKS.md P1-2）。selection の解析済みトラックへ解析値を
//! タグとして書く編集バッチを記録する。合成ファイルは ffmpeg で作る（無ければ skip）

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
use rusqlite::{params, Connection};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::Db;
use spindle::edit::Editor;
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, Scanner};
use spindle::jobs::handlers::tagwrite::TagwriteHandler;
use spindle::jobs::{JobType, Registry};

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

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
        std::fs::create_dir(dir.path().join("Library")).unwrap();
        let lib = dir.path().join("Library");
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let config = Arc::new(Config::parse(toml).unwrap());
        let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
            .await
            .unwrap();
        let root = Arc::new(RootDir::open(&lib).unwrap());
        let scanner = Scanner::new(db.clone(), root.clone(), 2);
        let state = AppState::new(config, db.clone(), mode);
        let editor =
            Arc::new(Editor::new(db, root, state.jobs.clone()).with_replaygain_reference(-18.0));
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
            JobType::Tagwrite,
            Arc::new(TagwriteHandler::new(self.editor.clone())),
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
        let ext = rel.rsplit_once('.').unwrap().1;
        common::make_audio(p.parent().unwrap(), &name, ext, 3).unwrap();
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

    fn set_rg(&self, rel: &str, gain: f64, scanned_at: i64) {
        self.conn()
            .execute(
                "UPDATE tracks SET rg_track_gain = ?2, rg_track_peak = 0.5, rg_album_gain = ?2,
                        rg_album_peak = 0.5, rg_scanned_at = ?3, rg_written_at = NULL
                  WHERE rel_path = ?1",
                params![rel, gain, scanned_at],
            )
            .unwrap();
    }

    fn written_at(&self, rel: &str) -> Option<i64> {
        self.conn()
            .query_row(
                "SELECT rg_written_at FROM tracks WHERE rel_path = ?1",
                [rel],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn batch_state(&self, id: i64) -> String {
        self.conn()
            .query_row("SELECT state FROM edit_batches WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
    }

    async fn wait_batch_terminal(&self, id: i64) -> String {
        // CI のランナーは I/O が遅く回数ベースでは足りないので、経過時間で待つ
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while std::time::Instant::now() < deadline {
            let st = self.batch_state(id);
            if st != "prepared" && st != "applying" {
                return st;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("batch {id} が終端にならない");
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

    async fn post(
        &self,
        c: &str,
        path: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let r = req(Method::POST, path)
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

    async fn get(&self, c: &str, path: &str) -> (StatusCode, serde_json::Value) {
        let r = req(Method::GET, path)
            .header(header::COOKIE, c)
            .body(Body::empty())
            .unwrap();
        let res = self.router.clone().oneshot(r).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
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

fn file_tag(path: &std::path::Path, key: &str) -> Vec<String> {
    let ext = path.extension().and_then(|e| e.to_str());
    let af =
        spindle::domain::tags::read_audio_file(std::fs::File::open(path).unwrap(), ext).unwrap();
    af.tags.values(key).map(str::to_owned).collect()
}

#[tokio::test]
async fn write_records_batch_and_worker_writes_tags() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let app = App::new().await;
    let flac = app.add("A/01.flac", "a1");
    let opus = app.add("A/02.opus", "a2");
    app.add("B/01.flac", "b1");
    app.scan().await;
    app.set_rg("A/01.flac", -2.0, 1000);
    app.set_rg("A/02.opus", 5.0, 1000);
    // B は未解析
    let c = app.cookie().await;
    let ids = vec![
        app.track_id("A/01.flac"),
        app.track_id("A/02.opus"),
        app.track_id("B/01.flac"),
    ];
    let (st, body) = app
        .post(
            &c,
            "/api/rg/write",
            serde_json::json!({ "selection": { "ids": ids }, "description": "RG 書き込み" }),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["affected"], 2, "{body}");
    assert_eq!(body["unchanged"], 0, "{body}");
    assert_eq!(body["unscanned"], 1, "{body}");
    assert_eq!(body["pending_excluded"], 0, "{body}");
    let batch_id = body["batch_id"].as_i64().unwrap();

    // 一覧の pending バッジと rg_unwritten フィルタ
    let (st, list) = app
        .get(
            &c,
            "/api/tracks?filter=%7B%22flags%22%3A%5B%22rg_unwritten%22%5D%7D",
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{list}");
    assert_eq!(list["total"], 2, "{list}");
    assert!(list["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|i| i["pending_batch_id"] == batch_id));

    app.start_worker();
    assert_eq!(app.wait_batch_terminal(batch_id).await, "applied");
    assert_eq!(file_tag(&flac, "REPLAYGAIN_TRACK_GAIN"), ["-2.00 dB"]);
    assert_eq!(file_tag(&opus, "R128_TRACK_GAIN"), ["0"]);
    assert!(app.written_at("A/01.flac").is_some());
    assert!(app.written_at("A/02.opus").is_some());
    assert_eq!(app.written_at("B/01.flac"), None);

    let (_, list) = app
        .get(
            &c,
            "/api/tracks?filter=%7B%22flags%22%3A%5B%22rg_unwritten%22%5D%7D",
        )
        .await;
    assert_eq!(list["total"], 0, "{list}");

    // 再実行: 既に一致しているのでバッチ無しの 200
    let (st, body) = app
        .post(
            &c,
            "/api/rg/write",
            serde_json::json!({ "selection": { "ids": [app.track_id("A/01.flac")] } }),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["batch_id"], serde_json::Value::Null, "{body}");
    assert_eq!(body["unchanged"], 1, "{body}");

    // 履歴に載る
    let (st, hist) = app.get(&c, &format!("/api/history/{batch_id}")).await;
    assert_eq!(st, StatusCode::OK, "{hist}");
    assert_eq!(hist["description"], "RG 書き込み", "{hist}");
}

#[tokio::test]
async fn nothing_to_write_is_no_changes() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let app = App::new().await;
    app.add("A/01.flac", "a1");
    app.scan().await;
    let c = app.cookie().await;
    let (st, body) = app
        .post(
            &c,
            "/api/rg/write",
            serde_json::json!({ "selection": { "ids": [app.track_id("A/01.flac")] } }),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "no_changes", "{body}");
}

#[tokio::test]
async fn pending_is_409_unless_skip_pending() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let app = App::new().await;
    app.add("A/01.flac", "a1");
    app.add("A/02.flac", "a2");
    app.scan().await;
    app.set_rg("A/01.flac", 1.0, 1000);
    app.set_rg("A/02.flac", 1.0, 1000);
    let (a, b) = (app.track_id("A/01.flac"), app.track_id("A/02.flac"));
    // A/01 を反映待ちにする（ワーカー無し）
    app.editor
        .prepare_tags(
            None,
            vec![spindle::edit::NewTagOp {
                track_id: a,
                changes: vec![spindle::edit::TagChange {
                    key: "TITLE".to_owned(),
                    values: Some(vec!["x".to_owned()]),
                }],
            }],
        )
        .await
        .unwrap();
    let c = app.cookie().await;
    let (st, body) = app
        .post(
            &c,
            "/api/rg/write",
            serde_json::json!({ "selection": { "ids": [a, b] } }),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "pending", "{body}");
    assert_eq!(body["track_ids"], serde_json::json!([a]), "{body}");

    let (st, body) = app
        .post(
            &c,
            "/api/rg/write",
            serde_json::json!({ "selection": { "ids": [a, b] }, "skip_pending": true }),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["affected"], 1, "{body}");
    assert_eq!(body["pending_excluded"], 1, "{body}");
}

#[tokio::test]
async fn disabled_by_config_is_409() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let toml = EXAMPLE.replace("write_tags = true", "write_tags = false");
    let app = App::with_config(&toml).await;
    app.add("A/01.flac", "a1");
    app.scan().await;
    app.set_rg("A/01.flac", 1.0, 1000);
    let c = app.cookie().await;
    let (st, body) = app
        .post(
            &c,
            "/api/rg/write",
            serde_json::json!({ "selection": { "ids": [app.track_id("A/01.flac")] } }),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "rg_write_disabled", "{body}");
}

#[tokio::test]
async fn bad_selection_is_400() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let app = App::new().await;
    let c = app.cookie().await;
    let (st, _) = app
        .post(&c, "/api/rg/write", serde_json::json!({ "selection": {} }))
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

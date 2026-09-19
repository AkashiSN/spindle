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
use spindle::media::fingerprint::{flac_streaminfo_md5, flac_streaminfo_md5_at};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
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

/// STREAMINFO の MD5 を全ゼロにする（MD5 無しの FLAC の模擬）
fn zero_md5(path: &std::path::Path) {
    let (offset, _) = flac_streaminfo_md5_at(File::open(path).unwrap()).unwrap();
    let mut f = OpenOptions::new().write(true).open(path).unwrap();
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.write_all(&[0u8; 16]).unwrap();
}

impl App {
    fn set_check(&self, rel: &str, status: &str) {
        self.conn()
            .execute(
                "UPDATE tracks SET flac_check = ?2, flac_checked_at = 1, flac_check_version = audio_version
                 WHERE rel_path = ?1",
                params![rel, status],
            )
            .unwrap();
    }
}

#[tokio::test]
async fn fill_records_batch_and_worker_writes_md5() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let app = App::new().await;
    let a = app.add("A/a.flac", "one");
    let expected = flac_streaminfo_md5(File::open(&a).unwrap())
        .unwrap()
        .unwrap();
    zero_md5(&a);
    app.add("A/b.flac", "two"); // MD5 あり（ok）
    app.add("A/c.m4a", "three"); // FLAC でない
    app.scan().await;
    app.set_check("A/a.flac", "md5_missing");
    app.set_check("A/b.flac", "ok");
    let c = app.cookie().await;
    app.start_worker();

    let (status, body) = app
        .post(
            &c,
            "/api/md5fill",
            serde_json::json!({ "selection": { "filter": "{}" }, "description": "補填" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["affected"], 1);
    assert_eq!(body["skipped"], 2);
    assert_eq!(body["pending_excluded"], 0);
    let batch_id = body["batch_id"].as_i64().unwrap();
    assert_eq!(app.wait_batch_terminal(batch_id).await, "applied");
    assert_eq!(
        flac_streaminfo_md5(File::open(&a).unwrap()).unwrap(),
        Some(expected)
    );
    // 履歴に md5 種別で残る
    let (_, h) = app.get(&c, &format!("/api/history/{batch_id}")).await;
    assert_eq!(h["ops"][0]["kind"], "md5");
    assert_eq!(
        h["ops"][0]["edits"]["audio_md5"]["new"],
        serde_json::json!(spindle::edit::md5_hex(&expected))
    );

    // もう対象が無い
    let (status, body) = app
        .post(
            &c,
            "/api/md5fill",
            serde_json::json!({ "selection": { "filter": "{}" } }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "no_changes");
}

#[tokio::test]
async fn pending_is_409_unless_skip_pending() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let app = App::new().await;
    let a = app.add("A/a.flac", "one");
    zero_md5(&a);
    let b = app.add("A/b.flac", "two");
    zero_md5(&b);
    app.scan().await;
    app.set_check("A/a.flac", "md5_missing");
    app.set_check("A/b.flac", "md5_missing");
    let c = app.cookie().await;
    // ワーカー無しで a を pending にする
    let a_id = app.track_id("A/a.flac");
    let (status, _) = app
        .post(
            &c,
            "/api/md5fill",
            serde_json::json!({ "selection": { "ids": [a_id] } }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = app
        .post(
            &c,
            "/api/md5fill",
            serde_json::json!({ "selection": { "filter": "{}" } }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "pending");
    assert_eq!(body["track_ids"], serde_json::json!([a_id]));

    let (status, body) = app
        .post(
            &c,
            "/api/md5fill",
            serde_json::json!({ "selection": { "filter": "{}" }, "skip_pending": true }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["affected"], 1);
    assert_eq!(body["pending_excluded"], 1);
}

#[tokio::test]
async fn disabled_by_config_is_409() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let toml = EXAMPLE.replace(
        "flac_fix_missing_md5 = true",
        "flac_fix_missing_md5 = false",
    );
    assert_ne!(toml, EXAMPLE);
    let app = App::with_config(&toml).await;
    let c = app.cookie().await;
    let (status, body) = app
        .post(
            &c,
            "/api/md5fill",
            serde_json::json!({ "selection": { "filter": "{}" } }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "md5_fill_disabled");
}

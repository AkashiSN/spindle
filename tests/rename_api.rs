//! `POST /api/rename/preview` / `POST /api/rename/apply`（SPEC §9 / §7.5、docs/TASKS.md P0-11、
//! D-33）。合成ファイルは ffmpeg で 1 本作り、コピーして増やす

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
use spindle::db::history::{self, BatchState, OpResult};
use spindle::db::Db;
use spindle::edit::{Editor, NewTagOp, RenameTarget, TagChange};
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, ScanReport, Scanner};
use spindle::jobs::handlers::rename::RenameHandler;
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
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Library");
        std::fs::create_dir(&lib).unwrap();
        Self::open(dir).await
    }

    async fn open(dir: tempfile::TempDir) -> Self {
        let lib = dir.path().join("Library");
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let config = Arc::new(Config::parse(EXAMPLE).unwrap());
        let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
            .await
            .unwrap();
        let root = Arc::new(RootDir::open(&lib).unwrap());
        let scanner = Scanner::new(db.clone(), root.clone(), 2);
        let state = AppState::new(config, db.clone(), mode);
        let editor = Arc::new(Editor::new(db, root, state.jobs.clone()));
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
            JobType::Rename,
            Arc::new(RenameHandler::new(self.editor.clone())),
        );
        self.state.jobs.start(reg, self.shutdown.clone());
    }

    fn lib(&self) -> PathBuf {
        self.dir.path().join("Library")
    }

    /// `rel` に音声を作る。2 本目以降は 1 本目のコピー + タグ書き換え（ffmpeg を毎回呼ばない）
    fn add(&self, rel: &str, title: &str) -> Option<PathBuf> {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let seed = self.dir.path().join(".seed.flac");
        if !seed.exists() {
            common::make_audio(self.dir.path(), ".seed.flac", "flac", 1)?;
        }
        std::fs::copy(&seed, &p).unwrap();
        common::set_basic_tags(&p, title, "Artist", "Album", "AlbumArtist", 1, 1);
        Some(p)
    }

    async fn scan(&self) -> ScanReport {
        self.scanner
            .run(
                ScanKind::Incremental,
                Arc::new(|_, _, _| {}),
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

    fn rel_path(&self, id: i64) -> String {
        self.conn()
            .query_row("SELECT rel_path FROM tracks WHERE id = ?1", [id], |r| {
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
        self.call(c, Method::POST, "/api/rename/preview", body)
            .await
    }

    async fn apply(&self, c: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        self.call(c, Method::POST, "/api/rename/apply", body).await
    }

    async fn wait_batch(&self, id: i64) -> BatchState {
        // CI のランナーは I/O が遅く回数ベースでは足りないので、経過時間で待つ
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while std::time::Instant::now() < deadline {
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

macro_rules! setup {
    ($app:ident, $($rel:expr => $title:expr),+ $(,)?) => {{
        $( require_ffmpeg!($app.add($rel, $title)); )+
        $app.scan().await;
    }};
}

// ---------------------------------------------------------------- preview

#[tokio::test]
async fn preview_returns_token_counts_and_planned_paths_and_excludes_pending() {
    let app = App::new().await;
    setup!(app, "old/a.flac" => "a", "old/b.flac" => "b", "_Unsorted/AlbumArtist/Album/01. c.flac" => "c", "old/p.flac" => "p");
    let c = app.cookie().await;
    let ids: Vec<i64> = [
        "old/a.flac",
        "old/b.flac",
        "_Unsorted/AlbumArtist/Album/01. c.flac",
        "old/p.flac",
    ]
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
    // a と b は old/ の album（Album）、c は既に _Unsorted/AlbumArtist/Album にいる別 album →
    // 同名別リリースなので a / b は年が無く降格できず conflict、c は変更なし
    assert_eq!(body["changed"], 0);
    assert_eq!(body["unchanged"], 1);
    assert_eq!(body["conflict"], 2);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{items:?}");
    let a = items.iter().find(|i| i["id"] == ids[0]).unwrap();
    assert_eq!(a["old"], "old/a.flac");
    assert!(a["new"].is_null());
    assert!(a["reason"].as_str().is_some_and(|r| !r.is_empty()));
    assert!(items.iter().all(|i| i["id"] != ids[3]));
}

#[tokio::test]
async fn preview_plans_paths_from_layout() {
    let app = App::new().await;
    require_ffmpeg!(app.add("old/a.flac", "曲: 一"));
    let b = app.add("old/b.flac", "二").unwrap();
    common::set_basic_tags(&b, "二", "Artist", "Album", "AlbumArtist", 2, 1);
    app.scan().await;
    let c = app.cookie().await;
    let (status, body) = app
        .preview(&c, serde_json::json!({ "selection": { "filter": "{}" } }))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], 2);
    let items = body["items"].as_array().unwrap();
    let new_of = |rel: &str| {
        let id = app.track_id(rel);
        items
            .iter()
            .find(|i| i["id"] == id)
            .map(|i| i["new"].clone())
    };
    assert_eq!(
        new_of("old/a.flac"),
        Some(serde_json::json!(
            "_Unsorted/AlbumArtist/Album/01. 曲： 一.flac"
        ))
    );
    assert_eq!(
        new_of("old/b.flac"),
        Some(serde_json::json!("_Unsorted/AlbumArtist/Album/02. 二.flac"))
    );
}

#[tokio::test]
async fn preview_rejects_bad_selection() {
    let app = App::new().await;
    setup!(app, "A/01.flac" => "a");
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

// ---------------------------------------------------------------- apply

#[tokio::test]
async fn apply_creates_rename_batch_and_worker_moves_files() {
    let app = App::new().await;
    require_ffmpeg!(app.add("old/a.flac", "a"));
    let b = app.add("old/b.flac", "b").unwrap();
    common::set_basic_tags(&b, "b", "Artist", "Album", "AlbumArtist", 2, 1);
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
            serde_json::json!({ "selection_token": token, "description": "整理" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["affected"], 2);
    assert_eq!(body["conflict"], 0);
    let batch_id = body["batch_id"].as_i64().unwrap();
    let batch = history::get_batch(&app.conn(), batch_id).unwrap().unwrap();
    assert_eq!(batch.description.as_deref(), Some("整理"));
    let a = app.track_id("_Unsorted/AlbumArtist/Album/01. a.flac");
    assert_eq!(app.rel_path(a), "_Unsorted/AlbumArtist/Album/01. a.flac");

    app.start_worker();
    assert_eq!(app.wait_batch(batch_id).await, BatchState::Applied);
    assert!(app
        .lib()
        .join("_Unsorted/AlbumArtist/Album/01. a.flac")
        .exists());
    assert!(app
        .lib()
        .join("_Unsorted/AlbumArtist/Album/02. b.flac")
        .exists());
    assert!(!app.lib().join("old/a.flac").exists());
    // token は消費済み
    let (status, body) = app
        .apply(&c, serde_json::json!({ "selection_token": token }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "preview_stale");
}

#[tokio::test]
async fn apply_with_pending_tracks_is_409_unless_skip_pending() {
    let app = App::new().await;
    require_ffmpeg!(app.add("old/a.flac", "a"));
    let b = app.add("old/b.flac", "b").unwrap();
    common::set_basic_tags(&b, "b", "Artist", "Album", "AlbumArtist", 2, 1);
    app.scan().await;
    let c = app.cookie().await;
    let (_, body) = app
        .preview(&c, serde_json::json!({ "selection": { "filter": "{}" } }))
        .await;
    let token = body["selection_token"].as_str().unwrap().to_owned();
    // preview の後に b が反映待ちになった
    let b = app.track_id("old/b.flac");
    app.editor
        .prepare_rename(
            None,
            vec![RenameTarget {
                track_id: b,
                new_rel_path: "x/b.flac".to_owned(),
                expected: None,
                planned_conflict: None,
            }],
        )
        .await
        .unwrap();
    let (status, body) = app
        .apply(&c, serde_json::json!({ "selection_token": token }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "pending");
    assert_eq!(body["track_ids"], serde_json::json!([b]));
    // 同じ token で skip_pending
    let (status, body) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": token, "skip_pending": true }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["affected"], 1);
}

#[tokio::test]
async fn apply_with_nothing_to_change_is_409_no_changes() {
    let app = App::new().await;
    setup!(app, "_Unsorted/AlbumArtist/Album/01. a.flac" => "a");
    let c = app.cookie().await;
    let (_, body) = app
        .preview(&c, serde_json::json!({ "selection": { "filter": "{}" } }))
        .await;
    assert_eq!(body["unchanged"], 1);
    let token = body["selection_token"].as_str().unwrap().to_owned();
    let (status, body) = app
        .apply(&c, serde_json::json!({ "selection_token": token }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "no_changes");
    // 409 では token を消費しない
    let (status, _) = app
        .apply(&c, serde_json::json!({ "selection_token": token }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn apply_records_conflict_for_rows_whose_tags_changed_after_preview() {
    let app = App::new().await;
    setup!(app, "old/a.flac" => "a");
    let c = app.cookie().await;
    let (_, body) = app
        .preview(&c, serde_json::json!({ "selection": { "filter": "{}" } }))
        .await;
    let token = body["selection_token"].as_str().unwrap().to_owned();
    // preview の後にタグが変わった（版が進んだ）
    let a = app.track_id("old/a.flac");
    app.editor
        .prepare_tags(
            None,
            vec![NewTagOp {
                track_id: a,
                changes: vec![TagChange {
                    key: "TITLE".into(),
                    values: Some(vec!["changed".into()]),
                }],
            }],
        )
        .await
        .unwrap();
    // tagwrite は走らせず、op だけ終端にして pending を外す
    app.conn()
        .execute("UPDATE edit_ops SET result = 'applied'", [])
        .unwrap();
    let (status, body) = app
        .apply(&c, serde_json::json!({ "selection_token": token }))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["conflict"], 1);
    let batch_id = body["batch_id"].as_i64().unwrap();
    let ops = history::list_ops(&app.conn(), batch_id).unwrap();
    assert_eq!(ops[0].result, OpResult::SkippedConflict);
    assert_eq!(app.rel_path(a), "old/a.flac");
}

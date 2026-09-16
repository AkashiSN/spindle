//! `GET /api/history`、`GET /api/history/:id`、`POST /api/history/:id/revert`、
//! `POST /api/history/:id/cancel`（SPEC §9 / §7.5 / §12.4、docs/TASKS.md P0-12）。
//! 合成ファイルは ffmpeg で 1 本作り、コピーして増やす

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
use spindle::db::history::{self, BatchState};
use spindle::db::Db;
use spindle::edit::{Editor, NewTagOp, TagChange};
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, ScanReport, Scanner};
use spindle::jobs::handlers::rename::RenameHandler;
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
        reg.register(
            JobType::Tagwrite,
            Arc::new(TagwriteHandler::new(self.editor.clone())),
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

    async fn get(&self, c: &str, uri: &str) -> (StatusCode, serde_json::Value) {
        let r = req(Method::GET, uri)
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

    async fn edit(&self, ids: &[i64], title: &str) -> i64 {
        let ops = ids
            .iter()
            .map(|id| NewTagOp {
                track_id: *id,
                changes: vec![TagChange {
                    key: "TITLE".into(),
                    values: Some(vec![title.into()]),
                }],
            })
            .collect();
        self.editor.prepare_tags(None, ops).await.unwrap().batch_id
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

macro_rules! setup {
    ($app:ident, $($rel:expr => $title:expr),+ $(,)?) => {{
        $( require_ffmpeg!($app.add($rel, $title)); )+
        $app.scan().await;
    }};
}

#[tokio::test]
async fn history_lists_batches_with_counts_kind_and_revert_links() {
    let app = App::new().await;
    setup!(app, "A/01.flac" => "a", "A/02.flac" => "b");
    let c = app.cookie().await;
    let (a, b) = (app.track_id("A/01.flac"), app.track_id("A/02.flac"));
    app.start_worker();
    let orig = app.edit(&[a, b], "x").await;
    assert_eq!(app.wait_batch(orig).await, BatchState::Applied);
    let (status, body) = app
        .call(
            &c,
            Method::POST,
            &format!("/api/history/{orig}/revert"),
            serde_json::json!({ "description": "戻す" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["affected"], 2);
    assert_eq!(body["conflict"], 0);
    let rev = body["batch_id"].as_i64().unwrap();
    assert_eq!(app.wait_batch(rev).await, BatchState::Applied);

    let (status, body) = app.get(&c, "/api/history").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    // 新しい順
    assert_eq!(items[0]["id"], rev);
    assert_eq!(items[0]["kind"], "tags");
    assert_eq!(items[0]["state"], "applied");
    assert_eq!(items[0]["affected"], 2);
    assert_eq!(items[0]["applied"], 2);
    assert_eq!(items[0]["conflict"], 0);
    assert_eq!(items[0]["failed"], 0);
    assert_eq!(items[0]["reverts_batch_id"], orig);
    assert!(items[0]["reverted_by"].is_null());
    assert_eq!(items[0]["description"], "戻す");
    assert!(items[0]["finished_at"].is_number());
    assert_eq!(items[1]["id"], orig);
    assert_eq!(items[1]["reverted_by"], rev);
    assert!(items[1]["reverted_at"].is_number());
}

#[tokio::test]
async fn history_detail_returns_ops_with_edits_and_current_values_for_conflicts() {
    let app = App::new().await;
    setup!(app, "A/01.flac" => "a", "A/02.flac" => "b");
    let c = app.cookie().await;
    let (a, b) = (app.track_id("A/01.flac"), app.track_id("A/02.flac"));
    let batch = app.edit(&[a, b], "x").await;
    // b は反映前に外部で書き換わる → conflict
    common::retag(&app.lib().join("A/02.flac"), |t| {
        use lofty::tag::Accessor as _;
        t.set_title("外部".to_owned())
    });
    app.start_worker();
    assert_eq!(app.wait_batch(batch).await, BatchState::Partial);
    let (status, body) = app.get(&c, &format!("/api/history/{batch}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], batch);
    assert_eq!(body["state"], "partial");
    assert_eq!(body["applied"], 1);
    assert_eq!(body["conflict"], 1);
    let ops = body["ops"].as_array().unwrap();
    assert_eq!(ops.len(), 2);
    let oa = ops.iter().find(|o| o["track_id"] == a).unwrap();
    assert_eq!(oa["kind"], "tags");
    assert_eq!(oa["result"], "applied");
    assert!(oa["error"].is_null());
    assert_eq!(oa["rel_path"], "A/01.flac");
    assert_eq!(oa["edits"]["TITLE"]["old"], serde_json::json!(["a"]));
    assert_eq!(oa["edits"]["TITLE"]["new"], serde_json::json!(["x"]));
    assert!(oa["current"].is_null());
    let ob = ops.iter().find(|o| o["track_id"] == b).unwrap();
    assert_eq!(ob["result"], "skipped_conflict");
    assert!(ob["error"].as_str().is_some_and(|e| !e.is_empty()));
    // conflict の op はファイルを再読込した現在値を持つ
    assert_eq!(ob["current"]["TITLE"], serde_json::json!(["外部"]));

    let (status, _) = app.get(&c, "/api/history/9999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn revert_returns_409_for_non_terminal_already_reverted_and_pending() {
    let app = App::new().await;
    setup!(app, "A/01.flac" => "a");
    let c = app.cookie().await;
    let a = app.track_id("A/01.flac");
    let batch = app.edit(&[a], "x").await;
    // prepared のまま
    let (status, body) = app
        .call(
            &c,
            Method::POST,
            &format!("/api/history/{batch}/revert"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "not_terminal");
    let (status, _) = app
        .call(
            &c,
            Method::POST,
            "/api/history/9999/revert",
            serde_json::json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    app.start_worker();
    assert_eq!(app.wait_batch(batch).await, BatchState::Applied);
    let (status, body) = app
        .call(
            &c,
            Method::POST,
            &format!("/api/history/{batch}/revert"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let rev = body["batch_id"].as_i64().unwrap();
    assert_eq!(app.wait_batch(rev).await, BatchState::Applied);
    let (status, body) = app
        .call(
            &c,
            Method::POST,
            &format!("/api/history/{batch}/revert"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "already_reverted");

    // pending: 逆バッチを戻す（redo）対象に別バッチが pending
    app.shutdown.cancel();
    app.edit(&[a], "y").await;
    let (status, body) = app
        .call(
            &c,
            Method::POST,
            &format!("/api/history/{rev}/revert"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "pending");
    assert_eq!(body["track_ids"], serde_json::json!([a]));
}

#[tokio::test]
async fn cancel_endpoint_cancels_open_batch_and_rejects_terminal() {
    let app = App::new().await;
    setup!(app, "A/01.flac" => "a");
    let c = app.cookie().await;
    let a = app.track_id("A/01.flac");
    let batch = app.edit(&[a], "x").await;
    let (status, _) = app
        .call(
            &c,
            Method::POST,
            &format!("/api/history/{batch}/cancel"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(app.wait_batch(batch).await, BatchState::Cancelled);
    let (status, body) = app
        .call(
            &c,
            Method::POST,
            &format!("/api/history/{batch}/cancel"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "not_cancellable");
    let (status, _) = app
        .call(
            &c,
            Method::POST,
            "/api/history/9999/cancel",
            serde_json::json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn history_endpoints_require_a_session() {
    let app = App::new().await;
    setup!(app, "A/01.flac" => "a");
    let (status, _) = app.get("", "/api/history").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

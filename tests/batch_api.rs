//! `POST /api/tracks/batch/preview` / `PATCH /api/tracks/batch`（SPEC §9 / §7.5 / §12.3、
//! docs/TASKS.md P0-10、D-33 / D-42）。合成ファイルは ffmpeg で 1 本作り、コピーして増やす

#![cfg(target_os = "linux")]

mod common;

use std::fs::File;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use lofty::tag::Accessor;
use rusqlite::Connection;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::history::{self, BatchState, OpResult};
use spindle::db::Db;
use spindle::domain::tags::read_audio_file;
use spindle::edit::{Editor, NewTagOp, TagChange};
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, ScanReport, Scanner};
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

    /// プロセス再起動の模擬。ワーカーは止め、同じ DB / ライブラリで開き直す
    async fn reopen(mut self) -> Self {
        self.shutdown.cancel();
        let dir = std::mem::replace(&mut self.dir, tempfile::tempdir().unwrap());
        Self::open(dir).await
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

    fn title_and_version(&self, id: i64) -> (Option<String>, i64) {
        self.conn()
            .query_row(
                "SELECT title, tag_version FROM tracks WHERE id = ?1",
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
        self.call(c, Method::POST, "/api/tracks/batch/preview", body)
            .await
    }

    async fn apply(&self, c: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        self.call(c, Method::PATCH, "/api/tracks/batch", body).await
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

fn file_title(path: &Path) -> Option<String> {
    let af = read_audio_file(File::open(path).unwrap(), Some("flac")).unwrap();
    af.tags.first("TITLE").map(str::to_owned)
}

fn set_title_op(title: &str) -> serde_json::Value {
    serde_json::json!([{ "op": "set", "key": "TITLE", "value": title }])
}

macro_rules! setup {
    ($app:ident, $($rel:expr => $title:expr),+ $(,)?) => {{
        $( require_ffmpeg!($app.add($rel, $title)); )+
        $app.scan().await;
    }};
}

// ---------------------------------------------------------------- preview

#[tokio::test]
async fn preview_returns_token_counts_and_diffs_and_excludes_pending() {
    let app = App::new().await;
    setup!(app, "A/01.flac" => "old1", "A/02.flac" => "same", "A/03.flac" => "old3", "A/04.flac" => "p");
    let c = app.cookie().await;
    let ids: Vec<i64> = ["A/01.flac", "A/02.flac", "A/03.flac", "A/04.flac"]
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
        .preview(
            &c,
            serde_json::json!({ "selection": { "ids": ids }, "ops": set_title_op("same") }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["selection_token"]
        .as_str()
        .is_some_and(|t| !t.is_empty()));
    assert_eq!(body["count"], 4);
    assert_eq!(body["changed"], 2);
    assert_eq!(body["unchanged"], 1);
    assert_eq!(body["pending_excluded"], 1);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{items:?}");
    let first = items.iter().find(|i| i["id"] == ids[0]).unwrap();
    assert_eq!(
        first["changes"]["TITLE"]["old"],
        serde_json::json!(["old1"])
    );
    assert_eq!(
        first["changes"]["TITLE"]["new"],
        serde_json::json!(["same"])
    );
    assert!(
        items.iter().all(|i| i["id"] != ids[3]),
        "pending は items に出ない"
    );
}

#[tokio::test]
async fn preview_numbers_in_requested_sort_order() {
    let app = App::new().await;
    setup!(app, "A/01.flac" => "c", "A/02.flac" => "b", "A/03.flac" => "a");
    let c = app.cookie().await;
    let (status, body) = app
        .preview(
            &c,
            serde_json::json!({
                "selection": { "filter": "{}", "exclude_ids": [] },
                "sort": "title",
                "ops": [{ "op": "number", "key": "TRACKNUMBER", "start": 10 }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    let number_of = |rel: &str| {
        let id = app.track_id(rel);
        items
            .iter()
            .find(|i| i["id"] == id)
            .map(|i| i["changes"]["TRACKNUMBER"]["new"].clone())
    };
    assert_eq!(number_of("A/03.flac"), Some(serde_json::json!(["10"])));
    assert_eq!(number_of("A/02.flac"), Some(serde_json::json!(["11"])));
    assert_eq!(number_of("A/01.flac"), Some(serde_json::json!(["12"])));
}

#[tokio::test]
async fn preview_rejects_bad_ops_selection_and_sort() {
    let app = App::new().await;
    setup!(app, "A/01.flac" => "a");
    let c = app.cookie().await;
    for body in [
        serde_json::json!({ "selection": { "ids": [1] }, "ops": [] }),
        serde_json::json!({ "selection": { "ids": [1] }, "ops": [{ "op": "nope", "key": "TITLE" }] }),
        serde_json::json!({ "selection": { "ids": [1] }, "ops": [{ "op": "replace", "key": "TITLE", "pattern": "(" }] }),
        serde_json::json!({ "selection": { "filter": "{\"bogus\":1}" }, "ops": set_title_op("x") }),
        serde_json::json!({ "selection": { "ids": [1] }, "sort": "sideways", "ops": set_title_op("x") }),
        serde_json::json!({ "ops": set_title_op("x") }),
    ] {
        let (status, res) = app.preview(&c, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} → {res}");
        assert_eq!(res["error"], "bad_request");
    }
    // 未認証は 401
    let r = req(Method::POST, "/api/tracks/batch/preview")
        .header(header::CONTENT_TYPE, "application/json")
        .header("sec-fetch-site", "same-origin")
        .body(Body::from("{}"))
        .unwrap();
    let res = app.router.clone().oneshot(r).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------- apply

#[tokio::test]
async fn apply_creates_batch_with_overlay_and_jobs_then_worker_writes_files() {
    let app = App::new().await;
    setup!(app, "A/01.flac" => "a", "A/02.flac" => "b");
    let c = app.cookie().await;
    let a = app.track_id("A/01.flac");
    let b = app.track_id("A/02.flac");
    let ops = serde_json::json!([
        { "op": "set", "key": "TITLE", "value": "x" },
        { "op": "ref", "key": "ALBUMARTIST", "template": "%artist% - %title%" }
    ]);
    let (_, pv) = app
        .preview(
            &c,
            serde_json::json!({ "selection": { "ids": [a, b] }, "ops": ops }),
        )
        .await;
    let token = pv["selection_token"].as_str().unwrap();

    let (status, body) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": token, "ops": ops, "description": "x に統一" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let batch_id = body["batch_id"].as_i64().unwrap();
    assert_eq!(body["affected"], 2);

    // 1 トランザクションで記録 → overlay → ジョブ
    let batch = history::get_batch(&app.conn(), batch_id).unwrap().unwrap();
    assert_eq!(batch.description.as_deref(), Some("x に統一"));
    assert_eq!(batch.state, BatchState::Prepared);
    let ops_rows = history::list_ops(&app.conn(), batch_id).unwrap();
    assert_eq!(ops_rows.len(), 2);
    assert!(ops_rows.iter().all(|o| o.result == OpResult::Pending));
    assert_eq!(app.title_and_version(a), (Some("x".into()), 2));
    let (albumartist,): (Option<String>,) = app
        .conn()
        .query_row("SELECT albumartist FROM tracks WHERE id = ?1", [a], |r| {
            Ok((r.get(0)?,))
        })
        .unwrap();
    assert_eq!(albumartist.as_deref(), Some("Artist - x"));
    let queued: i64 = app
        .conn()
        .query_row(
            "SELECT count(*) FROM jobs WHERE type = 'tagwrite' AND state = 'queued' AND edit_batch_id = ?1",
            [batch_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(queued, 2);
    // 同じ token は二度使えない
    let (status, body) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": token, "ops": ops }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "preview_stale");

    app.start_worker();
    assert_eq!(app.wait_batch(batch_id).await, BatchState::Applied);
    assert_eq!(
        file_title(&app.lib().join("A/01.flac")).as_deref(),
        Some("x")
    );
    assert_eq!(
        file_title(&app.lib().join("A/02.flac")).as_deref(),
        Some("x")
    );
}

#[tokio::test]
async fn apply_with_pending_tracks_is_409_unless_skip_pending() {
    let app = App::new().await;
    setup!(app, "A/01.flac" => "a", "A/02.flac" => "b");
    let c = app.cookie().await;
    let a = app.track_id("A/01.flac");
    let b = app.track_id("A/02.flac");
    let ops = set_title_op("x");
    let (_, pv) = app
        .preview(
            &c,
            serde_json::json!({ "selection": { "ids": [a, b] }, "ops": ops }),
        )
        .await;
    assert_eq!(pv["pending_excluded"], 0);
    let token = pv["selection_token"].as_str().unwrap();
    // preview の後に b が反映待ちになった
    app.editor
        .prepare_tags(
            None,
            vec![NewTagOp {
                track_id: b,
                changes: vec![TagChange {
                    key: "TITLE".into(),
                    values: Some(vec!["p".into()]),
                }],
            }],
        )
        .await
        .unwrap();

    let (status, body) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": token, "ops": ops }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "pending");
    assert_eq!(body["count"], 1);
    assert_eq!(body["track_ids"], serde_json::json!([b]));
    // 409 では token は消費されない → skip_pending で続行できる
    let (status, body) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": token, "ops": ops, "skip_pending": true }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["affected"], 1);
    let ops_rows = history::list_ops(&app.conn(), body["batch_id"].as_i64().unwrap()).unwrap();
    assert_eq!(ops_rows.len(), 1);
    assert_eq!(ops_rows[0].track_id, a);
}

#[tokio::test]
async fn apply_with_unknown_token_or_different_ops_is_preview_stale() {
    let app = App::new().await;
    setup!(app, "A/01.flac" => "a");
    let c = app.cookie().await;
    let a = app.track_id("A/01.flac");
    let (status, body) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": "bogus", "ops": set_title_op("x") }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "preview_stale");

    let (_, pv) = app
        .preview(
            &c,
            serde_json::json!({ "selection": { "ids": [a] }, "ops": set_title_op("x") }),
        )
        .await;
    let token = pv["selection_token"].as_str().unwrap();
    let (status, body) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": token, "ops": set_title_op("y") }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "preview_stale");
    // ops 不一致では token は残る（正しい ops で適用できる）
    let (status, _) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": token, "ops": set_title_op("x") }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn apply_records_conflict_op_for_rows_whose_tag_version_changed_after_preview() {
    let app = App::new().await;
    let a_path = require_ffmpeg!(app.add("A/01.flac", "a"));
    require_ffmpeg!(app.add("A/02.flac", "b"));
    app.scan().await;
    let c = app.cookie().await;
    let a = app.track_id("A/01.flac");
    let b = app.track_id("A/02.flac");
    let ops = set_title_op("x");
    let (_, pv) = app
        .preview(
            &c,
            serde_json::json!({ "selection": { "ids": [a, b] }, "ops": ops }),
        )
        .await;
    let token = pv["selection_token"].as_str().unwrap();
    // preview の後に a が外部で変わり、スキャンが tag_version を進めた
    common::retag(&a_path, |t| t.set_title("外部".to_owned()));
    let report = app.scan().await;
    assert_eq!(report.updated, 1);
    assert_eq!(app.title_and_version(a).1, 2);

    let (status, body) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": token, "ops": ops }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["affected"], 2);
    let batch_id = body["batch_id"].as_i64().unwrap();
    let rows = history::list_ops(&app.conn(), batch_id).unwrap();
    let of = |id: i64| rows.iter().find(|o| o.track_id == id).unwrap();
    assert_eq!(of(a).result, OpResult::SkippedConflict);
    assert!(of(a).error.is_some());
    assert_eq!(of(b).result, OpResult::Pending);
    // conflict の行は DB も版も触らない
    assert_eq!(app.title_and_version(a), (Some("外部".into()), 2));
    assert_eq!(app.title_and_version(b), (Some("x".into()), 2));
    // pending は 1 件だけなので、その完了で partial になる
    app.start_worker();
    assert_eq!(app.wait_batch(batch_id).await, BatchState::Partial);
}

#[tokio::test]
async fn apply_with_nothing_to_change_is_409_no_changes() {
    let app = App::new().await;
    setup!(app, "A/01.flac" => "a");
    let c = app.cookie().await;
    let a = app.track_id("A/01.flac");
    let ops = set_title_op("a");
    let (_, pv) = app
        .preview(
            &c,
            serde_json::json!({ "selection": { "ids": [a] }, "ops": ops }),
        )
        .await;
    assert_eq!(pv["changed"], 0);
    let token = pv["selection_token"].as_str().unwrap();
    let (status, body) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": token, "ops": ops }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "no_changes");
}

#[tokio::test]
async fn applied_op_enqueues_derived_retag_only_when_derived_exists() {
    let app = App::new().await;
    setup!(app, "A/01.flac" => "a", "A/02.flac" => "b");
    let c = app.cookie().await;
    let a = app.track_id("A/01.flac");
    let b = app.track_id("A/02.flac");
    app.conn()
        .execute(
            "INSERT INTO derived_files (track_id, rel_path, rel_path_key, codec, src_audio_version,
                                        src_tag_version, generated_at)
             VALUES (?1, 'A/01.opus', 'a/01.opus', 'opus', 1, 1, 1)",
            [a],
        )
        .unwrap();
    let ops = set_title_op("x");
    let (_, pv) = app
        .preview(
            &c,
            serde_json::json!({ "selection": { "ids": [a, b] }, "ops": ops }),
        )
        .await;
    let token = pv["selection_token"].as_str().unwrap();
    let (_, body) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": token, "ops": ops }),
        )
        .await;
    let batch_id = body["batch_id"].as_i64().unwrap();
    app.start_worker();
    assert_eq!(app.wait_batch(batch_id).await, BatchState::Applied);
    let conn = app.conn();
    let mut stmt = conn
        .prepare("SELECT payload FROM jobs WHERE type = 'transcode' ORDER BY id")
        .unwrap();
    let payloads: Vec<serde_json::Value> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|s| serde_json::from_str(&s.unwrap()).unwrap())
        .collect();
    assert_eq!(payloads.len(), 1, "{payloads:?}");
    assert_eq!(payloads[0]["track_id"], a);
    assert_eq!(payloads[0]["kind"], "retag");
    assert_eq!(payloads[0]["tag_version"], 2);
}

// ---------------------------------------------------------------- 受け入れ

#[tokio::test]
async fn bulk_edit_of_1000_tracks_then_rescan_creates_no_duplicates() {
    let app = App::new().await;
    require_ffmpeg!(app.add("A/0000.flac", "t"));
    for i in 1..1000 {
        let rel = format!("{}/{i:04}.flac", ["A", "B", "C", "D"][i % 4]);
        app.add(&rel, &format!("t{i}")).unwrap();
    }
    app.scan().await;
    let c = app.cookie().await;
    let ops = serde_json::json!([
        { "op": "replace", "key": "TITLE", "pattern": "^t", "replacement": "Track " },
        { "op": "set", "key": "COMMENT", "value": "bulk" }
    ]);
    let (status, pv) = app
        .preview(
            &c,
            serde_json::json!({ "selection": { "filter": "{}", "exclude_ids": [] }, "ops": ops }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{pv}");
    assert_eq!(pv["count"], 1000);
    assert_eq!(pv["changed"], 1000);
    let token = pv["selection_token"].as_str().unwrap();
    let (status, body) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": token, "ops": ops }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["affected"], 1000);
    let batch_id = body["batch_id"].as_i64().unwrap();

    app.start_worker();
    assert_eq!(app.wait_batch(batch_id).await, BatchState::Applied);
    let counts = history::batch_counts(&app.conn(), batch_id).unwrap();
    assert_eq!(counts.applied, 1000);

    let report = app.scan().await;
    assert_eq!(report.new, 0, "再スキャンで重複が出た");
    assert_eq!(report.updated, 0);
    let (n, maxv, minv): (i64, i64, i64) = app
        .conn()
        .query_row(
            "SELECT count(*), max(tag_version), min(tag_version) FROM tracks",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((n, maxv, minv), (1000, 2, 2));
    assert_eq!(
        file_title(&app.lib().join("B/0001.flac")).as_deref(),
        Some("Track 1")
    );
}

#[tokio::test]
async fn scan_while_tagwrite_is_pending_keeps_the_edit_in_db() {
    let app = App::new().await;
    setup!(app, "A/01.flac" => "a");
    let c = app.cookie().await;
    let a = app.track_id("A/01.flac");
    let ops = set_title_op("x");
    let (_, pv) = app
        .preview(
            &c,
            serde_json::json!({ "selection": { "ids": [a] }, "ops": ops }),
        )
        .await;
    let token = pv["selection_token"].as_str().unwrap();
    app.apply(
        &c,
        serde_json::json!({ "selection_token": token, "ops": ops }),
    )
    .await;
    // ワーカーは動いていない。ファイルは旧値のままスキャン
    for _ in 0..2 {
        app.scan().await;
        assert_eq!(app.title_and_version(a), (Some("x".into()), 2));
    }
    // deep でも同じ
    app.scanner
        .run(
            ScanKind::Deep,
            Arc::new(|_, _, _| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(app.title_and_version(a), (Some("x".into()), 2));
}

#[tokio::test]
async fn restart_in_the_middle_of_tagwrite_applies_the_rest_without_double_version_bump() {
    let app = App::new().await;
    require_ffmpeg!(app.add("A/0000.flac", "t"));
    for i in 1..40 {
        app.add(&format!("A/{i:04}.flac"), &format!("t{i}"))
            .unwrap();
    }
    app.scan().await;
    let c = app.cookie().await;
    let ops = set_title_op("x");
    let (_, pv) = app
        .preview(
            &c,
            serde_json::json!({ "selection": { "filter": "{}", "exclude_ids": [] }, "ops": ops }),
        )
        .await;
    let token = pv["selection_token"].as_str().unwrap();
    let (_, body) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": token, "ops": ops }),
        )
        .await;
    let batch_id = body["batch_id"].as_i64().unwrap();

    // 途中まで反映したところで止める（プロセス kill の模擬）
    app.start_worker();
    let mut applied = 0;
    for _ in 0..1000 {
        applied = history::batch_counts(&app.conn(), batch_id)
            .unwrap()
            .applied;
        if applied >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(applied >= 3, "反映が始まらない");
    let app = app.reopen().await;
    let state_after_kill = history::get_batch(&app.conn(), batch_id)
        .unwrap()
        .unwrap()
        .state;
    assert!(!state_after_kill.is_terminal(), "{state_after_kill:?}");

    // 再起動: ジョブのリカバリ → 編集バッチのリカバリ → ワーカー
    spindle::jobs::recovery::run(&app.state.db).await.unwrap();
    app.editor.recover().await.unwrap();
    app.start_worker();
    assert_eq!(app.wait_batch(batch_id).await, BatchState::Applied);
    let counts = history::batch_counts(&app.conn(), batch_id).unwrap();
    assert_eq!(counts.applied, 40);
    let (n, maxv, minv): (i64, i64, i64) = app
        .conn()
        .query_row(
            "SELECT count(*), max(tag_version), min(tag_version) FROM tracks",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((n, maxv, minv), (40, 2, 2), "版が二重に上がった");
    for i in 0..40 {
        assert_eq!(
            file_title(&app.lib().join(format!("A/{i:04}.flac"))).as_deref(),
            Some("x"),
            "{i}"
        );
    }
}

// ---------------------------------------------------------------- レビュー指摘（2 回目）

#[tokio::test]
async fn apply_records_snapshot_preconditions_so_audio_replaced_after_preview_is_conflict() {
    let app = App::new().await;
    let a_path = require_ffmpeg!(app.add("A/01.flac", "a"));
    require_ffmpeg!(app.add("A/02.flac", "b"));
    app.scan().await;
    let c = app.cookie().await;
    let a = app.track_id("A/01.flac");
    let b = app.track_id("A/02.flac");
    let (inode_before,): (i64,) = app
        .conn()
        .query_row("SELECT inode FROM tracks WHERE id = ?1", [a], |r| {
            Ok((r.get(0)?,))
        })
        .unwrap();
    let ops = set_title_op("x");
    let (_, pv) = app
        .preview(
            &c,
            serde_json::json!({ "selection": { "ids": [a, b] }, "ops": ops }),
        )
        .await;
    let token = pv["selection_token"].as_str().unwrap();
    // preview の後に「同じタグの別音源」へ差し替え → scan は audio_version / inode を進めるが
    // tag_version は据え置き
    let other = common::make_audio(app.dir.path(), "other.flac", "flac", 9).unwrap();
    common::set_basic_tags(&other, "a", "Artist", "Album", "AlbumArtist", 1, 1);
    std::fs::rename(&other, &a_path).unwrap();
    let report = app.scan().await;
    assert_eq!(report.updated, 1);
    let (tv, av, inode_after): (i64, i64, i64) = app
        .conn()
        .query_row(
            "SELECT tag_version, audio_version, inode FROM tracks WHERE id = ?1",
            [a],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((tv, av), (1, 2));
    assert_ne!(inode_after, inode_before);

    let (status, body) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": token, "ops": ops }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let batch_id = body["batch_id"].as_i64().unwrap();
    // op の事前条件は preview 時の snapshot（旧 inode）
    let rows = history::list_ops(&app.conn(), batch_id).unwrap();
    let op_a = rows.iter().find(|o| o.track_id == a).unwrap();
    assert_eq!(op_a.expected.inode, Some(inode_before));
    // tagwrite は事前条件不一致で conflict にし、差し替えられたファイルを触らない
    app.start_worker();
    assert_eq!(app.wait_batch(batch_id).await, BatchState::Partial);
    let rows = history::list_ops(&app.conn(), batch_id).unwrap();
    assert_eq!(
        rows.iter().find(|o| o.track_id == a).unwrap().result,
        OpResult::SkippedConflict
    );
    assert_eq!(
        rows.iter().find(|o| o.track_id == b).unwrap().result,
        OpResult::Applied
    );
    assert_eq!(file_title(&a_path).as_deref(), Some("a"));
    assert_eq!(app.title_and_version(a).0.as_deref(), Some("a"));
}

#[tokio::test]
async fn external_rename_after_preview_is_followed_via_snapshot_preconditions() {
    let app = App::new().await;
    let a_path = require_ffmpeg!(app.add("A/01.flac", "a"));
    app.scan().await;
    let c = app.cookie().await;
    let a = app.track_id("A/01.flac");
    let ops = set_title_op("x");
    let (_, pv) = app
        .preview(
            &c,
            serde_json::json!({ "selection": { "ids": [a] }, "ops": ops }),
        )
        .await;
    let token = pv["selection_token"].as_str().unwrap();
    // preview の後に外部 rename → scan が追随（ctime は進む）
    let moved = app.lib().join("A/01 moved.flac");
    std::fs::rename(&a_path, &moved).unwrap();
    assert_eq!(app.scan().await.moved, 1);
    let (status, body) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": token, "ops": ops }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let batch_id = body["batch_id"].as_i64().unwrap();
    app.start_worker();
    assert_eq!(app.wait_batch(batch_id).await, BatchState::Applied);
    assert_eq!(file_title(&moved).as_deref(), Some("x"));
}

#[tokio::test]
async fn token_survives_409_no_changes_and_pending() {
    let app = App::new().await;
    setup!(app, "A/01.flac" => "a", "A/02.flac" => "b");
    let c = app.cookie().await;
    let a = app.track_id("A/01.flac");
    let b = app.track_id("A/02.flac");
    // no_changes の後も同じ token が使える（preview_stale にならない）
    let ops = set_title_op("a");
    let (_, pv) = app
        .preview(
            &c,
            serde_json::json!({ "selection": { "ids": [a] }, "ops": ops }),
        )
        .await;
    let token = pv["selection_token"].as_str().unwrap();
    for _ in 0..2 {
        let (status, body) = app
            .apply(
                &c,
                serde_json::json!({ "selection_token": token, "ops": ops }),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "no_changes");
    }
    // pending の 409（事前確認・Editor 側の再確認のどちらでも）の後も token は残る
    let ops2 = set_title_op("x");
    let (_, pv) = app
        .preview(
            &c,
            serde_json::json!({ "selection": { "ids": [a, b] }, "ops": ops2 }),
        )
        .await;
    let token2 = pv["selection_token"].as_str().unwrap();
    app.editor
        .prepare_tags(
            None,
            vec![NewTagOp {
                track_id: b,
                changes: vec![TagChange {
                    key: "TITLE".into(),
                    values: Some(vec!["p".into()]),
                }],
            }],
        )
        .await
        .unwrap();
    let (status, body) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": token2, "ops": ops2 }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "pending");
    let (status, body) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": token2, "ops": ops2, "skip_pending": true }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // 201 で消費される
    let (status, body) = app
        .apply(
            &c,
            serde_json::json!({ "selection_token": token2, "ops": ops2, "skip_pending": true }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "preview_stale");
}

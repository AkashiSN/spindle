//! `scan` ジョブハンドラと `POST /api/scan`（SPEC §8 / §9、docs/TASKS.md P0-6、D-38）。

#![cfg(target_os = "linux")]

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use rusqlite::Connection;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::Db;
use spindle::fsroot::RootDir;
use spindle::import::scanner::Scanner;
use spindle::jobs::handlers::scan::{enqueue_scan, ScanHandler};
use spindle::jobs::{EnqueueResult, JobState, JobType, Jobs, Registry};

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

struct Harness {
    db: Arc<Db>,
    jobs: Arc<Jobs>,
    scanner: Arc<Scanner>,
    shutdown: CancellationToken,
    dir: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Library")).unwrap();
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let jobs = Jobs::new(db.clone());
        let root = Arc::new(RootDir::open(&dir.path().join("Library")).unwrap());
        let scanner = Arc::new(Scanner::new(db.clone(), root, 2));
        Self {
            db,
            jobs,
            scanner,
            shutdown: CancellationToken::new(),
            dir,
        }
    }

    fn start(&self, deep_interval_days: u32) {
        let mut reg = Registry::new();
        reg.register(
            JobType::Scan,
            Arc::new(ScanHandler::new(self.scanner.clone(), deep_interval_days)),
        );
        self.jobs.start(reg, self.shutdown.clone());
    }

    fn raw(&self) -> Connection {
        Connection::open(self.dir.path().join("spindle.db")).unwrap()
    }

    async fn wait_terminal(&self, id: i64) -> JobState {
        for _ in 0..1000 {
            let job = self.jobs.get(id).await.unwrap().unwrap();
            if job.state.is_terminal() {
                return job.state;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job {id} が終端にならない");
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[tokio::test]
async fn scan_job_runs_the_scanner_and_reports_progress() {
    let h = Harness::new();
    let lib = h.dir.path().join("Library");
    std::fs::create_dir_all(lib.join("A/B")).unwrap();
    let p = require_ffmpeg!(common::make_audio(&lib.join("A/B"), "01.flac", "flac", 1));
    common::set_basic_tags(&p, "t", "Ar", "B", "AA", 1, 1);
    let mut events = h.jobs.subscribe();
    h.start(0);

    let EnqueueResult::Inserted(id) = enqueue_scan(&h.jobs, "incremental").await.unwrap() else {
        panic!()
    };
    assert_eq!(h.wait_terminal(id).await, JobState::Done);
    let n: i64 = h
        .raw()
        .query_row("SELECT count(*) FROM tracks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    let job = h.jobs.get(id).await.unwrap().unwrap();
    assert_eq!(job.done, Some(1));
    assert_eq!(job.total, Some(1));
    // 進捗イベントが少なくとも 1 回流れている
    let mut saw_progress = false;
    while let Ok(ev) = events.try_recv() {
        if format!("{ev:?}").contains("progress: Some") {
            saw_progress = true;
        }
    }
    assert!(saw_progress);
    let _ = &h.db;
}

#[tokio::test]
async fn incremental_is_upgraded_to_deep_when_interval_elapsed() {
    let h = Harness::new();
    h.start(30);
    let EnqueueResult::Inserted(id) = enqueue_scan(&h.jobs, "incremental").await.unwrap() else {
        panic!()
    };
    assert_eq!(h.wait_terminal(id).await, JobState::Done);
    let kinds = |h: &Harness| -> Vec<String> {
        h.raw()
            .prepare("SELECT kind FROM scan_runs ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(kinds(&h), ["deep"], "deep が一度も完了していなければ昇格");

    let EnqueueResult::Inserted(id2) = enqueue_scan(&h.jobs, "incremental").await.unwrap() else {
        panic!()
    };
    assert_eq!(h.wait_terminal(id2).await, JobState::Done);
    assert_eq!(kinds(&h), ["deep", "incremental"]);

    // 前回の deep を 31 日前に戻す → 次は昇格
    h.raw()
        .execute(
            "UPDATE scan_runs SET started_at = started_at - 31 * 86400 WHERE kind = 'deep'",
            [],
        )
        .unwrap();
    let EnqueueResult::Inserted(id3) = enqueue_scan(&h.jobs, "incremental").await.unwrap() else {
        panic!()
    };
    assert_eq!(h.wait_terminal(id3).await, JobState::Done);
    assert_eq!(kinds(&h), ["deep", "incremental", "deep"]);
}

#[tokio::test]
async fn interval_zero_never_upgrades() {
    let h = Harness::new();
    h.start(0);
    let EnqueueResult::Inserted(id) = enqueue_scan(&h.jobs, "incremental").await.unwrap() else {
        panic!()
    };
    assert_eq!(h.wait_terminal(id).await, JobState::Done);
    let kind: String = h
        .raw()
        .query_row("SELECT kind FROM scan_runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(kind, "incremental");
}

#[tokio::test]
async fn scan_job_is_deduplicated_while_active() {
    let h = Harness::new();
    let EnqueueResult::Inserted(_) = enqueue_scan(&h.jobs, "incremental").await.unwrap() else {
        panic!()
    };
    assert!(matches!(
        enqueue_scan(&h.jobs, "deep").await.unwrap(),
        EnqueueResult::Duplicate(_)
    ));
}

#[tokio::test]
async fn cancelling_a_running_scan_marks_the_run_cancelled() {
    let h = Harness::new();
    let lib = h.dir.path().join("Library");
    std::fs::create_dir_all(lib.join("A/B")).unwrap();
    require_ffmpeg!(common::make_audio(&lib.join("A/B"), "01.flac", "flac", 1));
    for i in 2..=30 {
        std::fs::copy(
            lib.join("A/B/01.flac"),
            lib.join(format!("A/B/{i:02}.flac")),
        )
        .unwrap();
    }
    h.start(0);
    let EnqueueResult::Inserted(id) = enqueue_scan(&h.jobs, "incremental").await.unwrap() else {
        panic!()
    };
    // 走り出したらすぐキャンセル
    for _ in 0..500 {
        let job = h.jobs.get(id).await.unwrap().unwrap();
        if job.state == JobState::Running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    h.jobs.cancel(id).await.unwrap();
    let state = h.wait_terminal(id).await;
    let run_state: String = h
        .raw()
        .query_row(
            "SELECT state FROM scan_runs ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    // 間に合えば cancelled、既に終わっていれば done（完了が勝つ）
    match state {
        JobState::Cancelled => assert_eq!(run_state, "cancelled"),
        JobState::Done => assert_eq!(run_state, "completed"),
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------- HTTP API

struct TestApp {
    router: axum::Router,
    jobs: Arc<Jobs>,
    _dir: tempfile::TempDir,
}

async fn app() -> TestApp {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
    let config = Arc::new(Config::parse(EXAMPLE).unwrap());
    let mode = auth::bootstrap(&db, Some("correct horse".into()))
        .await
        .unwrap();
    let state = AppState::new(config, db, mode);
    TestApp {
        jobs: state.jobs.clone(),
        router: api::router(state),
        _dir: dir,
    }
}

fn req(method: Method, uri: &str) -> axum::http::request::Builder {
    let peer: std::net::SocketAddr = LAN.parse().unwrap();
    let mut b = Request::builder().method(method).uri(uri);
    b.extensions_mut().unwrap().insert(ConnectInfo(peer));
    b
}

async fn send(app: &TestApp, r: Request<Body>) -> axum::response::Response {
    app.router.clone().oneshot(r).await.unwrap()
}

async fn json(res: axum::response::Response) -> serde_json::Value {
    let body = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
}

async fn cookie(app: &TestApp) -> String {
    let r = req(Method::POST, "/api/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .header("sec-fetch-site", "same-origin")
        .body(Body::from(r#"{"password":"correct horse"}"#))
        .unwrap();
    let res = send(app, r).await;
    assert_eq!(res.status(), StatusCode::OK);
    let set = res
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    set.split(';').next().unwrap().to_string()
}

fn post_scan(c: &str, body: &'static str) -> Request<Body> {
    req(Method::POST, "/api/scan")
        .header(header::COOKIE, c)
        .header(header::CONTENT_TYPE, "application/json")
        .header("sec-fetch-site", "same-origin")
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn post_scan_requires_session() {
    let app = app().await;
    let res = send(
        &app,
        req(Method::POST, "/api/scan")
            .header("sec-fetch-site", "same-origin")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"kind":"deep"}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn post_scan_enqueues_once_and_rejects_bad_kind() {
    let app = app().await;
    let c = cookie(&app).await;

    let res = send(&app, post_scan(&c, r#"{"kind":"deep"}"#)).await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let body = json(res).await;
    let id = body["job_id"].as_i64().unwrap();
    let job = app.jobs.get(id).await.unwrap().unwrap();
    assert_eq!(job.job_type, JobType::Scan);
    assert_eq!(job.payload["kind"], "deep");

    // 既に queued なら 409 duplicate
    let res = send(&app, post_scan(&c, r#"{"kind":"incremental"}"#)).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(json(res).await["error"], "duplicate");

    // kind 省略は incremental、不正は 400
    app.jobs.cancel(id).await.unwrap();
    let res = send(&app, post_scan(&c, r#"{}"#)).await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let id2 = json(res).await["job_id"].as_i64().unwrap();
    assert_eq!(
        app.jobs.get(id2).await.unwrap().unwrap().payload["kind"],
        "incremental"
    );
    app.jobs.cancel(id2).await.unwrap();
    let res = send(&app, post_scan(&c, r#"{"kind":"full"}"#)).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

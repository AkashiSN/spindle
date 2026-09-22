//! `GET /api/cd/status` と `POST /api/cd/eject`（SPEC §9、P2-1）。ドライブはフェイクで差し替え、
//! ポーラの 1 周回を手で回して API の見え方を確かめる

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::cd::device::{DiscIds, Drive, DriveError, DriveMonitor, DriveState};
use spindle::cd::toc::Toc;
use spindle::config::Config;
use spindle::db::Db;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";
const TOC: &str = "0:20144:40290";

struct FakeDrive {
    state: Mutex<DriveState>,
    ejects: AtomicUsize,
    eject_fails: bool,
}

impl Drive for FakeDrive {
    fn status(&self) -> Result<DriveState, DriveError> {
        Ok(*self.state.lock().unwrap())
    }
    fn read_toc(&self) -> Result<Toc, DriveError> {
        Ok(Toc::parse(TOC).unwrap())
    }
    fn read_ids(&self, _toc: &Toc) -> Result<DiscIds, DriveError> {
        Ok(DiscIds {
            isrcs: vec![Some("JPQ402600330".into()), None],
            mcn: Some("4582515778491".into()),
        })
    }
    fn eject(&self) -> Result<(), DriveError> {
        self.ejects.fetch_add(1, Ordering::SeqCst);
        if self.eject_fails {
            return Err(DriveError::Io {
                what: "CDROMEJECT",
                source: std::io::Error::other("Input/output error"),
            });
        }
        *self.state.lock().unwrap() = DriveState::TrayOpen;
        Ok(())
    }
}

struct App {
    router: Router,
    drive: Option<Arc<dyn Drive>>,
    monitor: Arc<DriveMonitor>,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

impl App {
    async fn new(drive: Option<Arc<dyn Drive>>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let config = Arc::new(Config::parse(EXAMPLE).unwrap());
        let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
            .await
            .unwrap();
        let mut state = AppState::new(config, db, mode);
        let monitor = Arc::new(DriveMonitor::default());
        if let Some(d) = &drive {
            state = state.with_cd(Arc::clone(d), Arc::clone(&monitor));
        }
        Self {
            router: api::router(state),
            drive,
            monitor,
            dir,
        }
    }

    fn poll(&self, now: i64) {
        self.monitor
            .poll(self.drive.as_ref().unwrap().as_ref(), now);
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

    async fn call(&self, method: Method, uri: &str, c: &str) -> (StatusCode, Value) {
        let r = req(method, uri)
            .header(header::COOKIE, c)
            .header("sec-fetch-site", "same-origin")
            .body(Body::empty())
            .unwrap();
        let res = self.router.clone().oneshot(r).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}

fn req(method: Method, uri: &str) -> axum::http::request::Builder {
    let peer: SocketAddr = LAN.parse().unwrap();
    let mut b = Request::builder().method(method).uri(uri);
    b.extensions_mut().unwrap().insert(ConnectInfo(peer));
    b
}

fn drive(state: DriveState) -> Arc<FakeDrive> {
    Arc::new(FakeDrive {
        state: Mutex::new(state),
        ejects: AtomicUsize::new(0),
        eject_fails: false,
    })
}

fn dyn_drive(d: &Arc<FakeDrive>) -> Option<Arc<dyn Drive>> {
    Some(Arc::clone(d) as Arc<dyn Drive>)
}

#[tokio::test]
async fn status_requires_session() {
    let app = App::new(dyn_drive(&drive(DriveState::NoDisc))).await;
    let (st, _) = app.call(Method::GET, "/api/cd/status", "").await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn status_is_503_without_drive_wiring() {
    let app = App::new(None).await;
    let c = app.cookie().await;
    let (st, body) = app.call(Method::GET, "/api/cd/status", &c).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "cd_unavailable");
    let (st, _) = app.call(Method::POST, "/api/cd/eject", &c).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn status_before_first_poll_is_unknown() {
    let app = App::new(dyn_drive(&drive(DriveState::NoDisc))).await;
    let c = app.cookie().await;
    let (st, body) = app.call(Method::GET, "/api/cd/status", &c).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "unknown");
    assert_eq!(body["toc"], Value::Null);
    assert_eq!(body["tracks"], serde_json::json!([]), "TOC が無ければ空");
    assert_eq!(body["isrcs"], serde_json::json!([]));
    assert_eq!(body["mcn"], Value::Null);
    assert_eq!(body["error"], Value::Null);
    assert_eq!(body["checked_at"], 0);
}

#[tokio::test]
async fn status_reports_disc_and_toc_string() {
    let app = App::new(dyn_drive(&drive(DriveState::DiscOk))).await;
    app.poll(1_700_000_000);
    let c = app.cookie().await;
    let (st, body) = app.call(Method::GET, "/api/cd/status", &c).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "disc_ok");
    // lookup に渡す文字列と同じ形（CTDB 形式）
    assert_eq!(body["toc"], TOC);
    // TOC と一緒に読んだ ISRC（音声トラック順。無いトラックは null）と MCN
    assert_eq!(body["isrcs"], serde_json::json!(["JPQ402600330", null]));
    assert_eq!(body["mcn"], "4582515778491");
    assert_eq!(body["error"], Value::Null);
    assert_eq!(body["checked_at"], 1_700_000_000_i64);
    // 表は照会の前から出すので、TOC の音声トラック（番号と長さ）も返す（P4-20）
    let tracks = body["tracks"].as_array().expect("tracks");
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0]["number"], 1);
    assert_eq!(tracks[0]["length_ms"], 20144 * 1000 / 75);
    assert_eq!(tracks[1]["number"], 2);
    assert_eq!(tracks[1]["length_ms"], (40290 - 20144) * 1000 / 75);
}

#[tokio::test]
async fn status_reports_no_drive_with_reason() {
    struct Broken;
    impl Drive for Broken {
        fn status(&self) -> Result<DriveState, DriveError> {
            Err(DriveError::Io {
                what: "open",
                source: std::io::Error::other("Permission denied"),
            })
        }
        fn read_toc(&self) -> Result<Toc, DriveError> {
            unreachable!()
        }
        fn read_ids(&self, _toc: &Toc) -> Result<DiscIds, DriveError> {
            unreachable!()
        }
        fn eject(&self) -> Result<(), DriveError> {
            unreachable!()
        }
    }
    let app = App::new(Some(Arc::new(Broken))).await;
    app.poll(5);
    let c = app.cookie().await;
    let (st, body) = app.call(Method::GET, "/api/cd/status", &c).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "no_drive");
    assert_eq!(body["error"], "open: Permission denied");
}

#[tokio::test]
async fn eject_opens_tray_and_refreshes_status() {
    let fake = drive(DriveState::DiscOk);
    let app = App::new(dyn_drive(&fake)).await;
    app.poll(10);
    let c = app.cookie().await;
    let (st, body) = app.call(Method::POST, "/api/cd/eject", &c).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(fake.ejects.load(Ordering::SeqCst), 1);
    // eject の直後に状態を見直すので、次のポーリングを待たずに TOC が消える
    let (st, body) = app.call(Method::GET, "/api/cd/status", &c).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["state"], "tray_open");
    assert_eq!(body["toc"], Value::Null);
    assert_eq!(body["tracks"], serde_json::json!([]), "TOC が無ければ空");
}

#[tokio::test]
async fn eject_failure_is_reported() {
    let app = App::new(Some(Arc::new(FakeDrive {
        state: Mutex::new(DriveState::DiscOk),
        ejects: AtomicUsize::new(0),
        eject_fails: true,
    })))
    .await;
    let c = app.cookie().await;
    let (st, body) = app.call(Method::POST, "/api/cd/eject", &c).await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["error"], "eject_failed");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains("Input/output error"),
        "{body}"
    );
}

#[tokio::test]
async fn eject_requires_same_origin_post() {
    // CSRF: 他サイトからの POST は通らない（他の書き込み API と同じ）
    let fake = drive(DriveState::DiscOk);
    let app = App::new(dyn_drive(&fake)).await;
    let c = app.cookie().await;
    let r = req(Method::POST, "/api/cd/eject")
        .header(header::COOKIE, &c)
        .header("sec-fetch-site", "cross-site")
        .body(Body::empty())
        .unwrap();
    let res = app.router.clone().oneshot(r).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(fake.ejects.load(Ordering::SeqCst), 0);
}

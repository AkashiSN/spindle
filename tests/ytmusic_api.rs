//! `POST /api/ytmusic/download`（SPEC §9、D-70、P3-3）。URL ごとに ytdl ジョブを投入する

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::Db;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

struct App {
    router: Router,
    db: Arc<Db>,
    _dir: tempfile::TempDir,
}

impl App {
    async fn new(enabled: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let mut root: toml::Table = toml::from_str(EXAMPLE).unwrap();
        if !enabled {
            root.insert(
                "ytmusic".into(),
                toml::Value::Table(toml::from_str("enabled = false").unwrap()),
            );
        }
        let config = Arc::new(Config::parse(&toml::to_string(&root).unwrap()).unwrap());
        let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
            .await
            .unwrap();
        let state = AppState::new(config, db.clone(), mode);
        Self {
            router: api::router(state),
            db,
            _dir: dir,
        }
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

    async fn post(&self, c: Option<&str>, body: Value) -> (StatusCode, Value) {
        let mut r = req(Method::POST, "/api/ytmusic/download")
            .header(header::CONTENT_TYPE, "application/json")
            .header("sec-fetch-site", "same-origin");
        if let Some(c) = c {
            r = r.header(header::COOKIE, c);
        }
        let res = self
            .router
            .clone()
            .oneshot(r.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn jobs(&self) -> Vec<(i64, String, Option<String>, Value)> {
        self.db
            .read(|c| {
                let mut st =
                    c.prepare("SELECT id, type, dedup_key, payload FROM jobs ORDER BY id")?;
                let rows = st
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            serde_json::from_str::<Value>(&r.get::<_, String>(3)?)
                                .unwrap_or(Value::Null),
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .unwrap()
    }
}

fn req(method: Method, uri: &str) -> axum::http::request::Builder {
    let peer: SocketAddr = LAN.parse().unwrap();
    let mut b = Request::builder().method(method).uri(uri);
    b.extensions_mut().unwrap().insert(ConnectInfo(peer));
    b
}

#[tokio::test]
async fn download_enqueues_one_job_per_url_and_dedups() {
    let app = App::new(true).await;
    let c = app.cookie().await;
    let (st, body) = app
        .post(
            Some(&c),
            json!({ "urls": ["https://youtu.be/a", " https://youtu.be/b ", "https://youtu.be/a"] }),
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    let ids = body["job_ids"].as_array().unwrap();
    assert_eq!(ids.len(), 3, "{body}");
    assert_eq!(ids[0], ids[2], "同じ URL は同じジョブ");
    let jobs = app.jobs().await;
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[0].1, "ytdl");
    assert_eq!(jobs[0].2.as_deref(), Some("ytdl:https://youtu.be/a"));
    assert_eq!(jobs[1].2.as_deref(), Some("ytdl:https://youtu.be/b"));
    assert_eq!(jobs[1].3["url"], "https://youtu.be/b", "前後の空白は落とす");
}

#[tokio::test]
async fn download_rejects_bad_bodies_and_needs_ytmusic_enabled() {
    let app = App::new(true).await;
    let c = app.cookie().await;
    for body in [
        json!({ "urls": [] }),
        json!({ "urls": ["  "] }),
        json!({ "urls": ["ftp://x/y"] }),
        json!({ "urls": [format!("https://x/{}", "a".repeat(2100))] }),
        json!({}),
    ] {
        let (st, _) = app.post(Some(&c), body.clone()).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    }
    assert!(app.jobs().await.is_empty());
    // 未ログイン
    let (st, _) = app
        .post(None, json!({ "urls": ["https://youtu.be/a"] }))
        .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    // 無効
    let app = App::new(false).await;
    let c = app.cookie().await;
    let (st, body) = app
        .post(Some(&c), json!({ "urls": ["https://youtu.be/a"] }))
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
    assert!(app.jobs().await.is_empty());
}

//! エージェント API（`/api/agent/*`）の結合試験の準備。各試験ファイルが `mod agent_support;` で取り込む。
//! UI 側の呼び出し（ログイン済み Cookie 付き）と、エージェント側の呼び出し（Bearer のみ。
//! Cookie・Origin・Sec-Fetch-Site を付けない）を持つ。後のタスクは `App` にメソッドを足す
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Request};
use axum::Router;
use http_body_util::BodyExt;
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::Db;

pub use axum::http::{HeaderMap, HeaderName, Method, StatusCode};
pub use serde_json::{json, Value};

const EXAMPLE: &str = include_str!("../../deploy/config.example.toml");
/// 接続元（LAN。trusted_cidrs の扱いを試験に持ち込まないため、どの試験も同じピアから呼ぶ）
pub const LAN: &str = "192.168.1.23:50000";

pub struct App {
    pub router: Router,
    pub db: Arc<Db>,
    pub cookie: String,
    _dir: tempfile::TempDir,
}

pub fn req(method: Method, uri: &str) -> axum::http::request::Builder {
    let peer: SocketAddr = LAN.parse().unwrap();
    let mut b = Request::builder().method(method).uri(uri);
    b.extensions_mut().unwrap().insert(ConnectInfo(peer));
    b
}

impl App {
    pub async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let root: toml::Table = toml::from_str(EXAMPLE).unwrap();
        let config = Arc::new(Config::parse(&toml::to_string(&root).unwrap()).unwrap());
        let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
            .await
            .unwrap();
        let state = AppState::new(config, db.clone(), mode);
        let router = api::router(state);
        let r = req(Method::POST, "/api/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(r#"{"password":"correct horse"}"#))
            .unwrap();
        let res = router.clone().oneshot(r).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let cookie = res
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        Self {
            router,
            db,
            cookie,
            _dir: dir,
        }
    }

    /// UI からの呼び出し（セッション Cookie と sec-fetch-site 付き）
    pub async fn call(
        &self,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut r = req(method, uri)
            .header("sec-fetch-site", "same-origin")
            .header(header::COOKIE, &self.cookie);
        let body = match body {
            Some(v) => {
                r = r.header(header::CONTENT_TYPE, "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let res = self
            .router
            .clone()
            .oneshot(r.body(body).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// エージェントからの呼び出し。`token` があれば Bearer を付ける。Cookie・Origin・
    /// Sec-Fetch-Site は付けない
    pub async fn agent_call(
        &self,
        token: Option<&str>,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut r = req(method, uri);
        if let Some(t) = token {
            r = r.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let body = match body {
            Some(v) => {
                r = r.header(header::CONTENT_TYPE, "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let res = self
            .router
            .clone()
            .oneshot(r.body(body).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// エージェントからの呼び出し（本文なし・任意のヘッダ付き）。生の応答を返す。
    /// Cookie・Origin・Sec-Fetch-Site は呼び出し側が `headers` で明示しない限り付けない
    pub async fn agent_raw(
        &self,
        token: Option<&str>,
        method: Method,
        uri: &str,
        headers: &[(HeaderName, String)],
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut r = req(method, uri);
        if let Some(t) = token {
            r = r.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        for (k, v) in headers {
            r = r.header(k, v);
        }
        let res = self
            .router
            .clone()
            .oneshot(r.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let hdrs = res.headers().clone();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, hdrs, bytes.to_vec())
    }

    /// iPhone（Mac 経由。agent / aac / selection = all）を作って id を返す
    pub async fn create_iphone(&self, name: &str) -> i64 {
        let (st, v) = self
            .call(
                Method::POST,
                "/api/devices",
                Some(json!({"name": name, "transport": "agent", "variant": "aac", "selection": "all"})),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        v["id"].as_i64().unwrap()
    }

    /// UI からワンタイムコードを発行して返す
    pub async fn pair_code(&self, id: i64) -> String {
        let (st, v) = self
            .call(Method::POST, &format!("/api/devices/{id}/pair-code"), None)
            .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        v["code"].as_str().unwrap().to_owned()
    }

    /// コードを発行して pair し、トークンを返す
    pub async fn pair(&self, id: i64) -> String {
        let code = self.pair_code(id).await;
        let (st, v) = self
            .agent_call(
                None,
                Method::POST,
                "/api/agent/pair",
                Some(json!({"code": code})),
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        v["token"].as_str().unwrap().to_owned()
    }
}

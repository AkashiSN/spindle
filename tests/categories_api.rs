//! `GET /api/categories`、`POST /api/categories { name }`（SPEC §9、D-67）。統制語彙の一覧と追加。
//! CD 取り込みの確定フォーム（P2-8）が配置先の category を選ぶのに使う

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
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let config = Arc::new(Config::parse(EXAMPLE).unwrap());
        let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
            .await
            .unwrap();
        let state = AppState::new(config, db, mode);
        Self {
            router: api::router(state),
            dir,
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

    async fn get(&self, c: &str, uri: &str) -> (StatusCode, Value) {
        let r = req(Method::GET, uri)
            .header(header::COOKIE, c)
            .body(Body::empty())
            .unwrap();
        self.send(r).await
    }

    async fn post(&self, c: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        let r = req(Method::POST, uri)
            .header(header::COOKIE, c)
            .header(header::CONTENT_TYPE, "application/json")
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(body.to_string()))
            .unwrap();
        self.send(r).await
    }

    async fn send(&self, r: Request<Body>) -> (StatusCode, Value) {
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

#[tokio::test]
async fn categories_list_and_create() {
    let app = App::new().await;
    let c = app.cookie().await;
    let (st, body) = app.get(&c, "/api/categories").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 0);

    let (st, body) = app
        .post(&c, "/api/categories", json!({ "name": "J-Pop" }))
        .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["name"], "J-Pop");
    let id = body["id"].as_i64().unwrap();

    // 重複（大小文字違いも同じ語彙。ZFS insensitive と同じ）は 409
    let (st, body) = app
        .post(&c, "/api/categories", json!({ "name": "j-pop" }))
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["error"], "duplicate");

    // 空・空白だけ・区切りや禁止文字を含む・末尾ドット・予約名は 400（パスの先頭要素になる）
    for bad in ["", "  ", "a/b", "a\\b", "CON", "x.", "a:b"] {
        let (st, _) = app
            .post(&c, "/api/categories", json!({ "name": bad }))
            .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{bad:?}");
    }

    // 前後の空白は落として登録する
    let (st, body) = app
        .post(&c, "/api/categories", json!({ "name": "  Anime " }))
        .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["name"], "Anime");

    let (_, body) = app.get(&c, "/api/categories").await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(items.iter().any(|i| i["id"] == id && i["name"] == "J-Pop"));
}

#[tokio::test]
async fn categories_require_login() {
    let app = App::new().await;
    let r = req(Method::GET, "/api/categories")
        .body(Body::empty())
        .unwrap();
    let res = app.router.clone().oneshot(r).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

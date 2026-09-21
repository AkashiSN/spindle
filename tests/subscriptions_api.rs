//! `/api/ytmusic/subscriptions`（SPEC §9、D-78、P4-16）。登録 / 一覧 / 変更 / 削除 / 同期の投入

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
const PL: &str = "https://www.youtube.com/playlist?list=PLabc";

struct App {
    router: Router,
    db: Arc<Db>,
    cookie: String,
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

    async fn call(&self, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
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

    async fn post(&self, body: Value) -> (StatusCode, Value) {
        self.call(Method::POST, "/api/ytmusic/subscriptions", Some(body))
            .await
    }

    async fn list(&self) -> Value {
        let (st, body) = self
            .call(Method::GET, "/api/ytmusic/subscriptions", None)
            .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        body
    }

    async fn jobs(&self) -> Vec<(String, Option<String>)> {
        self.db
            .read(|c| {
                let mut st = c.prepare("SELECT type, dedup_key FROM jobs ORDER BY id")?;
                let rows = st
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
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
async fn create_list_patch_delete_round_trip() {
    let app = App::new(true).await;
    let (st, body) = app
        .post(json!({
            "url": PL, "albumartist": " Artist ", "album": "Album", "category": "Rock",
        }))
        .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let id = body["id"].as_i64().unwrap();
    assert_eq!(body["list_id"], "PLabc");
    assert_eq!(body["albumartist"], "Artist", "前後の空白は落とす");
    assert_eq!(body["album_id"], Value::Null);
    assert_eq!(body["align"], true);
    assert_eq!(body["enabled"], true);
    assert_eq!(body["max_enqueue"], 50);
    assert_eq!(body["last_result"], Value::Null);
    // 一覧
    let list = app.list().await;
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert_eq!(list["items"][0]["id"], id);
    // PATCH: 変えた欄だけ。category を null で消す
    let (st, body) = app
        .call(
            Method::PATCH,
            &format!("/api/ytmusic/subscriptions/{id}"),
            Some(json!({ "align": false, "max_enqueue": 5, "category": null })),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["align"], false);
    assert_eq!(body["max_enqueue"], 5);
    assert_eq!(body["category"], Value::Null);
    assert_eq!(body["album"], "Album");
    // DELETE
    let (st, _) = app
        .call(
            Method::DELETE,
            &format!("/api/ytmusic/subscriptions/{id}"),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = app
        .call(
            Method::DELETE,
            &format!("/api/ytmusic/subscriptions/{id}"),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(app.list().await["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn create_validates_the_url_and_rejects_duplicates() {
    let app = App::new(true).await;
    // list= の無い URL、YouTube 以外、空の albumartist / album
    for (body, what) in [
        (
            json!({ "url": "https://www.youtube.com/watch?v=abc", "albumartist": "A", "album": "B" }),
            "list= が無い",
        ),
        (
            json!({ "url": "https://example.com/playlist?list=PL1", "albumartist": "A", "album": "B" }),
            "YouTube でない",
        ),
        (
            json!({ "url": PL, "albumartist": " ", "album": "B" }),
            "albumartist が空",
        ),
        (
            json!({ "url": PL, "albumartist": "A", "album": "" }),
            "album が空",
        ),
        (
            json!({ "url": PL, "albumartist": "A", "album": "B", "max_enqueue": 0 }),
            "max_enqueue が 0",
        ),
    ] {
        let (st, res) = app.post(body).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{what}: {res}");
    }
    let (st, _) = app
        .post(json!({ "url": PL, "albumartist": "A", "album": "B" }))
        .await;
    assert_eq!(st, StatusCode::CREATED);
    // 同じ list_id
    let (st, body) = app
        .post(json!({ "url": "https://youtube.com/playlist?list=PLabc&x=1", "albumartist": "C", "album": "D" }))
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "duplicate_list");
    // 同じ追記先（表記違い）
    let (st, body) = app
        .post(json!({ "url": "https://www.youtube.com/playlist?list=PLxyz", "albumartist": "a", "album": "b" }))
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "duplicate_target");
}

#[tokio::test]
async fn sync_enqueues_a_job_sets_the_latch_and_dedups() {
    let app = App::new(true).await;
    let (_, body) = app
        .post(json!({ "url": PL, "albumartist": "A", "album": "B" }))
        .await;
    let id = body["id"].as_i64().unwrap();
    let (st, body) = app
        .call(
            Method::POST,
            &format!("/api/ytmusic/subscriptions/{id}/sync"),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    assert!(body["job_id"].is_i64());
    let (st, body) = app
        .call(
            Method::POST,
            &format!("/api/ytmusic/subscriptions/{id}/sync"),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "duplicate");
    assert_eq!(
        app.jobs().await,
        [(
            "playlist_sync".to_owned(),
            Some(format!("playlist_sync:{id}"))
        )]
    );
    let list = app.list().await;
    assert!(list["items"][0]["sync_requested_at"].is_i64(), "{list}");
    let (st, _) = app
        .call(Method::POST, "/api/ytmusic/subscriptions/999/sync", None)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn disabled_ytmusic_hides_the_endpoints() {
    let app = App::new(false).await;
    let (st, _) = app
        .call(Method::GET, "/api/ytmusic/subscriptions", None)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = app
        .post(json!({ "url": PL, "albumartist": "A", "album": "B" }))
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn endpoints_require_a_session() {
    let app = App::new(true).await;
    let r = req(Method::GET, "/api/ytmusic/subscriptions")
        .body(Body::empty())
        .unwrap();
    let res = app.router.clone().oneshot(r).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

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
    let (st, _) = app
        .call(Method::GET, "/api/ytmusic/subscriptions/1/progress", None)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn endpoints_require_a_session() {
    let app = App::new(true).await;
    for uri in [
        "/api/ytmusic/subscriptions",
        "/api/ytmusic/subscriptions/1/progress",
    ] {
        let r = req(Method::GET, uri).body(Body::empty()).unwrap();
        let res = app.router.clone().oneshot(r).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{uri}");
    }
}

async fn make_sub(app: &App, list: &str, album: &str) -> i64 {
    let url = format!("https://www.youtube.com/playlist?list={list}");
    let (st, body) = app
        .post(json!({ "url": url, "albumartist": "A", "album": album }))
        .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    body["id"].as_i64().unwrap()
}

async fn enqueue_ytdl(app: &App, url: &str, sub: i64, position: u32) -> i64 {
    let job = spindle::import::ytmusic::downloader::new_subscription_ytdl_job(url, sub, position);
    app.db
        .write(move |c| spindle::db::jobs::enqueue(c, &job, spindle::db::now_epoch()))
        .await
        .unwrap()
        .id()
}

async fn progress(app: &App, id: i64) -> (StatusCode, Value) {
    app.call(
        Method::GET,
        &format!("/api/ytmusic/subscriptions/{id}/progress"),
        None,
    )
    .await
}

#[tokio::test]
async fn progress_lists_the_latest_sync_and_its_downloads() {
    let app = App::new(true).await;
    let id = make_sub(&app, "PLone", "B").await;
    let other = make_sub(&app, "PLtwo", "C").await;
    let (st, _) = app
        .call(
            Method::POST,
            &format!("/api/ytmusic/subscriptions/{id}/sync"),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED);
    enqueue_ytdl(&app, "https://youtu.be/a", id, 3).await;
    enqueue_ytdl(&app, "https://youtu.be/b", id, 1).await;
    enqueue_ytdl(&app, "https://youtu.be/c", other, 1).await;
    let (st, body) = progress(&app, id).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["sync"]["type"], "playlist_sync");
    let pos: Vec<i64> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["payload"]["position"].as_i64().unwrap())
        .collect();
    assert_eq!(pos, [1, 3], "{body}");
}

#[tokio::test]
async fn progress_ignores_downloads_of_an_older_sync() {
    let app = App::new(true).await;
    let id = make_sub(&app, "PLone", "B").await;
    let old = enqueue_ytdl(&app, "https://youtu.be/old", id, 1).await;
    app.call(
        Method::POST,
        &format!("/api/ytmusic/subscriptions/{id}/sync"),
        None,
    )
    .await;
    let new = enqueue_ytdl(&app, "https://youtu.be/new", id, 2).await;
    app.db
        .write(move |c| {
            c.execute(
                "UPDATE jobs SET created_at = 10, state = 'done' WHERE id = ?1",
                [old],
            )?;
            c.execute(
                "UPDATE jobs SET created_at = 100 WHERE type = 'playlist_sync'
                   AND json_extract(payload, '$.subscription_id') = ?1",
                [id],
            )?;
            c.execute("UPDATE jobs SET created_at = 100 WHERE id = ?1", [new])?;
            Ok(())
        })
        .await
        .unwrap();
    let (_, body) = progress(&app, id).await;
    let ids: Vec<i64> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [new], "{body}");
}

/// 前回の同期の終わった ytdl が新しい playlist_sync と同じ秒に作られていても items に出ない
/// （created_at は秒単位なので、境界は id で取る）
#[tokio::test]
async fn progress_ignores_older_finished_downloads_in_the_same_second() {
    let app = App::new(true).await;
    let id = make_sub(&app, "PLone", "B").await;
    let old = enqueue_ytdl(&app, "https://youtu.be/old", id, 1).await;
    app.call(
        Method::POST,
        &format!("/api/ytmusic/subscriptions/{id}/sync"),
        None,
    )
    .await;
    let new = enqueue_ytdl(&app, "https://youtu.be/new", id, 2).await;
    app.db
        .write(move |c| {
            c.execute(
                "UPDATE jobs SET state = 'failed', last_error = 'x' WHERE id = ?1",
                [old],
            )?;
            c.execute(
                "UPDATE jobs SET created_at = 100
                 WHERE json_extract(payload, '$.subscription_id') = ?1",
                [id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (_, body) = progress(&app, id).await;
    assert!(body["sync"]["id"].as_i64().unwrap() > old, "{body}");
    let ids: Vec<i64> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [new], "{body}");
}

/// 投入が走行中の古いジョブに dedup されても、その件は items に残る。終わった古い件は出ない
#[tokio::test]
async fn progress_keeps_older_live_downloads() {
    let app = App::new(true).await;
    let id = make_sub(&app, "PLone", "B").await;
    let queued = enqueue_ytdl(&app, "https://youtu.be/q", id, 1).await;
    let running = enqueue_ytdl(&app, "https://youtu.be/r", id, 2).await;
    let done = enqueue_ytdl(&app, "https://youtu.be/d", id, 3).await;
    app.call(
        Method::POST,
        &format!("/api/ytmusic/subscriptions/{id}/sync"),
        None,
    )
    .await;
    app.db
        .write(move |c| {
            c.execute("UPDATE jobs SET created_at = 10 WHERE type = 'ytdl'", [])?;
            c.execute("UPDATE jobs SET state = 'running' WHERE id = ?1", [running])?;
            c.execute("UPDATE jobs SET state = 'done' WHERE id = ?1", [done])?;
            Ok(())
        })
        .await
        .unwrap();
    let (_, body) = progress(&app, id).await;
    let ids: Vec<i64> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [queued, running], "{body}");
}

#[tokio::test]
async fn progress_without_any_sync_is_empty() {
    let app = App::new(true).await;
    let id = make_sub(&app, "PLone", "B").await;
    let (st, body) = progress(&app, id).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, json!({ "sync": null, "items": [] }));
}

#[tokio::test]
async fn progress_of_unknown_subscription_is_404() {
    let app = App::new(true).await;
    let (st, body) = progress(&app, 999).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "not_found");
}

/// 同期が queued / running の間は PATCH を 409 にする（走行中の同期が古い追記先で揃えたり投入したりしない）。
/// 終端になれば通る
#[tokio::test]
async fn patch_is_refused_while_a_sync_is_active() {
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
    let job_id = body["job_id"].as_i64().unwrap();
    let (st, body) = app
        .call(
            Method::PATCH,
            &format!("/api/ytmusic/subscriptions/{id}"),
            Some(json!({ "album": "C" })),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "sync_running");
    let (st, body) = app
        .call(
            Method::DELETE,
            &format!("/api/ytmusic/subscriptions/{id}"),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "sync_running");
    // 終端（ワーカーが無いので DB で done に）
    app.db
        .write(move |c| {
            c.execute(
                "UPDATE jobs SET state = 'done', finished_at = 1 WHERE id = ?1",
                [job_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (st, body) = app
        .call(
            Method::PATCH,
            &format!("/api/ytmusic/subscriptions/{id}"),
            Some(json!({ "album": "C" })),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["album"], "C");
    let (st, _) = app
        .call(
            Method::DELETE,
            &format!("/api/ytmusic/subscriptions/{id}"),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
}

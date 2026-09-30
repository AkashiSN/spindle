//! `/api/devices`（UI 向け端末 API、P5-2 Task 7）

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
    cookie: String,
    _dir: tempfile::TempDir,
}

impl App {
    async fn new() -> Self {
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

    async fn call_without_cookie(&self, method: Method, uri: &str) -> (StatusCode, Value) {
        let r = req(method, uri).body(Body::empty()).unwrap();
        let res = self.router.clone().oneshot(r).await.unwrap();
        (res.status(), Value::Null)
    }
}

fn req(method: Method, uri: &str) -> axum::http::request::Builder {
    let peer: SocketAddr = LAN.parse().unwrap();
    let mut b = Request::builder().method(method).uri(uri);
    b.extensions_mut().unwrap().insert(ConnectInfo(peer));
    b
}

async fn create_iphone(app: &App, name: &str) -> i64 {
    let (st, v) = app
        .call(Method::POST, "/api/devices",
              Some(json!({"name": name, "transport": "agent", "variant": "aac", "selection": "playlists"})))
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    v["id"].as_i64().unwrap()
}

async fn insert_playlist(app: &App, id: i64, name: &str, rule: Option<&str>) {
    let (name, rule) = (name.to_owned(), rule.map(str::to_owned));
    app.db
        .write(move |c| {
            let (kind, ast) = match &rule {
                Some(src) => {
                    let r = spindle::playlist::dsl::parse(src).unwrap();
                    ("smart", Some(serde_json::to_string(&r).unwrap()))
                }
                None => ("manual", None),
            };
            c.execute(
                "INSERT INTO playlists (id, name, name_key, kind, rule_source, rule_ast, created_at, updated_at)
                 VALUES (?1, ?2, lower(?2), ?3, ?4, ?5, 0, 0)",
                rusqlite::params![id, name, kind, rule, ast],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn create_list_patch_delete_round_trip() {
    let app = App::new().await;
    let id = create_iphone(&app, "iPhone").await;
    let (st, v) = app.call(Method::GET, "/api/devices", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["items"][0]["name"], "iPhone");
    assert_eq!(v["items"][0]["transport"], "agent");
    assert!(v["items"][0]["connected"].is_null());
    assert_eq!(v["items"][0]["counts"]["add"], 0);
    let (st, v) = app
        .call(
            Method::PATCH,
            &format!("/api/devices/{id}"),
            Some(json!({"selection": "all"})),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["selection"], "all");
    assert_eq!(v["generation"], 2);
    let (st, _) = app
        .call(Method::DELETE, &format!("/api/devices/{id}"), None)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = app
        .call(Method::DELETE, &format!("/api/devices/{id}"), None)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_rejects_adb_empty_and_duplicate_names() {
    let app = App::new().await;
    create_iphone(&app, "iPhone").await;
    // adb の作成には serial と volume が要る
    let (st, v) = app.call(Method::POST, "/api/devices",
        Some(json!({"name": "Xperia", "transport": "adb", "variant": "opus", "selection": "all"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    let (st, v) = app.call(Method::POST, "/api/devices",
        Some(json!({"name": "Xperia", "transport": "adb", "variant": "opus", "selection": "all", "volume": "emulated"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    let (st, _) = app
        .call(
            Method::POST,
            "/api/devices",
            Some(json!({"name": "  ", "transport": "agent", "variant": "aac", "selection": "all"})),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, v) = app.call(Method::POST, "/api/devices",
        Some(json!({"name": "IPHONE", "transport": "agent", "variant": "aac", "selection": "all"}))).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(v["error"], "duplicate");
}

#[tokio::test]
async fn open_plan_blocks_changes_except_name() {
    let app = App::new().await;
    let id = create_iphone(&app, "iPhone").await;
    insert_playlist(&app, 5, "通勤", None).await;
    app.db
        .write(move |c| {
            c.execute(
                "INSERT INTO device_sync_plans (device_id, plan_token, plan, state, created_at)
                 VALUES (?1, 'x', '[]', 'open', 0)",
                [id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    for (method, uri, body) in [
        (
            Method::PATCH,
            format!("/api/devices/{id}"),
            Some(json!({"selection": "all"})),
        ),
        (
            Method::PATCH,
            format!("/api/devices/{id}"),
            Some(json!({"variant": "opus"})),
        ),
        (
            Method::PUT,
            format!("/api/devices/{id}/playlists"),
            Some(json!({"playlist_ids": [5]})),
        ),
        (Method::DELETE, format!("/api/devices/{id}"), None),
    ] {
        let (st, v) = app.call(method.clone(), &uri, body).await;
        assert_eq!(st, StatusCode::CONFLICT, "{method} {uri}: {v}");
        assert_eq!(v["error"], "open_plan");
    }
    let (st, v) = app
        .call(
            Method::PATCH,
            &format!("/api/devices/{id}"),
            Some(json!({"name": "iPhone 15"})),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["generation"], 1, "名前だけの変更は generation を進めない");
}

#[tokio::test]
async fn running_device_sync_job_also_blocks_changes() {
    let app = App::new().await;
    let id = create_iphone(&app, "iPhone").await;
    app.db
        .write(move |c| {
            c.execute(
                "INSERT INTO jobs (type, payload, state, created_at) VALUES ('device_sync', json_object('device_id', ?1), 'queued', 0)",
                [id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (st, v) = app
        .call(
            Method::PATCH,
            &format!("/api/devices/{id}"),
            Some(json!({"selection": "all"})),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(v["error"], "open_plan");
}

#[tokio::test]
async fn put_playlists_rejects_cycle_and_unknown() {
    let app = App::new().await;
    let id = create_iphone(&app, "iPhone").await;
    insert_playlist(&app, 5, "通勤", None).await;
    insert_playlist(&app, 6, "未反映", Some("%device_pending% IS Xperia")).await;
    let (st, v) = app
        .call(
            Method::PUT,
            &format!("/api/devices/{id}/playlists"),
            Some(json!({"playlist_ids": [5, 6]})),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"], "cycle");
    let (st, v) = app
        .call(
            Method::PUT,
            &format!("/api/devices/{id}/playlists"),
            Some(json!({"playlist_ids": [99]})),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    let (st, v) = app
        .call(
            Method::PUT,
            &format!("/api/devices/{id}/playlists"),
            Some(json!({"playlist_ids": [5]})),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["playlist_ids"], json!([5]));
}

#[tokio::test]
async fn rule_save_on_registered_playlist_rejects_cycle() {
    let app = App::new().await;
    let id = create_iphone(&app, "iPhone").await;
    insert_playlist(&app, 7, "新しめ", Some("%title% IS a")).await;
    let (st, _) = app
        .call(
            Method::PUT,
            &format!("/api/devices/{id}/playlists"),
            Some(json!({"playlist_ids": [7]})),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let (st, v) = app
        .call(
            Method::PATCH,
            "/api/playlists/7",
            Some(json!({"rule": "%on_device% IS iPhone"})),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"], "cycle");
    // 載っていないプレイリストなら保存できる
    insert_playlist(&app, 8, "別", Some("%title% IS b")).await;
    let (st, v) = app
        .call(
            Method::PATCH,
            "/api/playlists/8",
            Some(json!({"rule": "%on_device% IS iPhone"})),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
}

#[tokio::test]
async fn endpoints_require_a_session() {
    let app = App::new().await;
    let (st, _) = app.call_without_cookie(Method::GET, "/api/devices").await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn cycle_rejection_on_rule_save_changes_nothing_else() {
    let app = App::new().await;
    let id = create_iphone(&app, "iPhone").await;
    insert_playlist(&app, 7, "新しめ", Some("%title% IS a")).await;
    let (st, _) = app
        .call(
            Method::PUT,
            &format!("/api/devices/{id}/playlists"),
            Some(json!({"playlist_ids": [7]})),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let (st, v) = app
        .call(
            Method::PATCH,
            "/api/playlists/7",
            Some(json!({"name": "x", "rule": "%on_device% IS iPhone"})),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"], "cycle");
    let name: String = app
        .db
        .read(|c| Ok(c.query_row("SELECT name FROM playlists WHERE id = 7", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(name, "新しめ", "循環で弾いたら改名も反映しない");
}

#[tokio::test]
async fn patch_rename_to_another_devices_name_is_duplicate() {
    let app = App::new().await;
    create_iphone(&app, "iPhone").await;
    let other = create_iphone(&app, "Pixel").await;
    let (st, v) = app
        .call(
            Method::PATCH,
            &format!("/api/devices/{other}"),
            Some(json!({"name": "IPHONE"})),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], "duplicate");
}

#[tokio::test]
async fn diff_lists_waiting_items_and_evaluations() {
    let app = App::new().await;
    let id = create_iphone(&app, "iPhone").await;
    app.db
        .write(|c| {
            c.execute(
                "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless, channels,
                                     audio_version, tag_version, seen_at, title, artist_display)
                 VALUES (1, 'A/a.flac', 'a/a.flac', 1, 0, 0, 'flac', 1, 2, 1, 1, 0, '群青', 'YOASOBI')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    insert_playlist(&app, 5, "新しめ", Some("%title% IS 群青")).await;
    // 評価済みにする（playlist_items に 1 を入れる）
    app.db
        .write(|c| {
            spindle::db::playlists::materialize(c, 5, &[1], 100)?;
            Ok(())
        })
        .await
        .unwrap();
    app.call(
        Method::PUT,
        &format!("/api/devices/{id}/playlists"),
        Some(json!({"playlist_ids": [5]})),
    )
    .await;
    let (st, v) = app
        .call(Method::GET, &format!("/api/devices/{id}/diff"), None)
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["items"][0]["op"], "waiting");
    assert_eq!(v["items"][0]["title"], "群青");
    assert!(v["items"][0]["reason"].as_str().is_some());
    assert_eq!(v["evaluations"][0]["playlist_id"], 5);
    assert_eq!(v["evaluations"][0]["evaluated_at"], 100);
    assert!(v["estimate"]["free"].is_null());
    assert_eq!(v["plan_token"].as_str().unwrap().len(), 64);
    let (st, _) = app.call(Method::GET, "/api/devices/999/diff", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn estimate_counts_distinct_tracks_of_a_hypothetical_selection() {
    let app = App::new().await;
    let id = create_iphone(&app, "iPhone").await;
    app.db
        .write(|c| {
            for tid in 1..=3 {
                c.execute(
                    "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless, channels,
                                         audio_version, tag_version, seen_at)
                     VALUES (?1, ?2, ?2, 1, 0, 0, 'flac', 1, 2, 1, 1, 0)",
                    rusqlite::params![tid, format!("a/{tid}.flac")],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    insert_playlist(&app, 5, "p5", None).await;
    insert_playlist(&app, 6, "p6", None).await;
    app.db
        .write(|c| {
            spindle::db::playlists::materialize(c, 5, &[1, 2], 0)?;
            spindle::db::playlists::materialize(c, 6, &[2, 3], 0)?;
            Ok(())
        })
        .await
        .unwrap();
    let (st, v) = app
        .call(
            Method::GET,
            &format!("/api/devices/{id}/estimate?selection=playlists&playlist_ids=5,6"),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["tracks"], 3, "重複を除く");
    assert_eq!(v["unhashed"], 3);
    let (st, v) = app
        .call(
            Method::GET,
            &format!("/api/devices/{id}/estimate?selection=all"),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["tracks"], 3);
}

/// 投入する source_hash が無ければ GET /api/devices・差分は書き手に触らない（書き込みの通番を進めない）
#[tokio::test]
async fn listing_does_not_write_when_no_new_hash_jobs() {
    let app = App::new().await;
    app.db
        .write(|c| {
            c.execute(
                "INSERT INTO tracks (id, rel_path, rel_path_key, inode, size, mtime_ns, ctime_ns, codec, lossless,
                                     channels, audio_version, tag_version, seen_at)
                 VALUES (1, 'YT/a.opus', 'yt/a.opus', 1, 1, 0, 0, 'opus', 0, 2, 1, 1, 0)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    // 端末の作成より前に曲を入れる（一覧は表示用のスナップショットを使い、2 秒以内の書き込みは見ないことがある）
    let (st, v) = app
        .call(Method::POST, "/api/devices",
              Some(json!({"name": "iPhone", "transport": "agent", "variant": "opus", "selection": "all"})))
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let id = v["id"].as_i64().unwrap();
    let (st, _) = app.call(Method::GET, "/api/devices", None).await;
    assert_eq!(st, StatusCode::OK);
    let jobs: i64 = app
        .db
        .read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM jobs WHERE type = 'source_hash'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(jobs, 1, "ハッシュの無い原本に 1 件投入する");
    let seq = app.db.write_seq();
    let (st, _) = app.call(Method::GET, "/api/devices", None).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = app
        .call(Method::GET, &format!("/api/devices/{id}/diff"), None)
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        app.db.write_seq(),
        seq,
        "新しく投入するものが無ければ書かない"
    );
}

/// 保留（待ち）の曲に反映済みの行があれば、差分はその行き先と「端末に既にある」を出す
#[tokio::test]
async fn diff_shows_current_copy_of_held_tracks() {
    let app = App::new().await;
    app.db
        .write(|c| {
            c.execute(
                "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless, channels,
                                     audio_version, tag_version, seen_at)
                 VALUES (1, 'A/a.flac', 'a/a.flac', 1, 0, 0, 'flac', 1, 2, 1, 1, 0)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (st, v) = app
        .call(Method::POST, "/api/devices",
              Some(json!({"name": "iPhone", "transport": "agent", "variant": "aac", "selection": "all"})))
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let id = v["id"].as_i64().unwrap();
    app.db
        .write(move |c| {
            c.execute(
                "INSERT INTO device_items (device_id, track_id, dest_path, dest_path_key, token, size, sha256, synced_at)
                 VALUES (?1, 1, 'A/a.m4a', 'a/a.m4a', 't', 1, 'ab', 5)",
                [id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (st, v) = app
        .call(Method::GET, &format!("/api/devices/{id}/diff"), None)
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let item = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["track_id"] == 1)
        .unwrap();
    assert_eq!(item["op"], "waiting", "{v}");
    assert_eq!(item["dest_path"], "A/a.m4a");
    assert_eq!(item["has_copy"], true);
}

/// 反映済み（差分に操作が無い）の曲に端末から失敗の報告があれば、差分にもエラーとして理由を出す
/// （一覧の counts.error と食い違わせない。device_domain の reported_error_on_synced_track_is_error）
#[tokio::test]
async fn diff_lists_reported_error_on_synced_track() {
    use spindle::domain::device::{delivery_token, semantic_master, SourceHash, SourceKind};
    let app = App::new().await;
    let sha = "ab".repeat(32);
    let token = delivery_token(&semantic_master(1, 1), &sha);
    let h = SourceHash {
        semantic: semantic_master(1, 1),
        inode: 1,
        size: 1,
        mtime_ns: 0,
        ctime_ns: 0,
        sha256: sha.clone(),
    };
    app.db
        .write(move |c| {
            c.execute(
                "INSERT INTO tracks (id, rel_path, rel_path_key, inode, size, mtime_ns, ctime_ns, codec, lossless,
                                     channels, audio_version, tag_version, seen_at, title)
                 VALUES (1, 'YT/a.opus', 'yt/a.opus', 1, 1, 0, 0, 'opus', 0, 2, 1, 1, 0, '群青')",
                [],
            )?;
            spindle::db::devices::put_source_hash(c, 1, SourceKind::Master, &h, 5)?;
            Ok(())
        })
        .await
        .unwrap();
    let (st, v) = app
        .call(Method::POST, "/api/devices",
              Some(json!({"name": "Xperia", "transport": "agent", "variant": "opus", "selection": "all"})))
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let id = v["id"].as_i64().unwrap();
    app.db
        .write(move |c| {
            c.execute(
                "INSERT INTO device_items (device_id, track_id, dest_path, dest_path_key, token, size, sha256, synced_at)
                 VALUES (?1, 1, 'YT/a.opus', 'yt/a.opus', ?2, 1, ?3, 5)",
                rusqlite::params![id, token, sha],
            )?;
            c.execute(
                "INSERT INTO device_errors (device_id, kind, ref_id, reason, reported_at)
                 VALUES (?1, 'track', 1, '転送に失敗', 6)",
                [id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (st, v) = app
        .call(Method::GET, &format!("/api/devices/{id}/diff"), None)
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["counts"]["error"], 1, "{v}");
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{v}");
    assert_eq!(items[0]["op"], "error");
    assert_eq!(items[0]["track_id"], 1);
    assert_eq!(items[0]["title"], "群青");
    assert_eq!(items[0]["reason"], "転送に失敗");
    assert_eq!(items[0]["dest_path"], "YT/a.opus");
    assert_eq!(items[0]["has_copy"], true);
}

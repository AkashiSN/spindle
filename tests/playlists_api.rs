//! `/api/playlists`（SPEC §9 / §10、docs/TASKS.md P1-6、D-53）: CRUD、項目の追加・除外・移動、
//! m3u8 の書き出し（GET は本文、POST は Playlists root へ書く）、Playlists root からの取り込み

#![cfg(target_os = "linux")]

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::Db;
use spindle::fsroot::RootDir;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

struct TestApp {
    router: Router,
    dir: tempfile::TempDir,
}

impl TestApp {
    fn raw(&self) -> Connection {
        Connection::open(self.dir.path().join("spindle.db")).unwrap()
    }

    fn playlists_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("Playlists")
    }
}

async fn build(auth_override: &str, with_root: bool) -> TestApp {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("Playlists")).unwrap();
    let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
    let mut root: toml::Table = toml::from_str(EXAMPLE).unwrap();
    let patch: toml::Table = toml::from_str(auth_override).unwrap();
    let section = root.get_mut("auth").unwrap().as_table_mut().unwrap();
    for (k, v) in patch {
        section.insert(k, v);
    }
    let config = Arc::new(Config::parse(&toml::to_string(&root).unwrap()).unwrap());
    let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
        .await
        .unwrap();
    let mut state = AppState::new(config, db, mode);
    if with_root {
        let pl = Arc::new(RootDir::open(&dir.path().join("Playlists")).unwrap());
        state = state.with_playlists(pl);
    }
    TestApp {
        router: api::router(state),
        dir,
    }
}

async fn app() -> TestApp {
    build("", true).await
}

fn req(method: Method, uri: &str) -> axum::http::request::Builder {
    let peer: SocketAddr = LAN.parse().unwrap();
    let mut b = Request::builder().method(method).uri(uri);
    b.extensions_mut().unwrap().insert(ConnectInfo(peer));
    b
}

async fn send(app: &TestApp, r: Request<Body>) -> axum::response::Response {
    app.router.clone().oneshot(r).await.unwrap()
}

async fn body_json(res: axum::response::Response) -> Value {
    let body = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap_or(Value::Null)
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

async fn call(
    app: &TestApp,
    c: &str,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = req(method, uri)
        .header(header::COOKIE, c)
        .header("sec-fetch-site", "same-origin");
    let body = match body {
        Some(v) => {
            b = b.header(header::CONTENT_TYPE, "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let res = send(app, b.body(body).unwrap()).await;
    let status = res.status();
    (status, body_json(res).await)
}

async fn get(app: &TestApp, c: &str, uri: &str) -> (StatusCode, Value) {
    call(app, c, Method::GET, uri, None).await
}

async fn post(app: &TestApp, c: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    call(app, c, Method::POST, uri, Some(body)).await
}

fn insert_track(conn: &Connection, rel: &str, title: &str, missing: bool) -> i64 {
    let key = spindle::domain::relpath::canonical_key(rel);
    conn.execute(
        "INSERT INTO tracks (rel_path, rel_path_key, dev, inode, size, mtime_ns, ctime_ns, codec, lossless,
           title, artist_display, album, albumartist, track_no, disc_no, duration_ms, seen_at, missing_since)
         VALUES (?1, ?2, 1, abs(random()), 1, 1, 1, 'flac', 1, ?3, 'Ar', 'Al', 'AA', 1, 1, 61000, 1, ?4)",
        params![rel, key, title, if missing { Some(1) } else { None }],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn urlenc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn create(app: &TestApp, c: &str, name: &str) -> i64 {
    let (status, body) = post(app, c, "/api/playlists", json!({ "name": name })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_i64().unwrap()
}

async fn items_of(app: &TestApp, c: &str, id: i64) -> Vec<i64> {
    let filter = urlenc(&format!(r#"{{"playlist_id":{id}}}"#));
    let (status, body) = get(
        app,
        c,
        &format!("/api/tracks?filter={filter}&sort=position"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect()
}

// ---------------------------------------------------------------- CRUD

#[tokio::test]
async fn list_requires_session() {
    let app = app().await;
    let res = send(
        &app,
        req(Method::GET, "/api/playlists")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn create_list_rename_delete() {
    let app = app().await;
    let c = cookie(&app).await;
    let (status, body) = post(&app, &c, "/api/playlists", json!({ "name": "通勤" })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["name"], "通勤");
    assert_eq!(body["kind"], "manual");
    assert_eq!(body["track_count"], 0);
    assert_eq!(body["exports"], json!([]));
    let id = body["id"].as_i64().unwrap();

    let (status, body) = post(&app, &c, "/api/playlists", json!({ "name": "通勤" })).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "duplicate");

    let (status, body) = get(&app, &c, "/api/playlists").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);

    let (status, body) = call(
        &app,
        &c,
        Method::PATCH,
        &format!("/api/playlists/{id}"),
        Some(json!({ "name": "帰宅", "auto_export": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "帰宅");
    assert_eq!(body["auto_export"], false);

    create(&app, &c, "other").await;
    let (status, body) = call(
        &app,
        &c,
        Method::PATCH,
        &format!("/api/playlists/{id}"),
        Some(json!({ "name": "other" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "duplicate");

    let (status, _) = call(
        &app,
        &c,
        Method::PATCH,
        "/api/playlists/999",
        Some(json!({ "name": "z" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(
        &app,
        &c,
        Method::DELETE,
        &format!("/api/playlists/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &app,
        &c,
        Method::DELETE,
        &format!("/api/playlists/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&app, &c, &format!("/api/playlists/{id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn name_must_be_a_single_safe_filename_component() {
    let app = app().await;
    let c = cookie(&app).await;
    for bad in [
        "",
        "  ",
        "a/b",
        "a\\b",
        "CON",
        "end.",
        "a:b",
        "x\0y",
        &"あ".repeat(100),
    ] {
        let (status, body) = post(&app, &c, "/api/playlists", json!({ "name": bad })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?}: {body}");
        assert_eq!(body["error"], "bad_request");
    }
    // 書き出しファイル名は name + ".m3u8" なので、その長さで 255 バイトを超えてはならない
    let (status, _) = post(
        &app,
        &c,
        "/api/playlists",
        json!({ "name": "a".repeat(251) }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = post(
        &app,
        &c,
        "/api/playlists",
        json!({ "name": "a".repeat(250) }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    // 大小文字・NFC / NFD だけが違う名前は同じファイルになるので 409
    let (status, _) = post(&app, &c, "/api/playlists", json!({ "name": "Mix" })).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = post(&app, &c, "/api/playlists", json!({ "name": "mix" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, _) = post(&app, &c, "/api/playlists", json!({ "name": "\u{304c}" })).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = post(
        &app,
        &c,
        "/api/playlists",
        json!({ "name": "\u{304b}\u{3099}" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    // 前後の空白は落として保存する
    let (status, body) = post(&app, &c, "/api/playlists", json!({ "name": "  ok  " })).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "ok");
    let (status, _) = post(&app, &c, "/api/playlists", json!({ "name": "ok" })).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let id = body["id"].as_i64().unwrap();
    let (status, _) = call(
        &app,
        &c,
        Method::PATCH,
        &format!("/api/playlists/{id}"),
        Some(json!({ "name": "a/b" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------- 項目

#[tokio::test]
async fn items_append_in_selection_order_remove_and_move() {
    let app = app().await;
    let c = cookie(&app).await;
    let t: Vec<i64> = (1..=5)
        .map(|i| insert_track(&app.raw(), &format!("A/{i}.flac"), &format!("t{i}"), false))
        .collect();
    let id = create(&app, &c, "p").await;

    // ids 形は送った順。重複と不明 id は skipped
    let (status, body) = post(
        &app,
        &c,
        &format!("/api/playlists/{id}/items"),
        json!({ "selection": { "ids": [t[2], t[0], t[2], 9999] } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["added"], 2);
    assert_eq!(body["skipped"], 2);
    assert_eq!(items_of(&app, &c, id).await, vec![t[2], t[0]]);

    // filter 形 + sort（-title → t5, t4, t3(既存), t2, t1(既存)）
    let (status, body) = post(
        &app,
        &c,
        &format!("/api/playlists/{id}/items"),
        json!({ "selection": { "filter": "{}", "exclude_ids": [] }, "sort": "-title" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["added"], 3);
    assert_eq!(
        items_of(&app, &c, id).await,
        vec![t[2], t[0], t[4], t[3], t[1]]
    );
    let (_, body) = get(&app, &c, &format!("/api/playlists/{id}")).await;
    assert_eq!(body["track_count"], 5);
    assert_eq!(body["duration_ms"], 5 * 61000);

    // 除外
    let (status, body) = call(
        &app,
        &c,
        Method::DELETE,
        &format!("/api/playlists/{id}/items"),
        Some(json!({ "track_ids": [t[0], t[3], 9999] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["removed"], 2);
    assert_eq!(items_of(&app, &c, id).await, vec![t[2], t[4], t[1]]);

    // 除外は selection 形でも受ける（Ctrl+A のフィルタ形）。track_ids と selection のどちらも無ければ 400
    post(
        &app,
        &c,
        &format!("/api/playlists/{id}/items"),
        json!({ "selection": { "ids": [t[0]] } }),
    )
    .await;
    let (status, body) = call(
        &app,
        &c,
        Method::DELETE,
        &format!("/api/playlists/{id}/items"),
        Some(json!({ "selection": { "filter": "{\"playlist_id\":PL}".replace("PL", &id.to_string()), "exclude_ids": [t[2], t[4], t[1]] } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["removed"], 1);
    assert_eq!(items_of(&app, &c, id).await, vec![t[2], t[4], t[1]]);
    let (status, _) = call(
        &app,
        &c,
        Method::DELETE,
        &format!("/api/playlists/{id}/items"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 移動: t1 を t2 の前へ → t1, t2, t4。末尾へ → t2, t4, t1
    let (status, _) = post(
        &app,
        &c,
        &format!("/api/playlists/{id}/items/move"),
        json!({ "track_ids": [t[1]], "before": t[2] }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(items_of(&app, &c, id).await, vec![t[1], t[2], t[4]]);
    let (status, _) = post(
        &app,
        &c,
        &format!("/api/playlists/{id}/items/move"),
        json!({ "track_ids": [t[1]], "before": null }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(items_of(&app, &c, id).await, vec![t[2], t[4], t[1]]);

    // 移動先が不正
    let (status, body) = post(
        &app,
        &c,
        &format!("/api/playlists/{id}/items/move"),
        json!({ "track_ids": [t[1]], "before": t[1] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = post(
        &app,
        &c,
        &format!("/api/playlists/{id}/items/move"),
        json!({ "track_ids": [t[1]], "before": 9999 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 不明なプレイリスト
    for (m, uri, body) in [
        (
            Method::POST,
            "/api/playlists/999/items",
            json!({ "selection": { "ids": [1] } }),
        ),
        (
            Method::DELETE,
            "/api/playlists/999/items",
            json!({ "track_ids": [1] }),
        ),
        (
            Method::POST,
            "/api/playlists/999/items/move",
            json!({ "track_ids": [1], "before": null }),
        ),
    ] {
        let (status, _) = call(&app, &c, m, uri, Some(body)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn position_sort_without_playlist_filter_is_bad_request() {
    let app = app().await;
    let c = cookie(&app).await;
    let (status, body) = get(&app, &c, "/api/tracks?sort=position").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "bad_request");
}

// ---------------------------------------------------------------- 書き出し

#[tokio::test]
async fn export_get_returns_m3u8_body_and_post_writes_to_playlists_root() {
    let app = app().await;
    let c = cookie(&app).await;
    let a = insert_track(&app.raw(), "J-Pop/A/B/01 One.flac", "One", false);
    let m = insert_track(&app.raw(), "J-Pop/A/B/02 Gone.flac", "Gone", true);
    let b = insert_track(&app.raw(), "J-Pop/A/B/03 Three.flac", "Three", false);
    let id = create(&app, &c, "通勤").await;
    post(
        &app,
        &c,
        &format!("/api/playlists/{id}/items"),
        json!({ "selection": { "ids": [b, m, a] } }),
    )
    .await;

    let res = send(
        &app,
        req(
            Method::GET,
            &format!("/api/playlists/{id}/export?profile=internal"),
        )
        .header(header::COOKIE, &c)
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let ct = res
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(ct.starts_with("audio/x-mpegurl"), "{ct}");
    let body =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert_eq!(
        body,
        "#EXTM3U\n#EXTINF:61,Ar - Three\n../../Library/J-Pop/A/B/03 Three.flac\n#EXTINF:61,Ar - One\n../../Library/J-Pop/A/B/01 One.flac\n"
    );

    // foobar プロファイル: 絶対 + バックスラッシュ
    let res = send(
        &app,
        req(
            Method::GET,
            &format!("/api/playlists/{id}/export?profile=foobar"),
        )
        .header(header::COOKIE, &c)
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    let body =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(
        body.contains("\\\\TRUENAS\\music\\Library\\J-Pop\\A\\B\\03 Three.flac\n"),
        "{body}"
    );

    // profile 無し・不明は 400
    let (status, _) = get(&app, &c, &format!("/api/playlists/{id}/export")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = get(
        &app,
        &c,
        &format!("/api/playlists/{id}/export?profile=nope"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = get(&app, &c, "/api/playlists/999/export?profile=internal").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // POST: Playlists/<profile>/<name>.m3u8 に書き、記録する
    let (status, body) = post(
        &app,
        &c,
        &format!("/api/playlists/{id}/export?profile=internal"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["out_path"], "internal/通勤.m3u8");
    assert_eq!(body["count"], 2);
    assert_eq!(body["skipped_missing"], 1);
    let written = std::fs::read_to_string(app.playlists_dir().join("internal/通勤.m3u8")).unwrap();
    assert!(
        written.starts_with("#EXTM3U\n#EXTINF:61,Ar - Three\n"),
        "{written}"
    );
    let (_, body) = get(&app, &c, &format!("/api/playlists/{id}")).await;
    assert_eq!(body["exports"][0]["profile"], "internal");
    assert_eq!(body["exports"][0]["out_path"], "internal/通勤.m3u8");
    assert!(body["exports"][0]["exported_at"].as_i64().unwrap() > 0);
    // 一時ファイルが残らない
    let leftovers: Vec<_> = std::fs::read_dir(app.playlists_dir().join("internal"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(leftovers, vec!["通勤.m3u8"]);

    // 再書き出しは上書き（項目を減らして中身が変わる）
    call(
        &app,
        &c,
        Method::DELETE,
        &format!("/api/playlists/{id}/items"),
        Some(json!({ "track_ids": [b] })),
    )
    .await;
    let (status, body) = post(
        &app,
        &c,
        &format!("/api/playlists/{id}/export?profile=internal"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["count"], 1);
    let written = std::fs::read_to_string(app.playlists_dir().join("internal/通勤.m3u8")).unwrap();
    assert_eq!(
        written,
        "#EXTM3U\n#EXTINF:61,Ar - One\n../../Library/J-Pop/A/B/01 One.flac\n"
    );

    // 改名後の書き出しは新しい名前のファイル（古いファイルは残す。記録は新しいパス）
    call(
        &app,
        &c,
        Method::PATCH,
        &format!("/api/playlists/{id}"),
        Some(json!({ "name": "帰宅" })),
    )
    .await;
    let (_, body) = post(
        &app,
        &c,
        &format!("/api/playlists/{id}/export?profile=internal"),
        json!({}),
    )
    .await;
    assert_eq!(body["out_path"], "internal/帰宅.m3u8");
    assert!(app.playlists_dir().join("internal/帰宅.m3u8").exists());
}

#[tokio::test]
async fn export_get_is_allowed_from_trusted_cidr_without_session() {
    let app = build(r#"trusted_cidrs = ["192.168.1.0/24"]"#, true).await;
    let c = cookie(&app).await;
    let id = create(&app, &c, "p").await;
    let res = send(
        &app,
        req(
            Method::GET,
            &format!("/api/playlists/{id}/export?profile=android"),
        )
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    // 書き出し（POST）と一覧はセッション必須
    let res = send(
        &app,
        req(
            Method::POST,
            &format!("/api/playlists/{id}/export?profile=android"),
        )
        .header("sec-fetch-site", "same-origin")
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let res = send(
        &app,
        req(Method::GET, "/api/playlists")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn export_post_and_import_need_playlists_root() {
    let app = build("", false).await;
    let c = cookie(&app).await;
    let id = create(&app, &c, "p").await;
    let (status, body) = post(
        &app,
        &c,
        &format!("/api/playlists/{id}/export?profile=internal"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let (status, _) = get(&app, &c, "/api/playlists/import").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    // GET の本文は root が無くても返せる
    let (status, _) = get(
        &app,
        &c,
        &format!("/api/playlists/{id}/export?profile=internal"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------- 取り込み

#[tokio::test]
async fn import_lists_m3u8_files_under_playlists_root_and_creates_playlist() {
    let app = app().await;
    let c = cookie(&app).await;
    let crow = insert_track(
        &app.raw(),
        "Anime/Angel Beats!/1.01. Crow Song.m4a",
        "Crow Song",
        false,
    );
    let alchemy = insert_track(
        &app.raw(),
        "Anime/Angel Beats!/1.02. Alchemy.flac",
        "Alchemy",
        false,
    );
    let dir = app.playlists_dir().join("m3u8");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("00_Anime.m3u8"),
        "#\n../Anime/Angel Beats!/1.01. Crow Song.opus\n../Anime/Angel Beats!/1.02. Alchemy.opus\n../Anime/Nope/x.opus\n../Anime/Angel Beats!/1.01. Crow Song.opus\n",
    )
    .unwrap();
    std::fs::write(dir.join("old.m3u"), "x\n").unwrap();
    std::fs::write(dir.join("notes.txt"), "x\n").unwrap();
    std::fs::write(app.playlists_dir().join("top.m3u8"), "").unwrap();

    let (status, body) = get(&app, &c, "/api/playlists/import").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let paths: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        vec!["m3u8/00_Anime.m3u8", "m3u8/old.m3u", "top.m3u8"]
    );
    assert!(body["items"][0]["size"].as_i64().unwrap() > 0);

    let (status, body) = post(
        &app,
        &c,
        "/api/playlists/import",
        json!({ "path": "m3u8/00_Anime.m3u8" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["playlist"]["name"], "00_Anime");
    assert_eq!(body["playlist"]["track_count"], 2);
    assert_eq!(body["matched"], 2);
    assert_eq!(body["duplicates"], 1);
    assert_eq!(body["unresolved"], json!(["../Anime/Nope/x.opus"]));
    let id = body["playlist"]["id"].as_i64().unwrap();
    assert_eq!(items_of(&app, &c, id).await, vec![crow, alchemy]);

    // 同名は 409。name を指定すれば別名で取り込める
    let (status, body) = post(
        &app,
        &c,
        "/api/playlists/import",
        json!({ "path": "m3u8/00_Anime.m3u8" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = post(
        &app,
        &c,
        "/api/playlists/import",
        json!({ "path": "m3u8/00_Anime.m3u8", "name": "Anime 2" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["playlist"]["name"], "Anime 2");

    // 無い・境界の外・拡張子違いは弾く
    let (status, _) = post(
        &app,
        &c,
        "/api/playlists/import",
        json!({ "path": "m3u8/none.m3u8" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = post(
        &app,
        &c,
        "/api/playlists/import",
        json!({ "path": "../etc/passwd" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = post(
        &app,
        &c,
        "/api/playlists/import",
        json!({ "path": "m3u8/notes.txt" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn delivery_export_reports_stale_tags_count() {
    let app = app().await;
    let c = cookie(&app).await;
    let a = insert_track(&app.raw(), "J-Pop/A/B/01 One.flac", "One", false);
    let b = insert_track(&app.raw(), "J-Pop/A/B/02 Two.flac", "Two", false);
    let d = insert_track(&app.raw(), "J-Pop/A/B/03 Three.flac", "Three", false);
    // a: Derived が現在値、b: タグ版だけ陳腐化（配るが追随待ち）、d: Derived 無し
    app.raw()
        .execute(
            "INSERT INTO derived_files (track_id, variant, rel_path, rel_path_key, codec, src_audio_version,
                                        src_tag_version, generated_at, audio_profile, tag_profile)
             VALUES (?1, 'opus', 'opus/J-Pop/A/B/01 One.opus', 'opus/j-pop/a/b/01 one.opus', 'opus', 1, 1, 0, 'opus:256:v1', 'opus:v1'),
                    (?2, 'opus', 'opus/J-Pop/A/B/02 Two.opus', 'opus/j-pop/a/b/02 two.opus', 'opus', 1, 2, 0, 'opus:256:v1', 'opus:v1')",
            params![a, b],
        )
        .unwrap();
    let id = create(&app, &c, "配布").await;
    post(
        &app,
        &c,
        &format!("/api/playlists/{id}/items"),
        json!({ "selection": { "ids": [a, b, d] } }),
    )
    .await;
    let (status, body) = post(
        &app,
        &c,
        &format!("/api/playlists/{id}/export?profile=android"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["count"], 3);
    assert_eq!(body["skipped_missing"], 0);
    assert_eq!(body["stale_tags"], 1);
    // master のプロファイルは Derived を見ないので 0
    let (status, body) = post(
        &app,
        &c,
        &format!("/api/playlists/{id}/export?profile=internal"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["stale_tags"], 0);
}

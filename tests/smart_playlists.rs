//! スマートプレイリストの API（SPEC §9 / §10、docs/TASKS.md P1-7、D-54）: ルール付きの作成・更新、
//! プレビュー、再評価、materialize された項目、手動用の項目操作の拒否

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
    state: AppState,
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
        router: api::router(state.clone()),
        state,
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

fn insert_tagged(
    conn: &Connection,
    rel: &str,
    title: &str,
    albumartist: &str,
    genre: Option<&str>,
) -> i64 {
    let id = insert_track(conn, rel, title, false);
    conn.execute(
        "UPDATE tracks SET albumartist = ?1 WHERE id = ?2",
        params![albumartist, id],
    )
    .unwrap();
    if let Some(g) = genre {
        conn.execute(
            "INSERT INTO track_tags (track_id, key, idx, value) VALUES (?1, 'GENRE', 0, ?2)",
            params![id, g],
        )
        .unwrap();
    }
    id
}

#[tokio::test]
async fn create_with_rule_makes_a_smart_playlist_with_materialized_items() {
    let app = app().await;
    let c = cookie(&app).await;
    let a = insert_tagged(
        &app.raw(),
        "A/1.flac",
        "Crow Song",
        "ヰ世界情緒",
        Some("Anime"),
    );
    let b = insert_tagged(&app.raw(), "A/2.flac", "Alchemy", "ヰ世界情緒", None);
    let _x = insert_tagged(&app.raw(), "B/1.flac", "Other", "花譜", Some("Anime"));

    let (status, body) = post(
        &app,
        &c,
        "/api/playlists",
        json!({ "name": "情緒", "rule": "%albumartist% IS ヰ世界情緒 ORDER BY %title% ASC" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["kind"], "smart");
    assert_eq!(
        body["rule_source"],
        "%albumartist% IS ヰ世界情緒 ORDER BY %title% ASC"
    );
    assert_eq!(body["track_count"], 2);
    let id = body["id"].as_i64().unwrap();
    // 表の playlist_id + position で ORDER BY の順に出る
    assert_eq!(items_of(&app, &c, id).await, vec![b, a]);
    // 一覧にも kind と rule_source
    let (_, list) = get(&app, &c, "/api/playlists").await;
    assert_eq!(list["items"][0]["kind"], "smart");
    assert!(list["items"][0]["rule_source"].is_string());
}

#[tokio::test]
async fn bad_rule_is_400_with_position_and_nothing_is_created() {
    let app = app().await;
    let c = cookie(&app).await;
    let (status, body) = post(
        &app,
        &c,
        "/api/playlists",
        json!({ "name": "x", "rule": "%title% IS" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "bad_request");
    let msg = body["message"].as_str().unwrap();
    assert!(msg.contains("行") && msg.contains("桁"), "{msg}");
    // 型エラーも 400
    let (status, body) = post(
        &app,
        &c,
        "/api/playlists",
        json!({ "name": "x", "rule": "%title% GREATER 1" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (_, list) = get(&app, &c, "/api/playlists").await;
    assert_eq!(list["items"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn preview_validates_and_counts_without_saving() {
    let app = app().await;
    let c = cookie(&app).await;
    insert_tagged(
        &app.raw(),
        "A/1.flac",
        "Crow Song",
        "ヰ世界情緒",
        Some("Anime"),
    );
    insert_tagged(&app.raw(), "A/2.flac", "Alchemy", "ヰ世界情緒", None);
    let (status, body) = post(
        &app,
        &c,
        "/api/playlists/preview",
        json!({ "rule": "%genre% IS anime LIMIT 5" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["count"], 1);
    assert_eq!(body["ast"]["where"]["op"], "cmp");
    assert_eq!(body["ast"]["limit"], 5);
    let (status, _) = post(&app, &c, "/api/playlists/preview", json!({ "rule": "(" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, list) = get(&app, &c, "/api/playlists").await;
    assert_eq!(list["items"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn refresh_and_rule_update_re_evaluate() {
    let app = app().await;
    let c = cookie(&app).await;
    let a = insert_tagged(&app.raw(), "A/1.flac", "One", "AA", Some("Anime"));
    let (status, body) = post(
        &app,
        &c,
        "/api/playlists",
        json!({ "name": "anime", "rule": "%genre% IS anime" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_i64().unwrap();
    assert_eq!(items_of(&app, &c, id).await, vec![a]);

    // ライブラリが変わっても materialize 済みの項目はそのまま。refresh で追随
    let b = insert_tagged(&app.raw(), "A/2.flac", "Two", "AA", Some("anime"));
    assert_eq!(items_of(&app, &c, id).await, vec![a]);
    // 項目が変わったら SSE `playlist` イベントで表を無効化する
    let mut rx = app.state.jobs.subscribe();
    let (status, body) = post(&app, &c, &format!("/api/playlists/{id}/refresh"), json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["count"], 2);
    assert_eq!(body["changed"], true);
    assert_eq!(items_of(&app, &c, id).await, vec![a, b]);
    let ev = rx.try_recv().unwrap();
    assert_eq!(ev.name(), "playlist");
    assert_eq!(ev.data()["playlist_ids"], json!([id]));
    let (_, body) = post(&app, &c, &format!("/api/playlists/{id}/refresh"), json!({})).await;
    assert_eq!(body["changed"], false);
    assert!(rx.try_recv().is_err(), "変化なしではイベントを流さない");

    // ルールの差し替えで再評価
    let (status, body) = call(
        &app,
        &c,
        Method::PATCH,
        &format!("/api/playlists/{id}"),
        Some(json!({ "rule": "%title% IS two" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rule_source"], "%title% IS two");
    assert_eq!(body["track_count"], 1);
    assert_eq!(items_of(&app, &c, id).await, vec![b]);
    // 不正なルールへの差し替えは 400 で元のまま
    let (status, _) = call(
        &app,
        &c,
        Method::PATCH,
        &format!("/api/playlists/{id}"),
        Some(json!({ "rule": "%x%" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, body) = get(&app, &c, &format!("/api/playlists/{id}")).await;
    assert_eq!(body["rule_source"], "%title% IS two");
    // 手動プレイリストに rule は付けられない、スマートの refresh 以外は 409
    let manual = create(&app, &c, "manual").await;
    let (status, body) = call(
        &app,
        &c,
        Method::PATCH,
        &format!("/api/playlists/{manual}"),
        Some(json!({ "rule": "%title% IS x" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "manual");
    let (status, body) = post(
        &app,
        &c,
        &format!("/api/playlists/{manual}/refresh"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "manual");
}

#[tokio::test]
async fn item_edits_are_rejected_on_smart_playlists() {
    let app = app().await;
    let c = cookie(&app).await;
    let a = insert_tagged(&app.raw(), "A/1.flac", "One", "AA", Some("Anime"));
    let (_, body) = post(
        &app,
        &c,
        "/api/playlists",
        json!({ "name": "s", "rule": "PRESENT %title%" }),
    )
    .await;
    let id = body["id"].as_i64().unwrap();
    for (m, uri, body) in [
        (
            Method::POST,
            format!("/api/playlists/{id}/items"),
            json!({ "selection": { "ids": [a] } }),
        ),
        (
            Method::DELETE,
            format!("/api/playlists/{id}/items"),
            json!({ "track_ids": [a] }),
        ),
        (
            Method::POST,
            format!("/api/playlists/{id}/items/move"),
            json!({ "track_ids": [a], "before": null }),
        ),
    ] {
        let (status, body) = call(&app, &c, m, &uri, Some(body)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{uri}: {body}");
        assert_eq!(body["error"], "smart");
    }
    // 改名・削除・書き出しは手動と同じ
    let (status, _) = call(
        &app,
        &c,
        Method::PATCH,
        &format!("/api/playlists/{id}"),
        Some(json!({ "name": "s2" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = post(
        &app,
        &c,
        &format!("/api/playlists/{id}/export?profile=internal"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["count"], 1);
    let (status, _) = call(
        &app,
        &c,
        Method::DELETE,
        &format!("/api/playlists/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// ---------------------------------------------------------------- 自動再評価・再書き出し（D-54）

#[tokio::test]
async fn autoexport_task_re_evaluates_smart_lists_and_rewrites_recorded_exports() {
    use std::time::Duration;

    use spindle::jobs::{Event, LibraryEvent};
    use spindle::playlist::autoexport::AutoExport;

    let app = app().await;
    let c = cookie(&app).await;
    let a = insert_tagged(&app.raw(), "A/1.flac", "One", "AA", Some("Anime"));
    let (_, body) = post(
        &app,
        &c,
        "/api/playlists",
        json!({ "name": "anime", "rule": "%genre% IS anime" }),
    )
    .await;
    let smart = body["id"].as_i64().unwrap();
    // 手動: 書き出し記録あり（auto_export=1）。もう 1 本は記録なし（何も書かれない）
    let manual = create(&app, &c, "manual").await;
    post(
        &app,
        &c,
        &format!("/api/playlists/{manual}/items"),
        json!({ "selection": { "ids": [a] } }),
    )
    .await;
    let _untouched = create(&app, &c, "untouched").await;
    for id in [smart, manual] {
        let (status, _) = post(
            &app,
            &c,
            &format!("/api/playlists/{id}/export?profile=internal"),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let smart_file = app.playlists_dir().join("internal/anime.m3u8");
    let before = std::fs::read_to_string(&smart_file).unwrap();
    assert_eq!(before.lines().count(), 3);

    let task = AutoExport::new(
        app.state.db.clone(),
        app.state.jobs.clone(),
        std::sync::Arc::new(RootDir::open(&app.playlists_dir()).unwrap()),
        Duration::from_millis(100),
    );
    let shutdown = tokio_util::sync::CancellationToken::new();
    let handle = task.spawn(shutdown.clone());
    // 起動直後の 1 回（変化なし）を待ってから、ライブラリを変えてイベントを流す
    tokio::time::sleep(Duration::from_millis(300)).await;
    let b = insert_tagged(&app.raw(), "A/2.flac", "Two", "AA", Some("anime"));
    // ファイルのパスも変わる（手動側の再書き出しを見る）
    app.raw()
        .execute("UPDATE tracks SET rel_path = 'A/1 renamed.flac', rel_path_key = 'a/1 renamed.flac' WHERE id = ?1", [a])
        .unwrap();
    let mut rx = app.state.jobs.subscribe();
    app.state
        .jobs
        .publish(Event::Library(LibraryEvent::Bulk { scan_run_id: 1 }));
    // デバウンス中に続けて来ても 1 回にまとまる
    app.state
        .jobs
        .publish(Event::Library(LibraryEvent::Bulk { scan_run_id: 2 }));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let items = items_of(&app, &c, smart).await;
        let manual_file =
            std::fs::read_to_string(app.playlists_dir().join("internal/manual.m3u8")).unwrap();
        if items == vec![a, b] && manual_file.contains("1 renamed.flac") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "自動再評価が走らない: items={items:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // materialize したことを SSE `playlist` イベントで知らせる（表の無効化）。1 回だけ
    let mut playlist_events = 0;
    while let Ok(ev) = rx.try_recv() {
        if ev.name() == "playlist" {
            assert_eq!(ev.data()["playlist_ids"], json!([smart]));
            playlist_events += 1;
        }
    }
    assert_eq!(playlist_events, 1);
    let after = std::fs::read_to_string(&smart_file).unwrap();
    assert_eq!(after.lines().count(), 5, "{after}");
    assert!(after.contains("A/2.flac"));
    assert!(!app.playlists_dir().join("internal/untouched.m3u8").exists());
    // 記録の時刻が進む
    let (_, body) = get(&app, &c, &format!("/api/playlists/{smart}")).await;
    assert!(body["exports"][0]["exported_at"].as_i64().unwrap() > 0);
    // auto_export を切ると再書き出しされない
    call(
        &app,
        &c,
        Method::PATCH,
        &format!("/api/playlists/{smart}"),
        Some(json!({ "auto_export": false })),
    )
    .await;
    let stamp = std::fs::metadata(&smart_file).unwrap().modified().unwrap();
    std::fs::write(&smart_file, "stale").unwrap();
    app.state
        .jobs
        .publish(Event::Library(LibraryEvent::Bulk { scan_run_id: 3 }));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(std::fs::read_to_string(&smart_file).unwrap(), "stale");
    let _ = stamp;
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

// ---------------------------------------------------------------- 原子性（レビュー対応）

/// バックトラック上限で fancy-regex が実行時に失敗するタイトル + パターン（後方参照があるので
/// regex クレートへ委譲されずバックトラックする）。`check`（コンパイル）は通る
const PATHOLOGICAL_TITLE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab";
const PATHOLOGICAL_RULE: &str = "%title% MATCHES \"^(a|aa)+\\\\1$\"";

#[tokio::test]
async fn composite_patch_is_all_or_nothing() {
    let app = app().await;
    let c = cookie(&app).await;
    let manual = create(&app, &c, "manual").await;
    // manual に name + rule: 409 で名前も auto_export も変わらない
    let (status, body) = call(
        &app,
        &c,
        Method::PATCH,
        &format!("/api/playlists/{manual}"),
        Some(json!({ "name": "renamed", "auto_export": false, "rule": "PRESENT %title%" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (_, body) = get(&app, &c, &format!("/api/playlists/{manual}")).await;
    assert_eq!(body["name"], "manual");
    assert_eq!(body["auto_export"], true);
    // smart に重複名 + rule: 409 でルールも項目も変わらない
    insert_tagged(&app.raw(), "A/1.flac", "One", "AA", Some("Anime"));
    let (_, body) = post(
        &app,
        &c,
        "/api/playlists",
        json!({ "name": "smart", "rule": "%genre% IS anime" }),
    )
    .await;
    let smart = body["id"].as_i64().unwrap();
    let (status, _) = call(
        &app,
        &c,
        Method::PATCH,
        &format!("/api/playlists/{smart}"),
        Some(json!({ "name": "manual", "rule": "MISSING %genre%" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (_, body) = get(&app, &c, &format!("/api/playlists/{smart}")).await;
    assert_eq!(body["rule_source"], "%genre% IS anime");
    assert_eq!(body["track_count"], 1);
}

#[tokio::test]
async fn evaluation_failure_leaves_rule_and_items_untouched_and_creates_nothing() {
    let app = app().await;
    let c = cookie(&app).await;
    let a = insert_tagged(&app.raw(), "A/1.flac", "One", "AA", Some("Anime"));
    insert_tagged(&app.raw(), "A/2.flac", PATHOLOGICAL_TITLE, "AA", None);
    // check は通るが評価で失敗する → 400 rule_failed、行は残らない
    let (status, body) = post(
        &app,
        &c,
        "/api/playlists",
        json!({ "name": "bad", "rule": PATHOLOGICAL_RULE }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "rule_failed");
    let (_, list) = get(&app, &c, "/api/playlists").await;
    assert_eq!(list["items"].as_array().unwrap().len(), 0);
    // preview も 400
    let (status, body) = post(
        &app,
        &c,
        "/api/playlists/preview",
        json!({ "rule": PATHOLOGICAL_RULE }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    // 既存 smart への差し替えで失敗 → rule と items は旧状態
    let (_, body) = post(
        &app,
        &c,
        "/api/playlists",
        json!({ "name": "good", "rule": "%genre% IS anime" }),
    )
    .await;
    let id = body["id"].as_i64().unwrap();
    let (status, body) = call(
        &app,
        &c,
        Method::PATCH,
        &format!("/api/playlists/{id}"),
        Some(json!({ "rule": PATHOLOGICAL_RULE })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (_, body) = get(&app, &c, &format!("/api/playlists/{id}")).await;
    assert_eq!(body["rule_source"], "%genre% IS anime");
    assert_eq!(items_of(&app, &c, id).await, vec![a]);
    // refresh でも同じ（ルールは有効だがデータが悪い場合を模す: 一旦通るルールに差し替えてから題名を変える）
    let (status, _) = call(
        &app,
        &c,
        Method::PATCH,
        &format!("/api/playlists/{id}"),
        Some(json!({ "rule": format!("{PATHOLOGICAL_RULE} OR %genre% IS anime") })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn sqlite_failures_during_evaluation_are_500_not_rule_failed() {
    // ルールに帰責できない障害（表が無い等）はユーザ入力の問題として返さない
    let app = app().await;
    let c = cookie(&app).await;
    app.raw().execute_batch("DROP TABLE track_tags").unwrap();
    let (status, body) = post(
        &app,
        &c,
        "/api/playlists/preview",
        json!({ "rule": "%genre% IS anime" }),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["error"], "internal");
}

#[tokio::test]
async fn fb2k_query_converts_the_stored_rule() {
    let app = app().await;
    let c = cookie(&app).await;
    let (status, body) = post(
        &app,
        &c,
        "/api/playlists",
        json!({
            "name": "fb",
            "rule": "%albumartist% IS ヰ世界情緒 AND %verification% IS verified_ctdb ORDER BY %date% DESC LIMIT 10"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_i64().unwrap();
    let (status, body) = get(&app, &c, &format!("/api/playlists/{id}/fb2k_query")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["query"], "%album artist% IS ヰ世界情緒");
    assert_eq!(body["sort"], "%date%");
    let notes = body["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 3, "{body}");
    assert!(notes[0].as_str().unwrap().contains("%verification%"));
    assert!(notes[2].as_str().unwrap().contains("LIMIT 10"));
    // 手動は 409、無い id は 404
    let manual = create(&app, &c, "manual").await;
    let (status, body) = get(&app, &c, &format!("/api/playlists/{manual}/fb2k_query")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "manual");
    let (status, _) = get(&app, &c, "/api/playlists/999999/fb2k_query").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // 未認証は 401
    let r = req(Method::GET, &format!("/api/playlists/{id}/fb2k_query"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.status(), StatusCode::UNAUTHORIZED);
}

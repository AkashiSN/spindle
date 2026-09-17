//! `GET /api/tracks` / `/api/tracks/:id` / `/api/search` / `/api/albums`（P0-7）と
//! selection のスナップショット（D-33）。仕様: docs/SPEC.md §9、docs/DECISIONS.md D-27 / D-39

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use rusqlite::{params, Connection};
use tower::ServiceExt;

use spindle::api::{self, auth, selection, AppState};
use spindle::config::Config;
use spindle::db::Db;
use spindle::domain::selection::SelectionBody;

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
}

async fn build(auth_override: &str) -> TestApp {
    let dir = tempfile::tempdir().unwrap();
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
    let state = AppState::new(config, db, mode);
    TestApp {
        state: state.clone(),
        router: api::router(state),
        dir,
    }
}

async fn app() -> TestApp {
    build("").await
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

async fn get(app: &TestApp, c: &str, uri: &str) -> (StatusCode, serde_json::Value) {
    let r = req(Method::GET, uri)
        .header(header::COOKIE, c)
        .body(Body::empty())
        .unwrap();
    let res = send(app, r).await;
    let status = res.status();
    (status, json(res).await)
}

fn insert_track(conn: &Connection, rel: &str, title: &str, album_id: Option<i64>) -> i64 {
    conn.execute(
        "INSERT INTO tracks (album_id, rel_path, rel_path_key, dev, inode, size, mtime_ns, ctime_ns, codec, lossless,
           title, artist_display, album, albumartist, track_no, disc_no, seen_at)
         VALUES (?1, ?2, ?2, 1, abs(random()), 1, 1, 1, 'flac', 1, ?3, 'Ar', 'Al', 'AA', 1, 1, 1)",
        params![album_id, rel, title],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn insert_album(conn: &Connection, rel_dir: &str) -> i64 {
    conn.execute(
        "INSERT INTO categories (name) VALUES ('J-Pop') ON CONFLICT DO NOTHING",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO albums (rel_dir, rel_dir_key, category_id, albumartist, album, date)
         VALUES (?1, ?1, (SELECT id FROM categories WHERE name = 'J-Pop'), 'AA', 'Al', '2024')",
        [rel_dir],
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

// ---------------------------------------------------------------- 認証

#[tokio::test]
async fn list_search_and_albums_require_session() {
    let app = app().await;
    for uri in [
        "/api/tracks",
        "/api/search?q=abc",
        "/api/albums",
        "/api/albums/1",
    ] {
        let res = send(&app, req(Method::GET, uri).body(Body::empty()).unwrap()).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{uri}");
    }
}

// ---------------------------------------------------------------- 一覧

#[tokio::test]
async fn list_returns_spec_shape_with_cursor_and_total() {
    let app = app().await;
    let c = cookie(&app).await;
    {
        let conn = app.raw();
        let alb = insert_album(&conn, "J-Pop/AA/Al");
        for i in 0..5 {
            insert_track(
                &conn,
                &format!("J-Pop/AA/Al/{i}.flac"),
                &format!("t{i}"),
                Some(alb),
            );
        }
    }
    let (status, body) = get(&app, &c, "/api/tracks?sort=title&limit=2").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 5);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let row = &items[0];
    for key in [
        "id",
        "title",
        "artist_display",
        "album",
        "albumartist",
        "track_no",
        "disc_no",
        "date",
        "category",
        "duration_ms",
        "codec",
        "lossless",
        "verification",
        "rg_scanned_at",
        "rg_written_at",
        "rg",
        "derived",
        "pending_batch_id",
        "conflict_batch_id",
        "duplicate_group",
        "hardlink",
        "missing_since",
        "rel_path",
    ] {
        assert!(row.get(key).is_some(), "{key} が無い: {row}");
    }
    assert_eq!(row["title"], "t0");
    assert_eq!(row["category"], "J-Pop");
    assert_eq!(row["derived"], serde_json::Value::Null);
    assert_eq!(row["rg"], serde_json::Value::Null, "未解析なら null");
    assert_eq!(row["pending_batch_id"], serde_json::Value::Null);
    assert_eq!(row["hardlink"], false);
    assert_eq!(row["lossless"], true);
    let cursor = body["next_cursor"].as_str().expect("次ページがある");

    let (status, body2) = get(
        &app,
        &c,
        &format!("/api/tracks?sort=title&limit=2&cursor={cursor}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body2["items"][0]["title"], "t2");
    assert_eq!(body2["total"], 5);
    let cursor = body2["next_cursor"].as_str().unwrap();
    let (_, body3) = get(
        &app,
        &c,
        &format!("/api/tracks?sort=title&limit=2&cursor={cursor}"),
    )
    .await;
    assert_eq!(body3["items"].as_array().unwrap().len(), 1);
    assert_eq!(body3["next_cursor"], serde_json::Value::Null, "最終ページ");

    // 解析済みの行は rg に 4 値（album 無しなら null）
    app.raw()
        .execute(
            "UPDATE tracks SET rg_track_gain = -6.5, rg_track_peak = 0.9, rg_scanned_at = 1
             WHERE title = 't0'",
            [],
        )
        .unwrap();
    let (_, body) = get(&app, &c, "/api/tracks?sort=title&limit=1").await;
    let rg = &body["items"][0]["rg"];
    assert_eq!(rg["track_gain"], -6.5);
    assert_eq!(rg["track_peak"], 0.9);
    assert_eq!(rg["album_gain"], serde_json::Value::Null);

    // filter は URL エンコードした JSON
    let f = urlenc(r#"{"category":"J-Pop","flags":["missing"]}"#);
    let (status, body) = get(&app, &c, &format!("/api/tracks?filter={f}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);
    let f = urlenc(r#"{"category":"J-Pop"}"#);
    let (_, body) = get(&app, &c, &format!("/api/tracks?filter={f}&sort=-title")).await;
    assert_eq!(body["total"], 5);
    assert_eq!(body["items"][0]["title"], "t4");
}

#[tokio::test]
async fn bad_filter_sort_or_cursor_is_400() {
    let app = app().await;
    let c = cookie(&app).await;
    let f = urlenc(r#"{"title":"x"}"#);
    for uri in [
        format!("/api/tracks?filter={f}"),
        "/api/tracks?filter=not-json".to_owned(),
        "/api/tracks?sort=rowid".to_owned(),
        "/api/tracks?cursor=!!!".to_owned(),
        "/api/tracks?limit=abc".to_owned(),
        "/api/search".to_owned(),
        "/api/search?q=".to_owned(),
    ] {
        let (status, body) = get(&app, &c, &uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
    }
    // 別のソートで発行した正規のカーソルを再送しても 400（向き違いも）
    {
        let conn = app.raw();
        insert_track(&conn, "a.flac", "a", None);
        insert_track(&conn, "b.flac", "b", None);
    }
    let (status, body) = get(&app, &c, "/api/tracks?sort=duration&limit=1").await;
    assert_eq!(status, StatusCode::OK);
    let cursor = body["next_cursor"].as_str().unwrap().to_owned();
    let (status, _) = get(
        &app,
        &c,
        &format!("/api/tracks?sort=duration&limit=1&cursor={cursor}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "同じソートなら通る");
    for sort in ["title", "-duration", "album", ""] {
        let (status, body) = get(
            &app,
            &c,
            &format!("/api/tracks?sort={sort}&cursor={cursor}"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "sort={sort}: {body}");
        assert_eq!(body["error"], "bad_request");
    }
}

#[tokio::test]
async fn search_is_the_same_list_with_q_and_falls_back_to_like() {
    let app = app().await;
    let c = cookie(&app).await;
    {
        let conn = app.raw();
        insert_track(&conn, "1.flac", "ヰ世界情緒の歌", None);
        insert_track(&conn, "2.flac", "別の曲", None);
    }
    let (status, body) = get(&app, &c, &format!("/api/search?q={}", urlenc("世界情緒"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["title"], "ヰ世界情緒の歌");
    assert!(body.get("next_cursor").is_some());
    // 2 文字は LIKE
    let (_, body) = get(&app, &c, &format!("/api/search?q={}", urlenc("情緒"))).await;
    assert_eq!(body["total"], 1);
    // /api/tracks 側でも q は filter.q より優先
    let f = urlenc(r#"{"q":"zzz"}"#);
    let (_, body) = get(
        &app,
        &c,
        &format!("/api/tracks?filter={f}&q={}", urlenc("別の")),
    )
    .await;
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["title"], "別の曲");
}

// ---------------------------------------------------------------- 1 件

#[tokio::test]
async fn get_track_returns_full_row_with_session_and_limited_fields_from_trusted_cidr() {
    let app = build(r#"trusted_cidrs = ["192.168.1.0/24"]"#).await;
    let c = cookie(&app).await;
    let id = insert_track(&app.raw(), "x/1.flac", "one", None);

    let (status, body) = get(&app, &c, &format!("/api/tracks/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["rel_path"], "x/1.flac");
    assert!(body.get("pending_batch_id").is_some());

    // CIDR 内・セッション無し: 通るが限定フィールド
    let res = send(
        &app,
        req(Method::GET, &format!("/api/tracks/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = json(res).await;
    assert_eq!(body["title"], "one");
    assert_eq!(body["codec"], "flac");
    for hidden in [
        "rel_path",
        "pending_batch_id",
        "conflict_batch_id",
        "duplicate_group",
        "missing_since",
        "hardlink",
        "verification",
        "derived",
    ] {
        assert!(
            body.get(hidden).is_none(),
            "{hidden} は CIDR 経由で見せない: {body}"
        );
    }

    let (status, _) = get(&app, &c, "/api/tracks/99999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------- アルバム

#[tokio::test]
async fn albums_list_and_get() {
    let app = app().await;
    let c = cookie(&app).await;
    let alb = {
        let conn = app.raw();
        let alb = insert_album(&conn, "J-Pop/AA/Al");
        insert_track(&conn, "J-Pop/AA/Al/1.flac", "a", Some(alb));
        insert_track(&conn, "J-Pop/AA/Al/2.flac", "b", Some(alb));
        conn.execute("UPDATE tracks SET duration_ms = 1000", [])
            .unwrap();
        conn.execute("UPDATE tracks SET missing_since = 5 WHERE title = 'b'", [])
            .unwrap();
        alb
    };
    let (status, body) = get(&app, &c, "/api/albums").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], alb);
    assert_eq!(items[0]["category"], "J-Pop");
    assert_eq!(items[0]["albumartist"], "AA");
    assert_eq!(items[0]["album"], "Al");
    assert_eq!(items[0]["date"], "2024");
    assert_eq!(items[0]["track_count"], 1, "missing は数えない");
    assert_eq!(items[0]["duration_ms"], 1000);
    assert_eq!(items[0]["rel_dir"], "J-Pop/AA/Al");

    let (status, one) = get(&app, &c, &format!("/api/albums/{alb}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&one, &items[0]);
    let (status, _) = get(&app, &c, "/api/albums/999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------- selection スナップショット

#[tokio::test]
async fn snapshot_fixes_the_selection_so_later_rows_are_not_included() {
    let app = app().await;
    let (a, b) = {
        let conn = app.raw();
        let a = insert_track(&conn, "a.flac", "a", None);
        let b = insert_track(&conn, "b.flac", "b", None);
        (a, b)
    };
    // フィルタ形（全件）+ 除外
    let body = SelectionBody::Filter {
        filter: "{}".to_owned(),
        exclude_ids: vec![b],
    };
    let ops = serde_json::json!([{ "op": "set", "key": "TITLE", "value": "x" }]);
    let (token, snap) = selection::snapshot(&app.state, body, ops.clone())
        .await
        .unwrap();
    assert_eq!(snap.rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![a]);
    assert_eq!(snap.rows[0].tag_version, 1);
    assert_eq!(snap.rows[0].rel_path, "a.flac");

    // preview 後にスキャンで行が増えても token の集合は変わらない（D-33）
    insert_track(&app.raw(), "c.flac", "c", None);
    let again = selection::lookup(&app.state.selection, &token).expect("期限内");
    assert_eq!(again, snap);
    assert_eq!(again.ops, ops);
    // 一方、いま解決し直せば増えている
    let now = selection::resolve(
        &app.state,
        SelectionBody::Filter {
            filter: "{}".to_owned(),
            exclude_ids: vec![b],
        },
    )
    .await
    .unwrap();
    assert_eq!(now.len(), 2);

    assert!(selection::lookup(&app.state.selection, "bogus").is_none());
    // apply は take で原子的に消費する。二度目は preview_stale 扱い（None）
    assert_eq!(selection::take(&app.state.selection, &token), Some(snap));
    assert!(selection::take(&app.state.selection, &token).is_none());
    assert!(selection::lookup(&app.state.selection, &token).is_none());
    // 不正なフィルタ文字列は解決前に弾かれる
    let bad = SelectionBody::Filter {
        filter: r#"{"nope":1}"#.to_owned(),
        exclude_ids: vec![],
    };
    assert!(matches!(
        selection::snapshot(&app.state, bad, serde_json::Value::Null).await,
        Err(selection::SelectionError::Filter(_))
    ));
    // ids 形
    let (_, snap) = selection::snapshot(
        &app.state,
        SelectionBody::Ids { ids: vec![b, 9999] },
        serde_json::Value::Null,
    )
    .await
    .unwrap();
    assert_eq!(snap.rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![b]);
}

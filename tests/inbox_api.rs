//! Inbox の API（`GET /api/inbox`、`POST /api/inbox/scan`、`/:id/approve` / `reject` / `reopen`。
//! SPEC §9、D-68、P2-10）。件は DB に直接作る（走査と配置は tests/inbox_job.rs）

mod common;

use std::net::SocketAddr;
use std::path::PathBuf;
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
use spindle::db::inbox::{self, FileRow, ItemState};
use spindle::db::Db;
use spindle::fsroot::RootDir;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

struct App {
    router: Router,
    db: Arc<Db>,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Inbox")).unwrap();
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let config = Arc::new(Config::parse(EXAMPLE).unwrap());
        let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
            .await
            .unwrap();
        let inbox = Arc::new(RootDir::open(&dir.path().join("Inbox")).unwrap());
        let state = AppState::new(config, db.clone(), mode).with_inbox(inbox);
        Self {
            router: api::router(state),
            db,
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

    /// 件を作る（2 曲。1 曲目は TITLE あり、2 曲目は TITLE 無し）
    async fn item(&self, rel_dir: &str) -> i64 {
        let rel_dir = rel_dir.to_owned();
        self.db
            .write(move |c| {
                let id = inbox::insert_item(
                    c,
                    &rel_dir,
                    &spindle::domain::relpath::canonical_key(&rel_dir),
                    1000,
                )?;
                let f = |rel: &str, tags: Vec<(&str, &str)>| FileRow {
                    rel_path: format!("{rel_dir}/{rel}"),
                    inode: 1,
                    size: 1,
                    mtime_ns: 0,
                    ctime_ns: 0,
                    codec: "flac".into(),
                    lossless: true,
                    sample_rate: Some(44100),
                    bit_depth: Some(16),
                    channels: Some(2),
                    duration_ms: Some(1000),
                    tags: tags
                        .into_iter()
                        .map(|(k, v)| (k.to_owned(), v.to_owned()))
                        .collect(),
                };
                inbox::replace_files(
                    c,
                    id,
                    &[
                        f(
                            "01.flac",
                            vec![
                                ("TITLE", "One"),
                                ("ALBUM", "Album"),
                                ("ALBUMARTIST", "Artist"),
                                ("TRACKNUMBER", "1"),
                                ("GENRE", "Rock"),
                            ],
                        ),
                        f("02.flac", vec![("ALBUM", "Album"), ("TRACKNUMBER", "2")]),
                    ],
                )?;
                Ok(id)
            })
            .await
            .unwrap()
    }

    /// 件を作る（ファイルごとのタグを指定）
    async fn item_with(&self, rel_dir: &str, files: Vec<(&str, Vec<(&str, &str)>)>) -> i64 {
        let rel_dir = rel_dir.to_owned();
        let files: Vec<(String, Vec<(String, String)>)> = files
            .into_iter()
            .map(|(rel, tags)| {
                (
                    rel.to_owned(),
                    tags.into_iter()
                        .map(|(k, v)| (k.to_owned(), v.to_owned()))
                        .collect(),
                )
            })
            .collect();
        self.db
            .write(move |c| {
                let id = inbox::insert_item(
                    c,
                    &rel_dir,
                    &spindle::domain::relpath::canonical_key(&rel_dir),
                    1000,
                )?;
                let rows: Vec<FileRow> = files
                    .into_iter()
                    .map(|(rel, tags)| FileRow {
                        rel_path: format!("{rel_dir}/{rel}"),
                        inode: 1,
                        size: 1,
                        mtime_ns: 0,
                        ctime_ns: 0,
                        codec: "flac".into(),
                        lossless: true,
                        sample_rate: Some(44100),
                        bit_depth: Some(16),
                        channels: Some(2),
                        duration_ms: Some(1000),
                        tags,
                    })
                    .collect();
                inbox::replace_files(c, id, &rows)?;
                Ok(id)
            })
            .await
            .unwrap()
    }

    fn inbox_path(&self, rel: &str) -> PathBuf {
        self.dir.path().join("Inbox").join(rel)
    }

    /// 生の応答（ヘッダを見る用）
    async fn raw(
        &self,
        c: Option<&str>,
        uri: &str,
        if_none_match: Option<&str>,
    ) -> axum::response::Response {
        let mut b = req(Method::GET, uri);
        if let Some(c) = c {
            b = b.header(header::COOKIE, c);
        }
        if let Some(e) = if_none_match {
            b = b.header(header::IF_NONE_MATCH, e);
        }
        self.router
            .clone()
            .oneshot(b.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn state(&self, id: i64) -> ItemState {
        self.db
            .read(move |c| inbox::get(c, id))
            .await
            .unwrap()
            .unwrap()
            .state
    }
}

fn req(method: Method, uri: &str) -> axum::http::request::Builder {
    let peer: SocketAddr = LAN.parse().unwrap();
    let mut b = Request::builder().method(method).uri(uri);
    b.extensions_mut().unwrap().insert(ConnectInfo(peer));
    b
}

fn draft(rel_dir: &str) -> Value {
    json!({
        "category": null,
        "albumartist": "Artist",
        "album": "Album",
        "date": "2024",
        "tracks": [
            { "rel_path": format!("{rel_dir}/01.flac"), "disc_no": 1, "track_no": 1, "title": "One", "artist": "" },
            { "rel_path": format!("{rel_dir}/02.flac"), "disc_no": 1, "track_no": 2, "title": "Two", "artist": "" }
        ]
    })
}

#[tokio::test]
async fn list_returns_items_with_proposal_tracks_and_warnings() {
    let app = App::new().await;
    let c = app.cookie().await;
    app.db
        .write(|c| {
            c.execute("INSERT INTO categories (name) VALUES ('Rock')", [])?;
            c.execute(
                "INSERT INTO genre_category_map (genre, category_id) VALUES ('Rock', 1)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let id = app.item("AlbumA").await;
    let (st, body) = app.get(&c, "/api/inbox").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let it = &items[0];
    assert_eq!(it["id"], id);
    assert_eq!(it["rel_dir"], "AlbumA");
    assert_eq!(it["state"], "pending");
    assert_eq!(it["tracks"].as_array().unwrap().len(), 2);
    assert_eq!(it["tracks"][0]["rel_path"], "AlbumA/01.flac");
    assert_eq!(it["tracks"][0]["codec"], "flac");
    assert_eq!(it["proposal"]["albumartist"], "Artist");
    assert_eq!(it["proposal"]["album"], "Album");
    assert_eq!(it["proposal"]["category"], "Rock");
    assert_eq!(it["proposal"]["tracks"][1]["title"], "");
    let w = it["warnings"].as_array().unwrap();
    assert_eq!(w.len(), 1);
    assert!(w[0].as_str().unwrap().contains("02.flac"));
    assert!(it["draft"].is_null());
}

#[tokio::test]
async fn approve_validates_and_enqueues_job() {
    let app = App::new().await;
    let c = app.cookie().await;
    let id = app.item("AlbumA").await;
    // 400: タイトルが空
    let mut bad = draft("AlbumA");
    bad["tracks"][1]["title"] = json!("");
    let (st, body) = app.post(&c, &format!("/api/inbox/{id}/approve"), bad).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "bad_request");
    assert!(body["message"].as_str().unwrap().contains("02.flac"));
    // 400: 番号の重複
    let mut bad = draft("AlbumA");
    bad["tracks"][1]["track_no"] = json!(1);
    let (st, _) = app.post(&c, &format!("/api/inbox/{id}/approve"), bad).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // 400: 件に無いファイル
    let mut bad = draft("AlbumA");
    bad["tracks"][1]["rel_path"] = json!("AlbumA/09.flac");
    let (st, _) = app.post(&c, &format!("/api/inbox/{id}/approve"), bad).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(app.state(id).await, ItemState::Pending);
    // 404
    let (st, _) = app
        .post(&c, "/api/inbox/9999/approve", draft("AlbumA"))
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // 成功: approved + draft + inbox ジョブ
    let (st, body) = app
        .post(&c, &format!("/api/inbox/{id}/approve"), draft("AlbumA"))
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    let job_id = body["job_id"].as_i64().unwrap();
    assert_eq!(app.state(id).await, ItemState::Approved);
    let (_, body) = app.get(&c, "/api/inbox").await;
    assert_eq!(body["items"][0]["draft"]["album"], "Album");
    assert!(!body["items"][0]["approved_at"].is_null());
    let ty: String = app
        .db
        .read(move |c| {
            Ok(
                c.query_row("SELECT type FROM jobs WHERE id = ?1", [job_id], |r| {
                    r.get(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(ty, "inbox");
    // 409: approved の件をもう一度 approve
    let (st, body) = app
        .post(&c, &format!("/api/inbox/{id}/approve"), draft("AlbumA"))
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["error"], "state");
}

#[tokio::test]
async fn reject_reopen_and_scan() {
    let app = App::new().await;
    let c = app.cookie().await;
    let id = app.item("AlbumA").await;
    let (st, _) = app
        .post(&c, &format!("/api/inbox/{id}/reject"), json!({}))
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(app.state(id).await, ItemState::Rejected);
    // rejected は approve できない
    let (st, _) = app
        .post(&c, &format!("/api/inbox/{id}/approve"), draft("AlbumA"))
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, _) = app
        .post(&c, &format!("/api/inbox/{id}/reopen"), json!({}))
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(app.state(id).await, ItemState::Pending);
    // pending の reopen は 409、failed の approve は通る
    let (st, _) = app
        .post(&c, &format!("/api/inbox/{id}/reopen"), json!({}))
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    app.db
        .write(move |c| inbox::set_state(c, id, ItemState::Failed, Some("x"), 2000))
        .await
        .unwrap();
    let (st, _) = app
        .post(&c, &format!("/api/inbox/{id}/approve"), draft("AlbumA"))
        .await;
    assert_eq!(st, StatusCode::ACCEPTED);
    // approved → reopen で pending に戻る
    let (st, _) = app
        .post(&c, &format!("/api/inbox/{id}/reopen"), json!({}))
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(app.state(id).await, ItemState::Pending);
    // scan: 202、未完了があれば 409
    let (st, body) = app.post(&c, "/api/inbox/scan", json!({})).await;
    assert!(
        st == StatusCode::ACCEPTED || st == StatusCode::CONFLICT,
        "{st} {body}"
    );
    let (st, body) = app.post(&c, "/api/inbox/scan", json!({})).await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "duplicate");
}

#[tokio::test]
async fn inbox_requires_login() {
    let app = App::new().await;
    let r = req(Method::GET, "/api/inbox").body(Body::empty()).unwrap();
    let res = app.router.clone().oneshot(r).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------- 追記先と判定（D-70）

/// Library に album（`_Unsorted/Artist/Album`、#12 まで）を作る
async fn seed_album(app: &App) -> i64 {
    app.db
        .write(|c| {
            c.execute(
                "INSERT INTO albums (id, rel_dir, rel_dir_key, album, albumartist)
                 VALUES (7, '_Unsorted/Artist/Album', '_unsorted/artist/album', 'Album', 'Artist')",
                [],
            )?;
            for (id, n) in [(1, 11), (2, 12)] {
                c.execute(
                    "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec,
                                         lossless, title, artist_display, album, albumartist, seen_at,
                                         album_id, disc_no, track_no)
                     VALUES (?1, ?2, ?2, 0, 0, 0, 'flac', 1, 't', 'a', 'Album', 'Artist', 0, 7, 1, ?3)",
                    rusqlite::params![id, format!("_Unsorted/Artist/Album/{n} t.flac"), n],
                )?;
            }
            Ok(7)
        })
        .await
        .unwrap()
}

/// TRACKNUMBER の無い 1 曲の件と、サイドカー（category と判定）
async fn youtube_item(app: &App, rel_dir: &str, category: Option<&str>, verdict: &str) -> i64 {
    use spindle::import::ytmusic::sidecar::{FileEntry, Sidecar};
    let dir = spindle::domain::relpath::RelPath::parse(rel_dir).unwrap();
    let root = RootDir::open(&app.dir.path().join("Inbox")).unwrap();
    root.create_dir_all(&dir).unwrap();
    Sidecar::upsert(
        &root,
        &dir,
        category,
        "20260901 New [abc].opus",
        FileEntry {
            source: "youtube".into(),
            url: Some("https://www.youtube.com/watch?v=abc".into()),
            channel: Some("CH".into()),
            verdict: verdict.into(),
            message: (verdict != "ok").then(|| "ルールを足してください".to_owned()),
        },
    )
    .unwrap();
    let rel_dir = rel_dir.to_owned();
    app.db
        .write(move |c| {
            let id = inbox::insert_item(
                c,
                &rel_dir,
                &spindle::domain::relpath::canonical_key(&rel_dir),
                1000,
            )?;
            inbox::replace_files(
                c,
                id,
                &[FileRow {
                    rel_path: format!("{rel_dir}/20260901 New [abc].opus"),
                    inode: 1,
                    size: 1,
                    mtime_ns: 0,
                    ctime_ns: 0,
                    codec: "opus".into(),
                    lossless: false,
                    sample_rate: Some(48000),
                    bit_depth: None,
                    channels: Some(2),
                    duration_ms: Some(1000),
                    tags: [
                        ("TITLE", "New"),
                        ("ALBUM", "Album"),
                        ("ALBUMARTIST", "Artist"),
                        ("SOURCE_URL", "https://www.youtube.com/watch?v=abc"),
                    ]
                    .into_iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect(),
                }],
            )?;
            Ok(id)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn list_reports_destination_source_and_numbers_after_the_existing_album() {
    let app = App::new().await;
    let c = app.cookie().await;
    seed_album(&app).await;
    app.db
        .write(|c| {
            c.execute("INSERT INTO categories (name) VALUES ('Rock')", [])?;
            Ok(())
        })
        .await
        .unwrap();
    // category 無し（_Unsorted）で宛先 album 7 に当たる件
    let id = youtube_item(&app, "youtube/Artist/Album", None, "unmatched").await;
    let (st, body) = app.get(&c, "/api/inbox").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let it = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == id)
        .unwrap();
    assert_eq!(it["destination"]["album_id"], 7, "{it}");
    assert_eq!(it["destination"]["album"], "Album");
    assert_eq!(it["destination"]["track_count"], 2);
    assert_eq!(it["destination"]["max_track_no"], 12);
    assert_eq!(
        it["destination"]["album_gain"], false,
        "追記先の属性（D-74）"
    );
    assert!(it["destination"].get("numbers").is_none());
    assert_eq!(it["proposal"]["album_gain"], false);
    // 採番は 13 から
    assert_eq!(it["proposal"]["tracks"][0]["track_no"], 13);
    // 判定はトラック行に付く
    assert_eq!(it["tracks"][0]["source"]["verdict"], "unmatched");
    assert_eq!(it["tracks"][0]["source"]["source"], "youtube");
    assert_eq!(
        it["tracks"][0]["source"]["message"],
        "ルールを足してください"
    );
    assert_eq!(
        it["tracks"][0]["source"]["url"],
        "https://www.youtube.com/watch?v=abc"
    );

    // サイドカーの category は提案に写る（語彙にあるとき）。宛先は変わるので destination は無い
    let id2 = youtube_item(&app, "youtube/Artist/Other", Some("Rock"), "ok").await;
    let (_, body) = app.get(&c, "/api/inbox").await;
    let it = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == id2)
        .unwrap();
    assert_eq!(it["proposal"]["category"], "Rock");
    assert!(it["destination"].is_null());
    assert_eq!(it["proposal"]["tracks"][0]["track_no"], 1);
    assert_eq!(it["tracks"][0]["source"]["verdict"], "ok");
    assert!(it["tracks"][0]["source"]["message"].is_null());
    // 語彙に無い category は提案しない
    let id3 = youtube_item(&app, "youtube/Artist/Third", Some("Nope"), "ok").await;
    let (_, body) = app.get(&c, "/api/inbox").await;
    let it = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == id3)
        .unwrap();
    assert!(it["proposal"]["category"].is_null());
    // 手で置いた件（サイドカー無し）は source が null
    let id4 = app.item("AlbumA").await;
    let (_, body) = app.get(&c, "/api/inbox").await;
    let it = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == id4)
        .unwrap();
    assert!(it["tracks"][0]["source"].is_null());
}

#[tokio::test]
async fn approve_rejects_a_number_taken_in_the_destination_album() {
    let app = App::new().await;
    let c = app.cookie().await;
    seed_album(&app).await;
    let id = youtube_item(&app, "youtube/Artist/Album", None, "ok").await;
    let draft = |n: u32| {
        json!({
            "category": null, "albumartist": "Artist", "album": "Album", "date": null,
            "tracks": [{ "rel_path": "youtube/Artist/Album/20260901 New [abc].opus",
                         "disc_no": 1, "track_no": n, "title": "New", "artist": "" }]
        })
    };
    let (st, body) = app
        .post(&c, &format!("/api/inbox/{id}/approve"), draft(12))
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("track 12"),
        "{body}"
    );
    assert_eq!(app.state(id).await, ItemState::Pending);
    let (st, body) = app
        .post(&c, &format!("/api/inbox/{id}/approve"), draft(13))
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
}

#[tokio::test]
async fn pending_item_with_a_saved_draft_proposes_the_merge() {
    let app = App::new().await;
    let c = app.cookie().await;
    let id = app.item("AlbumA").await;
    let mut d = draft("AlbumA");
    d["album"] = json!("Corrected Album");
    d["tracks"][1]["title"] = json!("Two (fixed)");
    let (st, _) = app.post(&c, &format!("/api/inbox/{id}/approve"), d).await;
    assert_eq!(st, StatusCode::ACCEPTED);
    // 走査が pending に戻し、ファイルが 1 本増えた状況
    app.db
        .write(move |c| {
            inbox::set_state(c, id, ItemState::Pending, Some("ファイルが変わった"), 2)?;
            let mut files = inbox::files(c, id)?;
            let mut extra = files[0].clone();
            extra.rel_path = "AlbumA/03.flac".into();
            extra.tags = vec![("TITLE".to_owned(), "Three".to_owned())];
            files.push(extra);
            inbox::replace_files(c, id, &files)?;
            Ok(())
        })
        .await
        .unwrap();
    let (_, body) = app.get(&c, "/api/inbox").await;
    let it = &body["items"][0];
    assert_eq!(it["state"], "pending");
    assert_eq!(it["proposal"]["album"], "Corrected Album");
    assert_eq!(it["proposal"]["tracks"][1]["title"], "Two (fixed)");
    assert_eq!(it["proposal"]["tracks"][2]["rel_path"], "AlbumA/03.flac");
    assert_eq!(it["proposal"]["tracks"][2]["title"], "Three");
    // 新しいファイルは既存の番号（1, 2）の続き
    assert_eq!(it["proposal"]["tracks"][2]["track_no"], 3);
}

#[tokio::test]
async fn items_with_a_release_id_get_no_destination_and_no_number_check() {
    let app = App::new().await;
    let c = app.cookie().await;
    seed_album(&app).await;
    let id = app
        .db
        .write(|c| {
            let id = inbox::insert_item(c, "cd", "cd", 1000)?;
            inbox::replace_files(
                c,
                id,
                &[FileRow {
                    rel_path: "cd/01.flac".into(),
                    inode: 1,
                    size: 1,
                    mtime_ns: 0,
                    ctime_ns: 0,
                    codec: "flac".into(),
                    lossless: true,
                    sample_rate: Some(44100),
                    bit_depth: Some(16),
                    channels: Some(2),
                    duration_ms: Some(1000),
                    tags: [
                        ("TITLE", "One"),
                        ("ALBUM", "Album"),
                        ("ALBUMARTIST", "Artist"),
                        ("TRACKNUMBER", "12"),
                        ("MUSICBRAINZ_ALBUMID", "mbid-1"),
                    ]
                    .into_iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect(),
                }],
            )?;
            Ok(id)
        })
        .await
        .unwrap();
    let (_, body) = app.get(&c, "/api/inbox").await;
    let it = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == id)
        .unwrap();
    assert!(it["destination"].is_null(), "{it}");
    // #12 は宛先の非 MB album と重なるが、別リリースなので通る
    let (st, body) = app
        .post(
            &c,
            &format!("/api/inbox/{id}/approve"),
            json!({
                "category": null, "albumartist": "Artist", "album": "Album", "date": null,
                "tracks": [{ "rel_path": "cd/01.flac", "disc_no": 1, "track_no": 12, "title": "One", "artist": "" }]
            }),
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
}

// ---------------------------------------------------------------- 忠実表示（P4-4、D-70）

/// 提案の artist は ARTIST の全値を "; " で結合し、多値なら keep_artists = true
#[tokio::test]
async fn proposal_joins_multi_valued_artist_and_sets_keep_artists() {
    let app = App::new().await;
    let c = app.cookie().await;
    app.item_with(
        "AlbumM",
        vec![
            (
                "01.flac",
                vec![
                    ("TITLE", "One"),
                    ("ALBUM", "Album"),
                    ("ALBUMARTIST", "Artist"),
                    ("TRACKNUMBER", "1"),
                    ("ARTIST", "花譜"),
                    ("ARTIST", "理芽"),
                ],
            ),
            (
                "02.flac",
                vec![
                    ("TITLE", "Two"),
                    ("ALBUM", "Album"),
                    ("TRACKNUMBER", "2"),
                    ("ARTIST", "花譜"),
                ],
            ),
        ],
    )
    .await;
    let (st, body) = app.get(&c, "/api/inbox").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let tracks = &body["items"][0]["proposal"]["tracks"];
    assert_eq!(tracks[0]["artist"], "花譜; 理芽");
    assert_eq!(tracks[0]["keep_artists"], true);
    assert_eq!(tracks[1]["artist"], "花譜");
    assert_eq!(tracks[1]["keep_artists"], false);
    // keep_artists は承認の入力でも受ける（省略は旧下書き = null）
    let (st, body) = app
        .post(
            &c,
            "/api/inbox/1/approve",
            json!({
                "category": null, "albumartist": "Artist", "album": "Album", "date": null,
                "tracks": [
                    { "rel_path": "AlbumM/01.flac", "disc_no": 1, "track_no": 1, "title": "One", "artist": "花譜; 理芽", "keep_artists": true },
                    { "rel_path": "AlbumM/02.flac", "disc_no": 1, "track_no": 2, "title": "Two", "artist": "花譜" }
                ]
            }),
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    let (_, body) = app.get(&c, "/api/inbox").await;
    let saved = &body["items"][0]["draft"]["tracks"];
    assert_eq!(saved[0]["keep_artists"], true);
    assert!(saved[1]["keep_artists"].is_null());
}

/// 1x1 の JPEG（COM で内容を変える）
fn jpeg(tag: &[u8]) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8];
    v.extend_from_slice(&[
        0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01, 0x01, 0x01, 0x11, 0x00,
    ]);
    let len = (tag.len() + 2) as u16;
    v.extend_from_slice(&[0xFF, 0xFE]);
    v.extend_from_slice(&len.to_be_bytes());
    v.extend_from_slice(tag);
    v.extend_from_slice(&[0xFF, 0xD9]);
    v
}

fn set_picture(path: &std::path::Path, bytes: Vec<u8>) {
    use lofty::picture::{MimeType, Picture, PictureType};
    let pic = Picture::unchecked(bytes)
        .pic_type(PictureType::CoverFront)
        .mime_type(MimeType::Jpeg)
        .build();
    common::retag(path, |t| {
        while !t.pictures().is_empty() {
            t.remove_picture(0);
        }
        t.push_picture(pic);
    });
}

fn hex_of(bytes: &[u8]) -> String {
    spindle::media::artwork::ArtworkStore::hex(&spindle::media::artwork::ArtworkStore::hash_of(
        bytes,
    ))
}

/// `GET /api/inbox/:id/artwork/:hash`: 実体を照合して原寸を返す（MIME は sniff、immutable + ETag）。
/// 304 は照合の後。件に無い hash・他の件・実体の消失・差し替えは 404、未ログインは 401
#[tokio::test]
async fn artwork_returns_the_embedded_picture_after_verifying_the_file() {
    let app = App::new().await;
    let c = app.cookie().await;
    std::fs::create_dir_all(app.inbox_path("AlbumP")).unwrap();
    let p1 = require_ffmpeg!(common::make_audio(
        &app.inbox_path("AlbumP"),
        "01.flac",
        "flac",
        1
    ));
    let p2 = common::make_audio(&app.inbox_path("AlbumP"), "02.flac", "flac", 2).unwrap();
    let cover = jpeg(b"cover");
    let hash = hex_of(&cover);
    set_picture(&p1, cover.clone());
    set_picture(&p2, cover.clone());
    let picture = format!("image/png:{hash}"); // タグの MIME は信用しない（sniff で jpeg になる）
    let id = app
        .item_with(
            "AlbumP",
            vec![
                (
                    "01.flac",
                    vec![("TITLE", "One"), ("PICTURE", picture.as_str())],
                ),
                (
                    "02.flac",
                    vec![("TITLE", "Two"), ("PICTURE", picture.as_str())],
                ),
            ],
        )
        .await;
    let other = app
        .item_with("Other", vec![("01.flac", vec![("TITLE", "X")])])
        .await;
    let uri = format!("/api/inbox/{id}/artwork/{hash}");
    let etag = format!("\"{hash}-orig\"");

    // 未ログイン
    let res = app.raw(None, &uri, None).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    let res = app.raw(Some(&c), &uri, None).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(res.headers()[header::ETAG], etag.as_str());
    assert_eq!(
        res.headers()[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), cover.as_slice());

    // 304
    let res = app.raw(Some(&c), &uri, Some(&etag)).await;
    assert_eq!(res.status(), StatusCode::NOT_MODIFIED);

    // 大文字の hash も同じ画像
    let res = app
        .raw(
            Some(&c),
            &format!("/api/inbox/{id}/artwork/{}", hash.to_uppercase()),
            None,
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);

    // 件に無い hash、他の件、不正な hash
    let res = app
        .raw(
            Some(&c),
            &format!("/api/inbox/{id}/artwork/{}", hex_of(b"nope")),
            None,
        )
        .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = app
        .raw(
            Some(&c),
            &format!("/api/inbox/{other}/artwork/{hash}"),
            None,
        )
        .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = app
        .raw(Some(&c), &format!("/api/inbox/{id}/artwork/zz"), None)
        .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // 1 本目の画像が差し替わっても 2 本目の実体で返る。両方消えれば If-None-Match が一致しても 404
    set_picture(&p1, jpeg(b"replaced"));
    let res = app.raw(Some(&c), &uri, None).await;
    assert_eq!(res.status(), StatusCode::OK);
    // 読めない（権限）は消失ではなく I/O の失敗 → 500（root では作れないので飛ばす）
    if !is_root() {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p2, std::fs::Permissions::from_mode(0o000)).unwrap();
        let res = app.raw(Some(&c), &uri, None).await;
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        std::fs::set_permissions(&p2, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    std::fs::remove_file(&p2).unwrap();
    let res = app.raw(Some(&c), &uri, Some(&etag)).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = app.raw(Some(&c), &uri, None).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

fn is_root() -> bool {
    rustix::process::geteuid().is_root()
}

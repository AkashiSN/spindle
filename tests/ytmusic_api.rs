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

/// 偽の yt-dlp: `dump/<list_id>.json` を返す。無ければ yt-dlp と同じ形のエラーで失敗する
const FAKE_YTDLP: &str = r#"
FAKE="$1"; shift
url="${@: -1}"
list="${url##*list=}"
if [ -f "$FAKE/dump/$list.json" ]; then cat "$FAKE/dump/$list.json"; exit 0; fi
echo "ERROR: [youtube:tab] $list: The playlist does not exist." >&2; exit 1
"#;

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
        // 偽の yt-dlp（`fake/dump/<list_id>.json` を返す）
        let fake = dir.path().join("fake");
        std::fs::create_dir_all(fake.join("dump")).unwrap();
        std::fs::write(fake.join("ytdlp.sh"), FAKE_YTDLP).unwrap();
        root.get_mut("bin")
            .and_then(toml::Value::as_table_mut)
            .unwrap()
            .insert("ytdlp".into(), "/bin/bash".into());
        if let Some(yt) = root.get_mut("ytmusic").and_then(toml::Value::as_table_mut) {
            yt.insert(
                "ytdlp_args".into(),
                toml::Value::Array(vec![
                    fake.join("ytdlp.sh").display().to_string().into(),
                    fake.display().to_string().into(),
                ]),
            );
        }
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
        self.post_to("/api/ytmusic/download", c, body).await
    }

    async fn post_to(&self, path: &str, c: Option<&str>, body: Value) -> (StatusCode, Value) {
        let mut r = req(Method::POST, path)
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
        json!({ "urls": ["https://"] }),
        json!({ "urls": ["https://youtu.be/a b"] }),
        json!({ "urls": ["javascript:alert(1)"] }),
        json!({ "urls": ["//youtu.be/a"] }),
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

/// Library の active なトラック 1（SOURCE_URL = v1）と Inbox のファイル（SOURCE_URL = v2）
async fn seed_sources(app: &App) {
    app.db
        .write(|c| {
            c.execute_batch(
                r#"INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                                      title, artist_display, album, albumartist, seen_at)
                   VALUES (1, 'Pop/A/B/01 One.opus', 'pop/a/b/01 one.opus', 0, 0, 0, 'opus', 0, 't', 'a', 'al', 'aa', 0);
                   INSERT INTO track_tags (track_id, key, idx, value)
                   VALUES (1, 'SOURCE_URL', 0, 'https://www.youtube.com/watch?v=v1');
                   INSERT INTO inbox_items (id, rel_dir, rel_dir_key, detected_at, seen_at) VALUES (1, 'youtube/x', 'youtube/x', 0, 0);
                   INSERT INTO inbox_files (item_id, rel_path, rel_path_key, inode, size, mtime_ns, ctime_ns, codec, lossless, tags)
                   VALUES (1, 'youtube/x/a.opus', 'youtube/x/a.opus', 1, 1, 0, 0, 'opus', 0,
                           '[["TITLE","t"],["SOURCE_URL","https://www.youtube.com/watch?v=v2"]]');"#,
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn lookup_reports_kind_and_where_each_url_already_is() {
    let app = App::new(true).await;
    let c = app.cookie().await;
    seed_sources(&app).await;
    let (st, body) = app
        .post_to(
            "/api/ytmusic/subscriptions",
            Some(&c),
            json!({ "url": "https://music.youtube.com/playlist?list=PLsub", "albumartist": "AA", "album": "AL" }),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let (st, body) = app
        .post_to(
            "/api/ytmusic/lookup",
            Some(&c),
            json!({ "urls": [
                "https://youtu.be/v1",
                " https://music.youtube.com/watch?v=v2&feature=share ",
                "https://www.youtube.com/watch?v=v3",
                "https://www.youtube.com/playlist?list=PLsub",
                "https://www.youtube.com/watch?v=v1&list=PLother",
                "https://example.com/video/1",
                "not a url",
            ] }),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 7);
    assert_eq!(items[0]["kind"], "video");
    assert_eq!(items[0]["video_url"], "https://www.youtube.com/watch?v=v1");
    assert_eq!(items[0]["located"]["location"], "library");
    assert_eq!(items[0]["located"]["path"], "Pop/A/B/01 One.opus");
    assert_eq!(
        items[1]["url"], "https://music.youtube.com/watch?v=v2&feature=share",
        "前後の空白は落とす"
    );
    assert_eq!(items[1]["located"]["location"], "inbox");
    assert_eq!(items[2]["kind"], "video");
    assert!(items[2].get("located").is_none(), "{}", items[2]);
    assert_eq!(items[3]["kind"], "playlist");
    assert_eq!(items[3]["list_id"], "PLsub");
    assert_eq!(items[3]["subscription"]["album"], "AL");
    assert_eq!(
        items[4]["kind"], "playlist",
        "list= 付きの動画 URL は再生リスト"
    );
    assert!(items[4].get("subscription").is_none());
    assert_eq!(items[5]["kind"], "other");
    assert_eq!(items[6]["kind"], "invalid");

    // 上限・無効・未ログイン
    let many: Vec<String> = (0..201).map(|i| format!("https://youtu.be/v{i}")).collect();
    let (st, _) = app
        .post_to("/api/ytmusic/lookup", Some(&c), json!({ "urls": many }))
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = app
        .post_to("/api/ytmusic/lookup", None, json!({ "urls": [] }))
        .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let off = App::new(false).await;
    let c2 = off.cookie().await;
    let (st, _) = off
        .post_to("/api/ytmusic/lookup", Some(&c2), json!({ "urls": [] }))
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn playlist_counts_entries_by_where_they_already_are() {
    let app = App::new(true).await;
    let c = app.cookie().await;
    seed_sources(&app).await;
    std::fs::write(
        app._dir.path().join("fake/dump/PLx.json"),
        json!({
            "_type": "playlist",
            "title": "My List",
            "playlist_count": 4,
            "entries": [
                { "id": "v1", "title": "One" },
                { "id": "v2", "title": "Two" },
                { "id": "gone", "title": "[Private video]" },
                { "id": "v4", "title": "Four" },
            ]
        })
        .to_string(),
    )
    .unwrap();
    let (st, body) = app
        .post_to(
            "/api/ytmusic/playlist",
            Some(&c),
            json!({ "url": "https://music.youtube.com/playlist?list=PLx" }),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["list_id"], "PLx");
    assert_eq!(body["title"], "My List");
    assert_eq!(body["entries"], 4);
    assert_eq!(body["unavailable"], 1);
    assert_eq!(body["in_library"], 1);
    assert_eq!(body["in_inbox"], 1);
    assert_eq!(body["new"], 1);
    assert_eq!(body["truncated"], false);
    assert!(body.get("subscription").is_none());

    // yt-dlp の失敗は 502 で最後の行を伝える
    let (st, body) = app
        .post_to(
            "/api/ytmusic/playlist",
            Some(&c),
            json!({ "url": "https://www.youtube.com/playlist?list=PLmissing" }),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_GATEWAY, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains("does not exist"),
        "{body}"
    );
    // 再生リストでない URL は yt-dlp を呼ばずに 400
    let (st, _) = app
        .post_to(
            "/api/ytmusic/playlist",
            Some(&c),
            json!({ "url": "https://www.youtube.com/watch?v=v1" }),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

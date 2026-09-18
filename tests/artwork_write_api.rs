//! `POST /api/artwork/upload` と `POST /api/artwork/embed`（SPEC §9、P1-3 書き側、D-60）。
//! アップロードは画像をキャッシュと `artwork` 行に置き、embed は selection の埋め込み画像を
//! その 1 枚に差し替える編集バッチを記録する。合成ファイルは ffmpeg で作る（無ければ skip）

#![cfg(target_os = "linux")]

mod common;

use std::fs::File;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use rusqlite::Connection;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::Db;
use spindle::domain::tags::read_transfer_tags;
use spindle::edit::Editor;
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, Scanner};
use spindle::jobs::handlers::tagwrite::TagwriteHandler;
use spindle::jobs::{JobType, Registry};
use spindle::media::artwork::ArtworkStore;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

/// 1x1 の JPEG。`tag` で内容を変えられる
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct App {
    state: AppState,
    router: Router,
    dir: tempfile::TempDir,
    scanner: Scanner,
    editor: Arc<Editor>,
    store: Arc<ArtworkStore>,
    shutdown: CancellationToken,
}

impl App {
    async fn new() -> Self {
        Self::build(true).await
    }

    async fn build(with_store: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Library");
        std::fs::create_dir(&lib).unwrap();
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let config = Arc::new(Config::parse(EXAMPLE).unwrap());
        let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
            .await
            .unwrap();
        let root = Arc::new(RootDir::open(&lib).unwrap());
        let store = Arc::new(ArtworkStore::new(dir.path().join("thumbs")));
        let scanner = Scanner::new(db.clone(), root.clone(), 2).with_artwork(store.clone());
        let mut state = AppState::new(config, db.clone(), mode);
        let mut editor = Editor::new(db, root, state.jobs.clone());
        if with_store {
            state = state.with_artwork(store.clone());
            editor = editor.with_artwork(store.clone());
        }
        let editor = Arc::new(editor);
        let state = state.with_editor(editor.clone());
        Self {
            router: api::router(state.clone()),
            state,
            dir,
            scanner,
            editor,
            store,
            shutdown: CancellationToken::new(),
        }
    }

    fn start_worker(&self) {
        let mut reg = Registry::new();
        reg.register(
            JobType::Tagwrite,
            Arc::new(TagwriteHandler::new(self.editor.clone())),
        );
        self.state.jobs.start(reg, self.shutdown.clone());
    }

    fn lib(&self) -> PathBuf {
        self.dir.path().join("Library")
    }

    fn add(&self, rel: &str, picture: Option<&[u8]>) -> Option<PathBuf> {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        let ext = rel.rsplit_once('.').unwrap().1;
        common::make_audio(p.parent().unwrap(), &name, ext, 3)?;
        common::set_basic_tags(&p, "t", "Artist", "Album", "AlbumArtist", 1, 1);
        if let Some(bytes) = picture {
            use lofty::picture::{MimeType, Picture, PictureType};
            let pic = Picture::unchecked(bytes.to_vec())
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Jpeg)
                .build();
            common::retag(&p, |t| t.push_picture(pic));
        }
        Some(p)
    }

    async fn scan(&self) {
        self.scanner
            .run(
                ScanKind::Incremental,
                Arc::new(|_, _, _| {}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }

    fn conn(&self) -> Connection {
        Connection::open(self.dir.path().join("spindle.db")).unwrap()
    }

    fn track_id(&self, rel: &str) -> i64 {
        self.conn()
            .query_row("SELECT id FROM tracks WHERE rel_path = ?1", [rel], |r| {
                r.get(0)
            })
            .unwrap()
    }

    fn batch_state(&self, id: i64) -> String {
        self.conn()
            .query_row("SELECT state FROM edit_batches WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
    }

    async fn wait_batch_terminal(&self, id: i64) -> String {
        for _ in 0..1000 {
            let st = self.batch_state(id);
            if st != "prepared" && st != "applying" {
                return st;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("batch {id} が終端にならない");
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

    async fn upload(
        &self,
        c: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> (StatusCode, serde_json::Value) {
        let r = req(Method::POST, "/api/artwork/upload")
            .header(header::COOKIE, c)
            .header(header::CONTENT_TYPE, content_type)
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(bytes))
            .unwrap();
        let res = self.router.clone().oneshot(r).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn post(
        &self,
        c: &str,
        path: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let r = req(Method::POST, path)
            .header(header::COOKIE, c)
            .header(header::CONTENT_TYPE, "application/json")
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = self.router.clone().oneshot(r).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn get_raw(&self, c: &str, path: &str) -> (StatusCode, Vec<u8>) {
        let r = req(Method::GET, path)
            .header(header::COOKIE, c)
            .body(Body::empty())
            .unwrap();
        let res = self.router.clone().oneshot(r).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, bytes.to_vec())
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn req(method: Method, uri: &str) -> axum::http::request::Builder {
    let peer: SocketAddr = LAN.parse().unwrap();
    let mut b = Request::builder().method(method).uri(uri);
    b.extensions_mut().unwrap().insert(ConnectInfo(peer));
    b
}

fn file_pictures(path: &Path) -> Vec<Vec<u8>> {
    let ext = path.extension().and_then(|e| e.to_str());
    let t = read_transfer_tags(File::open(path).unwrap(), ext).unwrap();
    t.pictures.into_iter().map(|p| p.data().to_vec()).collect()
}

// ---------------------------------------------------------------- upload

#[tokio::test]
async fn upload_stores_the_image_registers_a_row_and_enqueues_a_thumbnail() {
    let app = App::new().await;
    let c = app.cookie().await;
    let img = jpeg(b"uploaded");
    let hash = ArtworkStore::hash_of(&img);
    // Content-Type は信用せずヘッダで判別する
    let (st, body) = app
        .upload(&c, "application/octet-stream", img.clone())
        .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["sha256"], hex(&hash));
    assert_eq!(body["mime"], "image/jpeg");
    assert_eq!(body["width"], 1);
    assert_eq!(body["height"], 1);
    assert_eq!(body["bytes"], img.len());
    assert!(app.store.has_original(&hash, "image/jpeg"));
    let (mime, origin): (String, String) = app
        .conn()
        .query_row(
            "SELECT mime, origin FROM artwork WHERE sha256 = ?1",
            [hash.to_vec()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((mime.as_str(), origin.as_str()), ("image/jpeg", "embedded"));
    let jobs: i64 = app
        .conn()
        .query_row(
            "SELECT count(*) FROM jobs WHERE type = 'thumbnail'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(jobs, 1);
    // 原画像として配信できる（プレビュー）
    let (st, got) = app
        .get_raw(&c, &format!("/api/artwork/{}", hex(&hash)))
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(got, img);

    // 同じ画像をもう一度上げても同じ応答（行も実体も 1 つ）。古い未参照の dir でも、上げ直した
    // 時点から GC 区分 E の 24 時間の猶予が数え直される（dir の mtime が今になる）
    let entry = app.store.dir().join(hex(&hash));
    let old = std::time::SystemTime::now() - Duration::from_secs(5 * 24 * 3600);
    File::open(&entry).unwrap().set_modified(old).unwrap();
    let (st, again) = app.upload(&c, "image/jpeg", img.clone()).await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(again["sha256"], body["sha256"]);
    let mtime = std::fs::metadata(&entry).unwrap().modified().unwrap();
    assert!(mtime.duration_since(old).unwrap() > Duration::from_secs(4 * 24 * 3600));
    let rows: i64 = app
        .conn()
        .query_row("SELECT count(*) FROM artwork", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn upload_rejects_non_images_and_unsupported_formats() {
    let app = App::new().await;
    let c = app.cookie().await;
    let (st, body) = app
        .upload(&c, "image/jpeg", b"not an image at all".to_vec())
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "unsupported_image");
    // GIF はヘッダで読めるが埋め込みには使わない
    let gif = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02\x44\x01\x00\x3b".to_vec();
    let (st, body) = app.upload(&c, "image/gif", gif).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "unsupported_image");
    let (st, _) = app.upload(&c, "image/jpeg", Vec::new()).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let rows: i64 = app
        .conn()
        .query_row("SELECT count(*) FROM artwork", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn upload_larger_than_the_limit_is_rejected() {
    let app = App::new().await;
    let c = app.cookie().await;
    let mut big = jpeg(b"big");
    big.resize(32 * 1024 * 1024 + 1, 0);
    let (st, _) = app.upload(&c, "image/jpeg", big).await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn upload_without_artwork_store_is_503() {
    let app = App::build(false).await;
    let c = app.cookie().await;
    let (st, body) = app.upload(&c, "image/jpeg", jpeg(b"x")).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "artwork_unavailable");
    let (st, body) = app
        .post(
            &c,
            "/api/artwork/embed",
            serde_json::json!({ "selection": { "ids": [1] }, "sha256": "0".repeat(64) }),
        )
        .await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "artwork_unavailable");
}

// ---------------------------------------------------------------- embed

#[tokio::test]
async fn embed_records_a_batch_and_the_worker_writes_the_picture() {
    let app = App::new().await;
    let old = jpeg(b"old");
    let p1 = require_ffmpeg!(app.add("A/1.flac", Some(&old)));
    let p2 = app.add("A/2.flac", None).unwrap();
    app.scan().await;
    let ids = [app.track_id("A/1.flac"), app.track_id("A/2.flac")];
    let c = app.cookie().await;
    let new = jpeg(b"new");
    let (st, up) = app.upload(&c, "image/jpeg", new.clone()).await;
    assert_eq!(st, StatusCode::CREATED);
    app.start_worker();

    let (st, body) = app
        .post(
            &c,
            "/api/artwork/embed",
            serde_json::json!({
                "selection": { "ids": ids },
                "sha256": up["sha256"],
                "description": "ジャケット差し替え",
            }),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["affected"], 2);
    assert_eq!(body["unchanged"], 0);
    assert_eq!(body["pending_excluded"], 0);
    let batch_id = body["batch_id"].as_i64().unwrap();
    assert_eq!(app.wait_batch_terminal(batch_id).await, "applied");
    assert_eq!(file_pictures(&p1), vec![new.clone()]);
    assert_eq!(file_pictures(&p2), vec![new.clone()]);
    let desc: String = app
        .conn()
        .query_row(
            "SELECT description FROM edit_batches WHERE id = ?1",
            [batch_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(desc, "ジャケット差し替え");

    // 全件が既にその画像なら no_changes
    let (st, body) = app
        .post(
            &c,
            "/api/artwork/embed",
            serde_json::json!({ "selection": { "ids": ids }, "sha256": up["sha256"] }),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["error"], "no_changes");
}

#[tokio::test]
async fn embed_with_unknown_hash_is_404_and_bad_hash_is_400() {
    let app = App::new().await;
    require_ffmpeg!(app.add("A/1.flac", None));
    app.scan().await;
    let id = app.track_id("A/1.flac");
    let c = app.cookie().await;
    let (st, body) = app
        .post(
            &c,
            "/api/artwork/embed",
            serde_json::json!({ "selection": { "ids": [id] }, "sha256": "7".repeat(64) }),
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "artwork_not_found");
    let (st, body) = app
        .post(
            &c,
            "/api/artwork/embed",
            serde_json::json!({ "selection": { "ids": [id] }, "sha256": "zz" }),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn embed_refuses_pending_tracks_unless_skipped() {
    let app = App::new().await;
    require_ffmpeg!(app.add("A/1.flac", None));
    app.add("A/2.flac", None).unwrap();
    app.scan().await;
    let id1 = app.track_id("A/1.flac");
    let id2 = app.track_id("A/2.flac");
    let c = app.cookie().await;
    let (_, up) = app.upload(&c, "image/jpeg", jpeg(b"new")).await;
    // ワーカーを起こさないので 1 件目のバッチは pending のまま
    let (st, first) = app
        .post(
            &c,
            "/api/artwork/embed",
            serde_json::json!({ "selection": { "ids": [id1] }, "sha256": up["sha256"] }),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{first}");

    let (st, body) = app
        .post(
            &c,
            "/api/artwork/embed",
            serde_json::json!({ "selection": { "ids": [id1, id2] }, "sha256": up["sha256"] }),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["error"], "pending");
    assert_eq!(body["track_ids"], serde_json::json!([id1]));

    let (st, body) = app
        .post(
            &c,
            "/api/artwork/embed",
            serde_json::json!({
                "selection": { "ids": [id1, id2] },
                "sha256": up["sha256"],
                "skip_pending": true,
            }),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["affected"], 1);
    assert_eq!(body["pending_excluded"], 1);
}

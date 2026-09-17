//! `GET /api/artwork/:hash?size=` と `GET /api/albums` の `artwork_hash`（SPEC §9、
//! docs/TASKS.md P1-3、D-49）。合成ファイルは ffmpeg で作る（無ければ skip）

#![cfg(target_os = "linux")]

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::Db;
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, Scanner};
use spindle::media::artwork::ArtworkStore;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

/// 1x1 の JPEG（ヘッダのみ）
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

struct App {
    router: Router,
    dir: tempfile::TempDir,
    scanner: Scanner,
    store: Arc<ArtworkStore>,
}

impl App {
    async fn new() -> Self {
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
        let scanner = Scanner::new(db.clone(), root, 2).with_artwork(store.clone());
        let state = AppState::new(config, db, mode).with_artwork(store.clone());
        Self {
            router: api::router(state),
            dir,
            scanner,
            store,
        }
    }

    fn add(&self, rel: &str, cover: Option<&[u8]>) {
        let p = self.dir.path().join("Library").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        common::make_audio(p.parent().unwrap(), &name, "flac", 1).unwrap();
        common::set_basic_tags(&p, "t", "Artist", "Album", "Artist", 1, 1);
        if let Some(bytes) = cover {
            std::fs::write(p.parent().unwrap().join("cover.jpg"), bytes).unwrap();
        }
    }

    async fn scan(&self) {
        self.scanner
            .run(
                ScanKind::Incremental,
                Arc::new(|_, _| {}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
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

    async fn get(
        &self,
        c: Option<&str>,
        path: &str,
        extra: &[(&str, &str)],
    ) -> axum::response::Response {
        let mut r = req(Method::GET, path);
        if let Some(c) = c {
            r = r.header(header::COOKIE, c);
        }
        for (k, v) in extra {
            r = r.header(*k, *v);
        }
        self.router
            .clone()
            .oneshot(r.body(Body::empty()).unwrap())
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

async fn body_of(res: axum::response::Response) -> Vec<u8> {
    res.into_body().collect().await.unwrap().to_bytes().to_vec()
}

#[tokio::test]
async fn albums_expose_hash_and_artwork_serves_original_with_immutable_cache() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let app = App::new().await;
    let cover = jpeg(b"cover");
    app.add("A/01.flac", Some(&cover));
    app.add("B/01.flac", None);
    app.scan().await;
    let c = app.cookie().await;

    let res = app.get(Some(&c), "/api/albums", &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body_of(res).await).unwrap();
    let items = json["items"].as_array().unwrap();
    let a = items.iter().find(|i| i["rel_dir"] == "A").unwrap();
    let b = items.iter().find(|i| i["rel_dir"] == "B").unwrap();
    let hash = a["artwork_hash"].as_str().unwrap().to_owned();
    assert_eq!(hash, ArtworkStore::hex(&ArtworkStore::hash_of(&cover)));
    assert!(a["artwork_id"].is_number());
    assert!(b["artwork_hash"].is_null());
    assert!(b["artwork_id"].is_null());

    // 原画像（size 無し）
    let res = app
        .get(Some(&c), &format!("/api/artwork/{hash}"), &[])
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(
        res.headers()[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    let etag = res.headers()[header::ETAG].to_str().unwrap().to_owned();
    assert_eq!(body_of(res).await, cover);

    // If-None-Match → 304
    let res = app
        .get(
            Some(&c),
            &format!("/api/artwork/{hash}"),
            &[("if-none-match", etag.as_str())],
        )
        .await;
    assert_eq!(res.status(), StatusCode::NOT_MODIFIED);

    // サムネイル未生成 → 原画像へ倒す。後で置き換わるので immutable にはしない
    let res = app
        .get(Some(&c), &format!("/api/artwork/{hash}?size=256"), &[])
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(res.headers()[header::ETAG], etag.as_str());
    assert_eq!(res.headers()[header::CACHE_CONTROL], "public, no-cache");

    // サムネイルがあればそれ（WebP）
    let sha = ArtworkStore::hash_of(&cover);
    let thumb = app.store.thumb_path(&sha, 256);
    std::fs::write(&thumb, b"RIFF\0\0\0\0WEBPVP8 ").unwrap();
    let res = app
        .get(Some(&c), &format!("/api/artwork/{hash}?size=256"), &[])
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "image/webp");
    assert_eq!(
        res.headers()[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    assert_ne!(res.headers()[header::ETAG], etag.as_str());
    assert_eq!(&body_of(res).await[..4], b"RIFF");

    // 不正な size / 未知のハッシュ / 不正なハッシュ
    let res = app
        .get(Some(&c), &format!("/api/artwork/{hash}?size=100"), &[])
        .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = app
        .get(Some(&c), &format!("/api/artwork/{}", "0".repeat(64)), &[])
        .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = app.get(Some(&c), "/api/artwork/../etc/passwd", &[]).await;
    assert!(
        res.status() == StatusCode::NOT_FOUND || res.status() == StatusCode::BAD_REQUEST,
        "{}",
        res.status()
    );
    // セッション無し（trusted_cidrs ではない）は 401
    let res = app.get(None, &format!("/api/artwork/{hash}"), &[]).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

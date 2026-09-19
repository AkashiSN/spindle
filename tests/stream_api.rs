//! `GET /api/stream/:id`（SPEC §11、docs/TASKS.md P1-9、D-52）。原本と Derived の Range 直送、
//! Derived が無いときの ffmpeg によるオンザフライ変換。ffmpeg が無ければ skip

#![cfg(target_os = "linux")]

mod common;

use std::fs::File;
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
use spindle::db::{derived, Db};
use spindle::domain::tags::read_audio_file;
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, Scanner};

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

struct App {
    router: Router,
    dir: tempfile::TempDir,
    scanner: Scanner,
    db_path: std::path::PathBuf,
}

impl App {
    async fn new() -> Self {
        Self::with_config(EXAMPLE).await
    }

    async fn with_config(toml: &str) -> Self {
        Self::build(toml, None).await
    }

    /// 変換の猶予を短くして起動する（deadline のテスト）
    async fn with_grace(toml: &str, grace: std::time::Duration) -> Self {
        Self::build(toml, Some(grace)).await
    }

    async fn build(toml: &str, grace: Option<std::time::Duration>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Library");
        let der = dir.path().join("Derived");
        std::fs::create_dir(&lib).unwrap();
        std::fs::create_dir(&der).unwrap();
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let config = Arc::new(Config::parse(toml).unwrap());
        let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
            .await
            .unwrap();
        let library = Arc::new(RootDir::open(&lib).unwrap());
        let derived_root = Arc::new(RootDir::open(&der).unwrap());
        let scanner = Scanner::new(db.clone(), library.clone(), 2);
        let mut state = AppState::new(config, db, mode).with_roots(library, derived_root);
        if let Some(g) = grace {
            state = state.with_transcode_grace(g);
        }
        Self {
            router: api::router(state),
            dir,
            scanner,
            db_path,
        }
    }

    fn lib(&self) -> std::path::PathBuf {
        self.dir.path().join("Library")
    }

    fn derived(&self) -> std::path::PathBuf {
        self.dir.path().join("Derived")
    }

    fn add(&self, rel: &str, ext: &str, seed: u32) -> std::path::PathBuf {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        let made = common::make_audio(p.parent().unwrap(), &name, ext, seed).unwrap();
        common::set_basic_tags(&made, "t", "Artist", "Album", "Artist", 1, 1);
        made
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

    fn conn(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.db_path).unwrap()
    }

    fn track_id(&self, rel: &str) -> i64 {
        self.conn()
            .query_row("SELECT id FROM tracks WHERE rel_path = ?1", [rel], |r| {
                r.get(0)
            })
            .unwrap()
    }

    /// Derived の Opus を置いて derived_files を揃える（transcode ジョブは使わない）
    fn add_derived(&self, id: i64, rel_opus: &str, seed: u32) -> std::path::PathBuf {
        let p = self.derived().join(rel_opus);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        let made = common::make_audio(p.parent().unwrap(), &name, "opus", seed).unwrap();
        let (av, tv): (i64, i64) = self
            .conn()
            .query_row(
                "SELECT audio_version, tag_version FROM tracks WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        derived::upsert(
            &self.conn(),
            id,
            rel_opus,
            "opus",
            Some(128),
            av,
            derived::TagState {
                src_tag_version: tv,
                src_artwork_id: None,
                src_rg_scanned_at: None,
            },
            0,
        )
        .unwrap();
        made
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

    async fn send(
        &self,
        method: Method,
        c: Option<&str>,
        path: &str,
        extra: &[(&str, &str)],
    ) -> axum::response::Response {
        let mut r = req(method, path);
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

    async fn get(
        &self,
        c: Option<&str>,
        path: &str,
        extra: &[(&str, &str)],
    ) -> axum::response::Response {
        self.send(Method::GET, c, path, extra).await
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

fn hdr<'a>(res: &'a axum::response::Response, name: &str) -> Option<&'a str> {
    res.headers().get(name).and_then(|v| v.to_str().ok())
}

macro_rules! require_ffmpeg {
    () => {
        if common::ffmpeg().is_none() {
            eprintln!("ffmpeg が無いので skip");
            return;
        }
    };
}

// ---------------------------------------------------------------- 原本の直送

#[tokio::test]
async fn serves_original_with_range_head_and_etag() {
    require_ffmpeg!();
    let app = App::new().await;
    let p = app.add("A/01.flac", "flac", 1);
    app.scan().await;
    let id = app.track_id("A/01.flac");
    let bytes = std::fs::read(&p).unwrap();
    let c = app.cookie().await;
    let uri = format!("/api/stream/{id}");

    let res = app.get(Some(&c), &uri, &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(hdr(&res, "content-type"), Some("audio/flac"));
    assert_eq!(hdr(&res, "accept-ranges"), Some("bytes"));
    assert_eq!(
        hdr(&res, "content-length"),
        Some(bytes.len().to_string().as_str())
    );
    let etag = hdr(&res, "etag").unwrap().to_owned();
    assert!(etag.starts_with('"'));
    assert_eq!(body_of(res).await, bytes);

    // Range
    let res = app.get(Some(&c), &uri, &[("range", "bytes=0-99")]).await;
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        hdr(&res, "content-range"),
        Some(format!("bytes 0-99/{}", bytes.len()).as_str())
    );
    assert_eq!(hdr(&res, "content-length"), Some("100"));
    assert_eq!(body_of(res).await, &bytes[..100]);
    let res = app.get(Some(&c), &uri, &[("range", "bytes=100-")]).await;
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(body_of(res).await, &bytes[100..]);
    let res = app.get(Some(&c), &uri, &[("range", "bytes=-50")]).await;
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(body_of(res).await, &bytes[bytes.len() - 50..]);
    // 範囲外 → 416
    let res = app
        .get(
            Some(&c),
            &uri,
            &[("range", format!("bytes={}-", bytes.len()).as_str())],
        )
        .await;
    assert_eq!(res.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        hdr(&res, "content-range"),
        Some(format!("bytes */{}", bytes.len()).as_str())
    );
    // 構文が不正な Range は無視して全体（単位違い・数値でない・桁あふれ・逆順）
    for bad in [
        "items=0-1",
        "bytes=abc-def",
        "bytes=-abc",
        "bytes=99999999999999999999999-",
        "bytes=10-5",
        "bytes=0-1,5-6",
    ] {
        let res = app.get(Some(&c), &uri, &[("range", bad)]).await;
        assert_eq!(res.status(), StatusCode::OK, "{bad}");
        assert_eq!(
            hdr(&res, "content-length"),
            Some(bytes.len().to_string().as_str()),
            "{bad}"
        );
    }
    // 構文は正しいが範囲外 → 416
    let res = app.get(Some(&c), &uri, &[("range", "bytes=-0")]).await;
    assert_eq!(res.status(), StatusCode::RANGE_NOT_SATISFIABLE);

    // HEAD
    let res = app.send(Method::HEAD, Some(&c), &uri, &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        hdr(&res, "content-length"),
        Some(bytes.len().to_string().as_str())
    );
    assert!(body_of(res).await.is_empty());

    // ETag: 完全一致・弱比較・並記・* は 304（ETag と Cache-Control 付き）、違えば 200
    for inm in [
        etag.clone(),
        format!("W/{etag}"),
        format!("\"other\", {etag}"),
        "*".to_owned(),
    ] {
        let res = app
            .get(Some(&c), &uri, &[("if-none-match", inm.as_str())])
            .await;
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED, "{inm}");
        assert_eq!(hdr(&res, "etag"), Some(etag.as_str()), "{inm}");
        assert!(hdr(&res, "cache-control").is_some(), "{inm}");
        assert!(body_of(res).await.is_empty());
    }
    let res = app
        .get(Some(&c), &uri, &[("if-none-match", "\"other\"")])
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    // HEAD + If-None-Match も 304
    let res = app
        .send(
            Method::HEAD,
            Some(&c),
            &uri,
            &[("if-none-match", etag.as_str())],
        )
        .await;
    assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn stale_row_missing_and_unknown() {
    require_ffmpeg!();
    let app = App::new().await;
    let p = app.add("A/01.flac", "flac", 1);
    app.add("A/02.flac", "flac", 2);
    app.scan().await;
    let id = app.track_id("A/01.flac");
    let c = app.cookie().await;
    // 行の mtime をずらす → 開いた FD が行と一致しない → 409
    app.conn()
        .execute(
            "UPDATE tracks SET mtime_ns = mtime_ns + 1 WHERE id = ?1",
            [id],
        )
        .unwrap();
    let res = app.get(Some(&c), &format!("/api/stream/{id}"), &[]).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let body: serde_json::Value = serde_json::from_slice(&body_of(res).await).unwrap();
    assert_eq!(body["error"], "stale");
    // missing → 404
    std::fs::remove_file(&p).unwrap();
    app.scan().await;
    let res = app.get(Some(&c), &format!("/api/stream/{id}"), &[]).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    // 無い id → 404
    let res = app.get(Some(&c), "/api/stream/999999", &[]).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    // セッション無し（CIDR 外）→ 401
    let b = app.track_id("A/02.flac");
    let res = app.get(None, &format!("/api/stream/{b}"), &[]).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

/// 行の dev だけが古い（ホスト再起動で振り直された）ときは stale にしない（D-62）
#[tokio::test]
async fn row_with_stale_dev_only_streams() {
    require_ffmpeg!();
    let app = App::new().await;
    app.add("A/01.flac", "flac", 1);
    app.scan().await;
    let id = app.track_id("A/01.flac");
    let c = app.cookie().await;
    app.conn()
        .execute("UPDATE tracks SET dev = dev + 1 WHERE id = ?1", [id])
        .unwrap();
    let res = app.get(Some(&c), &format!("/api/stream/{id}"), &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn trusted_cidr_streams_without_session() {
    require_ffmpeg!();
    let toml = EXAMPLE.replace(
        "trusted_cidrs = []",
        r#"trusted_cidrs = ["192.168.1.0/24"]"#,
    );
    let app = App::with_config(&toml).await;
    app.add("A/01.flac", "flac", 1);
    app.scan().await;
    let id = app.track_id("A/01.flac");
    let res = app
        .get(
            None,
            &format!("/api/stream/{id}"),
            &[("range", "bytes=0-9")],
        )
        .await;
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(body_of(res).await.len(), 10);
    // 一覧は CIDR 内でもセッション必須
    let res = app.get(None, "/api/tracks", &[]).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn mime_follows_codec() {
    require_ffmpeg!();
    let app = App::new().await;
    app.add("A/01.opus", "opus", 1);
    app.add("A/02.mp3", "mp3", 2);
    app.add("A/03.m4a", "alac.m4a", 3);
    app.scan().await;
    let c = app.cookie().await;
    for (rel, mime) in [
        ("A/01.opus", "audio/ogg"),
        ("A/02.mp3", "audio/mpeg"),
        ("A/03.m4a", "audio/mp4"),
    ] {
        let id = app.track_id(rel);
        let res = app.get(Some(&c), &format!("/api/stream/{id}"), &[]).await;
        assert_eq!(res.status(), StatusCode::OK, "{rel}");
        assert_eq!(hdr(&res, "content-type"), Some(mime), "{rel}");
    }
}

// ---------------------------------------------------------------- transcode=opus

#[tokio::test]
async fn transcode_serves_derived_when_present() {
    require_ffmpeg!();
    let app = App::new().await;
    app.add("A/01.flac", "flac", 1);
    app.scan().await;
    let id = app.track_id("A/01.flac");
    let d = app.add_derived(id, "A/01.opus", 1);
    let bytes = std::fs::read(&d).unwrap();
    let c = app.cookie().await;
    let uri = format!("/api/stream/{id}?transcode=opus");
    let res = app.get(Some(&c), &uri, &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(hdr(&res, "content-type"), Some("audio/ogg"));
    assert_eq!(hdr(&res, "accept-ranges"), Some("bytes"));
    assert_eq!(body_of(res).await, bytes);
    let res = app.get(Some(&c), &uri, &[("range", "bytes=10-19")]).await;
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(body_of(res).await, &bytes[10..20]);
    // 音声版が古い Derived は使わず変換に倒れる（Range 非対応）
    app.conn()
        .execute("UPDATE tracks SET audio_version = 2 WHERE id = ?1", [id])
        .unwrap();
    // 版を進めただけでは stat は変わらないので原本は読める
    let res = app.get(Some(&c), &uri, &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(hdr(&res, "accept-ranges"), Some("none"));
    assert!(hdr(&res, "content-length").is_none());
}

#[tokio::test]
async fn transcode_falls_back_to_ffmpeg_and_supports_start() {
    require_ffmpeg!();
    let app = App::new().await;
    app.add("A/01.flac", "flac", 1);
    app.add("A/02.opus", "opus", 2);
    app.scan().await;
    let id = app.track_id("A/01.flac");
    let c = app.cookie().await;
    let uri = format!("/api/stream/{id}?transcode=opus");
    let res = app.get(Some(&c), &uri, &[("range", "bytes=0-9")]).await;
    // Range は無視して全体を chunked で返す
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(hdr(&res, "content-type"), Some("audio/ogg"));
    assert_eq!(hdr(&res, "accept-ranges"), Some("none"));
    let full = body_of(res).await;
    let tmp = app.dir.path().join("out.opus");
    std::fs::write(&tmp, &full).unwrap();
    let af = read_audio_file(File::open(&tmp).unwrap(), Some("opus")).unwrap();
    assert_eq!(af.codec.as_str(), "opus");
    let full_ms = af.duration_ms.unwrap() as i64;
    assert!((full_ms - 1000).abs() < 150, "{full_ms}");

    let res = app.get(Some(&c), &format!("{uri}&start=0.5"), &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
    let part = body_of(res).await;
    std::fs::write(&tmp, &part).unwrap();
    let af = read_audio_file(File::open(&tmp).unwrap(), Some("opus")).unwrap();
    let part_ms = af.duration_ms.unwrap() as i64;
    assert!((part_ms - 500).abs() < 150, "{part_ms}");

    // 非可逆は変換しない（原本の直送、Range 対応）
    let o = app.track_id("A/02.opus");
    let res = app
        .get(Some(&c), &format!("/api/stream/{o}?transcode=opus"), &[])
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(hdr(&res, "accept-ranges"), Some("bytes"));
    // 不明な transcode 値 → 400
    let res = app
        .get(Some(&c), &format!("/api/stream/{id}?transcode=mp3"), &[])
        .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn transcode_without_ffmpeg_is_503() {
    require_ffmpeg!();
    let toml = EXAMPLE.replace("ffmpeg = \"ffmpeg\"", "ffmpeg = \"/nonexistent/ffmpeg\"");
    assert_ne!(
        toml, EXAMPLE,
        "config.example.toml の bin.ffmpeg の書式が変わった"
    );
    let app = App::with_config(&toml).await;
    app.add("A/01.flac", "flac", 1);
    app.scan().await;
    let id = app.track_id("A/01.flac");
    let c = app.cookie().await;
    let res = app
        .get(Some(&c), &format!("/api/stream/{id}?transcode=opus"), &[])
        .await;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    // 原本の直送は ffmpeg が無くても通る
    let res = app.get(Some(&c), &format!("/api/stream/{id}"), &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
}

/// 偽の ffmpeg を置く。`body` はシェルスクリプトの本文
fn fake_ffmpeg(dir: &std::path::Path, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt as _;
    let p = dir.join("fake-ffmpeg.sh");
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.to_string_lossy().into_owned()
}

fn config_with_ffmpeg(path: &str) -> String {
    let toml = EXAMPLE.replace("ffmpeg = \"ffmpeg\"", &format!("ffmpeg = \"{path}\""));
    assert_ne!(toml, EXAMPLE);
    toml
}

/// stdout が止まったままの ffmpeg は期限で打ち切られ、本文はエラーで終わる。子プロセスも残らない
#[tokio::test]
async fn hung_ffmpeg_is_cut_off_at_the_deadline() {
    require_ffmpeg!();
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let script = fake_ffmpeg(
        dir.path(),
        &format!(
            "echo $$ > {}\nprintf 'OggS'\nexec sleep 300",
            pidfile.display()
        ),
    );
    let app = App::with_grace(
        &config_with_ffmpeg(&script),
        std::time::Duration::from_millis(500),
    )
    .await;
    app.add("A/01.flac", "flac", 1);
    app.scan().await;
    let id = app.track_id("A/01.flac");
    // トラック長を 0 にして deadline = 猶予だけにする
    app.conn()
        .execute("UPDATE tracks SET duration_ms = 0 WHERE id = ?1", [id])
        .unwrap();
    let c = app.cookie().await;
    let started = std::time::Instant::now();
    let res = app
        .get(Some(&c), &format!("/api/stream/{id}?transcode=opus"), &[])
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    let collected = res.into_body().collect().await;
    assert!(collected.is_err(), "本文はエラーで終わる");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // CI のランナーは I/O が遅く回数ベースでは足りないので、経過時間で待つ
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("偽 ffmpeg {pid} が残っている");
}

/// 非ゼロで終わった ffmpeg は本文のエラーになる
#[tokio::test]
async fn failing_ffmpeg_ends_the_body_with_an_error() {
    require_ffmpeg!();
    let dir = tempfile::tempdir().unwrap();
    let script = fake_ffmpeg(dir.path(), "printf 'OggS'\necho boom >&2\nexit 3");
    let app = App::with_config(&config_with_ffmpeg(&script)).await;
    app.add("A/01.flac", "flac", 1);
    app.scan().await;
    let id = app.track_id("A/01.flac");
    let c = app.cookie().await;
    let res = app
        .get(Some(&c), &format!("/api/stream/{id}?transcode=opus"), &[])
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    let err = res.into_body().collect().await.unwrap_err();
    assert!(err.to_string().contains("異常終了"), "{err}");
}

/// 本文を途中で捨てる（クライアント切断）と子プロセスが kill される
#[tokio::test]
async fn dropping_the_body_kills_ffmpeg() {
    require_ffmpeg!();
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let script = fake_ffmpeg(
        dir.path(),
        &format!(
            "echo $$ > {}\nwhile true; do printf 'OggSOggSOggSOggS'; sleep 0.05; done",
            pidfile.display()
        ),
    );
    let app = App::with_config(&config_with_ffmpeg(&script)).await;
    app.add("A/01.flac", "flac", 1);
    app.scan().await;
    let id = app.track_id("A/01.flac");
    let c = app.cookie().await;
    let res = app
        .get(Some(&c), &format!("/api/stream/{id}?transcode=opus"), &[])
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    let mut body = res.into_body();
    // 少し読んでから捨てる
    let first = body.frame().await.unwrap().unwrap();
    assert!(!first.into_data().unwrap().is_empty());
    drop(body);
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // CI のランナーは I/O が遅く回数ベースでは足りないので、経過時間で待つ
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("偽 ffmpeg {pid} が残っている");
}

fn pid_gone(pidfile: &std::path::Path) -> impl std::future::Future<Output = bool> {
    let pidfile = pidfile.to_path_buf();
    async move {
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // CI のランナーは I/O が遅く回数ベースでは足りないので、経過時間で待つ
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while std::time::Instant::now() < deadline {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok();
            // 無い、または zombie でもない生きたプロセスが無ければ OK（zombie は reap 待ち）
            match stat {
                None => return true,
                Some(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            }
        }
        false
    }
}

/// HEAD でオンザフライ変換を要求しても子プロセスは残らない
#[tokio::test]
async fn head_transcode_leaves_no_process() {
    require_ffmpeg!();
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let script = fake_ffmpeg(
        dir.path(),
        &format!(
            "echo $$ > {}\nwhile true; do printf 'OggS'; sleep 0.05; done",
            pidfile.display()
        ),
    );
    let app = App::with_config(&config_with_ffmpeg(&script)).await;
    app.add("A/01.flac", "flac", 1);
    app.scan().await;
    let id = app.track_id("A/01.flac");
    let c = app.cookie().await;
    let res = app
        .send(
            Method::HEAD,
            Some(&c),
            &format!("/api/stream/{id}?transcode=opus"),
            &[],
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(body_of(res).await.is_empty());
    // 起動直後に片付けるので、pid を書く前に死んでいることもある（それなら残りようがない）。
    // 現れないこともあるので、ここは短い回数で打ち切る
    for _ in 0..10 {
        if pidfile.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    if pidfile.exists() {
        assert!(pid_gone(&pidfile).await, "偽 ffmpeg が残っている");
    }
}

/// stdout を閉じた後も終了しない ffmpeg（終了待ちの状態）は、期限で片付く
#[tokio::test]
async fn ffmpeg_that_closes_stdout_but_hangs_is_reaped_at_the_deadline() {
    require_ffmpeg!();
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let script = fake_ffmpeg(
        dir.path(),
        &format!(
            "echo $$ > {}\nprintf 'OggS'\nexec >&-\nexec sleep 300",
            pidfile.display()
        ),
    );
    let app = App::with_grace(
        &config_with_ffmpeg(&script),
        std::time::Duration::from_millis(500),
    )
    .await;
    app.add("A/01.flac", "flac", 1);
    app.scan().await;
    let id = app.track_id("A/01.flac");
    app.conn()
        .execute("UPDATE tracks SET duration_ms = 0 WHERE id = ?1", [id])
        .unwrap();
    let c = app.cookie().await;
    let res = app
        .get(Some(&c), &format!("/api/stream/{id}?transcode=opus"), &[])
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    let collected = res.into_body().collect().await;
    assert!(collected.is_err(), "本文はエラーで終わる");
    assert!(pid_gone(&pidfile).await, "偽 ffmpeg が残っている");
}

/// 終了待ちの状態で本文を捨てても片付く
#[tokio::test]
async fn dropping_the_body_while_waiting_for_exit_reaps() {
    require_ffmpeg!();
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let script = fake_ffmpeg(
        dir.path(),
        &format!(
            "echo $$ > {}\nprintf 'OggS'\nexec >&-\nexec sleep 300",
            pidfile.display()
        ),
    );
    let app = App::with_config(&config_with_ffmpeg(&script)).await;
    app.add("A/01.flac", "flac", 1);
    app.scan().await;
    let id = app.track_id("A/01.flac");
    let c = app.cookie().await;
    let res = app
        .get(Some(&c), &format!("/api/stream/{id}?transcode=opus"), &[])
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    let mut body = res.into_body();
    let first = body.frame().await.unwrap().unwrap();
    assert!(!first.into_data().unwrap().is_empty());
    // stdout は閉じたので次のフレームは来ない（終了待ち）。そこで捨てる
    let next = tokio::time::timeout(std::time::Duration::from_millis(300), body.frame()).await;
    assert!(next.is_err(), "終了待ちで止まっているはず");
    drop(body);
    assert!(pid_gone(&pidfile).await, "偽 ffmpeg が残っている");
}

//! `GET /api/cd/cover/{release_id}`（SPEC §9、D-83、P4-20）。Cover Art Archive の front 画像を
//! 中継する。CAA はローカルの axum で模す。
//!
//! 見るもの: 200 の中継（Content-Type をそのまま、nosniff 付き）、画像が無い盤は 404、MBID でない
//! id は上流に投げる前に 400、上流が壊れている（画像でない / 大きすぎる）ときは 404 と混ぜずに 502、
//! リダイレクトは追うが上限で止まる、クライアント未構成は 503

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Path;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use http_body_util::BodyExt;
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::cd::coverart::CoverArtClient;
use spindle::config::Config;
use spindle::db::Db;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

/// 画像のある盤
const WITH_ART: &str = "f1223d63-f359-457d-b935-fc27eb24a6de";
/// 画像の無い盤
const NO_ART: &str = "00000000-0000-0000-0000-000000000000";
/// 画像でないものを返す盤
const BAD_TYPE: &str = "11111111-1111-1111-1111-111111111111";
/// Content-Length が上限を超える盤
const TOO_BIG: &str = "22222222-2222-2222-2222-222222222222";
/// Content-Length を付けずに上限を超える盤（chunked）
const CHUNKED_BIG: &str = "33333333-3333-3333-3333-333333333333";
/// 307 で別のパスへ飛ばす盤（CAA → archive.org の模擬）
const REDIRECTED: &str = "44444444-4444-4444-4444-444444444444";
/// 自分自身へ飛ばし続ける盤
const REDIRECT_LOOP: &str = "55555555-5555-5555-5555-555555555555";

/// 1x1 の PNG（中身は問わないので先頭の署名だけ）
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
];

/// サーバの上限（8 MiB）より大きい本文
fn huge() -> Vec<u8> {
    vec![0u8; 9 * 1024 * 1024]
}

async fn caa_front(Path((id, size)): Path<(String, String)>) -> axum::response::Response {
    if size != "front-500" {
        return (StatusCode::NOT_FOUND, "wrong size").into_response();
    }
    match id.as_str() {
        WITH_ART => ([(header::CONTENT_TYPE, "image/png")], PNG.to_vec()).into_response(),
        BAD_TYPE => (
            [(header::CONTENT_TYPE, "text/html")],
            "<html>error</html>".to_owned(),
        )
            .into_response(),
        // Vec<u8> をそのまま返すと Content-Length が付く
        TOO_BIG => ([(header::CONTENT_TYPE, "image/png")], huge()).into_response(),
        // Body::from_stream は Content-Length を付けない（chunked）
        CHUNKED_BIG => {
            let chunks = (0..9).map(|_| Ok::<_, std::io::Error>(vec![0u8; 1024 * 1024]));
            (
                [(header::CONTENT_TYPE, "image/png")],
                Body::from_stream(futures_util::stream::iter(chunks)),
            )
                .into_response()
        }
        REDIRECTED => (
            StatusCode::TEMPORARY_REDIRECT,
            [(header::LOCATION, "/moved/image.png")],
        )
            .into_response(),
        REDIRECT_LOOP => (
            StatusCode::TEMPORARY_REDIRECT,
            [(
                header::LOCATION,
                format!("/release/{REDIRECT_LOOP}/front-500"),
            )],
        )
            .into_response(),
        _ => (StatusCode::NOT_FOUND, "no image").into_response(),
    }
}

/// リダイレクトの飛び先
async fn moved_image() -> axum::response::Response {
    ([(header::CONTENT_TYPE, "image/png")], PNG.to_vec()).into_response()
}

async fn serve_caa() -> String {
    let app = Router::new()
        .route("/release/{id}/{size}", get(caa_front))
        .route("/moved/image.png", get(moved_image));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}/")
}

struct App {
    router: Router,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

impl App {
    async fn new(caa_base: Option<String>) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).expect("db"));
        let config = Arc::new(Config::parse(EXAMPLE).expect("config"));
        let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
            .await
            .expect("bootstrap");
        let mut state = AppState::new(config, db, mode);
        if let Some(base) = caa_base {
            let client = CoverArtClient::new(base, "spindle-test/0.1").expect("client");
            state = state.with_coverart(Arc::new(client));
        }
        Self {
            router: api::router(state),
            dir,
        }
    }

    async fn cookie(&self) -> String {
        let r = req(Method::POST, "/api/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(r#"{"password":"correct horse"}"#))
            .expect("request");
        let res = self.router.clone().oneshot(r).await.expect("response");
        assert_eq!(res.status(), StatusCode::OK);
        let set = res
            .headers()
            .get(header::SET_COOKIE)
            .expect("set-cookie")
            .to_str()
            .expect("str");
        set.split(';').next().expect("cookie").to_string()
    }

    /// (status, Content-Type, X-Content-Type-Options, 本文)
    async fn cover(
        &self,
        c: &str,
        id: &str,
    ) -> (StatusCode, Option<String>, Option<String>, Vec<u8>) {
        let r = req(Method::GET, &format!("/api/cd/cover/{id}"))
            .header(header::COOKIE, c)
            .header("sec-fetch-site", "same-origin")
            .body(Body::empty())
            .expect("request");
        let res = self.router.clone().oneshot(r).await.expect("response");
        let status = res.status();
        let head = |k: header::HeaderName| {
            res.headers()
                .get(k)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let ct = head(header::CONTENT_TYPE);
        let nosniff = head(header::X_CONTENT_TYPE_OPTIONS);
        let bytes = res.into_body().collect().await.expect("body").to_bytes();
        (status, ct, nosniff, bytes.to_vec())
    }
}

fn req(method: Method, uri: &str) -> axum::http::request::Builder {
    let peer: SocketAddr = LAN.parse().expect("addr");
    let mut b = Request::builder().method(method).uri(uri);
    b.extensions_mut()
        .expect("ext")
        .insert(axum::extract::ConnectInfo(peer));
    b
}

#[tokio::test]
async fn relays_the_front_image() {
    let app = App::new(Some(serve_caa().await)).await;
    let c = app.cookie().await;
    let (status, ct, nosniff, body) = app.cover(&c, WITH_ART).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct.as_deref(), Some("image/png"));
    assert_eq!(nosniff.as_deref(), Some("nosniff"));
    assert_eq!(body, PNG);
}

#[tokio::test]
async fn a_release_without_art_is_not_found() {
    let app = App::new(Some(serve_caa().await)).await;
    let c = app.cookie().await;
    let (status, _, _, _) = app.cover(&c, NO_ART).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// 上流に投げる前に弾く（文字列をそのまま URL に継ぎ足さない）
#[tokio::test]
async fn a_release_id_that_is_not_an_mbid_is_rejected() {
    let app = App::new(Some(serve_caa().await)).await;
    let c = app.cookie().await;
    for id in [
        "not-an-mbid",
        "f1223d63f359457db935fc27eb24a6de",
        "f1223d63-f359-457d-b935-fc27eb24a6dz",
        "..%2F..%2Fetc%2Fpasswd",
    ] {
        let (status, _, _, _) = app.cover(&c, id).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{id}");
    }
}

/// 上流が壊れているときは 404 と混ぜず 502（「画像が無い」と「上流がおかしい」を見分ける）
#[tokio::test]
async fn a_broken_upstream_is_a_bad_gateway() {
    let app = App::new(Some(serve_caa().await)).await;
    let c = app.cookie().await;
    for id in [BAD_TYPE, TOO_BIG, CHUNKED_BIG] {
        let (status, _, _, _) = app.cover(&c, id).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{id}");
    }
}

/// 307 のリダイレクトは追う（CAA は archive.org へ飛ばす）。ただし上限で止まる
#[tokio::test]
async fn it_follows_the_redirect_but_stops_at_the_limit() {
    let app = App::new(Some(serve_caa().await)).await;
    let c = app.cookie().await;
    let (status, ct, _, body) = app.cover(&c, REDIRECTED).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct.as_deref(), Some("image/png"));
    assert_eq!(body, PNG);
    // 自分自身へ飛ばし続ける枝は上限で止まり、502 になる（無限には追わない）
    let (status, _, _, _) = app.cover(&c, REDIRECT_LOOP).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn without_a_client_it_is_unavailable() {
    let app = App::new(None).await;
    let c = app.cookie().await;
    let (status, _, _, _) = app.cover(&c, WITH_ART).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

/// 起動時の `cover_art_url` の検証（http(s) でホスト付きだけ通す）
#[test]
fn the_base_url_must_be_http_with_a_host() {
    for bad in [
        "ftp://example.com/",
        "file:///tmp/",
        "not a url",
        "https:///",
    ] {
        assert!(
            CoverArtClient::new(bad, "spindle-test/0.1").is_err(),
            "{bad}"
        );
    }
    assert!(CoverArtClient::new("https://coverartarchive.org/", "spindle-test/0.1").is_ok());
}

/// リダイレクトの判定（TLS を立てずに URL の列で固定する）
#[test]
fn redirects_are_limited_and_never_downgrade() {
    let u = |s: &str| reqwest::Url::parse(s).expect("url");
    let https = u("https://coverartarchive.org/release/x/front-500");
    // HTTPS → HTTPS は追う
    assert!(spindle::cd::may_follow(
        &https,
        &u("https://ia800.us.archive.org/x.jpg"),
        1,
        5
    ));
    // HTTPS → HTTP は追わない
    assert!(!spindle::cd::may_follow(
        &https,
        &u("http://ia800.us.archive.org/x.jpg"),
        1,
        5
    ));
    // 上限に達したら追わない
    assert!(!spindle::cd::may_follow(
        &https,
        &u("https://ia800.us.archive.org/x.jpg"),
        5,
        5
    ));
    // HTTP 起点（自前ミラー・テスト）から HTTP は追う
    let http = u("http://127.0.0.1:1/release/x/front-500");
    assert!(spindle::cd::may_follow(
        &http,
        &u("http://127.0.0.1:1/y.png"),
        0,
        5
    ));
}

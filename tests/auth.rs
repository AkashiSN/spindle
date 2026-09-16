//! 認証（P0-3）: 初期パスワード、セッション Cookie、CSRF、trusted_cidrs、ロックモード。
//! 仕様: docs/SPEC.md §9「認証」、docs/DECISIONS.md D-27 / D-28

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::Db;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";
const OUTSIDE: &str = "10.0.0.9:50000";

struct TestApp {
    router: Router,
    _dir: tempfile::TempDir,
}

/// 設定を上書きしつつ、一時 DB で AppState を組み立てる
async fn build(initial_password: Option<&str>, auth_override: &str) -> TestApp {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());

    let mut root: toml::Table = toml::from_str(EXAMPLE).unwrap();
    let patch: toml::Table = toml::from_str(auth_override).unwrap();
    let section = root.get_mut("auth").unwrap().as_table_mut().unwrap();
    for (k, v) in patch {
        section.insert(k, v);
    }
    let config = Arc::new(Config::parse(&toml::to_string(&root).unwrap()).unwrap());

    let mode = auth::bootstrap(&db, initial_password.map(str::to_owned))
        .await
        .unwrap();
    let state = AppState::new(config, db, mode);
    TestApp {
        router: api::router(state),
        _dir: dir,
    }
}

async fn app() -> TestApp {
    build(Some("correct horse"), "").await
}

/// 接続元アドレスを付けたリクエストを組み立てる
fn req(method: Method, uri: &str, peer: &str) -> axum::http::request::Builder {
    let peer: SocketAddr = peer.parse().unwrap();
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

/// ログインして Cookie 値（`name=value`）を返す
async fn login(app: &TestApp, peer: &str, password: &str) -> axum::response::Response {
    let r = req(Method::POST, "/api/auth/login", peer)
        .header(header::CONTENT_TYPE, "application/json")
        .header("sec-fetch-site", "same-origin")
        .body(Body::from(format!(r#"{{"password":{password:?}}}"#)))
        .unwrap();
    send(app, r).await
}

fn cookie_of(res: &axum::response::Response) -> String {
    let set = res
        .headers()
        .get(header::SET_COOKIE)
        .expect("Set-Cookie があること")
        .to_str()
        .unwrap();
    set.split(';').next().unwrap().to_string()
}

async fn session_cookie(app: &TestApp) -> String {
    let res = login(app, LAN, "correct horse").await;
    assert_eq!(res.status(), StatusCode::OK);
    cookie_of(&res)
}

// ---------------------------------------------------------------- ロックモード

#[tokio::test]
async fn lock_mode_when_no_password_anywhere() {
    let app = build(None, "").await;
    let res = send(
        &app,
        req(Method::GET, "/health", LAN)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(json(res).await, serde_json::json!({ "status": "locked" }));

    for uri in ["/", "/index.html", "/api/tracks", "/api/auth/session"] {
        let res = send(
            &app,
            req(Method::GET, uri, LAN).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        let body = json(res).await;
        assert_eq!(body["error"], "locked", "{uri}: {body}");
    }
    let res = login(&app, LAN, "anything").await;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn initial_password_is_stored_once_and_env_ignored_afterwards() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());

    let mode = auth::bootstrap(&db, Some("first".into())).await.unwrap();
    assert_eq!(mode, auth::Mode::Unlocked);
    let hash1: String = db
        .read(|c| {
            Ok(
                c.query_row("SELECT password_hash FROM auth WHERE id = 1", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert!(hash1.starts_with("$argon2id$"), "{hash1}");

    // 2 回目以降は環境変数を無視する（別の値でも上書きしない）
    let mode = auth::bootstrap(&db, Some("second".into())).await.unwrap();
    assert_eq!(mode, auth::Mode::Unlocked);
    let hash2: String = db
        .read(|c| {
            Ok(
                c.query_row("SELECT password_hash FROM auth WHERE id = 1", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(hash1, hash2);

    // DB にあれば環境変数が無くてもロックされない
    assert_eq!(
        auth::bootstrap(&db, None).await.unwrap(),
        auth::Mode::Unlocked
    );
    // 空文字は未設定扱い
    let dir2 = tempfile::tempdir().unwrap();
    let db2 = Arc::new(Db::open(&dir2.path().join("spindle.db")).unwrap());
    assert_eq!(
        auth::bootstrap(&db2, Some("".into())).await.unwrap(),
        auth::Mode::Locked
    );
}

// ---------------------------------------------------------------- 未認証

#[tokio::test]
async fn unauthenticated_api_returns_401() {
    let app = app().await;
    for uri in [
        "/api/tracks",
        "/api/jobs",
        "/api/events",
        "/api/auth/session",
        "/api/stream/1",
    ] {
        let res = send(
            &app,
            req(Method::GET, uri, OUTSIDE).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(json(res).await["error"], "unauthenticated", "{uri}");
    }
}

#[tokio::test]
async fn spa_and_health_are_served_without_session() {
    let app = app().await;
    let res = send(
        &app,
        req(Method::GET, "/health", OUTSIDE)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(json(res).await, serde_json::json!({ "status": "ok" }));
    // SPA（ログイン画面を出すため）はセッション不要。同梱の有無で 200 か 404
    let res = send(
        &app,
        req(Method::GET, "/", OUTSIDE).body(Body::empty()).unwrap(),
    )
    .await;
    assert!(
        res.status() == StatusCode::OK || res.status() == StatusCode::NOT_FOUND,
        "{}",
        res.status()
    );
}

#[tokio::test]
async fn cors_preflight_is_not_answered() {
    let app = app().await;
    let r = req(Method::OPTIONS, "/api/tracks", OUTSIDE)
        .header(header::ORIGIN, "http://evil.example")
        .header("access-control-request-method", "PATCH")
        .body(Body::empty())
        .unwrap();
    let res = send(&app, r).await;
    assert_ne!(res.status(), StatusCode::OK);
    assert_ne!(res.status(), StatusCode::NO_CONTENT);
    assert!(res.headers().get("access-control-allow-origin").is_none());
}

// ---------------------------------------------------------------- ログイン / セッション

#[tokio::test]
async fn login_sets_httponly_lax_cookie_and_session_works() {
    let app = app().await;
    let res = login(&app, LAN, "correct horse").await;
    assert_eq!(res.status(), StatusCode::OK);
    let set_cookie = res
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(set_cookie.starts_with("spindle_session="), "{set_cookie}");
    assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
    assert!(set_cookie.contains("SameSite=Lax"), "{set_cookie}");
    assert!(set_cookie.contains("Path=/"), "{set_cookie}");
    assert!(
        !set_cookie.contains("Secure"),
        "TLS でない接続では Secure を付けない: {set_cookie}"
    );
    let body = json(res).await;
    assert!(body["expires_at"].as_i64().unwrap() > 1_700_000_000);

    let cookie = set_cookie.split(';').next().unwrap().to_string();
    let r = req(Method::GET, "/api/auth/session", LAN)
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let res = send(&app, r).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(json(res).await["expires_at"].as_i64().is_some());
}

#[tokio::test]
async fn login_with_wrong_password_returns_401_without_cookie() {
    let app = app().await;
    let res = login(&app, LAN, "wrong").await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert!(res.headers().get(header::SET_COOKIE).is_none());
    assert_eq!(json(res).await["error"], "invalid_password");
}

#[tokio::test]
async fn session_token_is_stored_hashed_and_expires() {
    let app = build(Some("correct horse"), "session_days = 1").await;
    let cookie = session_cookie(&app).await;
    let raw = cookie.trim_start_matches("spindle_session=").to_string();

    let dir = &app._dir;
    let conn = rusqlite::Connection::open(dir.path().join("spindle.db")).unwrap();
    let (token_hash, created, expires): (Vec<u8>, i64, i64) = conn
        .query_row(
            "SELECT token_hash, created_at, expires_at FROM sessions",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(token_hash.len(), 32);
    assert_ne!(
        base64_url(&token_hash),
        raw,
        "生トークンを保存してはいけない"
    );
    assert_eq!(expires - created, 86_400, "session_days = 1");

    // 期限切れにするとセッションが無効になり、次のログインで掃除される
    conn.execute("UPDATE sessions SET expires_at = 1", [])
        .unwrap();
    let r = req(Method::GET, "/api/auth/session", LAN)
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.status(), StatusCode::UNAUTHORIZED);

    let _ = session_cookie(&app).await;
    let n: i64 = conn
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "期限切れの行が掃除されていない");
}

fn base64_url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[tokio::test]
async fn logout_deletes_session_and_clears_cookie() {
    let app = app().await;
    let cookie = session_cookie(&app).await;
    let r = req(Method::POST, "/api/auth/logout", LAN)
        .header(header::COOKIE, &cookie)
        .header("sec-fetch-site", "same-origin")
        .body(Body::empty())
        .unwrap();
    let res = send(&app, r).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let set = res
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(set.contains("Max-Age=0"), "{set}");

    let r = req(Method::GET, "/api/auth/session", LAN)
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn garbage_cookie_is_401() {
    let app = app().await;
    for value in [
        "spindle_session=",
        "spindle_session=notbase64!!",
        "spindle_session=AAAA",
    ] {
        let r = req(Method::GET, "/api/auth/session", LAN)
            .header(header::COOKIE, value)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            send(&app, r).await.status(),
            StatusCode::UNAUTHORIZED,
            "{value}"
        );
    }
}

#[tokio::test]
async fn login_failures_are_rate_limited_per_ip() {
    let app = app().await;
    let mut last = StatusCode::OK;
    for _ in 0..auth::LOGIN_MAX_FAILURES {
        last = login(&app, OUTSIDE, "wrong").await.status();
    }
    assert_eq!(last, StatusCode::UNAUTHORIZED);
    // 上限を超えると正しいパスワードでも 429
    assert_eq!(
        login(&app, OUTSIDE, "correct horse").await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    // 別 IP は影響を受けない
    assert_eq!(
        login(&app, LAN, "correct horse").await.status(),
        StatusCode::OK
    );
}

// ---------------------------------------------------------------- CSRF

#[tokio::test]
async fn mutating_request_with_foreign_origin_is_403_even_with_session() {
    let app = app().await;
    let cookie = session_cookie(&app).await;
    // Host は一致しているが Origin が別サイト → 403（Host を判定に使ってはいけない）
    let r = req(Method::POST, "/api/auth/logout", LAN)
        .header(header::HOST, "spindle.local:8080")
        .header(header::ORIGIN, "http://evil.example")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let res = send(&app, r).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(json(res).await["error"], "csrf");
    // Origin: null も拒否
    let r = req(Method::POST, "/api/auth/logout", LAN)
        .header(header::HOST, "spindle.local:8080")
        .header(header::ORIGIN, "null")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn mutating_request_with_matching_origin_passes() {
    let app = app().await;
    let cookie = session_cookie(&app).await;
    let r = req(Method::POST, "/api/auth/logout", LAN)
        .header(header::HOST, "spindle.local:8080")
        .header(header::ORIGIN, "http://spindle.local:8080")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn origin_scheme_or_port_mismatch_is_403() {
    let app = app().await;
    let cookie = session_cookie(&app).await;
    for origin in [
        "https://spindle.local:8080",
        "http://spindle.local",
        "http://spindle.local:8081",
    ] {
        let r = req(Method::POST, "/api/auth/logout", LAN)
            .header(header::HOST, "spindle.local:8080")
            .header(header::ORIGIN, origin)
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            send(&app, r).await.status(),
            StatusCode::FORBIDDEN,
            "{origin}"
        );
    }
}

#[tokio::test]
async fn mutating_request_without_origin_uses_sec_fetch_site() {
    let app = app().await;
    let cookie = session_cookie(&app).await;
    for (site, expected) in [
        (Some("same-origin"), StatusCode::NO_CONTENT),
        (Some("none"), StatusCode::NO_CONTENT),
        (Some("cross-site"), StatusCode::FORBIDDEN),
        (Some("same-site"), StatusCode::FORBIDDEN),
        (None, StatusCode::FORBIDDEN),
    ] {
        let app_cookie = if expected == StatusCode::NO_CONTENT {
            session_cookie(&app).await
        } else {
            cookie.clone()
        };
        let mut b = req(Method::POST, "/api/auth/logout", LAN)
            .header(header::HOST, "spindle.local:8080")
            .header(header::COOKIE, &app_cookie);
        if let Some(site) = site {
            b = b.header("sec-fetch-site", site);
        }
        let res = send(&app, b.body(Body::empty()).unwrap()).await;
        assert_eq!(res.status(), expected, "sec-fetch-site: {site:?}");
    }
}

#[tokio::test]
async fn csrf_check_applies_to_login_too() {
    let app = app().await;
    let r = req(Method::POST, "/api/auth/login", OUTSIDE)
        .header(header::HOST, "spindle.local:8080")
        .header(header::ORIGIN, "http://evil.example")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"password":"correct horse"}"#))
        .unwrap();
    assert_eq!(send(&app, r).await.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn forwarded_headers_are_only_trusted_from_trusted_proxies() {
    // proxy 越しの外部 origin は trusted_proxies からの X-Forwarded-* だけで構成する
    let app = build(
        Some("correct horse"),
        r#"trusted_proxies = ["10.0.0.9/32"]"#,
    )
    .await;
    let cookie = session_cookie(&app).await;

    // 信頼する proxy から: X-Forwarded-Host/Proto で https://music.example が自分自身
    let r = req(Method::POST, "/api/auth/logout", OUTSIDE)
        .header(header::HOST, "127.0.0.1:8080")
        .header("x-forwarded-host", "music.example")
        .header("x-forwarded-proto", "https")
        .header(header::ORIGIN, "https://music.example")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.status(), StatusCode::NO_CONTENT);

    // 信頼しない接続元からの同じヘッダは無視される → Origin が Host と合わず 403
    let cookie = session_cookie(&app).await;
    let r = req(Method::POST, "/api/auth/logout", LAN)
        .header(header::HOST, "127.0.0.1:8080")
        .header("x-forwarded-host", "music.example")
        .header("x-forwarded-proto", "https")
        .header(header::ORIGIN, "https://music.example")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn secure_cookie_when_proxy_says_https() {
    let app = build(
        Some("correct horse"),
        r#"trusted_proxies = ["10.0.0.9/32"]"#,
    )
    .await;
    let r = req(Method::POST, "/api/auth/login", OUTSIDE)
        .header("x-forwarded-proto", "https")
        .header("sec-fetch-site", "same-origin")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"password":"correct horse"}"#))
        .unwrap();
    let res = send(&app, r).await;
    assert_eq!(res.status(), StatusCode::OK);
    let set = res
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(set.contains("Secure"), "{set}");
}

// ---------------------------------------------------------------- trusted_cidrs

#[tokio::test]
async fn trusted_cidr_skips_auth_only_for_allowlisted_routes() {
    let app = build(
        Some("correct horse"),
        r#"trusted_cidrs = ["192.168.1.0/24"]"#,
    )
    .await;
    // allowlist: stream / artwork / tracks/:id / playlists/:id/export は 401 にならない
    // （ハンドラ未実装なので 404 だが、認証で弾かれていないことが要点）
    for uri in [
        "/api/stream/1",
        "/api/artwork/abc",
        "/api/tracks/1",
        "/api/playlists/1/export",
    ] {
        let res = send(
            &app,
            req(Method::GET, uri, LAN).body(Body::empty()).unwrap(),
        )
        .await;
        assert_ne!(res.status(), StatusCode::UNAUTHORIZED, "{uri}");
    }
    // 一覧・検索・SSE・履歴・ジョブ・session は CIDR 内でも 401
    for uri in [
        "/api/tracks",
        "/api/search?q=a",
        "/api/events",
        "/api/history",
        "/api/jobs",
        "/api/auth/session",
    ] {
        let res = send(
            &app,
            req(Method::GET, uri, LAN).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{uri}");
    }
    // 変更系は CIDR 内でも 401（CSRF が通っていても）
    let r = req(Method::POST, "/api/tracks/batch/preview", LAN)
        .header("sec-fetch-site", "same-origin")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.status(), StatusCode::UNAUTHORIZED);
    // allowlist のパスでも POST は 401
    let r = req(Method::POST, "/api/stream/1", LAN)
        .header("sec-fetch-site", "same-origin")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.status(), StatusCode::UNAUTHORIZED);
    // CIDR 外からは allowlist でも 401
    let res = send(
        &app,
        req(Method::GET, "/api/stream/1", OUTSIDE)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn trusted_cidr_uses_socket_address_not_forwarded_for() {
    let app = build(
        Some("correct horse"),
        r#"trusted_cidrs = ["192.168.1.0/24"]"#,
    )
    .await;
    // 接続元は CIDR 外。X-Forwarded-For で偽装しても通らない
    let r = req(Method::GET, "/api/stream/1", OUTSIDE)
        .header("x-forwarded-for", "192.168.1.5")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn forwarded_for_from_trusted_proxy_is_used_for_cidr_check() {
    let app = build(
        Some("correct horse"),
        r#"trusted_cidrs = ["192.168.1.0/24"]
trusted_proxies = ["10.0.0.9/32"]"#,
    )
    .await;
    let r = req(Method::GET, "/api/stream/1", OUTSIDE)
        .header("x-forwarded-for", "192.168.1.5")
        .body(Body::empty())
        .unwrap();
    assert_ne!(send(&app, r).await.status(), StatusCode::UNAUTHORIZED);
    // proxy 経由でも XFF の値が CIDR 外なら 401
    let r = req(Method::GET, "/api/stream/1", OUTSIDE)
        .header("x-forwarded-for", "203.0.113.7")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------- レビュー指摘の回帰

#[tokio::test]
async fn concurrent_login_attempts_cannot_exceed_rate_limit() {
    // 上限の確認と失敗記録が分離していると、並行送信で全部が検証へ進んでしまう。
    // 検証前に枠を原子的に予約するので、11 並行なら 10 件だけが検証（401）され 1 件は 429
    let app = Arc::new(app().await);
    let n = auth::LOGIN_MAX_FAILURES as usize + 1;
    let mut handles = Vec::new();
    for _ in 0..n {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            login(&app, OUTSIDE, "wrong").await.status()
        }));
    }
    let mut unauthorized = 0;
    let mut limited = 0;
    for h in handles {
        match h.await.unwrap() {
            StatusCode::UNAUTHORIZED => unauthorized += 1,
            StatusCode::TOO_MANY_REQUESTS => limited += 1,
            other => panic!("想定外の応答: {other}"),
        }
    }
    assert_eq!(unauthorized, auth::LOGIN_MAX_FAILURES as usize);
    assert_eq!(limited, 1);
}

#[tokio::test]
async fn db_failure_during_session_lookup_is_500_not_401() {
    let app = app().await;
    let cookie = session_cookie(&app).await;
    let conn = rusqlite::Connection::open(app._dir.path().join("spindle.db")).unwrap();
    conn.execute("DROP TABLE sessions", []).unwrap();
    let r = req(Method::GET, "/api/auth/session", LAN)
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let res = send(&app, r).await;
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json(res).await["error"], "internal");
}

#[tokio::test]
async fn db_failure_during_login_is_500_not_locked() {
    let app = app().await;
    let conn = rusqlite::Connection::open(app._dir.path().join("spindle.db")).unwrap();
    conn.execute("DROP TABLE auth", []).unwrap();
    let res = login(&app, LAN, "correct horse").await;
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn forwarded_for_chain_skips_trusted_proxy_hops() {
    // client, proxy1 → proxy2(peer) のチェーン。右から trusted proxy を飛ばした最初の IP が client
    let app = build(
        Some("correct horse"),
        r#"trusted_cidrs = ["192.168.1.0/24"]
trusted_proxies = ["10.0.0.9/32", "10.0.0.10/32"]"#,
    )
    .await;
    let r = req(Method::GET, "/api/stream/1", OUTSIDE)
        .header("x-forwarded-for", "192.168.1.5, 10.0.0.10")
        .body(Body::empty())
        .unwrap();
    assert_ne!(send(&app, r).await.status(), StatusCode::UNAUTHORIZED);

    // 複数ヘッダ行も 1 本のチェーンとして扱う
    let r = req(Method::GET, "/api/stream/1", OUTSIDE)
        .header("x-forwarded-for", "192.168.1.5")
        .header("x-forwarded-for", "10.0.0.10")
        .body(Body::empty())
        .unwrap();
    assert_ne!(send(&app, r).await.status(), StatusCode::UNAUTHORIZED);

    // trusted でない hop（203.0.113.7 の自己申告 192.168.1.5）は採用しない
    let r = req(Method::GET, "/api/stream/1", OUTSIDE)
        .header("x-forwarded-for", "192.168.1.5, 203.0.113.7")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.status(), StatusCode::UNAUTHORIZED);

    // 壊れた値は client 不明として扱い、allowlist を通さない
    let r = req(Method::GET, "/api/stream/1", OUTSIDE)
        .header("x-forwarded-for", "not-an-ip, 10.0.0.10")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn trusted_proxy_without_forwarded_for_is_not_a_client() {
    // trusted_proxies と trusted_cidrs が重なる設定で、XFF の無い / 空の / trusted hop だけの
    // リクエストが proxy 自身を「信頼できるクライアント」と誤認して allowlist を通してはいけない
    let app = build(
        Some("correct horse"),
        r#"trusted_cidrs = ["10.0.0.0/24"]
trusted_proxies = ["10.0.0.0/24"]"#,
    )
    .await;
    let cases: [Vec<&str>; 4] = [
        vec![],
        vec![""],
        vec!["10.0.0.10"],
        vec!["10.0.0.10, 10.0.0.11"],
    ];
    for xff in cases {
        let mut b = req(Method::GET, "/api/stream/1", OUTSIDE);
        for v in &xff {
            b = b.header("x-forwarded-for", *v);
        }
        let res = send(&app, b.body(Body::empty()).unwrap()).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "xff={xff:?}");
    }
    // 非 trusted のクライアントが CIDR 内なら通る（対照）
    let r = req(Method::GET, "/api/stream/1", OUTSIDE)
        .header("x-forwarded-for", "10.0.0.200")
        .body(Body::empty())
        .unwrap();
    // 10.0.0.200 は trusted_proxies にも入っているので hop として飛ばされる → 不明 → 401。
    // CIDR と proxy を分けた設定で通ることは forwarded_for_from_trusted_proxy_is_used_for_cidr_check が固定
    assert_eq!(send(&app, r).await.status(), StatusCode::UNAUTHORIZED);
}

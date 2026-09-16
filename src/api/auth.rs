//! 認証（SPEC §9「認証」、D-27 / D-28）。
//!
//! - パスワードは 1 つ。argon2id のハッシュを `auth` 表に持つ。初期値は
//!   `SPINDLE_INITIAL_PASSWORD` を DB にパスワードが無い初回起動だけ読む。無ければロックモード
//! - セッションはランダム 32 バイトの Cookie。DB には SHA-256 だけ保存する
//! - 変更系リクエストは `Origin` の完全一致、無ければ `Sec-Fetch-Site` で CSRF を判定する。
//!   **`Host` は判定に使わない**（クロスサイト POST でも Host は送信先になるため）
//! - `trusted_cidrs` からの接続は route allowlist（stream / artwork / tracks/:id /
//!   playlists/:id/export の GET）だけ認証をスキップする。判定は socket アドレスで、
//!   `X-Forwarded-*` は `trusted_proxies` からのものだけ採用する

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use rusqlite::OptionalExtension;

use crate::db::{now_epoch, Db, DbError};

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;

pub const COOKIE_NAME: &str = "spindle_session";
/// ログイン失敗のレート制限: 同一 IP から `LOGIN_WINDOW` の間に `LOGIN_MAX_FAILURES` 回失敗で 429
pub const LOGIN_MAX_FAILURES: u32 = 10;
pub const LOGIN_WINDOW: Duration = Duration::from_secs(15 * 60);
/// レート制限表のハード上限。到達したら期限切れを掃除し、それでも一杯なら最古の窓を追い出す
const LIMITER_MAX_ENTRIES: usize = 1024;

const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// 起動時に決まる動作モード。ロックモードは `/health` 以外を 503 で返す
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Unlocked,
    Locked,
}

/// プロセス内で共有する認証状態
pub struct Shared {
    pub mode: Mode,
    limiter: Mutex<HashMap<IpAddr, Attempts>>,
}

/// 窓内の試行数。失敗だけでなく**検証中の試行も含めて**数える（検証前に枠を予約する）
struct Attempts {
    count: u32,
    window_start: Instant,
}

/// 枠の予約結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reservation {
    Granted,
    Limited,
}

impl Shared {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            limiter: Mutex::new(HashMap::new()),
        }
    }

    /// ログイン試行の枠を原子的に予約する。窓内の試行が上限に達していれば `Limited`。
    /// 予約した枠は成功時に [`Shared::clear_attempts`] で解放し、失敗時はそのまま失敗として残す
    pub fn reserve_attempt(&self, ip: IpAddr) -> Reservation {
        let now = Instant::now();
        let mut map = self.limiter.lock().unwrap_or_else(|e| e.into_inner());
        if !map.contains_key(&ip) && map.len() >= LIMITER_MAX_ENTRIES {
            map.retain(|_, a| now.duration_since(a.window_start) < LOGIN_WINDOW);
            if map.len() >= LIMITER_MAX_ENTRIES {
                // 期限内で一杯なら最古の窓を追い出す（その IP の制限は緩むが、表は有界に保つ）
                let oldest = map
                    .iter()
                    .min_by_key(|(_, a)| a.window_start)
                    .map(|(ip, _)| *ip);
                if let Some(oldest) = oldest {
                    map.remove(&oldest);
                }
            }
        }
        let entry = map.entry(ip).or_insert(Attempts {
            count: 0,
            window_start: now,
        });
        if now.duration_since(entry.window_start) >= LOGIN_WINDOW {
            *entry = Attempts {
                count: 0,
                window_start: now,
            };
        }
        if entry.count >= LOGIN_MAX_FAILURES {
            return Reservation::Limited;
        }
        entry.count += 1;
        Reservation::Granted
    }

    /// ログイン成功: この IP の試行記録を消す
    pub fn clear_attempts(&self, ip: IpAddr) {
        self.limiter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&ip);
    }

    /// 追跡中の IP 数（テスト・診断用）
    pub fn tracked_ips(&self) -> usize {
        self.limiter.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

// ---------------------------------------------------------------- 起動時

/// DB にパスワードが無ければ `initial_password` を argon2id で保存する。
/// どちらも無ければ [`Mode::Locked`]。空文字は未設定として扱う。期限切れセッションも掃除する
pub async fn bootstrap(db: &Db, initial_password: Option<String>) -> Result<Mode, DbError> {
    let exists: bool = db
        .read(|c| {
            Ok(
                c.query_row("SELECT count(*) FROM auth WHERE id = 1", [], |r| {
                    r.get::<_, i64>(0)
                })? > 0,
            )
        })
        .await?;
    let mode = if exists {
        Mode::Unlocked
    } else {
        match initial_password.filter(|p| !p.is_empty()) {
            Some(password) => {
                let hash = tokio::task::spawn_blocking(move || hash_password(&password)).await??;
                db.write(move |c| {
                    c.execute(
                        "INSERT INTO auth (id, password_hash, updated_at) VALUES (1, ?1, ?2)",
                        (hash, now_epoch()),
                    )?;
                    Ok(())
                })
                .await?;
                tracing::info!("SPINDLE_INITIAL_PASSWORD からパスワードを初期化した。以後この環境変数は無視する");
                Mode::Unlocked
            }
            None => {
                tracing::warn!(
                    "パスワードが未設定（環境変数 SPINDLE_INITIAL_PASSWORD も DB も無い）。ロックモードで起動する"
                );
                Mode::Locked
            }
        }
    };
    purge_expired_sessions(db).await?;
    Ok(mode)
}

fn hash_password(password: &str) -> Result<String, DbError> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| DbError::Internal(format!("argon2id のハッシュ化に失敗: {e}")))
}

fn verify_password(password: &str, hash: &str) -> bool {
    // 壊れたハッシュ文字列も「不一致」として扱う（DB を手で触った場合など）
    PasswordHash::new(hash).is_ok_and(|parsed| {
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

async fn purge_expired_sessions(db: &Db) -> Result<(), DbError> {
    let n = db
        .write(|c| Ok(c.execute("DELETE FROM sessions WHERE expires_at <= ?1", [now_epoch()])?))
        .await?;
    if n > 0 {
        tracing::info!(count = n, "期限切れセッションを掃除した");
    }
    Ok(())
}

// ---------------------------------------------------------------- リクエストの素性

/// ミドルウェアが解決してハンドラへ渡す接続元情報
#[derive(Debug, Clone)]
pub struct ClientInfo {
    /// 接続元 IP。trusted proxy 経由なら `X-Forwarded-For` を右から辿って trusted hop を
    /// 飛ばした最初の IP。判定できなければ `None`（allowlist は通さず、レート制限は不明キー）
    pub ip: Option<IpAddr>,
    /// `X-Forwarded-Proto: https` を trusted proxy から受け取ったか（Cookie の Secure に使う）
    pub https: bool,
}

/// 検証済みのセッション。ハンドラは `Extension<Session>` で受け取る
#[derive(Debug, Clone)]
pub struct Session {
    pub token_hash: [u8; 32],
    pub expires_at: i64,
}

fn resolve_client(state: &AppState, headers: &HeaderMap, socket: Option<SocketAddr>) -> ClientInfo {
    let socket_ip = socket.map(|s| s.ip());
    let is_trusted_proxy = |ip: &IpAddr| {
        state
            .config
            .auth
            .trusted_proxies
            .iter()
            .any(|net| net.contains(ip))
    };
    let via_trusted_proxy = socket_ip.as_ref().is_some_and(is_trusted_proxy);
    if !via_trusted_proxy {
        return ClientInfo {
            ip: socket_ip,
            https: false,
        };
    }
    // X-Forwarded-For は `client, proxy1, proxy2...` の順に proxy が末尾へ追記する。
    // socket peer から右→左へ辿り、trusted proxy の hop を飛ばした最初の IP がクライアント。
    // それより左は自己申告なので信用しない。壊れた値があればクライアント不明として扱う。
    // XFF が無い・空・trusted hop だけの場合も不明（socket peer は proxy 自身であってクライアントではない）
    let chain: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let mut client_ip = None;
    for hop in chain.iter().rev() {
        match hop.parse::<IpAddr>() {
            Ok(ip) if is_trusted_proxy(&ip) => continue,
            Ok(ip) => {
                client_ip = Some(ip);
                break;
            }
            Err(_) => {
                client_ip = None;
                break;
            }
        }
    }
    let https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("https"));
    ClientInfo {
        ip: client_ip,
        https,
    }
}

/// 自分自身の origin（`scheme://host[:port]`）。Host は「自分がどう呼ばれたか」であり、
/// Origin ヘッダとの**完全一致の比較対象**としてのみ使う（Host 単独で許可判定はしない）
fn own_origin(req: &Request, client: &ClientInfo, trusted: bool) -> Option<String> {
    let headers = req.headers();
    let forwarded_host = if trusted {
        headers
            .get("x-forwarded-host")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
    } else {
        None
    };
    let host = match forwarded_host {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .or_else(|| req.uri().authority().map(|a| a.to_string()))?,
    };
    let scheme = if client.https { "https" } else { "http" };
    Some(format!("{scheme}://{}", host.to_ascii_lowercase()))
}

/// 変更系リクエストの CSRF 判定。`Origin` があれば自分自身と完全一致、
/// 無ければ `Sec-Fetch-Site` が `same-origin` / `none`。どちらも無ければ拒否
fn csrf_ok(req: &Request, client: &ClientInfo, trusted: bool) -> bool {
    let headers = req.headers();
    if let Some(origin) = headers.get(header::ORIGIN) {
        let Ok(origin) = origin.to_str() else {
            return false;
        };
        let Some(own) = own_origin(req, client, trusted) else {
            return false;
        };
        return origin.trim().to_ascii_lowercase() == own;
    }
    headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| matches!(v.trim(), "same-origin" | "none"))
}

fn is_mutating(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

/// trusted_cidrs が認証なしで通る route allowlist（D-27）。GET / HEAD のみ
fn is_allowlisted(method: &Method, path: &str) -> bool {
    if !matches!(*method, Method::GET | Method::HEAD) {
        return false;
    }
    let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    matches!(
        seg.as_slice(),
        ["api", "stream", id] | ["api", "artwork", id] | ["api", "tracks", id]
            if !id.is_empty()
    ) || matches!(seg.as_slice(), ["api", "playlists", id, "export"] if !id.is_empty())
}

fn session_cookie_value(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE_NAME)
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn token_hash_of(cookie_value: &str) -> Option<[u8; 32]> {
    let raw = BASE64.decode(cookie_value).ok()?;
    if raw.len() != 32 {
        return None;
    }
    Some(Sha256::digest(&raw).into())
}

async fn lookup_session(db: &Db, token_hash: [u8; 32]) -> Result<Option<Session>, DbError> {
    db.read(move |c| {
        // 不在（NoRows）だけを None にし、それ以外の DB エラーは 500 として上げる
        let expires_at: Option<i64> = c
            .query_row(
                "SELECT expires_at FROM sessions WHERE token_hash = ?1 AND expires_at > ?2",
                (token_hash.as_slice(), now_epoch()),
                |r| r.get(0),
            )
            .optional()?;
        Ok(expires_at.map(|expires_at| Session {
            token_hash,
            expires_at,
        }))
    })
    .await
}

// ---------------------------------------------------------------- ミドルウェア

/// `/health` 以外の全ルートが通る。ロックモード → 503、SPA は素通し、変更系は CSRF、
/// それ以外はセッション必須（trusted_cidrs の allowlist だけ例外）
pub async fn guard(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    if state.auth.mode == Mode::Locked {
        return error_response_with_message(
            StatusCode::SERVICE_UNAVAILABLE,
            "locked",
            "パスワードが未設定です。環境変数 SPINDLE_INITIAL_PASSWORD を設定して起動し直してください",
        );
    }

    let socket = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let trusted_proxy = socket.is_some_and(|s| {
        state
            .config
            .auth
            .trusted_proxies
            .iter()
            .any(|net| net.contains(&s.ip()))
    });
    let client = resolve_client(&state, req.headers(), socket);
    req.extensions_mut().insert(client.clone());

    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let is_api = path == "/api" || path.starts_with("/api/");

    // SPA（ログイン画面を含む）はセッション不要。ロックモードの判定だけ上で済んでいる
    if !is_api && matches!(method, Method::GET | Method::HEAD) {
        return next.run(req).await;
    }

    if is_mutating(&method) && !csrf_ok(&req, &client, trusted_proxy) {
        return error_response(StatusCode::FORBIDDEN, "csrf");
    }

    if path == "/api/auth/login" && method == Method::POST {
        return next.run(req).await;
    }

    if let Some(token_hash) = session_cookie_value(req.headers()).and_then(|v| token_hash_of(&v)) {
        match lookup_session(&state.db, token_hash).await {
            Ok(Some(session)) => {
                req.extensions_mut().insert(session);
                return next.run(req).await;
            }
            Ok(None) => {}
            Err(e) => return ApiError::from(e).into_response(),
        }
    }

    let in_trusted_cidr = client.ip.is_some_and(|ip| {
        state
            .config
            .auth
            .trusted_cidrs
            .iter()
            .any(|net| net.contains(&ip))
    });
    if in_trusted_cidr && is_allowlisted(&method, &path) {
        return next.run(req).await;
    }

    error_response(StatusCode::UNAUTHORIZED, "unauthenticated")
}

// ---------------------------------------------------------------- ハンドラ

#[derive(Deserialize)]
pub struct LoginBody {
    pub password: String,
}

#[derive(Serialize)]
pub struct SessionBody {
    pub expires_at: i64,
}

fn set_cookie_header(value: &str, max_age: i64, https: bool) -> String {
    let secure = if https { "; Secure" } else { "" };
    format!("{COOKIE_NAME}={value}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age}{secure}")
}

/// `POST /api/auth/login`
pub async fn login(
    State(state): State<AppState>,
    Extension(client): Extension<ClientInfo>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> Result<Response, ApiError> {
    let ip = client
        .ip
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    // 検証（argon2id は数十 ms かかる）に入る前に枠を予約する。並行送信で上限を超えさせない
    if state.auth.reserve_attempt(ip) == Reservation::Limited {
        return Ok(error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
        ));
    }

    let hash: Option<String> = state
        .db
        .read(|c| {
            Ok(
                c.query_row("SELECT password_hash FROM auth WHERE id = 1", [], |r| {
                    r.get(0)
                })
                .optional()?,
            )
        })
        .await?;
    let Some(hash) = hash else {
        return Ok(error_response(StatusCode::SERVICE_UNAVAILABLE, "locked"));
    };
    let password = body.password;
    let ok = tokio::task::spawn_blocking(move || verify_password(&password, &hash))
        .await
        .map_err(DbError::from)?;
    if !ok {
        // 予約した枠はそのまま失敗として残る
        tracing::warn!(%ip, "ログイン失敗");
        return Ok(error_response(StatusCode::UNAUTHORIZED, "invalid_password"));
    }
    state.auth.clear_attempts(ip);

    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| ApiError::Internal(format!("乱数の取得に失敗: {e}")))?;
    let token_hash: [u8; 32] = Sha256::digest(raw).into();
    let now = now_epoch();
    let max_age = i64::from(state.config.auth.session_days) * 86_400;
    let expires_at = now + max_age;
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().take(512).collect::<String>());
    state
        .db
        .write(move |c| {
            c.execute("DELETE FROM sessions WHERE expires_at <= ?1", [now])?;
            c.execute(
                "INSERT INTO sessions (token_hash, created_at, expires_at, user_agent) VALUES (?1, ?2, ?3, ?4)",
                (token_hash.as_slice(), now, expires_at, user_agent),
            )?;
            Ok(())
        })
        .await?;
    tracing::info!(%ip, "ログイン成功");

    let cookie = set_cookie_header(&BASE64.encode(raw), max_age, client.https);
    Ok((
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(SessionBody { expires_at }),
    )
        .into_response())
}

/// `POST /api/auth/logout`
pub async fn logout(
    State(state): State<AppState>,
    Extension(client): Extension<ClientInfo>,
    Extension(session): Extension<Session>,
) -> Result<Response, ApiError> {
    let token_hash = session.token_hash;
    state
        .db
        .write(move |c| {
            c.execute(
                "DELETE FROM sessions WHERE token_hash = ?1",
                [token_hash.as_slice()],
            )?;
            Ok(())
        })
        .await?;
    let cookie = set_cookie_header("", 0, client.https);
    Ok((StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response())
}

/// `GET /api/auth/session`
pub async fn session(Extension(session): Extension<Session>) -> Json<SessionBody> {
    Json(SessionBody {
        expires_at: session.expires_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u32) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::from(0x0a00_0000 + n))
    }

    #[test]
    fn reserve_attempt_grants_up_to_max_then_limits() {
        let shared = Shared::new(Mode::Unlocked);
        for _ in 0..LOGIN_MAX_FAILURES {
            assert_eq!(shared.reserve_attempt(ip(1)), Reservation::Granted);
        }
        assert_eq!(shared.reserve_attempt(ip(1)), Reservation::Limited);
        assert_eq!(
            shared.reserve_attempt(ip(2)),
            Reservation::Granted,
            "別 IP は独立"
        );
        shared.clear_attempts(ip(1));
        assert_eq!(
            shared.reserve_attempt(ip(1)),
            Reservation::Granted,
            "成功でリセット"
        );
    }

    #[test]
    fn limiter_table_is_bounded() {
        let shared = Shared::new(Mode::Unlocked);
        for n in 0..(LIMITER_MAX_ENTRIES as u32 * 2) {
            assert_eq!(shared.reserve_attempt(ip(n)), Reservation::Granted);
        }
        assert!(
            shared.tracked_ips() <= LIMITER_MAX_ENTRIES,
            "{}",
            shared.tracked_ips()
        );
        // 追跡中の IP は上限まで到達すれば引き続き制限される
        let last = ip(LIMITER_MAX_ENTRIES as u32 * 2 - 1);
        for _ in 1..LOGIN_MAX_FAILURES {
            assert_eq!(shared.reserve_attempt(last), Reservation::Granted);
        }
        assert_eq!(shared.reserve_attempt(last), Reservation::Limited);
    }
}

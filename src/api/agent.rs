//! エージェント API（`/api/agent/*`、仕様「認証と境界」、D-99）。
//! セッション Cookie・CSRF・trusted_cidrs のどれでも通らない。コードとトークンはログに出さない

use agent_proto::{PairRequest, PairResponse};
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

use super::auth::{self, verify_bytes};
use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;
use crate::db::devices::{self as dbdev, PairReserve};
use crate::db::{now_epoch, DbError};
use crate::device::credential::{
    self, PAIR_SECRET_BYTES, PAIR_SELECTOR_BYTES, TOKEN_SECRET_BYTES, TOKEN_SELECTOR_BYTES,
};

/// 報告（`/report`・`/plans/{id}/abandon`）の本文の上限
pub const REPORT_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// Bearer で確定した端末。ハンドラは `Extension<AgentDevice>` で受け取る
#[derive(Debug, Clone)]
pub struct AgentDevice {
    pub id: i64,
    pub uuid: String,
    pub name: String,
}

/// `/api/agent/*`（pair を除く）の Router。`guard` を route_layer で載せる
pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/manifest", get(manifest))
        .route("/files/{track_id}", get(file))
        .route("/plans", post(confirm))
        .route("/plans/open", get(open_plan))
        .route(
            "/plans/{id}/abandon",
            post(abandon).layer(DefaultBodyLimit::max(REPORT_BODY_LIMIT)),
        )
        .route(
            "/report",
            post(report).layer(DefaultBodyLimit::max(REPORT_BODY_LIMIT)),
        )
        .fallback(not_found)
        .route_layer(middleware::from_fn_with_state(state, guard))
}

/// ロックモード（パスワード未設定）なら 503。pair を含むエージェントの全経路で使う
fn locked(state: &AppState) -> Option<Response> {
    (state.auth.mode == auth::Mode::Locked)
        .then(|| error_response(StatusCode::SERVICE_UNAVAILABLE, "locked"))
}

/// `pair` だけに載せる層。pair は Bearer を要らないが、ロックモードでは閉じる
pub async fn locked_guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if let Some(r) = locked(&state) {
        return r;
    }
    next.run(req).await
}

/// `/api/agent/*`（pair を除く）の認証。`Authorization: Bearer <selector>.<secret>` だけを見る。
/// セッション Cookie・CSRF・trusted_cidrs は見ない
pub async fn guard(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    if let Some(r) = locked(&state) {
        return r;
    }
    let Some(text) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
    else {
        return unauthorized();
    };
    let Some((selector, secret)) =
        credential::split(text, TOKEN_SELECTOR_BYTES, TOKEN_SECRET_BYTES)
    else {
        return unauthorized();
    };
    let found = match state
        .db
        .read(move |c| dbdev::agent_by_selector(c, &selector))
        .await
    {
        Ok(f) => f,
        Err(e) => return ApiError::from(e).into_response(),
    };
    let Some(a) = found else {
        return unauthorized();
    };
    if !credential::ct_eq(
        credential::token_hash(&secret).as_bytes(),
        a.secret_hash.as_bytes(),
    ) {
        tracing::warn!(
            device_id = a.device_id,
            "エージェントのトークンが一致しない"
        );
        return unauthorized();
    }
    req.extensions_mut().insert(AgentDevice {
        id: a.device_id,
        uuid: a.uuid,
        name: a.name,
    });
    next.run(req).await
}

fn unauthorized() -> Response {
    let mut r = error_response(StatusCode::UNAUTHORIZED, "unauthenticated");
    r.headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    r
}

async fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not_found")
}

// 以下は仮のハンドラ（Task 5〜8 で置き換える）
async fn manifest() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

async fn file() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

async fn confirm() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

async fn open_plan() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

async fn abandon() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

async fn report() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

fn bad_request(msg: impl Into<String>) -> Response {
    error_response_with_message(StatusCode::BAD_REQUEST, "bad_request", msg)
}

fn invalid_code() -> Response {
    error_response(StatusCode::UNAUTHORIZED, "invalid_code")
}

/// `POST /api/agent/pair`: ワンタイムコードを消費してトークンを返す
pub async fn pair(
    State(state): State<AppState>,
    body: Result<Json<PairRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Ok(Json(body)) = body else {
        return Ok(bad_request("本文が不正"));
    };
    let Some((selector, secret)) =
        credential::split(body.code.trim(), PAIR_SELECTOR_BYTES, PAIR_SECRET_BYTES)
    else {
        return Ok(invalid_code());
    };
    let now = now_epoch();
    let sel = selector.clone();
    let reserved = state
        .db
        .write(move |c| dbdev::reserve_pair_attempt(c, &sel, now))
        .await?;
    let PairReserve::Reserved {
        device_id,
        code_hash,
    } = reserved
    else {
        return Ok(invalid_code());
    };
    let h = code_hash.clone();
    let ok = tokio::task::spawn_blocking(move || verify_bytes(&secret, &h))
        .await
        .map_err(DbError::from)?;
    if !ok {
        let sel = selector.clone();
        state
            .db
            .write(move |c| dbdev::fail_pair_attempt(c, device_id, &sel))
            .await?;
        tracing::warn!(device_id, "pair のコードが一致しない");
        return Ok(invalid_code());
    }
    let token = credential::issue(TOKEN_SELECTOR_BYTES, TOKEN_SECRET_BYTES)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let token_secret = credential::base32_decode(&token.secret)
        .ok_or_else(|| ApiError::Internal("トークンを作れない".into()))?;
    let (th, tsel) = (
        credential::token_hash(&token_secret),
        token.selector.clone(),
    );
    let consumed = state
        .db
        .write(move |c| {
            let tx = c.transaction()?;
            let ok = dbdev::consume_pair_code(
                &tx,
                device_id,
                &selector,
                &code_hash,
                &tsel,
                &th,
                now_epoch(),
            )?;
            let d = if ok {
                dbdev::get(&tx, device_id)?
            } else {
                None
            };
            tx.commit()?;
            Ok(d)
        })
        .await?;
    let Some(d) = consumed else {
        return Ok(invalid_code());
    };
    tracing::info!(device_id, "エージェントを pair した");
    Ok(Json(PairResponse {
        device_uuid: d.uuid,
        device_name: d.name,
        token: token.text(),
    })
    .into_response())
}

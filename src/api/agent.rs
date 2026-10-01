//! エージェント API（`/api/agent/*`、仕様「認証と境界」、D-99）。
//! セッション Cookie・CSRF・trusted_cidrs のどれでも通らない。コードとトークンはログに出さない

use agent_proto::{PairRequest, PairResponse};
use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

use super::auth::verify_bytes;
use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;
use crate::db::devices::{self as dbdev, PairReserve};
use crate::db::{now_epoch, DbError};
use crate::device::credential::{
    self, PAIR_SECRET_BYTES, PAIR_SELECTOR_BYTES, TOKEN_SECRET_BYTES, TOKEN_SELECTOR_BYTES,
};

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

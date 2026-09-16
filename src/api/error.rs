//! API 共通のエラー応答。本文は `{ "error": "<code>" }`（SPEC §9）

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::db::DbError;

#[derive(Serialize)]
pub struct ErrorBody {
    pub error: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// `(status, code)` の JSON 応答を作る
pub fn error_response(status: StatusCode, code: &'static str) -> Response {
    (
        status,
        Json(ErrorBody {
            error: code,
            message: None,
        }),
    )
        .into_response()
}

pub fn error_response_with_message(
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
) -> Response {
    (
        status,
        Json(ErrorBody {
            error: code,
            message: Some(message.into()),
        }),
    )
        .into_response()
}

/// ハンドラが `?` で返すエラー。内部エラーは詳細をログに出し、クライアントには code だけ返す
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("{0}")]
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        tracing::error!(error = %self, "API 内部エラー");
        error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal")
    }
}

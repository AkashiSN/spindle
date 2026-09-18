//! 編集履歴の API（SPEC §9 / §7.5「巻き戻し」/ §12.4、docs/TASKS.md P0-12）。
//!
//! - `GET /api/history`: バッチ一覧（新しい順）。state / affected / applied / conflict / failed /
//!   reverts_batch_id / reverted_by
//! - `GET /api/history/:id`: 上 + op 一覧（track_id / kind / result / error / edits）。
//!   `skipped_conflict` の op には編集キーの現在値（DB = ファイルの再読込結果）を `current` で付ける
//! - `POST /api/history/:id/revert { description? }`: 逆バッチを記録して 201。終端でなければ
//!   409 `not_terminal`、戻す op が残っていなければ 409 `already_reverted`、対象に反映待ちがあれば
//!   409 `pending`
//! - `POST /api/history/:id/cancel`: prepared / applying のバッチをキャンセルして 202。
//!   終端なら 409 `not_cancellable`

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db::history::{self, BatchSummary, OpKind, OpResult};
use crate::edit::{CancelOutcome, EditError, RevertError};

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;

#[derive(Serialize)]
pub struct HistoryList {
    pub items: Vec<BatchSummary>,
}

#[derive(Serialize)]
pub struct OpView {
    pub id: i64,
    pub track_id: i64,
    pub kind: OpKind,
    pub result: OpResult,
    pub error: Option<String>,
    /// トラックの現在の rel_path（削除済みなら None）
    pub rel_path: Option<String>,
    /// キー → { old, new }
    pub edits: serde_json::Map<String, serde_json::Value>,
    /// `skipped_conflict` の op だけ: 編集キーの現在値（キー → 値）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Serialize)]
pub struct HistoryDetail {
    #[serde(flatten)]
    pub batch: BatchSummary,
    pub ops: Vec<OpView>,
}

#[derive(Debug, Deserialize, Default)]
pub struct RevertBody {
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Serialize)]
pub struct RevertResponse {
    pub batch_id: i64,
    pub affected: usize,
    pub conflict: usize,
}

pub async fn list(State(state): State<AppState>) -> Result<Json<HistoryList>, ApiError> {
    let items = state.db.read(history::list_batches).await?;
    Ok(Json(HistoryList { items }))
}

pub async fn get(State(state): State<AppState>, Path(id): Path<i64>) -> Result<Response, ApiError> {
    let detail = state
        .db
        .read(move |c| {
            let Some(batch) = history::get_batch_summary(c, id)? else {
                return Ok(None);
            };
            let mut ops = Vec::new();
            for op in history::list_ops(c, id)? {
                let mut edits = serde_json::Map::new();
                let list = history::list_edits(c, op.id)?;
                for e in &list {
                    edits.insert(
                        e.key.clone(),
                        serde_json::json!({ "old": e.old_value, "new": e.new_value }),
                    );
                }
                let rel_path = history::track_rel_path(c, op.track_id)?;
                let current = if op.result == OpResult::SkippedConflict {
                    Some(current_values(c, &op, &list, rel_path.as_deref())?)
                } else {
                    None
                };
                ops.push(OpView {
                    id: op.id,
                    track_id: op.track_id,
                    kind: op.kind,
                    result: op.result,
                    error: op.error,
                    rel_path,
                    edits,
                    current,
                });
            }
            Ok(Some(HistoryDetail { batch, ops }))
        })
        .await?;
    Ok(match detail {
        Some(d) => Json(d).into_response(),
        None => error_response(StatusCode::NOT_FOUND, "not_found"),
    })
}

/// conflict の op について、編集キーの現在値（DB = ファイルの再読込結果）
fn current_values(
    c: &rusqlite::Connection,
    op: &history::Op,
    edits: &[history::Edit],
    rel_path: Option<&str>,
) -> crate::db::Result<serde_json::Map<String, serde_json::Value>> {
    let mut out = serde_json::Map::new();
    match op.kind {
        OpKind::Tags => {
            let tags = history::load_track_tags(c, op.track_id)?;
            for e in edits {
                let values: Vec<&str> = tags.values(&e.key).collect();
                let v = if values.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::json!(values)
                };
                out.insert(e.key.clone(), v);
            }
        }
        OpKind::Rename => {
            out.insert("rel_path".to_owned(), serde_json::json!(rel_path));
        }
        OpKind::Delete => {
            let missing: Option<i64> = c
                .query_row(
                    "SELECT missing_since FROM tracks WHERE id = ?1",
                    [op.track_id],
                    |r| r.get(0),
                )
                .unwrap_or(None);
            out.insert("missing_since".to_owned(), serde_json::json!(missing));
        }
        OpKind::Md5 => {
            let md5: Option<Vec<u8>> = c
                .query_row(
                    "SELECT audio_md5 FROM tracks WHERE id = ?1",
                    [op.track_id],
                    |r| r.get(0),
                )
                .unwrap_or(None);
            out.insert(
                "audio_md5".to_owned(),
                serde_json::json!(md5.map(|m| crate::edit::md5_hex(&m))),
            );
        }
        OpKind::Archive => {
            out.insert("rel_path".to_owned(), serde_json::json!(rel_path));
            let codec: Option<String> = c
                .query_row(
                    "SELECT codec FROM tracks WHERE id = ?1",
                    [op.track_id],
                    |r| r.get(0),
                )
                .unwrap_or(None);
            out.insert("codec".to_owned(), serde_json::json!(codec));
        }
    }
    Ok(out)
}

pub async fn revert(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    body: Result<Json<RevertBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let body = match body {
        Ok(Json(b)) => b,
        // 本文なし（Content-Type 無し）は既定値
        Err(JsonRejection::MissingJsonContentType(_)) => RevertBody::default(),
        Err(e) => {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                e.body_text(),
            ))
        }
    };
    let Some(editor) = state.editor.clone() else {
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "editor_unavailable",
        ));
    };
    match editor.revert_batch(id, body.description.as_deref()).await {
        Ok(p) => Ok((
            StatusCode::CREATED,
            Json(RevertResponse {
                batch_id: p.batch_id,
                affected: p.affected,
                conflict: p.conflict,
            }),
        )
            .into_response()),
        Err(RevertError::NotFound) => Ok(error_response(StatusCode::NOT_FOUND, "not_found")),
        Err(RevertError::NotTerminal) => Ok(error_response(StatusCode::CONFLICT, "not_terminal")),
        Err(RevertError::AlreadyReverted) => {
            Ok(error_response(StatusCode::CONFLICT, "already_reverted"))
        }
        Err(RevertError::Edit(EditError::Pending { track_ids })) => Ok((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "pending",
                "count": track_ids.len(),
                "track_ids": track_ids,
            })),
        )
            .into_response()),
        Err(RevertError::Edit(EditError::NoChanges)) => {
            Ok(error_response(StatusCode::CONFLICT, "no_changes"))
        }
        Err(e) => Err(ApiError::Internal(e.to_string())),
    }
}

pub async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let Some(editor) = state.editor.clone() else {
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "editor_unavailable",
        ));
    };
    Ok(
        match editor
            .cancel_batch(id)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?
        {
            CancelOutcome::Cancelled { .. } => StatusCode::ACCEPTED.into_response(),
            CancelOutcome::NotFound => error_response(StatusCode::NOT_FOUND, "not_found"),
            CancelOutcome::NotCancellable => {
                error_response(StatusCode::CONFLICT, "not_cancellable")
            }
        },
    )
}

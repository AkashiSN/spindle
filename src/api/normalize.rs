//! ロスレス → FLAC 正規化の API（SPEC §9 / §7.4、D-33 / D-45 / D-46、P1-4）。
//!
//! - `POST /api/normalize/preview { selection, sort? }`: selection を解決して固定し
//!   `selection_token` を返す。各行に宛先（拡張子を `.flac` にした同じパス）か、対象外・衝突の
//!   理由を返す。DB もファイルも書かない
//! - `POST /api/normalize/apply { selection_token, description?, skip_pending? }`: token の集合だけを
//!   対象に、計画を取り直して正規化バッチを記録する（track 単位の normalize ジョブを投入。
//!   `edit::Editor`）。反映待ちがあれば 409 `pending`、`skip_pending` で除外。token 不明・期限切れは
//!   409 `preview_stale`、対象が無ければ 409 `no_changes`。preview の後にタグが変わった行は
//!   `skipped_conflict` の op として記録する
//!
//! `[normalize].wav_to_flac = false` なら両方とも 409 `normalize_disabled`

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db::history::{self, Precondition};
use crate::db::tracks;
use crate::domain::filter::Sort;
use crate::domain::selection::{SelectionBody, Snapshot, SnapshotRow};
use crate::edit::{EditError, Editor, NormalizePlan, NormalizeTarget};

use super::error::{error_response, error_response_with_message, ApiError};
use super::selection;
use super::AppState;

/// スナップショットの `ops` に入れる目印（他の apply と token を取り違えない）
fn normalize_marker() -> serde_json::Value {
    serde_json::json!({ "normalize": true })
}

#[derive(Debug, Deserialize)]
pub struct PreviewBody {
    pub selection: SelectionBody,
    #[serde(default)]
    pub sort: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PreviewItem {
    pub id: i64,
    pub old: String,
    pub codec: String,
    /// 宛先。衝突なら null
    pub new: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PreviewResponse {
    pub selection_token: String,
    pub count: usize,
    pub changed: usize,
    /// 対象外（既に FLAC、非可逆）
    pub unchanged: usize,
    pub conflict: usize,
    pub pending_excluded: usize,
    pub items: Vec<PreviewItem>,
}

#[derive(Debug, Deserialize)]
pub struct ApplyBody {
    pub selection_token: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub skip_pending: bool,
}

#[derive(Debug, Serialize)]
pub struct ApplyResponse {
    pub batch_id: i64,
    pub affected: usize,
    pub conflict: usize,
}

fn bad_request(msg: impl Into<String>) -> Response {
    error_response_with_message(StatusCode::BAD_REQUEST, "bad_request", msg)
}

/// 正規化 API が使える条件（設定で有効、Editor あり）。使えなければ返す応答
fn editor_of(state: &AppState) -> Result<Arc<Editor>, Box<Response>> {
    if !state.config.normalize.wav_to_flac {
        return Err(Box::new(error_response(
            StatusCode::CONFLICT,
            "normalize_disabled",
        )));
    }
    state.editor.clone().ok_or_else(|| {
        Box::new(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "editor_unavailable",
        ))
    })
}

pub async fn preview(
    State(state): State<AppState>,
    body: Result<Json<PreviewBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return Ok(bad_request(e.body_text())),
    };
    let editor = match editor_of(&state) {
        Ok(e) => e,
        Err(r) => return Ok(*r),
    };
    let sort = match body.sort.as_deref() {
        None => Sort::default(),
        Some(s) => match Sort::parse(s) {
            Ok(s) => s,
            Err(e) => return Ok(bad_request(e.to_string())),
        },
    };
    let sel = match body.selection.parse() {
        Ok(s) => s,
        Err(e) => return Ok(bad_request(e.to_string())),
    };
    let (rows, pending) = state
        .db
        .read(move |c| {
            let tx = c.unchecked_transaction()?;
            let rows = tracks::resolve_selection_sorted(&tx, &sel, Some(sort))?;
            let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
            let pending = history::pending_track_ids(&tx, &ids)?;
            tx.finish()?;
            Ok((rows, pending))
        })
        .await?;
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| r.id)
        .filter(|id| pending.binary_search(id).is_err())
        .collect();
    let planned = editor
        .plan_normalize(&ids)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut items = Vec::new();
    let mut unchanged = 0usize;
    let mut conflict = 0usize;
    for p in planned {
        match p.planned {
            NormalizePlan::Path(new) => items.push(PreviewItem {
                id: p.track_id,
                old: p.current_rel_path,
                codec: p.codec,
                new: Some(new),
                reason: None,
            }),
            NormalizePlan::Unchanged => unchanged += 1,
            NormalizePlan::Conflict(reason) => {
                conflict += 1;
                items.push(PreviewItem {
                    id: p.track_id,
                    old: p.current_rel_path,
                    codec: p.codec,
                    new: None,
                    reason: Some(reason),
                });
            }
        }
    }
    let count = rows.len();
    let token = state
        .selection
        .insert(Snapshot {
            rows,
            ops: normalize_marker(),
        })
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(PreviewResponse {
        selection_token: token,
        count,
        changed: items.len() - conflict,
        unchanged,
        conflict,
        pending_excluded: pending.len(),
        items,
    })
    .into_response())
}

pub async fn apply(
    State(state): State<AppState>,
    body: Result<Json<ApplyBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return Ok(bad_request(e.body_text())),
    };
    let editor = match editor_of(&state) {
        Ok(e) => e,
        Err(r) => return Ok(*r),
    };
    let Some(snapshot) = selection::claim(&state.selection, &body.selection_token) else {
        return Ok(error_response(StatusCode::CONFLICT, "preview_stale"));
    };
    let token = body.selection_token.clone();
    let result = apply_claimed(&state, &editor, snapshot, &body).await;
    match &result {
        Ok(r) if r.status() == StatusCode::CREATED => selection::finish(&state.selection, &token),
        _ => selection::release(&state.selection, &token),
    }
    result
}

fn pending_response(track_ids: Vec<i64>) -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "error": "pending",
            "count": track_ids.len(),
            "track_ids": track_ids,
        })),
    )
        .into_response()
}

fn precondition_of(r: &SnapshotRow) -> Precondition {
    Precondition {
        dev: r.dev,
        inode: r.inode,
        size: Some(r.size),
        mtime_ns: Some(r.mtime_ns),
        ctime_ns: Some(r.ctime_ns),
        tag_hash: r.tag_hash.clone(),
        rel_path: Some(r.rel_path.clone()),
    }
}

async fn apply_claimed(
    state: &AppState,
    editor: &Arc<Editor>,
    snapshot: Snapshot,
    body: &ApplyBody,
) -> Result<Response, ApiError> {
    if snapshot.ops != normalize_marker() {
        return Ok(error_response(StatusCode::CONFLICT, "preview_stale"));
    }
    let ids: Vec<i64> = snapshot.rows.iter().map(|r| r.id).collect();
    let pending = state
        .db
        .read(move |c| history::pending_track_ids(c, &ids))
        .await?;
    if !pending.is_empty() && !body.skip_pending {
        return Ok(pending_response(pending));
    }
    let rows: Vec<&SnapshotRow> = snapshot
        .rows
        .iter()
        .filter(|r| pending.binary_search(&r.id).is_err())
        .collect();
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    // 計画は apply 時点の DB で取り直す（preview の後の変更は事前条件の tag_hash で conflict になる）
    let planned = editor
        .plan_normalize(&ids)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut targets = Vec::new();
    for (row, p) in rows.iter().zip(planned) {
        let (new_rel_path, planned_conflict) = match p.planned {
            NormalizePlan::Path(new) => (new, None),
            NormalizePlan::Unchanged => continue,
            // 衝突は skipped_conflict の op として記録する（理由を履歴に残す）
            NormalizePlan::Conflict(reason) => (row.rel_path.clone(), Some(reason)),
        };
        targets.push(NormalizeTarget {
            track_id: row.id,
            new_rel_path,
            new_codec: "flac".to_owned(),
            expected: Some(precondition_of(row)),
            planned_conflict,
        });
    }
    match editor
        .prepare_normalize(body.description.as_deref(), targets)
        .await
    {
        Ok(p) => Ok((
            StatusCode::CREATED,
            Json(ApplyResponse {
                batch_id: p.batch_id,
                affected: p.affected,
                conflict: p.conflict,
            }),
        )
            .into_response()),
        Err(EditError::NoChanges) => Ok(error_response(StatusCode::CONFLICT, "no_changes")),
        Err(EditError::Pending { track_ids }) => Ok(pending_response(track_ids)),
        Err(e) => Err(ApiError::Internal(e.to_string())),
    }
}

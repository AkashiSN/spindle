//! 一括リネームの API（SPEC §9 / §7.5 / §5、D-33、P0-11）。
//!
//! - `POST /api/rename/preview { selection, sort? }`: selection を解決して固定し
//!   `selection_token` を返す。各行に `[layout]` のテンプレートを適用した宛先（`new`）と、
//!   衝突・生成不能の理由（`reason`）を返す。変更なし・反映待ちの行は件数だけ。
//!   DB もファイルも書かない
//! - `POST /api/rename/apply { selection_token, description?, skip_pending? }`: token の
//!   集合だけを対象に、計画を取り直してリネームバッチを記録する（DB 先行更新 → rename ジョブ
//!   投入。`edit::Editor`）。反映待ちがあれば 409 `pending`、`skip_pending` で除外。
//!   token 不明・期限切れは 409 `preview_stale`、変更が無ければ 409 `no_changes`。
//!   preview の後にタグが変わった行（`tag_hash` 不一致）は `skipped_conflict` の op として記録する
//!
//! テンプレートはリクエストでは変えられない（`config.toml` の `[layout]` が唯一の規則）

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
use crate::domain::pathgen::Planned;
use crate::domain::selection::{SelectionBody, Snapshot, SnapshotRow};
use crate::edit::{EditError, Editor, RenameTarget};

use super::error::{error_response, error_response_with_message, ApiError};
use super::selection;
use super::AppState;

/// スナップショットの `ops` に入れる目印（tags の apply と token を取り違えない）
fn rename_marker() -> serde_json::Value {
    serde_json::json!({ "rename": true })
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
    /// 宛先。衝突・生成不能なら null
    pub new: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PreviewResponse {
    pub selection_token: String,
    pub count: usize,
    pub changed: usize,
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

pub async fn preview(
    State(state): State<AppState>,
    body: Result<Json<PreviewBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return Ok(bad_request(e.body_text())),
    };
    let Some(editor) = state.editor.clone() else {
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "editor_unavailable",
        ));
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
        .plan_rename(&ids, &state.config.layout)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut items = Vec::new();
    let mut unchanged = 0usize;
    let mut conflict = 0usize;
    for p in planned {
        match p.planned {
            Planned::Path(new) => items.push(PreviewItem {
                id: p.track_id,
                old: p.current_rel_path,
                new: Some(new.as_str().to_owned()),
                reason: None,
            }),
            Planned::Unchanged => unchanged += 1,
            Planned::Conflict(reason) => {
                conflict += 1;
                items.push(PreviewItem {
                    id: p.track_id,
                    old: p.current_rel_path,
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
            ops: rename_marker(),
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
    let Some(editor) = state.editor.clone() else {
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "editor_unavailable",
        ));
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
    if snapshot.ops != rename_marker() {
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
        .plan_rename(&ids, &state.config.layout)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut targets = Vec::new();
    for (row, p) in rows.iter().zip(planned) {
        let new_rel_path = match p.planned {
            Planned::Path(new) => new.as_str().to_owned(),
            Planned::Unchanged => continue,
            // 衝突は skipped_conflict の op として記録する（理由を履歴に残す）
            Planned::Conflict(reason) => {
                targets.push(RenameTarget {
                    track_id: row.id,
                    new_rel_path: row.rel_path.clone(),
                    expected: Some(precondition_of(row)),
                    planned_conflict: Some(reason),
                });
                continue;
            }
        };
        targets.push(RenameTarget {
            track_id: row.id,
            new_rel_path,
            expected: Some(precondition_of(row)),
            planned_conflict: None,
        });
    }
    match editor
        .prepare_rename(body.description.as_deref(), targets)
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

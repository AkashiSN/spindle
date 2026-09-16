//! 一括編集の API（SPEC §9 / §7.5 / §12.3、D-33 / D-42、P0-10）。
//!
//! - `POST /api/tracks/batch/preview { selection, ops, sort? }`: selection を解決して固定し
//!   `selection_token` を返す。各行に操作を評価した差分（changed）と、変更なし・反映待ちで
//!   除外した件数を返す。DB もファイルも書かない
//! - `PATCH /api/tracks/batch { selection_token, ops, description?, skip_pending? }`: token の
//!   集合だけを対象に編集バッチを記録する（1 トランザクションで記録 → DB 先行更新 →
//!   tagwrite 投入。`edit::Editor`）。反映待ちがあれば 409 `pending`、`skip_pending` で除外。
//!   token 不明・期限切れ・ops 不一致は 409 `preview_stale`、変更が無ければ 409 `no_changes`
//!
//! 操作の評価は preview と apply で同じ `domain::tagops::apply_ops` を通す。連番の位置は
//! 「解決した selection をソートしたときの位置」で、反映待ちの行も位置を消費する
//! （preview と apply で番号がずれない）

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
use crate::domain::tagops::{apply_ops, ops_to_json, parse_ops, Op};
use crate::edit::{EditError, Evaluator, PlanTarget};

use super::error::{error_response, error_response_with_message, ApiError};
use super::selection;
use super::AppState;

#[derive(Debug, Deserialize)]
pub struct PreviewBody {
    pub selection: SelectionBody,
    pub ops: serde_json::Value,
    /// 一覧と同じ `sort`（連番の順）。省略時は既定ソート
    #[serde(default)]
    pub sort: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PreviewItem {
    pub id: i64,
    /// キー → { old, new }（値は文字列配列か null）
    pub changes: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct PreviewResponse {
    pub selection_token: String,
    pub count: usize,
    pub changed: usize,
    pub unchanged: usize,
    pub pending_excluded: usize,
    pub items: Vec<PreviewItem>,
}

#[derive(Debug, Deserialize)]
pub struct ApplyBody {
    pub selection_token: String,
    pub ops: serde_json::Value,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub skip_pending: bool,
}

#[derive(Debug, Serialize)]
pub struct ApplyResponse {
    pub batch_id: i64,
    pub affected: usize,
}

fn bad_request(msg: impl Into<String>) -> Response {
    error_response_with_message(StatusCode::BAD_REQUEST, "bad_request", msg)
}

fn parse_sort(s: Option<&str>) -> Result<Sort, String> {
    match s {
        None => Ok(Sort::default()),
        Some(s) => Sort::parse(s).map_err(|e| e.to_string()),
    }
}

/// 差分（キー → { old, new }）
fn changes_of(
    current: &crate::domain::tags::TagSet,
    new: &crate::domain::tags::TagSet,
) -> serde_json::Map<String, serde_json::Value> {
    let mut keys: Vec<&str> = current
        .items()
        .iter()
        .chain(new.items().iter())
        .map(|(k, _)| k.as_str())
        .collect();
    keys.sort_unstable();
    keys.dedup();
    let json = |v: Vec<&str>| {
        if v.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!(v)
        }
    };
    let mut out = serde_json::Map::new();
    for key in keys {
        let old: Vec<&str> = current.values(key).collect();
        let new_v: Vec<&str> = new.values(key).collect();
        if old != new_v {
            out.insert(
                key.to_owned(),
                serde_json::json!({ "old": json(old), "new": json(new_v) }),
            );
        }
    }
    out
}

pub async fn preview(
    State(state): State<AppState>,
    body: Result<Json<PreviewBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return Ok(bad_request(e.body_text())),
    };
    let ops: Arc<Vec<Op>> = match parse_ops(&body.ops) {
        Ok(o) => Arc::new(o),
        Err(e) => return Ok(bad_request(e.to_string())),
    };
    let sort = match parse_sort(body.sort.as_deref()) {
        Ok(s) => s,
        Err(e) => return Ok(bad_request(e)),
    };
    let sel = match body.selection.parse() {
        Ok(s) => s,
        Err(e) => return Ok(bad_request(e.to_string())),
    };
    let ops_json = ops_to_json(&ops);
    let evaluated = state
        .db
        .read(move |c| {
            // selection の解決・pending・各行のタグを同じ読み取りスナップショットで読む
            // （途中でスキャンが commit しても行ごとに世代が混ざらない。WAL）
            let tx = c.unchecked_transaction()?;
            let rows = tracks::resolve_selection_sorted(&tx, &sel, Some(sort))?;
            let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
            let pending = history::pending_track_ids(&tx, &ids)?;
            let mut items = Vec::new();
            let mut unchanged = 0usize;
            for (index, row) in rows.iter().enumerate() {
                if pending.binary_search(&row.id).is_ok() {
                    continue;
                }
                let current = history::load_track_tags(&tx, row.id)?;
                let new = apply_ops(&ops, &current, index)
                    .map_err(|e| crate::db::DbError::Internal(e.to_string()))?;
                let changes = changes_of(&current, &new);
                if changes.is_empty() {
                    unchanged += 1;
                } else {
                    items.push(PreviewItem {
                        id: row.id,
                        changes,
                    });
                }
            }
            tx.finish()?;
            Ok((rows, pending.len(), unchanged, items))
        })
        .await?;
    let (rows, pending_excluded, unchanged, items) = evaluated;
    let count = rows.len();
    let token = state
        .selection
        .insert(Snapshot {
            rows,
            ops: ops_json,
        })
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(PreviewResponse {
        selection_token: token,
        count,
        changed: items.len(),
        unchanged,
        pending_excluded,
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
    let ops: Arc<Vec<Op>> = match parse_ops(&body.ops) {
        Ok(o) => Arc::new(o),
        Err(e) => return Ok(bad_request(e.to_string())),
    };
    // token は処理の間だけ占有する（並行する apply は 1 つだけ通る）。409 では release して
    // 同じ token でやり直せるようにし、201 で finish（消費）する。占有中・不明・期限切れは
    // preview_stale
    let Some(snapshot) = selection::claim(&state.selection, &body.selection_token) else {
        return Ok(error_response(StatusCode::CONFLICT, "preview_stale"));
    };
    let token = body.selection_token.clone();
    let result = apply_claimed(&state, &editor, snapshot, ops, &body).await;
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

/// token を占有した状態での apply 本体
async fn apply_claimed(
    state: &AppState,
    editor: &Arc<crate::edit::Editor>,
    snapshot: Snapshot,
    ops: Arc<Vec<Op>>,
    body: &ApplyBody,
) -> Result<Response, ApiError> {
    if snapshot.ops != ops_to_json(&ops) {
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
    // 事前条件は preview 時の snapshot（D-33）。preview の後にファイルが差し替えられていれば
    // tagwrite の事前条件確認が conflict にする。外部 rename の追随は D-41 の規則どおり
    let targets: Vec<PlanTarget> = snapshot
        .rows
        .iter()
        .enumerate()
        .filter(|(_, r)| pending.binary_search(&r.id).is_err())
        .map(|(index, r)| PlanTarget {
            track_id: r.id,
            expected_tag_version: Some(r.tag_version),
            index,
            expected: Some(precondition_of(r)),
            conflict: None,
        })
        .collect();
    let eval: Arc<Evaluator> = {
        let ops = Arc::clone(&ops);
        Arc::new(move |t: &PlanTarget, current| {
            apply_ops(&ops, current, t.index).map_err(|e| e.to_string())
        })
    };
    match editor
        .prepare_tags_with(body.description.as_deref(), targets, eval)
        .await
    {
        Ok(p) => Ok((
            StatusCode::CREATED,
            Json(ApplyResponse {
                batch_id: p.batch_id,
                affected: p.affected,
            }),
        )
            .into_response()),
        Err(EditError::NoChanges) => Ok(error_response(StatusCode::CONFLICT, "no_changes")),
        // 事前確認の後に pending が入った競合。Editor 側の再確認（同じ tx）で拾う
        Err(EditError::Pending { track_ids }) => Ok(pending_response(track_ids)),
        Err(e) => Err(ApiError::Internal(e.to_string())),
    }
}

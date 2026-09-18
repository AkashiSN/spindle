//! `POST /api/md5fill { selection, description?, skip_pending? }`（SPEC §7.9 / §9、P1-5b、D-59）。
//! selection の active な FLAC で `flac_check = 'md5_missing'` の行に md5 op の編集バッチを記録し、
//! tagwrite ジョブで STREAMINFO の MD5 を補填する（`Editor::prepare_md5_fill`。巻き戻せる）。
//! 値はデコードして決まるので preview 段階は無い。反映待ちがあれば 409 `pending`、`skip_pending`
//! で除外。対象が無ければ 409 `no_changes`。`[normalize].flac_fix_missing_md5 = false` なら
//! 409 `md5_fill_disabled`

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db::{history, tracks};
use crate::domain::selection::SelectionBody;
use crate::edit::{EditError, Editor};

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;

#[derive(Debug, Deserialize)]
pub struct FillBody {
    pub selection: SelectionBody,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub skip_pending: bool,
}

#[derive(Debug, Serialize)]
pub struct FillResponse {
    pub batch_id: i64,
    /// 記録した op 数
    pub affected: usize,
    /// FLAC でない・欠落・`md5_missing` でないため対象外の行数
    pub skipped: usize,
    /// `skip_pending` で除外した行数
    pub pending_excluded: usize,
}

fn editor_of(state: &AppState) -> Result<Arc<Editor>, Box<Response>> {
    if !state.config.normalize.flac_fix_missing_md5 {
        return Err(Box::new(error_response(
            StatusCode::CONFLICT,
            "md5_fill_disabled",
        )));
    }
    state.editor.clone().ok_or_else(|| {
        Box::new(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "editor_unavailable",
        ))
    })
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

pub async fn start(
    State(state): State<AppState>,
    body: Result<Json<FillBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                e.body_text(),
            ))
        }
    };
    let editor = match editor_of(&state) {
        Ok(e) => e,
        Err(r) => return Ok(*r),
    };
    let sel = match body.selection.parse() {
        Ok(s) => s,
        Err(e) => {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                e.to_string(),
            ))
        }
    };
    let (ids, pending) = state
        .db
        .read(move |c| {
            let tx = c.unchecked_transaction()?;
            let rows = tracks::resolve_selection(&tx, &sel)?;
            let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
            let pending = history::pending_track_ids(&tx, &ids)?;
            tx.finish()?;
            Ok((ids, pending))
        })
        .await?;
    if !pending.is_empty() && !body.skip_pending {
        return Ok(pending_response(pending));
    }
    let ids: Vec<i64> = ids
        .into_iter()
        .filter(|id| pending.binary_search(id).is_err())
        .collect();
    let pending_excluded = pending.len();
    match editor
        .prepare_md5_fill(body.description.as_deref(), ids)
        .await
    {
        Ok(p) => {
            let Some(batch_id) = p.batch_id else {
                return Ok(error_response(StatusCode::CONFLICT, "no_changes"));
            };
            Ok((
                StatusCode::CREATED,
                Json(FillResponse {
                    batch_id,
                    affected: p.affected,
                    skipped: p.skipped,
                    pending_excluded,
                }),
            )
                .into_response())
        }
        Err(EditError::NoChanges) => Ok(error_response(StatusCode::CONFLICT, "no_changes")),
        Err(EditError::Pending { track_ids }) => Ok(pending_response(track_ids)),
        Err(e) => Err(ApiError::Internal(e.to_string())),
    }
}

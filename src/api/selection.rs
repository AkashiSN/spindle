//! selection の解決とスナップショット（SPEC §9、D-33）。
//!
//! `POST /api/tracks/batch/preview`（P0-10）がここを呼んで `selection_token` を発行し、
//! `PATCH /api/tracks/batch` は token の集合だけを対象にする。エンドポイント自体は P0-10 で
//! 載せる。ここにあるのは両者が共有する解決・保存・取り出しだけ

use std::sync::Arc;

use crate::db::tracks;
use crate::domain::filter::FilterError;
use crate::domain::selection::{SelectionBody, SelectionStore, Snapshot, SnapshotRow, TokenError};

use super::error::ApiError;
use super::AppState;

#[derive(Debug, thiserror::Error)]
pub enum SelectionError {
    #[error(transparent)]
    Filter(#[from] FilterError),
    #[error(transparent)]
    Api(#[from] ApiError),
}

impl From<crate::db::DbError> for SelectionError {
    fn from(e: crate::db::DbError) -> Self {
        SelectionError::Api(e.into())
    }
}

impl From<TokenError> for SelectionError {
    fn from(e: TokenError) -> Self {
        SelectionError::Api(ApiError::Internal(e.to_string()))
    }
}

/// selection を現在の DB で解決する（スナップショットは取らない。件数表示などに使う）
pub async fn resolve(
    state: &AppState,
    body: SelectionBody,
) -> Result<Vec<SnapshotRow>, SelectionError> {
    let sel = body.parse()?;
    Ok(state
        .db
        .read(move |c| tracks::resolve_selection(c, &sel))
        .await?)
}

/// selection を解決して固定し、token を返す。preview の中核（D-33）
pub async fn snapshot(
    state: &AppState,
    body: SelectionBody,
    ops: serde_json::Value,
) -> Result<(String, Snapshot), SelectionError> {
    let rows = resolve(state, body).await?;
    let snapshot = Snapshot { rows, ops };
    let token = state.selection.insert(snapshot.clone())?;
    Ok((token, snapshot))
}

/// token の集合を参照する（消さない）。期限切れ・不明なら None
pub fn lookup(store: &Arc<SelectionStore>, token: &str) -> Option<Snapshot> {
    store.get(token)
}

/// token の集合を**消費**する。apply はこちらを使う（同じ token の並行 apply は 1 つだけ通る）。
/// 期限切れ・不明なら None（API は 409 `preview_stale`）
pub fn take(store: &Arc<SelectionStore>, token: &str) -> Option<Snapshot> {
    store.take(token)
}

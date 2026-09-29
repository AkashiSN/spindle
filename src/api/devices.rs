//! 端末 API（UI 向け。仕様「API の変更一覧」、D-95）

use crate::domain::device::PendingSets;
use crate::domain::filter::Filter;
use crate::playlist::dsl::Rule;

use super::error::ApiError;
use super::AppState;

/// フィルタが端末の状態を引くときだけ、スナップショットから未反映の集合を埋める
pub async fn attach_pending(state: &AppState, f: &mut Filter) -> Result<(), ApiError> {
    if f.uses_devices() {
        f.pending = state.db.device_snapshot().await?.pending_sets();
    }
    Ok(())
}

/// ルールの評価に渡す集合（端末のフィールドを使わないルールなら空）
pub async fn pending_for_rule(state: &AppState, rule: &Rule) -> Result<PendingSets, ApiError> {
    if rule.references_device_fields() {
        Ok(state.db.device_snapshot().await?.pending_sets())
    } else {
        Ok(PendingSets::default())
    }
}

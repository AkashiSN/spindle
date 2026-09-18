//! 巻き戻し（SPEC §7.5「巻き戻し」、§9 `/api/history/:batch/revert`、docs/TASKS.md P0-12、D-44）。
//!
//! 巻き戻しは**通常のバッチ**として記録し、`reverts_batch_id` で元を指す。
//!
//! - 対象は終端状態（applied / partial / failed / cancelled）のバッチだけ（`prepared` /
//!   `applying` は先にキャンセル）
//! - 対象 op 集合 = 元バッチで `applied` になった op − 既存の逆バッチで既に `applied` になった
//!   op（トラック単位。1 バッチ 1 トラック 1 op なので track_id で対応づく）。空なら
//!   `AlreadyReverted`
//! - 各 op は**全フィールドの現在値が元バッチの `new_value` と一致する場合だけ** `old_value` へ
//!   戻す。1 フィールドでも違えば `skipped_conflict` の op として記録だけする（外部変更または
//!   後続バッチの変更）
//! - `tags` は `prepare_tags_tx`、`rename` は `prepare_rename_tx` に乗る（DB 先行更新 + 子ジョブ。
//!   事前条件は記録時点の DB 値）。`delete` は DB だけの操作なので同じトランザクションで確定する
//! - 元バッチの `reverted_at` は、逆バッチが終端になり対象が全件 `applied` になったときだけ
//!   `aggregate_batch` が立てる（`history::mark_reverted_if_covered`）。やり直し = 逆バッチの revert
//! - 巻き戻しも通常のバッチなので、対象トラックに pending があれば `EditError::Pending`

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension as _};

use crate::db::archive::{self, ArchiveState};
use crate::db::history::{self, Op, OpKind, OpResult};
use crate::db::now_epoch;
use crate::domain::tags::TagSet;
use crate::jobs::Event;

use super::md5fill::{hex as md5_hex, md5_job, MD5_EDIT_KEY, MD5_ZERO_HEX};
use super::normalize::{prepare_normalize_tx, NormalizeTarget};
use super::rename::{prepare_rename_tx, RenameTarget};
use super::{
    batch_event, json_values, prepare_tags_tx, with_replaced, EditError, Editor, Evaluator,
    PlanTarget, Prepared,
};
use crate::db::jobs as dbjobs;

#[derive(Debug, thiserror::Error)]
pub enum RevertError {
    #[error("バッチが存在しない")]
    NotFound,
    #[error("バッチが終端状態ではない（先にキャンセルする）")]
    NotTerminal,
    #[error("戻す op が残っていない（全件戻し済み）")]
    AlreadyReverted,
    #[error(transparent)]
    Edit(#[from] EditError),
}

impl From<crate::db::DbError> for RevertError {
    fn from(e: crate::db::DbError) -> Self {
        RevertError::Edit(e.into())
    }
}

impl From<rusqlite::Error> for RevertError {
    fn from(e: rusqlite::Error) -> Self {
        RevertError::Edit(e.into())
    }
}

/// 巻き戻す op（元バッチの applied op と、その edits）
struct Target {
    op: Op,
    edits: Vec<history::Edit>,
}

/// 対象 op 集合を決める。空なら `AlreadyReverted`
fn targets_of(conn: &Connection, batch_id: i64) -> Result<Vec<Target>, RevertError> {
    let Some(batch) = history::get_batch(conn, batch_id)? else {
        return Err(RevertError::NotFound);
    };
    if !batch.state.is_terminal() {
        return Err(RevertError::NotTerminal);
    }
    let reverted: HashSet<i64> = history::reverted_track_ids(conn, batch_id)?
        .into_iter()
        .collect();
    let mut targets = Vec::new();
    for op in history::list_ops(conn, batch_id)? {
        if op.result != OpResult::Applied || reverted.contains(&op.track_id) {
            continue;
        }
        let edits = history::list_edits(conn, op.id)?;
        targets.push(Target { op, edits });
    }
    if targets.is_empty() {
        return Err(RevertError::AlreadyReverted);
    }
    Ok(targets)
}

/// [`Editor::revert_batch`] の本体（単一の書き込みコネクション上で、計画と記録を続けて行う）
fn revert_tx(
    conn: &mut Connection,
    batch_id: i64,
    description: Option<&str>,
    now: i64,
) -> Result<Prepared, RevertError> {
    let targets = targets_of(conn, batch_id)?;
    let kind = targets[0].op.kind;
    match kind {
        OpKind::Tags => revert_tags(conn, batch_id, description, targets, now),
        OpKind::Rename => revert_rename(conn, batch_id, description, targets, now),
        OpKind::Delete => revert_delete(conn, batch_id, description, targets, now),
        OpKind::Archive => revert_archive(conn, batch_id, description, targets, now),
        OpKind::Md5 => revert_md5(conn, batch_id, description, targets, now),
    }
}

/// MD5 補填の巻き戻し（P1-5b）: 逆向きの md5 op（old = 補填した値、new = 全ゼロ）を記録して
/// tagwrite ジョブを投入する。DB の `audio_md5` が元バッチの新値（補填した値）と違えば
/// conflict（外部変更または後続の補填）。`new_value` が入っているので反映はデコードせずそれを書く
fn revert_md5(
    conn: &mut Connection,
    batch_id: i64,
    description: Option<&str>,
    targets: Vec<Target>,
    now: i64,
) -> Result<Prepared, RevertError> {
    let tx = conn.transaction()?;
    let ids: Vec<i64> = targets.iter().map(|t| t.op.track_id).collect();
    let pending = history::pending_track_ids(&tx, &ids)?;
    if !pending.is_empty() {
        return Err(EditError::Pending { track_ids: pending }.into());
    }
    let new_batch =
        history::insert_batch(&tx, description, targets.len() as i64, Some(batch_id), now)?;
    let mut conflict = 0usize;
    let mut job_ids = Vec::new();
    for (ordinal, t) in targets.iter().enumerate() {
        let edit = t
            .edits
            .iter()
            .find(|e| e.key == MD5_EDIT_KEY)
            .ok_or_else(|| {
                EditError::Internal(format!("op {} に {MD5_EDIT_KEY} の edit が無い", t.op.id))
            })?;
        let expected = history::precondition_of_track(&tx, t.op.track_id)?
            .ok_or(EditError::TrackNotFound(t.op.track_id))?;
        let op_id = history::insert_op(
            &tx,
            new_batch,
            ordinal as i64,
            t.op.track_id,
            OpKind::Md5,
            &expected,
        )?;
        history::insert_edit(&tx, op_id, MD5_EDIT_KEY, &edit.new_value, &edit.old_value)?;
        // DB の現在値（ファイルのキャッシュ）が元バッチの新値と一致するときだけ戻す
        let current: Option<Vec<u8>> = tx
            .query_row(
                "SELECT audio_md5 FROM tracks WHERE id = ?1",
                [t.op.track_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let current_json = match current {
            Some(m) => serde_json::json!(md5_hex(&m)),
            None => serde_json::json!(MD5_ZERO_HEX),
        };
        if current_json != edit.new_value {
            conflict += 1;
            history::finish_op(
                &tx,
                op_id,
                OpResult::SkippedConflict,
                Some("現在値が元バッチの新値と違う（外部変更または後続の補填）: audio_md5"),
                None,
                now,
            )?;
            continue;
        }
        let job = md5_job(t.op.track_id, op_id, new_batch);
        let job_id = dbjobs::enqueue(&tx, &job, now)?.id();
        history::set_op_job(&tx, op_id, job_id)?;
        job_ids.push(job_id);
    }
    let affected = targets.len();
    let event = match history::aggregate_batch(&tx, new_batch, now)? {
        Some(state) => Some(batch_event(&tx, new_batch, state)?),
        None => None,
    };
    tx.commit()?;
    Ok(Prepared {
        batch_id: new_batch,
        affected,
        unchanged: 0,
        conflict,
        job_ids,
        event,
    })
}

/// ロスレス正規化の巻き戻し（SPEC §7.4、D-46）。同じ op 種別で向きを逆にする（`rel_path` 新→旧、
/// `codec` 新→旧）。復元元は Archive の held 行なので、GC 済み（deleted）や台帳に無いものは
/// conflict。Library の現在値（rel_path / codec）が元バッチの新値と違うものも conflict
fn revert_archive(
    conn: &mut Connection,
    batch_id: i64,
    description: Option<&str>,
    targets: Vec<Target>,
    now: i64,
) -> Result<Prepared, RevertError> {
    let mut plan: Vec<NormalizeTarget> = Vec::with_capacity(targets.len());
    for t in &targets {
        let edit_of = |key: &str| -> Result<(String, String), RevertError> {
            let e = t.edits.iter().find(|e| e.key == key).ok_or_else(|| {
                EditError::Internal(format!("op {} に {key} の edit が無い", t.op.id))
            })?;
            Ok((
                e.old_value.as_str().unwrap_or_default().to_owned(),
                e.new_value.as_str().unwrap_or_default().to_owned(),
            ))
        };
        let (old_path, new_path) = edit_of("rel_path")?;
        let (old_codec, new_codec) = edit_of("codec")?;
        let current: Option<(String, String)> = conn
            .query_row(
                "SELECT rel_path, codec FROM tracks WHERE id = ?1",
                [t.op.track_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((cur_path, cur_codec)) = current else {
            return Err(EditError::TrackNotFound(t.op.track_id).into());
        };
        let conflict = if cur_path != new_path || cur_codec != new_codec {
            Some(format!(
                "現在値が元バッチの新値と違う（外部変更または後続の編集）: {cur_path} ({cur_codec})"
            ))
        } else {
            match archive::get_by_rel_path(conn, &old_path)? {
                Some(a) if a.state == ArchiveState::Held => None,
                Some(a) => Some(format!(
                    "退避ファイルが Archive に無い（台帳 {}）: {old_path}",
                    a.state.as_str()
                )),
                None => Some(format!("退避ファイルが台帳に無い: {old_path}")),
            }
        };
        plan.push(NormalizeTarget {
            track_id: t.op.track_id,
            new_rel_path: old_path,
            new_codec: old_codec,
            expected: None,
            planned_conflict: conflict,
        });
    }
    Ok(prepare_normalize_tx(
        conn,
        description,
        &plan,
        Some(batch_id),
        now,
    )?)
}

fn revert_tags(
    conn: &mut Connection,
    batch_id: i64,
    description: Option<&str>,
    targets: Vec<Target>,
    now: i64,
) -> Result<Prepared, RevertError> {
    let mut plan: Vec<PlanTarget> = Vec::with_capacity(targets.len());
    let mut restore: HashMap<i64, Vec<(String, Vec<String>)>> = HashMap::new();
    for (index, t) in targets.iter().enumerate() {
        let current = history::load_track_tags(conn, t.op.track_id)?;
        let mismatched: Vec<&str> = t
            .edits
            .iter()
            .filter(|e| {
                let now: Vec<&str> = current.values(&e.key).collect();
                now != json_values(&e.new_value)
            })
            .map(|e| e.key.as_str())
            .collect();
        let conflict = (!mismatched.is_empty()).then(|| {
            format!(
                "現在値が元バッチの新値と違う（外部変更または後続の編集）: {}",
                mismatched.join(", ")
            )
        });
        restore.insert(
            t.op.track_id,
            t.edits
                .iter()
                .map(|e| (e.key.clone(), json_values(&e.old_value)))
                .collect(),
        );
        plan.push(PlanTarget {
            track_id: t.op.track_id,
            expected_tag_version: None,
            index,
            expected: None,
            conflict,
        });
    }
    let eval: Arc<Evaluator> = Arc::new(move |t: &PlanTarget, current: &TagSet| {
        let changes = restore.get(&t.track_id).cloned().unwrap_or_default();
        Ok(with_replaced(current, changes))
    });
    Ok(prepare_tags_tx(
        conn,
        description,
        &plan,
        &*eval,
        Some(batch_id),
        now,
    )?)
}

fn revert_rename(
    conn: &mut Connection,
    batch_id: i64,
    description: Option<&str>,
    targets: Vec<Target>,
    now: i64,
) -> Result<Prepared, RevertError> {
    let mut plan: Vec<RenameTarget> = Vec::with_capacity(targets.len());
    for t in &targets {
        let edit = t
            .edits
            .iter()
            .find(|e| e.key == "rel_path")
            .ok_or_else(|| {
                EditError::Internal(format!("op {} に rel_path の edit が無い", t.op.id))
            })?;
        let old = edit.old_value.as_str().unwrap_or_default().to_owned();
        let new = edit.new_value.as_str().unwrap_or_default().to_owned();
        let current = history::track_rel_path(conn, t.op.track_id)?
            .ok_or(EditError::TrackNotFound(t.op.track_id))?;
        let conflict = (current != new).then(|| {
            format!("現在のパスが元バッチの新値と違う（外部変更または後続の編集）: {current}")
        });
        plan.push(RenameTarget {
            track_id: t.op.track_id,
            new_rel_path: old,
            expected: None,
            planned_conflict: conflict,
        });
    }
    Ok(prepare_rename_tx(
        conn,
        description,
        &plan,
        Some(batch_id),
        now,
    )?)
}

/// 論理削除の巻き戻し。`missing_since` を戻すだけなので DB だけで完結し、逆バッチは即終端
fn revert_delete(
    conn: &mut Connection,
    batch_id: i64,
    description: Option<&str>,
    targets: Vec<Target>,
    now: i64,
) -> Result<Prepared, RevertError> {
    let tx = conn.transaction()?;
    let ids: Vec<i64> = targets.iter().map(|t| t.op.track_id).collect();
    let pending = history::pending_track_ids(&tx, &ids)?;
    if !pending.is_empty() {
        return Err(EditError::Pending { track_ids: pending }.into());
    }
    let new_batch =
        history::insert_batch(&tx, description, targets.len() as i64, Some(batch_id), now)?;
    let mut conflict = 0usize;
    for (ordinal, t) in targets.iter().enumerate() {
        let edit = t
            .edits
            .iter()
            .find(|e| e.key == "missing_since")
            .ok_or_else(|| {
                EditError::Internal(format!("op {} に missing_since の edit が無い", t.op.id))
            })?;
        let current: Option<i64> = tx
            .query_row(
                "SELECT missing_since FROM tracks WHERE id = ?1",
                [t.op.track_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let expected = history::precondition_of_track(&tx, t.op.track_id)?
            .ok_or(EditError::TrackNotFound(t.op.track_id))?;
        let op_id = history::insert_op(
            &tx,
            new_batch,
            ordinal as i64,
            t.op.track_id,
            OpKind::Delete,
            &expected,
        )?;
        history::insert_edit(
            &tx,
            op_id,
            "missing_since",
            &edit.new_value,
            &edit.old_value,
        )?;
        let current_json = match current {
            Some(v) => serde_json::json!(v),
            None => serde_json::Value::Null,
        };
        if current_json != edit.new_value {
            conflict += 1;
            history::finish_op(
                &tx,
                op_id,
                OpResult::SkippedConflict,
                Some("現在値が元バッチの新値と違う（外部変更または後続の編集）: missing_since"),
                None,
                now,
            )?;
            continue;
        }
        let restored: Option<i64> = edit.old_value.as_i64();
        tx.execute(
            "UPDATE tracks SET missing_since = ?2 WHERE id = ?1",
            params![t.op.track_id, restored],
        )?;
        if restored.is_none() {
            tx.execute(
                "UPDATE albums SET missing_since = NULL
                 WHERE id = (SELECT album_id FROM tracks WHERE id = ?1)",
                [t.op.track_id],
            )?;
        }
        history::finish_op(&tx, op_id, OpResult::Applied, None, None, now)?;
    }
    let event = match history::aggregate_batch(&tx, new_batch, now)? {
        Some(state) => Some(batch_event(&tx, new_batch, state)?),
        None => None,
    };
    tx.commit()?;
    Ok(Prepared {
        batch_id: new_batch,
        affected: targets.len(),
        unchanged: 0,
        conflict,
        job_ids: Vec::new(),
        event,
    })
}

impl Editor {
    /// バッチを巻き戻す逆バッチを記録する（SPEC §7.5「巻き戻し」）。tags / rename は DB 先行更新
    /// + 子ジョブ、delete は即終端。元バッチの `reverted_at` は逆バッチの集計時に立つ
    pub async fn revert_batch(
        &self,
        batch_id: i64,
        description: Option<&str>,
    ) -> Result<Prepared, RevertError> {
        let description = description.map(str::to_owned);
        let prepared = self
            .db
            .write(move |c| Ok(revert_tx(c, batch_id, description.as_deref(), now_epoch())))
            .await??;
        self.jobs.notify_enqueued(&prepared.job_ids).await;
        if let Some(ev) = &prepared.event {
            self.jobs.publish(Event::Batch(ev.clone()));
        }
        tracing::info!(
            batch_id,
            reverse_batch_id = prepared.batch_id,
            affected = prepared.affected,
            conflict = prepared.conflict,
            "巻き戻しバッチを記録した"
        );
        Ok(prepared)
    }
}

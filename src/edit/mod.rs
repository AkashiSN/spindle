//! 編集バッチの coordinator（SPEC §7.5 / §8、D-24、docs/TASKS.md P0-9）。
//!
//! ```text
//! prepare_tags:  1 トランザクションで edit_batches(prepared) / edit_ops(pending) / edits を記録
//!                → DB を新値へ更新（overlay。tag_version はトラックごとに 1 回 ++）
//!                → track 単位の tagwrite ジョブを投入（edit_batch_id で紐づけ）
//! apply_op:      open した FD の fstat + 同じ FD のタグ読みで事前条件を確認
//!                  一致       → 親 dir に tmp（O_EXCL）→ コピー → 全フィールドを書く → fsync → rename
//!                               → 同じトランザクションで物理属性・tag_hash を追随、op を applied
//!                  不一致     → ファイルの全フィールドが新値なら applied として確定（クラッシュ前に
//!                               rename まで済んでいた op）。そうでなければ skipped_conflict。
//!                               ファイルは触らず、同じトランザクションで DB をファイルの現在値へ戻す
//! close_op:      cancel / 最終失敗で op を failed に閉じ、overlay を解消する
//! cancel_batch:  子ジョブへ cancel 要求、未着手の op を failed('cancelled')、進行中は完了を待つ
//! recover:       起動時。prepared / applying のバッチの pending op に track ジョブを再投入
//! ```
//!
//! バッチはジョブではない。終端状態は op の結果から集計し、最後に op を終端にした
//! トランザクションが `edit_batches` を終端へ進める（SPEC §8）。
//!
//! overlay の解消は「ファイルの現在値」が原則。ファイルを読めないとき（消えている・壊れている）
//! だけ、記録時点の旧値（`edits.old_value`）と事前条件の物理属性へ戻す。物理属性を記録時点へ
//! 戻すのは、その後ファイルが外部で変わっていればスキャンが差分として拾い直せるようにするため

mod rename;

use std::io::{Seek, SeekFrom};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use serde::Serialize;

use crate::db::history::{self, BatchState, Op, OpKind, OpResult, Precondition, CANCELLED_ERROR};
use crate::db::jobs as dbjobs;
use crate::db::scans::{self, CacheColumns, Fingerprint, Physical, TrackContent};
use crate::db::{now_epoch, Db, DbError};
use crate::domain::relpath::{RelPath, RelPathError};
use crate::domain::tags::{
    normalize_tags, read_audio_file, tag_hash, write_tag_changes, TagReadError, TagSet,
    TagWriteError,
};
use crate::fsroot::{self, FsError, RootDir};
use crate::import::scanner::{
    audio_changed, cache_columns, effective_fingerprint, read_fingerprint, track_content,
};
use crate::jobs::{BatchEvent, Event, JobState, JobType, Jobs, NewJob};

pub use crate::domain::tags::TagChange;
pub use rename::{
    in_progress_keys, rename_dedup_key, temp_rel_path, PlannedRename, RenameHook, RenameOutcome,
    RenameStep, RenameTarget,
};

/// 1 トラックへのタグ変更（`prepare_tags` の入力）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTagOp {
    pub track_id: i64,
    pub changes: Vec<TagChange>,
}

/// `prepare_tags` の結果
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Prepared {
    pub batch_id: i64,
    /// 記録した op 数（値が変わるトラック + preview 後に版が進んで conflict になったトラック）
    pub affected: usize,
    /// 全フィールドが現在値と同じで op にならなかったトラック数
    pub unchanged: usize,
    /// preview 後に版が進んでいたので `skipped_conflict` で記録した op 数
    pub conflict: usize,
    pub job_ids: Vec<i64>,
    /// 記録の時点で終端になった（全件 conflict）ときの batch イベント
    #[serde(skip)]
    pub event: Option<BatchEvent>,
}

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("反映待ちの op があるトラック: {track_ids:?}")]
    Pending { track_ids: Vec<i64> },
    #[error("変更が無い")]
    NoChanges,
    #[error("同じトラックが複数回指定された: {0}")]
    DuplicateTrack(i64),
    #[error("トラックが存在しない: {0}")]
    TrackNotFound(i64),
    #[error("op が存在しない: {0}")]
    OpNotFound(i64),
    #[error("この種別の op はまだ反映できない: {0:?}")]
    UnsupportedKind(OpKind),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("ファイル操作に失敗: {0}")]
    Fs(#[from] FsError),
    #[error("パスが不正: {0}")]
    RelPath(#[from] RelPathError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    TagRead(#[from] TagReadError),
    #[error(transparent)]
    TagWrite(#[from] TagWriteError),
    #[error("内部エラー: {0}")]
    Internal(String),
}

impl From<tokio::task::JoinError> for EditError {
    fn from(e: tokio::task::JoinError) -> Self {
        EditError::Internal(format!("ファイル操作タスクが異常終了: {e}"))
    }
}

/// `apply_op` の結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpOutcome {
    Applied,
    /// 事前条件不一致。ファイルは触っていない
    Conflict(String),
    /// 書けない（書き込み結果が意図と一致しない等）。ファイルは触っていない。op は failed
    Failed(String),
    /// 既に終端だった（何もしていない）
    AlreadyTerminal(OpResult),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelOutcome {
    NotFound,
    /// 既に終端
    NotCancellable,
    Cancelled {
        /// 未着手だったので failed('cancelled') に閉じた op 数
        ops_cancelled: usize,
        /// 進行中で完了を待つ op 数
        running: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecoverReport {
    /// track ジョブを再投入した op 数
    pub requeued: usize,
    /// ジョブが cancelled だったので閉じた op 数
    pub cancelled_ops: usize,
    /// pending が無いのに開いていたので集計したバッチ数
    pub aggregated: usize,
}

/// rename 直前に呼ばれるフック（テスト用。事前条件確認〜rename の窓で外部更新を起こす）
pub type BeforeRenameHook = Arc<dyn Fn(&str) + Send + Sync>;

pub struct Editor {
    db: Arc<Db>,
    root: Arc<RootDir>,
    jobs: Arc<Jobs>,
    before_rename: Mutex<Option<BeforeRenameHook>>,
    rename_hook: Mutex<Option<RenameHook>>,
}

/// tagwrite ジョブの dedup key（SPEC §8）
pub fn tagwrite_dedup_key(track_id: i64, tag_version: i64) -> String {
    format!("tagwrite:{track_id}:{tag_version}")
}

fn tagwrite_job(track_id: i64, tag_version: i64, op_id: i64, batch_id: i64) -> NewJob {
    NewJob::new(
        JobType::Tagwrite,
        serde_json::json!({
            "track_id": track_id,
            "tag_version": tag_version,
            "op_id": op_id,
            "batch_id": batch_id,
        }),
    )
    .dedup_key(tagwrite_dedup_key(track_id, tag_version))
    .edit_batch_id(batch_id)
}

/// JSON の値（文字列配列 / null）→ 値の列
fn json_values(v: &serde_json::Value) -> Vec<String> {
    match v {
        serde_json::Value::Array(a) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

fn values_json(values: &[String]) -> serde_json::Value {
    if values.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!(values)
    }
}

/// `set` の `key` を `values` に置き換えた集合
fn with_replaced(set: &TagSet, changes: impl IntoIterator<Item = (String, Vec<String>)>) -> TagSet {
    let mut items: Vec<(String, String)> = set.items().to_vec();
    for (key, values) in changes {
        items.retain(|(k, _)| *k != key);
        items.extend(values.into_iter().map(|v| (key.clone(), v)));
    }
    normalize_tags(items)
}

fn batch_event(
    conn: &Connection,
    batch_id: i64,
    state: BatchState,
) -> crate::db::Result<BatchEvent> {
    let c = history::batch_counts(conn, batch_id)?;
    Ok(BatchEvent {
        id: batch_id,
        state: state.as_str().to_owned(),
        applied: c.applied,
        conflict: c.conflict,
        failed: c.failed,
    })
}

/// ファイル側で確定した状態（stat + 読み取ったタグ）。`fp` は外部の実体を採用するときだけ
/// 計算する（自分の tagwrite の結果は音声不変が既知。SPEC §6）
struct FileState {
    ph: Physical,
    content: TrackContent,
    fp: Option<Fingerprint>,
}

impl FileState {
    /// 外部の実体（事前条件不一致・overlay 解消・cancel）として読む。フィンガープリントも計算する
    fn external(
        root: &RootDir,
        rel: &RelPath,
        st: fsroot::Stat,
        af: crate::domain::tags::AudioFile,
    ) -> Self {
        let fp = read_fingerprint(root, rel, &af);
        FileState {
            ph: st.into(),
            content: track_content(af),
            fp: Some(fp),
        }
    }
}

impl From<fsroot::Stat> for Physical {
    fn from(st: fsroot::Stat) -> Self {
        Physical {
            dev: st.dev,
            inode: st.inode,
            nlink: st.nlink,
            size: st.size,
            mtime_ns: st.mtime_ns,
            ctime_ns: st.ctime_ns,
        }
    }
}

/// `apply_op` のファイル側の結果
enum Staged {
    /// 事前条件一致 → 書き込み・rename 済み。DB を追随させる
    Written(FileState),
    /// 事前条件不一致だがファイルの全フィールドが新値 → applied として確定
    AlreadyMatches(FileState),
    /// 事前条件不一致 → conflict。`current` はファイルの現在値（読めなければ None）
    Conflict {
        reason: String,
        current: Option<FileState>,
    },
    /// 書けない（書き込み結果が意図と一致しない等）。ファイルは触っていない。再試行しても
    /// 直らないので op を failed に閉じる。`current` はファイルの現在値
    Failed {
        reason: String,
        current: Option<FileState>,
    },
}

impl Editor {
    pub fn new(db: Arc<Db>, root: Arc<RootDir>, jobs: Arc<Jobs>) -> Self {
        Self {
            db,
            root,
            jobs,
            before_rename: Mutex::new(None),
            rename_hook: Mutex::new(None),
        }
    }

    /// テスト用: 事前条件の確認後・rename の直前に呼ばれるフックを置く（引数は rel_path）
    #[doc(hidden)]
    pub fn set_before_rename_hook(&self, hook: BeforeRenameHook) {
        *self.before_rename.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    // ------------------------------------------------------------ prepare

    /// タグ編集バッチを記録し、DB を先行更新して track ジョブを投入する（SPEC §7.5）。
    /// 対象トラックに pending の op があれば何も記録せず [`EditError::Pending`]
    pub async fn prepare_tags(
        &self,
        description: Option<&str>,
        ops: Vec<NewTagOp>,
    ) -> Result<Prepared, EditError> {
        let targets: Vec<PlanTarget> = ops
            .iter()
            .enumerate()
            .map(|(index, o)| PlanTarget {
                track_id: o.track_id,
                expected_tag_version: None,
                index,
                expected: None,
            })
            .collect();
        let by_track: std::collections::HashMap<i64, Vec<TagChange>> =
            ops.into_iter().map(|o| (o.track_id, o.changes)).collect();
        let eval: Arc<Evaluator> = Arc::new(move |t: &PlanTarget, current: &TagSet| {
            let changes = by_track.get(&t.track_id).cloned().unwrap_or_default();
            Ok(with_replaced(
                current,
                changes.iter().map(|c| {
                    let c = c.normalized();
                    (c.key, c.values.unwrap_or_default())
                }),
            ))
        });
        self.prepare_tags_with(description, targets, eval).await
    }

    /// 対象と評価器からタグ編集バッチを記録する（`prepare_tags` の一般形。API の apply が
    /// `domain::tagops::apply_ops` を評価器にして使う）。`expected_tag_version` が現在値と
    /// 違う対象は `skipped_conflict` の op として記録だけする
    pub async fn prepare_tags_with(
        &self,
        description: Option<&str>,
        targets: Vec<PlanTarget>,
        eval: Arc<Evaluator>,
    ) -> Result<Prepared, EditError> {
        let description = description.map(str::to_owned);
        let prepared = self
            .db
            .write(move |c| {
                Ok(prepare_tags_tx(
                    c,
                    description.as_deref(),
                    &targets,
                    &*eval,
                    now_epoch(),
                ))
            })
            .await??;
        self.jobs.notify_enqueued(&prepared.job_ids).await;
        if let Some(ev) = &prepared.event {
            self.jobs.publish(Event::Batch(ev.clone()));
        }
        Ok(prepared)
    }

    // ------------------------------------------------------------ apply

    /// pending の op をファイルへ反映する（tagwrite ジョブの本体。何度呼んでも結果は同じ）。
    /// `job_id` は反映したジョブとして op に記録する
    pub async fn apply_op(&self, op_id: i64, job_id: Option<i64>) -> Result<OpOutcome, EditError> {
        let (op, edits, rel_path) = self.load_op(op_id).await?;
        if op.result != OpResult::Pending {
            return Ok(OpOutcome::AlreadyTerminal(op.result));
        }
        if op.kind != OpKind::Tags {
            return Err(EditError::UnsupportedKind(op.kind));
        }
        self.mark_applying(op.batch_id).await?;

        let staged = {
            let root = Arc::clone(&self.root);
            let op = op.clone();
            let edits = edits.clone();
            let hook = self
                .before_rename
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            tokio::task::spawn_blocking(move || {
                stage_tags(&root, &op, &edits, &rel_path, hook.as_ref())
            })
            .await??
        };

        let batch_id = op.batch_id;
        let (outcome, event, followup_jobs) = self
            .db
            .write(move |c| {
                let tx = c.transaction()?;
                let now = now_epoch();
                let mut followup_jobs: Vec<i64> = Vec::new();
                let outcome = match staged {
                    Staged::Written(fs) | Staged::AlreadyMatches(fs) => {
                        history::finish_op(&tx, op.id, OpResult::Applied, None, job_id, now)?;
                        sync_track_to_file(&tx, op.track_id, &fs)?;
                        if let Some(id) = enqueue_derived_retag(&tx, op.track_id, now)? {
                            followup_jobs.push(id);
                        }
                        OpOutcome::Applied
                    }
                    Staged::Conflict { reason, current } => {
                        if history::finish_op(
                            &tx,
                            op.id,
                            OpResult::SkippedConflict,
                            Some(&reason),
                            job_id,
                            now,
                        )? {
                            resolve_overlay(&tx, &op, &edits, current.as_ref())?;
                        }
                        OpOutcome::Conflict(reason)
                    }
                    Staged::Failed { reason, current } => {
                        if history::finish_op(
                            &tx,
                            op.id,
                            OpResult::Failed,
                            Some(&reason),
                            job_id,
                            now,
                        )? {
                            resolve_overlay(&tx, &op, &edits, current.as_ref())?;
                        }
                        OpOutcome::Failed(reason)
                    }
                };
                let event = match history::aggregate_batch(&tx, batch_id, now)? {
                    Some(state) => Some(batch_event(&tx, batch_id, state)?),
                    None => None,
                };
                tx.commit()?;
                Ok((outcome, event, followup_jobs))
            })
            .await?;
        self.jobs.notify_enqueued(&followup_jobs).await;
        if let Some(ev) = event {
            tracing::info!(batch_id, state = %ev.state, applied = ev.applied, conflict = ev.conflict, failed = ev.failed, "編集バッチが終端になった");
            self.jobs.publish(Event::Batch(ev));
        }
        Ok(outcome)
    }

    /// pending の op を `applied` 以外の終端（failed）に閉じ、overlay を解消する。
    /// cancel（未着手）、ハンドラの最終失敗、リカバリ（ジョブが cancelled だった）で使う。
    /// 既に終端なら何もせず `false`
    pub async fn close_op(
        &self,
        op_id: i64,
        error: &str,
        job_id: Option<i64>,
    ) -> Result<bool, EditError> {
        let (op, edits, rel_path) = self.load_op(op_id).await?;
        if op.result != OpResult::Pending {
            return Ok(false);
        }
        if op.kind == OpKind::Rename {
            // rename op はパスが互いに絡む（swap / 循環）ので、バッチの pending を一度に閉じる
            return Ok(self.close_rename_ops(op.batch_id, job_id, error).await? > 0);
        }
        let current = {
            let root = Arc::clone(&self.root);
            tokio::task::spawn_blocking(move || read_file_state(&root, &rel_path)).await?
        };
        let error = error.to_owned();
        let batch_id = op.batch_id;
        let (closed, event) = self
            .db
            .write(move |c| {
                let tx = c.transaction()?;
                let now = now_epoch();
                let closed =
                    history::finish_op(&tx, op.id, OpResult::Failed, Some(&error), job_id, now)?;
                if closed {
                    resolve_overlay(&tx, &op, &edits, current.as_ref())?;
                }
                let event = match history::aggregate_batch(&tx, batch_id, now)? {
                    Some(state) => Some(batch_event(&tx, batch_id, state)?),
                    None => None,
                };
                tx.commit()?;
                Ok((closed, event))
            })
            .await?;
        if let Some(ev) = event {
            self.jobs.publish(Event::Batch(ev));
        }
        Ok(closed)
    }

    // ------------------------------------------------------------ cancel

    /// バッチ単位のキャンセル（SPEC §7.5）。子ジョブに cancel 要求を立て、未着手の op は
    /// failed('cancelled') に閉じる。進行中の op は完了を待つ（そのジョブが集計する）
    pub async fn cancel_batch(&self, batch_id: i64) -> Result<CancelOutcome, EditError> {
        enum Step {
            NotFound,
            NotCancellable,
            Proceed {
                to_close: Vec<i64>,
                running: Vec<i64>,
                touched_jobs: Vec<i64>,
            },
        }
        let step = self
            .db
            .write(move |c| {
                let tx = c.transaction()?;
                let now = now_epoch();
                let Some(batch) = history::get_batch(&tx, batch_id)? else {
                    return Ok(Step::NotFound);
                };
                if batch.state.is_terminal() {
                    return Ok(Step::NotCancellable);
                }
                let mut touched_jobs = Vec::new();
                for job in dbjobs::active_jobs_of_batch(&tx, batch_id)? {
                    dbjobs::request_cancel(&tx, job.id, now)?;
                    touched_jobs.push(job.id);
                }
                let mut to_close = Vec::new();
                let mut running = Vec::new();
                for op in history::pending_ops(&tx, batch_id)? {
                    let state = match op.job_id {
                        Some(j) => dbjobs::get(&tx, j)?.map(|j| j.state),
                        None => None,
                    };
                    if state == Some(JobState::Running) {
                        running.push(op.job_id.unwrap_or_default());
                    } else {
                        to_close.push(op.id);
                    }
                }
                tx.commit()?;
                Ok(Step::Proceed {
                    to_close,
                    running,
                    touched_jobs,
                })
            })
            .await?;
        let (to_close, running, touched_jobs) = match step {
            Step::NotFound => return Ok(CancelOutcome::NotFound),
            Step::NotCancellable => return Ok(CancelOutcome::NotCancellable),
            Step::Proceed {
                to_close,
                running,
                touched_jobs,
            } => (to_close, running, touched_jobs),
        };
        for id in &running {
            self.jobs.cancel_running_token(*id);
        }
        self.jobs.notify_changed(&touched_jobs).await;
        let mut ops_cancelled = 0;
        for op_id in to_close {
            match self.load_op(op_id).await {
                // rename op は 1 回の呼び出しでバッチの pending を全件閉じる
                Ok((op, _, _)) if op.kind == OpKind::Rename => {
                    ops_cancelled += self
                        .close_rename_ops(op.batch_id, None, CANCELLED_ERROR)
                        .await?;
                }
                Ok(_) => {
                    if self.close_op(op_id, CANCELLED_ERROR, None).await? {
                        ops_cancelled += 1;
                    }
                }
                Err(EditError::OpNotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        if running.is_empty() {
            // 閉じる op が無かった（全て終端だった）場合も集計を通す
            self.aggregate(batch_id).await?;
        }
        tracing::info!(
            batch_id,
            ops_cancelled,
            running = running.len(),
            "編集バッチをキャンセルした"
        );
        Ok(CancelOutcome::Cancelled {
            ops_cancelled,
            running: running.len(),
        })
    }

    // ------------------------------------------------------------ recovery

    /// 起動時リカバリ（SPEC §7.5）。`jobs::recovery::run` の後、ワーカー起動の前に呼ぶ。
    /// prepared / applying のバッチの pending op について、ジョブが cancelled なら op を閉じ、
    /// ジョブが無い・終端なら track ジョブを再投入する（dedup で二重にはならない）。
    /// rename 済みだった op の確定は再投入されたジョブが `apply_op` で行う
    pub async fn recover(&self) -> Result<RecoverReport, EditError> {
        struct Plan {
            to_cancel: Vec<i64>,
            to_requeue: Vec<Op>,
            to_aggregate: Vec<i64>,
        }
        let plan = self
            .db
            .read(|c| {
                let mut plan = Plan {
                    to_cancel: Vec::new(),
                    to_requeue: Vec::new(),
                    to_aggregate: Vec::new(),
                };
                for batch_id in history::open_batch_ids(c)? {
                    let ops = history::pending_ops(c, batch_id)?;
                    if ops.is_empty() {
                        plan.to_aggregate.push(batch_id);
                        continue;
                    }
                    for op in ops {
                        let state = match op.job_id {
                            Some(j) => dbjobs::get(c, j)?.map(|j| j.state),
                            None => None,
                        };
                        match state {
                            Some(JobState::Cancelled) => plan.to_cancel.push(op.id),
                            Some(JobState::Queued | JobState::Running) => {}
                            Some(JobState::Done | JobState::Failed) | None => {
                                plan.to_requeue.push(op)
                            }
                        }
                    }
                }
                Ok(plan)
            })
            .await?;

        let mut report = RecoverReport::default();
        for op_id in plan.to_cancel {
            match self.load_op(op_id).await {
                Ok((op, _, _)) if op.kind == OpKind::Rename => {
                    report.cancelled_ops += self
                        .close_rename_ops(op.batch_id, None, CANCELLED_ERROR)
                        .await?;
                }
                Ok(_) => {
                    if self.close_op(op_id, CANCELLED_ERROR, None).await? {
                        report.cancelled_ops += 1;
                    }
                }
                Err(EditError::OpNotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        let to_requeue = plan.to_requeue;
        let (requeued, job_ids) = self
            .db
            .write(move |c| {
                let tx = c.transaction()?;
                let now = now_epoch();
                let mut job_ids = Vec::new();
                let mut requeued = 0;
                for op in &to_requeue {
                    let job = match op.kind {
                        OpKind::Tags => {
                            let Some(tag_version) = history::track_tag_version(&tx, op.track_id)?
                            else {
                                continue;
                            };
                            tagwrite_job(op.track_id, tag_version, op.id, op.batch_id)
                        }
                        // rename はバッチ 1 つに 1 ジョブ。dedup で 2 件目以降は Duplicate になる
                        OpKind::Rename => rename::rename_job(op.batch_id),
                        _ => continue,
                    };
                    match dbjobs::enqueue(&tx, &job, now)? {
                        dbjobs::EnqueueResult::Inserted(id) => {
                            history::set_op_job(&tx, op.id, id)?;
                            job_ids.push(id);
                            requeued += 1;
                        }
                        dbjobs::EnqueueResult::Duplicate(id) => {
                            history::set_op_job(&tx, op.id, id)?;
                        }
                    }
                }
                tx.commit()?;
                Ok((requeued, job_ids))
            })
            .await?;
        report.requeued = requeued;
        self.jobs.notify_enqueued(&job_ids).await;
        for batch_id in plan.to_aggregate {
            if self.aggregate(batch_id).await? {
                report.aggregated += 1;
            }
        }
        if report != RecoverReport::default() {
            tracing::info!(
                requeued = report.requeued,
                cancelled_ops = report.cancelled_ops,
                aggregated = report.aggregated,
                "編集バッチをリカバリした"
            );
        }
        Ok(report)
    }

    // ------------------------------------------------------------ 内部

    async fn load_op(&self, op_id: i64) -> Result<(Op, Vec<history::Edit>, String), EditError> {
        self.db
            .read(move |c| {
                let Some(op) = history::get_op(c, op_id)? else {
                    return Ok(Err(EditError::OpNotFound(op_id)));
                };
                let edits = history::list_edits(c, op_id)?;
                let Some(rel_path) = history::track_rel_path(c, op.track_id)? else {
                    return Ok(Err(EditError::TrackNotFound(op.track_id)));
                };
                Ok(Ok((op, edits, rel_path)))
            })
            .await?
    }

    async fn mark_applying(&self, batch_id: i64) -> Result<(), EditError> {
        let event = self
            .db
            .write(move |c| {
                if history::mark_batch_applying(c, batch_id)? {
                    Ok(Some(batch_event(c, batch_id, BatchState::Applying)?))
                } else {
                    Ok(None)
                }
            })
            .await?;
        if let Some(ev) = event {
            self.jobs.publish(Event::Batch(ev));
        }
        Ok(())
    }

    /// pending が無ければバッチを終端にする。遷移したら `true`
    async fn aggregate(&self, batch_id: i64) -> Result<bool, EditError> {
        let event = self
            .db
            .write(move |c| {
                let tx = c.transaction()?;
                let ev = match history::aggregate_batch(&tx, batch_id, now_epoch())? {
                    Some(state) => Some(batch_event(&tx, batch_id, state)?),
                    None => None,
                };
                tx.commit()?;
                Ok(ev)
            })
            .await?;
        match event {
            Some(ev) => {
                self.jobs.publish(Event::Batch(ev));
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

// ---------------------------------------------------------------- prepare（同期）

/// バッチの対象 1 件（[`Editor::prepare_tags_with`] の入力）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTarget {
    pub track_id: i64,
    /// preview 時の `tag_version`。現在値と違えば op を `skipped_conflict` で記録し、DB も版も
    /// 触らない（SPEC §9「スナップショット時と tag_version が変わった行は skipped_conflict」）
    pub expected_tag_version: Option<i64>,
    /// 連番などの位置（選択集合をソートしたときの 0 始まり）。evaluator に渡す
    pub index: usize,
    /// preview 時の事前条件（D-33 の snapshot）。あれば apply 時の DB 値ではなくこれを op に記録し、
    /// preview の後の外部変更（音声の差し替え等）を tagwrite の事前条件確認で conflict にする。
    /// 無ければ記録時点の DB 値
    pub expected: Option<Precondition>,
}

/// 現在のタグ集合から新しいタグ集合を作る評価器（`domain::tagops::apply_ops` など）。
/// 返り値が現在値と同じ（差分なし）なら op にならない
pub type Evaluator = dyn Fn(&PlanTarget, &TagSet) -> Result<TagSet, String> + Send + Sync;

struct PlannedOp {
    track_id: i64,
    expected: Precondition,
    tag_version: i64,
    changes: Vec<(String, serde_json::Value, serde_json::Value)>,
    new_set: TagSet,
}

/// preview 後に版が進んでいた対象（op を conflict で記録する）
struct ConflictTarget {
    track_id: i64,
    expected: Precondition,
    error: String,
}

/// 現在値との差分（キー → (旧 JSON, 新 JSON)）
fn diff_sets(
    current: &TagSet,
    new: &TagSet,
) -> Vec<(String, serde_json::Value, serde_json::Value)> {
    let mut keys: Vec<&str> = current
        .items()
        .iter()
        .chain(new.items().iter())
        .map(|(k, _)| k.as_str())
        .collect();
    keys.sort_unstable();
    keys.dedup();
    let mut out = Vec::new();
    for key in keys {
        let old: Vec<String> = current.values(key).map(str::to_owned).collect();
        let new_v: Vec<String> = new.values(key).map(str::to_owned).collect();
        if old != new_v {
            out.push((key.to_owned(), values_json(&old), values_json(&new_v)));
        }
    }
    out
}

/// [`Editor::prepare_tags_with`] のトランザクション本体
fn prepare_tags_tx(
    conn: &mut Connection,
    description: Option<&str>,
    targets: &[PlanTarget],
    eval: &Evaluator,
    now: i64,
) -> Result<Prepared, EditError> {
    let tx = conn.transaction()?;
    let mut ids: Vec<i64> = targets.iter().map(|t| t.track_id).collect();
    ids.sort_unstable();
    if let Some(w) = ids.windows(2).find(|w| w[0] == w[1]) {
        return Err(EditError::DuplicateTrack(w[0]));
    }
    let pending = history::pending_track_ids(&tx, &ids)?;
    if !pending.is_empty() {
        return Err(EditError::Pending { track_ids: pending });
    }

    let mut planned: Vec<PlannedOp> = Vec::new();
    let mut conflicts: Vec<ConflictTarget> = Vec::new();
    let mut unchanged = 0;
    for target in targets {
        let current_pre = history::precondition_of_track(&tx, target.track_id)?
            .ok_or(EditError::TrackNotFound(target.track_id))?;
        let expected = target.expected.clone().unwrap_or(current_pre);
        let tag_version = history::track_tag_version(&tx, target.track_id)?
            .ok_or(EditError::TrackNotFound(target.track_id))?;
        if let Some(v) = target.expected_tag_version {
            if v != tag_version {
                conflicts.push(ConflictTarget {
                    track_id: target.track_id,
                    expected,
                    error: format!("プレビューの後に変更された（tag_version {v} → {tag_version}）"),
                });
                continue;
            }
        }
        let current = history::load_track_tags(&tx, target.track_id)?;
        let new_set = eval(target, &current).map_err(EditError::Internal)?;
        let changes = diff_sets(&current, &new_set);
        if changes.is_empty() {
            unchanged += 1;
            continue;
        }
        planned.push(PlannedOp {
            track_id: target.track_id,
            expected,
            tag_version: tag_version + 1,
            changes,
            new_set,
        });
    }
    if planned.is_empty() && conflicts.is_empty() {
        return Err(EditError::NoChanges);
    }

    let affected = planned.len() + conflicts.len();
    let batch_id = history::insert_batch(&tx, description, affected as i64, None, now)?;
    let mut job_ids = Vec::with_capacity(planned.len());
    let mut ordinal = 0i64;
    for p in &planned {
        let op_id = history::insert_op(
            &tx,
            batch_id,
            ordinal,
            p.track_id,
            OpKind::Tags,
            &p.expected,
        )?;
        ordinal += 1;
        for (key, old, new) in &p.changes {
            history::insert_edit(&tx, op_id, key, old, new)?;
        }
        // overlay: DB を新値へ。tag_version はトラックごとに 1 回だけ進める
        let hash = tag_hash(&p.new_set);
        history::set_track_tags(
            &tx,
            p.track_id,
            &p.new_set,
            Some(&hash),
            &cache_columns(&p.new_set),
            p.tag_version,
        )?;
        let job = tagwrite_job(p.track_id, p.tag_version, op_id, batch_id);
        let job_id = dbjobs::enqueue(&tx, &job, now)?.id();
        history::set_op_job(&tx, op_id, job_id)?;
        job_ids.push(job_id);
    }
    // preview 後に版が進んだ行は conflict として記録だけする（DB も版も触らない）
    for c in &conflicts {
        let op_id = history::insert_op(
            &tx,
            batch_id,
            ordinal,
            c.track_id,
            OpKind::Tags,
            &c.expected,
        )?;
        ordinal += 1;
        history::finish_op(
            &tx,
            op_id,
            OpResult::SkippedConflict,
            Some(&c.error),
            None,
            now,
        )?;
    }
    // 全件 conflict なら pending が無いので、ここで終端にする
    let event = match history::aggregate_batch(&tx, batch_id, now)? {
        Some(state) => Some(batch_event(&tx, batch_id, state)?),
        None => None,
    };
    tx.commit()?;
    tracing::info!(
        batch_id,
        affected,
        conflict = conflicts.len(),
        unchanged,
        "編集バッチを記録した"
    );
    Ok(Prepared {
        batch_id,
        affected,
        unchanged,
        conflict: conflicts.len(),
        job_ids,
        event,
    })
}

// ---------------------------------------------------------------- apply（ファイル側、同期）

fn ext_of(rel: &RelPath) -> Option<&str> {
    rel.file_name().rsplit_once('.').map(|(_, x)| x)
}

/// 開いたファイルの stat とタグ（読めなければ None）
fn read_file_state(root: &RootDir, rel_path: &str) -> Option<FileState> {
    let rel = RelPath::parse(rel_path).ok()?;
    let file = root
        .open_file(&rel)
        .map_err(|e| tracing::warn!(path = rel_path, error = %e, "overlay 解消のためにファイルを開けない"))
        .ok()?;
    let st = fsroot::fstat(&file).ok()?;
    let af = read_audio_file(file, ext_of(&rel))
        .map_err(
            |e| tracing::warn!(path = rel_path, error = %e, "overlay 解消のためにタグを読めない"),
        )
        .ok()?;
    Some(FileState::external(root, &rel, st, af))
}

/// 事前条件と実体の差（一致なら空）。
///
/// `rel_path` が記録時点と違う（外部 rename をスキャナが `(dev, inode)` で追随した）ときは、
/// tags op はそのまま続行してよい（SPEC §7.5）。Linux の rename は ctime を進めるので、
/// その場合に限り **ctime_ns だけの不一致**は rename によるものとみなして許容する
/// （タグ内容は同じ FD から読んだ `tag_hash` で別に確認している）
fn mismatches(
    expected: &Precondition,
    st: &fsroot::Stat,
    hash: &[u8; 32],
    rel_path: &str,
) -> Vec<&'static str> {
    let mut out = Vec::new();
    let renamed = expected.rel_path.as_deref() != Some(rel_path);
    if expected.dev != Some(st.dev as i64) {
        out.push("dev");
    }
    if expected.inode != Some(st.inode as i64) {
        out.push("inode");
    }
    if expected.size != Some(st.size as i64) {
        out.push("size");
    }
    if expected.mtime_ns != Some(st.mtime_ns) {
        out.push("mtime_ns");
    }
    if expected.tag_hash.as_deref() != Some(hash.as_slice()) {
        out.push("tag_hash");
    }
    if expected.ctime_ns != Some(st.ctime_ns) && !(renamed && out.is_empty()) {
        out.push("ctime_ns");
    }
    out
}

/// ファイルの全フィールドが op の新値と一致するか
fn file_matches_new_values(tags: &TagSet, edits: &[history::Edit]) -> bool {
    edits.iter().all(|e| {
        let current: Vec<&str> = tags.values(&e.key).collect();
        current == json_values(&e.new_value)
    })
}

fn stage_tags(
    root: &RootDir,
    op: &Op,
    edits: &[history::Edit],
    rel_path: &str,
    before_rename: Option<&BeforeRenameHook>,
) -> Result<Staged, EditError> {
    let rel = RelPath::parse(rel_path)?;
    let ext = ext_of(&rel);
    let mut file = match root.open_file(&rel) {
        Ok(f) => f,
        Err(FsError::NotFound | FsError::Symlink | FsError::Escaped) => {
            return Ok(Staged::Conflict {
                reason: format!("ファイルを開けない: {rel_path}"),
                current: None,
            })
        }
        Err(e) => return Err(e.into()),
    };
    let st = fsroot::fstat(&file)?;
    let af = {
        let mut reader = file.try_clone()?;
        reader.seek(SeekFrom::Start(0))?;
        match read_audio_file(reader, ext) {
            Ok(af) => af,
            Err(e) => {
                return Ok(Staged::Conflict {
                    reason: format!("タグを読めない: {e}"),
                    current: None,
                })
            }
        }
    };
    let hash = tag_hash(&af.tags);
    let diff = mismatches(&op.expected, &st, &hash, rel_path);
    if !diff.is_empty() {
        let state = FileState::external(root, &rel, st, af);
        if file_matches_new_values(&state.content.tags, edits) {
            tracing::info!(
                op_id = op.id,
                path = rel_path,
                "事前条件は外れているがファイルは新値。applied として確定"
            );
            return Ok(Staged::AlreadyMatches(state));
        }
        return Ok(Staged::Conflict {
            reason: format!("事前条件不一致: {}", diff.join(", ")),
            current: Some(state),
        });
    }

    // 一致: 親 dir に tmp を O_EXCL で作り、内容をコピーして全フィールドを書き、fsync → rename
    let changes: Vec<TagChange> = edits
        .iter()
        .map(|e| TagChange {
            key: e.key.clone(),
            values: match &e.new_value {
                serde_json::Value::Null => None,
                v => Some(json_values(v)),
            },
        })
        .collect();
    let parent = rel.parent();
    let (tmp_rel, mut tmp) = root.create_tmp(parent.as_ref())?;
    let written = (|| -> Result<Staged, EditError> {
        file.seek(SeekFrom::Start(0))?;
        std::io::copy(&mut file, &mut tmp)?;
        // mode / 所有者 / xattr（ACL）を元ファイルから写す（D-41）
        fsroot::copy_attrs(&file, &tmp)?;
        write_tag_changes(&mut tmp, ext, &changes)?;
        tmp.sync_all()?;
        // 書いた内容を同じ FD から読み戻して tag_hash を確定する（スキャナと同じ計算）。
        // 編集したキーが意図どおりに読めなければ、この形式には書けないので反映しない
        let mut reader = tmp.try_clone()?;
        reader.seek(SeekFrom::Start(0))?;
        let written = read_audio_file(reader, ext)?;
        if !file_matches_new_values(&written.tags, edits) {
            let mismatched: Vec<&str> = edits
                .iter()
                .filter(|e| {
                    let got: Vec<&str> = written.tags.values(&e.key).collect();
                    got != json_values(&e.new_value)
                })
                .map(|e| e.key.as_str())
                .collect();
            return Ok(Staged::Failed {
                reason: format!(
                    "書き込み結果が意図と一致しない（この形式では表現できない）: {}",
                    mismatched.join(", ")
                ),
                current: Some(FileState::external(root, &rel, st, af.clone())),
            });
        }
        if let Some(hook) = before_rename {
            hook(rel_path);
        }
        // 事前条件の確認から rename までの窓で外部が書き換えていないか、宛先を開き直して
        // 確かめる。stat（ctime を含む）が確認時点と同じなら内容も同じ（書けば ctime が進む）。
        // この再確認と rename の間にも窓は残るが、排他ロックはしない（不変条件 5）ので
        // これ以上は縮められない。残りはスキャンが調停する
        let recheck = match root.open_file(&rel) {
            Ok(f) => f,
            Err(FsError::NotFound | FsError::Symlink | FsError::Escaped) => {
                return Ok(Staged::Conflict {
                    reason: "反映の直前にファイルが無くなった".to_owned(),
                    current: None,
                })
            }
            Err(e) => return Err(e.into()),
        };
        let now_st = fsroot::fstat(&recheck)?;
        if !same_stat(&st, &now_st) {
            let current = read_audio_file(recheck, ext)
                .ok()
                .map(|af| FileState::external(root, &rel, now_st, af));
            return Ok(Staged::Conflict {
                reason: "反映の直前に外部で更新された".to_owned(),
                current,
            });
        }
        drop(recheck);
        root.replace_file(&tmp_rel, &rel)?;
        // rename は ctime を進めるので、rename 後に同じ FD を fstat する
        let st = fsroot::fstat(&tmp)?;
        Ok(Staged::Written(FileState {
            ph: st.into(),
            content: track_content(written),
            fp: None,
        }))
    })();
    // rename まで到達した Written 以外は tmp を消す
    if !matches!(written, Ok(Staged::Written(_))) {
        if let Err(u) = root.unlink(&tmp_rel) {
            if !matches!(u, FsError::NotFound) {
                tracing::warn!(path = %tmp_rel, error = %u, "tmp を消せない");
            }
        }
    }
    written
}

/// 同じ実体・同じ内容か（dev / inode / size / mtime / ctime）
fn same_stat(a: &fsroot::Stat, b: &fsroot::Stat) -> bool {
    a.dev == b.dev
        && a.inode == b.inode
        && a.size == b.size
        && a.mtime_ns == b.mtime_ns
        && a.ctime_ns == b.ctime_ns
}

// ---------------------------------------------------------------- Derived の追随

/// applied になったトラックに Derived があり、その `src_tag_version` が現在の `tag_version` より
/// 古ければタグ上書きジョブを投入する（SPEC §7.6「tag_version 差分のみ → タグ上書き」、D-42）。
/// 種別は `transcode`（payload `kind = "retag"`、dedup key は `transcode:<track_id>:<audio_version>`）。
/// ハンドラは P1-10。Derived が無ければ何もしない
fn enqueue_derived_retag(
    tx: &Connection,
    track_id: i64,
    now: i64,
) -> crate::db::Result<Option<i64>> {
    use rusqlite::OptionalExtension as _;
    let stale: Option<(i64, i64)> = tx
        .query_row(
            "SELECT t.audio_version, t.tag_version FROM tracks t
             JOIN derived_files d ON d.track_id = t.id
             WHERE t.id = ?1 AND d.src_tag_version <> t.tag_version",
            [track_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((audio_version, tag_version)) = stale else {
        return Ok(None);
    };
    let job = NewJob::new(
        JobType::Transcode,
        serde_json::json!({
            "track_id": track_id,
            "audio_version": audio_version,
            "tag_version": tag_version,
            "kind": "retag",
        }),
    )
    .dedup_key(format!("transcode:{track_id}:{audio_version}"));
    Ok(match dbjobs::enqueue(tx, &job, now)? {
        dbjobs::EnqueueResult::Inserted(id) => Some(id),
        dbjobs::EnqueueResult::Duplicate(_) => None,
    })
}

// ---------------------------------------------------------------- DB の追随と overlay の解消

/// DB のタグ・キャッシュ列・`tag_hash`・音声属性・物理属性をファイルの現在値に揃える。
/// `tag_version` は動かさない。外部の実体を採用する（`fp` あり）ときはスキャナと同じ規則で
/// フィンガープリントを比べ、音声が変わっていれば `audio_version` を進める
/// （物理属性が新ファイルに揃うので、次回スキャンには「変更なし」と見える。ここで確定しないと
/// 音声の差し替えを deep scan まで拾えない）
fn sync_track_to_file(tx: &Connection, track_id: i64, fs: &FileState) -> crate::db::Result<()> {
    let row = scans::load_current_row(tx, track_id)?;
    history::set_track_physical(tx, track_id, &fs.ph)?;
    scans::update_content(tx, track_id, &fs.content, row.tag_version)?;
    if let Some(fp) = fs.fp {
        let audio_version = if audio_changed(fp, row.audio_md5, row.audio_fp) {
            tracing::info!(
                track_id,
                "外部で音声が差し替えられていた。audio_version を進める"
            );
            row.audio_version + 1
        } else {
            row.audio_version
        };
        let fp = effective_fingerprint(fp, row.audio_md5, row.audio_fp);
        scans::update_fingerprint(tx, track_id, fp, audio_version)?;
    }
    Ok(())
}

/// applied 以外の終端になる op の overlay を解消する（SPEC §7.5）。ファイルの現在値があれば
/// それに、無ければ記録時点の旧値と事前条件の物理属性に戻す。`tag_version` は据え置く
fn resolve_overlay(
    tx: &Connection,
    op: &Op,
    edits: &[history::Edit],
    current: Option<&FileState>,
) -> crate::db::Result<()> {
    if let Some(fs) = current {
        return sync_track_to_file(tx, op.track_id, fs);
    }
    let tag_version = history::track_tag_version(tx, op.track_id)?.unwrap_or(1);
    let now = history::load_track_tags(tx, op.track_id)?;
    let restored = with_replaced(
        &now,
        edits
            .iter()
            .map(|e| (e.key.clone(), json_values(&e.old_value))),
    );
    let cache: CacheColumns = cache_columns(&restored);
    history::set_track_tags(
        tx,
        op.track_id,
        &restored,
        op.expected.tag_hash.as_deref(),
        &cache,
        tag_version,
    )?;
    history::restore_track_precondition(tx, op.track_id, &op.expected)
}

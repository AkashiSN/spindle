//! `tagwrite` ジョブ（SPEC §7.5 / §8）。payload は
//! `{"track_id", "tag_version", "op_id", "batch_id"}`、dedup key は `tagwrite:<track_id>:<tag_version>`。
//!
//! 基盤（worker）がトラックをロックし stale 判定を済ませた状態で始まる。本体は
//! [`Editor::apply_op`] で、何度実行しても結果は変わらない。
//!
//! - 手を付ける前に cancel 要求があれば op を failed('cancelled') に閉じて `Cancelled`。
//!   ファイル操作を始めた後は cancel を見ない（進行中の op は完了を待つ。SPEC §7.5）
//! - 失敗はバックオフで再試行し、最終試行の失敗では op を failed に閉じて overlay を解消する
//!   （op を pending のまま残さない）

use std::sync::Arc;

use crate::db::history::CANCELLED_ERROR;
use crate::edit::Editor;
use crate::jobs::{BoxFuture, Handler, HandlerResult, JobContext, JobError, Outcome};

pub struct TagwriteHandler {
    editor: Arc<Editor>,
}

impl TagwriteHandler {
    pub fn new(editor: Arc<Editor>) -> Self {
        Self { editor }
    }
}

impl Handler for TagwriteHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let editor = Arc::clone(&self.editor);
        Box::pin(async move {
            let job_id = ctx.job.id;
            let Some(op_id) = ctx.job.payload.get("op_id").and_then(|v| v.as_i64()) else {
                return Err(JobError::Failed(anyhow::anyhow!(
                    "payload に整数の op_id が無い: {}",
                    ctx.job.payload
                )));
            };
            match ctx.check_cancel().await {
                Ok(()) => {}
                Err(JobError::Cancelled) => {
                    editor
                        .close_op(op_id, CANCELLED_ERROR, Some(job_id))
                        .await
                        .map_err(|e| JobError::Failed(e.into()))?;
                    return Err(JobError::Cancelled);
                }
                Err(e) => return Err(e),
            }
            match editor.apply_op(op_id, Some(job_id)).await {
                Ok(outcome) => {
                    tracing::debug!(job_id, op_id, ?outcome, "tagwrite 完了");
                    Ok(Outcome::Done)
                }
                Err(e) => {
                    let message = format!("{e:#}");
                    let last_attempt = ctx.job.attempts + 1 >= ctx.job.max_attempts;
                    if last_attempt {
                        // 最終試行: op を pending のまま残さず閉じる（overlay の解消を伴う）
                        if let Err(c) = editor.close_op(op_id, &message, Some(job_id)).await {
                            tracing::warn!(job_id, op_id, error = %c, "最終失敗の op を閉じられない");
                        }
                    }
                    Err(JobError::Failed(anyhow::anyhow!(message)))
                }
            }
        })
    }
}

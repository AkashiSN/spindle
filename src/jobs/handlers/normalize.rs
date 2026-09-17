//! `normalize` ジョブ（SPEC §7.4 / §8、P1-4）。payload は `{"track_id", "op_id", "batch_id"}`、
//! dedup key は `normalize:<track_id>:<op_id>`。track 単位・並列 2。
//!
//! 版を持たないので基盤はロックしない。ハンドラが自分でトラックをロックし（取れなければ
//! 再キュー）、本体は [`Editor::apply_archive_op`]。何度実行しても結果は変わらない。
//!
//! - 手を付ける前に cancel 要求があれば op を failed('cancelled') に閉じて `Cancelled`。
//!   変換中（ffmpeg / flac）の cancel は子プロセスごと止めて同じく閉じる。Library を触り始めた後
//!   （配置 → 退避）は cancel を見ない
//! - 失敗はバックオフで再試行し、最終試行の失敗では op を failed に閉じる（pending のまま残さない）

use std::sync::Arc;

use crate::db::history::CANCELLED_ERROR;
use crate::edit::{EditError, Editor};
use crate::jobs::{BoxFuture, Handler, HandlerResult, JobContext, JobError, Outcome};

pub struct NormalizeHandler {
    editor: Arc<Editor>,
}

impl NormalizeHandler {
    pub fn new(editor: Arc<Editor>) -> Self {
        Self { editor }
    }
}

impl Handler for NormalizeHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let editor = Arc::clone(&self.editor);
        Box::pin(async move {
            let job_id = ctx.job.id;
            let int_of = |key: &str| ctx.job.payload.get(key).and_then(|v| v.as_i64());
            let (Some(op_id), Some(track_id)) = (int_of("op_id"), int_of("track_id")) else {
                return Err(JobError::Failed(anyhow::anyhow!(
                    "payload に整数の op_id / track_id が無い: {}",
                    ctx.job.payload
                )));
            };
            if !ctx.lock_tracks(&[track_id]).await? {
                return Ok(Outcome::Requeue);
            }
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
            match editor
                .apply_archive_op(op_id, Some(job_id), &ctx.cancel_token())
                .await
            {
                Ok(outcome) => {
                    tracing::debug!(job_id, op_id, ?outcome, "normalize 完了");
                    Ok(Outcome::Done)
                }
                Err(EditError::Cancelled) => {
                    if let Err(c) = editor.close_op(op_id, CANCELLED_ERROR, Some(job_id)).await {
                        tracing::warn!(job_id, op_id, error = %c, "キャンセルした op を閉じられない");
                    }
                    Err(JobError::Cancelled)
                }
                Err(e) => {
                    let message = format!("{e:#}");
                    let last_attempt = ctx.job.attempts + 1 >= ctx.job.max_attempts;
                    if last_attempt {
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

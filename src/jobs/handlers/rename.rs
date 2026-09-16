//! `rename` ジョブ（SPEC §7.5「リネーム」/ §8）。payload は `{"batch_id"}`、dedup key は
//! `rename:batch:<batch_id>`。バッチ 1 つに 1 ジョブで、並列度 1（2 phase の順序を守る）。
//!
//! 本体は [`Editor::apply_rename_batch`] で、何度実行しても結果は変わらない。バッチの全トラックを
//! ロックしてから始める（取れなければ再キュー）。cancel は phase 1 の間だけ効き、退避済みの
//! ファイルを戻して op を閉じる。phase 2 に入ったら最後まで進める（一部だけ戻すと swap / 循環が
//! 解けない）。最終試行の失敗では pending の op を failed に閉じて overlay を解消する

use std::sync::Arc;

use crate::db::history;
use crate::edit::Editor;
use crate::jobs::{BoxFuture, Handler, HandlerResult, JobContext, JobError, Outcome};

pub struct RenameHandler {
    editor: Arc<Editor>,
}

impl RenameHandler {
    pub fn new(editor: Arc<Editor>) -> Self {
        Self { editor }
    }
}

impl Handler for RenameHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let editor = Arc::clone(&self.editor);
        Box::pin(async move {
            let job_id = ctx.job.id;
            let Some(batch_id) = ctx.job.payload.get("batch_id").and_then(|v| v.as_i64()) else {
                return Err(JobError::Failed(anyhow::anyhow!(
                    "payload に整数の batch_id が無い: {}",
                    ctx.job.payload
                )));
            };
            let track_ids: Vec<i64> = ctx
                .db()
                .read(move |c| {
                    Ok(history::pending_ops(c, batch_id)?
                        .into_iter()
                        .map(|o| o.track_id)
                        .collect())
                })
                .await?;
            if !ctx.lock_tracks(&track_ids).await? {
                return Ok(Outcome::Requeue);
            }
            match ctx.check_cancel().await {
                Ok(()) => {}
                Err(JobError::Cancelled) => {
                    editor
                        .close_op_batch_cancelled(batch_id, Some(job_id))
                        .await
                        .map_err(|e| JobError::Failed(e.into()))?;
                    return Err(JobError::Cancelled);
                }
                Err(e) => return Err(e),
            }
            match editor
                .apply_rename_batch(batch_id, Some(job_id), Some(&ctx))
                .await
            {
                Ok(outcome) if outcome.cancelled => {
                    tracing::info!(job_id, batch_id, "rename をキャンセルした");
                    Err(JobError::Cancelled)
                }
                Ok(outcome) => {
                    tracing::debug!(job_id, batch_id, ?outcome, "rename 完了");
                    Ok(Outcome::Done)
                }
                Err(e) => {
                    let message = format!("{e:#}");
                    let last_attempt = ctx.job.attempts + 1 >= ctx.job.max_attempts;
                    if last_attempt {
                        if let Err(c) = editor
                            .close_op_batch_failed(batch_id, Some(job_id), &message)
                            .await
                        {
                            tracing::warn!(job_id, batch_id, error = %c, "最終失敗の op を閉じられない");
                        }
                    }
                    Err(JobError::Failed(anyhow::anyhow!(message)))
                }
            }
        })
    }
}

//! `inbox_rejudge` ジョブ（Inbox の件の再判定）。並列 1、dedup `inbox_rejudge:<item_id>`、再試行しない。
//! payload は `{ "item_id" }`。本体は `import::ytmusic::rejudge::rejudge_item`

use std::sync::Arc;

use crate::import::ytmusic::downloader::{DownloadError, DownloaderEnv};
use crate::import::ytmusic::rejudge::{rejudge_item, RejudgeError, RejudgeReport};
use crate::jobs::{
    BoxFuture, Handler, HandlerResult, JobContext, JobError, JobType, NewJob, Outcome,
};

const DEDUP_PREFIX: &str = "inbox_rejudge:";

/// 件 `item_id` の再判定ジョブ。同じ件は走行中・待ちの間 1 つにまとまる。人が直す前に失敗を
/// 繰り返しても意味がないので、再試行しない
pub fn new_inbox_rejudge_job(item_id: i64) -> NewJob {
    NewJob::new(
        JobType::InboxRejudge,
        serde_json::json!({ "item_id": item_id }),
    )
    .dedup_key(format!("{DEDUP_PREFIX}{item_id}"))
    .max_attempts(1)
}

pub struct InboxRejudgeHandler {
    env: Arc<DownloaderEnv>,
}

impl InboxRejudgeHandler {
    pub fn new(env: DownloaderEnv) -> Self {
        Self { env: Arc::new(env) }
    }
}

/// 結果の 1 行目に件数、続けて移せなかった曲と失敗した曲を 1 行ずつ
fn note_of(report: &RejudgeReport) -> String {
    let mut note = format!(
        "再判定: {}/{} 曲、移動 {} 曲、失敗 {} 曲",
        report.rejudged,
        report.total,
        report.moved.len(),
        report.failed.len()
    );
    for (file, state) in &report.not_moved {
        note.push_str(&format!("\n{file}: 移せなかった（移動先が {state}）"));
    }
    for (file, reason) in &report.failed {
        note.push_str(&format!("\n{file}: {reason}"));
    }
    note
}

impl Handler for InboxRejudgeHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let env = Arc::clone(&self.env);
        Box::pin(async move {
            let Some(item_id) = ctx.job.payload.get("item_id").and_then(|v| v.as_i64()) else {
                return Err(JobError::Fatal(anyhow::anyhow!(
                    "payload に item_id が無い"
                )));
            };
            let token = ctx.cancel_token();
            let (tx, mut rx) = tokio::sync::watch::channel((0usize, 0usize));
            let run = rejudge_item(&env, item_id, &token, &tx);
            tokio::pin!(run);
            // 本体を進めながら、曲を 1 つ終えるたびの進捗を job へ流す
            let result = loop {
                tokio::select! {
                    r = &mut run => break r,
                    changed = rx.changed() => {
                        if changed.is_ok() {
                            let (done, total) = *rx.borrow_and_update();
                            ctx.progress(done as i64, total as i64).await?;
                        }
                    }
                }
            };
            match result {
                Ok(report) => Ok(Outcome::DoneWith(note_of(&report))),
                Err(RejudgeError::NotPending(m)) => Err(JobError::Fatal(anyhow::anyhow!(m))),
                Err(RejudgeError::Download(DownloadError::Cancelled)) => Err(JobError::Cancelled),
                Err(RejudgeError::Download(DownloadError::Fatal(m))) => {
                    Err(JobError::Fatal(anyhow::anyhow!(m)))
                }
                Err(RejudgeError::Download(DownloadError::Failed(e))) => Err(JobError::Failed(e)),
            }
        })
    }
}

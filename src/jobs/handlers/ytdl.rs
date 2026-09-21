//! `ytdl` ジョブ（SPEC §7.7 / §8、D-70、P3-3）。並列 1、dedup `ytdl:<url>`。payload は `{ "url" }`
//! （購読由来なら `subscription_id` / `position` も。P4-16）。
//! 本体は `import::ytmusic::downloader::download_one`。Fatal は再試行せず `failed`

use std::sync::Arc;

use crate::import::ytmusic::downloader::{
    download_one, DownloadError, DownloadRequest, Downloaded, DownloaderEnv,
};
use crate::jobs::{BoxFuture, Handler, HandlerResult, JobContext, JobError, Outcome};

pub use crate::import::ytmusic::downloader::{new_subscription_ytdl_job, new_ytdl_job};

pub struct YtdlHandler {
    env: Arc<DownloaderEnv>,
}

impl YtdlHandler {
    pub fn new(env: DownloaderEnv) -> Self {
        Self { env: Arc::new(env) }
    }
}

impl Handler for YtdlHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let env = Arc::clone(&self.env);
        Box::pin(async move {
            let request: DownloadRequest = match serde_json::from_value(ctx.job.payload.clone()) {
                Ok(r) => r,
                Err(e) => {
                    return Err(JobError::Fatal(anyhow::anyhow!("payload を読めない: {e}")));
                }
            };
            if request.url.trim().is_empty() {
                return Err(JobError::Fatal(anyhow::anyhow!("payload に url が無い")));
            }
            match download_one(&env, ctx.job.id, &request, &ctx.cancel_token()).await {
                // 結果 1 行を note に残す（YouTube 画面が出す。done の中身を区別する）
                Ok(Downloaded::Staged { rel_path, .. }) => Ok(Outcome::DoneWith(format!(
                    "Inbox に置いた: {}",
                    rel_path.as_str()
                ))),
                Ok(Downloaded::Skipped { message }) => {
                    Ok(Outcome::DoneWith(format!("プラグインが skip: {message}")))
                }
                Ok(Downloaded::Playlist { enqueued, skipped }) => Ok(Outcome::DoneWith(format!(
                    "再生リストを展開した: {enqueued} 件を投入、{skipped} 件は取り込み済み"
                ))),
                Err(DownloadError::Cancelled) => Err(JobError::Cancelled),
                Err(DownloadError::Fatal(m)) => Err(JobError::Fatal(anyhow::anyhow!(m))),
                Err(DownloadError::Failed(e)) => Err(JobError::Failed(e)),
            }
        })
    }
}

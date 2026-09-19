//! `ytdl` ジョブ（SPEC §7.7 / §8、D-70、P3-3）。並列 1、dedup `ytdl:<url>`。payload は `{ "url" }`。
//! 本体は `import::ytmusic::downloader::download_one`。Fatal は再試行せず `failed`

use std::sync::Arc;

use crate::import::ytmusic::downloader::{download_one, DownloadError, DownloaderEnv};
use crate::jobs::{BoxFuture, Handler, HandlerResult, JobContext, JobError, Outcome};

pub use crate::import::ytmusic::downloader::new_ytdl_job;

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
            let url = ctx
                .job
                .payload
                .get("url")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .map(str::to_owned);
            let Some(url) = url else {
                return Err(JobError::Fatal(anyhow::anyhow!("payload に url が無い")));
            };
            match download_one(&env, ctx.job.id, &url, &ctx.cancel_token()).await {
                Ok(_) => Ok(Outcome::Done),
                Err(DownloadError::Cancelled) => Err(JobError::Cancelled),
                Err(DownloadError::Fatal(m)) => Err(JobError::Fatal(anyhow::anyhow!(m))),
                Err(DownloadError::Failed(e)) => Err(JobError::Failed(e)),
            }
        })
    }
}

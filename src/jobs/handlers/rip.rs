//! `rip` ジョブ（SPEC §7.2、D-12 / D-66 / D-67 追記 / D-83、P2-5）。payload は
//! `{"toc": <CTDB 形式>, "metadata": DiscMetadata}`（CD 画面の下書き。名前は空でもよい）。dedup key は
//! `rip`（ドライブは物理的に 1 台なので並列 1）。中身は [`crate::cd::rip::rip_disc`]: 吸い出し →
//! 照合（オフセットを当てる）→ 直せなければ CTDB の修復 → それでも駄目なら吸い直し → Inbox に置く。
//! 進捗は相ごとに SSE の `job` イベントの `detail`（[`RipProgress`]）で流す

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::cd::metadata::DiscMetadata;
use crate::cd::place::PlaceError;
use crate::cd::rip::{rip_disc, RipEnv, RipJobError, RipPhase, RipProgress};
use crate::cd::toc::Toc;
use crate::jobs::{
    BoxFuture, Handler, HandlerResult, JobContext, JobError, JobType, NewJob, Outcome,
};

pub const DEDUP_KEY: &str = "rip";
/// 同じ相の中で SSE を流す最短の間隔（読み取りの進捗はセクタごとに来る）
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Deserialize)]
struct Payload {
    toc: String,
    metadata: DiscMetadata,
}

pub fn new_rip_job(toc: &Toc, metadata: &DiscMetadata) -> NewJob {
    NewJob::new(
        JobType::Rip,
        serde_json::json!({ "toc": toc.ctdb_toc(), "metadata": metadata }),
    )
    .dedup_key(DEDUP_KEY)
}

pub struct RipHandler {
    env: Arc<RipEnv>,
}

impl RipHandler {
    pub fn new(env: RipEnv) -> Self {
        Self { env: Arc::new(env) }
    }

    async fn run_inner(&self, ctx: &JobContext) -> HandlerResult {
        let payload: Payload = serde_json::from_value(ctx.job.payload.clone())
            .map_err(|e| JobError::Fatal(anyhow::anyhow!("payload を読めない: {e}")))?;
        let toc = Toc::parse(&payload.toc)
            .map_err(|e| JobError::Fatal(anyhow::anyhow!("TOC を読めない: {e}")))?;
        let token = ctx.cancel_token();
        // 進捗は同期のコールバックで来るので、チャネルで受けてここで間引いて流す
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RipProgress>();
        let progress = Arc::new(move |p: RipProgress| {
            let _ = tx.send(p);
        });
        let rip = rip_disc(&self.env, &toc, &payload.metadata, progress, &token);
        let report = async {
            let mut last: Option<(RipPhase, u32, Instant)> = None;
            while let Some(p) = rx.recv().await {
                let finished = p.total > 0 && p.done >= p.total;
                let due = match &last {
                    Some((phase, attempt, at)) => {
                        *phase != p.phase
                            || *attempt != p.attempt
                            || at.elapsed() >= PROGRESS_INTERVAL
                    }
                    None => true,
                };
                if !(due || finished) {
                    continue;
                }
                last = Some((p.phase, p.attempt, Instant::now()));
                let detail = serde_json::to_value(&p).ok();
                let (done, total) = (
                    i64::try_from(p.done).unwrap_or(i64::MAX),
                    i64::try_from(p.total).unwrap_or(i64::MAX),
                );
                if let Err(JobError::Cancelled) = ctx.progress_with(done, total, detail).await {
                    token.cancel();
                }
            }
        };
        let (result, ()) = tokio::join!(rip, report);
        match result {
            Ok(placed) => {
                tracing::info!(
                    job_id = ctx.job.id,
                    dir = %placed.rel_dir,
                    reused = placed.reused,
                    "CD を吸い出して Inbox に置いた"
                );
                // 結果 1 行を note に残す（CD 画面が「Inbox に置いた」を出す）
                Ok(Outcome::DoneWith(format!(
                    "Inbox に置いた: {}",
                    placed.rel_dir
                )))
            }
            Err(RipJobError::Cancelled) => Err(JobError::Cancelled),
            // 吸い直しても変わらない失敗（別の盤・入力の不正・置き場所の衝突）は再試行しない
            Err(e @ (RipJobError::DiscChanged { .. } | RipJobError::Metadata(_))) => {
                Err(JobError::Fatal(e.into()))
            }
            Err(RipJobError::Place(PlaceError::Cancelled)) => Err(JobError::Cancelled),
            Err(RipJobError::Place(e @ PlaceError::Conflict(_))) => Err(JobError::Fatal(e.into())),
            Err(e) => Err(JobError::Failed(e.into())),
        }
    }
}

impl Handler for RipHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let this = Self {
            env: Arc::clone(&self.env),
        };
        Box::pin(async move { this.run_inner(&ctx).await })
    }
}

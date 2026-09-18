//! `gc` ジョブ（SPEC §8、P1-11、D-56）。payload は `{}`、dedup key は `gc`、並列 1。
//! 本体は `crate::gc`（判定 → 実行）。scan と同じ `LIBRARY_MUTEX` を取れなければ `Requeue`。
//! 周期投入は [`spawn_scheduler`]（起動時と 10 分ごとに「最後の終端 `gc` から 24 時間」で判定）

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::db::{now_epoch, Db};
use crate::gc::{self, GcError, GcRoots};
use crate::jobs::{
    BoxFuture, Handler, HandlerResult, JobContext, JobError, JobType, Jobs, NewJob, Outcome,
    LIBRARY_MUTEX,
};

pub const DEDUP_KEY: &str = "gc";
/// 自動実行の間隔（`[gc]` に設定は足さない。retention が日単位なので 1 日 1 回で足りる）
pub const INTERVAL_SECS: i64 = 24 * 3600;
const SCHEDULER_TICK: Duration = Duration::from_secs(10 * 60);

pub fn new_gc_job() -> NewJob {
    NewJob::new(JobType::Gc, serde_json::json!({})).dedup_key(DEDUP_KEY)
}

pub fn spawn_scheduler(
    jobs: Arc<Jobs>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    crate::jobs::scheduler::spawn_periodic(
        jobs,
        JobType::Gc,
        new_gc_job,
        INTERVAL_SECS,
        SCHEDULER_TICK,
        "定期 GC",
        shutdown,
    )
}

pub struct GcHandler {
    db: Arc<Db>,
    roots: Arc<GcRoots>,
    retention_secs: i64,
}

impl GcHandler {
    pub fn new(db: Arc<Db>, roots: Arc<GcRoots>, retention_secs: i64) -> Self {
        Self {
            db,
            roots,
            retention_secs,
        }
    }

    async fn run_inner(&self, ctx: &JobContext) -> HandlerResult {
        ctx.check_cancel().await?;
        // scan と同じ排他。取れた側だけが走る（D-56）。終端で自動的に解放される
        if !ctx.lock_mutex(LIBRARY_MUTEX).await? {
            tracing::info!(job_id = ctx.job.id, "scan が動いているので GC を待たせる");
            return Ok(Outcome::Requeue);
        }
        let plan = gc::plan(&self.db, &self.roots, self.retention_secs, now_epoch())
            .await
            .map_err(map_err)?;
        let total = (plan.tracks.len()
            + plan.albums.len()
            + plan.archived.len()
            + plan.derived.len()
            + plan.artwork_rows.len()
            + plan.artwork_dirs.len()) as i64;
        tracing::info!(
            job_id = ctx.job.id,
            tracks = plan.tracks.len(),
            albums = plan.albums.len(),
            archived = plan.archived.len(),
            derived = plan.derived.len(),
            artwork_rows = plan.artwork_rows.len(),
            artwork_dirs = plan.artwork_dirs.len(),
            "GC の対象を決めた"
        );
        ctx.progress(0, total).await?;
        let token = ctx.cancel_token();
        let mut summary = gc::execute_rows(&self.db, &self.roots, &plan)
            .await
            .map_err(map_err)?;
        let mut done = (plan.tracks.len() + plan.albums.len() + plan.artwork_rows.len()) as i64;
        ctx.progress(done, total).await?;
        summary.archived =
            gc::execute_archive(&self.db, &self.roots, &plan, &token, Some(ctx.job.id))
                .await
                .map_err(map_err)?;
        done += plan.archived.len() as i64;
        ctx.progress(done, total).await?;
        summary.derived =
            gc::execute_derived(&self.db, &self.roots, &plan, &token, Some(ctx.job.id))
                .await
                .map_err(map_err)?;
        done += plan.derived.len() as i64;
        ctx.progress(done, total).await?;
        summary.artwork_dirs = gc::execute_artwork_dirs(&self.db, &self.roots, &plan, &token)
            .await
            .map_err(map_err)?;
        ctx.progress(total, total).await?;
        gc::log_summary(&summary);
        Ok(Outcome::Done)
    }
}

fn map_err(e: GcError) -> JobError {
    match e {
        GcError::Cancelled => JobError::Cancelled,
        other => JobError::Failed(other.into()),
    }
}

impl Handler for GcHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let this = GcHandler {
            db: Arc::clone(&self.db),
            roots: Arc::clone(&self.roots),
            retention_secs: self.retention_secs,
        };
        Box::pin(async move { this.run_inner(&ctx).await })
    }
}

//! `scan` ジョブ（SPEC §7.1 / §8、D-38）。payload は `{"kind": "incremental" | "deep"}`。
//!
//! `incremental` は、完了した deep scan が `[scan].deep_interval_days` より古い（または一度も
//! 無い）とき deep に昇格する。`0` なら昇格しない。dedup key は固定の `scan`

use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::db::scans::{last_completed_deep_started_at, ScanKind};
use crate::db::{now_epoch, Result as DbResult};
use crate::import::scanner::{ScanError, Scanner};
use crate::jobs::{
    BoxFuture, EnqueueResult, Event, Handler, HandlerResult, JobContext, JobError, JobType, Jobs,
    LibraryEvent, NewJob, Outcome,
};

pub const DEDUP_KEY: &str = "scan";

pub fn parse_kind(s: &str) -> Option<ScanKind> {
    match s {
        "incremental" => Some(ScanKind::Incremental),
        "deep" => Some(ScanKind::Deep),
        _ => None,
    }
}

pub fn new_scan_job(kind: ScanKind) -> NewJob {
    NewJob::new(JobType::Scan, serde_json::json!({ "kind": kind.as_str() })).dedup_key(DEDUP_KEY)
}

/// スキャンを投入する。`kind` は `"incremental"` / `"deep"`（不正なら `None`）
pub async fn enqueue_scan(jobs: &Jobs, kind: &str) -> DbResult<EnqueueResult> {
    let kind = parse_kind(kind).unwrap_or(ScanKind::Incremental);
    jobs.enqueue(new_scan_job(kind)).await
}

pub struct ScanHandler {
    scanner: Arc<Scanner>,
    deep_interval_days: u32,
}

impl ScanHandler {
    pub fn new(scanner: Arc<Scanner>, deep_interval_days: u32) -> Self {
        Self {
            scanner,
            deep_interval_days,
        }
    }

    /// incremental を deep に昇格させるか
    async fn effective_kind(&self, ctx: &JobContext, requested: ScanKind) -> DbResult<ScanKind> {
        if requested == ScanKind::Deep || self.deep_interval_days == 0 {
            return Ok(requested);
        }
        let last = ctx.db().read(last_completed_deep_started_at).await?;
        let interval = i64::from(self.deep_interval_days) * 86_400;
        let due = match last {
            None => true,
            Some(t) => now_epoch() - t >= interval,
        };
        Ok(if due { ScanKind::Deep } else { requested })
    }
}

impl Handler for ScanHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let scanner = Arc::clone(&self.scanner);
        let deep_interval_days = self.deep_interval_days;
        Box::pin(async move {
            let this = ScanHandler {
                scanner,
                deep_interval_days,
            };
            let requested = ctx
                .job
                .payload
                .get("kind")
                .and_then(|v| v.as_str())
                .and_then(parse_kind)
                .unwrap_or(ScanKind::Incremental);
            let kind = this.effective_kind(&ctx, requested).await?;
            if kind != requested {
                tracing::info!(
                    job_id = ctx.job.id,
                    "deep scan の間隔が過ぎているので昇格する"
                );
            }

            // 同期コールバック → 非同期の ctx.progress へ橋渡し。最新値だけを持ち、
            // 進捗のたびに DB を叩かない（ctx.progress が間引く）。キャンセル要求は token で伝わる
            let latest: Arc<Mutex<Option<(u64, u64)>>> = Arc::default();
            let notify = Arc::new(Notify::new());
            let progress = {
                let latest = Arc::clone(&latest);
                let notify = Arc::clone(&notify);
                Arc::new(move |done: u64, total: u64| {
                    *latest.lock().unwrap_or_else(|e| e.into_inner()) = Some((done, total));
                    notify.notify_one();
                })
            };
            let token = ctx.cancel_token();
            let pump = {
                let ctx = ctx.clone();
                let latest = Arc::clone(&latest);
                let notify = Arc::clone(&notify);
                let token = token.clone();
                tokio::spawn(async move {
                    loop {
                        notify.notified().await;
                        let v = latest.lock().unwrap_or_else(|e| e.into_inner()).take();
                        if let Some((done, total)) = v {
                            if ctx.progress(done as i64, total as i64).await.is_err() {
                                token.cancel();
                                return;
                            }
                        }
                    }
                })
            };
            let result = this.scanner.run(kind, progress, token).await;
            pump.abort();
            match result {
                Ok(report) => {
                    // 最後の値を確実に書く
                    let total = report.files_seen as i64;
                    let _ = ctx.progress(total, total).await;
                    // 変更行を表へ通知する（SPEC §9 `library`）。commit 済みなので取得すれば新しい値が見える
                    if let Some(ev) = LibraryEvent::from_changes(report.run_id, report.changed_ids)
                    {
                        ctx.jobs().publish(Event::Library(ev));
                    }
                    Ok(Outcome::Done)
                }
                Err(ScanError::Cancelled) => Err(JobError::Cancelled),
                Err(e) => Err(JobError::Failed(e.into())),
            }
        })
    }
}

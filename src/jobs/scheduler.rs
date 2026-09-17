//! 固定キーのジョブ（backup / gc）を周期投入する常駐タスク。due 判定は `jobs` 表の最後の
//! 終端ジョブからの経過で行い、ファイルの mtime には依らない（復元直後は古い DB の記録しか
//! 無いので、すぐ 1 回走る）。起動直後に一度判定し、以後は due までの残りか `tick` の短い方だけ
//! 待って見直す。投入の重複は dedup key が防ぐ

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::db::jobs as dbjobs;
use crate::db::now_epoch;

use super::{EnqueueResult, JobType, Jobs, NewJob};

/// 最後の終端ジョブから `interval_secs` 経ったか。一度も無ければ true。
/// 時計が戻って `last` が未来にあるときは due にしない（次の実機時刻で自然に解消する）
pub fn is_due(last: Option<i64>, now: i64, interval_secs: i64) -> bool {
    match last {
        None => true,
        Some(last) => now >= last && now - last >= interval_secs,
    }
}

/// 周期投入のタスクを起動する。`label` はログ用（「定期バックアップ」など）
pub fn spawn_periodic<F>(
    jobs: Arc<Jobs>,
    ty: JobType,
    new_job: F,
    interval_secs: i64,
    tick: Duration,
    label: &'static str,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()>
where
    F: Fn() -> NewJob + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            let wait = match jobs
                .db()
                .read(move |c| dbjobs::last_finished_at(c, ty))
                .await
            {
                Ok(last) => {
                    let now = now_epoch();
                    if is_due(last, now, interval_secs) {
                        match jobs.enqueue(new_job()).await {
                            Ok(EnqueueResult::Inserted(id)) => {
                                tracing::info!(job_id = id, "{label}を投入した")
                            }
                            Ok(EnqueueResult::Duplicate(_)) => {}
                            Err(e) => tracing::warn!(error = %e, "{label}を投入できない"),
                        }
                        tick
                    } else {
                        let remaining = last.map_or(0, |l| l + interval_secs - now).max(1);
                        tick.min(Duration::from_secs(remaining as u64))
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "{label}の due 判定に失敗");
                    tick
                }
            };
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(wait) => {}
            }
        }
    })
}

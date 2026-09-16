//! 起動時リカバリ（SPEC §8）。ワーカーを起動する前に必ず通す。
//! `running` を `queued` へ戻し、`track_locks` を全件消す。cancel 要求が立っていた行は
//! 再実行せず `cancelled` にする（D-36）。`run_after` / `attempts` は触らない
//! （バックオフは再起動を跨いで保つ。D-23）

use crate::db::jobs as dbjobs;
use crate::db::{now_epoch, Db, Result};

pub use crate::db::jobs::RecoveryReport;

pub async fn run(db: &Db) -> Result<RecoveryReport> {
    let report = db.write(|c| dbjobs::recover(c, now_epoch())).await?;
    if report.requeued > 0 || report.cancelled > 0 || report.locks_cleared > 0 {
        tracing::info!(
            requeued = report.requeued,
            cancelled = report.cancelled,
            locks_cleared = report.locks_cleared,
            "中断ジョブを再キューした"
        );
    }
    Ok(report)
}

//! スケジューラ本体。種別ごとの Semaphore で並列度を絞り、`claim_next` で取ったジョブを
//! 個別タスクで実行する。終端遷移はすべてここで行う（ハンドラは結果を返すだけ）

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::{debug, info, warn};

use crate::db::jobs as dbjobs;
use crate::db::now_epoch;

use super::{Handler, Job, JobContext, JobError, JobType, Jobs, Outcome, Registry};

/// `run_after` の到来を待つ最長間隔。Notify で起こされるので通常はこれより早く回る
const TICK: Duration = Duration::from_secs(1);
/// `Outcome::Requeue` 後に再度対象になるまでの秒数（同じジョブが空回りしないように）
const REQUEUE_DELAY_SECS: i64 = 1;

/// 版付きジョブの実行前ゲートの結果
enum Gate {
    Proceed,
    Locked(dbjobs::RequeueOutcome),
    Stale,
}

pub(super) async fn run(jobs: Arc<Jobs>, registry: Registry, shutdown: CancellationToken) {
    let registry = Arc::new(registry);
    let cpus = jobs.cpus();
    let mut slots: Vec<(JobType, Arc<Semaphore>)> = registry
        .types()
        .map(|ty| (ty, Arc::new(Semaphore::new(ty.concurrency(cpus)))))
        .collect();
    slots.sort_by_key(|(ty, _)| ty.as_str());
    info!(
        types = ?slots.iter().map(|(t, s)| format!("{t}:{}", s.available_permits())).collect::<Vec<_>>(),
        "ジョブワーカーを開始"
    );

    let mut tasks: JoinSet<()> = JoinSet::new();
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        // バックオフ中などに cancel された queued を、claim する前に cancelled へ送る
        match jobs
            .db()
            .write(|c| dbjobs::sweep_cancel_requested(c, now_epoch()))
            .await
        {
            Ok(ids) => {
                for id in ids {
                    jobs.emit_job(id).await;
                }
            }
            Err(e) => warn!(error = %e, "cancel 要求済み queued の掃除に失敗"),
        }
        for (ty, sem) in &slots {
            claim_all(&jobs, &registry, *ty, sem, &mut tasks, &shutdown).await;
        }

        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = jobs.wake().notified() => {}
            _ = tokio::time::sleep(TICK) => {}
            Some(res) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(e) = res {
                    warn!(error = %e, "ジョブタスクが異常終了");
                }
            }
        }
    }
    // 実行中のタスクは破棄する。DB 上は running のまま残り、次回起動のリカバリで queued に戻る
    let inflight = tasks.len();
    tasks.shutdown().await;
    info!(inflight, "ジョブワーカーを停止");
}

/// 種別 `ty` の空きスロットが尽きるか queued が無くなるまで claim して起動する
async fn claim_all(
    jobs: &Arc<Jobs>,
    registry: &Arc<Registry>,
    ty: JobType,
    sem: &Arc<Semaphore>,
    tasks: &mut JoinSet<()>,
    shutdown: &CancellationToken,
) {
    // slots は registry から作るので必ずあるが、無ければ claim して放置するより何もしない
    let Some(handler) = registry.get(ty) else {
        return;
    };
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        let Ok(permit) = Arc::clone(sem).try_acquire_owned() else {
            return;
        };
        let claimed = jobs
            .db()
            .write(move |c| dbjobs::claim_next(c, ty, now_epoch()))
            .await;
        match claimed {
            Ok(Some(job)) => {
                // claim の DB 往復中に停止が来たら起動せず queued へ戻す（停止後に副作用を始めない）
                if shutdown.is_cancelled() {
                    let id = job.id;
                    let r = jobs
                        .db()
                        .write(move |c| dbjobs::requeue(c, id, now_epoch(), 0))
                        .await;
                    if let Err(e) = r {
                        warn!(job_id = id, %ty, error = %e, "停止時の再キューに失敗");
                    }
                    return;
                }
                tasks.spawn(execute(Arc::clone(jobs), Arc::clone(&handler), job, permit));
            }
            Ok(None) => return,
            Err(e) => {
                warn!(%ty, error = %e, "ジョブの claim に失敗");
                return;
            }
        }
    }
}

/// 1 件を実行して終端まで進める。permit はこの関数の終わりで返る
async fn execute(
    jobs: Arc<Jobs>,
    handler: Arc<dyn Handler>,
    job: Job,
    _permit: OwnedSemaphorePermit,
) {
    let id = job.id;
    let ty = job.job_type;
    let token = CancellationToken::new();
    jobs.track_running(id, token.clone());
    jobs.emit_job(id).await;

    let finished = run_one(&jobs, handler, job, token).await;
    if let Err(e) = finished {
        // ここに来るのは DB 障害。running のまま固着させず、失敗として記録を試みる
        warn!(job_id = id, %ty, error = %e, "ジョブの終端処理に失敗。failed として記録する");
        let message = format!("終端処理に失敗: {e:#}");
        let r = jobs
            .db()
            .write(move |c| {
                let tx = c.transaction()?;
                dbjobs::release_track_locks(&tx, id)?;
                dbjobs::mark_failed(&tx, id, &message, now_epoch())?;
                tx.commit()?;
                Ok(())
            })
            .await;
        if let Err(e) = r {
            warn!(job_id = id, %ty, error = %e, "failed への記録もできない。次回起動のリカバリで queued に戻る");
        }
    }

    jobs.untrack_running(id);
    jobs.emit_job(id).await;
    jobs.wake().notify_one();
}

async fn run_one(
    jobs: &Arc<Jobs>,
    handler: Arc<dyn Handler>,
    job: Job,
    token: CancellationToken,
) -> crate::db::Result<()> {
    let id = job.id;
    let ty = job.job_type;
    let db = Arc::clone(jobs.db());

    // 版付きジョブは payload のトラックをロックしてから、同じトランザクションで stale 判定する
    // （SPEC §8 / §7.5）。ロック待ちの間に版が進んだジョブを実行しないための順序。
    // トラックが既に無ければロックは取れない（FK）ので、先に存在を見て no-op で done にする
    if ty.version_field().is_some() {
        let versioned = match dbjobs::versioned_payload(ty, &job.payload) {
            Ok(v) => v,
            Err(e) => {
                let message = format!("{e:#}");
                warn!(job_id = id, %ty, error = %message, "payload が不正なので実行しない");
                db.write(move |c| dbjobs::mark_failed_permanently(c, id, &message, now_epoch()))
                    .await?;
                return Ok(());
            }
        };
        let Some((track_id, _)) = versioned else {
            return Ok(());
        };
        let job = job.clone();
        let gate = db
            .write(move |c| {
                let tx = c.transaction()?;
                if !dbjobs::track_exists(&tx, track_id)? {
                    dbjobs::mark_done(&tx, id, now_epoch())?;
                    tx.commit()?;
                    return Ok(Gate::Stale);
                }
                if !dbjobs::acquire_track_locks(&tx, id, &[track_id], now_epoch())? {
                    let outcome = dbjobs::requeue(&tx, id, now_epoch(), REQUEUE_DELAY_SECS)?;
                    tx.commit()?;
                    return Ok(Gate::Locked(outcome));
                }
                if dbjobs::is_stale(&tx, &job)? {
                    dbjobs::release_track_locks(&tx, id)?;
                    dbjobs::mark_done(&tx, id, now_epoch())?;
                    tx.commit()?;
                    return Ok(Gate::Stale);
                }
                tx.commit()?;
                Ok(Gate::Proceed)
            })
            .await?;
        match gate {
            Gate::Locked(outcome) => {
                debug!(job_id = id, %ty, track_id, ?outcome, "トラックがロック中なので実行しない");
                return Ok(());
            }
            Gate::Stale => {
                debug!(job_id = id, %ty, "stale なので no-op で完了");
                return Ok(());
            }
            Gate::Proceed => {}
        }
    }

    // claim と track_running の間にキャンセルが来ていれば token を倒しておく
    if db.read(move |c| dbjobs::is_cancel_requested(c, id)).await? {
        token.cancel();
    }

    let ctx = JobContext::new(job, Arc::clone(jobs), token);
    // ハンドラは別タスクで走らせ、panic をジョブの失敗に変換する（ワーカー全体を巻き込まない）。
    // 停止時にこのタスクが abort されたら内側も道連れにする（detach させない）
    let outcome = {
        let ctx = ctx.clone();
        let handle = AbortOnDropHandle::new(tokio::spawn(async move { handler.run(ctx).await }));
        match handle.await {
            Ok(r) => r,
            Err(e) => Err(JobError::Failed(anyhow::anyhow!("ハンドラが異常終了: {e}"))),
        }
    };

    let unflushed = ctx.take_unflushed_progress();
    let now = now_epoch();
    db.write(move |c| {
        let tx = c.transaction()?;
        if let Some((done, total)) = unflushed {
            dbjobs::update_progress(&tx, id, done, total)?;
        }
        dbjobs::release_track_locks(&tx, id)?;
        match outcome {
            // 完了直前に cancel が来ていても完了が勝つ（仕事は済んでいる。D-36）
            Ok(Outcome::Done) => {
                dbjobs::mark_done(&tx, id, now)?;
                debug!(job_id = id, %ty, "完了");
            }
            Ok(Outcome::Requeue) => match dbjobs::requeue(&tx, id, now, REQUEUE_DELAY_SECS)? {
                dbjobs::RequeueOutcome::Requeued { run_after } => {
                    debug!(job_id = id, %ty, run_after, "前提が取れないので再キュー")
                }
                dbjobs::RequeueOutcome::Cancelled => {
                    info!(job_id = id, %ty, "再キュー前に cancel 要求があったので cancelled")
                }
                dbjobs::RequeueOutcome::NotRunning => {
                    warn!(job_id = id, %ty, "再キューしようとしたが running ではなかった")
                }
            },
            Err(JobError::Cancelled) => {
                dbjobs::mark_cancelled(&tx, id, now)?;
                info!(job_id = id, %ty, "キャンセルされた");
            }
            Err(JobError::Failed(e)) => {
                let message = format!("{e:#}");
                match dbjobs::mark_failed(&tx, id, &message, now)? {
                    dbjobs::FailureOutcome::Retrying { run_after, attempts } => {
                        warn!(job_id = id, %ty, attempts, run_after, error = %message, "失敗。再試行を予約")
                    }
                    dbjobs::FailureOutcome::Failed { attempts } => {
                        warn!(job_id = id, %ty, attempts, error = %message, "失敗。上限に達した")
                    }
                    dbjobs::FailureOutcome::Cancelled => {
                        info!(job_id = id, %ty, error = %message, "失敗したが cancel 要求があったので cancelled")
                    }
                    dbjobs::FailureOutcome::NotRunning => {
                        warn!(job_id = id, %ty, error = %message, "失敗したが running ではなかった")
                    }
                }
            }
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

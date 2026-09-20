//! スケジューラ本体。種別ごとの Semaphore で並列度を絞り、`claim_next` で取ったジョブを
//! 個別タスクで実行する。終端遷移はすべてここで行う（ハンドラは結果を返すだけ）。
//!
//! CPU 系の種別（`JobType::cpu_bound`）は種別の許可に加えて共通の予算（= コア数。D-73）も取る。
//! 取得順は「種別 → 共通」で、共通が取れなければ claim せずに次の周回で試す（ジョブは queued の
//! まま。1 本のループで待つと他の種別を止めるので、許可を持ったまま待たずに手放して回る）。
//! 予算を分け合う種別は 1 件ずつ**ラウンドロビン**で claim し、開始位置を周回ごとに回す（種別名順に
//! 空きが尽きるまで取ると、先頭の種別のキューが尽きるまで残りが始まらない）

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

/// 終端遷移の書き込みに失敗したときの再試行間隔（秒。合計約 1 分。D-76）。この間は種別の許可と
/// CPU 予算を持ったまま待つ
const FINISH_RETRY_DELAYS: [u64; 6] = [1, 2, 4, 8, 16, 32];

/// ハンドラの結果を DB に書ける形にしたもの。`JobError` は `anyhow::Error` を含んで Clone
/// できないので、再試行のためにメッセージへ落とす
#[derive(Debug, Clone)]
enum Terminal {
    Done,
    Requeue,
    Cancelled,
    Fatal(String),
    Failed(String),
}

impl From<super::HandlerResult> for Terminal {
    fn from(r: super::HandlerResult) -> Self {
        match r {
            Ok(Outcome::Done) => Terminal::Done,
            Ok(Outcome::Requeue) => Terminal::Requeue,
            Err(JobError::Cancelled) => Terminal::Cancelled,
            Err(JobError::Fatal(e)) => Terminal::Fatal(format!("{e:#}")),
            Err(JobError::Failed(e)) => Terminal::Failed(format!("{e:#}")),
        }
    }
}

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
    let budget = Arc::new(Semaphore::new(jobs.cpu_budget()));
    info!(
        types = ?slots.iter().map(|(t, s)| format!("{t}:{}", s.available_permits())).collect::<Vec<_>>(),
        cpu_budget = budget.available_permits(),
        "ジョブワーカーを開始"
    );

    let (cpu_slots, other_slots): (Vec<_>, Vec<_>) =
        slots.iter().cloned().partition(|(ty, _)| ty.cpu_bound());
    // CPU 系のラウンドロビンの開始位置（周回ごとに 1 つ進める）
    let mut rotation = 0usize;
    let mut tasks: JoinSet<()> = JoinSet::new();
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        // バックオフ中などに cancel された queued を、claim する前に cancelled へ送る。同じ
        // トランザクションで、終端を書けずに running に残った行（実行中表に無い）も回収する（D-76）
        let tracked = jobs.running_ids();
        match jobs
            .db()
            .write(move |c| {
                let tx = c.transaction()?;
                let now = now_epoch();
                let cancelled = dbjobs::sweep_cancel_requested(&tx, now)?;
                let orphans = dbjobs::sweep_orphaned_running(&tx, &tracked, now)?;
                tx.commit()?;
                Ok((cancelled, orphans))
            })
            .await
        {
            Ok((cancelled, orphans)) => {
                if !orphans.is_empty() {
                    warn!(
                        requeued = ?orphans.requeued,
                        cancelled = ?orphans.cancelled,
                        locks_cleared = orphans.locks_cleared,
                        "終端を記録できないまま running に残っていたジョブを回収した"
                    );
                }
                for id in cancelled
                    .into_iter()
                    .chain(orphans.requeued)
                    .chain(orphans.cancelled)
                {
                    jobs.emit_job(id).await;
                }
            }
            Err(e) => warn!(error = %e, "cancel 要求済み queued の掃除と running の回収に失敗"),
        }
        for (ty, sem) in &other_slots {
            while claim_one(&jobs, &registry, *ty, sem, &budget, &mut tasks, &shutdown).await
                == Claimed::Yes
            {}
        }
        // CPU 系: 種別を 1 件ずつ順に回し、1 周で何も取れなくなるまで（予算切れ・空）繰り返す
        if !cpu_slots.is_empty() {
            let start = rotation % cpu_slots.len();
            rotation = rotation.wrapping_add(1);
            loop {
                let mut progressed = false;
                for i in 0..cpu_slots.len() {
                    let (ty, sem) = &cpu_slots[(start + i) % cpu_slots.len()];
                    if claim_one(&jobs, &registry, *ty, sem, &budget, &mut tasks, &shutdown).await
                        == Claimed::Yes
                    {
                        progressed = true;
                    }
                }
                if !progressed {
                    break;
                }
            }
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

/// [`claim_one`] の結果。`No` は空き無し・予算無し・queued 無し・停止・claim 失敗のいずれか
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Claimed {
    Yes,
    No,
}

/// 種別 `ty` の queued を 1 件 claim して起動する。空きスロットが無い・予算が無い・queued が無ければ
/// 何もしない
async fn claim_one(
    jobs: &Arc<Jobs>,
    registry: &Arc<Registry>,
    ty: JobType,
    sem: &Arc<Semaphore>,
    budget: &Arc<Semaphore>,
    tasks: &mut JoinSet<()>,
    shutdown: &CancellationToken,
) -> Claimed {
    // slots は registry から作るので必ずあるが、無ければ claim して放置するより何もしない
    let Some(handler) = registry.get(ty) else {
        return Claimed::No;
    };
    if shutdown.is_cancelled() {
        return Claimed::No;
    }
    let Ok(permit) = Arc::clone(sem).try_acquire_owned() else {
        return Claimed::No;
    };
    // CPU 系は共通の予算も要る（D-73）。取れなければ種別の許可も手放して次の周回に回す
    // （実行中のタスクが終わると wake が来る）
    let budget_permit = if ty.cpu_bound() {
        match Arc::clone(budget).try_acquire_owned() {
            Ok(p) => Some(p),
            Err(_) => return Claimed::No,
        }
    } else {
        None
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
                return Claimed::No;
            }
            // 実行中表への登録は spawn の前、claim と同じ流れで行う（回収が「claim 済みだが
            // 未登録」の瞬間に本物を戻さないため。D-76）
            let token = CancellationToken::new();
            jobs.track_running(job.id, token.clone());
            tasks.spawn(execute(
                Arc::clone(jobs),
                Arc::clone(&handler),
                job,
                token,
                permit,
                budget_permit,
            ));
            Claimed::Yes
        }
        Ok(None) => Claimed::No,
        Err(e) => {
            warn!(%ty, error = %e, "ジョブの claim に失敗");
            Claimed::No
        }
    }
}

/// 1 件を実行して終端まで進める。`token` は claim 時に実行中表へ登録済み。permit（種別と、
/// CPU 系なら共通予算）はこの関数の終わりで返る
async fn execute(
    jobs: Arc<Jobs>,
    handler: Arc<dyn Handler>,
    job: Job,
    token: CancellationToken,
    _permit: OwnedSemaphorePermit,
    _budget_permit: Option<OwnedSemaphorePermit>,
) {
    let id = job.id;
    let ty = job.job_type;
    // 登録の解除は drop で行う（この関数自体が panic しても実行中表に残らず、回収の対象になる）
    let _untrack = Untrack {
        jobs: Arc::clone(&jobs),
        id,
    };
    jobs.emit_job(id).await;

    let finished = run_one(&jobs, handler, job, token).await;
    if let Err(e) = finished {
        // ここに来るのは DB 障害（終端の書き込みは再試行済み）。running のまま固着させず、
        // 失敗として記録を試みる
        warn!(job_id = id, %ty, error = %e, "ジョブの終端処理に失敗。failed として記録する");
        let message = format!("終端処理に失敗: {e:#}");
        let r = jobs
            .db()
            .write(move |c| {
                let tx = c.transaction()?;
                dbjobs::release_track_locks(&tx, id)?;
                dbjobs::release_mutexes(&tx, id)?;
                dbjobs::mark_failed(&tx, id, &message, now_epoch())?;
                tx.commit()?;
                Ok(())
            })
            .await;
        if let Err(e) = r {
            warn!(job_id = id, %ty, error = %e, "failed への記録もできない。running のまま手放し、稼働中の回収で queued に戻す");
        }
    }

    // 登録を外した後は回収の対象になる（終端を書けていればもう running ではないので無関係）
    drop(_untrack);
    jobs.emit_job(id).await;
    jobs.wake().notify_one();
}

/// drop で実行中表から外す（[`execute`] の終端。D-76）
struct Untrack {
    jobs: Arc<Jobs>,
    id: i64,
}

impl Drop for Untrack {
    fn drop(&mut self) {
        self.jobs.untrack_running(self.id);
    }
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
                    dbjobs::release_mutexes(&tx, id)?;
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
    let terminal = Terminal::from(outcome);
    // 終端の書き込みは DB 障害（ディスク満杯等）で失敗し得る。ハンドラの仕事は済んでいるので、
    // 許可を持ったまま間隔を空けて書き直す（D-76）
    let mut delays = FINISH_RETRY_DELAYS.iter();
    loop {
        let terminal = terminal.clone();
        let r = db
            .write(move |c| finish(c, id, ty, unflushed, terminal, now_epoch()))
            .await;
        match r {
            Ok(()) => return Ok(()),
            Err(e) => match delays.next() {
                Some(secs) => {
                    warn!(job_id = id, %ty, error = %e, retry_in = secs, "終端の書き込みに失敗。再試行する");
                    tokio::time::sleep(Duration::from_secs(*secs)).await;
                }
                None => return Err(e),
            },
        }
    }
}

/// 終端遷移を 1 トランザクションで書く（未 flush の進捗・ロック解放・状態遷移）。失敗したら
/// 呼び出し側が丸ごとやり直す
fn finish(
    c: &mut rusqlite::Connection,
    id: i64,
    ty: JobType,
    unflushed: Option<(i64, i64)>,
    terminal: Terminal,
    now: i64,
) -> crate::db::Result<()> {
    let tx = c.transaction()?;
    if let Some((done, total)) = unflushed {
        dbjobs::update_progress(&tx, id, done, total)?;
    }
    dbjobs::release_track_locks(&tx, id)?;
    dbjobs::release_mutexes(&tx, id)?;
    match terminal {
        // 完了直前に cancel が来ていても完了が勝つ（仕事は済んでいる。D-36）
        Terminal::Done => {
            dbjobs::mark_done(&tx, id, now)?;
            debug!(job_id = id, %ty, "完了");
        }
        Terminal::Requeue => match dbjobs::requeue(&tx, id, now, REQUEUE_DELAY_SECS)? {
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
        Terminal::Cancelled => {
            dbjobs::mark_cancelled(&tx, id, now)?;
            info!(job_id = id, %ty, "キャンセルされた");
        }
        Terminal::Fatal(message) => {
            if dbjobs::mark_failed_fatally(&tx, id, &message, now)? {
                warn!(job_id = id, %ty, error = %message, "失敗。再試行しても変わらないので failed");
            } else {
                warn!(job_id = id, %ty, error = %message, "失敗したが running ではなかった");
            }
        }
        Terminal::Failed(message) => match dbjobs::mark_failed(&tx, id, &message, now)? {
            dbjobs::FailureOutcome::Retrying {
                run_after,
                attempts,
            } => {
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
        },
    }
    tx.commit()?;
    Ok(())
}

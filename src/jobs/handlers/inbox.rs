//! `inbox` ジョブ（SPEC §7.8 / §8、D-68、P2-10）。並列 1・固定キー。走査（`scan_inbox`）と、
//! `approved` の件の配置（`place_item`）を 1 本で行う（配置中の件を走査が触る競合を作らない）。
//!
//! - 周期投入は `[inbox].poll_interval_secs`（0 で無し）。手動は `POST /api/inbox/scan`
//! - 件ごとに `library` の排他を取る（scan / gc / CD の配置と同じ）。取れなければ件を `approved` に
//!   戻して `Requeue`（次の実行で続きから）
//! - 失敗の扱い: Inbox 側が変わった → `pending`（再承認）、衝突・下書き不正・その他 → `failed`
//!   （理由を残す。件ごとで、ジョブは続行する）

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::db::inbox::{self as dbinbox, ItemState};
use crate::db::jobs as dbjobs;
use crate::db::now_epoch;
use crate::db::subscriptions;
use crate::import::inbox::{place_item, scan_inbox, InboxError, PlaceItemEnv};
use crate::import::scanner::resolve_album_artwork_now;
use crate::jobs::handlers::playlist_sync::new_sync_job;
use crate::jobs::EnqueueResult;
use crate::jobs::{
    BoxFuture, Handler, HandlerResult, JobContext, JobError, JobType, NewJob, Outcome,
};

pub const DEDUP_KEY: &str = "inbox";
const SCHEDULER_TICK: Duration = Duration::from_secs(30);

pub fn new_inbox_job() -> NewJob {
    NewJob::new(JobType::Inbox, serde_json::json!({})).dedup_key(DEDUP_KEY)
}

/// 周期投入。`interval_secs <= 0` なら何もしない
pub fn spawn_scheduler(
    jobs: Arc<crate::jobs::Jobs>,
    interval_secs: i64,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    if interval_secs <= 0 {
        return tokio::spawn(async {});
    }
    crate::jobs::scheduler::spawn_periodic(
        jobs,
        JobType::Inbox,
        new_inbox_job,
        interval_secs,
        SCHEDULER_TICK.min(Duration::from_secs(interval_secs as u64)),
        "Inbox の検出",
        shutdown,
    )
}

pub struct InboxHandler {
    env: Arc<PlaceItemEnv>,
}

impl InboxHandler {
    pub fn new(env: PlaceItemEnv) -> Self {
        Self { env: Arc::new(env) }
    }

    async fn run_inner(&self, ctx: &JobContext) -> HandlerResult {
        let job_id = ctx.job.id;
        let token = ctx.cancel_token();
        // 前のプロセスが配置の途中で落ちた件（placing）を approved に戻す。inbox ジョブは並列 1 なので、
        // ここで見える placing は必ず前の実行の残り。配置は冪等（音声の指紋で自分の成果物を採用する）
        let recovered = ctx.db().write(|c| dbinbox::recover_placing(c)).await?;
        if recovered > 0 {
            tracing::warn!(
                job_id,
                recovered,
                "配置の途中で止まっていた Inbox の件を配置待ちに戻した"
            );
        }
        let out = scan_inbox(&self.env.db, &self.env.inbox, now_epoch())
            .await
            .map_err(|e| JobError::Failed(anyhow::anyhow!("Inbox の走査に失敗: {e}")))?;
        tracing::info!(
            job_id,
            seen = out.items_seen,
            new = out.items_new,
            removed = out.items_removed,
            read = out.files_read,
            "Inbox を走査した"
        );
        let approved = ctx
            .db()
            .read(|c| dbinbox::list_by_state(c, ItemState::Approved))
            .await?;
        let total = approved.len() as i64;
        for (i, item) in approved.into_iter().enumerate() {
            ctx.check_cancel().await?;
            let _ = ctx.progress(i as i64, total).await;
            let id = item.id;
            if !ctx.lock_mutex("library").await? {
                tracing::info!(
                    job_id,
                    item_id = id,
                    "library の排他を取れないので後で続ける"
                );
                return Ok(Outcome::Requeue);
            }
            // approved → placing は CAS。一覧を読んでから排他を取るまでの間に却下 / 再開 / 走査で
            // 動かされた件はここで外れる（上書きして配置しない）
            let claimed = ctx
                .db()
                .write(move |c| {
                    dbinbox::transition(
                        c,
                        id,
                        &[ItemState::Approved],
                        ItemState::Placing,
                        None,
                        now_epoch(),
                    )
                })
                .await?;
            let result = if claimed {
                Some(place_item(&self.env, &item, &token).await)
            } else {
                tracing::info!(job_id, item_id = id, "承認が取り消されたので配置しない");
                None
            };
            // アートワークは次のスキャンを待たずに決める（P3-4）。**library の排他を持ったまま**行う:
            // 解放してからだと、並行する scan が同じ album を予約 → 解決した後に、こちらの古い読み取り
            // 結果で上書きし得る（ファイルはロックしないので、その間の外部の書き換えは scan だけが見る）。
            // 失敗しても予約が残り、次のスキャンが拾う
            if let Some(Ok(p)) = &result {
                if let Some(store) = &self.env.artwork {
                    if let Some(hook) = &self.env.before_artwork {
                        hook();
                    }
                    match resolve_album_artwork_now(
                        &self.env.db,
                        &self.env.library,
                        store,
                        p.album_id,
                    )
                    .await
                    {
                        Ok(jobs) => self.env.jobs.notify_enqueued(&jobs).await,
                        Err(e) => tracing::warn!(
                            job_id,
                            album_id = p.album_id,
                            error = %e,
                            "配置した album のアートワークを解決できない（次のスキャンで続き）"
                        ),
                    }
                }
            }
            if let Err(e) = ctx
                .db()
                .write(move |c| dbjobs::release_mutexes(c, job_id))
                .await
            {
                tracing::warn!(job_id, error = %e, "library の排他を解放できない");
            }
            let Some(result) = result else {
                continue;
            };
            match result {
                Ok(p) => {
                    // placed は登録トランザクションの中で確定済み（place_item）
                    tracing::info!(job_id, item_id = id, album_id = p.album_id, "配置済み");
                    // 購読由来なら追記先を束ねて同期を要求する（番号揃えの後続。P4-16、D-78）。
                    // latch を書いてから投入するので、走行中の同期があっても要求は失われない
                    for sid in p.subscription_ids {
                        let album_id = p.album_id;
                        ctx.db()
                            .write(move |c| {
                                let _ = subscriptions::bind_album(c, sid, album_id)?;
                                subscriptions::request_sync(c, sid, now_epoch())
                            })
                            .await?;
                        match self.env.jobs.enqueue(new_sync_job(sid)).await? {
                            EnqueueResult::Inserted(sync_id) => {
                                tracing::info!(
                                    job_id,
                                    subscription_id = sid,
                                    sync_job = sync_id,
                                    "配置の後続で同期を投入"
                                )
                            }
                            EnqueueResult::Duplicate(_) => {}
                        }
                    }
                }
                Err(InboxError::Cancelled) => {
                    ctx.db()
                        .write(move |c| {
                            dbinbox::set_state(c, id, ItemState::Approved, None, now_epoch())
                        })
                        .await?;
                    return Err(JobError::Cancelled);
                }
                Err(InboxError::Changed(what)) => {
                    let msg = format!("Inbox のファイルが変わったので再承認が必要: {what}");
                    tracing::warn!(job_id, item_id = id, %msg);
                    ctx.db()
                        .write(move |c| {
                            dbinbox::set_state(c, id, ItemState::Pending, Some(&msg), now_epoch())
                        })
                        .await?;
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    tracing::warn!(job_id, item_id = id, error = %msg, "Inbox の件を配置できない");
                    ctx.db()
                        .write(move |c| {
                            dbinbox::set_state(c, id, ItemState::Failed, Some(&msg), now_epoch())
                        })
                        .await?;
                }
            }
        }
        let _ = ctx.progress(total, total).await;
        Ok(Outcome::Done)
    }
}

impl Handler for InboxHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let this = Self {
            env: Arc::clone(&self.env),
        };
        Box::pin(async move { this.run_inner(&ctx).await })
    }
}

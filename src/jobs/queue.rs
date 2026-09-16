//! プロセス内のジョブハンドル。投入・キャンセル・再試行・一覧・イベント購読を提供し、
//! ワーカー（`worker.rs`）を起動する。API ハンドラと将来のハンドラ群はこれ経由で触る

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, Notify};
use tokio_util::sync::CancellationToken;

use crate::db::jobs as dbjobs;
use crate::db::{now_epoch, Db, Result};

use super::{
    CancelOutcome, EnqueueResult, Event, Job, JobEvent, NewJob, Registry, RetryOutcome, Summary,
};

/// broadcast チャネルの容量。遅い購読者は Lagged を受け取り、SSE 側で読み飛ばす
pub const EVENT_CAPACITY: usize = 1024;
/// `GET /api/jobs` が返す最大件数。未完了が先に並ぶので、溢れるのは古い終端行だけ
pub const LIST_LIMIT: usize = 1000;

pub struct Jobs {
    db: Arc<Db>,
    events: broadcast::Sender<Event>,
    wake: Notify,
    /// 実行中ジョブの token。API のキャンセルで倒す（DB の `cancel_requested_at` が正で、
    /// これは速く止めるための補助）
    running: Mutex<HashMap<i64, CancellationToken>>,
    cpus: usize,
}

impl Jobs {
    pub fn new(db: Arc<Db>) -> Arc<Self> {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Arc::new(Self {
            db,
            events,
            wake: Notify::new(),
            running: Mutex::new(HashMap::new()),
            cpus,
        })
    }

    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    pub(super) fn cpus(&self) -> usize {
        self.cpus
    }

    /// ワーカーを起動する。`shutdown` を倒すと新規 claim を止め、実行中のタスクを破棄する
    /// （DB 上は `running` のまま残り、次回起動のリカバリで `queued` に戻る）
    pub fn start(
        self: &Arc<Self>,
        registry: Registry,
        shutdown: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(super::worker::run(Arc::clone(self), registry, shutdown))
    }

    pub async fn enqueue(&self, job: NewJob) -> Result<EnqueueResult> {
        let result = self
            .db
            .write(move |c| dbjobs::enqueue(c, &job, now_epoch()))
            .await?;
        if let EnqueueResult::Inserted(id) = result {
            self.emit_job(id).await;
            self.wake.notify_one();
        }
        Ok(result)
    }

    pub async fn get(&self, id: i64) -> Result<Option<Job>> {
        self.db.read(move |c| dbjobs::get(c, id)).await
    }

    pub async fn list(&self) -> Result<(Vec<Job>, Summary)> {
        self.db
            .read(|c| dbjobs::list_with_summary(c, LIST_LIMIT))
            .await
    }

    pub async fn cancel(&self, id: i64) -> Result<CancelOutcome> {
        let outcome = self
            .db
            .write(move |c| dbjobs::request_cancel(c, id, now_epoch()))
            .await?;
        match outcome {
            CancelOutcome::Requested => {
                let token = self
                    .running
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&id)
                    .cloned();
                if let Some(token) = token {
                    token.cancel();
                }
            }
            CancelOutcome::Cancelled => self.emit_job(id).await,
            CancelOutcome::NotFound | CancelOutcome::NotCancellable => {}
        }
        Ok(outcome)
    }

    pub async fn retry(&self, id: i64) -> Result<RetryOutcome> {
        let outcome = self.db.write(move |c| dbjobs::retry(c, id)).await?;
        if outcome == RetryOutcome::Requeued {
            self.emit_job(id).await;
            self.wake.notify_one();
        }
        Ok(outcome)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// 任意のイベントを流す（batch / library はそれぞれの機能から呼ぶ）
    pub fn publish(&self, event: Event) {
        // 購読者がいないときの Err は正常
        let _ = self.events.send(event);
    }

    /// DB の現在値からジョブイベントを流す
    pub(super) async fn emit_job(&self, id: i64) {
        match self.get(id).await {
            Ok(Some(job)) => self.publish(Event::Job(JobEvent::of(&job))),
            Ok(None) => {}
            Err(e) => tracing::warn!(job_id = id, error = %e, "ジョブイベントの読み出しに失敗"),
        }
    }

    pub(super) fn wake(&self) -> &Notify {
        &self.wake
    }

    pub(super) fn track_running(&self, id: i64, token: CancellationToken) {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, token);
    }

    pub(super) fn untrack_running(&self, id: i64) {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }
}

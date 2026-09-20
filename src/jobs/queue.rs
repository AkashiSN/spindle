//! プロセス内のジョブハンドル。投入・キャンセル・再試行・一覧・イベント購読を提供し、
//! ワーカー（`worker.rs`）を起動する。API ハンドラと将来のハンドラ群はこれ経由で触る

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, Notify};
use tokio_util::sync::CancellationToken;

use crate::db::jobs as dbjobs;
use crate::db::{now_epoch, Db, Result};

use super::{
    CancelOutcome, EnqueueResult, Event, Job, JobEvent, ListLimits, NewJob, Registry, RetryOutcome,
    Summary, TypeCounts,
};

/// broadcast チャネルの容量。遅い購読者は Lagged を受け取り、SSE 側で読み飛ばす
pub const EVENT_CAPACITY: usize = 1024;
/// `GET /api/jobs` が返す最大件数（状態ごと。実行中・待ち 1,000 / 完了 300 / 失敗・取り消し 300）
pub const LIST_LIMITS: ListLimits = ListLimits {
    active: 1000,
    done: 300,
    failed: 300,
};

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
        Self::with_cpus(db, cpus)
    }

    /// コア数を固定して作る（並列度のテスト用。0 は 1 に丸める）
    pub fn with_cpus(db: Arc<Db>, cpus: usize) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Arc::new(Self {
            db,
            events,
            wake: Notify::new(),
            running: Mutex::new(HashMap::new()),
            cpus: cpus.max(1),
        })
    }

    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    /// 論理コア数（種別ごとの並列度の元。`JobType::concurrency`）
    pub fn cpus(&self) -> usize {
        self.cpus
    }

    /// CPU 系（`JobType::cpu_bound`）が共有する並列予算（= コア数。D-73）
    pub fn cpu_budget(&self) -> usize {
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

    pub async fn list(&self) -> Result<(Vec<Job>, Summary, BTreeMap<String, TypeCounts>)> {
        self.db
            .read(|c| dbjobs::list_with_summary(c, LIST_LIMITS))
            .await
    }

    pub async fn cancel(&self, id: i64) -> Result<CancelOutcome> {
        let outcome = self
            .db
            .write(move |c| dbjobs::request_cancel(c, id, now_epoch()))
            .await?;
        match outcome {
            CancelOutcome::Requested => self.cancel_running_token(id),
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

    /// 別のトランザクションで直接 `jobs` に投入した後に呼ぶ（編集バッチの prepare 等）。
    /// イベントを流してワーカーを起こす
    pub async fn notify_enqueued(&self, ids: &[i64]) {
        self.notify_changed(ids).await;
        if !ids.is_empty() {
            self.wake.notify_one();
        }
    }

    /// 別のトランザクションで状態を変えたジョブのイベントを流す
    pub async fn notify_changed(&self, ids: &[i64]) {
        for id in ids {
            self.emit_job(*id).await;
        }
    }

    /// 実行中ジョブの token を倒す（DB の `cancel_requested_at` は呼び出し側が立てている）
    pub fn cancel_running_token(&self, id: i64) {
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

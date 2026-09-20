//! ジョブシステム（SPEC §8）。
//!
//! - 状態は DB の `jobs` 表が正。プロセス内の状態（実行中の CancellationToken、進捗の
//!   スロットリング）はすべて再起動で消えてよいものだけ
//! - `queued` → `running` は種別ごとの Semaphore で並列度を絞り、`claim_next` で 1 件ずつ取る
//! - キャンセルは協調方式。API が `cancel_requested_at` を立て、実行中なら token も倒す。
//!   ハンドラは [`JobContext::progress`] / [`JobContext::check_cancel`] で確認して自発的に止まる
//! - 失敗は `run_after` に指数バックオフを永続化し、`max_attempts` で `failed`
//! - 版付きジョブ（tagwrite / transcode）は開始直前に stale 判定し、古ければ no-op で `done`
//!
//! ハンドラ本体は `handlers/` に種別ごとに置く（P0-6 以降）。ここにあるのは実行基盤だけ

mod context;
pub mod handlers;
pub mod process;
mod queue;
pub mod recovery;
pub mod scheduler;
mod worker;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Serialize;

pub use crate::db::jobs::{
    backoff_secs, CancelOutcome, EnqueueResult, Job, JobState, JobType, ListLimits, NewJob,
    RecoveryReport, RetryOutcome, Summary, TypeCounts,
};
pub use context::{JobContext, TempGuard};
pub use queue::{Jobs, EVENT_CAPACITY, LIST_LIMITS};

/// ハンドラが返す future
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// ハンドラの正常終了
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 完了。`done` にする
    Done,
    /// 前提（track_locks 等）が取れなかった。試行回数を数えずに `queued` へ戻す
    Requeue,
}

/// ハンドラの異常終了
#[derive(Debug, thiserror::Error)]
pub enum JobError {
    /// キャンセル要求に応じて止まった。`cancelled` にする
    #[error("キャンセルされた")]
    Cancelled,
    /// 失敗。バックオフして再試行し、上限で `failed`
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
    /// 再試行しても変わらない失敗（取り込み済み等。D-70）。バックオフせず直ちに `failed`
    #[error(transparent)]
    Fatal(anyhow::Error),
}

impl From<crate::db::DbError> for JobError {
    fn from(e: crate::db::DbError) -> Self {
        JobError::Failed(e.into())
    }
}

impl From<std::io::Error> for JobError {
    fn from(e: std::io::Error) -> Self {
        JobError::Failed(e.into())
    }
}

pub type HandlerResult = Result<Outcome, JobError>;

/// 種別ごとの実行本体。`JobContext` は安価に clone できる
pub trait Handler: Send + Sync + 'static {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult>;
}

struct FnHandler<F>(F);

impl<F, Fut> Handler for FnHandler<F>
where
    F: Fn(JobContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = HandlerResult> + Send + 'static,
{
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        Box::pin((self.0)(ctx))
    }
}

/// 種別 → ハンドラ。登録された種別だけがワーカーの対象になる
#[derive(Default)]
pub struct Registry {
    handlers: HashMap<JobType, Arc<dyn Handler>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, ty: JobType, handler: Arc<dyn Handler>) -> &mut Self {
        self.handlers.insert(ty, handler);
        self
    }

    pub fn register_fn<F, Fut>(&mut self, ty: JobType, f: F) -> &mut Self
    where
        F: Fn(JobContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HandlerResult> + Send + 'static,
    {
        self.register(ty, Arc::new(FnHandler(f)))
    }

    pub fn get(&self, ty: JobType) -> Option<Arc<dyn Handler>> {
        self.handlers.get(&ty).cloned()
    }

    pub fn types(&self) -> impl Iterator<Item = JobType> + '_ {
        self.handlers.keys().copied()
    }
}

/// SSE `/api/events` に流すイベント（SPEC §9）。種別名が SSE の `event:` になる
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Job(JobEvent),
    Batch(BatchEvent),
    Library(LibraryEvent),
    /// プレイリストの項目が（再評価で）書き換わった。表示中なら表を取り直す（P1-7、D-54）
    Playlist(PlaylistEvent),
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Event::Job(_) => "job",
            Event::Batch(_) => "batch",
            Event::Library(_) => "library",
            Event::Playlist(_) => "playlist",
        }
    }

    pub fn data(&self) -> serde_json::Value {
        // Serialize が失敗するのは型がおかしいときだけ。空オブジェクトに倒す
        let r = match self {
            Event::Job(e) => serde_json::to_value(e),
            Event::Batch(e) => serde_json::to_value(e),
            Event::Library(e) => serde_json::to_value(e),
            Event::Playlist(e) => serde_json::to_value(e),
        };
        r.unwrap_or_else(|_| serde_json::json!({}))
    }
}

/// 項目が書き換わったプレイリスト
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlaylistEvent {
    pub playlist_ids: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobEvent {
    pub id: i64,
    pub state: JobState,
    pub progress: Option<f64>,
    pub done: Option<i64>,
    pub total: Option<i64>,
}

impl JobEvent {
    pub fn of(job: &Job) -> Self {
        Self {
            id: job.id,
            state: job.state,
            progress: job.progress,
            done: job.done,
            total: job.total,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BatchEvent {
    pub id: i64,
    pub state: String,
    pub applied: i64,
    pub conflict: i64,
    pub failed: i64,
}

/// SSE `library` イベントを `ids` で流す変更行数の上限。超えたら `bulk`（SPEC §9）
pub const LIBRARY_IDS_MAX: usize = 200;

/// scan と gc が取り合う名前付き排他（`job_mutexes`。D-56）。取れた側だけが走る
pub const LIBRARY_MUTEX: &str = "library";

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LibraryEvent {
    Ids {
        scan_run_id: i64,
        track_ids: Vec<i64>,
    },
    Bulk {
        scan_run_id: i64,
    },
}

impl LibraryEvent {
    /// 変更行の集合からイベントを作る。変更が無ければ None（何も流さない）。
    /// `LIBRARY_IDS_MAX` 以下なら `ids`、超えたら `bulk`
    pub fn from_changes(scan_run_id: i64, mut track_ids: Vec<i64>) -> Option<LibraryEvent> {
        track_ids.sort_unstable();
        track_ids.dedup();
        if track_ids.is_empty() {
            None
        } else if track_ids.len() <= LIBRARY_IDS_MAX {
            Some(LibraryEvent::Ids {
                scan_run_id,
                track_ids,
            })
        } else {
            Some(LibraryEvent::Bulk { scan_run_id })
        }
    }
}

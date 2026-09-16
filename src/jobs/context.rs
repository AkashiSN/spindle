//! ハンドラに渡す実行文脈: 進捗の永続化と配信、協調キャンセル、track_locks、tmp の後始末

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::db::jobs as dbjobs;
use crate::db::{now_epoch, Db, Result};

use super::{Event, Job, JobError, JobEvent, JobState, Jobs};

/// 進捗を DB に書く最短間隔。配信（SSE）は毎回、永続化はこの間隔で間引く
const PERSIST_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub struct JobContext {
    pub job: Arc<Job>,
    jobs: Arc<Jobs>,
    token: CancellationToken,
    progress: Arc<Mutex<ProgressState>>,
}

#[derive(Default)]
struct ProgressState {
    last: Option<(i64, i64)>,
    persisted_at: Option<Instant>,
    persisted: Option<(i64, i64)>,
}

impl JobContext {
    pub(super) fn new(job: Job, jobs: Arc<Jobs>, token: CancellationToken) -> Self {
        Self {
            job: Arc::new(job),
            jobs,
            token,
            progress: Arc::default(),
        }
    }

    pub fn db(&self) -> &Arc<Db> {
        self.jobs.db()
    }

    pub fn jobs(&self) -> &Arc<Jobs> {
        &self.jobs
    }

    /// キャンセル要求で倒れる token。外部プロセスの待ちや長い I/O の `select!` に使う
    pub fn cancel_token(&self) -> CancellationToken {
        self.token.clone()
    }

    pub fn is_cancel_requested(&self) -> bool {
        self.token.is_cancelled()
    }

    /// キャンセル要求があれば `Err(Cancelled)`。DB の `cancel_requested_at` も確認する
    pub async fn check_cancel(&self) -> std::result::Result<(), JobError> {
        if self.token.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        let id = self.job.id;
        if self
            .db()
            .read(move |c| dbjobs::is_cancel_requested(c, id))
            .await?
        {
            self.token.cancel();
            return Err(JobError::Cancelled);
        }
        Ok(())
    }

    /// 進捗を報告する。SSE には毎回流し、DB へは間引いて書く。
    /// キャンセル要求があれば `Err(Cancelled)` を返すので、ハンドラは `?` で抜ければよい
    pub async fn progress(&self, done: i64, total: i64) -> std::result::Result<(), JobError> {
        if self.token.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        let should_persist = {
            let mut st = self.progress.lock().unwrap_or_else(|e| e.into_inner());
            st.last = Some((done, total));
            let due = st
                .persisted_at
                .is_none_or(|t| t.elapsed() >= PERSIST_INTERVAL);
            let finished = total > 0 && done >= total;
            if due || finished {
                st.persisted_at = Some(Instant::now());
                true
            } else {
                false
            }
        };
        let progress = if total > 0 {
            Some((done as f64 / total as f64).clamp(0.0, 1.0))
        } else {
            None
        };
        self.jobs.publish(Event::Job(JobEvent {
            id: self.job.id,
            state: JobState::Running,
            progress,
            done: Some(done),
            total: Some(total),
        }));
        if should_persist {
            let id = self.job.id;
            let cancel_requested = self
                .db()
                .write(move |c| {
                    dbjobs::update_progress(c, id, done, total)?;
                    dbjobs::is_cancel_requested(c, id)
                })
                .await?;
            self.progress
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .persisted = Some((done, total));
            if cancel_requested {
                self.token.cancel();
                return Err(JobError::Cancelled);
            }
        }
        Ok(())
    }

    /// まだ DB に書いていない最後の進捗（終端遷移と同じトランザクションでワーカーが書く）
    pub(super) fn take_unflushed_progress(&self) -> Option<(i64, i64)> {
        let mut st = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        match (st.last, st.persisted) {
            (Some(last), persisted) if Some(last) != persisted => {
                st.persisted = Some(last);
                Some(last)
            }
            _ => None,
        }
    }

    /// 版付きジョブが stale か（payload の版 < 現在値、またはトラックが無い）。
    /// 基盤はハンドラ起動前にロックを取ったうえで判定済みだが、ハンドラが追加のロックを
    /// 取った後に再確認したいときに使う。版を持たない種別では常に `false`
    pub async fn is_stale(&self) -> Result<bool> {
        let job = Arc::clone(&self.job);
        self.db().read(move |c| dbjobs::is_stale(c, &job)).await
    }

    /// `track_ids` を昇順に全件ロックする。取れなければ `false`（ハンドラは
    /// `Outcome::Requeue` を返す）。ロックはジョブ終了時にワーカーが全解放する
    pub async fn lock_tracks(&self, track_ids: &[i64]) -> Result<bool> {
        let id = self.job.id;
        let ids = track_ids.to_vec();
        self.db()
            .write(move |c| dbjobs::acquire_track_locks(c, id, &ids, now_epoch()))
            .await
    }

    /// drop 時に消える一時ファイルのガード。成果物として残すなら [`TempGuard::keep`]
    pub fn temp_file(&self, path: impl AsRef<Path>) -> TempGuard {
        TempGuard::new(path)
    }
}

/// drop されると対象パスを削除する。キャンセル・失敗・panic のどれでも tmp を残さないための道具
#[must_use = "束縛しないと即座に削除される"]
pub struct TempGuard {
    path: PathBuf,
    keep: bool,
}

impl TempGuard {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            keep: false,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 成果物として確定したので削除しない
    pub fn keep(mut self) -> PathBuf {
        self.keep = true;
        std::mem::take(&mut self.path)
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %self.path.display(), error = %e, "一時ファイルを消せない")
            }
        }
    }
}

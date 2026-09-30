//! スマートプレイリストの自動再評価と、記録済みプロファイルへの自動再書き出し（SPEC §10、D-54）。
//!
//! プロセス内の常駐タスク。`library` イベント・終端の `batch` イベント・完了した job を合図に
//! `[export].autoexport_debounce_sec` だけ待って（その間の合図はまとめて）1 回走る:
//!
//! 1. 全 smart を再評価して、並びが変わったものだけ `playlist_items` を書き直す
//! 2. `auto_export = 1` で書き出し記録のあるプレイリスト（手動も）を記録済みプロファイルへ書き直す
//!
//! 起動時にも 1 回走る（前回停止中の変更に追随）。ルールが正なので永続化はしない

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::db::{now_epoch, playlists as dbpl, Db};
use crate::domain::device::PendingSets;
use crate::fsroot::RootDir;
use crate::jobs::{Event, JobState, JobType, Jobs, PlaylistEvent};

use super::smart;
use super::writer;

pub struct AutoExport {
    db: Arc<Db>,
    jobs: Arc<Jobs>,
    root: Arc<RootDir>,
    debounce: Duration,
    reeval: Arc<ReevalFlag>,
}

impl AutoExport {
    pub fn new(
        db: Arc<Db>,
        jobs: Arc<Jobs>,
        root: Arc<RootDir>,
        debounce: Duration,
        reeval: Arc<ReevalFlag>,
    ) -> Self {
        Self {
            db,
            jobs,
            root,
            debounce,
            reeval,
        }
    }

    pub fn spawn(self, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run(shutdown).await })
    }

    async fn run(self, shutdown: CancellationToken) {
        let mut rx = self.jobs.subscribe();
        // 起動時に 1 回
        let mut deadline: Option<tokio::time::Instant> =
            Some(tokio::time::Instant::now() + self.debounce);
        loop {
            let sleep = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = sleep => {
                    deadline = None;
                    self.run_once().await;
                }
                ev = rx.recv() => match ev {
                    Ok(ev) if triggers(&ev) => {
                        if marks_reevaluation(&ev) {
                            self.reeval.mark();
                        }
                        deadline = Some(tokio::time::Instant::now() + self.debounce);
                    }
                    Ok(_) => {}
                    // 遅れて取りこぼしたときも、何かが変わったとして 1 回走る
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        self.reeval.mark();
                        deadline = Some(tokio::time::Instant::now() + self.debounce);
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                },
            }
        }
    }

    async fn run_once(&self) {
        let sets = match self.db.device_snapshot().await {
            Ok(s) => s.pending_sets(),
            Err(e) => {
                tracing::warn!(error = %e, "端末の状態を読めない（device_pending は空として評価する）");
                PendingSets::default()
            }
        };
        let refreshed = self
            .db
            .write(move |c| smart::refresh_all(c, now_epoch(), &sets))
            .await;
        match refreshed {
            Ok((evaluated, changed)) => {
                tracing::info!(
                    evaluated,
                    changed = changed.len(),
                    "スマートプレイリストを再評価した"
                );
                // 表示中の表を無効化する（UI は library イベントを既に処理済みで、この書き換えを知らない）
                if !changed.is_empty() {
                    self.jobs.publish(Event::Playlist(PlaylistEvent {
                        playlist_ids: changed,
                    }));
                }
                self.reeval.clear();
            }
            Err(e) => tracing::warn!(error = %e, "スマートプレイリストの再評価に失敗"),
        }
        let targets = match self.db.read(dbpl::auto_export_targets).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "自動再書き出しの対象を読めない");
                return;
            }
        };
        let mut written = 0;
        for (playlist_id, profile) in targets {
            match writer::export_to_root(&self.db, Arc::clone(&self.root), playlist_id, &profile)
                .await
            {
                Ok(w) => {
                    written += 1;
                    if w.stale_tags > 0 {
                        // 追随ジョブが終われば次の合図で書き直される
                        tracing::info!(
                            playlist_id,
                            profile,
                            stale_tags = w.stale_tags,
                            "タグ追随待ちの Derived を含めて書き出した"
                        );
                    }
                }
                Err(e) => tracing::warn!(playlist_id, profile, error = %e, "自動再書き出しに失敗"),
            }
        }
        if written > 0 {
            tracing::info!(written, "プレイリストを再書き出しした");
        }
    }
}

/// 再評価の合図になるイベント
fn triggers(ev: &Event) -> bool {
    match ev {
        Event::Library(_) => true,
        Event::Batch(b) => matches!(b.state.as_str(), "applied" | "partial" | "failed"),
        Event::Job(j) => j.state == JobState::Done,
        Event::Playlist(_) => false,
    }
}

/// 端末の差分に「評価待ち」を出す合図になるイベント（[`triggers`] の部分集合）。
/// 端末の選曲に載るプレイリストは端末のフィールド（`on_device` / `device_pending`）を使えない
/// （③ 循環の禁止）ので、端末の状態だけを変えるジョブ（`source_hash`）や、ルールが引く値を変えない
/// ジョブ（`thumbnail`）の完了では立てない。再評価そのもの（[`triggers`]）はこれまでどおり走る
/// （`device_pending` を使うプレイリストは source_hash の完了で結果が変わる）
fn marks_reevaluation(ev: &Event) -> bool {
    match ev {
        Event::Job(j) => {
            j.state == JobState::Done
                && !matches!(
                    j.job_type,
                    JobType::SourceHash
                        | JobType::Thumbnail
                        // 端末の状態だけを変えるジョブ
                        | JobType::DeviceScan
                        | JobType::DeviceSync
                        | JobType::DeviceVerify
                )
        }
        other => triggers(other),
    }
}

/// スマートプレイリストの再評価待ち（仕様 ③「評価待ち」）。D-54 の常駐タスクは全件をまとめて評価するので
/// 1 ビットで持つ。永続化しない（起動時は dirty から始める）
#[derive(Debug, Default)]
pub struct ReevalFlag(std::sync::atomic::AtomicBool);

impl ReevalFlag {
    pub fn new_dirty() -> Self {
        ReevalFlag(std::sync::atomic::AtomicBool::new(true))
    }
    pub fn is_pending(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
    pub fn mark(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst)
    }
    pub fn clear(&self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::{marks_reevaluation, triggers, ReevalFlag};
    use crate::jobs::{Event, JobEvent, JobState, JobType, LibraryEvent};

    fn job_done(job_type: JobType) -> Event {
        Event::Job(JobEvent {
            id: 1,
            job_type,
            state: JobState::Done,
            progress: None,
            done: None,
            total: None,
            detail: None,
        })
    }

    #[test]
    fn reevaluation_mark_skips_jobs_that_only_change_device_state() {
        for t in [
            JobType::SourceHash,
            JobType::Thumbnail,
            JobType::DeviceScan,
            JobType::DeviceSync,
            JobType::DeviceVerify,
        ] {
            assert!(triggers(&job_done(t)), "再評価そのものは走る: {t:?}");
            assert!(
                !marks_reevaluation(&job_done(t)),
                "評価待ちは立てない: {t:?}"
            );
        }
        for t in [
            JobType::Tagwrite,
            JobType::Transcode,
            JobType::Rg,
            JobType::Scan,
        ] {
            assert!(marks_reevaluation(&job_done(t)), "{t:?}");
        }
        let running = Event::Job(JobEvent {
            state: JobState::Running,
            ..match job_done(JobType::Tagwrite) {
                Event::Job(j) => j,
                _ => unreachable!(),
            }
        });
        assert!(!marks_reevaluation(&running));
        assert!(marks_reevaluation(&Event::Library(LibraryEvent::Bulk {
            scan_run_id: 1
        })));
    }

    #[test]
    fn reeval_flag_starts_dirty_and_clears() {
        let f = ReevalFlag::new_dirty();
        assert!(f.is_pending());
        f.clear();
        assert!(!f.is_pending());
        f.mark();
        assert!(f.is_pending());
        assert!(!ReevalFlag::default().is_pending());
    }
}

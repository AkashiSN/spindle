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
use crate::fsroot::RootDir;
use crate::jobs::{Event, JobState, Jobs, PlaylistEvent};

use super::smart;
use super::writer;

pub struct AutoExport {
    db: Arc<Db>,
    jobs: Arc<Jobs>,
    root: Arc<RootDir>,
    debounce: Duration,
}

impl AutoExport {
    pub fn new(db: Arc<Db>, jobs: Arc<Jobs>, root: Arc<RootDir>, debounce: Duration) -> Self {
        Self {
            db,
            jobs,
            root,
            debounce,
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
                        deadline = Some(tokio::time::Instant::now() + self.debounce);
                    }
                    Ok(_) => {}
                    // 遅れて取りこぼしたときも、何かが変わったとして 1 回走る
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        deadline = Some(tokio::time::Instant::now() + self.debounce);
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                },
            }
        }
    }

    async fn run_once(&self) {
        let refreshed = self.db.write(|c| smart::refresh_all(c, now_epoch())).await;
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
                Ok(_) => written += 1,
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

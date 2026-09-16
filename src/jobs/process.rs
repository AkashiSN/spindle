//! 外部プロセスを自分のプロセスグループで起動し、キャンセル時にグループごと kill する。
//! 引数配列・`--` 前置・タイムアウト・終了コード検査を含む共通ラッパは P0-5 で
//! この上に載せる。
//!
//! 「グループが消えたか」は leader の終了ではなく `killpg(pgid, 0)` で判定する。leader が
//! TERM で死んでも、TERM を無視する孫が残っていればグループはまだ存在する（D-36）。

use std::process::ExitStatus;
use std::time::{Duration, Instant};

use rustix::process::{kill_process_group, test_kill_process_group, Pid, Signal};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use super::JobError;

/// SIGTERM 後にこれだけ待ってグループが残っていれば SIGKILL
const KILL_GRACE: Duration = Duration::from_secs(2);
/// SIGKILL 後にグループが消えるのを待つ上限（消えなければ諦めてログに残す）
const KILL_SETTLE: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(20);

pub struct ChildGroup {
    child: Child,
    pid: u32,
    /// leader を wait で回収済みか。回収後もグループにプロセスが残っていれば pgid は
    /// 再利用されない（参照中の pid は割り当てられない）ので `killpg` は安全だが、
    /// 回収済みなら Drop での無条件 SIGKILL はしない（送る相手が残っているとは限らない）
    reaped: bool,
}

impl ChildGroup {
    /// 新しいプロセスグループのリーダーとして起動する
    pub fn spawn(mut cmd: Command) -> std::io::Result<Self> {
        // kill_on_drop は leader だけが対象。グループは Drop で自前で kill する
        cmd.process_group(0).kill_on_drop(true);
        let child = cmd.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| std::io::Error::other("起動直後に pid が取れない"))?;
        Ok(Self {
            child,
            pid,
            reaped: false,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    fn pgid(&self) -> Option<Pid> {
        Pid::from_raw(self.pid as i32)
    }

    /// 終了を待つ。`token` が倒れたらプロセスグループを kill して `Err(Cancelled)`
    pub async fn wait(mut self, token: CancellationToken) -> Result<ExitStatus, JobError> {
        tokio::select! {
            status = self.child.wait() => {
                self.reaped = true;
                Ok(status?)
            }
            _ = token.cancelled() => {
                self.kill_group().await;
                Err(JobError::Cancelled)
            }
        }
    }

    /// プロセスグループへ SIGTERM、猶予後にグループが残っていれば SIGKILL。
    /// leader は zombie のままだと `killpg(pgid, 0)` で「存在」と数えられるので、待っている間も
    /// `try_wait` で回収し続ける。回収後にグループが残っていれば孫なので、そこへ SIGKILL を送る
    pub async fn kill_group(&mut self) {
        let Some(pgid) = self.pgid() else {
            return;
        };
        if let Err(e) = kill_process_group(pgid, Signal::TERM) {
            tracing::debug!(pid = self.pid, error = %e, "SIGTERM を送れない（既に終了か）");
        }
        if !self.wait_group_gone(pgid, KILL_GRACE).await {
            tracing::info!(pid = self.pid, "SIGTERM で終わらないのでグループへ SIGKILL");
            if let Err(e) = kill_process_group(pgid, Signal::KILL) {
                tracing::debug!(pid = self.pid, error = %e, "SIGKILL を送れない");
            }
            if !self.wait_group_gone(pgid, KILL_SETTLE).await {
                tracing::warn!(pid = self.pid, "SIGKILL 後もプロセスグループが残っている");
            }
        }
        // leader をまだ回収していなければここで必ず回収する
        if !self.reaped {
            let _ = self.child.wait().await;
            self.reaped = true;
        }
    }

    /// グループの全プロセスが消えるまで待つ。`within` 内に消えたら `true`。
    /// 待つ間に leader が終わっていれば回収する（zombie が残るとグループが消えない）
    async fn wait_group_gone(&mut self, pgid: Pid, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if !self.reaped {
                if let Ok(Some(_)) = self.child.try_wait() {
                    self.reaped = true;
                }
            }
            // ESRCH ならグループにプロセスがいない
            if test_kill_process_group(pgid).is_err() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

impl Drop for ChildGroup {
    /// wait を経ずに drop された（ハンドラの panic / 停止時の abort）ときはグループごと SIGKILL。
    /// leader を reap 済み（wait / kill_group を通った）なら後始末は済んでいるので何もしない
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        if let Some(pgid) = self.pgid() {
            if let Err(e) = kill_process_group(pgid, Signal::KILL) {
                tracing::debug!(pid = self.pid, error = %e, "drop 時の SIGKILL を送れない");
            }
        }
    }
}

//! エンジンの各段が共有する文脈。state は `Ctx` が持ち、`save()` で耐久化する。
//! `save()` の直後に中断点 `state.saved` を通る（試験は「k 回目の保存の直後で落ちる」を列挙する）

use agent_proto::{ManifestResponse, ProgressRequest, ReportError};

use crate::failpoint::Failpoints;
use crate::local::LocalRoot;
use crate::music::Music;
use crate::progress::{Progress, ProgressSent};
use crate::server::Server;
use crate::state::{State, StateFile};
use crate::Result;

/// 利用者とのやり取り（CLI は標準入出力、試験は台本）
pub trait Ui {
    fn info(&mut self, msg: &str);
    /// 差分を見せて y/N を聞く
    fn confirm(&mut self, m: &ManifestResponse) -> bool;
}

pub struct Ctx<'a, M: Music, S: Server> {
    pub music: &'a M,
    pub server: &'a S,
    pub local: &'a LocalRoot,
    pub file: &'a StateFile,
    pub fp: &'a Failpoints,
    pub ui: &'a mut dyn Ui,
    pub state: State,
    /// この実行で出た項目ごとのエラー（報告の `errors`）
    pub errors: Vec<ReportError>,
    /// 時刻（試験で固定できるように関数で持つ）
    pub now: fn() -> i64,
    /// sync の進捗（描画と spindle への送信）。sync 以外は `Progress::disabled()`
    pub progress: Progress,
}

impl<M: Music, S: Server> Ctx<'_, M, S> {
    pub fn save(&mut self) -> Result<()> {
        self.file.save(&self.state)?;
        self.fp.hit("state.saved")
    }

    /// 進捗を間引いて描き、時刻なら spindle へ送る（state には何も触らない）
    pub fn progress_tick(&self) {
        self.progress.tick(&|id, r| self.send_progress(id, r));
    }

    /// 報告の前の最後の進捗を送る
    pub fn progress_flush(&self) {
        self.progress.flush(&|id, r| self.send_progress(id, r));
    }

    /// 送信の失敗（通信エラー・5xx）は同期を止めない。`Failed` として間隔を延ばし、続けば送るのをやめる
    fn send_progress(&self, plan_id: i64, r: &ProgressRequest) -> ProgressSent {
        self.server
            .progress(plan_id, r)
            .unwrap_or(ProgressSent::Failed)
    }

    pub fn error_track(&mut self, track_id: i64, reason: impl Into<String>) {
        self.errors.push(ReportError {
            kind: agent_proto::ErrorKind::Track,
            ref_id: track_id,
            reason: reason.into(),
        });
    }

    pub fn error_playlist(&mut self, playlist_id: i64, reason: impl Into<String>) {
        self.errors.push(ReportError {
            kind: agent_proto::ErrorKind::Playlist,
            ref_id: playlist_id,
            reason: reason.into(),
        });
    }
}

pub fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

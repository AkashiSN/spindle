//! spindle-agent: spindle の端末（iPhone）の写しを Mac のミュージック.app へ反映する
//! （docs/superpowers/specs/2026-09-29-device-delivery-design.md ⑥、D-95・D-99・D-100）。
//! 外界は `Music`（ミュージック.app）・`Server`（spindle の `/api/agent/*`）・`Secrets`（トークン）の
//! trait に閉じ込め、ローカルの root と state.json は本物のファイルシステムで扱う

pub mod ctx;
pub mod failpoint;
pub mod local;
pub mod music;
pub mod pathkey;
pub mod plan;
pub mod rediscover;
pub mod secrets;
pub mod server;
pub mod state;

/// エージェントのエラー。`Stop` はユーザへの案内付きで止めるもの（終了コード 1）
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ミュージック.app の操作に失敗: {0}")]
    Music(String),
    #[error("spindle との通信に失敗: {0}")]
    Server(String),
    #[error("ファイル操作に失敗: {0}")]
    Io(#[from] std::io::Error),
    #[error("state.json を扱えない: {0}")]
    State(String),
    #[error("{0}")]
    Stop(String),
    /// 試験の中断点（`Failpoints`）。本番では起きない
    #[error("試験の中断点: {0}")]
    Crash(String),
}

pub type Result<T> = std::result::Result<T, Error>;

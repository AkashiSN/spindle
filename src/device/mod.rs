//! 端末への配信の実行側（docs/superpowers/specs/2026-09-29-device-delivery-design.md ⑤、D-95）。
//! 差分の判定は `domain::device`、キャッシュは `db::devices`。ここは端末側の正本（manifest・ジャーナル）を
//! 読み書きし、確定した計画を端末へ反映する。端末の操作は [`remote::DeviceFs`] で差し替える

pub mod adb;
pub mod credential;
pub mod journal;
pub mod ondevice;
pub mod plan;
pub mod quote;
pub mod recover;
pub mod remote;
pub mod report;
pub mod runtime;
pub mod store;
pub mod sync;
pub mod track;
pub mod verify;

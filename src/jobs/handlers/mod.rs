//! 種別ごとのジョブハンドラ。各タスクで追加する（scan は P0-6、tagwrite は P0-9 …）。
//! 実行基盤（`Registry` / `JobContext`）は親モジュールにある

pub mod backup;
pub mod flaccheck;
pub mod gc;
pub mod normalize;
pub mod rename;
pub mod rg;
pub mod scan;
pub mod tagwrite;
pub mod thumbnail;
pub mod transcode;
pub mod verify;

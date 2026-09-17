//! spindle: TrueNAS 上で動く単一コンテナの音楽ライブラリ管理アプリ。
//! 仕様は docs/SPEC.md、判断の理由は docs/DECISIONS.md

pub mod api;
pub mod config;
pub mod db;
pub mod domain;
pub mod edit;
pub mod fsroot;
pub mod gc;
pub mod import;
pub mod jobs;
pub mod logging;
pub mod media;
pub mod playlist;

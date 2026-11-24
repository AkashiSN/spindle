//! ブラウザからのアップロード（設計 docs/development/2026-11-25-upload-design.md §5）。
//!
//! 送ったファイルは Inbox 直下の隠しディレクトリ `.spindle-upload-<id>/` に置き、状態の正は
//! その中のマニフェスト `upload.json`（[`manifest`]）。DB に表は作らない（不変条件 1）。

pub mod manifest;

pub use manifest::*;

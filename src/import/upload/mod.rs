//! ブラウザからのアップロード（SPEC §7.12、D-116）。
//!
//! 送ったファイルは Inbox 直下の隠しディレクトリ `.spindle-upload-<id>/` に置き、状態の正は
//! その中のマニフェスト `upload.json`（[`manifest`]）。DB に表は作らない（不変条件 1）。

pub mod budget;
pub mod group;
pub mod manifest;
pub mod publish;
pub mod receive;
pub mod reconcile;
pub mod unpack;
pub mod view;

pub use budget::*;
pub use group::*;
pub use manifest::*;
pub use publish::*;
pub use receive::*;
pub use reconcile::*;
pub use unpack::*;
pub use view::*;

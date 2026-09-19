//! ytmusic 統合（SPEC §7.7）。タイトルの解釈はメタデータプラグイン（外部コマンド）に任せ、
//! spindle はプロトコルの往復とメタデータの写像だけを持つ（D-69）

pub mod downloader;
pub mod metadata;
pub mod sidecar;

pub use metadata::{Item, MetadataProvider, Outcome, ProviderError, Track};

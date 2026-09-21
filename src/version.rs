//! いまどの版が動いているか（P4-12、D-79）。`build.rs` が `SPINDLE_VERSION`（CI / build.sh が渡す
//! `git describe --tags --always`）を焼き、無ければ `git describe` を試し、それも無ければ `dev`。
//! `spindle --version` と `GET /health` の `version` が返す

/// ビルド時に確定した版の文字列（空にならない）
pub const VERSION: &str = env!("SPINDLE_VERSION");

/// Cargo.toml の version（参考。リリースタグとは独立）
pub const CARGO_VERSION: &str = env!("CARGO_PKG_VERSION");

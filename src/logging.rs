//! `tracing` の初期化。`RUST_LOG` で上書きでき、既定は `info`

use std::io::IsTerminal;

use tracing_subscriber::EnvFilter;

pub fn init() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info"));
    // コンテナのログ収集にエスケープ列が混ざらないよう、端末以外では色を切る
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_ansi(std::io::stdout().is_terminal())
        .init();
}

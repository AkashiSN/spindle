use std::path::PathBuf;

use anyhow::Context;
use tracing::info;

use spindle::{api, config::Config, db::migrations, logging};

/// `SPINDLE_CONFIG` 未設定時の設定ファイルパス（SPEC §14 環境変数）
const DEFAULT_CONFIG_PATH: &str = "/data/config.toml";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    logging::init();

    let config_path = std::env::var_os("SPINDLE_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
    let config = Config::load(&config_path)
        .with_context(|| format!("設定の読み込みに失敗: {}", config_path.display()))?;
    info!(config = %config_path.display(), library = %config.paths.library.display(), "設定を読み込んだ");

    let migrations = migrations::embedded().context("埋め込みマイグレーションの検証に失敗")?;
    info!(
        count = migrations.len(),
        latest = migrations.last().map(|m| m.version),
        "マイグレーションを確認した"
    );

    let listener = tokio::net::TcpListener::bind(config.server.listen)
        .await
        .with_context(|| format!("待ち受けに失敗: {}", config.server.listen))?;
    info!(listen = %config.server.listen, "HTTP サーバを開始");
    axum::serve(listener, api::router())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP サーバが異常終了")?;
    info!("停止した");
    Ok(())
}

/// SIGINT / SIGTERM で graceful shutdown する（コンテナ停止時に SIGTERM が来る）
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "SIGTERM ハンドラを登録できない。Ctrl-C のみ待つ");
            let _ = ctrl_c.await;
            return;
        }
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = term.recv() => {},
    }
    info!("停止シグナルを受け取った");
}

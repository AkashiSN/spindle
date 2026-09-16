use std::path::PathBuf;

use anyhow::Context;
use tracing::info;

use spindle::db::{migrations, Db};
use spindle::{api, config::Config, logging};

/// `SPINDLE_CONFIG` 未設定時の設定ファイルパス（SPEC §14 環境変数）
const DEFAULT_CONFIG_PATH: &str = "/data/config.toml";
/// `[paths].data` 直下の DB ファイル名（SPEC §5）
const DB_FILE_NAME: &str = "spindle.db";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    logging::init();

    let config_path = std::env::var_os("SPINDLE_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
    let config = Config::load(&config_path)
        .with_context(|| format!("設定の読み込みに失敗: {}", config_path.display()))?;
    info!(config = %config_path.display(), library = %config.paths.library.display(), "設定を読み込んだ");

    let db_path = config.paths.data.join(DB_FILE_NAME);
    let db = {
        let path = db_path.clone();
        tokio::task::spawn_blocking(move || Db::open(&path))
            .await
            .context("DB を開くタスクが異常終了")?
            .with_context(|| format!("DB を開けない: {}", db_path.display()))?
    };
    let version = db
        .read(|conn| Ok(migrations::current_version(conn)?))
        .await
        .context("スキーマ版の読み取りに失敗")?;
    info!(db = %db_path.display(), schema_version = ?version, "DB を開いた");
    // ルータへの受け渡し（AppState）は認証と一緒に P0-3 で入れる。ここでは寿命だけ持つ
    let _db = db;

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

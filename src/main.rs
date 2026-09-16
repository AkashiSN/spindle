use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tracing::info;

use spindle::api::{self, auth, AppState};
use spindle::db::{migrations, Db};
use spindle::{config::Config, logging};

/// `SPINDLE_CONFIG` 未設定時の設定ファイルパス（SPEC §14 環境変数）
const DEFAULT_CONFIG_PATH: &str = "/data/config.toml";
/// `[paths].data` 直下の DB ファイル名（SPEC §5）
const DB_FILE_NAME: &str = "spindle.db";
/// 初回起動時の管理パスワード（SPEC §14 環境変数、D-28）
const INITIAL_PASSWORD_ENV: &str = "SPINDLE_INITIAL_PASSWORD";

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
    let db = Arc::new(db);

    // 初期パスワードは DB に無い初回起動だけ読む（D-28）。読んだ後は環境から消す
    let initial_password = std::env::var(INITIAL_PASSWORD_ENV).ok();
    std::env::remove_var(INITIAL_PASSWORD_ENV);
    let mode = auth::bootstrap(&db, initial_password)
        .await
        .context("認証の初期化に失敗")?;

    let listen = config.server.listen;
    let state = AppState::new(Arc::new(config), db, mode);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("待ち受けに失敗: {listen}"))?;
    info!(%listen, ?mode, "HTTP サーバを開始");
    // 接続元アドレスを ConnectInfo で渡す（trusted_cidrs / trusted_proxies / レート制限の判定に使う）
    axum::serve(
        listener,
        api::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
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

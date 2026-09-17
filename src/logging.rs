//! `tracing` の初期化。`RUST_LOG` で上書きでき、既定は [`default_filter`]

use std::io::IsTerminal;

use tracing_subscriber::EnvFilter;

/// `RUST_LOG` 未設定時のフィルタ。symphonia / lofty はファイルごとの WARN（非対応の
/// メタデータ・軽微な破損など）を大量に出すので error に落とす（初回 deep scan で数千行になる。
/// P1-0）。読めなかった結果は spindle 側が自分のログと `ScanReport.errors` で報告する
pub fn default_filter() -> String {
    [
        "info",
        "tower_http=info",
        "lofty=error",
        "symphonia=error",
        "symphonia_core=error",
        "symphonia_common=error",
        "symphonia_metadata=error",
        "symphonia_bundle_flac=error",
        "symphonia_codec_aac=error",
        "symphonia_codec_alac=error",
        "symphonia_codec_pcm=error",
        "symphonia_format_ogg=error",
        "symphonia_format_riff=error",
    ]
    .join(",")
}

pub fn init() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter()));
    // コンテナのログ収集にエスケープ列が混ざらないよう、端末以外では色を切る
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_ansi(std::io::stdout().is_terminal())
        .init();
}

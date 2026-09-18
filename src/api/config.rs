//! `GET /api/config`（設定画面 SPEC §12.6、P1-12、D-58）。読み込んだ `config.toml` の原文をそのまま
//! 返す（コメントも含む。秘密は config に無い。初期パスワードは環境変数）。セッション必須

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use super::AppState;

#[derive(Serialize)]
pub struct ConfigView {
    /// 読み込んだファイルのパス。パス無しで組み立てた設定（テスト）なら null
    pub path: Option<String>,
    /// TOML の原文
    pub text: String,
}

pub async fn get(State(state): State<AppState>) -> Json<ConfigView> {
    Json(ConfigView {
        path: state
            .config
            .source_path
            .as_ref()
            .map(|p| p.display().to_string()),
        text: state.config.source.clone(),
    })
}

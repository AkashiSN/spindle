//! CD 取り込みの API（SPEC §9 `/api/cd/*`、§7.2）。P2-3 は照会だけ:
//! `POST /api/cd/lookup { toc }` — TOC 文字列（CTDB 形式 `0:13915:…:leadout` か MusicBrainz 形式
//! `1 12 leadout+150 offset+150 …`）から各種 DiscID を出し、MusicBrainz に照会して候補を返す。
//! ドライブからの TOC 取得（`GET /api/cd/status`）は P2-1 / P2-2 で、同じ文字列をここへ渡す。
//! 照会に失敗したら 502 `lookup_failed`、MusicBrainz が負荷制限（503）で通らない・クライアント
//! 未構成なら 503 `musicbrainz_unavailable`（不一致や 0 件は 200 で候補が空）

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::cd::musicbrainz::ReleaseCandidate;
use crate::cd::toc::Toc;
use crate::cd::LookupError;

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;

#[derive(Debug, Deserialize)]
pub struct LookupBody {
    pub toc: String,
}

#[derive(Debug, Serialize)]
pub struct LookupResponse {
    pub discid: String,
    /// MusicBrainz 形式の TOC（`ws/2/discid/-?toc=` に渡す形。表示・デバッグ用）
    pub mb_toc: String,
    pub accuraterip_id: String,
    pub ctdb_toc_id: String,
    /// DiscID そのもので引けた（false なら TOC の fuzzy 照会）
    pub exact: bool,
    pub candidates: Vec<ReleaseCandidate>,
}

pub async fn lookup(
    State(state): State<AppState>,
    body: Result<Json<LookupBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                e.body_text(),
            ))
        }
    };
    let toc = match Toc::parse(&body.toc) {
        Ok(t) => t,
        Err(e) => {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("TOC: {e}"),
            ))
        }
    };
    let Some(client) = state.musicbrainz.as_ref() else {
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "musicbrainz_unavailable",
        ));
    };
    let result = match client.lookup_disc(&toc).await {
        Ok(r) => r,
        // 再試行しても 503（負荷制限）: 使えないのは一時的で、間を置けば通る
        Err(LookupError::Status(503)) => {
            tracing::warn!("MusicBrainz が 503（負荷制限）。再試行しても通らない");
            return Ok(error_response_with_message(
                StatusCode::SERVICE_UNAVAILABLE,
                "musicbrainz_unavailable",
                "MusicBrainz が負荷制限中（503）。しばらく待って再試行",
            ));
        }
        Err(e) => {
            tracing::warn!(error = %e, "MusicBrainz の照会に失敗");
            return Ok(error_response_with_message(
                StatusCode::BAD_GATEWAY,
                "lookup_failed",
                e.to_string(),
            ));
        }
    };
    Ok(Json(LookupResponse {
        discid: result.discid,
        mb_toc: toc.musicbrainz_toc(),
        accuraterip_id: toc.accuraterip_id().to_string(),
        ctdb_toc_id: toc.ctdb_toc_id(),
        exact: result.exact,
        candidates: result.candidates,
    })
    .into_response())
}

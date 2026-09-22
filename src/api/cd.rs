//! CD 取り込みの API（SPEC §9 `/api/cd/*`、§7.2）。P2-3 は照会だけ:
//! `POST /api/cd/lookup { toc, isrcs?, mcn?, release? }` — TOC 文字列（CTDB 形式 `0:13915:…:leadout` か
//! MusicBrainz 形式 `1 12 leadout+150 offset+150 …`）から各種 DiscID を出し、MusicBrainz に照会して
//! 候補を返す。ISRC / MCN（status が読んだもの）と貼り付けたリリース URL でも引く（D-64 追記）。
//! `GET /api/cd/status`（P2-1 / P2-2）はポーラ（`cd::device`）が持つドライブの状態と TOC
//! （lookup に渡すのと同じ CTDB 形式の文字列）を返し、`POST /api/cd/eject` はトレイを開けて
//! 状態を見直す。ドライブが配線されていなければどちらも 503 `cd_unavailable`。
//! 照会に失敗したら 502 `lookup_failed`、MusicBrainz が負荷制限（503）で通らない・クライアント
//! 未構成なら 503 `musicbrainz_unavailable`（不一致や 0 件は 200 で候補が空）

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::cd::device::DriveState;
use crate::cd::musicbrainz::ReleaseCandidate;
use crate::cd::toc::Toc;
use crate::cd::LookupError;
use crate::db::now_epoch;

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub state: DriveState,
    /// ディスクがあって TOC を読めたら CTDB 形式の文字列（`POST /api/cd/lookup` にそのまま渡せる）
    pub toc: Option<String>,
    /// TOC と一緒に読んだ ISRC（音声トラック順。無いトラックは null）。TOC が無ければ空
    pub isrcs: Vec<Option<String>>,
    /// メディアカタログ番号（JAN / UPC）。入っていない盤は null
    pub mcn: Option<String>,
    /// 直近の失敗（開けない・TOC を読めない）
    pub error: Option<String>,
    /// 最後にドライブを見た時刻（epoch 秒）。まだなら 0
    pub checked_at: i64,
}

fn cd_unavailable() -> Response {
    error_response(StatusCode::SERVICE_UNAVAILABLE, "cd_unavailable")
}

pub async fn status(State(state): State<AppState>) -> Result<Response, ApiError> {
    let Some(cd) = state.cd.as_ref() else {
        return Ok(cd_unavailable());
    };
    let s = cd.monitor.snapshot();
    Ok(Json(StatusResponse {
        state: s.state,
        toc: s.toc.as_ref().map(Toc::ctdb_toc),
        isrcs: s.ids.isrcs,
        mcn: s.ids.mcn,
        error: s.error,
        checked_at: s.checked_at,
    })
    .into_response())
}

/// トレイを開ける。開けた直後にポーラの 1 周回を回して、次の周期を待たずに状態を反映する
pub async fn eject(State(state): State<AppState>) -> Result<Response, ApiError> {
    let Some(cd) = state.cd.clone() else {
        return Ok(cd_unavailable());
    };
    let result = tokio::task::spawn_blocking(move || {
        cd.monitor.eject_and_poll(cd.drive.as_ref(), now_epoch())
    })
    .await
    .map_err(|e| ApiError::Internal(format!("eject のタスクが異常終了: {e}")))?;
    match result {
        Ok(()) => Ok(StatusCode::NO_CONTENT.into_response()),
        Err(e) => {
            tracing::warn!(error = %e, "CD の eject に失敗");
            Ok(error_response_with_message(
                StatusCode::INTERNAL_SERVER_ERROR,
                "eject_failed",
                e.to_string(),
            ))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct LookupBody {
    pub toc: String,
    /// ディスクから読んだ ISRC（`GET /api/cd/status` の `isrcs`。読めなかったトラックは null）
    #[serde(default)]
    pub isrcs: Vec<Option<String>>,
    /// メディアカタログ番号（同 `mcn`）
    #[serde(default)]
    pub mcn: Option<String>,
    /// ユーザが貼った MusicBrainz のリリース URL か MBID
    #[serde(default)]
    pub release: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LookupResponse {
    pub discid: String,
    /// MusicBrainz 形式の TOC（`ws/2/discid/-?toc=` に渡す形。表示・デバッグ用）
    pub mb_toc: String,
    pub accuraterip_id: String,
    pub ctdb_toc_id: String,
    /// DiscID そのもので引けた（false なら TOC の fuzzy + ISRC + バーコード）
    pub exact: bool,
    /// 経路（`matched_by`）付きの候補。強い経路の順
    pub candidates: Vec<ReleaseCandidate>,
    /// 候補に入れられなかった理由（指定リリースが読めない・トラック数が合わない）
    pub notes: Vec<String>,
    /// TOC の音声トラック（番号と長さ）。候補が無くても手入力フォーム（P2-4）の行数と長さの元になる
    pub tracks: Vec<TocTrackInfo>,
}

#[derive(Debug, Serialize)]
pub struct TocTrackInfo {
    pub number: u8,
    /// セクタ数から（75 セクタ = 1 秒）
    pub length_ms: u64,
}

fn toc_tracks(toc: &Toc) -> Vec<TocTrackInfo> {
    toc.audio_track_sectors()
        .into_iter()
        .map(|(number, sectors)| TocTrackInfo {
            number,
            length_ms: u64::from(sectors) * 1000 / 75,
        })
        .collect()
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
    let isrcs: Vec<String> = body.isrcs.iter().flatten().cloned().collect();
    let query = crate::cd::musicbrainz::DiscQuery {
        toc: &toc,
        isrcs: &isrcs,
        mcn: body.mcn.as_deref().filter(|m| !m.trim().is_empty()),
        release: body.release.as_deref().filter(|r| !r.trim().is_empty()),
    };
    let result = match client.lookup(&query).await {
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
        notes: result.notes,
        tracks: toc_tracks(&toc),
    })
    .into_response())
}

//! CD 取り込みの API（SPEC §9 `/api/cd/*`、§7.2）。P2-3 は照会だけ:
//! `POST /api/cd/lookup { toc, isrcs?, mcn?, release?, refresh?, widen? }` — TOC 文字列（CTDB 形式 `0:13915:…:leadout` か
//! MusicBrainz 形式 `1 12 leadout+150 offset+150 …`）から各種 DiscID を出し、MusicBrainz に照会して
//! 候補を返す。ISRC / MCN（status が読んだもの）と貼り付けたリリース URL でも引く（D-64 追記）。
//! 経路は 3 段（`discid` → `ids` → `toc`）で、上の段で候補が残れば下は引かない（D-64 追記 4）。
//! 応答の `stage` がどこで止まったか、`can_widen` がまだ引いていない段があるかを示し、要求の
//! `widen` で段を打ち切らずに全部引く。
//! `GET /api/cd/status`（P2-1 / P2-2）はポーラ（`cd::device`）が持つドライブの状態と TOC
//! （lookup に渡すのと同じ CTDB 形式の文字列。音声トラックの番号と長さも `tracks` で返す。P4-20）を
//! 返し、`POST /api/cd/eject` はトレイを開けて
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
use crate::cd::musicbrainz::{LookupStage, ReleaseCandidate};
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
    /// TOC の音声トラック（番号と長さ）。TOC が読めていなければ空。
    /// 照会を待たずに CD 画面のトラック表を出すために返す（P4-20）
    pub tracks: Vec<TocTrackInfo>,
    /// TOC と一緒に読んだ ISRC（音声トラック順。無いトラックは null）。TOC が無ければ空
    pub isrcs: Vec<Option<String>>,
    /// メディアカタログ番号（JAN / UPC）。入っていない盤は null
    pub mcn: Option<String>,
    /// 直近の失敗（開けない・TOC を読めない）
    pub error: Option<String>,
    /// 最後にドライブを見た時刻（epoch 秒）。まだなら 0
    pub checked_at: i64,
    /// 進行中（queued / running）の吸い出しジョブ（P2-5。画面を開き直しても進捗を追えるように）
    pub rip_job: Option<i64>,
    /// ドライブの型番と、次に吸うときの読み取りオフセット（ディスクが無くても出る。D-83 追記）。
    /// 型番が読めていなければ null
    pub drive: Option<DriveInfo>,
}

#[derive(Debug, Serialize)]
pub struct DriveInfo {
    pub model: String,
    /// 次の吸い出しで当てるオフセット（サンプル）
    pub offset: i32,
    /// manual / learned / table / unknown（`OffsetSource`）
    pub offset_source: crate::cd::riplog::OffsetSource,
}

fn cd_unavailable() -> Response {
    error_response(StatusCode::SERVICE_UNAVAILABLE, "cd_unavailable")
}

pub async fn status(State(state): State<AppState>) -> Result<Response, ApiError> {
    let Some(cd) = state.cd.as_ref() else {
        return Ok(cd_unavailable());
    };
    let s = cd.monitor.snapshot();
    let tracks = s.toc.as_ref().map(toc_tracks).unwrap_or_default();
    let rip_job = state.db.read(active_rip_job).await?;
    let drive = match s.model.clone() {
        Some(model) => {
            let key = model.clone();
            let learned = state
                .db
                .read(move |c| crate::db::drive_offsets::get(c, &key))
                .await?
                .map(|l| l.offset);
            // 表は読み込み済みのものだけを見る（status でネットワークを待たない。起動時に読む）
            let table = state
                .drive_offsets
                .as_ref()
                .and_then(|t| t.peek(&model))
                .map(|e| e.offset);
            let (offset, offset_source) =
                crate::cd::rip::choose_offset(state.config.rip.drive_offset, learned, table);
            Some(DriveInfo {
                model,
                offset,
                offset_source,
            })
        }
        None => None,
    };
    Ok(Json(StatusResponse {
        state: s.state,
        toc: s.toc.as_ref().map(Toc::ctdb_toc),
        tracks,
        isrcs: s.ids.isrcs,
        mcn: s.ids.mcn,
        error: s.error,
        checked_at: s.checked_at,
        rip_job,
        drive,
    })
    .into_response())
}

fn active_rip_job(c: &rusqlite::Connection) -> crate::db::Result<Option<i64>> {
    use rusqlite::OptionalExtension as _;
    Ok(c.query_row(
        "SELECT id FROM jobs WHERE type = 'rip' AND state IN ('queued', 'running')
          ORDER BY id DESC LIMIT 1",
        [],
        |r| r.get(0),
    )
    .optional()?)
}

#[derive(Debug, Deserialize)]
pub struct RipBody {
    /// CTDB 形式の TOC（`GET /api/cd/status` の `toc`）。ドライブの盤と同じであること
    pub toc: String,
    /// CD 画面の下書き（`lib/cd.ts` の `finalizeDraft`）。名前は空でもよい（D-67 追記）
    pub metadata: crate::cd::metadata::DiscMetadata,
}

#[derive(Debug, Serialize)]
pub struct RipAccepted {
    pub job_id: i64,
}

/// 吸い出しを投入する（P2-5）。盤がドライブに入っていて TOC が一致すること。ドライブは 1 台なので
/// 進行中の吸い出しがあれば 409 `duplicate`
pub async fn rip(
    State(state): State<AppState>,
    body: Result<Json<RipBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Some(cd) = state.cd.as_ref() else {
        return Ok(cd_unavailable());
    };
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
                "bad_toc",
                e.to_string(),
            ))
        }
    };
    if let Err(e) = body.metadata.validate(&toc) {
        return Ok(error_response_with_message(
            StatusCode::BAD_REQUEST,
            "bad_metadata",
            e.to_string(),
        ));
    }
    let s = cd.monitor.snapshot();
    let in_drive = s.toc.as_ref().map(Toc::ctdb_toc);
    if s.state != DriveState::DiscOk || in_drive.as_deref() != Some(toc.ctdb_toc().as_str()) {
        return Ok(error_response_with_message(
            StatusCode::CONFLICT,
            "disc_mismatch",
            "ドライブに入っている盤がこの TOC と違う（入れ替えた / 取り出した）".to_owned(),
        ));
    }
    let job = crate::jobs::handlers::rip::new_rip_job(&toc, &body.metadata, &s.ids);
    Ok(match state.jobs.enqueue(job).await? {
        crate::jobs::EnqueueResult::Inserted(job_id) => {
            (StatusCode::ACCEPTED, Json(RipAccepted { job_id })).into_response()
        }
        crate::jobs::EnqueueResult::Duplicate(_) => {
            error_response(StatusCode::CONFLICT, "duplicate")
        }
    })
}

/// トレイを開ける。開けた直後にポーラの 1 周回を回して、次の周期を待たずに状態を反映する。
/// 吸い出し中（rip ジョブが running）は 409 `ripping`
pub async fn eject(State(state): State<AppState>) -> Result<Response, ApiError> {
    let Some(cd) = state.cd.clone() else {
        return Ok(cd_unavailable());
    };
    let ripping: bool = state
        .db
        .read(|c| {
            Ok(c.query_row(
                "SELECT EXISTS (SELECT 1 FROM jobs WHERE type = 'rip' AND state = 'running')",
                [],
                |r| r.get(0),
            )?)
        })
        .await?;
    if ripping {
        return Ok(error_response(StatusCode::CONFLICT, "ripping"));
    }
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
    /// 覚えている結果を捨てて引き直す（画面の「MusicBrainz に照会」。ディスク検出の自動照会は省略）
    #[serde(default)]
    pub refresh: bool,
    /// 段を打ち切らずに全部引く（画面の「さらに広げて探す」。D-64 追記 4）
    #[serde(default)]
    pub widen: bool,
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
    /// どの段で止まったか（discid / ids / toc。D-64 追記 4）
    pub stage: LookupStage,
    /// まだ引いていない段がある（画面に「さらに広げて探す」を出す）
    pub can_widen: bool,
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
    // ISRC / MCN は MB の Lucene クエリに載せるので、形を検証してから通す（英数字 12 / 数字 13。
    // 空と null は「無い」）。件数は TOC の音声トラック数まで
    let audio_tracks = toc.audio_tracks().count();
    let mut isrcs: Vec<String> = Vec::new();
    for raw in body.isrcs.iter().flatten() {
        let v = raw.trim().to_ascii_uppercase();
        if v.is_empty() {
            continue;
        }
        if v.len() != 12 || !v.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("ISRC の形が不正: {raw:?}（英数字 12 文字）"),
            ));
        }
        if isrcs.contains(&v) {
            continue;
        }
        // 上限は走査中に見る（超えた時点で 400。Vec が入力の長さに比例して伸びない）
        if isrcs.len() >= audio_tracks {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("ISRC が音声トラック数 {audio_tracks} を超えている"),
            ));
        }
        isrcs.push(v);
    }
    let mcn = match body.mcn.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        None => None,
        Some(m) if m.len() == 13 && m.chars().all(|c| c.is_ascii_digit()) => Some(m.to_owned()),
        Some(m) => {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("MCN の形が不正: {m:?}（数字 13 桁）"),
            ))
        }
    };
    let query = crate::cd::musicbrainz::DiscQuery {
        toc: &toc,
        isrcs: &isrcs,
        mcn: mcn.as_deref(),
        release: body.release.as_deref().filter(|r| !r.trim().is_empty()),
        refresh: body.refresh,
        widen: body.widen,
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
            // 原因の末端まで出す（reqwest の Display は「error sending request」で止まり、
            // TLS の失敗・タイムアウト・接続断の区別が消える）
            let detail = crate::cd::error_chain(&e);
            tracing::warn!(error = %detail, "MusicBrainz の照会に失敗");
            return Ok(error_response_with_message(
                StatusCode::BAD_GATEWAY,
                "lookup_failed",
                detail,
            ));
        }
    };
    // candidates を move する前に見る
    let can_widen = result.can_widen();
    Ok(Json(LookupResponse {
        discid: result.discid,
        mb_toc: toc.musicbrainz_toc(),
        accuraterip_id: toc.accuraterip_id().to_string(),
        ctdb_toc_id: toc.ctdb_toc_id(),
        exact: result.exact,
        candidates: result.candidates,
        notes: result.notes,
        tracks: toc_tracks(&toc),
        stage: result.stage,
        can_widen,
    })
    .into_response())
}

/// MBID（8-4-4-4-12 の 16 進）か。上流に投げる前に形を確かめる
/// （利用者の文字列をそのまま URL に継ぎ足さない。D-82）
fn is_mbid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            == [
                parts[0].len(),
                parts[1].len(),
                parts[2].len(),
                parts[3].len(),
                parts[4].len(),
            ]
        && parts
            .iter()
            .all(|p| p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// 候補のジャケット（D-82、P4-20）。Cover Art Archive の front 画像を中継する。
/// 画像が無い盤は 404、上流が壊れているときは 502（混ぜると診断できない）
pub async fn cover(
    State(state): State<AppState>,
    axum::extract::Path(release_id): axum::extract::Path<String>,
) -> Result<Response, ApiError> {
    if !is_mbid(&release_id) {
        return Ok(error_response_with_message(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "リリース id は MusicBrainz の MBID（8-4-4-4-12）".to_owned(),
        ));
    }
    let Some(client) = state.coverart.as_ref() else {
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "coverart_unavailable",
        ));
    };
    match client.front(&release_id).await {
        Ok(None) => Ok(error_response(StatusCode::NOT_FOUND, "not_found")),
        Ok(Some((content_type, bytes))) => Ok((
            [
                (axum::http::header::CONTENT_TYPE, content_type),
                // 同じ盤を選び直すたびに取りに行かない。長くは持たない（差し替えがある）
                (
                    axum::http::header::CACHE_CONTROL,
                    "private, max-age=600".to_owned(),
                ),
                // 上流が名乗った型で描かせる（中身の推測で別の型として扱わせない）
                (
                    axum::http::header::X_CONTENT_TYPE_OPTIONS,
                    "nosniff".to_owned(),
                ),
            ],
            bytes,
        )
            .into_response()),
        Err(e) => {
            let detail = crate::cd::error_chain(&e);
            tracing::warn!(error = %detail, release_id, "ジャケットの取得に失敗");
            Ok(error_response_with_message(
                StatusCode::BAD_GATEWAY,
                "lookup_failed",
                detail,
            ))
        }
    }
}

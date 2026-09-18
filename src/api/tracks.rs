//! `GET /api/tracks`（カーソルページング・ソート・フィルタ）、`GET /api/search`、
//! `GET /api/tracks/:id`（SPEC §9、P0-7、D-39）。
//!
//! `filter` は URL エンコードした JSON（`domain::filter::Filter`）、`sort` は `title` / `-title`、
//! `cursor` は前ページの `next_cursor`、`limit` は 1..=1000（既定 100）。
//! `/api/search?q=` は同じ一覧を `filter.q` を差し替えて返す（表の集合を差し替えるだけ。
//! SPEC §12.1）。3 文字未満は LIKE にフォールバックする（`db::tracks`）

use axum::extract::{Path, Query as QueryParams, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};

use crate::db::tracks::{self, TrackDetail, TrackRow};
use crate::domain::filter::{FilterError, Query};

use super::auth::Session;
use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;

#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    pub filter: Option<String>,
    pub sort: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    /// `/api/search` の検索語。`/api/tracks` でも受け付け、`filter.q` より優先する
    pub q: Option<String>,
}

fn build_query(p: &ListParams) -> Result<Query, FilterError> {
    let mut q = Query::from_params(
        p.filter.as_deref(),
        p.sort.as_deref(),
        p.cursor.as_deref(),
        p.limit,
    )?;
    if let Some(term) = p.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        q.filter.q = Some(term.to_owned());
    }
    Ok(q)
}

fn bad_request(e: FilterError) -> Response {
    error_response_with_message(StatusCode::BAD_REQUEST, "bad_request", e.to_string())
}

pub async fn list(
    State(state): State<AppState>,
    QueryParams(params): QueryParams<ListParams>,
) -> Result<Response, ApiError> {
    let query = match build_query(&params) {
        Ok(q) => q,
        Err(e) => return Ok(bad_request(e)),
    };
    let page = state.db.read(move |c| tracks::list(c, &query)).await?;
    Ok(Json(page).into_response())
}

/// `GET /api/search?q=`。`q` が無ければ 400
pub async fn search(
    State(state): State<AppState>,
    QueryParams(params): QueryParams<ListParams>,
) -> Result<Response, ApiError> {
    if params.q.as_deref().map(str::trim).unwrap_or("").is_empty() {
        return Ok(error_response_with_message(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "q が必要",
        ));
    }
    list(State(state), QueryParams(params)).await
}

/// trusted_cidrs からセッション無しで来たときに返す限定フィールド（D-27）。
/// パス・編集状態・重複などライブラリの内部状態は含めない
#[derive(Serialize)]
pub struct PublicTrack {
    pub id: i64,
    pub title: Option<String>,
    pub artist_display: Option<String>,
    pub album: Option<String>,
    pub albumartist: Option<String>,
    pub track_no: Option<i64>,
    pub disc_no: Option<i64>,
    pub date: Option<String>,
    pub duration_ms: Option<i64>,
    pub codec: String,
    pub lossless: bool,
}

impl From<TrackRow> for PublicTrack {
    fn from(r: TrackRow) -> Self {
        PublicTrack {
            id: r.id,
            title: r.title,
            artist_display: r.artist_display,
            album: r.album,
            albumartist: r.albumartist,
            track_no: r.track_no,
            disc_no: r.disc_no,
            date: r.date,
            duration_ms: r.duration_ms,
            codec: r.codec,
            lossless: r.lossless,
        }
    }
}

/// セッション向けの応答: 一覧と同じ行 + `detail`（D-58）
#[derive(Serialize)]
pub struct TrackWithDetail {
    #[serde(flatten)]
    pub row: TrackRow,
    pub detail: TrackDetail,
}

/// `GET /api/tracks/:id`。セッションがあれば一覧と同じ行 + `detail`、allowlist 経由なら
/// 限定フィールド（`detail` も付けない）
pub async fn get(
    State(state): State<AppState>,
    session: Option<Extension<Session>>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    if session.is_none() {
        let row = state.db.read(move |c| tracks::get(c, id)).await?;
        return Ok(match row {
            Some(row) => Json(PublicTrack::from(row)).into_response(),
            None => error_response(StatusCode::NOT_FOUND, "not_found"),
        });
    }
    // 行と詳細は同じ読み取りトランザクションで取る（間にスキャンが commit しても世代が混ざらない）
    let found =
        state
            .db
            .read(move |c| {
                Ok(tracks::get_with_detail(c, id)?
                    .map(|(row, detail)| TrackWithDetail { row, detail }))
            })
            .await?;
    Ok(match found {
        Some(t) => Json(t).into_response(),
        None => error_response(StatusCode::NOT_FOUND, "not_found"),
    })
}

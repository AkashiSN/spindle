//! Inbox の承認キュー（SPEC §7.8 / §9、D-68、P2-10）。
//! `GET /api/inbox`（件と下書き・警告）、`POST /api/inbox/scan`、`POST /api/inbox/:id/approve`
//! （検証して approved にし、inbox ジョブを投入）、`/reject`、`/reopen`

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::db::inbox::{self as dbinbox, FileRow, Item, ItemState};
use crate::db::now_epoch;
use crate::db::scans;
use crate::import::inbox::{destination, embedded_picture, propose, Destination, InboxDraft};
use crate::import::ytmusic::sidecar::FileEntry;
use crate::jobs::handlers::inbox::new_inbox_job;
use crate::jobs::EnqueueResult;

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;

#[derive(Serialize)]
pub struct TrackView {
    #[serde(flatten)]
    pub file: FileRow,
    /// サイドカーの項（ダウンローダが置いた件。D-70）
    pub source: Option<FileEntry>,
}

#[derive(Serialize)]
pub struct ItemView {
    #[serde(flatten)]
    pub item: Item,
    pub tracks: Vec<TrackView>,
    pub proposal: InboxDraft,
    pub warnings: Vec<String>,
    /// 追記先の既存 album（D-70）
    pub destination: Option<Destination>,
}

#[derive(Serialize)]
pub struct ItemList {
    pub items: Vec<ItemView>,
}

#[derive(Serialize)]
pub struct Accepted {
    pub job_id: i64,
}

fn unavailable(state: &AppState) -> Option<Response> {
    state
        .inbox
        .is_none()
        .then(|| error_response(StatusCode::SERVICE_UNAVAILABLE, "inbox_unavailable"))
}

pub async fn list(State(state): State<AppState>) -> Result<Response, ApiError> {
    if let Some(r) = unavailable(&state) {
        return Ok(r);
    }
    let Some(inbox) = state.inbox.clone() else {
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "inbox_unavailable",
        ));
    };
    let layout = state.config.layout.clone();
    let items = state
        .db
        .read(move |c| {
            // 表示名（`scans::load_categories` は照合用の canonical key を返す）
            let categories: Vec<(i64, String)> = crate::db::categories::list(c)?
                .into_iter()
                .map(|cat| (cat.id, cat.name))
                .collect();
            let genre_map = scans::load_genre_map(c)?;
            let mut out = Vec::new();
            for item in dbinbox::list(c)? {
                let files = dbinbox::files(c, item.id)?;
                let p = match propose(c, &inbox, &layout, &item, &files, &categories, &genre_map) {
                    Ok(p) => p,
                    Err(e) => return Ok(Err(e.to_string())),
                };
                let tracks = files
                    .into_iter()
                    .zip(p.sources)
                    .map(|(file, source)| TrackView { file, source })
                    .collect();
                out.push(ItemView {
                    item,
                    tracks,
                    proposal: p.draft,
                    warnings: p.warnings,
                    destination: p.destination,
                });
            }
            Ok(Ok(out))
        })
        .await?
        .map_err(ApiError::Internal)?;
    Ok(Json(ItemList { items }).into_response())
}

pub async fn scan(State(state): State<AppState>) -> Result<Response, ApiError> {
    if let Some(r) = unavailable(&state) {
        return Ok(r);
    }
    Ok(match state.jobs.enqueue(new_inbox_job()).await? {
        EnqueueResult::Inserted(job_id) => {
            (StatusCode::ACCEPTED, Json(Accepted { job_id })).into_response()
        }
        EnqueueResult::Duplicate(_) => error_response(StatusCode::CONFLICT, "duplicate"),
    })
}

pub async fn approve(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(draft): Json<InboxDraft>,
) -> Result<Response, ApiError> {
    if let Some(r) = unavailable(&state) {
        return Ok(r);
    }
    let value = serde_json::to_value(&draft)
        .map_err(|e| ApiError::Internal(format!("下書きを JSON にできない: {e}")))?;
    let layout = state.config.layout.clone();
    // 状態の検査・下書きの検証・保存・遷移を 1 トランザクションで行い、遷移は CAS
    // （読んでから書くまでに worker / 走査 / 別の API が動かした件を上書きしない）
    let outcome = state
        .db
        .transaction(move |c| {
            let Some(item) = dbinbox::get(c, id)? else {
                return Ok(Approve::NotFound);
            };
            if !matches!(item.state, ItemState::Pending | ItemState::Failed) {
                return Ok(Approve::State);
            }
            let files = dbinbox::files(c, id)?;
            let names: Vec<String> = files.iter().map(|f| f.rel_path.clone()).collect();
            if let Err(e) = draft.validate(&names) {
                return Ok(Approve::Bad(e.to_string()));
            }
            // 追記先の active なトラックと番号が重ならないこと（配置で failed になる前に直させる。D-70）
            let dest = match destination(c, &layout, &draft, &files) {
                Ok(d) => d,
                Err(e) => return Ok(Approve::Bad(e.to_string())),
            };
            if let Some(d) = dest {
                if let Some(t) = draft
                    .tracks
                    .iter()
                    .find(|t| d.numbers.contains(&(t.disc_no, t.track_no)))
                {
                    return Ok(Approve::Bad(format!(
                        "宛先の album に同じ番号のトラックがある: disc {} track {}",
                        t.disc_no, t.track_no
                    )));
                }
            }
            dbinbox::set_draft(c, id, &value)?;
            let moved = dbinbox::transition(
                c,
                id,
                &[ItemState::Pending, ItemState::Failed],
                ItemState::Approved,
                None,
                now_epoch(),
            )?;
            Ok(if moved { Approve::Ok } else { Approve::State })
        })
        .await?;
    match outcome {
        Approve::NotFound => return Ok(error_response(StatusCode::NOT_FOUND, "not_found")),
        Approve::State => return Ok(error_response(StatusCode::CONFLICT, "state")),
        Approve::Bad(msg) => {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                msg,
            ))
        }
        Approve::Ok => {}
    }
    let job_id = state.jobs.enqueue(new_inbox_job()).await?.id();
    Ok((StatusCode::ACCEPTED, Json(Accepted { job_id })).into_response())
}

enum Approve {
    NotFound,
    State,
    Bad(String),
    Ok,
}

async fn transition(
    state: &AppState,
    id: i64,
    from: &[ItemState],
    to: ItemState,
) -> Result<Response, ApiError> {
    if let Some(r) = unavailable(state) {
        return Ok(r);
    }
    // 遷移は CAS（今の状態が `from` のどれかのときだけ）。外れたら 404 か 409 を状態で分ける
    let from = from.to_vec();
    let moved = state
        .db
        .transaction(move |c| {
            if dbinbox::transition(c, id, &from, to, None, now_epoch())? {
                return Ok(Some(true));
            }
            Ok(dbinbox::get(c, id)?.map(|_| false))
        })
        .await?;
    Ok(match moved {
        Some(true) => StatusCode::NO_CONTENT.into_response(),
        Some(false) => error_response(StatusCode::CONFLICT, "state"),
        None => error_response(StatusCode::NOT_FOUND, "not_found"),
    })
}

pub async fn reject(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    transition(
        &state,
        id,
        &[ItemState::Pending, ItemState::Failed, ItemState::Approved],
        ItemState::Rejected,
    )
    .await
}

pub async fn reopen(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    transition(
        &state,
        id,
        &[ItemState::Approved, ItemState::Rejected, ItemState::Failed],
        ItemState::Pending,
    )
    .await
}

/// `GET /api/inbox/:id/artwork/:hash`（SPEC §7.8、P4-4）。件のファイルのうち `PICTURE` の sha256 が
/// `hash` のものを開き、一致する埋め込み画像を原寸で返す（MIME は sniff）。ハッシュアドレスなので
/// `immutable` + ETag。304 は**実体の照合の後**（件に無い・ファイルが消えた・画像が変わっていれば
/// If-None-Match が一致しても 404）。件の状態は見ない。セッション必須
pub async fn artwork(
    State(state): State<AppState>,
    Path((id, hash)): Path<(i64, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let Some(inbox) = state.inbox.clone() else {
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "inbox_unavailable",
        ));
    };
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(error_response(StatusCode::NOT_FOUND, "not_found"));
    }
    let hash = hash.to_ascii_lowercase();
    let files = state
        .db
        .read(move |c| {
            if dbinbox::get(c, id)?.is_none() {
                return Ok(None);
            }
            Ok(Some(dbinbox::files(c, id)?))
        })
        .await?;
    let Some(files) = files else {
        return Ok(error_response(StatusCode::NOT_FOUND, "not_found"));
    };
    let lookup = hash.clone();
    let found = tokio::task::spawn_blocking(move || embedded_picture(&inbox, &files, &lookup))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let Some((mime, bytes)) = found else {
        return Ok(error_response(StatusCode::NOT_FOUND, "not_found"));
    };
    let etag = format!("\"{hash}-orig\"");
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag))
    {
        return Ok(StatusCode::NOT_MODIFIED.into_response());
    }
    let mut res = (StatusCode::OK, bytes).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    Ok(res)
}

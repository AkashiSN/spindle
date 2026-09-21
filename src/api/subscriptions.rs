//! `/api/ytmusic/subscriptions`（SPEC §9、D-78、P4-16）。再生リストの購読の登録 / 一覧 / 変更 / 削除と
//! 同期の投入。`[ytmusic].enabled` でなければ 404（ハンドラが登録されないので投入しても動かない）。
//! `/api/playlists` は spindle 自身の m3u8 プレイリストなので、購読は ytmusic の下に置く

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db::now_epoch;
use crate::db::subscriptions::{
    self as dbsubs, NewSubscription, Patch, Subscription, WriteOutcome,
};
use crate::jobs::handlers::playlist_sync::new_sync_job;
use crate::jobs::EnqueueResult;

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;

const MAX_URL_CHARS: usize = 2048;
const MAX_TEXT_CHARS: usize = 512;
/// 1 回の同期で投入する上限の上限
const MAX_ENQUEUE_CAP: i64 = 1000;

#[derive(Deserialize)]
pub struct CreateBody {
    pub url: String,
    pub albumartist: String,
    pub album: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default = "default_true")]
    pub align: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_max_enqueue")]
    pub max_enqueue: i64,
}

fn default_true() -> bool {
    true
}

fn default_max_enqueue() -> i64 {
    50
}

#[derive(Serialize)]
pub struct ListResponse {
    pub items: Vec<Subscription>,
}

#[derive(Serialize)]
pub struct Accepted {
    pub job_id: i64,
}

fn bad(msg: impl Into<String>) -> Response {
    error_response_with_message(StatusCode::BAD_REQUEST, "bad_request", msg)
}

/// YouTube の再生リスト URL から `list=` を取る。YouTube 以外・`list=` 無しは None
pub fn list_id_of(url: &str) -> Option<String> {
    let p = url::Url::parse(url.trim()).ok()?;
    if !matches!(p.scheme(), "http" | "https") {
        return None;
    }
    let host = p.host_str()?.to_ascii_lowercase();
    let youtube = ["youtube.com", "youtu.be"]
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")));
    if !youtube {
        return None;
    }
    p.query_pairs()
        .find(|(k, _)| k == "list")
        .map(|(_, v)| v.trim().to_owned())
        .filter(|v| {
            !v.is_empty()
                && v.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
}

/// 入力の問題（400 の文言）
fn check_text(name: &str, v: &str) -> Option<String> {
    let t = v.trim();
    if t.is_empty() {
        return Some(format!("{name} が空"));
    }
    if t.chars().count() > MAX_TEXT_CHARS || t.chars().any(char::is_control) {
        return Some(format!("{name} が長すぎるか制御文字を含む"));
    }
    None
}

fn check_category(v: Option<&str>) -> Option<String> {
    v.filter(|c| c.chars().count() > MAX_TEXT_CHARS || c.chars().any(char::is_control))
        .map(|_| "category が長すぎるか制御文字を含む".to_owned())
}

fn check_max_enqueue(n: i64) -> Option<String> {
    (!(1..=MAX_ENQUEUE_CAP).contains(&n)).then(|| format!("max_enqueue は 1〜{MAX_ENQUEUE_CAP}"))
}

/// 書き込みの結果を応答に。Ok なら id
fn write_outcome_response(outcome: WriteOutcome) -> Result<i64, Box<Response>> {
    match outcome {
        WriteOutcome::Ok(id) => Ok(id),
        WriteOutcome::NotFound => Err(Box::new(error_response(StatusCode::NOT_FOUND, "not_found"))),
        WriteOutcome::DuplicateList => Err(Box::new(error_response_with_message(
            StatusCode::CONFLICT,
            "duplicate_list",
            "同じ再生リストの購読がある",
        ))),
        WriteOutcome::DuplicateTarget => Err(Box::new(error_response_with_message(
            StatusCode::CONFLICT,
            "duplicate_target",
            "同じ追記先（アルバムアーティスト + アルバム）の購読がある",
        ))),
    }
}

fn disabled(state: &AppState) -> Option<Response> {
    (!state.config.ytmusic.enabled).then(|| error_response(StatusCode::NOT_FOUND, "not_found"))
}

pub async fn list(State(state): State<AppState>) -> Result<Response, ApiError> {
    if let Some(r) = disabled(&state) {
        return Ok(r);
    }
    let items = state.db.read(dbsubs::list).await?;
    Ok(Json(ListResponse { items }).into_response())
}

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateBody>,
) -> Result<Response, ApiError> {
    if let Some(r) = disabled(&state) {
        return Ok(r);
    }
    let url = body.url.trim().to_owned();
    if url.is_empty() || url.chars().count() > MAX_URL_CHARS {
        return Ok(bad(format!(
            "URL が空か長すぎる（{MAX_URL_CHARS} 文字まで）"
        )));
    }
    let Some(list_id) = list_id_of(&url) else {
        return Ok(bad("YouTube の再生リスト URL（list= 付き）でない"));
    };
    if let Some(msg) = check_text("albumartist", &body.albumartist)
        .or_else(|| check_text("album", &body.album))
        .or_else(|| check_category(body.category.as_deref()))
        .or_else(|| check_max_enqueue(body.max_enqueue))
    {
        return Ok(bad(msg));
    }
    let new = NewSubscription {
        list_id,
        url,
        albumartist: body.albumartist.trim().to_owned(),
        album: body.album.trim().to_owned(),
        category: body
            .category
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_owned),
        align: body.align,
        enabled: body.enabled,
        max_enqueue: body.max_enqueue,
    };
    let outcome = state
        .db
        .write(move |c| dbsubs::insert(c, &new, now_epoch()))
        .await?;
    let id = match write_outcome_response(outcome) {
        Ok(id) => id,
        Err(r) => return Ok(*r),
    };
    let sub = state.db.read(move |c| dbsubs::get(c, id)).await?;
    Ok((StatusCode::CREATED, Json(sub)).into_response())
}

pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(patch): Json<Patch>,
) -> Result<Response, ApiError> {
    if let Some(r) = disabled(&state) {
        return Ok(r);
    }
    if let Some(msg) = patch
        .albumartist
        .as_deref()
        .and_then(|v| check_text("albumartist", v))
        .or_else(|| patch.album.as_deref().and_then(|v| check_text("album", v)))
        .or_else(|| check_category(patch.category.as_ref().and_then(|c| c.as_deref())))
        .or_else(|| patch.max_enqueue.and_then(check_max_enqueue))
    {
        return Ok(bad(msg));
    }
    // 走行中（queued / running）の同期があれば変更しない（同期が古い追記先で揃えたり投入したりしない。
    // 検査と UPDATE は同じ書き込み閉包 = 投入と直列）
    let outcome = state
        .db
        .write(move |c| {
            if dbsubs::sync_active(c, id)? {
                return Ok(None);
            }
            dbsubs::update(c, id, &patch, now_epoch()).map(Some)
        })
        .await?;
    let Some(outcome) = outcome else {
        return Ok(error_response_with_message(
            StatusCode::CONFLICT,
            "sync_running",
            "同期の実行中は変更できない（終わるか取り消してから）",
        ));
    };
    if let Err(r) = write_outcome_response(outcome) {
        return Ok(*r);
    }
    let sub = state.db.read(move |c| dbsubs::get(c, id)).await?;
    Ok(Json(sub).into_response())
}

pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    if let Some(r) = disabled(&state) {
        return Ok(r);
    }
    // PATCH と同じく、走行中の同期があれば消さない（消えた購読の album を揃えたり、死んだ id で投入したり
    // しない）
    let deleted = state
        .db
        .write(move |c| {
            if dbsubs::sync_active(c, id)? {
                return Ok(None);
            }
            dbsubs::delete(c, id).map(Some)
        })
        .await?;
    Ok(match deleted {
        None => error_response_with_message(
            StatusCode::CONFLICT,
            "sync_running",
            "同期の実行中は削除できない（終わるか取り消してから）",
        ),
        Some(true) => StatusCode::NO_CONTENT.into_response(),
        Some(false) => error_response(StatusCode::NOT_FOUND, "not_found"),
    })
}

/// 手動の同期。latch を立ててから投入する（走行中なら Duplicate だが要求は残る）
pub async fn sync(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    if let Some(r) = disabled(&state) {
        return Ok(r);
    }
    let found = state
        .db
        .write(move |c| dbsubs::request_sync(c, id, now_epoch()))
        .await?;
    if !found {
        return Ok(error_response(StatusCode::NOT_FOUND, "not_found"));
    }
    Ok(match state.jobs.enqueue(new_sync_job(id)).await? {
        EnqueueResult::Inserted(job_id) => {
            (StatusCode::ACCEPTED, Json(Accepted { job_id })).into_response()
        }
        EnqueueResult::Duplicate(_) => error_response(StatusCode::CONFLICT, "duplicate"),
    })
}

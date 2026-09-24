//! `GET /api/artwork/:hash?size=`（SPEC §9、P1-3、D-49）と、書き側の
//! `POST /api/artwork/upload` / `POST /api/artwork/embed`（P1-3 書き側、D-60）。
//!
//! `:hash` は画像の SHA-256（hex）。`size` は [`THUMB_SIZES`] のいずれかで WebP のサムネイル、
//! 無ければ原画像を元の MIME で返す。サムネイルがまだ無ければ（thumbnail ジョブが未了）原画像へ
//! 倒す。ハッシュアドレスなので内容は不変: `Cache-Control: immutable` と ETag（304）。ただし
//! 倒した応答は後でサムネイルに置き換わるので `no-cache`（ETag で再検証させる）。
//! trusted_cidrs からはセッション無しで通る（`auth::is_allowlisted`）

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db::artwork as dbart;
use crate::db::{history, tracks};
use crate::domain::selection::SelectionBody;
use crate::edit::{EditError, Editor};
use crate::jobs::handlers::thumbnail::new_thumbnail_job;
use crate::media::artwork::{sniff, ArtworkStore, MAX_COVER_BYTES, THUMB_SIZES};

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;

/// アップロードの上限（`DefaultBodyLimit`。[`MAX_COVER_BYTES`] と同じ）
pub const UPLOAD_LIMIT: usize = MAX_COVER_BYTES as usize;

/// 埋め込みに使う形式（[`sniff`] が読める形式のうち、lofty で書けて主要プレイヤーが表示するもの）
const EMBED_MIMES: [&str; 3] = ["image/jpeg", "image/png", "image/webp"];

#[derive(Debug, Deserialize)]
pub struct SizeQuery {
    #[serde(default)]
    pub size: Option<u32>,
}

fn parse_hash(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

pub async fn get(
    State(state): State<AppState>,
    Path(hash): Path<String>,
    Query(q): Query<SizeQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let Some(store) = state.artwork.clone() else {
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "artwork_unavailable",
        ));
    };
    let Some(sha) = parse_hash(&hash) else {
        return Ok(error_response(StatusCode::NOT_FOUND, "not_found"));
    };
    if let Some(s) = q.size {
        if !THUMB_SIZES.contains(&s) {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("size は {THUMB_SIZES:?} のいずれか"),
            ));
        }
    }
    let Some(art) = state
        .db
        .read(move |c| dbart::get_by_sha256(c, &sha))
        .await?
    else {
        return Ok(error_response(StatusCode::NOT_FOUND, "not_found"));
    };
    // サムネイルが無ければ原画像へ倒す（ETag はどちらを返したかで変える）
    let (path, mime, variant, fallback) = match q.size {
        Some(s) if store.thumb_path(&sha, s).is_file() => (
            store.thumb_path(&sha, s),
            "image/webp".to_owned(),
            s.to_string(),
            false,
        ),
        size => (
            store.original_path(&sha, &art.mime),
            art.mime.clone(),
            "orig".to_owned(),
            size.is_some(),
        ),
    };
    let etag = format!("\"{}-{variant}\"", hash.to_ascii_lowercase());
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag))
    {
        return Ok(StatusCode::NOT_MODIFIED.into_response());
    }
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(error_response(StatusCode::NOT_FOUND, "not_found"));
        }
        Err(e) => return Err(ApiError::Internal(e.to_string())),
    };
    let mut res = (StatusCode::OK, bytes).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if fallback {
            "public, no-cache"
        } else {
            "public, max-age=31536000, immutable"
        }),
    );
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    Ok(res)
}

// ---------------------------------------------------------------- 書き側（D-60）

#[derive(Debug, Serialize)]
pub struct UploadResponse {
    pub sha256: String,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
}

/// `POST /api/artwork/upload`: 生の画像バイト列を [`ArtworkStore`] と `artwork` 行に置き、
/// thumbnail ジョブを投入する。形式はヘッダで判別し（Content-Type と拡張子は信用しない）、
/// JPEG / PNG / WebP 以外は 400 `unsupported_image`。同じ画像は 1 回だけ置かれる
pub async fn upload(State(state): State<AppState>, body: Bytes) -> Result<Response, ApiError> {
    store_image(&state, body).await
}

#[derive(Debug, Deserialize)]
pub struct FromCaaBody {
    /// MusicBrainz のリリース（MBID）
    pub release_id: String,
}

/// `POST /api/artwork/from-caa { release_id }`（D-86）: Cover Art Archive の front 画像（500px）を取り、
/// アップロードと同じく store と `artwork` 行に置く（応答も同じ）。Inbox の承認画面の「Cover Art Archive
/// から取る」。画像の無い盤は 404、上流の失敗は 502 `lookup_failed`、クライアント未構成は 503
pub async fn from_caa(
    State(state): State<AppState>,
    body: Result<Json<FromCaaBody>, JsonRejection>,
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
    let release_id = body.release_id.trim().to_ascii_lowercase();
    if !super::cd::is_mbid(&release_id) {
        return Ok(error_response_with_message(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "リリース id は MusicBrainz の MBID（8-4-4-4-12）",
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
        Ok(Some((_content_type, bytes))) => store_image(&state, Bytes::from(bytes)).await,
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

/// 画像のバイト列を store と `artwork` 行に置き、thumbnail ジョブを投入する（upload / from-caa 共通）
async fn store_image(state: &AppState, body: Bytes) -> Result<Response, ApiError> {
    let Some(store) = state.artwork.clone() else {
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "artwork_unavailable",
        ));
    };
    let Some(info) = sniff(&body).filter(|i| EMBED_MIMES.contains(&i.mime)) else {
        return Ok(error_response_with_message(
            StatusCode::BAD_REQUEST,
            "unsupported_image",
            "JPEG / PNG / WebP の画像だけを受け付ける",
        ));
    };
    let bytes = body.len();
    let hash = ArtworkStore::hash_of(&body);
    {
        // 既にある画像でも put_original が dir の mtime を今にするので、GC 区分 E の 24 時間の猶予は
        // このアップロードから数え直される（embed までの窓を守る）
        let store = Arc::clone(&store);
        tokio::task::spawn_blocking(move || store.put_original(&hash, info.mime, &body))
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?
            .map_err(|e| ApiError::Internal(format!("原画像を置けない: {e}")))?;
    }
    let (job, needs_thumbs) = {
        let needs = !store.missing_thumbs(&hash).is_empty();
        let job = state
            .db
            .write(move |c| {
                let tx = c.transaction()?;
                let id = dbart::upsert(
                    &tx,
                    &hash,
                    info.mime,
                    Some(info.width),
                    Some(info.height),
                    bytes,
                    "embedded",
                )?;
                let job = if needs {
                    Some(
                        crate::db::jobs::enqueue(
                            &tx,
                            &new_thumbnail_job(id),
                            crate::db::now_epoch(),
                        )?
                        .id(),
                    )
                } else {
                    None
                };
                tx.commit()?;
                Ok(job)
            })
            .await?;
        (job, needs)
    };
    if let Some(id) = job {
        state.jobs.notify_enqueued(&[id]).await;
    }
    tracing::info!(
        sha256 = %ArtworkStore::hex(&hash),
        mime = info.mime,
        bytes,
        needs_thumbs,
        "画像をアップロードした"
    );
    Ok((
        StatusCode::CREATED,
        Json(UploadResponse {
            sha256: ArtworkStore::hex(&hash),
            mime: info.mime.to_owned(),
            width: info.width,
            height: info.height,
            bytes,
        }),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
pub struct EmbedBody {
    pub selection: SelectionBody,
    /// アップロードした画像の SHA-256（hex）
    pub sha256: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub skip_pending: bool,
}

#[derive(Debug, Serialize)]
pub struct EmbedResponse {
    pub batch_id: i64,
    /// 記録した op 数
    pub affected: usize,
    /// 既にその 1 枚だけを持っていて op にならなかった行数
    pub unchanged: usize,
    /// missing で対象外の行数
    pub missing: usize,
    /// `skip_pending` で除外した行数
    pub pending_excluded: usize,
}

fn editor_of(state: &AppState) -> Result<Arc<Editor>, Box<Response>> {
    let editor = state.editor.clone().ok_or_else(|| {
        Box::new(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "editor_unavailable",
        ))
    })?;
    if editor.artwork_store().is_none() {
        return Err(Box::new(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "artwork_unavailable",
        )));
    }
    Ok(editor)
}

fn pending_response(track_ids: Vec<i64>) -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "error": "pending",
            "count": track_ids.len(),
            "track_ids": track_ids,
        })),
    )
        .into_response()
}

/// `POST /api/artwork/embed { selection, sha256, description?, skip_pending? }`: selection の
/// active 全行の埋め込み画像を `sha256` の 1 枚に差し替える tags op（`PICTURE`）の編集バッチを
/// 記録する（`Editor::prepare_picture`。巻き戻せる）。RG 書き込み・MD5 補填と同型: 反映待ちが
/// あれば 409 `pending`、`skip_pending` で除外。対象が無ければ 409 `no_changes`。画像が
/// 登録されていなければ 404 `artwork_not_found`
pub async fn embed(
    State(state): State<AppState>,
    body: Result<Json<EmbedBody>, JsonRejection>,
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
    let editor = match editor_of(&state) {
        Ok(e) => e,
        Err(r) => return Ok(*r),
    };
    let Some(sha256) = parse_hash(&body.sha256) else {
        return Ok(error_response_with_message(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "sha256 は 64 桁の hex",
        ));
    };
    let sel = match body.selection.parse() {
        Ok(s) => s,
        Err(e) => {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                e.to_string(),
            ))
        }
    };
    let (ids, pending) = state
        .db
        .read(move |c| {
            let tx = c.unchecked_transaction()?;
            let rows = tracks::resolve_selection(&tx, &sel)?;
            let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
            let pending = history::pending_track_ids(&tx, &ids)?;
            tx.finish()?;
            Ok((ids, pending))
        })
        .await?;
    if !pending.is_empty() && !body.skip_pending {
        return Ok(pending_response(pending));
    }
    let ids: Vec<i64> = ids
        .into_iter()
        .filter(|id| pending.binary_search(id).is_err())
        .collect();
    let pending_excluded = pending.len();
    match editor
        .prepare_picture(body.description.as_deref(), ids, sha256)
        .await
    {
        Ok(p) => {
            let Some(batch_id) = p.batch_id else {
                return Ok(error_response(StatusCode::CONFLICT, "no_changes"));
            };
            Ok((
                StatusCode::CREATED,
                Json(EmbedResponse {
                    batch_id,
                    affected: p.affected,
                    unchanged: p.unchanged,
                    missing: p.missing,
                    pending_excluded,
                }),
            )
                .into_response())
        }
        Err(EditError::NoChanges) => Ok(error_response(StatusCode::CONFLICT, "no_changes")),
        Err(EditError::Pending { track_ids }) => Ok(pending_response(track_ids)),
        Err(EditError::ArtworkNotFound) => {
            Ok(error_response(StatusCode::NOT_FOUND, "artwork_not_found"))
        }
        Err(EditError::ArtworkUnavailable) => Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "artwork_unavailable",
        )),
        Err(e) => Err(ApiError::Internal(e.to_string())),
    }
}

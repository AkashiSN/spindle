//! `GET /api/artwork/:hash?size=`（SPEC §9、P1-3、D-49）。
//!
//! `:hash` は画像の SHA-256（hex）。`size` は [`THUMB_SIZES`] のいずれかで WebP のサムネイル、
//! 無ければ原画像を元の MIME で返す。サムネイルがまだ無ければ（thumbnail ジョブが未了）原画像へ
//! 倒す。ハッシュアドレスなので内容は不変: `Cache-Control: immutable` と ETag（304）。ただし
//! 倒した応答は後でサムネイルに置き換わるので `no-cache`（ETag で再検証させる）。
//! trusted_cidrs からはセッション無しで通る（`auth::is_allowlisted`）

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::db::artwork as dbart;
use crate::media::artwork::THUMB_SIZES;

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;

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

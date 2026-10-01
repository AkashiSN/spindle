//! エージェント API（`/api/agent/*`、仕様「認証と境界」、D-99）。
//! セッション Cookie・CSRF・trusted_cidrs のどれでも通らない。コードとトークンはログに出さない

use agent_proto::{
    DiffView, Held, ItemOp, ManifestItem, ManifestPlaylist, ManifestResponse, PairRequest,
    PairResponse, PlaylistError, PlaylistOp,
};
use axum::extract::rejection::JsonRejection;
use std::collections::HashMap;

use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};

use super::auth::{self, verify_bytes};
use super::devices;
use super::error::{error_response, error_response_with_message, ApiError};
use super::stream::{parse_range, range_not_satisfiable, ranged_body, Unsatisfiable};
use super::AppState;
use crate::db::devices::{self as dbdev, PairReserve};
use crate::db::{now_epoch, DbError};
use crate::device::credential::{
    self, PAIR_SECRET_BYTES, PAIR_SELECTOR_BYTES, TOKEN_SECRET_BYTES, TOKEN_SELECTOR_BYTES,
};
use crate::device::sync::{RootSources, SourceEntry, SourceError, Sources as _};
use crate::domain::device::{
    delivery_token, DesiredItem, Hold, OpKind, PlaylistOpKind, SourceHash,
};
use crate::domain::relpath::RelPath;

/// 報告（`/report`・`/plans/{id}/abandon`）の本文の上限
pub const REPORT_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// Bearer で確定した端末。ハンドラは `Extension<AgentDevice>` で受け取る
#[derive(Debug, Clone)]
pub struct AgentDevice {
    pub id: i64,
    pub uuid: String,
    pub name: String,
}

/// `/api/agent/*`（pair を除く）の Router。`guard` を route_layer で載せる
pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/manifest", get(manifest))
        .route("/files/{track_id}", get(file))
        .route("/plans", post(confirm))
        .route("/plans/open", get(open_plan))
        .route(
            "/plans/{id}/abandon",
            post(abandon).layer(DefaultBodyLimit::max(REPORT_BODY_LIMIT)),
        )
        .route(
            "/report",
            post(report).layer(DefaultBodyLimit::max(REPORT_BODY_LIMIT)),
        )
        .fallback(not_found)
        .route_layer(middleware::from_fn_with_state(state, guard))
}

/// ロックモード（パスワード未設定）なら 503。pair を含むエージェントの全経路で使う
fn locked(state: &AppState) -> Option<Response> {
    (state.auth.mode == auth::Mode::Locked)
        .then(|| error_response(StatusCode::SERVICE_UNAVAILABLE, "locked"))
}

/// `pair` だけに載せる層。pair は Bearer を要らないが、ロックモードでは閉じる
pub async fn locked_guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if let Some(r) = locked(&state) {
        return r;
    }
    next.run(req).await
}

/// `/api/agent/*`（pair を除く）の認証。`Authorization: Bearer <selector>.<secret>` だけを見る。
/// セッション Cookie・CSRF・trusted_cidrs は見ない
pub async fn guard(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    if let Some(r) = locked(&state) {
        return r;
    }
    let Some(text) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
    else {
        return unauthorized();
    };
    let Some((selector, secret)) =
        credential::split(text, TOKEN_SELECTOR_BYTES, TOKEN_SECRET_BYTES)
    else {
        return unauthorized();
    };
    let found = match state
        .db
        .read(move |c| dbdev::agent_by_selector(c, &selector))
        .await
    {
        Ok(f) => f,
        Err(e) => return ApiError::from(e).into_response(),
    };
    let Some(a) = found else {
        return unauthorized();
    };
    if !credential::ct_eq(
        credential::token_hash(&secret).as_bytes(),
        a.secret_hash.as_bytes(),
    ) {
        tracing::warn!(
            device_id = a.device_id,
            "エージェントのトークンが一致しない"
        );
        return unauthorized();
    }
    req.extensions_mut().insert(AgentDevice {
        id: a.device_id,
        uuid: a.uuid,
        name: a.name,
    });
    next.run(req).await
}

fn unauthorized() -> Response {
    let mut r = error_response(StatusCode::UNAUTHORIZED, "unauthenticated");
    r.headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    r
}

async fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not_found")
}

pub(crate) fn to_proto_op(k: OpKind) -> agent_proto::OpKind {
    match k {
        OpKind::Delete => agent_proto::OpKind::Delete,
        OpKind::Move => agent_proto::OpKind::Move,
        OpKind::UpdateMove => agent_proto::OpKind::UpdateMove,
        OpKind::Update => agent_proto::OpKind::Update,
        OpKind::Add => agent_proto::OpKind::Add,
    }
}

pub(crate) fn to_proto_pl_op(k: PlaylistOpKind) -> agent_proto::PlaylistOpKind {
    match k {
        PlaylistOpKind::Add => agent_proto::PlaylistOpKind::Add,
        PlaylistOpKind::Update => agent_proto::PlaylistOpKind::Update,
        PlaylistOpKind::Delete => agent_proto::PlaylistOpKind::Delete,
    }
}

/// `GET /api/agent/manifest`: 端末の desired・プレイリスト・差分（UI の差分と同じスナップショットから作る）
async fn manifest(
    State(state): State<AppState>,
    Extension(dev): Extension<AgentDevice>,
) -> Result<Response, ApiError> {
    let snap = state.db.device_snapshot().await?;
    let Some(d) = snap.get(dev.id) else {
        return Ok(error_response(StatusCode::NOT_FOUND, "not_found"));
    };
    let id = dev.id;
    let evals = state
        .db
        .read(move |c| dbdev::smart_evaluations(c, id))
        .await?;
    let pending =
        state.reeval_pending() && devices::waits_for_reevaluation(d.device.selection, &evals);
    let c = &d.computed;
    // agent のプレイリストの中身は `[[track_id, path], …]` の JSON（render_playlist）。id だけを並べる
    let playlists = c
        .playlists
        .iter()
        .map(|p| {
            let entries: Vec<(i64, String)> = serde_json::from_slice(&p.body)
                .map_err(|e| ApiError::Internal(format!("プレイリストの中身を読めない: {e}")))?;
            Ok(ManifestPlaylist {
                playlist_id: p.playlist_id,
                name: p.name.clone(),
                token: p.token.clone(),
                tracks: entries.into_iter().map(|(id, _)| id).collect(),
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    let body = ManifestResponse {
        device_uuid: d.device.uuid.clone(),
        device_name: d.device.name.clone(),
        generation: c.generation,
        plan_token: c.plan_token.clone(),
        pending_reevaluation: pending,
        items: c
            .desired
            .iter()
            .map(|x| ManifestItem {
                track_id: x.track_id,
                dest_path: x.dest_path.clone(),
                token: x.token.clone(),
                size: x.size,
                sha256: x.sha256.clone(),
            })
            .collect(),
        playlists,
        diff: DiffView {
            items: c
                .diff
                .items
                .iter()
                .map(|o| ItemOp {
                    op: to_proto_op(o.kind),
                    track_id: o.track_id,
                    from: o.from.clone(),
                    to: o.to.clone(),
                    token: o.token.clone(),
                    size: o.size,
                    sha256: o.sha256.clone(),
                })
                .collect(),
            held: c
                .diff
                .held
                .iter()
                .map(|h| Held {
                    track_id: h.track_id,
                    reason: h.hold.reason().to_owned(),
                    waiting: matches!(h.hold, Hold::Wait(_)),
                    has_copy: h.has_copy,
                })
                .collect(),
            playlists: c
                .diff
                .playlists
                .iter()
                .map(|p| PlaylistOp {
                    op: to_proto_pl_op(p.kind),
                    playlist_id: p.playlist_id,
                    from: p.from.clone(),
                    to: p.to.clone(),
                    token: p.token.clone(),
                })
                .collect(),
            playlist_errors: c
                .diff
                .playlist_errors
                .iter()
                .map(|(id, r)| PlaylistError {
                    playlist_id: *id,
                    reason: (*r).to_owned(),
                })
                .collect(),
        },
    };
    devices::enqueue_hashes(&state, &snap).await?;
    Ok(Json(body).into_response())
}

/// `If-Match` の値から配信トークン（`"<64 桁の小文字 16 進>"` の中身）を取り出す。
/// 弱い ETag・`*`・並記・それ以外の形は None（412）
fn strong_token(v: &str) -> Option<&str> {
    let inner = v.trim().strip_prefix('"')?.strip_suffix('"')?;
    (inner.len() == 64
        && inner
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(inner)
}

fn precondition_failed() -> Response {
    error_response(StatusCode::PRECONDITION_FAILED, "precondition_failed")
}

/// 端末の desired から曲を引き、記録したハッシュと合わせた結果
enum Resolved {
    Found(Box<DesiredItem>, SourceHash),
    /// 端末の desired に無い（別端末の曲もここ）
    NotDesired,
    /// ハッシュの行が無い・送る元の意味が変わった
    NoHash,
}

async fn resolve_source(
    state: &AppState,
    device_id: i64,
    track_id: i64,
    fresh: bool,
) -> Result<Resolved, ApiError> {
    let snap = if fresh {
        state.db.device_snapshot().await?
    } else {
        state.db.device_snapshot_for_display().await?
    };
    let Some(item) = snap
        .get(device_id)
        .and_then(|d| d.computed.desired.iter().find(|x| x.track_id == track_id))
        .cloned()
    else {
        return Ok(Resolved::NotDesired);
    };
    let kind = item.source.kind;
    let hash = state
        .db
        .read(move |c| dbdev::source_hash(c, track_id, kind))
        .await?;
    Ok(match hash {
        Some(h) if h.semantic == item.source.semantic => Resolved::Found(Box::new(item), h),
        _ => Resolved::NoHash,
    })
}

/// `GET /api/agent/files/{track_id}`: 配信トークンを強い `If-Match` で確かめてから、送る元を Range 対応で返す。
/// 送る元は開いた FD の identity を `source_hashes` と照合する（違えば行を忘れて 412 `source_changed`）
async fn file(
    State(state): State<AppState>,
    Extension(dev): Extension<AgentDevice>,
    Path(track_id): Path<i64>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let (Some(library), Some(derived)) = (state.library.clone(), state.derived.clone()) else {
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "files_unavailable",
        ));
    };
    let mut if_match = headers.get_all(header::IF_MATCH).iter();
    let Some(first) = if_match.next() else {
        return Ok(error_response(
            StatusCode::PRECONDITION_REQUIRED,
            "if_match_required",
        ));
    };
    if if_match.next().is_some() {
        return Ok(precondition_failed());
    }
    let Some(wanted) = first
        .to_str()
        .ok()
        .and_then(strong_token)
        .map(str::to_owned)
    else {
        return Ok(precondition_failed());
    };

    // 表示用のスナップショット（最大 2 秒古い）で引き、外れたときだけ最新で引き直す。
    // 最新の manifest の直後なら表示用で当たる。古さで 404 / 412 を返さないために引き直す
    let mut resolved = resolve_source(&state, dev.id, track_id, false).await?;
    if !matches!(resolved, Resolved::Found(..)) {
        resolved = resolve_source(&state, dev.id, track_id, true).await?;
    }
    let (item, hash) = match resolved {
        Resolved::Found(item, hash) => (item, hash),
        Resolved::NotDesired => return Ok(error_response(StatusCode::NOT_FOUND, "not_found")),
        Resolved::NoHash => return Ok(precondition_failed()),
    };
    let kind = item.source.kind;
    let token = delivery_token(&hash.semantic, &hash.sha256);
    if !credential::ct_eq(wanted.as_bytes(), token.as_bytes()) {
        return Ok(precondition_failed());
    }

    let rel_path = RelPath::parse(&item.source.root_rel_path)
        .map_err(|e| ApiError::Internal(format!("送る元のパスが不正（{e}）")))?;
    let size = hash.size;
    let entry = SourceEntry {
        kind,
        rel_path,
        hash,
    };
    let opened = tokio::task::spawn_blocking(move || {
        RootSources::new(library, derived, HashMap::from([(track_id, entry)])).open(track_id)
    })
    .await
    .map_err(|e| ApiError::Internal(format!("open タスクが異常終了: {e}")))?;
    let file = match opened {
        Ok(f) => f,
        Err(e @ (SourceError::Changed | SourceError::Missing)) => {
            tracing::warn!(device_id = dev.id, track_id, error = %e, "送る元が配信トークンの時点から変わった");
            state
                .db
                .write(move |c| dbdev::forget_source_hash(c, track_id, kind))
                .await?;
            return Ok(error_response(
                StatusCode::PRECONDITION_FAILED,
                "source_changed",
            ));
        }
        Err(SourceError::Other(e)) => return Err(ApiError::Internal(e)),
    };

    let range = match parse_range(
        headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
        size,
    ) {
        Ok(r) => r,
        Err(Unsatisfiable) => return range_not_satisfiable(size),
    };
    let etag = HeaderValue::from_str(&format!("\"{token}\""))
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    ranged_body(
        file,
        size,
        range,
        etag,
        "application/octet-stream",
        method == Method::HEAD,
    )
}

// 以下は仮のハンドラ（Task 7〜8 で置き換える）

async fn confirm() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

async fn open_plan() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

async fn abandon() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

async fn report() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

fn bad_request(msg: impl Into<String>) -> Response {
    error_response_with_message(StatusCode::BAD_REQUEST, "bad_request", msg)
}

fn invalid_code() -> Response {
    error_response(StatusCode::UNAUTHORIZED, "invalid_code")
}

/// `POST /api/agent/pair`: ワンタイムコードを消費してトークンを返す
pub async fn pair(
    State(state): State<AppState>,
    body: Result<Json<PairRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Ok(Json(body)) = body else {
        return Ok(bad_request("本文が不正"));
    };
    let Some((selector, secret)) =
        credential::split(body.code.trim(), PAIR_SELECTOR_BYTES, PAIR_SECRET_BYTES)
    else {
        return Ok(invalid_code());
    };
    let now = now_epoch();
    let sel = selector.clone();
    let reserved = state
        .db
        .write(move |c| dbdev::reserve_pair_attempt(c, &sel, now))
        .await?;
    let PairReserve::Reserved {
        device_id,
        code_hash,
    } = reserved
    else {
        return Ok(invalid_code());
    };
    let h = code_hash.clone();
    let ok = tokio::task::spawn_blocking(move || verify_bytes(&secret, &h))
        .await
        .map_err(DbError::from)?;
    if !ok {
        let sel = selector.clone();
        state
            .db
            .write(move |c| dbdev::fail_pair_attempt(c, device_id, &sel))
            .await?;
        tracing::warn!(device_id, "pair のコードが一致しない");
        return Ok(invalid_code());
    }
    let token = credential::issue(TOKEN_SELECTOR_BYTES, TOKEN_SECRET_BYTES)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let token_secret = credential::base32_decode(&token.secret)
        .ok_or_else(|| ApiError::Internal("トークンを作れない".into()))?;
    let (th, tsel) = (
        credential::token_hash(&token_secret),
        token.selector.clone(),
    );
    let consumed = state
        .db
        .write(move |c| {
            let tx = c.transaction()?;
            let ok = dbdev::consume_pair_code(
                &tx,
                device_id,
                &selector,
                &code_hash,
                &tsel,
                &th,
                now_epoch(),
            )?;
            let d = if ok {
                dbdev::get(&tx, device_id)?
            } else {
                None
            };
            tx.commit()?;
            Ok(d)
        })
        .await?;
    let Some(d) = consumed else {
        return Ok(invalid_code());
    };
    tracing::info!(device_id, "エージェントを pair した");
    Ok(Json(PairResponse {
        device_uuid: d.uuid,
        device_name: d.name,
        token: token.text(),
    })
    .into_response())
}

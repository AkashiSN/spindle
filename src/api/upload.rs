//! ブラウザからのアップロード（SPEC §7.12、API は SPEC §9）。
//!
//! - `POST /api/uploads` 作る / `GET /api/uploads`・`GET /api/uploads/:id` 状態（先に照合と
//!   `sync_with_disk` を走らせ、実体に合わせた結果をマニフェストに書く）
//! - `POST /api/uploads/:id/origins` origin を作る / `PUT …/origins/:origin?offset=` チャンク
//! - `POST …/origins/:origin/unpack` やり直せる失敗の zip を展開し直す
//! - `PATCH /api/uploads/:id` 分け方の変更 / `POST …/publish` 公開（冪等）/ `DELETE` 捨てる
//! - `DELETE /api/uploads/broken/:dir` 壊れたステージングに捨てる印を付ける
//!
//! マニフェストの鍵は std の Mutex なので、マニフェストとファイルの操作はすべてブロッキングの
//! スレッド（`spawn_blocking` / `Db::read` の閉包）で行う。鍵の表に項を作るのは、ステージングの
//! ディレクトリがあると確かめてから（無い id で表を太らせない）

use std::collections::HashSet;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::db::now_epoch;
use crate::fsroot::RootDir;
use crate::import::upload::{
    begin_origin, build_view_cached, create_upload, ensure_staging, freeze_cached,
    has_discard_mark, list_uploads, mark_broken_discarded, origin_view, receive_chunk,
    reconcile_jobs, sync_with_disk, with_manifest, FileClass, Listed, Manifest, OriginView,
    UploadError, UploadId, UploadView,
};
use crate::jobs::handlers::upload::{new_publish_job, new_unpack_job, upload_busy};
use crate::jobs::EnqueueResult;

use super::error::{error_response, error_response_with_message, ApiError};
use super::state::UploadsState;
use super::AppState;

/// `PUT …/origins/:origin` の 1 リクエストの本文の上限（web は 32 MiB ずつ送る）
pub const CHUNK_LIMIT: usize = 40 << 20;

/// ハンドラの失敗の応答（`Response` は大きいので箱に入れて `?` で返す）
pub struct Fail(Box<Response>);

impl From<Response> for Fail {
    fn from(r: Response) -> Self {
        Fail(Box::new(r))
    }
}

impl From<UploadError> for Fail {
    fn from(e: UploadError) -> Self {
        upload_error(e).into()
    }
}

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        *self.0
    }
}

/// ハンドラが使う root と状態
struct Ctx {
    inbox: Arc<RootDir>,
    up: Arc<UploadsState>,
}

fn ctx(state: &AppState) -> Result<Ctx, Fail> {
    match (&state.inbox, &state.uploads) {
        (Some(inbox), Some(up)) => Ok(Ctx {
            inbox: Arc::clone(inbox),
            up: Arc::clone(up),
        }),
        _ => Err(error_response(StatusCode::SERVICE_UNAVAILABLE, "inbox_unavailable").into()),
    }
}

#[derive(Serialize)]
struct OffsetBody {
    error: &'static str,
    received: u64,
}

/// アップロードの失敗を応答へ
fn upload_error(e: UploadError) -> Response {
    match e {
        UploadError::BadPath(m) => {
            error_response_with_message(StatusCode::BAD_REQUEST, "bad_path", m)
        }
        UploadError::Exists(m) => error_response_with_message(StatusCode::CONFLICT, "exists", m),
        UploadError::NotFound => error_response(StatusCode::NOT_FOUND, "not_found"),
        UploadError::OriginNotFound => error_response(StatusCode::NOT_FOUND, "origin_not_found"),
        UploadError::Offset { received } => (
            StatusCode::CONFLICT,
            Json(OffsetBody {
                error: "offset",
                received,
            }),
        )
            .into_response(),
        UploadError::Limit(m) => {
            error_response_with_message(StatusCode::PAYLOAD_TOO_LARGE, "limit", m)
        }
        UploadError::NoSpace => error_response(StatusCode::INSUFFICIENT_STORAGE, "no_space"),
        UploadError::Busy => error_response(StatusCode::CONFLICT, "busy"),
        UploadError::Conflict(code) => match code.as_str() {
            "incomplete" => error_response(StatusCode::CONFLICT, "incomplete"),
            "empty" => error_response(StatusCode::CONFLICT, "empty"),
            "not_frozen" => error_response(StatusCode::CONFLICT, "not_frozen"),
            "not_broken" => error_response(StatusCode::CONFLICT, "not_broken"),
            "not_deletable" => error_response(StatusCode::CONFLICT, "not_deletable"),
            "not_retryable" => error_response(StatusCode::CONFLICT, "not_retryable"),
            _ => error_response_with_message(StatusCode::CONFLICT, "conflict", code),
        },
        e => {
            tracing::error!(error = %e, "アップロードの API の内部エラー");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal")
        }
    }
}

/// ブロッキングのスレッドで `f` を走らせる（マニフェストの鍵とファイルの操作）
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, UploadError> + Send + 'static,
) -> Result<T, Fail> {
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => Ok(r?),
        Err(e) => Err(
            ApiError::Internal(format!("アップロードの処理が異常終了: {e}"))
                .into_response()
                .into(),
        ),
    }
}

/// 読み取りのコネクションを持ってブロッキングのスレッドで `f` を走らせる（`Db::read` の閉包は
/// spawn_blocking の中で走る）。ジョブの有無（`upload_busy`）・ライブラリとの一致を見る処理用
async fn with_conn<T: Send + 'static>(
    state: &AppState,
    f: impl FnOnce(&Connection) -> Result<T, UploadError> + Send + 'static,
) -> Result<T, Fail> {
    match state.db.read(move |c| Ok(f(c))).await {
        Ok(r) => Ok(r?),
        Err(e) => Err(ApiError::Db(e).into_response().into()),
    }
}

fn parse_id(raw: &str) -> Result<UploadId, Fail> {
    Ok(UploadId::parse(raw)?)
}

/// 照合（取りこぼした展開・公開の投入）を走らせ、投入したらワーカーを起こす。失敗は表示を止めない
async fn run_reconcile(state: &AppState, cx: &Ctx) {
    match reconcile_jobs(&state.db, Arc::clone(&cx.inbox), &cx.up.locks).await {
        Ok(ids) => state.jobs.notify_enqueued(&ids).await,
        Err(e) => tracing::warn!(error = %e, "アップロードの照合に失敗"),
    }
}

/// ステージングがあると確かめてから鍵を取り、実体に合わせ（変われば書く）、写しを返す。
/// 捨てたものは変えない
fn synced(inbox: &RootDir, cx_up: &UploadsState, id: &UploadId) -> Result<Manifest, UploadError> {
    ensure_staging(inbox, id)?;
    with_manifest(inbox, &cx_up.locks, id, |m| {
        if m.discarded_at.is_none() {
            sync_with_disk(inbox, m)?;
        }
        Ok(m.clone())
    })
}

/// 表示を組み立てる（呼び出し側がブロッキングのスレッドでコネクションを持つ）
fn view_of(
    c: &Connection,
    state_cfg: &crate::config::LayoutConfig,
    inbox: &RootDir,
    up: &UploadsState,
    m: &Manifest,
) -> Result<UploadView, UploadError> {
    let id = UploadId::parse(&m.id)?;
    let busy = upload_busy(c, &id)?;
    build_view_cached(c, state_cfg, inbox, m, &up.cfg, busy, &up.cache)
}

pub async fn create(State(state): State<AppState>) -> Result<Response, Fail> {
    let cx = ctx(&state)?;
    let layout = state.config.layout.clone();
    let (inbox, up) = (cx.inbox, cx.up);
    let r = with_conn(&state, move |c| {
        let m = create_upload(&inbox, now_epoch())?;
        view_of(c, &layout, &inbox, &up, &m)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(r)).into_response())
}

#[derive(Serialize)]
pub struct BrokenView {
    /// Inbox 相対のステージングのディレクトリ（`.spindle-upload-<hex>`）
    pub dir: String,
    pub mtime_ns: i64,
    pub reason: String,
    /// 捨てられるか（マニフェストが無いか JSON として壊れている）。false は読み取りの I/O の失敗・
    /// 新しい版などで、GC も消さない（管理者が確かめる）
    pub deletable: bool,
}

#[derive(Serialize)]
pub struct ListView {
    pub items: Vec<UploadView>,
    pub broken: Vec<BrokenView>,
}

pub async fn list(State(state): State<AppState>) -> Result<Response, Fail> {
    let cx = ctx(&state)?;
    run_reconcile(&state, &cx).await;
    let layout = state.config.layout.clone();
    let (inbox, up) = (cx.inbox, cx.up);
    let r = with_conn(&state, move |c| {
        let mut items = Vec::new();
        let mut broken = Vec::new();
        let mut keep = HashSet::new();
        for listed in list_uploads(&inbox)? {
            match listed {
                Listed::Ok(m) => {
                    if m.discarded_at.is_some() {
                        continue;
                    }
                    let id = match UploadId::parse(&m.id) {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::warn!(upload = m.id.as_str(), error = %e, "アップロードの ID が不正");
                            continue;
                        }
                    };
                    // 1 つのアップロードの失敗で一覧全体を落とさない（壊れたものとして出す）
                    let one = synced(&inbox, &up, &id).and_then(|m| {
                        if m.discarded_at.is_some() {
                            return Ok(None);
                        }
                        view_of(c, &layout, &inbox, &up, &m).map(Some)
                    });
                    match one {
                        Ok(Some(v)) => {
                            keep.insert(v.id.clone());
                            items.push(v);
                        }
                        Ok(None) => {}
                        // 一覧の後に GC が消した
                        Err(UploadError::NotFound) => {}
                        Err(e) => {
                            tracing::warn!(upload = id.as_str(), error = %e, "アップロードの状態を作れない");
                            let dir = id.staging_dir();
                            let mtime_ns = inbox.stat(&dir).map(|st| st.mtime_ns).unwrap_or(0);
                            broken.push(BrokenView {
                                dir: dir.as_str().to_string(),
                                mtime_ns,
                                reason: e.to_string(),
                                // マニフェストは読める: 画面は DELETE /api/uploads/:id で捨てる
                                deletable: true,
                            });
                        }
                    }
                }
                Listed::Broken {
                    dir,
                    mtime_ns,
                    reason,
                    deletable,
                } => {
                    // 捨てる印を付けたものは GC を待つだけなので出さない（確かめられなければ出す）
                    match has_discard_mark(&inbox, &dir) {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(e) => {
                            tracing::warn!(dir = %dir, error = %e, "捨てる印を確かめられない")
                        }
                    }
                    broken.push(BrokenView {
                        dir: dir.as_str().to_string(),
                        mtime_ns,
                        reason,
                        deletable,
                    });
                }
            }
        }
        up.cache.retain_uploads(&keep);
        items.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        broken.sort_by(|a, b| b.mtime_ns.cmp(&a.mtime_ns).then(a.dir.cmp(&b.dir)));
        Ok(ListView { items, broken })
    })
    .await?;
    Ok(Json(r).into_response())
}

pub async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, Fail> {
    let cx = ctx(&state)?;
    let id = parse_id(&id)?;
    run_reconcile(&state, &cx).await;
    let layout = state.config.layout.clone();
    let (inbox, up) = (cx.inbox, cx.up);
    let r = with_conn(&state, move |c| {
        let m = match synced(&inbox, &up, &id) {
            Ok(m) => m,
            Err(UploadError::NotFound) => {
                // GC が消し終えた: 覚えも落とす
                up.cache.forget(id.as_str());
                return Err(UploadError::NotFound);
            }
            Err(e) => return Err(e),
        };
        // 捨てたものは出さない（公開して捨てた扱いになったものは、公開の結果を見せるため出す）
        if m.discarded_at.is_some() && m.publish.is_none() {
            return Err(UploadError::NotFound);
        }
        view_of(c, &layout, &inbox, &up, &m)
    })
    .await?;
    Ok(Json(r).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeginBody {
    pub path: String,
    pub total: u64,
}

#[derive(Serialize)]
pub struct BeginView {
    pub origin: OriginView,
}

pub async fn begin(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<BeginBody>,
) -> Result<Response, Fail> {
    let cx = ctx(&state)?;
    let id = parse_id(&id)?;
    let (inbox, up) = (cx.inbox, cx.up);
    let r = blocking(move || {
        ensure_staging(&inbox, &id)?;
        begin_origin(
            &inbox,
            &up.locks,
            &id,
            &body.path,
            body.total,
            &up.cfg,
            now_epoch(),
        )
    })
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(BeginView {
            origin: origin_view(&r),
        }),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct ChunkQuery {
    pub offset: u64,
}

#[derive(Serialize)]
pub struct ChunkView {
    pub received: u64,
    pub complete: bool,
}

#[derive(Serialize)]
pub struct Accepted {
    pub job_id: i64,
}

pub async fn chunk(
    State(state): State<AppState>,
    Path((id, origin)): Path<(String, String)>,
    Query(q): Query<ChunkQuery>,
    body: Bytes,
) -> Result<Response, Fail> {
    let cx = ctx(&state)?;
    let id = parse_id(&id)?;
    let (inbox, up) = (cx.inbox, cx.up);
    let (bid, borigin) = (id.clone(), origin.clone());
    let (r, unpack) = blocking(move || {
        ensure_staging(&inbox, &bid)?;
        let r = receive_chunk(
            &inbox,
            &up.locks,
            &bid,
            &borigin,
            q.offset,
            &body,
            now_epoch(),
            &|_| false,
        )?;
        // 受け取り終えた未展開の zip は展開を投入する
        let unpack = r.complete
            && with_manifest(&inbox, &up.locks, &bid, |m| {
                Ok(m.origins
                    .iter()
                    .any(|o| o.id == borigin && o.needs_unpack()))
            })?;
        Ok((r, unpack))
    })
    .await?;
    if unpack {
        if let Err(e) = state
            .jobs
            .enqueue(new_unpack_job(&id, &origin, false))
            .await
        {
            // 照合（一覧・単体の取得）が投入し直す
            tracing::warn!(upload = id.as_str(), origin = origin.as_str(), error = %e, "展開を投入できない");
        }
    }
    Ok(Json(ChunkView {
        received: r.received,
        complete: r.complete,
    })
    .into_response())
}

pub async fn retry_unpack(
    State(state): State<AppState>,
    Path((id, origin)): Path<(String, String)>,
) -> Result<Response, Fail> {
    let cx = ctx(&state)?;
    let id = parse_id(&id)?;
    let (inbox, up) = (cx.inbox, cx.up);
    let (bid, borigin) = (id.clone(), origin.clone());
    blocking(move || {
        ensure_staging(&inbox, &bid)?;
        with_manifest(&inbox, &up.locks, &bid, |m| {
            if m.publish_requested_at.is_some() || m.discarded_at.is_some() {
                return Err(UploadError::Busy);
            }
            let o = m
                .origins
                .iter()
                .find(|o| o.id == borigin)
                .ok_or(UploadError::OriginNotFound)?;
            let retryable = o.class == FileClass::Zip
                && !o.removed
                && !o.damaged
                && o.zip
                    .as_ref()
                    .and_then(|z| z.error.as_ref())
                    .is_some_and(|e| e.retryable);
            if retryable {
                Ok(())
            } else {
                Err(UploadError::Conflict("not_retryable".into()))
            }
        })
    })
    .await?;
    accepted(&state, new_unpack_job(&id, &origin, true)).await
}

/// ジョブを投入して 202（active な同じジョブがあればその id）
async fn accepted(state: &AppState, job: crate::jobs::NewJob) -> Result<Response, Fail> {
    match state.jobs.enqueue(job).await {
        Ok(EnqueueResult::Inserted(job_id)) | Ok(EnqueueResult::Duplicate(job_id)) => {
            Ok((StatusCode::ACCEPTED, Json(Accepted { job_id })).into_response())
        }
        Err(e) => Err(ApiError::Db(e).into_response().into()),
    }
}

/// `PATCH /api/uploads/:id`。与えた項目を置き換える（`removed_origins` は外す origin を足すだけ。
/// 外した origin は戻さない: 同じパスを送り直すと別の origin になるため）
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchBody {
    pub merges: Option<Vec<Vec<String>>>,
    pub excluded_files: Option<Vec<String>>,
    pub excluded_groups: Option<Vec<String>>,
    pub included_files: Option<Vec<String>>,
    pub included_groups: Option<Vec<String>>,
    pub removed_origins: Option<Vec<String>>,
}

/// 展開・公開のジョブが active か、公開を要求済み・捨てたものは変えない（鍵の中で呼ぶ）
fn ensure_idle(c: &Connection, id: &UploadId, m: &Manifest) -> Result<(), UploadError> {
    if m.publish_requested_at.is_some() || m.discarded_at.is_some() || upload_busy(c, id)? {
        return Err(UploadError::Busy);
    }
    Ok(())
}

pub async fn patch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PatchBody>,
) -> Result<Response, Fail> {
    let cx = ctx(&state)?;
    let id = parse_id(&id)?;
    let layout = state.config.layout.clone();
    let (inbox, up) = (cx.inbox, cx.up);
    let r = with_conn(&state, move |c| {
        ensure_staging(&inbox, &id)?;
        let m = with_manifest(&inbox, &up.locks, &id, |m| {
            ensure_idle(c, &id, m)?;
            if let Some(removed) = &body.removed_origins {
                if removed
                    .iter()
                    .any(|r| !m.origins.iter().any(|o| &o.id == r))
                {
                    return Err(UploadError::OriginNotFound);
                }
                for o in m.origins.iter_mut().filter(|o| removed.contains(&o.id)) {
                    o.removed = true;
                }
            }
            if let Some(v) = body.merges {
                m.merges = v;
            }
            if let Some(v) = body.excluded_files {
                m.excluded_files = v;
            }
            if let Some(v) = body.excluded_groups {
                m.excluded_groups = v;
            }
            if let Some(v) = body.included_files {
                m.included_files = v;
            }
            if let Some(v) = body.included_groups {
                m.included_groups = v;
            }
            m.updated_at = now_epoch();
            Ok(m.clone())
        })?;
        view_of(c, &layout, &inbox, &up, &m)
    })
    .await?;
    Ok(Json(r).into_response())
}

pub async fn publish(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, Fail> {
    let cx = ctx(&state)?;
    let id = parse_id(&id)?;
    let layout = state.config.layout.clone();
    let (inbox, up) = (cx.inbox, cx.up);
    let bid = id.clone();
    with_conn(&state, move |c| {
        ensure_staging(&inbox, &bid)?;
        // 凍結（既に凍結してあれば何もしない）。押し直しは止まった公開の失敗を消して投入し直す
        // 捨てた後に置き終えていない計画は 409 busy（止まった失敗も消さない）
        freeze_cached(
            &inbox,
            &up.locks,
            &bid,
            c,
            &layout,
            &up.cfg,
            now_epoch(),
            &up.cache,
        )?;
        with_manifest(&inbox, &up.locks, &bid, |m| {
            m.publish_error = None;
            Ok(())
        })?;
        up.cache.forget(bid.as_str());
        Ok(())
    })
    .await?;
    accepted(&state, new_publish_job(&id)).await
}

pub async fn discard(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, Fail> {
    let cx = ctx(&state)?;
    let id = parse_id(&id)?;
    let (inbox, up) = (cx.inbox, cx.up);
    with_conn(&state, move |c| {
        ensure_staging(&inbox, &id)?;
        with_manifest(&inbox, &up.locks, &id, |m| {
            if m.discarded_at.is_some()
                && (m.publish_requested_at.is_none() || m.publish_error.is_some())
            {
                // 捨て済み（二度押し）
                return Ok(());
            }
            // ジョブの判定と discarded_at の書き込みは同じ鍵の中（展開・公開と競合しない）
            if m.publish_error.is_some() {
                // 公開がやり直しても変わらない失敗で止まったものは、公開ジョブが動いていなければ
                // 捨てられる（そうしないと押し直すしかなく、永久に残る）
                if upload_busy(c, &id)? {
                    return Err(UploadError::Busy);
                }
            } else {
                ensure_idle(c, &id, m)?;
            }
            m.discarded_at = Some(now_epoch());
            Ok(())
        })?;
        up.cache.forget(id.as_str());
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn discard_broken(
    State(state): State<AppState>,
    Path(dir): Path<String>,
) -> Result<Response, Fail> {
    let cx = ctx(&state)?;
    let Some(id) = UploadId::from_dir_name(&dir) else {
        return Ok(error_response(StatusCode::BAD_REQUEST, "bad_path"));
    };
    let (inbox, up) = (cx.inbox, cx.up);
    blocking(move || mark_broken_discarded(&inbox, &up.locks, &id)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

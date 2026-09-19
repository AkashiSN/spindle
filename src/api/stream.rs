//! `GET /api/stream/:id`（SPEC §11、P1-9、D-52）。
//!
//! - 既定は Library の原本を Range 対応で返す（単一範囲、206 / 416、HEAD、`ETag`）。ファイルは
//!   `RootDir::open_file`（dirfd 基準）で開き、**開いた FD の fstat が DB の行と一致する**ことを
//!   確かめてから返す（パスは識別子ではない。同名で差し替えられていれば 409 `stale`）
//! - `?transcode=opus` は `delivery` ビューが Derived を指す（音声版が一致）なら Derived の Opus を
//!   同じ Range 対応で直送する（Derived が変換結果のキャッシュ）。無ければ ffmpeg を stdout パイプで
//!   起動して Ogg/Opus を chunked で返す（Range 非対応、`?start=<秒>` を `-ss` に渡す）。非可逆は
//!   変換せず原本を返す（多重劣化の回避。D-8）
//! - trusted_cidrs からはセッション無しで通る（`auth::is_allowlisted`）

use std::fs::File;
use std::future::Future as _;
use std::io::{Seek as _, SeekFrom};
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_core::Stream;
use rusqlite::{Connection, OptionalExtension as _};
use serde::Deserialize;
use tokio::io::AsyncReadExt as _;
use tokio::process::{ChildStdout, Command};
use tokio_util::io::ReaderStream;

use crate::db::Result as DbResult;
use crate::domain::relpath::RelPath;
use crate::domain::tags::Codec;
use crate::fsroot::{self, FsError, RootDir};
use crate::jobs::process::ChildGroup;
use crate::jobs::JobError;
use tokio_util::sync::CancellationToken;

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;

/// 変換の上限（トラック長に足す猶予）の既定。`AppState::transcode_grace`
pub const TRANSCODE_GRACE: Duration = Duration::from_secs(60);
/// トラック長が不明なときの変換の上限
const TRANSCODE_DEFAULT_LIMIT: Duration = Duration::from_secs(3600);
/// Opus のビットレート（D-9）
const TRANSCODE_BITRATE: &str = "128k";

#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    #[serde(default)]
    pub transcode: Option<String>,
    /// 変換時の開始位置（秒）
    #[serde(default)]
    pub start: Option<f64>,
}

/// 再生に要る行
struct StreamRow {
    rel_path: String,
    codec: String,
    lossless: bool,
    missing: bool,
    inode: Option<i64>,
    size: i64,
    mtime_ns: i64,
    ctime_ns: i64,
    duration_ms: Option<i64>,
    /// `delivery` ビューの path（`Derived/...` なら Derived が使える）
    delivery_path: String,
}

fn load_row(conn: &Connection, id: i64) -> DbResult<Option<StreamRow>> {
    Ok(conn
        .query_row(
            "SELECT t.rel_path, t.codec, t.lossless, t.missing_since IS NOT NULL,
                    t.inode, t.size, t.mtime_ns, t.ctime_ns, t.duration_ms,
                    COALESCE(d.path, 'Library/' || t.rel_path)
             FROM tracks t LEFT JOIN delivery d ON d.track_id = t.id
             WHERE t.id = ?1",
            [id],
            |r| {
                Ok(StreamRow {
                    rel_path: r.get(0)?,
                    codec: r.get(1)?,
                    lossless: r.get::<_, i64>(2)? == 1,
                    missing: r.get::<_, i64>(3)? == 1,
                    inode: r.get(4)?,
                    size: r.get(5)?,
                    mtime_ns: r.get(6)?,
                    ctime_ns: r.get(7)?,
                    duration_ms: r.get(8)?,
                    delivery_path: r.get(9)?,
                })
            },
        )
        .optional()?)
}

impl StreamRow {
    /// 開いた FD がこの行の実体か（同じ実体で、DB に取り込んだ後に書かれていない）。
    /// dev は照合しない（マウントのたびに振り直されうる。D-62）
    fn matches(&self, st: &fsroot::Stat) -> bool {
        self.inode == Some(st.inode as i64)
            && self.size == st.size as i64
            && self.mtime_ns == st.mtime_ns
            && self.ctime_ns == st.ctime_ns
    }
}

/// codec から `Content-Type`
pub fn mime_of(codec: &str) -> &'static str {
    match Codec::parse(codec) {
        Some(Codec::Flac) => "audio/flac",
        Some(Codec::Opus | Codec::Ogg) => "audio/ogg",
        Some(Codec::Alac | Codec::Aac) => "audio/mp4",
        Some(Codec::Mp3) => "audio/mpeg",
        Some(Codec::Wav) => "audio/wav",
        Some(Codec::Aiff) => "audio/aiff",
        Some(Codec::Wv) => "audio/x-wavpack",
        Some(Codec::Ape) => "audio/x-ape",
        None => "application/octet-stream",
    }
}

/// Range が満たせない（416）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unsatisfiable;

/// `Range: bytes=a-b` / `a-` / `-n`（単一範囲）。`Ok(None)` は Range 無し・構文が不正（数値でない、
/// 桁あふれ、複数範囲、単位違い。無視して全体を 200 で返す）、`Err(Unsatisfiable)` は構文は正しいが
/// 範囲外（416。RFC 9110 §14.1.2 / §15.5.17）
pub fn parse_range(header: Option<&str>, size: u64) -> Result<Option<(u64, u64)>, Unsatisfiable> {
    let Some(h) = header else {
        return Ok(None);
    };
    let Some(spec) = h.trim().strip_prefix("bytes=") else {
        return Ok(None);
    };
    if spec.contains(',') {
        return Ok(None);
    }
    let Some((a, b)) = spec.split_once('-') else {
        return Ok(None);
    };
    let (a, b) = (a.trim(), b.trim());
    let num = |s: &str| -> Option<Option<u64>> {
        if s.is_empty() {
            return Some(None);
        }
        if !s.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        s.parse::<u64>().ok().map(Some)
    };
    let (Some(start), Some(end)) = (num(a), num(b)) else {
        return Ok(None); // 構文が不正
    };
    let range = match (start, end) {
        (None, None) => return Ok(None),
        (None, Some(n)) => {
            // 末尾 n バイト
            if n == 0 || size == 0 {
                return Err(Unsatisfiable);
            }
            let n = n.min(size);
            (size - n, size - 1)
        }
        (Some(start), None) => {
            if start >= size {
                return Err(Unsatisfiable);
            }
            (start, size - 1)
        }
        (Some(start), Some(end)) => {
            if end < start {
                return Ok(None); // 構文が不正
            }
            if start >= size {
                return Err(Unsatisfiable);
            }
            (start, end.min(size - 1))
        }
    };
    Ok(Some(range))
}

/// `If-None-Match` が `etag` に一致するか（`*`、弱比較 `W/`、複数の並記。RFC 9110 §13.1.2）
pub fn etag_matches(if_none_match: Option<&str>, etag: &str) -> bool {
    let Some(h) = if_none_match else {
        return false;
    };
    let h = h.trim();
    if h == "*" {
        return true;
    }
    let strip = |s: &str| s.trim().strip_prefix("W/").unwrap_or(s.trim()).to_owned();
    let want = strip(etag);
    h.split(',').any(|t| strip(t) == want)
}

pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<StreamQuery>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let (Some(library), Some(derived)) = (state.library.clone(), state.derived.clone()) else {
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "stream_unavailable",
        ));
    };
    let transcode = match q.transcode.as_deref() {
        None => false,
        Some("opus") => true,
        Some(other) => {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("transcode は opus のみ: {other:?}"),
            ))
        }
    };
    let Some(row) = state.db.read(move |c| load_row(c, id)).await? else {
        return Ok(error_response(StatusCode::NOT_FOUND, "not_found"));
    };
    if row.missing {
        return Ok(error_response(StatusCode::NOT_FOUND, "not_found"));
    }

    // Derived を直送できるか（可逆で音声版が一致）
    if transcode && row.lossless {
        if let Some(rel) = row.delivery_path.strip_prefix("Derived/") {
            let rel = RelPath::parse(rel).map_err(|e| ApiError::Internal(e.to_string()))?;
            match open_blocking(&derived, &rel).await? {
                Some((file, st)) => {
                    return serve_file(file, &st, "audio/ogg", &method, &headers).await;
                }
                None => {
                    // 行はあるが実体が無い（GC 待ち等）。変換に倒す
                    tracing::warn!(track_id = id, path = %rel, "Derived の実体が無いので変換する");
                }
            }
        }
    }

    // 原本を開いて行と照合する
    let rel = RelPath::parse(&row.rel_path).map_err(|e| ApiError::Internal(e.to_string()))?;
    let Some((file, st)) = open_blocking(&library, &rel).await? else {
        return Ok(error_response(StatusCode::NOT_FOUND, "not_found"));
    };
    if !row.matches(&st) {
        return Ok(error_response_with_message(
            StatusCode::CONFLICT,
            "stale",
            "ファイルが DB の行と一致しない（再スキャン待ち）",
        ));
    }
    if transcode && row.lossless {
        return transcode_opus(&state, id, file, q.start, row.duration_ms, &method);
    }
    serve_file(file, &st, mime_of(&row.codec), &method, &headers).await
}

/// root から開いて fstat する。無ければ None
async fn open_blocking(
    root: &Arc<RootDir>,
    rel: &RelPath,
) -> Result<Option<(File, fsroot::Stat)>, ApiError> {
    let root = Arc::clone(root);
    let rel = rel.clone();
    tokio::task::spawn_blocking(move || -> Result<Option<(File, fsroot::Stat)>, ApiError> {
        let file = match root.open_file(&rel) {
            Ok(f) => f,
            Err(FsError::NotFound) => return Ok(None),
            Err(e) => return Err(ApiError::Internal(e.to_string())),
        };
        let st = fsroot::fstat(&file).map_err(|e| ApiError::Internal(e.to_string()))?;
        Ok(Some((file, st)))
    })
    .await
    .map_err(|e| ApiError::Internal(format!("open タスクが異常終了: {e}")))?
}

/// 開いたファイルを Range 対応で返す
async fn serve_file(
    mut file: File,
    st: &fsroot::Stat,
    mime: &'static str,
    method: &Method,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let size = st.size;
    let etag = format!("\"{}-{}-{}\"", st.inode, st.size, st.mtime_ns);
    let etag_value = HeaderValue::from_str(&etag).map_err(|e| ApiError::Internal(e.to_string()))?;
    if etag_matches(
        headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok()),
        &etag,
    ) {
        let mut res = StatusCode::NOT_MODIFIED.into_response();
        let h = res.headers_mut();
        h.insert(header::ETAG, etag_value);
        h.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-cache"),
        );
        return Ok(res);
    }
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let (status, start, end) = match parse_range(range, size) {
        Ok(None) => (StatusCode::OK, 0, size.saturating_sub(1)),
        Ok(Some((a, b))) => (StatusCode::PARTIAL_CONTENT, a, b),
        Err(Unsatisfiable) => {
            let mut res = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            res.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{size}"))
                    .map_err(|e| ApiError::Internal(e.to_string()))?,
            );
            return Ok(res);
        }
    };
    let len = if size == 0 { 0 } else { end - start + 1 };
    let body = if *method == Method::HEAD || len == 0 {
        Body::empty()
    } else {
        file.seek(SeekFrom::Start(start))
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        let f = tokio::fs::File::from_std(file);
        Body::from_stream(ReaderStream::new(f.take(len)))
    };
    let mut res = (status, body).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&len.to_string()).map_err(|e| ApiError::Internal(e.to_string()))?,
    );
    h.insert(header::ETAG, etag_value);
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        h.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{size}"))
                .map_err(|e| ApiError::Internal(e.to_string()))?,
        );
    }
    Ok(res)
}

// ---------------------------------------------------------------- オンザフライ変換

/// ffmpeg の stdout を本文にする。
///
/// - `child` を持っているので、クライアントが切断して本文が drop されると子をプロセスグループごと
///   kill して reap する（`reap_in_background`）
/// - `deadline`（`Sleep`）も poll するので、stdout が止まったまま（ffmpeg が固まった）でも期限で
///   起こされて打ち切れる。打ち切りは本文のエラー（Err）で伝え、子は kill + reap
/// - stdout が EOF になったら子の終了を待って終了コードを検査する。非ゼロなら本文をエラーで終える
///   （200 の途中で切れる形になるが、成功と区別できる）
struct FfmpegBody {
    child: Option<ChildGroup>,
    stdout: Option<ReaderStream<ChildStdout>>,
    deadline: Pin<Box<tokio::time::Sleep>>,
    /// stdout の EOF 後: 子の終了待ち。`ChildGroup::wait` を別タスクで動かし、その JoinHandle を
    /// poll する。本文が drop されてもタスクは detach されて wait / reap を続け、期限では token を
    /// 倒してグループごと片付けさせる（future の中に child を抱えると drop で reap されない）
    waiting: Option<(
        CancellationToken,
        tokio::task::JoinHandle<Result<ExitStatus, JobError>>,
    )>,
    track_id: i64,
}

/// 子プロセスを非同期に片付ける（SIGTERM → 猶予 → SIGKILL、そして reap）。同期の drop だけだと
/// SIGKILL は送れても wait できず、zombie が runtime の reaper が回るまで残る。runtime の外
/// （あり得ないが）では drop に任せる（SIGKILL のみ）
fn reap_in_background(child: ChildGroup, track_id: i64) {
    match tokio::runtime::Handle::try_current() {
        Ok(h) => {
            h.spawn(async move {
                let mut child = child;
                child.kill_group().await;
                tracing::debug!(track_id, "ffmpeg を片付けた");
            });
        }
        Err(_) => drop(child),
    }
}

impl FfmpegBody {
    /// 子の後始末を始める（どの経路でも 1 回だけ）。stdout を先に閉じて子が SIGPIPE で止まれるように
    /// し、本体を持っていれば kill + reap のタスクへ、終了待ちのタスクに移っていれば token を倒す
    fn cleanup(&mut self) {
        self.stdout.take();
        if let Some(child) = self.child.take() {
            reap_in_background(child, self.track_id);
        }
        if let Some((token, _handle)) = self.waiting.take() {
            token.cancel();
        }
    }

    fn abort(&mut self, why: &str) -> Poll<Option<std::io::Result<Bytes>>> {
        tracing::warn!(track_id = self.track_id, "{why}");
        self.cleanup();
        Poll::Ready(Some(Err(std::io::Error::other(why.to_owned()))))
    }
}

impl Drop for FfmpegBody {
    /// クライアントの切断（本文の drop）
    fn drop(&mut self) {
        self.cleanup();
    }
}

impl Stream for FfmpegBody {
    type Item = std::io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if this.child.is_none() && this.waiting.is_none() {
            return Poll::Ready(None);
        }
        if this.deadline.as_mut().poll(cx).is_ready() {
            return this.abort("変換の上限時間を超えたので打ち切る");
        }
        if let Some(stdout) = this.stdout.as_mut() {
            match Pin::new(stdout).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(item)) => return Poll::Ready(Some(item)),
                Poll::Ready(None) => {
                    // EOF → 終了を待って終了コードを見る（待ちは detach 可能なタスクで）
                    this.stdout.take();
                    if let Some(child) = this.child.take() {
                        let token = CancellationToken::new();
                        let handle = tokio::spawn(child.wait(token.clone()));
                        this.waiting = Some((token, handle));
                    }
                }
            }
        }
        if let Some((_, handle)) = this.waiting.as_mut() {
            return match Pin::new(handle).poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(Ok(status))) if status.success() => {
                    this.waiting.take();
                    Poll::Ready(None)
                }
                Poll::Ready(Ok(Ok(status))) => {
                    this.waiting.take();
                    let why = format!("ffmpeg が異常終了した: {status}");
                    tracing::error!(track_id = this.track_id, "{why}");
                    Poll::Ready(Some(Err(std::io::Error::other(why))))
                }
                Poll::Ready(Ok(Err(e))) => {
                    this.waiting.take();
                    let why = format!("ffmpeg の終了を待てない: {e}");
                    tracing::error!(track_id = this.track_id, "{why}");
                    Poll::Ready(Some(Err(std::io::Error::other(why))))
                }
                Poll::Ready(Err(e)) => {
                    this.waiting.take();
                    let why = format!("ffmpeg の終了待ちタスクが異常終了: {e}");
                    tracing::error!(track_id = this.track_id, "{why}");
                    Poll::Ready(Some(Err(std::io::Error::other(why))))
                }
            };
        }
        Poll::Ready(None)
    }
}

fn transcode_opus(
    state: &AppState,
    track_id: i64,
    source: File,
    start: Option<f64>,
    duration_ms: Option<i64>,
    method: &Method,
) -> Result<Response, ApiError> {
    let mut cmd = Command::new(&state.config.bin.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error"]);
    if let Some(s) = start.filter(|s| s.is_finite() && *s > 0.0) {
        cmd.arg("-ss").arg(format!("{s:.3}"));
    }
    cmd.args([
        "-i",
        "/dev/stdin",
        "-map",
        "0:a:0",
        "-vn",
        "-map_metadata",
        "-1",
        "-c:a",
        "libopus",
        "-b:a",
        TRANSCODE_BITRATE,
        "-vbr",
        "on",
        "-f",
        "ogg",
        "pipe:1",
    ])
    .stdin(Stdio::from(source))
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = match ChildGroup::spawn(cmd) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(track_id, error = %e, "ffmpeg を起動できない");
            return Ok(error_response_with_message(
                StatusCode::SERVICE_UNAVAILABLE,
                "transcode_unavailable",
                format!("ffmpeg を起動できない: {e}"),
            ));
        }
    };
    let stdout = child
        .child_mut()
        .stdout
        .take()
        .ok_or_else(|| ApiError::Internal("ffmpeg の stdout を取れない".into()))?;
    if let Some(mut stderr) = child.child_mut().stderr.take() {
        tokio::spawn(async move {
            let mut buf = String::new();
            if stderr.read_to_string(&mut buf).await.is_ok() && !buf.trim().is_empty() {
                tracing::warn!(track_id, stderr = %buf.trim(), "ffmpeg の警告 / エラー");
            }
        });
    }
    let limit = duration_ms
        .map(|ms| Duration::from_millis(ms.max(0) as u64) + state.transcode_grace)
        .unwrap_or(TRANSCODE_DEFAULT_LIMIT);
    let body = if *method == Method::HEAD {
        // 起動できることだけ確かめて片付ける（kill + reap）
        drop(stdout);
        reap_in_background(child, track_id);
        Body::empty()
    } else {
        Body::from_stream(FfmpegBody {
            child: Some(child),
            stdout: Some(ReaderStream::new(stdout)),
            deadline: Box::pin(tokio::time::sleep(limit)),
            waiting: None,
            track_id,
        })
    };
    let mut res = (StatusCode::OK, body).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/ogg"));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(res)
}

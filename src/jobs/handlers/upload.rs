//! `upload` ジョブ（ブラウザからのアップロード。設計 §5.3 / §5.5 / §5.7）。並列 1。
//! payload は `{ "upload_id", "op": "unpack", "origin", "retry" }` か `{ "upload_id", "op": "publish" }`。
//!
//! - unpack: zip の origin を新しい世代へ展開する（`import::upload::unpack`）。成功した zip の再実行は
//!   何もしない。zip 全体の失敗はマニフェストに記録済みなので、ジョブは結果を残して終える
//! - publish: 凍結した計画で件を Inbox へ置く（`import::upload::publish`）。全件が公開済みになったら
//!   Inbox の走査を確保する（[`ensure_inbox_scan`]）。確保できたときだけ `inbox_enqueued_at` と
//!   `discarded_at` を入れる。走っている inbox ジョブと重なったら数秒後に自分をやり直す
//! - マニフェストの鍵は std の Mutex なので、鍵を取る処理はすべて `spawn_blocking` の中で行う
//! - どちらも冪等。再起動で `queued` に戻って最初から走ってよい

use std::sync::Arc;

use rusqlite::Connection;

use crate::config::UploadConfig;
use crate::db::jobs as dbjobs;
use crate::db::now_epoch;
use crate::fsroot::RootDir;
use crate::import::upload::{
    publish, unpack, with_manifest, PublishReport, PublishStep, UnpackOutcome, UploadError,
    UploadId, UploadLocks,
};
use crate::jobs::handlers::inbox::new_inbox_job;
use crate::jobs::{
    BoxFuture, EnqueueResult, Handler, HandlerResult, JobContext, JobError, JobState, JobType,
    NewJob, Outcome,
};

const DEDUP_PREFIX: &str = "upload:";
/// 走っている inbox ジョブと重なったときに、公開ジョブをやり直すまでの秒数
const INBOX_BUSY_RETRY_SECS: i64 = 5;

/// アップロード `id` の dedup キーの前置き（`upload:<id>:`）
fn key_prefix(id: &UploadId) -> String {
    format!("{DEDUP_PREFIX}{}:", id.as_str())
}

/// zip の origin の展開ジョブ。`retry` はやり直せる失敗の後に「展開し直す」ときだけ true
pub fn new_unpack_job(id: &UploadId, origin: &str, retry: bool) -> NewJob {
    NewJob::new(
        JobType::Upload,
        serde_json::json!({
            "upload_id": id.as_str(),
            "op": "unpack",
            "origin": origin,
            "retry": retry,
        }),
    )
    .dedup_key(format!("{}unpack:{origin}", key_prefix(id)))
}

/// 凍結した計画の公開ジョブ
pub fn new_publish_job(id: &UploadId) -> NewJob {
    NewJob::new(
        JobType::Upload,
        serde_json::json!({ "upload_id": id.as_str(), "op": "publish" }),
    )
    .dedup_key(format!("{}publish", key_prefix(id)))
}

/// [`ensure_inbox_scan`] の結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanSecured {
    /// 公開より後に走る走査がある（新しく投入した・待ちの inbox ジョブがある）
    Secured,
    /// 実行中の inbox ジョブと重なった（既に走査を終えて新しい件を見ないかもしれない）
    Busy,
}

/// 公開より後に走る Inbox の走査を確保する。新しく投入できた・待ちの inbox ジョブがあれば
/// [`ScanSecured::Secured`]、実行中なら [`ScanSecured::Busy`]
pub fn ensure_inbox_scan(conn: &Connection) -> crate::db::Result<ScanSecured> {
    Ok(secure_inbox_scan(conn)?.0)
}

/// [`ensure_inbox_scan`] と、新しく投入した inbox ジョブの id（ワーカーを起こすため）
fn secure_inbox_scan(conn: &Connection) -> crate::db::Result<(ScanSecured, Option<i64>)> {
    match dbjobs::enqueue(conn, &new_inbox_job(), now_epoch())? {
        EnqueueResult::Inserted(id) => Ok((ScanSecured::Secured, Some(id))),
        EnqueueResult::Duplicate(id) => {
            let state = dbjobs::get(conn, id)?.map(|j| j.state);
            Ok(match state {
                Some(JobState::Queued) => (ScanSecured::Secured, None),
                // 実行中（か、照会の間に終わった。dedup が外れたので次は投入できる）
                _ => (ScanSecured::Busy, None),
            })
        }
    }
}

/// アップロード `id` の upload ジョブ（展開・公開）が queued / running か。その間 DELETE と PATCH は
/// 409 にする（実行中の処理と捨てる・分け方の変更が競合しない）
pub fn upload_busy(conn: &Connection, id: &UploadId) -> crate::db::Result<bool> {
    let prefix = key_prefix(id);
    let len = i64::try_from(prefix.chars().count()).unwrap_or(i64::MAX);
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM jobs
         WHERE dedup_key IS NOT NULL AND substr(dedup_key, 1, ?1) = ?2
           AND state IN ('queued', 'running')",
        rusqlite::params![len, prefix],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// 公開の故障注入（テスト用。本番は常に false）
pub type PublishFault = Arc<dyn Fn(PublishStep) -> bool + Send + Sync>;

pub struct UploadHandler {
    inbox: Arc<RootDir>,
    locks: UploadLocks,
    cfg: Arc<UploadConfig>,
    fault: PublishFault,
}

impl UploadHandler {
    pub fn new(inbox: Arc<RootDir>, locks: UploadLocks, cfg: UploadConfig) -> Self {
        Self {
            inbox,
            locks,
            cfg: Arc::new(cfg),
            fault: Arc::new(|_| false),
        }
    }

    /// テスト用: 公開の各地点で `fault` を呼ぶ（true でその場で止まる。`import::upload::publish`）
    #[doc(hidden)]
    pub fn with_publish_fault(mut self, fault: PublishFault) -> Self {
        self.fault = fault;
        self
    }
}

/// payload の文字列
fn text<'a>(payload: &'a serde_json::Value, key: &str) -> Result<&'a str, JobError> {
    payload
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| JobError::Fatal(anyhow::anyhow!("payload に {key} が無い")))
}

/// アップロードの失敗をジョブの失敗へ。アップロードが無い・状態が合わないものは再試行しても変わらない
fn job_error(e: UploadError) -> JobError {
    match e {
        UploadError::NotFound | UploadError::BadPath(_) | UploadError::Conflict(_) => {
            JobError::Fatal(anyhow::anyhow!("{e}"))
        }
        e => JobError::Failed(anyhow::anyhow!("{e}")),
    }
}

/// `spawn_blocking` の結果をほどく
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, UploadError> + Send + 'static,
) -> Result<T, JobError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| JobError::Failed(anyhow::anyhow!("アップロードの処理が異常終了: {e}")))?
        .map_err(job_error)
}

/// 公開の結果の note: 1 行目に件数、続けて置いた dir を 1 行ずつ
fn publish_note(placed: &[String]) -> String {
    let mut note = format!("Inbox へ置いた: {} 件", placed.len());
    for dir in placed {
        note.push('\n');
        note.push_str(dir);
    }
    note
}

impl UploadHandler {
    async fn run_unpack(
        &self,
        ctx: &JobContext,
        id: UploadId,
        origin: String,
        retry: bool,
    ) -> HandlerResult {
        let (inbox, locks, cfg) = (
            Arc::clone(&self.inbox),
            self.locks.clone(),
            Arc::clone(&self.cfg),
        );
        let token = ctx.cancel_token();
        let cancel = token.clone();
        let out =
            blocking(move || unpack(&inbox, &locks, &id, &origin, retry, &cfg, &cancel)).await?;
        match out {
            UnpackOutcome::Done {
                files, rejected, ..
            } => Ok(Outcome::DoneWith(format!(
                "展開: {files} ファイル（取り込めなかった {rejected}）"
            ))),
            // 取り消しはやり直せる失敗として記録済み。ジョブは取り消しで終える
            UnpackOutcome::Failed { .. } if token.is_cancelled() => Err(JobError::Cancelled),
            UnpackOutcome::Failed { reason, .. } => {
                Ok(Outcome::DoneWith(format!("展開に失敗: {reason}")))
            }
            UnpackOutcome::Skipped => Ok(Outcome::Done),
        }
    }

    async fn run_publish(&self, ctx: &JobContext, id: UploadId) -> HandlerResult {
        let (inbox, locks, fault) = (
            Arc::clone(&self.inbox),
            self.locks.clone(),
            Arc::clone(&self.fault),
        );
        let token = ctx.cancel_token();
        let cancel = token.clone();
        let pid = id.clone();
        // 既に走査を確保した後の再実行（照合との行き違い）は、走査を重ねて投入しない
        let (secured_before, report) = blocking(move || {
            let secured = with_manifest(&inbox, &locks, &pid, |m| Ok(m.inbox_enqueued_at))?;
            let report = publish(&inbox, &locks, &pid, now_epoch(), &cancel, &*fault)?;
            Ok((secured.is_some(), report))
        })
        .await?;
        let PublishReport { placed, finished } = report;
        if secured_before {
            return Ok(Outcome::DoneWith(publish_note(&placed)));
        }
        if !finished {
            if token.is_cancelled() {
                return Err(JobError::Cancelled);
            }
            // 捨てたアップロード（公開要求後は捨てられないので、普通は起きない）
            return Ok(Outcome::DoneWith("破棄済みなので公開しない".into()));
        }
        let (secured, inserted) = ctx.db().write(|c| secure_inbox_scan(c)).await?;
        if let Some(job) = inserted {
            ctx.jobs().notify_enqueued(&[job]).await;
        }
        match secured {
            ScanSecured::Busy => {
                tracing::info!(
                    job_id = ctx.job.id,
                    upload = id.as_str(),
                    "走っている Inbox の走査と重なったので、終わってから走査を投入し直す"
                );
                Ok(Outcome::RequeueAfter(INBOX_BUSY_RETRY_SECS))
            }
            ScanSecured::Secured => {
                let (inbox, locks) = (Arc::clone(&self.inbox), self.locks.clone());
                blocking(move || {
                    with_manifest(&inbox, &locks, &id, |m| {
                        let now = now_epoch();
                        m.inbox_enqueued_at = Some(now);
                        m.discarded_at.get_or_insert(now);
                        Ok(())
                    })
                })
                .await?;
                Ok(Outcome::DoneWith(publish_note(&placed)))
            }
        }
    }

    async fn run_inner(&self, ctx: &JobContext) -> HandlerResult {
        let payload = &ctx.job.payload;
        let id = UploadId::parse(text(payload, "upload_id")?)
            .map_err(|e| JobError::Fatal(anyhow::anyhow!("upload_id が不正: {e}")))?;
        match text(payload, "op")? {
            "unpack" => {
                let origin = text(payload, "origin")?.to_string();
                let retry = payload
                    .get("retry")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                self.run_unpack(ctx, id, origin, retry).await
            }
            "publish" => self.run_publish(ctx, id).await,
            op => Err(JobError::Fatal(anyhow::anyhow!("不明な op: {op:?}"))),
        }
    }
}

impl Handler for UploadHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let this = Self {
            inbox: Arc::clone(&self.inbox),
            locks: self.locks.clone(),
            cfg: Arc::clone(&self.cfg),
            fault: Arc::clone(&self.fault),
        };
        Box::pin(async move { this.run_inner(&ctx).await })
    }
}

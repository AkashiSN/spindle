//! 照合（設計 §5.3）: マニフェストを正として、取りこぼした upload ジョブを投入し直す。
//!
//! 起動時と一覧・単体の取得時に走る。ジョブを投入する前に落ちても、DB が消えても、ここで拾う。
//! 投入は dedup キーでまとまるので、active なジョブがあれば重ならない。
//!
//! - 捨てたアップロード（`discarded_at`）は見ない
//! - 受け取り終えて未展開（`generation == 0`）で失敗していない zip（外した・壊れたものを除く）→ 展開
//! - 公開を要求済み（計画あり）で `inbox_enqueued_at` も `publish_error` も無い → 公開。全件公開済みでも同じ:
//!   公開ジョブは全件公開済みなら走査の確保へ進み、走っている inbox と重なれば自分で再試行するので、
//!   照合が次に走らなくても走査は確保される

use std::sync::Arc;

use crate::db::jobs as dbjobs;
use crate::db::{now_epoch, Db};
use crate::fsroot::RootDir;
use crate::jobs::handlers::upload::{new_publish_job, new_unpack_job};
use crate::jobs::{EnqueueResult, NewJob};

use super::manifest::{
    list_uploads, with_manifest, FileClass, Listed, Manifest, UploadError, UploadId, UploadLocks,
};

/// マニフェスト 1 つから、あるべきジョブを並べる
fn wanted(id: &UploadId, m: &Manifest) -> Vec<NewJob> {
    if m.discarded_at.is_some() {
        return Vec::new();
    }
    let mut out: Vec<NewJob> = m
        .origins
        .iter()
        .filter(|o| o.class == FileClass::Zip && o.complete && !o.removed && !o.damaged)
        .filter(|o| {
            o.zip
                .as_ref()
                .is_none_or(|z| z.generation == 0 && z.error.is_none())
        })
        .map(|o| new_unpack_job(id, &o.id, false))
        .collect();
    // やり直しても変わらない失敗で止まった公開は、人が押し直すまで投入しない
    if m.publish.is_some() && m.inbox_enqueued_at.is_none() && m.publish_error.is_none() {
        out.push(new_publish_job(id));
    }
    out
}

/// 各マニフェストを読み、取りこぼした upload ジョブを投入する。返り値は新しく投入した数
pub async fn reconcile(
    db: &Db,
    inbox: Arc<RootDir>,
    locks: &UploadLocks,
) -> Result<usize, UploadError> {
    Ok(reconcile_jobs(db, inbox, locks).await?.len())
}

/// [`reconcile`] と同じで、新しく投入したジョブの id を返す（API がワーカーを起こしイベントを流す）
pub async fn reconcile_jobs(
    db: &Db,
    inbox: Arc<RootDir>,
    locks: &UploadLocks,
) -> Result<Vec<i64>, UploadError> {
    let locks = locks.clone();
    // マニフェストの鍵は std の Mutex なので、読むのはブロッキングのスレッドで
    let jobs = tokio::task::spawn_blocking(move || -> Result<Vec<NewJob>, UploadError> {
        let mut jobs = Vec::new();
        for listed in list_uploads(&inbox)? {
            let Listed::Ok(m) = listed else {
                continue;
            };
            let Ok(id) = UploadId::parse(&m.id) else {
                continue;
            };
            // 一覧は鍵の外で読んだので、鍵の中で読み直してから決める
            match with_manifest(&inbox, &locks, &id, |m| Ok(wanted(&id, m))) {
                Ok(w) => jobs.extend(w),
                // 一覧の後に GC が消した・壊れた: 投入するものは無い
                Err(e) => tracing::debug!(upload = id.as_str(), error = %e, "照合で読めない"),
            }
        }
        Ok(jobs)
    })
    .await
    .map_err(|e| UploadError::Io(std::io::Error::other(e)))??;
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    let inserted = db
        .write(move |c| {
            let now = now_epoch();
            let mut ids = Vec::new();
            for job in &jobs {
                if let EnqueueResult::Inserted(id) = dbjobs::enqueue(c, job, now)? {
                    tracing::info!(job_id = id, payload = %job.payload, "照合で upload ジョブを投入し直した");
                    ids.push(id);
                }
            }
            Ok(ids)
        })
        .await?;
    Ok(inserted)
}

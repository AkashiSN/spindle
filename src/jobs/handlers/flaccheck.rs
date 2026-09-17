//! `flaccheck` ジョブ（SPEC §7.9 / §8、P1-5、D-57）。payload は `{"track_id", "audio_version"}`
//! （版付き。dedup key `flaccheck:<id>:<ver>`。ワーカーの stale ゲートで古い版は no-op、
//! `track_locks` で同じトラックの書き手と直列化）、並列度は CPU コア数。
//!
//! 読むだけでファイルは書かない。root で開いた FD を fstat して DB の行と照合し（同名で
//! 差し替えられた実体を検査しない）、STREAMINFO の MD5 を読み、`flac -t -` に同じ FD を stdin で
//! 渡す。結果は `tracks.flac_check*` に版付きで 1 UPDATE（版が進んでいれば書かない）

use std::fs::File;
use std::io::Seek;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::db::flaccheck::{self as dbfc, Status, Target};
use crate::db::now_epoch;
use crate::domain::relpath::RelPath;
use crate::fsroot::{self, RootDir};
use crate::jobs::process::{ExternalCommand, ProcessError};
use crate::jobs::{BoxFuture, Handler, HandlerResult, JobContext, JobError, NewJob, Outcome};
use crate::media::fingerprint::flac_streaminfo_md5;

pub use crate::db::flaccheck::dedup_key;

/// `flac -t` の上限。デコードは実時間より十分速いので、長さに比例させた上で下限を置く
const MIN_TIMEOUT: Duration = Duration::from_secs(60);
/// 1 秒の音声あたりに許す時間
const PER_SECOND: Duration = Duration::from_millis(200);
/// `flac_check_error` に残す stderr の長さ
const ERROR_MAX: usize = 500;

/// 開いた FD と STREAMINFO の MD5（`None` = 未設定）。行と一致しない・通常ファイルでなければ `None`
type Opened = Option<(File, Option<[u8; 16]>)>;

pub fn new_flaccheck_job(track_id: i64, audio_version: i64) -> NewJob {
    dbfc::new_job(track_id, audio_version)
}

pub struct FlaccheckHandler {
    root: Arc<RootDir>,
    flac: PathBuf,
}

impl FlaccheckHandler {
    pub fn new(root: Arc<RootDir>, flac: impl AsRef<Path>) -> Self {
        Self {
            root,
            flac: flac.as_ref().to_path_buf(),
        }
    }

    async fn run_inner(&self, ctx: &JobContext) -> HandlerResult {
        let job_id = ctx.job.id;
        let (track_id, audio_version) = match (
            ctx.job.payload.get("track_id").and_then(|v| v.as_i64()),
            ctx.job
                .payload
                .get("audio_version")
                .and_then(|v| v.as_i64()),
        ) {
            (Some(t), Some(v)) => (t, v),
            _ => {
                return Err(JobError::Failed(anyhow::anyhow!(
                    "payload に track_id / audio_version が無い: {}",
                    ctx.job.payload
                )))
            }
        };
        ctx.check_cancel().await?;
        let Some(target) = ctx
            .db()
            .read(move |c| dbfc::load_target(c, track_id))
            .await?
        else {
            tracing::info!(job_id, track_id, "トラックが無いので何もしない");
            return Ok(Outcome::Done);
        };
        if target.codec != "flac" || target.missing || target.audio_version != audio_version {
            tracing::info!(
                job_id,
                track_id,
                codec = target.codec,
                missing = target.missing,
                "検査対象でないので何もしない"
            );
            return Ok(Outcome::Done);
        }

        // 開いて DB の行と照合（FD の fstat。パスではない）。不一致は次のスキャン待ち（書かずに done）。
        // STREAMINFO は照合が通ってから読む（別実体に差し替わっていれば FLAC ですらないかもしれない）
        let rel = RelPath::parse(&target.rel_path)
            .map_err(|e| JobError::Failed(anyhow::anyhow!("rel_path が不正: {e}")))?;
        let root = Arc::clone(&self.root);
        let rel2 = rel.clone();
        let target2 = target.clone();
        let opened = tokio::task::spawn_blocking(move || -> Result<Opened, JobError> {
            let mut file = root.open_file(&rel2).map_err(|e| {
                JobError::Failed(anyhow::anyhow!("{}: 開けない: {e}", rel2.as_str()))
            })?;
            let st = fsroot::fstat(&file).map_err(|e| {
                JobError::Failed(anyhow::anyhow!("{}: stat できない: {e}", rel2.as_str()))
            })?;
            if st.kind != fsroot::FileKind::File || !target2.matches(&st) {
                return Ok(None);
            }
            let md5 = flac_streaminfo_md5(&mut file).map_err(|e| {
                JobError::Failed(anyhow::anyhow!(
                    "{}: STREAMINFO を読めない: {e}",
                    rel2.as_str()
                ))
            })?;
            file.rewind().map_err(|e| JobError::Failed(e.into()))?;
            Ok(Some((file, md5)))
        })
        .await
        .map_err(|e| JobError::Failed(e.into()))??;
        let Some((file, md5)) = opened else {
            tracing::info!(
                job_id,
                track_id,
                rel_path = target.rel_path,
                "ファイルが DB の行と一致しない（差し替えか更新）か通常ファイルでないので検査しない。再スキャン後に再投入される"
            );
            return Ok(Outcome::Done);
        };

        // flac -t -（同じ FD を stdin で）
        let timeout = timeout_for(&target);
        let output = ExternalCommand::new(&self.flac)
            .args(["-t", "-s", "-"])
            .stdin_file(file)
            .timeout(timeout)
            .run(&ctx.cancel_token())
            .await;
        let (status, error) = match output {
            Ok(_) => (
                if md5.is_some() {
                    Status::Ok
                } else {
                    Status::Md5Missing
                },
                None,
            ),
            // 終了コード ≠ 0 はデコードエラー（ジョブの失敗ではなく検査結果）
            Err(ProcessError::Failed { status, stderr, .. }) => {
                let tail = truncate_tail(&stderr, ERROR_MAX);
                tracing::warn!(job_id, track_id, rel_path = target.rel_path, %status, stderr = %tail, "flac -t が失敗");
                (
                    Status::DecodeError,
                    Some(if tail.is_empty() {
                        format!("flac -t が {status} で終了")
                    } else {
                        tail
                    }),
                )
            }
            Err(ProcessError::Cancelled) => return Err(JobError::Cancelled),
            Err(e) => return Err(JobError::Failed(e.into())),
        };
        let written = ctx
            .db()
            .write(move |c| {
                dbfc::record(
                    c,
                    track_id,
                    audio_version,
                    status,
                    error.as_deref(),
                    now_epoch(),
                )
            })
            .await?;
        if written {
            tracing::info!(
                job_id,
                track_id,
                rel_path = target.rel_path,
                status = status.as_str(),
                "FLAC を検査した"
            );
        } else {
            tracing::info!(job_id, track_id, "検査中に版が進んだので結果を捨てる");
        }
        Ok(Outcome::Done)
    }
}

fn timeout_for(t: &Target) -> Duration {
    // 長さは DB に無いことがあるので size から大まかに（1 MB ≈ 10 秒の FLAC）
    let secs = (t.size / 100_000).max(1) as u32;
    MIN_TIMEOUT.max(PER_SECOND * secs)
}

/// 末尾 `max` 文字（文字境界で切る）
fn truncate_tail(s: &str, max: usize) -> String {
    let s = s.trim();
    let n = s.chars().count();
    if n <= max {
        s.to_owned()
    } else {
        s.chars().skip(n - max).collect()
    }
}

impl Handler for FlaccheckHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let this = FlaccheckHandler {
            root: Arc::clone(&self.root),
            flac: self.flac.clone(),
        };
        Box::pin(async move { this.run_inner(&ctx).await })
    }
}

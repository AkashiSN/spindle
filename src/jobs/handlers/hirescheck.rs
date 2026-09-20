//! `hirescheck` ジョブ（SPEC §7.10 / §8、P3-5、D-71）。payload は `{"track_id", "audio_version"}`
//! （版付き。dedup key `hirescheck:<id>:<ver>`。ワーカーの stale ゲートで古い版は no-op、
//! `track_locks` で同じトラックの書き手と直列化）、並列度は CPU コア数の半分。
//!
//! 読むだけでファイルは書かない。root で開いた FD を fstat して DB の行と照合し（同名で
//! 差し替えられた実体を検査しない）、`media::decode` で PCM を [`HiresSink`] に流して計測し、
//! `[hires]` のしきい値で判定する。結果は `tracks.hires_*` に版付きで 1 UPDATE（版が進んでいれば
//! 書かない）。デコード失敗は検査結果 `decode_error`（ジョブの失敗ではない）

use std::sync::Arc;

use crate::db::hires::{self as dbh, Status, Target};
use crate::db::now_epoch;
use crate::domain::relpath::RelPath;
use crate::fsroot::{self, RootDir};
use crate::jobs::process::ProcessError;
use crate::jobs::{BoxFuture, Handler, HandlerResult, JobContext, JobError, NewJob, Outcome};
use crate::media::decode::{DecodeError, Decoder};
use crate::media::hires::{judge, HiresSink, Measurement, Thresholds, Verdict};

pub use crate::db::hires::dedup_key;

/// `hires_check_error` に残すメッセージの長さ
const ERROR_MAX: usize = 500;

pub fn new_hirescheck_job(track_id: i64, audio_version: i64) -> NewJob {
    dbh::new_job(track_id, audio_version)
}

pub struct HirescheckHandler {
    root: Arc<RootDir>,
    decoder: Decoder,
    thresholds: Thresholds,
}

impl HirescheckHandler {
    pub fn new(root: Arc<RootDir>, decoder: Decoder, thresholds: Thresholds) -> Self {
        Self {
            root,
            decoder,
            thresholds,
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
            .read(move |c| dbh::load_target(c, track_id))
            .await?
        else {
            tracing::info!(job_id, track_id, "トラックが無いので何もしない");
            return Ok(Outcome::Done);
        };
        if !target.eligible() || target.audio_version != audio_version {
            tracing::info!(
                job_id,
                track_id,
                lossless = target.lossless,
                sample_rate = target.sample_rate,
                bit_depth = target.bit_depth,
                missing = target.missing,
                "検査対象でないので何もしない"
            );
            return Ok(Outcome::Done);
        }

        // 開いて DB の行と照合（FD の fstat。パスではない）。不一致は次のスキャン待ち（書かずに done）
        let rel = RelPath::parse(&target.rel_path)
            .map_err(|e| JobError::Failed(anyhow::anyhow!("rel_path が不正: {e}")))?;
        let root = Arc::clone(&self.root);
        let rel2 = rel.clone();
        let target2 = target.clone();
        let opened =
            tokio::task::spawn_blocking(move || -> Result<Option<std::fs::File>, JobError> {
                let file = root.open_file(&rel2).map_err(|e| {
                    JobError::Failed(anyhow::anyhow!("{}: 開けない: {e}", rel2.as_str()))
                })?;
                let st = fsroot::fstat(&file).map_err(|e| {
                    JobError::Failed(anyhow::anyhow!("{}: stat できない: {e}", rel2.as_str()))
                })?;
                if st.kind != fsroot::FileKind::File || !target2.matches(&st) {
                    return Ok(None);
                }
                Ok(Some(file))
            })
            .await
            .map_err(|e| JobError::Failed(e.into()))??;
        let Some(file) = opened else {
            tracing::info!(
                job_id,
                track_id,
                rel_path = target.rel_path,
                "ファイルが DB の行と一致しない（差し替えか更新）か通常ファイルでないので検査しない。再スキャン後に再投入される"
            );
            return Ok(Outcome::Done);
        };
        // デコード後にもう一度照合するための FD（デコーダは自分で seek する）
        let probe = file.try_clone().map_err(|e| {
            JobError::Failed(anyhow::anyhow!(
                "{}: FD を複製できない: {e}",
                target.rel_path
            ))
        })?;

        let ext = rel.as_str().rsplit_once('.').map(|(_, e)| e);
        let bit_depth = target.bit_depth.and_then(|b| u32::try_from(b).ok());
        let sink = HiresSink::new(bit_depth);
        let outcome = self
            .decoder
            .decode(file, ext, sink, &ctx.cancel_token())
            .await;
        // 成功でも失敗でも、デコード中に実体が変わっていれば何も書かない（次のスキャン待ち）
        if !decoded_unchanged(&probe, &target) {
            tracing::info!(
                job_id,
                track_id,
                rel_path = target.rel_path,
                "デコード中にファイルが変わったので結果を捨てる。再スキャン後に再投入される"
            );
            return Ok(Outcome::Done);
        }
        let (status, error, measurement) = match outcome {
            Ok((info, sink)) => {
                let m = sink.finish();
                tracing::debug!(
                    job_id,
                    track_id,
                    channels = info.channels,
                    sample_rate = info.sample_rate,
                    cutoff_hz = m.cutoff_hz,
                    cliff_db = m.cliff_db,
                    effective_bits = m.effective_bits,
                    "トラックを解析した"
                );
                (status_of(judge(&m, &self.thresholds)), None, m)
            }
            Err(DecodeError::Cancelled) => return Err(JobError::Cancelled),
            // ファイルの内容に由来する失敗だけが検査結果（再試行しても同じ）。環境の失敗
            // （ffmpeg が無い・タイムアウト・I/O・受け手）はジョブの失敗にして再試行に回す。
            // 検査結果にしてしまうと同じ版は再投入されず、設定を直しても回復しない
            Err(e) if is_content_error(&e) => {
                let message = truncate_head(&format!("{e:#}"), ERROR_MAX);
                tracing::warn!(job_id, track_id, rel_path = target.rel_path, error = %message, "デコードに失敗");
                (Status::DecodeError, Some(message), Measurement::default())
            }
            Err(e) => {
                return Err(JobError::Failed(anyhow::anyhow!(
                    "{}: {e:#}",
                    target.rel_path
                )))
            }
        };
        let written = ctx
            .db()
            .write(move |c| {
                dbh::record(
                    c,
                    track_id,
                    audio_version,
                    status,
                    error.as_deref(),
                    measurement.cutoff_hz,
                    measurement.cliff_db,
                    measurement.effective_bits,
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
                cutoff_hz = measurement.cutoff_hz,
                cliff_db = measurement.cliff_db,
                effective_bits = measurement.effective_bits,
                "偽ハイレゾ検出を記録した"
            );
        } else {
            tracing::info!(job_id, track_id, "検査中に版が進んだので結果を捨てる");
        }
        Ok(Outcome::Done)
    }
}

/// デコード後の FD がまだ行の実体か（stat できなければ「変わった」扱い）
fn decoded_unchanged(probe: &std::fs::File, target: &Target) -> bool {
    fsroot::fstat(probe).is_ok_and(|st| target.matches(&st))
}

fn status_of(v: Verdict) -> Status {
    match v {
        Verdict::Ok => Status::Ok,
        Verdict::Upsampled => Status::Upsampled,
        Verdict::Padded => Status::Padded,
        Verdict::Both => Status::Both,
        Verdict::Inconclusive => Status::Inconclusive,
    }
}

/// ファイルの内容に由来するデコード失敗か（音声が無い・判別できない・壊れている・対応外・
/// ffmpeg が非ゼロで終わった）。起動できない・タイムアウト・I/O・受け手の失敗は環境の問題
pub fn is_content_error(e: &DecodeError) -> bool {
    match e {
        DecodeError::NoAudioTrack
        | DecodeError::Probe(_)
        | DecodeError::Decode(_)
        | DecodeError::Unsupported(_) => true,
        DecodeError::Process(p) => matches!(p, ProcessError::Failed { .. }),
        DecodeError::Sink(_) | DecodeError::Cancelled | DecodeError::Io(_) => false,
    }
}

/// 先頭 `max` 文字（文字境界で切る）。anyhow の外側の文脈を落とさない
pub fn truncate_head(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max).collect()
    }
}

impl Handler for HirescheckHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let this = HirescheckHandler {
            root: Arc::clone(&self.root),
            decoder: self.decoder.clone(),
            thresholds: self.thresholds,
        };
        Box::pin(async move { this.run_inner(&ctx).await })
    }
}

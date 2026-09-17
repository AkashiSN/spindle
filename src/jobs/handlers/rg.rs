//! `rg` ジョブ（SPEC §6「ReplayGain の内部表現」/ §8、D-22 / D-47、P1-1）。
//!
//! payload は `{"album_id": N}`（dedup `rg:album:<N>`）か、album を持たないトラック向けの
//! `{"track_id": N}`（dedup `rg:track:<N>`）。並列度は CPU コア数。
//!
//! - album の active な構成トラックを `id` 昇順に全件ロックし、取れなければ再キュー。
//!   ロック後に構成を読み直す（ロック前に終わった tagwrite 等で stat が動いていてもよい）
//! - 各トラックを root から開き、**開いた FD の fstat が DB の行と一致する**ことを確かめてから
//!   デコードする（パスは識別子ではない。同名で差し替えられていれば別トラックの音声を
//!   この行の値として保存してしまう）。デコード後にも同じ FD を fstat し直し、解析中の
//!   in-place 更新を検出する。不一致は失敗（再スキャン後の再試行で通る）
//! - 積分ラウドネスと true peak を測る（[`Decoder`]、[`LoudnessMeter`]）。1 本でもデコードできな
//!   ければジョブ全体を失敗にし、何も書かない（album の集計が揃わないまま一部だけ書くと、次回の
//!   再解析まで album gain が嘘になる）
//! - album gain は **デコード結果が 2ch** のトラックだけをまとめてゲートし直した値。2ch 以外は
//!   track の値だけ持ち、album の値は NULL（D-22）。DB の `channels` 列ではなく実際に
//!   デコードした結果で判定する（ファイルが正）
//! - 書き込みは 1 トランザクション。**その中で構成を読み直し**、解析した行の集合（id と stat）と
//!   違えば（scanner が missing にした・別 album へ移した・新しいトラックが加わった・同じ行を
//!   別実体へ追随させた）何も書かずに失敗する。`rg_scanned_at` だけ更新し、`audio_version` /
//!   `tag_version` は動かさない（`rg_written_at` は値が変わった行だけ NULL。D-48）
//! - 無音（絶対ゲート以下）は gain 0 dB（`domain::replaygain`）

use std::fs::File;
use std::sync::Arc;

use crate::db::replaygain::{self as dbrg, Member, Values};
use crate::db::{now_epoch, DbError};
use crate::domain::relpath::RelPath;
use crate::domain::replaygain::{album_loudness, LoudnessMeter, TrackLoudness};
use crate::fsroot::{self, RootDir};
use crate::jobs::{
    BoxFuture, Handler, HandlerResult, JobContext, JobError, JobType, NewJob, Outcome,
};
use crate::media::decode::{DecodeError, Decoder, PcmInfo, PcmSink};

pub fn new_album_job(album_id: i64) -> NewJob {
    NewJob::new(JobType::Rg, serde_json::json!({ "album_id": album_id }))
        .dedup_key(format!("rg:album:{album_id}"))
}

pub fn new_track_job(track_id: i64) -> NewJob {
    NewJob::new(JobType::Rg, serde_json::json!({ "track_id": track_id }))
        .dedup_key(format!("rg:track:{track_id}"))
}

/// album 集計に入れるチャンネル数（SPEC §6: 2ch 以外は除外）
const ALBUM_CHANNELS: u32 = 2;

pub struct RgHandler {
    root: Arc<RootDir>,
    decoder: Decoder,
    reference_lufs: f64,
}

impl RgHandler {
    pub fn new(root: Arc<RootDir>, decoder: Decoder, reference_lufs: f64) -> Self {
        Self {
            root,
            decoder,
            reference_lufs,
        }
    }
}

/// デコード結果を直接ラウドネス計に流す受け手
struct MeterSink {
    meter: Option<LoudnessMeter>,
}

impl PcmSink for MeterSink {
    fn start(&mut self, info: &PcmInfo) -> anyhow::Result<()> {
        self.meter = Some(LoudnessMeter::new(info.channels, info.sample_rate)?);
        Ok(())
    }

    fn push(&mut self, interleaved: &[f32]) -> anyhow::Result<()> {
        match self.meter.as_mut() {
            Some(m) => Ok(m.push(interleaved)?),
            None => Err(anyhow::anyhow!("start の前に push された")),
        }
    }
}

struct Analyzed {
    member: Member,
    info: PcmInfo,
    meter: LoudnessMeter,
    track: TrackLoudness,
}

#[derive(Clone, Copy)]
enum Scope {
    Album(i64),
    Track(i64),
}

impl Scope {
    fn members(self, c: &rusqlite::Connection) -> crate::db::Result<Vec<Member>> {
        match self {
            Scope::Album(id) => dbrg::album_members(c, id),
            Scope::Track(id) => dbrg::track_member(c, id),
        }
    }
}

/// 開いた FD が `member` の行の実体で、DB に取り込んだ後に書かれていないことを確かめる
fn verify_unchanged(file: &File, member: &Member, when: &str) -> Result<(), JobError> {
    let st = fsroot::fstat(file).map_err(|e| {
        JobError::Failed(anyhow::anyhow!("{}: stat できない: {e}", member.rel_path))
    })?;
    if member.matches(&st) {
        Ok(())
    } else {
        Err(JobError::Failed(anyhow::anyhow!(
            "{}: {when}にファイルが DB の行と一致しない（差し替えか更新。再スキャン後に再試行）",
            member.rel_path
        )))
    }
}

/// 書き込み時の構成が解析した集合と違う。id 列だけでなく行の stat（dev / inode / size /
/// mtime / ctime）と rel_path まで比較する。解析中に外部が同じパスを別実体へ差し替え、scanner が
/// 同じ行を新実体へ追随させた場合、旧 FD から測った値をその行に書いてはいけない
fn membership_changed(analyzed: &[Member], current: &[Member]) -> bool {
    analyzed != current
}

impl Handler for RgHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let root = Arc::clone(&self.root);
        let decoder = self.decoder.clone();
        let reference = self.reference_lufs;
        Box::pin(async move {
            let job_id = ctx.job.id;
            let int_of = |key: &str| ctx.job.payload.get(key).and_then(|v| v.as_i64());
            let scope = match (int_of("album_id"), int_of("track_id")) {
                (Some(a), _) => Scope::Album(a),
                (None, Some(t)) => Scope::Track(t),
                (None, None) => {
                    return Err(JobError::Failed(anyhow::anyhow!(
                        "payload に整数の album_id / track_id が無い: {}",
                        ctx.job.payload
                    )))
                }
            };
            let members = ctx.db().read(move |c| scope.members(c)).await?;
            if members.is_empty() {
                tracing::info!(job_id, "解析対象の active なトラックが無い");
                return Ok(Outcome::Done);
            }
            let ids: Vec<i64> = members.iter().map(|m| m.id).collect();
            if !ctx.lock_tracks(&ids).await? {
                return Ok(Outcome::Requeue);
            }
            // ロックを取った後の構成が正（ロック前に終わった spindle 自身の編集で stat が動いて
            // いてもよい）。ロック中に構成が変わるのは scanner だけで、書き込み時に検出する
            let members = ctx.db().read(move |c| scope.members(c)).await?;
            if members.iter().map(|m| m.id).ne(ids.iter().copied()) {
                return Err(JobError::Failed(anyhow::anyhow!(
                    "ロックの前後で構成が変わった（再試行）"
                )));
            }
            ctx.check_cancel().await?;

            let total = members.len() as i64;
            let mut analyzed: Vec<Analyzed> = Vec::with_capacity(members.len());
            for (i, member) in members.into_iter().enumerate() {
                ctx.progress(i as i64, total).await?;
                let rel = RelPath::parse(&member.rel_path).map_err(|e| {
                    JobError::Failed(anyhow::anyhow!("{}: 不正なパス: {e}", member.rel_path))
                })?;
                let file = root.open_file(&rel).map_err(|e| {
                    JobError::Failed(anyhow::anyhow!("{}: 開けない: {e}", member.rel_path))
                })?;
                verify_unchanged(&file, &member, "解析前")?;
                // fstat 用に FD を残す（dup はオフセットを共有するがデコーダは自分で seek する）
                let probe = file.try_clone().map_err(|e| {
                    JobError::Failed(anyhow::anyhow!(
                        "{}: FD を複製できない: {e}",
                        member.rel_path
                    ))
                })?;
                let ext = rel.as_str().rsplit_once('.').map(|(_, e)| e);
                let sink = MeterSink { meter: None };
                let (info, sink) = match decoder.decode(file, ext, sink, &ctx.cancel_token()).await
                {
                    Ok(r) => r,
                    Err(DecodeError::Cancelled) => return Err(JobError::Cancelled),
                    Err(e) => {
                        return Err(JobError::Failed(anyhow::anyhow!(
                            "{}: {e:#}",
                            member.rel_path
                        )))
                    }
                };
                verify_unchanged(&probe, &member, "解析中")?;
                let Some(meter) = sink.meter else {
                    return Err(JobError::Failed(anyhow::anyhow!(
                        "{}: デコーダが start を呼ばなかった",
                        member.rel_path
                    )));
                };
                let track = meter.loudness();
                tracing::debug!(
                    job_id,
                    track_id = member.id,
                    lufs = track.lufs,
                    peak = track.peak,
                    channels = info.channels,
                    "トラックを解析した"
                );
                analyzed.push(Analyzed {
                    member,
                    info,
                    meter,
                    track,
                });
            }

            // album の集計（2ch だけ）。track 単独ジョブは album の値を持たない
            let album = match scope {
                Scope::Track(_) => None,
                Scope::Album(_) => {
                    let members: Vec<&LoudnessMeter> = analyzed
                        .iter()
                        .filter(|a| a.info.channels == ALBUM_CHANNELS)
                        .map(|a| &a.meter)
                        .collect();
                    if members.is_empty() {
                        None
                    } else {
                        let lufs =
                            album_loudness(&members).map_err(|e| JobError::Failed(e.into()))?;
                        let peak = analyzed
                            .iter()
                            .filter(|a| a.info.channels == ALBUM_CHANNELS)
                            .map(|a| a.track.peak)
                            .fold(0.0f64, f64::max);
                        Some((TrackLoudness { lufs, peak }.gain(reference), peak))
                    }
                }
            };
            let results: Vec<(i64, Values)> = analyzed
                .iter()
                .map(|a| {
                    let in_album = album.filter(|_| a.info.channels == ALBUM_CHANNELS);
                    (
                        a.member.id,
                        Values {
                            track_gain: a.track.gain(reference),
                            track_peak: a.track.peak,
                            album_gain: in_album.map(|(g, _)| g),
                            album_peak: in_album.map(|(_, p)| p),
                        },
                    )
                })
                .collect();
            ctx.check_cancel().await?;
            // 書き込みは all-or-nothing。トランザクション内で構成を読み直し、解析した集合と
            // 違えば（missing 化・album の移動・追加）何も書かない
            let analyzed_members: Vec<Member> = analyzed.into_iter().map(|a| a.member).collect();
            let written = ctx
                .db()
                .write(move |c| {
                    let tx = c.unchecked_transaction()?;
                    let current = scope.members(&tx)?;
                    if membership_changed(&analyzed_members, &current) {
                        tx.rollback()?;
                        return Ok(None);
                    }
                    let now = now_epoch();
                    let n = dbrg::store(&tx, &results, now)?;
                    if n != results.len() {
                        tx.rollback()?;
                        return Err(DbError::Internal(format!(
                            "rg の更新件数が合わない: {n} != {}",
                            results.len()
                        )));
                    }
                    // Derived の追随（D-51）。解析値は版に乗らないので、ここで retag を投入する
                    let mut derived_jobs = Vec::new();
                    for (track_id, _) in &results {
                        if let Some(id) = crate::db::derived::enqueue_if_stale(&tx, *track_id, now)?
                        {
                            derived_jobs.push(id);
                        }
                    }
                    tx.commit()?;
                    Ok(Some((n, derived_jobs)))
                })
                .await?;
            let Some((written, derived_jobs)) = written else {
                return Err(JobError::Failed(anyhow::anyhow!(
                    "解析中に構成が変わったので書かなかった（再試行）"
                )));
            };
            ctx.jobs().notify_enqueued(&derived_jobs).await;
            ctx.progress(total, total).await?;
            tracing::info!(job_id, written, "ReplayGain を解析した");
            Ok(Outcome::Done)
        })
    }
}

//! `verify` ジョブ（SPEC §7.3「遡及照合」、§8、D-13、D-63、P2-9）。payload は `{"album_id"}`
//! （dedup key `verify:<album_id>`、並列度 2）。CUETools の "verify from files" 相当。
//!
//! アルバムの active なトラックをディスク（`disc_no`）ごとに分け、ディスクごとに:
//! 1. 適格判定。全トラックが 44.1 kHz / 16 bit / 2ch の FLAC でなければ `unverifiable`
//!    （TOC が存在しない音源。品質の劣後ではない）。トラック番号が 1 から連続していなければ
//!    TOC を作れないので何もしない（不完全なディスク）
//! 2. root で開いた FD を fstat して DB の行と照合し（同名で差し替えられた実体は読まない）、
//!    STREAMINFO のサンプル数から TOC を再構成する。588 の倍数でなければ `unverifiable`
//! 3. デコードして CRC 表（[`CrcSampler`]）を作る。デコードしたサンプル数が STREAMINFO と
//!    違えば失敗（壊れたファイル。flaccheck の領分）
//! 4. CTDB と AccurateRip に照会し、オフセットを探して照合する（[`crate::cd::verify`]）
//! 5. `album_verifications`（手法ごと・ディスクごと。履歴として積む）と `track_verifications`
//!    に記録し、`tracks.verification` を更新する。CTDB が一致 → `verified_ctdb`、
//!    AccurateRip だけ一致 → `verified_ar`、候補はあるが不一致 → `mismatch`、候補なし → 据え置き
//!
//! verify.log は `data/verify/<album_id>.log`（D-63。Library には置かない）に、アルバムの
//! 全ディスクぶんを毎回書き直す。照会に失敗したらジョブを失敗させて再試行し、何も記録しない
//! （不一致を「照会できなかった」で汚さない）

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::cd::accuraterip::{AccurateRipClient, ArDiscEntry};
use crate::cd::crctable::{CrcSampler, CrcTable};
use crate::cd::ctdb::{CtdbClient, CtdbEntry};
use crate::cd::toc::{Toc, TocError};
use crate::cd::verify::{match_accuraterip, match_ctdb, MethodResult, Outcome};
use crate::cd::SECTOR_SAMPLES;
use crate::db::now_epoch;
use crate::db::verify::{
    self as dbv, AlbumInfo, DiscRecord, DiscResult, Method, MethodRecord, RecordOutcome,
    TrackRecord, TrackState, VerifySource, VerifyTrack,
};
use crate::domain::relpath::RelPath;
use crate::fsroot::{self, RootDir};
use crate::jobs::handlers::backup::format_utc;
use crate::jobs::{
    BoxFuture, Handler, HandlerResult, JobContext, JobError, NewJob, Outcome as JobOutcome,
};
use crate::media::fingerprint::{decode_s16, flac_streaminfo};

pub use crate::db::verify::dedup_key;

/// verify.log の置き場（`data/` 直下）
pub const LOG_DIR_NAME: &str = "verify";

pub fn new_verify_job(album_id: i64) -> NewJob {
    dbv::new_job(album_id)
}

pub struct VerifyHandler {
    root: Arc<RootDir>,
    log_dir: PathBuf,
    ar: AccurateRipClient,
    ctdb: CtdbClient,
}

/// 1 ディスクの判定結果（DB とログに書く材料）
struct DiscReport {
    disc_no: i64,
    tracks: Vec<VerifyTrack>,
    kind: DiscKind,
}

enum DiscKind {
    /// TOC が作れない（トラック番号の抜け）か、ファイルが行と一致しない。記録しない
    Skipped(String),
    /// TOC が存在しない音源
    Unverifiable(String),
    Verified {
        toc: Toc,
        ctdb: MethodResult,
        ar: MethodResult,
        ar_entries: usize,
        ctdb_entries: usize,
    },
}

/// 開いて照合した 1 ファイル
struct Opened {
    file: File,
    total_samples: u64,
}

impl VerifyHandler {
    pub fn new(
        root: Arc<RootDir>,
        data_dir: impl AsRef<Path>,
        ar: AccurateRipClient,
        ctdb: CtdbClient,
    ) -> Self {
        Self {
            root,
            log_dir: data_dir.as_ref().join(LOG_DIR_NAME),
            ar,
            ctdb,
        }
    }

    async fn run_inner(&self, ctx: &JobContext) -> HandlerResult {
        let job_id = ctx.job.id;
        let Some(album_id) = ctx.job.payload.get("album_id").and_then(|v| v.as_i64()) else {
            return Err(JobError::Failed(anyhow::anyhow!(
                "payload に album_id が無い: {}",
                ctx.job.payload
            )));
        };
        ctx.check_cancel().await?;
        let (album, tracks, already) = ctx
            .db()
            .read(move |c| {
                let album = dbv::load_album(c, album_id)?;
                let tracks = dbv::load_album_tracks(c, album_id)?;
                let already = dbv::has_records_for_job(c, job_id)?;
                Ok((album, tracks, already))
            })
            .await?;
        if already {
            // commit の後・done の前に落ちて再実行された。DB もログ（rename は commit の前）も
            // 確定済みなので、読み直して別の結果を上書きしない
            tracing::info!(job_id, album_id, "前回の実行が記録済みなので何もしない");
            return Ok(JobOutcome::Done);
        }
        let Some(album) = album else {
            tracing::info!(job_id, album_id, "アルバムが無いので何もしない");
            return Ok(JobOutcome::Done);
        };
        if tracks.is_empty() {
            tracing::info!(job_id, album_id, "active なトラックが無いので何もしない");
            return Ok(JobOutcome::Done);
        }

        // ディスクごとに分ける（disc_no が無ければ 1）
        let mut discs: Vec<(i64, Vec<VerifyTrack>)> = Vec::new();
        for t in tracks {
            let no = t.disc_no.unwrap_or(1);
            match discs.last_mut() {
                Some((n, v)) if *n == no => v.push(t),
                _ => discs.push((no, vec![t])),
            }
        }
        let total_tracks: i64 = discs.iter().map(|(_, v)| v.len() as i64).sum();
        let mut done_tracks = 0i64;
        let mut reports = Vec::with_capacity(discs.len());
        for (disc_no, tracks) in discs {
            ctx.check_cancel().await?;
            let n = tracks.len() as i64;
            let kind = self.verify_disc(ctx, album_id, disc_no, &tracks).await?;
            reports.push(DiscReport {
                disc_no,
                tracks,
                kind,
            });
            done_tracks += n;
            ctx.progress(done_tracks, total_tracks).await?;
        }

        // 記録の直前にもう一度、ファイルが照合中に変わっていないか（差し替え・その場の更新）を見る。
        // 変わったディスクは記録しない（再スキャン後に再投入される）
        ctx.check_cancel().await?;
        for r in &mut reports {
            if matches!(r.kind, DiscKind::Skipped(_)) {
                continue;
            }
            let root = Arc::clone(&self.root);
            let tracks = r.tracks.clone();
            let changed =
                tokio::task::spawn_blocking(move || -> Result<Option<String>, JobError> {
                    for t in &tracks {
                        if !still_matches(&root, t)? {
                            return Ok(Some(t.rel_path.clone()));
                        }
                    }
                    Ok(None)
                })
                .await
                .map_err(|e| JobError::Failed(e.into()))??;
            if let Some(rel) = changed {
                tracing::info!(
                    job_id,
                    album_id,
                    disc_no = r.disc_no,
                    rel_path = rel,
                    "照合中にファイルが変わったので記録しない"
                );
                r.kind = DiscKind::Skipped(format!(
                    "{rel}: 照合中にファイルが変わった。再スキャン後に再投入"
                ));
            }
        }

        let summary: Vec<String> = reports
            .iter()
            .map(|r| format!("disc {}: {}", r.disc_no, r.kind.summary()))
            .collect();
        let discs: Vec<DiscRecord> = reports.iter().filter_map(disc_record).collect();
        if discs.is_empty() {
            tracing::info!(job_id, album_id, ?summary, "記録するディスクが無い");
            return Ok(JobOutcome::Done);
        }
        let expected: Vec<(i64, i64)> = reports
            .iter()
            .filter(|r| !matches!(r.kind, DiscKind::Skipped(_)))
            .flat_map(|r| r.tracks.iter().map(|t| (t.id, t.audio_version)))
            .collect();

        // ログは tmp に書き、DB のトランザクションの中で本来の名前に rename してから commit する。
        // rename に失敗すれば巻き戻す（DB だけ残さない）。rename の後 commit の前に落ちても、
        // ログは再実行が書き直す。commit の後に落ちれば再実行は job_id で既存行を見つけて
        // 何もしない（履歴もログも初回のまま。CLAUDE.md「ジョブは冪等」）
        let now = now_epoch();
        let log_path = self.log_dir.join(format!("{album_id}.log"));
        let tmp_path = self.log_dir.join(format!(".{album_id}.log.tmp"));
        write_log(&tmp_path, &render_log(&album, &reports, now))?;
        let tmp_guard = ctx.temp_file(&tmp_path);
        let log_path_str = log_path.to_string_lossy().into_owned();
        let (tmp_for_tx, log_for_tx) = (tmp_path.clone(), log_path.clone());
        let outcome = ctx
            .db()
            .transaction(move |c| {
                let out = dbv::record_album(
                    c,
                    album_id,
                    job_id,
                    VerifySource::Retro,
                    &expected,
                    &discs,
                    Some(&log_path_str),
                    now,
                )?;
                if matches!(out, RecordOutcome::Recorded(_)) {
                    std::fs::rename(&tmp_for_tx, &log_for_tx).map_err(|e| {
                        crate::db::DbError::Internal(format!(
                            "verify.log を確定できない（{}）: {e}",
                            log_for_tx.display()
                        ))
                    })?;
                }
                Ok(out)
            })
            .await?;
        match outcome {
            RecordOutcome::Changed { track_id } => {
                tracing::info!(
                    job_id,
                    album_id,
                    track_id,
                    "照合中に音声が変わったので記録しない。再スキャン後に再投入"
                );
                drop(tmp_guard);
            }
            RecordOutcome::AlreadyRecorded => {
                // 冒頭の判定の後に別の実行が記録した。今回の結果は捨て、既存のログも触らない
                tracing::info!(
                    job_id,
                    album_id,
                    "別の実行が記録済みなので今回の結果は捨てる"
                );
                drop(tmp_guard);
            }
            RecordOutcome::Recorded(ids) => {
                let _ = tmp_guard.keep();
                tracing::info!(job_id, album_id, ?summary, verifications = ?ids, "遡及照合した");
            }
        }
        Ok(JobOutcome::Done)
    }

    /// 1 ディスクを判定する。ネットワーク・デコードの失敗は Err（ジョブの失敗 → 再試行）
    async fn verify_disc(
        &self,
        ctx: &JobContext,
        album_id: i64,
        disc_no: i64,
        tracks: &[VerifyTrack],
    ) -> Result<DiscKind, JobError> {
        let job_id = ctx.job.id;
        // 形式（TOC が存在する音源か）
        if let Some(t) = tracks.iter().find(|t| {
            t.codec != "flac"
                || t.sample_rate != Some(44_100)
                || t.bit_depth != Some(16)
                || t.channels != Some(2)
        }) {
            return Ok(DiscKind::Unverifiable(format!(
                "{}: {} {}Hz/{}bit/{}ch は CD の形式でない",
                t.rel_path,
                t.codec,
                t.sample_rate.unwrap_or(0),
                t.bit_depth.unwrap_or(0),
                t.channels.unwrap_or(0)
            )));
        }
        // 完全性（トラック番号が 1 から連続）
        for (i, t) in tracks.iter().enumerate() {
            if t.track_no != Some(i as i64 + 1) {
                return Ok(DiscKind::Skipped(format!(
                    "トラック番号が 1 から連続していない（{} 番目が {:?}）",
                    i + 1,
                    t.track_no
                )));
            }
        }
        // 開いて行と照合し、STREAMINFO を読む
        let mut opened = Vec::with_capacity(tracks.len());
        for t in tracks {
            ctx.check_cancel().await?;
            let root = Arc::clone(&self.root);
            let track = t.clone();
            let o = tokio::task::spawn_blocking(move || open_track(&root, &track))
                .await
                .map_err(|e| JobError::Failed(e.into()))??;
            match o {
                Some(o) => opened.push(o),
                None => {
                    return Ok(DiscKind::Skipped(format!(
                    "{}: ファイルが DB の行と一致しない（差し替えか更新）。再スキャン後に再投入",
                    t.rel_path
                )))
                }
            }
        }
        let toc = match Toc::from_audio_sample_counts(opened.iter().map(|o| o.total_samples)) {
            Ok(toc) => toc,
            Err(TocError::NotSectorAligned { index, samples }) => {
                return Ok(DiscKind::Unverifiable(format!(
                    "{}: サンプル数 {samples} が 588 の倍数でない（CD 由来でないか加工済み）",
                    tracks[index].rel_path
                )))
            }
            Err(e) => {
                return Ok(DiscKind::Unverifiable(format!("TOC を作れない: {e}")));
            }
        };
        let layout = toc
            .track_layout()
            .map_err(|e| JobError::Failed(anyhow::anyhow!("レイアウト: {e}")))?;

        // デコードして CRC 表を作る（トラックごとにブロッキングスレッドで）
        let mut sampler = CrcSampler::new(&layout);
        for (t, o) in tracks.iter().zip(opened) {
            ctx.check_cancel().await?;
            let rel = t.rel_path.clone();
            let expected = o.total_samples;
            let (s, frames) =
                tokio::task::spawn_blocking(move || -> Result<(CrcSampler, u64), JobError> {
                    let mut s = sampler;
                    let frames = decode_s16(o.file, Some("flac"), |chunk| {
                        s.push(chunk).map_err(anyhow::Error::from)
                    })
                    .map_err(|e| JobError::Failed(anyhow::anyhow!("{rel}: デコード: {e}")))?;
                    Ok((s, frames))
                })
                .await
                .map_err(|e| JobError::Failed(e.into()))??;
            if frames != expected {
                return Err(JobError::Failed(anyhow::anyhow!(
                    "{}: デコードしたサンプル数 {frames} が STREAMINFO の {expected} と違う（壊れている）",
                    t.rel_path
                )));
            }
            sampler = s;
        }
        let table: CrcTable = sampler
            .finish()
            .map_err(|e| JobError::Failed(anyhow::anyhow!("CRC 表: {e}")))?;

        // 照会（CTDB を主、AccurateRip を補助。D-13）
        ctx.check_cancel().await?;
        let ctdb_entries: Vec<CtdbEntry> = self
            .ctdb
            .lookup(&toc)
            .await
            .map_err(|e| JobError::Failed(anyhow::anyhow!("CTDB の照会: {e}")))?;
        ctx.check_cancel().await?;
        let ar_entries: Vec<ArDiscEntry> = self
            .ar
            .lookup(&toc.accuraterip_id())
            .await
            .map_err(|e| JobError::Failed(anyhow::anyhow!("AccurateRip の照会: {e}")))?;
        ctx.check_cancel().await?;
        let ctdb = match_ctdb(&table, &toc, &ctdb_entries);
        let ar = match_accuraterip(&table, &ar_entries);
        tracing::info!(
            job_id,
            album_id,
            disc_no,
            ctdb = ?ctdb.outcome,
            ctdb_offset = ctdb.offset,
            ar = ?ar.outcome,
            ar_offset = ar.offset,
            "照合した"
        );
        Ok(DiscKind::Verified {
            toc,
            ctdb,
            ar,
            ar_entries: ar_entries.len(),
            ctdb_entries: ctdb_entries.len(),
        })
    }
}

/// root で開いて fstat を行と照合し、STREAMINFO を読む。一致しなければ `None`
fn open_track(root: &RootDir, t: &VerifyTrack) -> Result<Option<Opened>, JobError> {
    let rel = RelPath::parse(&t.rel_path)
        .map_err(|e| JobError::Failed(anyhow::anyhow!("rel_path が不正: {e}")))?;
    let mut file = root
        .open_file(&rel)
        .map_err(|e| JobError::Failed(anyhow::anyhow!("{}: 開けない: {e}", t.rel_path)))?;
    let st = fsroot::fstat(&file)
        .map_err(|e| JobError::Failed(anyhow::anyhow!("{}: stat できない: {e}", t.rel_path)))?;
    if st.kind != fsroot::FileKind::File || !t.matches(&st) {
        return Ok(None);
    }
    let info = flac_streaminfo(&mut file).map_err(|e| {
        JobError::Failed(anyhow::anyhow!(
            "{}: STREAMINFO を読めない: {e}",
            t.rel_path
        ))
    })?;
    if info.sample_rate != 44_100 || info.channels != 2 || info.bits_per_sample != 16 {
        return Err(JobError::Failed(anyhow::anyhow!(
            "{}: STREAMINFO が DB の行と違う（{}Hz/{}bit/{}ch）。再スキャン待ち",
            t.rel_path,
            info.sample_rate,
            info.bits_per_sample,
            info.channels
        )));
    }
    if info.total_samples == 0 {
        return Err(JobError::Failed(anyhow::anyhow!(
            "{}: STREAMINFO にサンプル数が無い",
            t.rel_path
        )));
    }
    use std::io::Seek;
    file.rewind().map_err(|e| JobError::Failed(e.into()))?;
    Ok(Some(Opened {
        file,
        total_samples: info.total_samples,
    }))
}

/// 記録の直前の再照合。root からパスで開き直して fstat し、行と一致するか
/// （rename での差し替えは inode、その場の更新は size / mtime / ctime で外れる）
fn still_matches(root: &RootDir, t: &VerifyTrack) -> Result<bool, JobError> {
    let rel = RelPath::parse(&t.rel_path)
        .map_err(|e| JobError::Failed(anyhow::anyhow!("rel_path が不正: {e}")))?;
    let file = match root.open_file(&rel) {
        Ok(f) => f,
        Err(fsroot::FsError::NotFound) => return Ok(false),
        Err(e) => {
            return Err(JobError::Failed(anyhow::anyhow!(
                "{}: 開けない: {e}",
                t.rel_path
            )))
        }
    };
    let st = fsroot::fstat(&file)
        .map_err(|e| JobError::Failed(anyhow::anyhow!("{}: stat できない: {e}", t.rel_path)))?;
    Ok(st.kind == fsroot::FileKind::File && t.matches(&st))
}

impl DiscKind {
    fn summary(&self) -> String {
        match self {
            DiscKind::Skipped(why) => format!("skipped ({why})"),
            DiscKind::Unverifiable(why) => format!("unverifiable ({why})"),
            DiscKind::Verified { ctdb, ar, .. } => format!(
                "ctdb {:?} @{} / accuraterip {:?} @{}",
                ctdb.outcome, ctdb.offset, ar.outcome, ar.offset
            ),
        }
    }
}

fn disc_result(r: &MethodResult) -> DiscResult {
    match r.outcome {
        Outcome::Verified => DiscResult::Verified,
        Outcome::Mismatch => DiscResult::Mismatch,
        Outcome::NotFound => DiscResult::NotFound,
    }
}

/// 1 ディスクの判定を DB の記録に直す。Skipped は `None`
fn disc_record(r: &DiscReport) -> Option<DiscRecord> {
    match &r.kind {
        DiscKind::Skipped(_) => None,
        DiscKind::Unverifiable(_) => {
            let records: Vec<TrackRecord> = r
                .tracks
                .iter()
                .map(|t| TrackRecord {
                    track_id: t.id,
                    crc_v1: None,
                    crc_v2: None,
                    ctdb_crc: None,
                    matched: false,
                })
                .collect();
            Some(DiscRecord {
                disc_no: r.disc_no,
                methods: [Method::Ctdb, Method::AccurateRip]
                    .into_iter()
                    .map(|method| MethodRecord {
                        method,
                        result: DiscResult::Unverifiable,
                        detected_offset: None,
                        confidence: None,
                        tracks: records.clone(),
                    })
                    .collect(),
                states: r
                    .tracks
                    .iter()
                    .map(|t| (t.id, TrackState::Unverifiable))
                    .collect(),
            })
        }
        DiscKind::Verified { ctdb, ar, .. } => {
            let ctdb_records = MethodRecord {
                method: Method::Ctdb,
                result: disc_result(ctdb),
                detected_offset: Some(ctdb.offset),
                confidence: Some(ctdb.confidence),
                tracks: r
                    .tracks
                    .iter()
                    .zip(&ctdb.tracks)
                    .map(|(t, v)| TrackRecord {
                        track_id: t.id,
                        crc_v1: None,
                        crc_v2: None,
                        ctdb_crc: Some(v.crc),
                        matched: v.matched,
                    })
                    .collect(),
            };
            let ar_records = MethodRecord {
                method: Method::AccurateRip,
                result: disc_result(ar),
                detected_offset: Some(ar.offset),
                confidence: Some(ar.confidence),
                tracks: r
                    .tracks
                    .iter()
                    .zip(&ar.tracks)
                    .map(|(t, v)| TrackRecord {
                        track_id: t.id,
                        crc_v1: Some(v.crc),
                        crc_v2: v.crc_v2,
                        ctdb_crc: None,
                        matched: v.matched,
                    })
                    .collect(),
            };
            let any_candidates =
                ctdb.outcome != Outcome::NotFound || ar.outcome != Outcome::NotFound;
            let states = r
                .tracks
                .iter()
                .enumerate()
                .filter_map(|(i, t)| {
                    let state = if ctdb.tracks[i].matched {
                        TrackState::VerifiedCtdb
                    } else if ar.tracks[i].matched {
                        TrackState::VerifiedAr
                    } else if any_candidates {
                        TrackState::Mismatch
                    } else {
                        return None;
                    };
                    Some((t.id, state))
                })
                .collect();
            Some(DiscRecord {
                disc_no: r.disc_no,
                methods: vec![ctdb_records, ar_records],
                states,
            })
        }
    }
}

// ---------------------------------------------------------------- verify.log

fn render_log(album: &AlbumInfo, reports: &[DiscReport], now: i64) -> String {
    let mut s = String::new();
    s.push_str("spindle verify.log（遡及照合。CUETools の verify from files 相当）\n");
    s.push_str(&format!("日時: {}\n", format_utc(now)));
    s.push_str(&format!(
        "アルバム: {} / {}\n",
        album.albumartist.as_deref().unwrap_or("-"),
        album.album.as_deref().unwrap_or("-")
    ));
    s.push_str(&format!("ディレクトリ: {}\n", album.rel_dir));
    s.push_str(&format!("album_id: {}\n", album.id));
    for r in reports {
        s.push('\n');
        s.push_str(&format!("[Disc {}]\n", r.disc_no));
        match &r.kind {
            DiscKind::Skipped(why) => {
                s.push_str(&format!("判定なし: {why}\n"));
            }
            DiscKind::Unverifiable(why) => {
                s.push_str(&format!("unverifiable: {why}\n"));
            }
            DiscKind::Verified {
                toc,
                ctdb,
                ar,
                ar_entries,
                ctdb_entries,
            } => {
                s.push_str(&format!("TOC: {}\n", toc.ctdb_toc()));
                s.push_str(&format!(
                    "MusicBrainz DiscID: {}\n",
                    toc.musicbrainz_disc_id()
                ));
                s.push_str(&format!("AccurateRip ID: {}\n", toc.accuraterip_id()));
                s.push_str(&format!("CTDB TOCID: {}\n", toc.ctdb_toc_id()));
                s.push_str(&format!(
                    "CTDB: {} (offset {}, confidence {}, {} entries)\n",
                    outcome_str(ctdb.outcome),
                    ctdb.offset,
                    ctdb.confidence,
                    ctdb_entries
                ));
                s.push_str(&format!(
                    "AccurateRip: {} (offset {}, confidence {}, {} entries)\n",
                    outcome_str(ar.outcome),
                    ar.offset,
                    ar.confidence,
                    ar_entries
                ));
                s.push_str(
                    " No  Length    CRC32     ARv1      ARv2      CTDB      AccurateRip  File\n",
                );
                for (i, t) in r.tracks.iter().enumerate() {
                    let sectors = toc
                        .track_layout()
                        .map(|l| l.lengths()[i] / SECTOR_SAMPLES)
                        .unwrap_or(0);
                    let cv = &ctdb.tracks[i];
                    let av = &ar.tracks[i];
                    s.push_str(&format!(
                        " {:02}  {:>8}  {:08x}  {:08x}  {:08x}  {:<8}  {:<11}  {}\n",
                        i + 1,
                        format_msf(sectors),
                        cv.crc,
                        av.crc,
                        av.crc_v2.unwrap_or(0),
                        verdict_str(cv.matched, cv.confidence),
                        verdict_str(av.matched, av.confidence),
                        t.rel_path
                    ));
                }
            }
        }
    }
    s
}

fn outcome_str(o: Outcome) -> &'static str {
    match o {
        Outcome::Verified => "verified",
        Outcome::Mismatch => "mismatch",
        Outcome::NotFound => "not found",
    }
}

fn verdict_str(matched: bool, confidence: u32) -> String {
    if matched {
        format!("ok({confidence})")
    } else {
        "-".to_owned()
    }
}

/// セクタ数を MM:SS.FF に
fn format_msf(sectors: u64) -> String {
    let frames = sectors % 75;
    let secs = sectors / 75;
    format!("{:02}:{:02}.{:02}", secs / 60, secs % 60, frames)
}

/// tmp に書いて rename（途中で落ちても壊れたログを残さない）
fn write_log(path: &Path, text: &str) -> Result<(), JobError> {
    let dir = path.parent().ok_or_else(|| {
        JobError::Failed(anyhow::anyhow!("ログの置き場が不正: {}", path.display()))
    })?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("verify.log")
    ));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

impl Handler for VerifyHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let this = VerifyHandler {
            root: Arc::clone(&self.root),
            log_dir: self.log_dir.clone(),
            ar: self.ar.clone(),
            ctdb: self.ctdb.clone(),
        };
        Box::pin(async move { this.run_inner(&ctx).await })
    }
}

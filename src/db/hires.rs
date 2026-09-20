//! 偽ハイレゾ検出の結果（P3-5、D-71、SPEC §7.10）。`tracks.hires_*` の読み書きと、対象の列挙。
//! 対象は可逆かつ active で `sample_rate > 48000` または `bit_depth > 16`

use rusqlite::{params, Connection, OptionalExtension};

use super::jobs::{self as dbjobs, EnqueueResult, JobType, NewJob};
use super::Result;

/// 対象の条件（`t.` なしの列名。`tracks` 単独のクエリで使う）
pub const TARGET_SQL: &str =
    "lossless = 1 AND missing_since IS NULL AND (sample_rate > 48000 OR bit_depth > 16)";

/// 検査結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Upsampled,
    Padded,
    Both,
    Inconclusive,
    DecodeError,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Upsampled => "upsampled",
            Status::Padded => "padded",
            Status::Both => "both",
            Status::Inconclusive => "inconclusive",
            Status::DecodeError => "decode_error",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ok" => Some(Status::Ok),
            "upsampled" => Some(Status::Upsampled),
            "padded" => Some(Status::Padded),
            "both" => Some(Status::Both),
            "inconclusive" => Some(Status::Inconclusive),
            "decode_error" => Some(Status::DecodeError),
            _ => None,
        }
    }
}

/// 検査対象の行（FD の fstat と照合するための物理属性込み）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub id: i64,
    pub rel_path: String,
    pub lossless: bool,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i64>,
    pub missing: bool,
    pub audio_version: i64,
    pub inode: Option<i64>,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl Target {
    /// 可逆かつ active で >48 kHz または >16 bit（`TARGET_SQL` と同じ条件）
    pub fn eligible(&self) -> bool {
        self.lossless
            && !self.missing
            && (self.sample_rate.is_some_and(|r| r > 48_000)
                || self.bit_depth.is_some_and(|b| b > 16))
    }

    /// 開いた FD の stat がこの行と一致するか。dev は照合しない（D-62）
    pub fn matches(&self, st: &crate::fsroot::Stat) -> bool {
        self.inode == Some(st.inode as i64)
            && self.size == st.size as i64
            && self.mtime_ns == st.mtime_ns
            && self.ctime_ns == st.ctime_ns
    }
}

pub fn load_target(conn: &Connection, track_id: i64) -> Result<Option<Target>> {
    Ok(conn
        .query_row(
            "SELECT id, rel_path, lossless, sample_rate, bit_depth, missing_since IS NOT NULL,
                    audio_version, inode, size, mtime_ns, ctime_ns
             FROM tracks WHERE id = ?1",
            [track_id],
            |r| {
                Ok(Target {
                    id: r.get(0)?,
                    rel_path: r.get(1)?,
                    lossless: r.get(2)?,
                    sample_rate: r.get(3)?,
                    bit_depth: r.get(4)?,
                    missing: r.get(5)?,
                    audio_version: r.get(6)?,
                    inode: r.get(7)?,
                    size: r.get(8)?,
                    mtime_ns: r.get(9)?,
                    ctime_ns: r.get(10)?,
                })
            },
        )
        .optional()?)
}

/// 結果を書く。`audio_version` が検査時と違えば書かない（false）
#[allow(clippy::too_many_arguments)]
pub fn record(
    conn: &Connection,
    track_id: i64,
    audio_version: i64,
    status: Status,
    error: Option<&str>,
    cutoff_hz: Option<u32>,
    cliff_db: Option<f64>,
    effective_bits: Option<u32>,
    now: i64,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE tracks SET hires_check = ?3, hires_checked_at = ?4, hires_check_version = ?2,
                           hires_check_error = ?5, hires_cutoff_hz = ?6, hires_cliff_db = ?7,
                           hires_effective_bits = ?8
         WHERE id = ?1 AND audio_version = ?2",
        params![
            track_id,
            audio_version,
            status.as_str(),
            now,
            error,
            cutoff_hz.map(i64::from),
            cliff_db,
            effective_bits.map(i64::from),
        ],
    )?;
    Ok(n > 0)
}

pub fn dedup_key(track_id: i64, audio_version: i64) -> String {
    format!("hirescheck:{track_id}:{audio_version}")
}

pub fn new_job(track_id: i64, audio_version: i64) -> NewJob {
    NewJob::new(
        JobType::Hirescheck,
        serde_json::json!({ "track_id": track_id, "audio_version": audio_version }),
    )
    .dedup_key(dedup_key(track_id, audio_version))
}

/// 対象のうち、現在の `audio_version` の結果が無いものを全件投入する（スキャン完了時。
/// `[hires].check_on_import`）。投入したジョブ id
pub fn enqueue_all_unchecked(conn: &Connection, now: i64) -> Result<Vec<i64>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT id, audio_version FROM tracks
          WHERE {TARGET_SQL}
            AND (hires_check_version IS NULL OR hires_check_version <> audio_version)
          ORDER BY id"
    ))?;
    let targets: Vec<(i64, i64)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut ids = Vec::new();
    for (id, ver) in targets {
        if let EnqueueResult::Inserted(job) = dbjobs::enqueue(conn, &new_job(id, ver), now)? {
            ids.push(job);
        }
    }
    Ok(ids)
}

/// selection に含まれる対象を投入する。`(投入, 対象外で飛ばした数, 重複)`
pub fn enqueue_selection(
    conn: &Connection,
    track_ids: &[i64],
    now: i64,
) -> Result<(Vec<i64>, usize, usize)> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT audio_version FROM tracks WHERE id = ?1 AND {TARGET_SQL}"
    ))?;
    let mut ids = Vec::new();
    let mut skipped = 0;
    let mut duplicates = 0;
    for id in track_ids {
        let ver: Option<i64> = st.query_row([id], |r| r.get(0)).optional()?;
        let Some(ver) = ver else {
            skipped += 1;
            continue;
        };
        match dbjobs::enqueue(conn, &new_job(*id, ver), now)? {
            EnqueueResult::Inserted(job) => ids.push(job),
            EnqueueResult::Duplicate(_) => duplicates += 1,
        }
    }
    Ok((ids, skipped, duplicates))
}

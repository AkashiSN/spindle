//! FLAC 健全性チェックの結果（P1-5、D-57）。`tracks.flac_check*` の読み書きと、対象の列挙

use rusqlite::{params, Connection, OptionalExtension};

use super::jobs::{self as dbjobs, EnqueueResult, JobType, NewJob};
use super::Result;

/// 検査結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Md5Missing,
    DecodeError,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Md5Missing => "md5_missing",
            Status::DecodeError => "decode_error",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ok" => Some(Status::Ok),
            "md5_missing" => Some(Status::Md5Missing),
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
    pub codec: String,
    pub missing: bool,
    pub audio_version: i64,
    pub dev: Option<i64>,
    pub inode: Option<i64>,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl Target {
    /// 開いた FD の stat がこの行と一致するか（同じ実体で、DB に取り込んだ後に書かれていない）
    pub fn matches(&self, st: &crate::fsroot::Stat) -> bool {
        self.dev == Some(st.dev as i64)
            && self.inode == Some(st.inode as i64)
            && self.size == st.size as i64
            && self.mtime_ns == st.mtime_ns
            && self.ctime_ns == st.ctime_ns
    }
}

pub fn load_target(conn: &Connection, track_id: i64) -> Result<Option<Target>> {
    Ok(conn
        .query_row(
            "SELECT id, rel_path, codec, missing_since IS NOT NULL, audio_version,
                    dev, inode, size, mtime_ns, ctime_ns
             FROM tracks WHERE id = ?1",
            [track_id],
            |r| {
                Ok(Target {
                    id: r.get(0)?,
                    rel_path: r.get(1)?,
                    codec: r.get(2)?,
                    missing: r.get(3)?,
                    audio_version: r.get(4)?,
                    dev: r.get(5)?,
                    inode: r.get(6)?,
                    size: r.get(7)?,
                    mtime_ns: r.get(8)?,
                    ctime_ns: r.get(9)?,
                })
            },
        )
        .optional()?)
}

/// 結果を書く。`audio_version` が検査時と違えば書かない（false）
pub fn record(
    conn: &Connection,
    track_id: i64,
    audio_version: i64,
    status: Status,
    error: Option<&str>,
    now: i64,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE tracks SET flac_check = ?3, flac_checked_at = ?4, flac_check_version = ?2,
                           flac_check_error = ?5
         WHERE id = ?1 AND audio_version = ?2",
        params![track_id, audio_version, status.as_str(), now, error],
    )?;
    Ok(n > 0)
}

pub fn dedup_key(track_id: i64, audio_version: i64) -> String {
    format!("flaccheck:{track_id}:{audio_version}")
}

pub fn new_job(track_id: i64, audio_version: i64) -> NewJob {
    NewJob::new(
        JobType::Flaccheck,
        serde_json::json!({ "track_id": track_id, "audio_version": audio_version }),
    )
    .dedup_key(dedup_key(track_id, audio_version))
}

/// active な FLAC のうち、現在の `audio_version` の結果が無いものを全件投入する（スキャン完了時。
/// `[normalize].flac_verify_on_import`）。投入したジョブ id
pub fn enqueue_all_unchecked(conn: &Connection, now: i64) -> Result<Vec<i64>> {
    let mut st = conn.prepare_cached(
        "SELECT id, audio_version FROM tracks
          WHERE codec = 'flac' AND missing_since IS NULL
            AND (flac_check_version IS NULL OR flac_check_version <> audio_version)
          ORDER BY id",
    )?;
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

/// selection に含まれる active な FLAC を投入する。`(投入, 非 FLAC / missing で飛ばした数, 重複)`
pub fn enqueue_selection(
    conn: &Connection,
    track_ids: &[i64],
    now: i64,
) -> Result<(Vec<i64>, usize, usize)> {
    let mut st = conn.prepare_cached(
        "SELECT audio_version FROM tracks WHERE id = ?1 AND codec = 'flac' AND missing_since IS NULL",
    )?;
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

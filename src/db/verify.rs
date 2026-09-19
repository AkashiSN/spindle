//! 遡及照合の DB 側（P2-9、SPEC §7.3）。`album_verifications` / `track_verifications` への記録、
//! `tracks.verification` の更新、対象の列挙と `verify` ジョブの投入

use std::collections::BTreeSet;

use rusqlite::{params, Connection, OptionalExtension};

use super::jobs::{self as dbjobs, EnqueueResult, JobType, NewJob};
use super::Result;

/// 照合の対象となる行（FD の fstat と照合するための物理属性込み）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyTrack {
    pub id: i64,
    pub rel_path: String,
    pub codec: String,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i64>,
    pub channels: Option<i64>,
    pub track_no: Option<i64>,
    pub disc_no: Option<i64>,
    pub missing: bool,
    pub audio_version: i64,
    pub inode: Option<i64>,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl VerifyTrack {
    /// 開いた FD の stat がこの行と一致するか。dev は照合しない（D-62）
    pub fn matches(&self, st: &crate::fsroot::Stat) -> bool {
        self.inode == Some(st.inode as i64)
            && self.size == st.size as i64
            && self.mtime_ns == st.mtime_ns
            && self.ctime_ns == st.ctime_ns
    }
}

/// アルバムの表示用の情報（verify.log のヘッダ）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumInfo {
    pub id: i64,
    pub rel_dir: String,
    pub albumartist: Option<String>,
    pub album: Option<String>,
}

pub fn load_album(conn: &Connection, album_id: i64) -> Result<Option<AlbumInfo>> {
    Ok(conn
        .query_row(
            "SELECT id, rel_dir, albumartist, album FROM albums WHERE id = ?1",
            [album_id],
            |r| {
                Ok(AlbumInfo {
                    id: r.get(0)?,
                    rel_dir: r.get(1)?,
                    albumartist: r.get(2)?,
                    album: r.get(3)?,
                })
            },
        )
        .optional()?)
}

/// アルバムの active なトラックを disc_no, track_no 順に
pub fn load_album_tracks(conn: &Connection, album_id: i64) -> Result<Vec<VerifyTrack>> {
    let mut st = conn.prepare_cached(
        "SELECT id, rel_path, codec, sample_rate, bit_depth, channels, track_no, disc_no,
                missing_since IS NOT NULL, audio_version, inode, size, mtime_ns, ctime_ns
           FROM tracks
          WHERE album_id = ?1 AND missing_since IS NULL
          ORDER BY disc_no, track_no, id",
    )?;
    let rows = st
        .query_map([album_id], |r| {
            Ok(VerifyTrack {
                id: r.get(0)?,
                rel_path: r.get(1)?,
                codec: r.get(2)?,
                sample_rate: r.get(3)?,
                bit_depth: r.get(4)?,
                channels: r.get(5)?,
                track_no: r.get(6)?,
                disc_no: r.get(7)?,
                missing: r.get(8)?,
                audio_version: r.get(9)?,
                inode: r.get(10)?,
                size: r.get(11)?,
                mtime_ns: r.get(12)?,
                ctime_ns: r.get(13)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 照合手法
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    AccurateRip,
    Ctdb,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::AccurateRip => "accuraterip",
            Method::Ctdb => "ctdb",
        }
    }
}

/// 記録の出所（`album_verifications.source`）。自前のリップか遡及照合か
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifySource {
    Rip,
    Retro,
}

impl VerifySource {
    pub fn as_str(self) -> &'static str {
        match self {
            VerifySource::Rip => "rip",
            VerifySource::Retro => "retro",
        }
    }
}

/// ディスク単位の結論（`album_verifications.result`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscResult {
    Verified,
    Mismatch,
    NotFound,
    Unverifiable,
}

impl DiscResult {
    pub fn as_str(self) -> &'static str {
        match self {
            DiscResult::Verified => "verified",
            DiscResult::Mismatch => "mismatch",
            DiscResult::NotFound => "not_found",
            DiscResult::Unverifiable => "unverifiable",
        }
    }
}

/// トラック 1 本の記録
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackRecord {
    pub track_id: i64,
    pub crc_v1: Option<u32>,
    pub crc_v2: Option<u32>,
    pub ctdb_crc: Option<u32>,
    pub matched: bool,
}

/// `tracks.verification` の値
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackState {
    VerifiedAr,
    VerifiedCtdb,
    Mismatch,
    Unverifiable,
}

impl TrackState {
    pub fn as_str(self) -> &'static str {
        match self {
            TrackState::VerifiedAr => "verified_ar",
            TrackState::VerifiedCtdb => "verified_ctdb",
            TrackState::Mismatch => "mismatch",
            TrackState::Unverifiable => "unverifiable",
        }
    }
}

/// 1 ディスク × 1 手法の結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodRecord {
    pub method: Method,
    pub result: DiscResult,
    pub detected_offset: Option<i32>,
    pub confidence: Option<u32>,
    pub tracks: Vec<TrackRecord>,
}

/// 1 ディスクの記録（手法ごとの行と、トラックの状態）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscRecord {
    pub disc_no: i64,
    pub methods: Vec<MethodRecord>,
    /// 更新するトラックの状態（据え置くトラックは含めない）
    pub states: Vec<(i64, TrackState)>,
}

/// [`record_album`] の結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordOutcome {
    /// 記録した（`album_verifications.id` の一覧）
    Recorded(Vec<i64>),
    /// 照合中に音声が変わった（`audio_version` が進んだか行が消えた）トラックがあり、何も記録しなかった
    Changed { track_id: i64 },
    /// 同じジョブが既に記録している（commit の後に落ちて再実行された）。何も書かなかった
    AlreadyRecorded,
}

/// このジョブが既に記録しているか（commit の後に落ちて再実行されたとき）
pub fn has_records_for_job(conn: &Connection, job_id: i64) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM album_verifications WHERE job_id = ?1",
        [job_id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// アルバムの照合結果をまとめて記録する。呼び出し側がトランザクションの中で呼ぶこと
/// （`Db::transaction`）。`expected` は照合を始めたときの `(track_id, audio_version)` で、
/// 1 本でも現在値と違えば何も書かずに [`RecordOutcome::Changed`]（部分記録も stale な記録もしない）。
/// 同じ `job_id` の行が既にあれば（commit 後の再実行）何も書かずに [`RecordOutcome::AlreadyRecorded`]
#[allow(clippy::too_many_arguments)]
pub fn record_album(
    conn: &Connection,
    album_id: i64,
    job_id: i64,
    source: VerifySource,
    expected: &[(i64, i64)],
    discs: &[DiscRecord],
    log_path: Option<&str>,
    now: i64,
) -> Result<RecordOutcome> {
    if has_records_for_job(conn, job_id)? {
        return Ok(RecordOutcome::AlreadyRecorded);
    }
    let mut st = conn.prepare_cached(
        "SELECT audio_version FROM tracks WHERE id = ?1 AND missing_since IS NULL",
    )?;
    for &(track_id, version) in expected {
        let current: Option<i64> = st.query_row([track_id], |r| r.get(0)).optional()?;
        if current != Some(version) {
            return Ok(RecordOutcome::Changed { track_id });
        }
    }
    let mut ids = Vec::new();
    for d in discs {
        for m in &d.methods {
            ids.push(record_disc(
                conn,
                album_id,
                job_id,
                source,
                d.disc_no,
                m.method,
                m.result,
                m.detected_offset,
                m.confidence,
                log_path,
                now,
                &m.tracks,
            )?);
        }
        for &(track_id, state) in &d.states {
            conn.execute(
                "UPDATE tracks SET verification = ?2 WHERE id = ?1",
                params![track_id, state.as_str()],
            )?;
        }
    }
    Ok(RecordOutcome::Recorded(ids))
}

/// 1 ディスク × 1 手法の結果を履歴として積む。返り値は `album_verifications.id`
#[allow(clippy::too_many_arguments)]
fn record_disc(
    conn: &Connection,
    album_id: i64,
    job_id: i64,
    source: VerifySource,
    disc_no: i64,
    method: Method,
    result: DiscResult,
    detected_offset: Option<i32>,
    confidence: Option<u32>,
    log_path: Option<&str>,
    now: i64,
    tracks: &[TrackRecord],
) -> Result<i64> {
    conn.execute(
        "INSERT INTO album_verifications
           (album_id, method, result, source, drive_offset, detected_offset, confidence,
            verified_at, log_path, disc_no, job_id)
         VALUES (?1, ?2, ?3, ?10, NULL, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            album_id,
            method.as_str(),
            result.as_str(),
            detected_offset,
            confidence.map(i64::from),
            now,
            log_path,
            disc_no,
            job_id,
            source.as_str()
        ],
    )?;
    let id = conn.last_insert_rowid();
    let mut st = conn.prepare_cached(
        "INSERT INTO track_verifications (track_id, verification_id, crc_v1, crc_v2, ctdb_crc, matched)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for t in tracks {
        st.execute(params![
            t.track_id,
            id,
            t.crc_v1.map(i64::from),
            t.crc_v2.map(i64::from),
            t.ctdb_crc.map(i64::from),
            t.matched
        ])?;
    }
    Ok(id)
}

pub fn dedup_key(album_id: i64) -> String {
    format!("verify:{album_id}")
}

pub fn new_job(album_id: i64) -> NewJob {
    NewJob::new(JobType::Verify, serde_json::json!({ "album_id": album_id }))
        .dedup_key(dedup_key(album_id))
}

/// selection のトラックが属するアルバムを投入する。`(投入したジョブ id, 重複)`
pub fn enqueue_selection(
    conn: &Connection,
    track_ids: &[i64],
    now: i64,
) -> Result<(Vec<i64>, usize)> {
    let mut st = conn.prepare_cached(
        "SELECT album_id FROM tracks WHERE id = ?1 AND album_id IS NOT NULL AND missing_since IS NULL",
    )?;
    let mut albums = BTreeSet::new();
    for id in track_ids {
        let album: Option<i64> = st.query_row([id], |r| r.get(0)).optional()?;
        if let Some(a) = album {
            albums.insert(a);
        }
    }
    enqueue_albums(conn, albums.into_iter(), now)
}

/// active な FLAC を持つ全アルバムを投入する
pub fn enqueue_all(conn: &Connection, now: i64) -> Result<(Vec<i64>, usize)> {
    let mut st = conn.prepare_cached(
        "SELECT DISTINCT album_id FROM tracks
          WHERE codec = 'flac' AND album_id IS NOT NULL AND missing_since IS NULL
          ORDER BY album_id",
    )?;
    let albums: Vec<i64> = st
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    enqueue_albums(conn, albums.into_iter(), now)
}

fn enqueue_albums(
    conn: &Connection,
    albums: impl Iterator<Item = i64>,
    now: i64,
) -> Result<(Vec<i64>, usize)> {
    let mut ids = Vec::new();
    let mut duplicates = 0;
    for album in albums {
        match dbjobs::enqueue(conn, &new_job(album), now)? {
            EnqueueResult::Inserted(job) => ids.push(job),
            EnqueueResult::Duplicate(_) => duplicates += 1,
        }
    }
    Ok((ids, duplicates))
}

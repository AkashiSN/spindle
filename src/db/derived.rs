//! `derived_files` の読み書きと `transcode` ジョブの投入（SPEC §6「版管理」/ §8、D-51）。
//!
//! 投入判定は [`enqueue_if_stale`] に集約する。呼ぶのは scan 完了時（[`enqueue_all_stale`]）、
//! tagwrite / rename の applied、RG 解析の保存後。ジョブは `audio_version` 単位で dedup され、
//! ハンドラは現在値から必要な処理を判定するので、投入が重複しても最初の 1 本で全部片づく

use rusqlite::{params, Connection, OptionalExtension as _};

use crate::db::jobs::{self as dbjobs, EnqueueResult, JobType, NewJob};
use crate::db::Result;
use crate::domain::derived::{expected_rel_path, plan, Current, Target};
use crate::domain::relpath::canonical_key;

pub fn load_target(conn: &Connection, track_id: i64) -> Result<Option<Target>> {
    Ok(conn
        .query_row(
            "SELECT t.id, t.lossless, t.missing_since IS NOT NULL, t.channels, t.rel_path,
                    t.audio_version, t.tag_version, coalesce(t.artwork_id, a.artwork_id),
                    t.rg_scanned_at
             FROM tracks t LEFT JOIN albums a ON a.id = t.album_id
             WHERE t.id = ?1",
            [track_id],
            |r| {
                Ok(Target {
                    track_id: r.get(0)?,
                    lossless: r.get::<_, i64>(1)? == 1,
                    missing: r.get::<_, i64>(2)? == 1,
                    channels: r.get(3)?,
                    library_rel_path: r.get(4)?,
                    audio_version: r.get(5)?,
                    tag_version: r.get(6)?,
                    artwork_id: r.get(7)?,
                    rg_scanned_at: r.get(8)?,
                })
            },
        )
        .optional()?)
}

pub fn get(conn: &Connection, track_id: i64) -> Result<Option<Current>> {
    Ok(conn
        .query_row(
            "SELECT rel_path, src_audio_version, src_tag_version, src_artwork_id,
                    src_rg_scanned_at
             FROM derived_files WHERE track_id = ?1",
            [track_id],
            |r| {
                Ok(Current {
                    rel_path: r.get(0)?,
                    src_audio_version: r.get(1)?,
                    src_tag_version: r.get(2)?,
                    src_artwork_id: r.get(3)?,
                    src_rg_scanned_at: r.get(4)?,
                })
            },
        )
        .optional()?)
}

/// その Derived パス（canonical key）を持つトラックの状態
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub track_id: i64,
    pub missing: bool,
    /// 持っているパスが自分の期待パスと違う（Library 側で移動済みで、Derived が追随待ち）
    pub stale: bool,
}

/// `key` を `derived_files` で持つトラックとその状態
pub fn holder_state(conn: &Connection, key: &str) -> Result<Option<Holder>> {
    Ok(conn
        .query_row(
            "SELECT d.track_id, t.missing_since IS NOT NULL, t.rel_path
             FROM derived_files d JOIN tracks t ON t.id = d.track_id
             WHERE d.rel_path_key = ?1",
            [key],
            |r| {
                let library_rel_path: String = r.get(2)?;
                Ok(Holder {
                    track_id: r.get(0)?,
                    missing: r.get::<_, i64>(1)? == 1,
                    stale: canonical_key(&expected_rel_path(&library_rel_path)) != key,
                })
            },
        )
        .optional()?)
}

/// そのトラックの `transcode` が queued / running にあるか（追随待ちの相手が動いているか）
pub fn has_active_job(conn: &Connection, track_id: i64) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM jobs
              WHERE type = 'transcode' AND state IN ('queued', 'running')
                AND json_extract(payload, '$.track_id') = ?1
              LIMIT 1",
            [track_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// その Derived パス（canonical key）を持つ track_id
pub fn holder_of_key(conn: &Connection, key: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT track_id FROM derived_files WHERE rel_path_key = ?1",
            [key],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn track_is_missing(conn: &Connection, track_id: i64) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT missing_since IS NOT NULL FROM tracks WHERE id = ?1",
            [track_id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0)
        == 1)
}

/// Derived に書いたタグ側の世代（`plan` の retag 判定に使う 3 つ）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TagState {
    pub src_tag_version: i64,
    pub src_artwork_id: Option<i64>,
    pub src_rg_scanned_at: Option<i64>,
}

impl TagState {
    pub fn of(t: &Target) -> Self {
        Self {
            src_tag_version: t.tag_version,
            src_artwork_id: t.artwork_id,
            src_rg_scanned_at: t.rg_scanned_at,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn upsert(
    conn: &Connection,
    track_id: i64,
    rel_path: &str,
    codec: &str,
    bitrate: Option<i64>,
    src_audio_version: i64,
    tags: TagState,
    now: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO derived_files (track_id, rel_path, rel_path_key, codec, bitrate,
                                    src_audio_version, src_tag_version, src_artwork_id,
                                    src_rg_scanned_at, generated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(track_id) DO UPDATE SET
           rel_path = excluded.rel_path, rel_path_key = excluded.rel_path_key,
           codec = excluded.codec, bitrate = excluded.bitrate,
           src_audio_version = excluded.src_audio_version,
           src_tag_version = excluded.src_tag_version,
           src_artwork_id = excluded.src_artwork_id,
           src_rg_scanned_at = excluded.src_rg_scanned_at,
           generated_at = excluded.generated_at",
        params![
            track_id,
            rel_path,
            canonical_key(rel_path),
            codec,
            bitrate,
            src_audio_version,
            tags.src_tag_version,
            tags.src_artwork_id,
            tags.src_rg_scanned_at,
            now
        ],
    )?;
    Ok(())
}

pub fn set_path(conn: &Connection, track_id: i64, rel_path: &str) -> Result<()> {
    conn.execute(
        "UPDATE derived_files SET rel_path = ?2, rel_path_key = ?3 WHERE track_id = ?1",
        params![track_id, rel_path, canonical_key(rel_path)],
    )?;
    Ok(())
}

pub fn set_tag_state(conn: &Connection, track_id: i64, tags: TagState, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE derived_files SET src_tag_version = ?2, src_artwork_id = ?3, src_rg_scanned_at = ?4,
                generated_at = ?5
         WHERE track_id = ?1",
        params![
            track_id,
            tags.src_tag_version,
            tags.src_artwork_id,
            tags.src_rg_scanned_at,
            now
        ],
    )?;
    Ok(())
}

pub fn delete(conn: &Connection, track_id: i64) -> Result<bool> {
    Ok(conn.execute("DELETE FROM derived_files WHERE track_id = ?1", [track_id])? == 1)
}

pub fn dedup_key(track_id: i64, audio_version: i64) -> String {
    format!("transcode:{track_id}:{audio_version}")
}

/// `transcode` ジョブ（SPEC §8）。`audio_version` は基盤の stale 判定に使う。`tag_version` は
/// 参考情報（ハンドラは現在値を読み直す）
pub fn new_job(track_id: i64, audio_version: i64, tag_version: i64) -> NewJob {
    NewJob::new(
        JobType::Transcode,
        serde_json::json!({
            "track_id": track_id,
            "audio_version": audio_version,
            "tag_version": tag_version,
        }),
    )
    .dedup_key(dedup_key(track_id, audio_version))
}

fn enqueue_target(conn: &Connection, t: &Target, now: i64) -> Result<Option<i64>> {
    Ok(
        match dbjobs::enqueue(
            conn,
            &new_job(t.track_id, t.audio_version, t.tag_version),
            now,
        )? {
            EnqueueResult::Inserted(id) => Some(id),
            EnqueueResult::Duplicate(_) => None,
        },
    )
}

/// Derived が現在値と食い違っていれば `transcode` を投入する。投入した job id（対象外・揃っている・
/// dedup で既にあれば None）
pub fn enqueue_if_stale(conn: &Connection, track_id: i64, now: i64) -> Result<Option<i64>> {
    let Some(t) = load_target(conn, track_id)? else {
        return Ok(None);
    };
    let current = get(conn, track_id)?;
    if !plan(&t, current.as_ref()).needs_job() {
        return Ok(None);
    }
    enqueue_target(conn, &t, now)
}

/// 対象になりうる全トラック（可逆・active・1ch / 2ch）を見て食い違う分を一括投入する（scan 完了時）。
/// 期待パスの比較は SQL では書きにくいので行を取ってから Rust で判定する
pub fn enqueue_all_stale(conn: &Connection, now: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.channels, t.rel_path, t.audio_version, t.tag_version,
                coalesce(t.artwork_id, a.artwork_id), t.rg_scanned_at,
                d.rel_path, d.src_audio_version, d.src_tag_version, d.src_artwork_id,
                d.src_rg_scanned_at
         FROM tracks t
         LEFT JOIN albums a ON a.id = t.album_id
         LEFT JOIN derived_files d ON d.track_id = t.id
         WHERE t.missing_since IS NULL AND t.lossless = 1 AND t.channels IN (1, 2)
         ORDER BY t.id",
    )?;
    let rows = stmt.query_map([], |r| {
        let t = Target {
            track_id: r.get(0)?,
            lossless: true,
            missing: false,
            channels: r.get(1)?,
            library_rel_path: r.get(2)?,
            audio_version: r.get(3)?,
            tag_version: r.get(4)?,
            artwork_id: r.get(5)?,
            rg_scanned_at: r.get(6)?,
        };
        let rel: Option<String> = r.get(7)?;
        let current = match rel {
            Some(rel_path) => Some(Current {
                rel_path,
                src_audio_version: r.get(8)?,
                src_tag_version: r.get(9)?,
                src_artwork_id: r.get(10)?,
                src_rg_scanned_at: r.get(11)?,
            }),
            None => None,
        };
        Ok((t, current))
    })?;
    let mut ids = Vec::new();
    for row in rows {
        let (t, current) = row?;
        if !plan(&t, current.as_ref()).needs_job() {
            continue;
        }
        if let Some(id) = enqueue_target(conn, &t, now)? {
            ids.push(id);
        }
    }
    Ok(ids)
}

// ---------------------------------------------------------------- Derived パスの排他予約

/// `key`（Derived の canonical key）をジョブ `job_id` のために予約する。別のジョブが持っていれば
/// false。持ち主のジョブが `running` でなくなっていれば（panic / 強制終了）無効として奪う。
/// 同じジョブの再取得は true。`track_id` は GC の予約（孤児の削除。D-56）では `None`
pub fn lock_path(
    conn: &Connection,
    key: &str,
    track_id: Option<i64>,
    job_id: i64,
    now: i64,
) -> Result<bool> {
    conn.execute(
        "DELETE FROM derived_path_locks
          WHERE rel_path_key = ?1
            AND job_id NOT IN (SELECT id FROM jobs WHERE state = 'running')",
        [key],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO derived_path_locks (rel_path_key, track_id, job_id, acquired_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![key, track_id, job_id, now],
    )?;
    let holder: i64 = conn.query_row(
        "SELECT job_id FROM derived_path_locks WHERE rel_path_key = ?1",
        [key],
        |r| r.get(0),
    )?;
    Ok(holder == job_id)
}

/// ジョブが持つ予約を全部解放する
pub fn unlock_paths(conn: &Connection, job_id: i64) -> Result<usize> {
    Ok(conn.execute("DELETE FROM derived_path_locks WHERE job_id = ?1", [job_id])?)
}

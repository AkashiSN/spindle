//! `archived_files` 台帳（SPEC §7.4、D-10 / D-45 / D-46）。
//!
//! Library から Archive へ退避したファイルの台帳。GC（P1-11）は `state = 'held'` かつ
//! `eligible_after` を過ぎた行だけを物理削除の根拠にする。巻き戻しで Library へ戻した行は
//! `restored`（ファイルは Archive に残る。Archive は追記のみ）。
//! `rel_path` は Archive/ からの相対パスで UNIQUE。同じパスへ再度退避する（restore → redo）
//! ときは行を作り直さず `held` に戻して期限を更新する

use rusqlite::{params, Connection, OptionalExtension as _, Row};
use serde::Serialize;

use super::Result;
use crate::domain::relpath::canonical_key;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ArchiveReason {
    /// ロスレス正規化で置き換えられた元ファイル（WAV / ALAC / AIFF）
    Normalize,
    /// 正規化の巻き戻しで Library から外した FLAC
    Restore,
}

impl ArchiveReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ArchiveReason::Normalize => "normalize",
            ArchiveReason::Restore => "restore",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "normalize" => Some(ArchiveReason::Normalize),
            "restore" => Some(ArchiveReason::Restore),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ArchiveState {
    Held,
    Restored,
    Deleted,
}

impl ArchiveState {
    pub fn as_str(self) -> &'static str {
        match self {
            ArchiveState::Held => "held",
            ArchiveState::Restored => "restored",
            ArchiveState::Deleted => "deleted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "held" => Some(ArchiveState::Held),
            "restored" => Some(ArchiveState::Restored),
            "deleted" => Some(ArchiveState::Deleted),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArchivedFile {
    pub id: i64,
    pub track_id: Option<i64>,
    pub op_id: Option<i64>,
    pub rel_path: String,
    pub source_rel_path: String,
    pub reason: ArchiveReason,
    pub archived_at: i64,
    pub eligible_after: i64,
    pub state: ArchiveState,
    pub state_at: Option<i64>,
}

const COLUMNS: &str = "id, track_id, op_id, rel_path, source_rel_path, reason, archived_at,
                       eligible_after, state, state_at";

fn bad_enum(idx: usize, msg: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        idx,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::other(msg)),
    )
}

fn from_row(r: &Row<'_>) -> rusqlite::Result<ArchivedFile> {
    let reason: String = r.get(5)?;
    let state: String = r.get(8)?;
    Ok(ArchivedFile {
        id: r.get(0)?,
        track_id: r.get(1)?,
        op_id: r.get(2)?,
        rel_path: r.get(3)?,
        source_rel_path: r.get(4)?,
        reason: ArchiveReason::parse(&reason)
            .ok_or_else(|| bad_enum(5, format!("archived_files.reason が不正: {reason}")))?,
        archived_at: r.get(6)?,
        eligible_after: r.get(7)?,
        state: ArchiveState::parse(&state)
            .ok_or_else(|| bad_enum(8, format!("archived_files.state が不正: {state}")))?,
        state_at: r.get(9)?,
    })
}

/// 退避を台帳に載せる。同じ `rel_path`（key 一致）の行があれば作り直さず `held` に戻し、
/// 期限と出自を更新する（restore → redo で同じパスへ再退避する）。戻り値は行 id
#[allow(clippy::too_many_arguments)]
pub fn record_held(
    conn: &Connection,
    track_id: i64,
    op_id: Option<i64>,
    rel_path: &str,
    source_rel_path: &str,
    reason: ArchiveReason,
    now: i64,
    retention_days: u32,
) -> Result<i64> {
    let key = canonical_key(rel_path);
    let eligible_after = now + i64::from(retention_days) * 86_400;
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM archived_files WHERE rel_path_key = ?1",
            [&key],
            |r| r.get(0),
        )
        .optional()?;
    match existing {
        Some(id) => {
            conn.execute(
                "UPDATE archived_files
                 SET track_id = ?2, op_id = ?3, rel_path = ?4, source_rel_path = ?5, reason = ?6,
                     archived_at = ?7, eligible_after = ?8, state = 'held', state_at = ?7
                 WHERE id = ?1",
                params![
                    id,
                    track_id,
                    op_id,
                    rel_path,
                    source_rel_path,
                    reason.as_str(),
                    now,
                    eligible_after
                ],
            )?;
            Ok(id)
        }
        None => {
            conn.execute(
                "INSERT INTO archived_files
                   (track_id, op_id, rel_path, rel_path_key, source_rel_path, reason,
                    archived_at, eligible_after, state, state_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'held', ?7)",
                params![
                    track_id,
                    op_id,
                    rel_path,
                    key,
                    source_rel_path,
                    reason.as_str(),
                    now,
                    eligible_after
                ],
            )?;
            Ok(conn.last_insert_rowid())
        }
    }
}

/// `rel_path`（key 一致）の行
pub fn get_by_rel_path(conn: &Connection, rel_path: &str) -> Result<Option<ArchivedFile>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM archived_files WHERE rel_path_key = ?1"),
            [canonical_key(rel_path)],
            from_row,
        )
        .optional()?)
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<ArchivedFile>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM archived_files WHERE id = ?1"),
            [id],
            from_row,
        )
        .optional()?)
}

/// 状態を進める。遷移したら `true`
pub fn set_state(conn: &Connection, id: i64, state: ArchiveState, now: i64) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE archived_files SET state = ?2, state_at = ?3 WHERE id = ?1 AND state <> ?2",
        params![id, state.as_str(), now],
    )?;
    Ok(changed > 0)
}

/// 台帳の一覧（新しい順）に、退避した op のバッチ id を添える（`GET /api/archive`）
pub fn list_with_batch(conn: &Connection) -> Result<Vec<(ArchivedFile, Option<i64>)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS}, (SELECT o.batch_id FROM edit_ops o WHERE o.id = archived_files.op_id)
         FROM archived_files ORDER BY id DESC"
    ))?;
    let rows = stmt.query_map([], |r| Ok((from_row(r)?, r.get::<_, Option<i64>>(10)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 台帳の一覧（新しい順）
pub fn list(conn: &Connection) -> Result<Vec<ArchivedFile>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM archived_files ORDER BY id DESC"
    ))?;
    let rows = stmt.query_map([], from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

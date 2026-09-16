//! 編集履歴（`edit_batches` / `edit_ops` / `edits`）への同期アクセス（SPEC §7.5、D-24）。
//!
//! ここは `&Connection` を受け取る純粋な DB 操作だけを置く。バッチの組み立て・ファイル反映・
//! overlay の解消の手順は `crate::edit` が担う。状態遷移は `WHERE result = 'pending'` /
//! `WHERE state IN (...)` 付きの UPDATE で行い、同じ op / バッチを二重に終端へ進めない。

use std::fmt;
use std::str::FromStr;

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;

use super::{DbError, Result};

/// バッチ状態機械（SPEC §7.5）: `prepared → applying → applied | partial | failed | cancelled`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BatchState {
    Prepared,
    Applying,
    Applied,
    Partial,
    Failed,
    Cancelled,
}

impl BatchState {
    pub fn as_str(self) -> &'static str {
        match self {
            BatchState::Prepared => "prepared",
            BatchState::Applying => "applying",
            BatchState::Applied => "applied",
            BatchState::Partial => "partial",
            BatchState::Failed => "failed",
            BatchState::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, BatchState::Prepared | BatchState::Applying)
    }
}

impl fmt::Display for BatchState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BatchState {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "prepared" => BatchState::Prepared,
            "applying" => BatchState::Applying,
            "applied" => BatchState::Applied,
            "partial" => BatchState::Partial,
            "failed" => BatchState::Failed,
            "cancelled" => BatchState::Cancelled,
            other => return Err(format!("不明なバッチ状態: {other:?}")),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OpKind {
    Tags,
    Rename,
    Delete,
    Archive,
}

impl OpKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OpKind::Tags => "tags",
            OpKind::Rename => "rename",
            OpKind::Delete => "delete",
            OpKind::Archive => "archive",
        }
    }
}

impl FromStr for OpKind {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "tags" => OpKind::Tags,
            "rename" => OpKind::Rename,
            "delete" => OpKind::Delete,
            "archive" => OpKind::Archive,
            other => return Err(format!("不明な op 種別: {other:?}")),
        })
    }
}

/// op の結果。`Superseded` はスキーマに予約されているだけで P0 では使わない
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpResult {
    Pending,
    Applied,
    SkippedConflict,
    Failed,
    Superseded,
}

impl OpResult {
    pub fn as_str(self) -> &'static str {
        match self {
            OpResult::Pending => "pending",
            OpResult::Applied => "applied",
            OpResult::SkippedConflict => "skipped_conflict",
            OpResult::Failed => "failed",
            OpResult::Superseded => "superseded",
        }
    }
}

impl fmt::Display for OpResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OpResult {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "pending" => OpResult::Pending,
            "applied" => OpResult::Applied,
            "skipped_conflict" => OpResult::SkippedConflict,
            "failed" => OpResult::Failed,
            "superseded" => OpResult::Superseded,
            other => return Err(format!("不明な op 結果: {other:?}")),
        })
    }
}

/// キャンセルで閉じた op の `error` 値（バッチの `cancelled` 集計はこれを見る）
pub const CANCELLED_ERROR: &str = "cancelled";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Batch {
    pub id: i64,
    pub created_at: i64,
    pub description: Option<String>,
    pub state: BatchState,
    pub affected: Option<i64>,
    pub reverts_batch_id: Option<i64>,
    pub finished_at: Option<i64>,
    pub reverted_at: Option<i64>,
}

/// op のファイル反映の事前条件（記録時点の行の実体。SPEC §7.5）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Precondition {
    pub dev: Option<i64>,
    pub inode: Option<i64>,
    pub size: Option<i64>,
    pub mtime_ns: Option<i64>,
    pub ctime_ns: Option<i64>,
    pub tag_hash: Option<Vec<u8>>,
    pub rel_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Op {
    pub id: i64,
    pub batch_id: i64,
    pub ordinal: i64,
    pub track_id: i64,
    pub kind: OpKind,
    pub expected: Precondition,
    pub result: OpResult,
    pub error: Option<String>,
    pub job_id: Option<i64>,
    pub applied_at: Option<i64>,
}

/// フィールド値。`tags` op では値は文字列配列か JSON null（不存在）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub id: i64,
    pub op_id: i64,
    pub key: String,
    pub old_value: serde_json::Value,
    pub new_value: serde_json::Value,
}

fn parse_col<T: FromStr<Err = String>>(row: &Row<'_>, idx: usize) -> rusqlite::Result<T> {
    let s: String = row.get(idx)?;
    s.parse().map_err(|e: String| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(e)),
        )
    })
}

fn json_col(row: &Row<'_>, idx: usize) -> rusqlite::Result<serde_json::Value> {
    let s: String = row.get(idx)?;
    serde_json::from_str(&s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
    })
}

// ---------------------------------------------------------------- batch

const BATCH_COLUMNS: &str =
    "id, created_at, description, state, affected, reverts_batch_id, finished_at, reverted_at";

fn batch_from_row(r: &Row<'_>) -> rusqlite::Result<Batch> {
    Ok(Batch {
        id: r.get(0)?,
        created_at: r.get(1)?,
        description: r.get(2)?,
        state: parse_col(r, 3)?,
        affected: r.get(4)?,
        reverts_batch_id: r.get(5)?,
        finished_at: r.get(6)?,
        reverted_at: r.get(7)?,
    })
}

pub fn insert_batch(
    conn: &Connection,
    description: Option<&str>,
    affected: i64,
    reverts_batch_id: Option<i64>,
    now: i64,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO edit_batches (created_at, description, state, affected, reverts_batch_id)
         VALUES (?1, ?2, 'prepared', ?3, ?4)",
        params![now, description, affected, reverts_batch_id],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn get_batch(conn: &Connection, id: i64) -> Result<Option<Batch>> {
    Ok(conn
        .query_row(
            &format!("SELECT {BATCH_COLUMNS} FROM edit_batches WHERE id = ?1"),
            [id],
            batch_from_row,
        )
        .optional()?)
}

/// 未終端（`prepared` / `applying`）のバッチ id
pub fn open_batch_ids(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM edit_batches WHERE state IN ('prepared','applying') ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<i64>>>()?)
}

/// `prepared` → `applying`。遷移したら `true`
pub fn mark_batch_applying(conn: &Connection, id: i64) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE edit_batches SET state = 'applying' WHERE id = ?1 AND state = 'prepared'",
        [id],
    )?;
    Ok(changed > 0)
}

/// op 結果の集計
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct BatchCounts {
    pub pending: i64,
    pub applied: i64,
    pub conflict: i64,
    pub failed: i64,
    /// `failed` のうち error = 'cancelled' のもの（`failed` に含まれる）
    pub cancelled: i64,
}

impl BatchCounts {
    pub fn total(&self) -> i64 {
        self.pending + self.applied + self.conflict + self.failed
    }
}

pub fn batch_counts(conn: &Connection, batch_id: i64) -> Result<BatchCounts> {
    Ok(conn.query_row(
        "SELECT
            count(*) FILTER (WHERE result = 'pending'),
            count(*) FILTER (WHERE result = 'applied'),
            count(*) FILTER (WHERE result = 'skipped_conflict'),
            count(*) FILTER (WHERE result = 'failed'),
            count(*) FILTER (WHERE result = 'failed' AND error = ?2)
         FROM edit_ops WHERE batch_id = ?1",
        params![batch_id, CANCELLED_ERROR],
        |r| {
            Ok(BatchCounts {
                pending: r.get(0)?,
                applied: r.get(1)?,
                conflict: r.get(2)?,
                failed: r.get(3)?,
                cancelled: r.get(4)?,
            })
        },
    )?)
}

/// 全 op が終端なら集計してバッチを終端状態にする（SPEC §7.5「全 op が終端になったら」）。
/// 遷移したら新しい状態を返す。pending が残っていれば `None`
pub fn aggregate_batch(conn: &Connection, batch_id: i64, now: i64) -> Result<Option<BatchState>> {
    let c = batch_counts(conn, batch_id)?;
    if c.pending > 0 {
        return Ok(None);
    }
    let state = if c.applied == c.total() {
        BatchState::Applied
    } else if c.cancelled > 0 {
        BatchState::Cancelled
    } else if c.applied == 0 {
        BatchState::Failed
    } else {
        BatchState::Partial
    };
    let changed = conn.execute(
        "UPDATE edit_batches SET state = ?2, finished_at = ?3
         WHERE id = ?1 AND state IN ('prepared','applying')",
        params![batch_id, state.as_str(), now],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    // 逆バッチが終端になったら、元バッチの applied op が逆バッチ群で全件 applied になったかを見て
    // reverted_at を立てる（SPEC §7.5「巻き戻し」）
    let reverts: Option<i64> = conn
        .query_row(
            "SELECT reverts_batch_id FROM edit_batches WHERE id = ?1",
            [batch_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    if let Some(orig) = reverts {
        mark_reverted_if_covered(conn, orig, now)?;
    }
    Ok(Some(state))
}

/// 元バッチ `orig` の applied op のうち、逆バッチ（`reverts_batch_id = orig`）で applied に
/// なっていない op が無ければ `reverted_at` を立てる。立てたら `true`
pub fn mark_reverted_if_covered(conn: &Connection, orig: i64, now: i64) -> Result<bool> {
    let uncovered: i64 = conn.query_row(
        "SELECT count(*) FROM edit_ops o
         WHERE o.batch_id = ?1 AND o.result = 'applied'
           AND NOT EXISTS (
             SELECT 1 FROM edit_ops r JOIN edit_batches b ON b.id = r.batch_id
             WHERE b.reverts_batch_id = ?1 AND r.result = 'applied' AND r.track_id = o.track_id)",
        [orig],
        |r| r.get(0),
    )?;
    if uncovered > 0 {
        return Ok(false);
    }
    let changed = conn.execute(
        "UPDATE edit_batches SET reverted_at = ?2 WHERE id = ?1 AND reverted_at IS NULL",
        params![orig, now],
    )?;
    Ok(changed > 0)
}

/// 元バッチ `orig` の applied op のうち、逆バッチで既に applied になっているトラック
pub fn reverted_track_ids(conn: &Connection, orig: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT r.track_id FROM edit_ops r JOIN edit_batches b ON b.id = r.batch_id
         WHERE b.reverts_batch_id = ?1 AND r.result = 'applied' ORDER BY r.track_id",
    )?;
    let rows = stmt.query_map([orig], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<i64>>>()?)
}

// ---------------------------------------------------------------- op

const OP_COLUMNS: &str = "id, batch_id, ordinal, track_id, kind,
     expected_dev, expected_inode, expected_size, expected_mtime_ns, expected_ctime_ns,
     expected_tag_hash, expected_rel_path, result, error, job_id, applied_at";

fn op_from_row(r: &Row<'_>) -> rusqlite::Result<Op> {
    Ok(Op {
        id: r.get(0)?,
        batch_id: r.get(1)?,
        ordinal: r.get(2)?,
        track_id: r.get(3)?,
        kind: parse_col(r, 4)?,
        expected: Precondition {
            dev: r.get(5)?,
            inode: r.get(6)?,
            size: r.get(7)?,
            mtime_ns: r.get(8)?,
            ctime_ns: r.get(9)?,
            tag_hash: r.get(10)?,
            rel_path: r.get(11)?,
        },
        result: parse_col(r, 12)?,
        error: r.get(13)?,
        job_id: r.get(14)?,
        applied_at: r.get(15)?,
    })
}

/// `tracks` の現在値から事前条件を作る。行が無ければ `None`
pub fn precondition_of_track(conn: &Connection, track_id: i64) -> Result<Option<Precondition>> {
    Ok(conn
        .query_row(
            "SELECT dev, inode, size, mtime_ns, ctime_ns, tag_hash, rel_path
             FROM tracks WHERE id = ?1",
            [track_id],
            |r| {
                Ok(Precondition {
                    dev: r.get(0)?,
                    inode: r.get(1)?,
                    size: r.get(2)?,
                    mtime_ns: r.get(3)?,
                    ctime_ns: r.get(4)?,
                    tag_hash: r.get(5)?,
                    rel_path: r.get(6)?,
                })
            },
        )
        .optional()?)
}

/// op を `pending` で記録する。同じトラックに pending があれば partial UNIQUE で失敗する
pub fn insert_op(
    conn: &Connection,
    batch_id: i64,
    ordinal: i64,
    track_id: i64,
    kind: OpKind,
    expected: &Precondition,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO edit_ops (batch_id, ordinal, track_id, kind, expected_dev, expected_inode,
                               expected_size, expected_mtime_ns, expected_ctime_ns,
                               expected_tag_hash, expected_rel_path)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            batch_id,
            ordinal,
            track_id,
            kind.as_str(),
            expected.dev,
            expected.inode,
            expected.size,
            expected.mtime_ns,
            expected.ctime_ns,
            expected.tag_hash,
            expected.rel_path,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn get_op(conn: &Connection, id: i64) -> Result<Option<Op>> {
    Ok(conn
        .query_row(
            &format!("SELECT {OP_COLUMNS} FROM edit_ops WHERE id = ?1"),
            [id],
            op_from_row,
        )
        .optional()?)
}

/// バッチの op を `ordinal` 順に
pub fn list_ops(conn: &Connection, batch_id: i64) -> Result<Vec<Op>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {OP_COLUMNS} FROM edit_ops WHERE batch_id = ?1 ORDER BY ordinal"
    ))?;
    let rows = stmt.query_map([batch_id], op_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// バッチの pending op
pub fn pending_ops(conn: &Connection, batch_id: i64) -> Result<Vec<Op>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {OP_COLUMNS} FROM edit_ops WHERE batch_id = ?1 AND result = 'pending'
         ORDER BY ordinal"
    ))?;
    let rows = stmt.query_map([batch_id], op_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// `track_ids` のうち pending の op を持つもの（昇順・重複なし）
pub fn pending_track_ids(conn: &Connection, track_ids: &[i64]) -> Result<Vec<i64>> {
    let json = serde_json::to_string(track_ids).map_err(|e| DbError::Internal(e.to_string()))?;
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT o.track_id FROM edit_ops o
         WHERE o.result = 'pending' AND o.track_id IN (SELECT value FROM json_each(?1))
         ORDER BY o.track_id",
    )?;
    let rows = stmt.query_map([json], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<i64>>>()?)
}

pub fn set_op_job(conn: &Connection, op_id: i64, job_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE edit_ops SET job_id = ?2 WHERE id = ?1",
        params![op_id, job_id],
    )?;
    Ok(())
}

/// op を終端にする。`pending` からのみ遷移し、遷移したら `true`
pub fn finish_op(
    conn: &Connection,
    op_id: i64,
    result: OpResult,
    error: Option<&str>,
    job_id: Option<i64>,
    now: i64,
) -> Result<bool> {
    debug_assert!(result != OpResult::Pending);
    let changed = conn.execute(
        "UPDATE edit_ops SET result = ?2, error = ?3, job_id = COALESCE(?4, job_id),
                             applied_at = ?5
         WHERE id = ?1 AND result = 'pending'",
        params![op_id, result.as_str(), error, job_id, now],
    )?;
    Ok(changed > 0)
}

// ---------------------------------------------------------------- edits

pub fn insert_edit(
    conn: &Connection,
    op_id: i64,
    key: &str,
    old_value: &serde_json::Value,
    new_value: &serde_json::Value,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO edits (op_id, key, old_value, new_value) VALUES (?1, ?2, ?3, ?4)",
        params![op_id, key, old_value.to_string(), new_value.to_string()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn list_edits(conn: &Connection, op_id: i64) -> Result<Vec<Edit>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, op_id, key, old_value, new_value FROM edits WHERE op_id = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([op_id], |r| {
        Ok(Edit {
            id: r.get(0)?,
            op_id: r.get(1)?,
            key: r.get(2)?,
            old_value: json_col(r, 3)?,
            new_value: json_col(r, 4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---------------------------------------------------------------- tracks 側の補助

/// `track_tags` の現在値を正規化タグ集合として読む
pub fn load_track_tags(conn: &Connection, track_id: i64) -> Result<crate::domain::tags::TagSet> {
    let mut stmt = conn.prepare_cached(
        "SELECT key, value FROM track_tags WHERE track_id = ?1 ORDER BY key, idx",
    )?;
    let rows = stmt.query_map([track_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(crate::domain::tags::normalize_tags(items))
}

/// タグ集合・キャッシュ列・`tag_hash`・版を置き換える（overlay の書き込みと解消の両方で使う）
pub fn set_track_tags(
    conn: &Connection,
    track_id: i64,
    tags: &crate::domain::tags::TagSet,
    tag_hash: Option<&[u8]>,
    cache: &super::scans::CacheColumns,
    tag_version: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE tracks SET tag_hash = ?2, tag_version = ?3, title = ?4, artist_display = ?5,
                albumartist = ?6, track_no = ?7, disc_no = ?8, date = ?9
         WHERE id = ?1",
        params![
            track_id,
            tag_hash,
            tag_version,
            cache.title,
            cache.artist_display,
            cache.albumartist,
            cache.track_no,
            cache.disc_no,
            cache.date,
        ],
    )?;
    super::scans::replace_tags(conn, track_id, tags)
}

/// 物理属性だけを更新する（`seen_*` は触らない）
pub fn set_track_physical(
    conn: &Connection,
    track_id: i64,
    ph: &super::scans::Physical,
) -> Result<()> {
    conn.execute(
        "UPDATE tracks SET dev = ?2, inode = ?3, nlink = ?4, size = ?5, mtime_ns = ?6, ctime_ns = ?7
         WHERE id = ?1",
        params![
            track_id,
            ph.dev as i64,
            ph.inode as i64,
            ph.nlink as i64,
            ph.size as i64,
            ph.mtime_ns,
            ph.ctime_ns
        ],
    )?;
    Ok(())
}

/// 記録時点の物理属性へ戻す（ファイルを読めないときの overlay 解消。`None` の列は触らない）
pub fn restore_track_precondition(
    conn: &Connection,
    track_id: i64,
    p: &Precondition,
) -> Result<()> {
    conn.execute(
        "UPDATE tracks SET dev = COALESCE(?2, dev), inode = COALESCE(?3, inode),
                size = COALESCE(?4, size), mtime_ns = COALESCE(?5, mtime_ns),
                ctime_ns = COALESCE(?6, ctime_ns)
         WHERE id = ?1",
        params![track_id, p.dev, p.inode, p.size, p.mtime_ns, p.ctime_ns],
    )?;
    Ok(())
}

pub fn track_tag_version(conn: &Connection, track_id: i64) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT tag_version FROM tracks WHERE id = ?1",
            [track_id],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn track_rel_path(conn: &Connection, track_id: i64) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT rel_path FROM tracks WHERE id = ?1",
            [track_id],
            |r| r.get(0),
        )
        .optional()?)
}

// ---------------------------------------------------------------- 一覧（API 用）

/// 一覧の上限。編集履歴はユーザ操作 1 回 = 1 行なので、数千行を超えることは想定しない
pub const LIST_LIMIT: usize = 1000;

/// `GET /api/history` の 1 行（SPEC §9）
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatchSummary {
    pub id: i64,
    pub created_at: i64,
    pub description: Option<String>,
    /// op の種別（バッチは単一種別）。op が無ければ None
    pub kind: Option<OpKind>,
    pub state: BatchState,
    pub affected: Option<i64>,
    pub applied: i64,
    pub conflict: i64,
    pub failed: i64,
    pub reverts_batch_id: Option<i64>,
    /// このバッチを戻した逆バッチ（`reverted_at` が立っているときだけ）
    pub reverted_by: Option<i64>,
    pub finished_at: Option<i64>,
    pub reverted_at: Option<i64>,
}

const SUMMARY_SQL: &str = "SELECT b.id, b.created_at, b.description, b.state, b.affected,
        b.reverts_batch_id, b.finished_at, b.reverted_at,
        (SELECT o.kind FROM edit_ops o WHERE o.batch_id = b.id ORDER BY o.ordinal LIMIT 1),
        (SELECT count(*) FROM edit_ops o WHERE o.batch_id = b.id AND o.result = 'applied'),
        (SELECT count(*) FROM edit_ops o WHERE o.batch_id = b.id AND o.result = 'skipped_conflict'),
        (SELECT count(*) FROM edit_ops o WHERE o.batch_id = b.id AND o.result = 'failed'),
        CASE WHEN b.reverted_at IS NULL THEN NULL ELSE
          (SELECT max(r.id) FROM edit_batches r
            WHERE r.reverts_batch_id = b.id
              AND EXISTS (SELECT 1 FROM edit_ops o WHERE o.batch_id = r.id AND o.result = 'applied'))
        END
     FROM edit_batches b";

fn summary_from_row(r: &Row<'_>) -> rusqlite::Result<BatchSummary> {
    let kind: Option<String> = r.get(8)?;
    let kind = match kind {
        Some(k) => Some(k.parse::<OpKind>().map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                8,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(e)),
            )
        })?),
        None => None,
    };
    Ok(BatchSummary {
        id: r.get(0)?,
        created_at: r.get(1)?,
        description: r.get(2)?,
        state: parse_col(r, 3)?,
        affected: r.get(4)?,
        reverts_batch_id: r.get(5)?,
        finished_at: r.get(6)?,
        reverted_at: r.get(7)?,
        kind,
        applied: r.get(9)?,
        conflict: r.get(10)?,
        failed: r.get(11)?,
        reverted_by: r.get(12)?,
    })
}

/// バッチ一覧（新しい順、`LIST_LIMIT` 件まで）
pub fn list_batches(conn: &Connection) -> Result<Vec<BatchSummary>> {
    let mut stmt = conn.prepare_cached(&format!("{SUMMARY_SQL} ORDER BY b.id DESC LIMIT ?1"))?;
    let rows = stmt.query_map([LIST_LIMIT as i64], summary_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn get_batch_summary(conn: &Connection, id: i64) -> Result<Option<BatchSummary>> {
    Ok(conn
        .query_row(
            &format!("{SUMMARY_SQL} WHERE b.id = ?1"),
            [id],
            summary_from_row,
        )
        .optional()?)
}

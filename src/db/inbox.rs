//! Inbox の承認キュー（`inbox_items` / `inbox_files`。SPEC §7.8、D-68、P2-10）。
//! 正は Inbox のファイルで、ここは走査が写したキャッシュ。状態は
//! `pending` → `approved` → `placing` → `placed` | `failed`、`rejected`

use rusqlite::{params, Connection, OptionalExtension as _};
use serde::{Deserialize, Serialize};

use super::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemState {
    Pending,
    Approved,
    Placing,
    Placed,
    Rejected,
    Failed,
}

impl ItemState {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemState::Pending => "pending",
            ItemState::Approved => "approved",
            ItemState::Placing => "placing",
            ItemState::Placed => "placed",
            ItemState::Rejected => "rejected",
            ItemState::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<ItemState> {
        Some(match s {
            "pending" => ItemState::Pending,
            "approved" => ItemState::Approved,
            "placing" => ItemState::Placing,
            "placed" => ItemState::Placed,
            "rejected" => ItemState::Rejected,
            "failed" => ItemState::Failed,
            _ => return None,
        })
    }
}

/// 1 件（音声ファイルのあるディレクトリ）
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    pub id: i64,
    pub rel_dir: String,
    pub state: ItemState,
    pub detected_at: i64,
    pub seen_at: i64,
    pub approved_at: Option<i64>,
    pub draft: Option<serde_json::Value>,
    pub error: Option<String>,
    pub placed_album_id: Option<i64>,
    pub placed_at: Option<i64>,
}

/// 件の中の音声ファイル 1 本
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRow {
    pub rel_path: String,
    pub inode: i64,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub codec: String,
    pub lossless: bool,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub channels: Option<u32>,
    pub duration_ms: Option<u64>,
    /// 正規化済みタグ（キーは大文字。多値は反復）
    pub tags: Vec<(String, String)>,
}

const ITEM_COLS: &str = "id, rel_dir, state, detected_at, seen_at, approved_at, draft, error, placed_album_id, placed_at";

fn row_to_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    let state: String = r.get(2)?;
    let draft: Option<String> = r.get(6)?;
    Ok(Item {
        id: r.get(0)?,
        rel_dir: r.get(1)?,
        state: ItemState::parse(&state).unwrap_or(ItemState::Failed),
        detected_at: r.get(3)?,
        seen_at: r.get(4)?,
        approved_at: r.get(5)?,
        draft: draft.and_then(|s| serde_json::from_str(&s).ok()),
        error: r.get(7)?,
        placed_album_id: r.get(8)?,
        placed_at: r.get(9)?,
    })
}

/// 全件（新しい順）
pub fn list(conn: &Connection) -> Result<Vec<Item>> {
    let mut st = conn.prepare(&format!(
        "SELECT {ITEM_COLS} FROM inbox_items ORDER BY detected_at DESC, id DESC"
    ))?;
    let rows = st
        .query_map([], row_to_item)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Item>> {
    Ok(conn
        .query_row(
            &format!("SELECT {ITEM_COLS} FROM inbox_items WHERE id = ?1"),
            [id],
            row_to_item,
        )
        .optional()?)
}

pub fn find_by_dir_key(conn: &Connection, key: &str) -> Result<Option<Item>> {
    Ok(conn
        .query_row(
            &format!("SELECT {ITEM_COLS} FROM inbox_items WHERE rel_dir_key = ?1"),
            [key],
            row_to_item,
        )
        .optional()?)
}

/// `state` の件（id 順）
pub fn list_by_state(conn: &Connection, state: ItemState) -> Result<Vec<Item>> {
    let mut st = conn.prepare(&format!(
        "SELECT {ITEM_COLS} FROM inbox_items WHERE state = ?1 ORDER BY id"
    ))?;
    let rows = st
        .query_map([state.as_str()], row_to_item)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn insert_item(conn: &Connection, rel_dir: &str, rel_dir_key: &str, now: i64) -> Result<i64> {
    conn.execute(
        "INSERT INTO inbox_items (rel_dir, rel_dir_key, state, detected_at, seen_at)
         VALUES (?1, ?2, 'pending', ?3, ?3)",
        params![rel_dir, rel_dir_key, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 走査で見た（`seen_at`）
pub fn touch(conn: &Connection, id: i64, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE inbox_items SET seen_at = ?2 WHERE id = ?1",
        params![id, now],
    )?;
    Ok(())
}

pub fn files(conn: &Connection, item_id: i64) -> Result<Vec<FileRow>> {
    let mut st = conn.prepare(
        "SELECT rel_path, inode, size, mtime_ns, ctime_ns, codec, lossless, sample_rate, bit_depth,
                channels, duration_ms, tags
           FROM inbox_files WHERE item_id = ?1 ORDER BY rel_path",
    )?;
    let rows = st
        .query_map([item_id], |r| {
            let tags: String = r.get(11)?;
            Ok(FileRow {
                rel_path: r.get(0)?,
                inode: r.get(1)?,
                size: r.get(2)?,
                mtime_ns: r.get(3)?,
                ctime_ns: r.get(4)?,
                codec: r.get(5)?,
                lossless: r.get::<_, i64>(6)? == 1,
                sample_rate: r.get(7)?,
                bit_depth: r.get(8)?,
                channels: r.get(9)?,
                duration_ms: r.get::<_, Option<i64>>(10)?.map(|d| d as u64),
                tags: serde_json::from_str(&tags).unwrap_or_default(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 件のファイルを入れ替える（`rel_path_key` は `rel_path` から作る）
pub fn replace_files(conn: &Connection, item_id: i64, files: &[FileRow]) -> Result<()> {
    conn.execute("DELETE FROM inbox_files WHERE item_id = ?1", [item_id])?;
    let mut st = conn.prepare_cached(
        "INSERT INTO inbox_files (item_id, rel_path, rel_path_key, inode, size, mtime_ns, ctime_ns,
                                  codec, lossless, sample_rate, bit_depth, channels, duration_ms, tags)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
    )?;
    for f in files {
        let tags = serde_json::to_string(&f.tags)
            .map_err(|e| super::DbError::Internal(format!("タグを JSON にできない: {e}")))?;
        st.execute(params![
            item_id,
            f.rel_path,
            crate::domain::relpath::canonical_key(&f.rel_path),
            f.inode,
            f.size,
            f.mtime_ns,
            f.ctime_ns,
            f.codec,
            f.lossless as i64,
            f.sample_rate,
            f.bit_depth,
            f.channels,
            f.duration_ms.map(|d| d as i64),
            tags,
        ])?;
    }
    Ok(())
}

/// 状態を変える。`approved` にすると `approved_at`、それ以外に戻すと `error` を置き換える
pub fn set_state(
    conn: &Connection,
    id: i64,
    state: ItemState,
    error: Option<&str>,
    now: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE inbox_items
            SET state = ?2, error = ?3,
                approved_at = CASE WHEN ?2 = 'approved' THEN ?4 ELSE approved_at END
          WHERE id = ?1",
        params![id, state.as_str(), error, now],
    )?;
    Ok(())
}

pub fn set_draft(conn: &Connection, id: i64, draft: &serde_json::Value) -> Result<()> {
    conn.execute(
        "UPDATE inbox_items SET draft = ?2 WHERE id = ?1",
        params![id, draft.to_string()],
    )?;
    Ok(())
}

/// 配置済み
pub fn set_placed(conn: &Connection, id: i64, album_id: i64, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE inbox_items SET state = 'placed', error = NULL, placed_album_id = ?2, placed_at = ?3
          WHERE id = ?1",
        params![id, album_id, now],
    )?;
    Ok(())
}

pub fn delete_item(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM inbox_items WHERE id = ?1", [id])?;
    Ok(())
}

/// 走査で見なかった件（`seen_at < seen_before`）のうち `placed` 以外
pub fn stale_items(conn: &Connection, seen_before: i64) -> Result<Vec<Item>> {
    let mut st = conn.prepare(&format!(
        "SELECT {ITEM_COLS} FROM inbox_items WHERE seen_at < ?1 AND state <> 'placed' ORDER BY id"
    ))?;
    let rows = st
        .query_map([seen_before], row_to_item)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// `placed` で `placed_at < placed_before` の件を消す。返り値は消した数
pub fn expire_placed(conn: &Connection, placed_before: i64) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM inbox_items WHERE state = 'placed' AND placed_at IS NOT NULL AND placed_at < ?1",
        [placed_before],
    )?)
}

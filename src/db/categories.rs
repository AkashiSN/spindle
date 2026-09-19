//! 統制語彙 `categories`（SPEC §5「Category」、D-67）。名前の一意性は canonical key で見る
//! （ZFS insensitive と同じ扱い。`J-Pop` と `j-pop` は同じ語彙）

use rusqlite::{params, Connection};
use serde::Serialize;

use super::Result;
use crate::domain::relpath::canonical_key;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Category {
    pub id: i64,
    pub name: String,
}

pub fn list(conn: &Connection) -> Result<Vec<Category>> {
    let mut st = conn.prepare("SELECT id, name FROM categories ORDER BY sort_key, name")?;
    let rows = st
        .query_map([], |r| {
            Ok(Category {
                id: r.get(0)?,
                name: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// canonical key が `name` と同じ語彙の id（表記は違ってもよい）
pub fn find_by_key(conn: &Connection, name: &str) -> Result<Option<Category>> {
    let key = canonical_key(name);
    Ok(list(conn)?
        .into_iter()
        .find(|c| canonical_key(&c.name) == key))
}

/// 同じ canonical key の語彙があればその id、無ければ追加して id を返す（メタデータプラグインの
/// category など、外部の定義が正のとき。D-69）
pub fn ensure(conn: &Connection, name: &str) -> Result<i64> {
    if let Some(c) = find_by_key(conn, name)? {
        return Ok(c.id);
    }
    conn.execute("INSERT INTO categories (name) VALUES (?1)", params![name])?;
    Ok(conn.last_insert_rowid())
}

/// 追加する。canonical key が同じ語彙が既にあれば `None`
pub fn insert(conn: &Connection, name: &str) -> Result<Option<i64>> {
    if find_by_key(conn, name)?.is_some() {
        return Ok(None);
    }
    conn.execute("INSERT INTO categories (name) VALUES (?1)", params![name])?;
    Ok(Some(conn.last_insert_rowid()))
}

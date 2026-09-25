//! 統制語彙 `categories`（SPEC §5「Category」、D-67）。名前の一意性は canonical key で見る
//! （ZFS insensitive と同じ扱い。`J-Pop` と `j-pop` は同じ語彙）

use rusqlite::{params, Connection, OptionalExtension};
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

/// [`delete_if_unused`] の結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteOutcome {
    Deleted,
    NotFound,
    /// 使われている。中身は理由（画面に出す）
    InUse(String),
}

/// 使われていない語彙だけを消す（D-92）。自動登録は追加しかしないので、SMB で一時的に作った Library 直下の
/// フォルダの名前などを人が消す経路。次のどれかがあれば消さない:
/// - active な album（`missing_since IS NULL`）の category（missing の album は ON DELETE SET NULL で外れる）
/// - GENRE の写像（`genre_category_map`。CASCADE で黙って消えるので明示的に外してもらう）
/// - 再生リストの購読の category（名前で持つ。canonical key で照合）
/// - 承認前・承認済みの Inbox の取り込みの保存済みの下書きの category（名前で持つ）
///
/// 呼び出し側は書き込みのトランザクションの中で呼ぶ（確かめと削除の間に参照が増えない）
pub fn delete_if_unused(conn: &Connection, id: i64) -> Result<DeleteOutcome> {
    let name: Option<String> = conn
        .query_row(
            "SELECT name FROM categories WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(name) = name else {
        return Ok(DeleteOutcome::NotFound);
    };
    let key = canonical_key(&name);
    let albums: i64 = conn.query_row(
        "SELECT count(*) FROM albums WHERE category_id = ?1 AND missing_since IS NULL",
        params![id],
        |r| r.get(0),
    )?;
    if albums > 0 {
        return Ok(DeleteOutcome::InUse(format!(
            "アルバム {albums} 件の category"
        )));
    }
    let genres: i64 = conn.query_row(
        "SELECT count(*) FROM genre_category_map WHERE category_id = ?1",
        params![id],
        |r| r.get(0),
    )?;
    if genres > 0 {
        return Ok(DeleteOutcome::InUse(format!("GENRE の写像 {genres} 件")));
    }
    let mut st =
        conn.prepare("SELECT category FROM playlist_subscriptions WHERE category IS NOT NULL")?;
    let subs = st
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let n = subs.iter().filter(|c| canonical_key(c) == key).count();
    if n > 0 {
        return Ok(DeleteOutcome::InUse(format!("再生リストの購読 {n} 件")));
    }
    let mut st = conn.prepare(
        "SELECT json_extract(draft, '$.category') FROM inbox_items
          WHERE draft IS NOT NULL AND json_valid(draft)
            AND state IN ('pending', 'approved', 'placing', 'failed')",
    )?;
    let drafts = st
        .query_map([], |r| r.get::<_, Option<String>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let n = drafts
        .iter()
        .flatten()
        .filter(|c| canonical_key(c) == key)
        .count();
    if n > 0 {
        return Ok(DeleteOutcome::InUse(format!(
            "Inbox の取り込みの下書き {n} 件"
        )));
    }
    conn.execute("DELETE FROM categories WHERE id = ?1", params![id])?;
    Ok(DeleteOutcome::Deleted)
}

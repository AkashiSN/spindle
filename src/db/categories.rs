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

/// 語彙の使用数（`GET /api/categories` と [`delete_if_unused`] が同じ数え方を使う。D-92）
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct Usage {
    /// active な album（`missing_since IS NULL`）
    pub albums: i64,
    /// GENRE の写像（`genre_category_map`）
    pub genres: i64,
    /// 再生リストの購読（名前で持つ。canonical key で照合）
    pub subscriptions: i64,
    /// 承認前・承認済みの Inbox の取り込みの保存済みの下書き（名前で持つ）
    pub inbox_drafts: i64,
}

impl Usage {
    /// 使われていれば理由（削除の 409 と画面に出す）。最初に当たった 1 つだけ
    pub fn in_use_reason(&self) -> Option<String> {
        if self.albums > 0 {
            Some(format!("アルバム {} 件の category", self.albums))
        } else if self.genres > 0 {
            Some(format!("GENRE の写像 {} 件", self.genres))
        } else if self.subscriptions > 0 {
            Some(format!("再生リストの購読 {} 件", self.subscriptions))
        } else if self.inbox_drafts > 0 {
            Some(format!("Inbox の取り込みの下書き {} 件", self.inbox_drafts))
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CategoryUsage {
    #[serde(flatten)]
    pub category: Category,
    #[serde(flatten)]
    pub usage: Usage,
}

pub fn usage(conn: &Connection, id: i64, name: &str) -> Result<Usage> {
    let key = canonical_key(name);
    let albums: i64 = conn.query_row(
        "SELECT count(*) FROM albums WHERE category_id = ?1 AND missing_since IS NULL",
        params![id],
        |r| r.get(0),
    )?;
    let genres: i64 = conn.query_row(
        "SELECT count(*) FROM genre_category_map WHERE category_id = ?1",
        params![id],
        |r| r.get(0),
    )?;
    let mut st = conn
        .prepare_cached("SELECT category FROM playlist_subscriptions WHERE category IS NOT NULL")?;
    let subscriptions = st
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .filter(|c| canonical_key(c) == key)
        .count() as i64;
    let mut st = conn.prepare_cached(
        "SELECT json_extract(draft, '$.category') FROM inbox_items
          WHERE draft IS NOT NULL AND json_valid(draft)
            AND state IN ('pending', 'approved', 'placing', 'failed')",
    )?;
    let inbox_drafts = st
        .query_map([], |r| r.get::<_, Option<String>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .flatten()
        .filter(|c| canonical_key(c) == key)
        .count() as i64;
    Ok(Usage {
        albums,
        genres,
        subscriptions,
        inbox_drafts,
    })
}

/// 一覧に使用数を添える（語彙は十数件なので 1 件ずつ数える）
pub fn list_with_usage(conn: &Connection) -> Result<Vec<CategoryUsage>> {
    list(conn)?
        .into_iter()
        .map(|c| {
            let usage = usage(conn, c.id, &c.name)?;
            Ok(CategoryUsage { category: c, usage })
        })
        .collect()
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
    if let Some(reason) = usage(conn, id, &name)?.in_use_reason() {
        return Ok(DeleteOutcome::InUse(reason));
    }
    conn.execute("DELETE FROM categories WHERE id = ?1", params![id])?;
    Ok(DeleteOutcome::Deleted)
}

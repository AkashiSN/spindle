//! プレイリスト（`playlists` / `playlist_items` / `playlist_exports` / `export_profiles`。
//! SPEC §10、docs/TASKS.md P1-6、D-53）。
//!
//! `playlist_items.position` は 0 から連続。追加・除外・移動のたびに振り直す（1 プレイリストは
//! 高々数千件なので全行の書き直しで足りる）。同じトラックは 1 プレイリストに 1 回だけ

use std::collections::{HashMap, HashSet};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::domain::relpath::canonical_key;
use crate::playlist::export::{ExportProfile, ExportTrack, PathStyle, Source};
use crate::playlist::import::Candidate;

use super::Result;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    /// `manual` / `smart`
    pub kind: String,
    pub auto_export: bool,
    pub created_at: i64,
    pub updated_at: i64,
    /// 項目数（missing を含む）
    pub track_count: i64,
    /// うち missing
    pub missing_count: i64,
    /// active な項目の合計（ms）
    pub duration_ms: i64,
    pub exports: Vec<ExportRecord>,
}

/// `playlist_exports` の 1 行
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportRecord {
    pub profile: String,
    /// Playlists root からの相対パス
    pub out_path: String,
    pub exported_at: Option<i64>,
}

/// `export_profiles` の 1 行
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRow {
    pub id: i64,
    pub name: String,
    /// `m3u8` / `pls` / `fb2k_query`
    pub format: String,
    pub profile: ExportProfile,
}

const PLAYLIST_COLUMNS: &str = "p.id, p.name, p.kind, p.auto_export, p.created_at, p.updated_at,
  (SELECT count(*) FROM playlist_items i WHERE i.playlist_id = p.id),
  (SELECT count(*) FROM playlist_items i JOIN tracks t ON t.id = i.track_id
     WHERE i.playlist_id = p.id AND t.missing_since IS NOT NULL),
  (SELECT coalesce(sum(t.duration_ms), 0) FROM playlist_items i JOIN tracks t ON t.id = i.track_id
     WHERE i.playlist_id = p.id AND t.missing_since IS NULL)";

fn playlist_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<Playlist> {
    Ok(Playlist {
        id: r.get(0)?,
        name: r.get(1)?,
        kind: r.get(2)?,
        auto_export: r.get(3)?,
        created_at: r.get(4)?,
        updated_at: r.get(5)?,
        track_count: r.get(6)?,
        missing_count: r.get(7)?,
        duration_ms: r.get(8)?,
        exports: Vec::new(),
    })
}

fn attach_exports(conn: &Connection, rows: &mut [Playlist]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut st = conn.prepare_cached(
        "SELECT e.playlist_id, x.name, e.out_path, e.exported_at
         FROM playlist_exports e JOIN export_profiles x ON x.id = e.profile_id
         ORDER BY e.playlist_id, x.id",
    )?;
    let mut by_id: HashMap<i64, Vec<ExportRecord>> = HashMap::new();
    for r in st.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            ExportRecord {
                profile: r.get(1)?,
                out_path: r.get(2)?,
                exported_at: r.get(3)?,
            },
        ))
    })? {
        let (id, rec) = r?;
        by_id.entry(id).or_default().push(rec);
    }
    for p in rows {
        if let Some(v) = by_id.remove(&p.id) {
            p.exports = v;
        }
    }
    Ok(())
}

/// 全プレイリスト（名前順）
pub fn list(conn: &Connection) -> Result<Vec<Playlist>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT {PLAYLIST_COLUMNS} FROM playlists p ORDER BY p.name, p.id"
    ))?;
    let mut rows = st
        .query_map([], playlist_of)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    attach_exports(conn, &mut rows)?;
    Ok(rows)
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Playlist>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT {PLAYLIST_COLUMNS} FROM playlists p WHERE p.id = ?"
    ))?;
    let Some(mut p) = st.query_row([id], playlist_of).optional()? else {
        return Ok(None);
    };
    attach_exports(conn, std::slice::from_mut(&mut p))?;
    Ok(Some(p))
}

/// 手動プレイリストを作る。名前（canonical key で比較。D-53）が使われていれば `None`
pub fn create(conn: &Connection, name: &str, now: i64) -> Result<Option<Playlist>> {
    let n = conn.execute(
        "INSERT INTO playlists (name, name_key, kind, created_at, updated_at)
         VALUES (?1, ?2, 'manual', ?3, ?3)
         ON CONFLICT DO NOTHING",
        params![name, canonical_key(name), now],
    )?;
    if n == 0 {
        return Ok(None);
    }
    get(conn, conn.last_insert_rowid())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rename {
    Ok,
    NotFound,
    Duplicate,
}

pub fn rename(conn: &Connection, id: i64, name: &str, now: i64) -> Result<Rename> {
    if !exists(conn, id)? {
        return Ok(Rename::NotFound);
    }
    let key = canonical_key(name);
    let taken: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM playlists WHERE (name = ?1 OR name_key = ?2) AND id <> ?3)",
        params![name, key, id],
        |r| r.get(0),
    )?;
    if taken {
        return Ok(Rename::Duplicate);
    }
    conn.execute(
        "UPDATE playlists SET name = ?1, name_key = ?2, updated_at = ?3 WHERE id = ?4",
        params![name, key, now, id],
    )?;
    Ok(Rename::Ok)
}

pub fn set_auto_export(conn: &Connection, id: i64, on: bool, now: i64) -> Result<bool> {
    let n = conn.execute(
        "UPDATE playlists SET auto_export = ?1, updated_at = ?2 WHERE id = ?3",
        params![on, now, id],
    )?;
    Ok(n > 0)
}

/// 消えたら true。項目と書き出し記録は CASCADE で消える
pub fn delete(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.execute("DELETE FROM playlists WHERE id = ?", [id])? > 0)
}

fn exists(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM playlists WHERE id = ?)",
        [id],
        |r| r.get(0),
    )?)
}

/// 項目のトラック id（position 順）
pub fn items(conn: &Connection, id: i64) -> Result<Vec<i64>> {
    let mut st = conn.prepare_cached(
        "SELECT track_id FROM playlist_items WHERE playlist_id = ? ORDER BY position",
    )?;
    let rows = st
        .query_map([id], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 項目を `order` の並びで書き直す（position を 0 から振り直す）
fn rewrite_items(conn: &Connection, id: i64, order: &[i64], now: i64) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM playlist_items WHERE playlist_id = ?", [id])?;
    {
        let mut st = tx.prepare_cached(
            "INSERT INTO playlist_items (playlist_id, position, track_id) VALUES (?1, ?2, ?3)",
        )?;
        for (pos, track_id) in order.iter().enumerate() {
            st.execute(params![id, pos as i64, track_id])?;
        }
    }
    tx.execute(
        "UPDATE playlists SET updated_at = ?1 WHERE id = ?2",
        params![now, id],
    )?;
    tx.commit()?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Appended {
    pub added: usize,
    /// 既にあった・入力内で重複した・存在しないトラック
    pub skipped: usize,
}

/// 末尾に追加する。同じトラックは 1 回だけ、存在しないトラックは飛ばす。プレイリストが無ければ `None`
pub fn append(conn: &Connection, id: i64, track_ids: &[i64], now: i64) -> Result<Option<Appended>> {
    if !exists(conn, id)? {
        return Ok(None);
    }
    let mut order = items(conn, id)?;
    let mut present: HashSet<i64> = order.iter().copied().collect();
    let existing = existing_tracks(conn, track_ids)?;
    let mut added = 0;
    for &t in track_ids {
        if existing.contains(&t) && present.insert(t) {
            order.push(t);
            added += 1;
        }
    }
    if added > 0 {
        rewrite_items(conn, id, &order, now)?;
    }
    Ok(Some(Appended {
        added,
        skipped: track_ids.len() - added,
    }))
}

/// `tracks` に存在する id の集合
fn existing_tracks(conn: &Connection, ids: &[i64]) -> Result<HashSet<i64>> {
    let json = serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_owned());
    let mut st = conn.prepare_cached(
        "SELECT t.id FROM tracks t WHERE t.id IN (SELECT value FROM json_each(?))",
    )?;
    let rows = st
        .query_map([json], |r| r.get(0))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    Ok(rows)
}

/// 指定トラックを外す。外した件数。プレイリストが無ければ `None`
pub fn remove(conn: &Connection, id: i64, track_ids: &[i64], now: i64) -> Result<Option<usize>> {
    if !exists(conn, id)? {
        return Ok(None);
    }
    let drop: HashSet<i64> = track_ids.iter().copied().collect();
    let before = items(conn, id)?;
    let after: Vec<i64> = before
        .iter()
        .copied()
        .filter(|t| !drop.contains(t))
        .collect();
    let n = before.len() - after.len();
    if n > 0 {
        rewrite_items(conn, id, &after, now)?;
    }
    Ok(Some(n))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MoveError {
    #[error("移動先のトラックがプレイリストに無い")]
    BeforeNotInPlaylist,
    #[error("移動先が移動する集合の中にある")]
    BeforeInMovedSet,
}

/// `track_ids` を `before` の直前（`None` なら末尾）へ動かす。移動する側は現在の相対順を保つ。
/// 集合に無いトラックは無視。プレイリストが無ければ `None`
pub fn move_items(
    conn: &Connection,
    id: i64,
    track_ids: &[i64],
    before: Option<i64>,
    now: i64,
) -> Result<Option<std::result::Result<(), MoveError>>> {
    if !exists(conn, id)? {
        return Ok(None);
    }
    let current = items(conn, id)?;
    let moving: HashSet<i64> = track_ids.iter().copied().collect();
    if let Some(b) = before {
        if moving.contains(&b) {
            return Ok(Some(Err(MoveError::BeforeInMovedSet)));
        }
        if !current.contains(&b) {
            return Ok(Some(Err(MoveError::BeforeNotInPlaylist)));
        }
    }
    let moved: Vec<i64> = current
        .iter()
        .copied()
        .filter(|t| moving.contains(t))
        .collect();
    if moved.is_empty() {
        return Ok(Some(Ok(())));
    }
    let mut order = Vec::with_capacity(current.len());
    for &t in &current {
        if moving.contains(&t) {
            continue;
        }
        if before == Some(t) {
            order.extend_from_slice(&moved);
        }
        order.push(t);
    }
    if before.is_none() {
        order.extend_from_slice(&moved);
    }
    if order != current {
        rewrite_items(conn, id, &order, now)?;
    }
    Ok(Some(Ok(())))
}

fn profile_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<ProfileRow> {
    let source: String = r.get(3)?;
    let style: String = r.get(4)?;
    let invalid = |col: usize, v: &str| {
        rusqlite::Error::FromSqlConversionFailure(
            col,
            rusqlite::types::Type::Text,
            format!("export_profiles の値が不正: {v}").into(),
        )
    };
    Ok(ProfileRow {
        id: r.get(0)?,
        name: r.get(1)?,
        format: r.get(2)?,
        profile: ExportProfile {
            name: r.get(1)?,
            source: Source::parse(&source).ok_or_else(|| invalid(3, &source))?,
            path_style: PathStyle::parse(&style).ok_or_else(|| invalid(4, &style))?,
            path_prefix: r.get(5)?,
            path_sep: r.get(6)?,
        },
    })
}

const PROFILE_COLUMNS: &str = "id, name, format, source, path_style, path_prefix, path_sep";

pub fn profiles(conn: &Connection) -> Result<Vec<ProfileRow>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT {PROFILE_COLUMNS} FROM export_profiles ORDER BY id"
    ))?;
    let rows = st
        .query_map([], profile_of)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn profile_by_name(conn: &Connection, name: &str) -> Result<Option<ProfileRow>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT {PROFILE_COLUMNS} FROM export_profiles WHERE name = ?"
    ))?;
    Ok(st.query_row([name], profile_of).optional()?)
}

/// 書き出す行（position 順、missing は除く）と除いた件数。プレイリストが無ければ `None`
pub fn export_tracks(
    conn: &Connection,
    id: i64,
    source: Source,
) -> Result<Option<(Vec<ExportTrack>, usize)>> {
    if !exists(conn, id)? {
        return Ok(None);
    }
    let path_expr = match source {
        Source::Master => "'Library/' || t.rel_path",
        // delivery ビューは missing を含まない（missing はどのみち書かないので原本で埋める）
        Source::Delivery => {
            "coalesce((SELECT v.path FROM delivery v WHERE v.track_id = t.id), 'Library/' || t.rel_path)"
        }
    };
    let mut st = conn.prepare_cached(&format!(
        "SELECT {path_expr}, t.title, t.artist_display, t.duration_ms, t.missing_since IS NOT NULL
         FROM playlist_items i JOIN tracks t ON t.id = i.track_id
         WHERE i.playlist_id = ? ORDER BY i.position"
    ))?;
    let mut rows = Vec::new();
    let mut skipped = 0;
    for r in st.query_map([id], |r| {
        Ok((
            ExportTrack {
                path: r.get(0)?,
                title: r.get(1)?,
                artist: r.get(2)?,
                duration_ms: r.get(3)?,
            },
            r.get::<_, bool>(4)?,
        ))
    })? {
        let (t, missing) = r?;
        if missing {
            skipped += 1;
        } else {
            rows.push(t);
        }
    }
    Ok(Some((rows, skipped)))
}

/// 書き出しの記録（同じプロファイルは上書き）
pub fn record_export(
    conn: &Connection,
    playlist_id: i64,
    profile_id: i64,
    out_path: &str,
    now: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO playlist_exports (playlist_id, profile_id, out_path, exported_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(playlist_id, profile_id) DO UPDATE SET out_path = excluded.out_path,
           exported_at = excluded.exported_at",
        params![playlist_id, profile_id, out_path, now],
    )?;
    Ok(())
}

/// 取り込みの解決に使う全トラック
pub fn import_candidates(conn: &Connection) -> Result<Vec<Candidate>> {
    let mut st =
        conn.prepare_cached("SELECT id, rel_path_key, missing_since IS NULL FROM tracks")?;
    let rows = st
        .query_map([], |r| {
            Ok(Candidate {
                track_id: r.get(0)?,
                rel_path_key: r.get(1)?,
                active: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

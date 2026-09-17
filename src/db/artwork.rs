//! アートワークの表（`artwork`）と album の解決状態（P1-3、SPEC §7.1、D-49）。
//!
//! `artwork` は画像バイト列の SHA-256 でハッシュアドレスする。行は消さない（参照が無くなった
//! 行の回収は GC の仕事）。album 側は `artwork_id` に加えて、同梱カバー画像の stat（変更検出用）
//! と `artwork_resolved_at`（NULL なら次のスキャンで必ず解決する）を持つ

use rusqlite::{params, Connection, OptionalExtension};

use super::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artwork {
    pub id: i64,
    pub sha256: Vec<u8>,
    pub mime: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub bytes: i64,
    /// `embedded` / `file`
    pub origin: String,
}

const ARTWORK_COLUMNS: &str = "id, sha256, mime, width, height, bytes, origin";

fn artwork_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<Artwork> {
    Ok(Artwork {
        id: r.get(0)?,
        sha256: r.get(1)?,
        mime: r.get(2)?,
        width: r.get(3)?,
        height: r.get(4)?,
        bytes: r.get(5)?,
        origin: r.get(6)?,
    })
}

/// 画像を登録する（同じハッシュがあればその id。`origin` は最初に見つけた出自のまま）
pub fn upsert(
    conn: &Connection,
    sha256: &[u8],
    mime: &str,
    width: Option<u32>,
    height: Option<u32>,
    bytes: usize,
    origin: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO artwork (sha256, mime, width, height, bytes, origin)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(sha256) DO NOTHING",
        params![sha256, mime, width, height, bytes as i64, origin],
    )?;
    let id: i64 = conn.query_row("SELECT id FROM artwork WHERE sha256 = ?1", [sha256], |r| {
        r.get(0)
    })?;
    Ok(id)
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Artwork>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT {ARTWORK_COLUMNS} FROM artwork WHERE id = ?1"
    ))?;
    Ok(st.query_row([id], artwork_of).optional()?)
}

pub fn get_by_sha256(conn: &Connection, sha256: &[u8]) -> Result<Option<Artwork>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT {ARTWORK_COLUMNS} FROM artwork WHERE sha256 = ?1"
    ))?;
    Ok(st.query_row([sha256], artwork_of).optional()?)
}

// ---------------------------------------------------------------- album 側

/// 同梱カバー画像の stat（トラックの最速パスと同じ規則で変更を検出する）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoverStat {
    pub inode: i64,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

/// album の解決状態
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumArtworkState {
    pub id: i64,
    pub rel_dir: String,
    pub rel_dir_key: String,
    pub artwork_id: Option<i64>,
    /// 参照中の画像のハッシュと MIME（原画像がキャッシュに残っているかの確認用）
    pub artwork_sha256: Option<Vec<u8>>,
    pub artwork_mime: Option<String>,
    /// 参照中の画像の長さ（原画像の欠損・破損の検出用）
    pub artwork_bytes: Option<i64>,
    /// 前回解決したときの同梱カバー画像（無ければ None）
    pub cover: Option<CoverStat>,
    pub resolved_at: Option<i64>,
    pub missing: bool,
}

/// 全 album の解決状態
pub fn album_states(conn: &Connection) -> Result<Vec<AlbumArtworkState>> {
    let mut st = conn.prepare_cached(
        "SELECT a.id, a.rel_dir, a.rel_dir_key, a.artwork_id, a.cover_inode, a.cover_size,
                a.cover_mtime_ns, a.cover_ctime_ns, a.artwork_resolved_at,
                a.missing_since IS NOT NULL, w.sha256, w.mime, w.bytes
           FROM albums a LEFT JOIN artwork w ON w.id = a.artwork_id",
    )?;
    let rows = st
        .query_map([], |r| {
            let cover = match (
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, Option<i64>>(7)?,
            ) {
                (Some(inode), Some(size), Some(mtime_ns), Some(ctime_ns)) => Some(CoverStat {
                    inode,
                    size,
                    mtime_ns,
                    ctime_ns,
                }),
                _ => None,
            };
            Ok(AlbumArtworkState {
                id: r.get(0)?,
                rel_dir: r.get(1)?,
                rel_dir_key: r.get(2)?,
                artwork_id: r.get(3)?,
                artwork_sha256: r.get(10)?,
                artwork_mime: r.get(11)?,
                artwork_bytes: r.get(12)?,
                cover,
                resolved_at: r.get(8)?,
                missing: r.get(9)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// album の再解決を予約する（`artwork_resolved_at = NULL`。次のスキャンの Phase 5 が対象にする）
pub fn mark_unresolved(conn: &Connection, album_ids: &[i64]) -> Result<usize> {
    let mut st =
        conn.prepare_cached("UPDATE albums SET artwork_resolved_at = NULL WHERE id = ?1")?;
    let mut n = 0;
    for id in album_ids {
        n += st.execute([id])?;
    }
    Ok(n)
}

/// active な album 全件の再解決を予約する（deep scan）
pub fn mark_all_unresolved(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE albums SET artwork_resolved_at = NULL WHERE missing_since IS NULL",
        [],
    )?)
}

/// `artwork_id` を参照する album の再解決を予約する（原画像がキャッシュから消えていたとき）
pub fn mark_unresolved_by_artwork(conn: &Connection, artwork_id: i64) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE albums SET artwork_resolved_at = NULL WHERE artwork_id = ?1",
        [artwork_id],
    )?)
}

/// album の解決結果を書く。`artwork_id` が None なら外す。`cover` は今回見つけた同梱画像の stat
pub fn set_album_artwork(
    conn: &Connection,
    album_id: i64,
    artwork_id: Option<i64>,
    cover: Option<&CoverStat>,
    now: i64,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE albums
            SET artwork_id = ?2, cover_inode = ?3, cover_size = ?4, cover_mtime_ns = ?5,
                cover_ctime_ns = ?6, artwork_resolved_at = ?7
          WHERE id = ?1",
        params![
            album_id,
            artwork_id,
            cover.map(|c| c.inode),
            cover.map(|c| c.size),
            cover.map(|c| c.mtime_ns),
            cover.map(|c| c.ctime_ns),
            now
        ],
    )?;
    Ok(n > 0)
}

/// album の active な構成トラックのパス（disc_no / track_no / rel_path 順。埋め込み画像の探索順）
pub fn album_track_paths(conn: &Connection, album_id: i64) -> Result<Vec<String>> {
    let mut st = conn.prepare_cached(
        "SELECT rel_path FROM tracks
          WHERE album_id = ?1 AND missing_since IS NULL
          ORDER BY disc_no IS NULL, disc_no, track_no IS NULL, track_no, rel_path",
    )?;
    let rows = st
        .query_map([album_id], |r| r.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    Ok(rows)
}

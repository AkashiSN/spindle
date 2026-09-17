//! ReplayGain の解析結果の読み書き（SPEC §6「ReplayGain の内部表現」、P1-1）。
//! 値は内部表現（-18 LUFS 基準の dB、peak は線形）。タグへの書き込み状態（`rg_written_at`）は
//! ここでは触らない（P1-2）

use rusqlite::{params, Connection};

use super::Result;

/// 解析対象の 1 行（active なトラックだけ）。stat の列は「開いた FD がこの行の実体か」の
/// 照合用（パスは識別子ではない。SPEC §6）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub id: i64,
    pub rel_path: String,
    pub dev: Option<i64>,
    pub inode: Option<i64>,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl Member {
    /// 開いた FD の stat がこの行と一致するか（同じ実体で、DB に取り込んだ後に書かれていない）。
    /// 音声が同じでもタグの書き換えで size / mtime は動くので、一致しなければ再スキャン待ち
    pub fn matches(&self, st: &crate::fsroot::Stat) -> bool {
        self.dev == Some(st.dev as i64)
            && self.inode == Some(st.inode as i64)
            && self.size == st.size as i64
            && self.mtime_ns == st.mtime_ns
            && self.ctime_ns == st.ctime_ns
    }
}

const MEMBER_COLUMNS: &str = "id, rel_path, dev, inode, size, mtime_ns, ctime_ns";

fn member_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<Member> {
    Ok(Member {
        id: r.get(0)?,
        rel_path: r.get(1)?,
        dev: r.get(2)?,
        inode: r.get(3)?,
        size: r.get(4)?,
        mtime_ns: r.get(5)?,
        ctime_ns: r.get(6)?,
    })
}

/// album の active な構成トラック（`id` 昇順。ロック取得の順序と揃える）
pub fn album_members(conn: &Connection, album_id: i64) -> Result<Vec<Member>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT {MEMBER_COLUMNS} FROM tracks
          WHERE album_id = ?1 AND missing_since IS NULL ORDER BY id"
    ))?;
    let rows = st
        .query_map([album_id], member_of)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 単独トラック（active でなければ空）
pub fn track_member(conn: &Connection, track_id: i64) -> Result<Vec<Member>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT {MEMBER_COLUMNS} FROM tracks WHERE id = ?1 AND missing_since IS NULL"
    ))?;
    let rows = st
        .query_map([track_id], member_of)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 1 トラック分の書き込み値。album 側は集計に入らないトラック（2ch 以外、album 無し）で `None`
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Values {
    pub track_gain: f64,
    pub track_peak: f64,
    pub album_gain: Option<f64>,
    pub album_peak: Option<f64>,
}

/// 解析結果をまとめて書く（呼び出し側のトランザクション内）。`rg_scanned_at` を `now` にし、
/// `rg_written_at` と版は動かさない。走査中に missing になった行は書かない
pub fn store(conn: &Connection, results: &[(i64, Values)], now: i64) -> Result<usize> {
    let mut st = conn.prepare_cached(
        "UPDATE tracks
            SET rg_track_gain = ?2, rg_track_peak = ?3, rg_album_gain = ?4, rg_album_peak = ?5,
                rg_scanned_at = ?6
          WHERE id = ?1 AND missing_since IS NULL",
    )?;
    let mut n = 0;
    for (id, v) in results {
        n += st.execute(params![
            id,
            v.track_gain,
            v.track_peak,
            v.album_gain,
            v.album_peak,
            now
        ])?;
    }
    Ok(n)
}

/// `track_ids` の active な行を解析単位に分ける: album を持つものは album_id（重複なし、昇順）、
/// 持たないものは track_id（昇順）
pub fn scopes_of(conn: &Connection, track_ids: &[i64]) -> Result<(Vec<i64>, Vec<i64>)> {
    let json =
        serde_json::to_string(track_ids).map_err(|e| super::DbError::Internal(e.to_string()))?;
    let mut st = conn.prepare_cached(
        "SELECT DISTINCT album_id FROM tracks
          WHERE album_id IS NOT NULL AND missing_since IS NULL
            AND id IN (SELECT value FROM json_each(?1))
          ORDER BY album_id",
    )?;
    let albums = st
        .query_map([&json], |r| r.get(0))?
        .collect::<std::result::Result<Vec<i64>, _>>()?;
    let mut st = conn.prepare_cached(
        "SELECT id FROM tracks
          WHERE album_id IS NULL AND missing_since IS NULL
            AND id IN (SELECT value FROM json_each(?1))
          ORDER BY id",
    )?;
    let tracks = st
        .query_map([&json], |r| r.get(0))?
        .collect::<std::result::Result<Vec<i64>, _>>()?;
    Ok((albums, tracks))
}

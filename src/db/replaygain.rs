//! ReplayGain の解析結果の読み書き（SPEC §6「ReplayGain の内部表現」、P1-1 / P1-2）。
//! 値は内部表現（-18 LUFS 基準の dB、peak は線形）。
//!
//! `rg_written_at` は「ファイルの RG タグが解析値と一致していることを確認した時刻」。書き込み
//! バッチの applied だけでなく、DB をファイルの現在値へ揃える経路（overlay の解消、外部変更の
//! 追随）でも [`sync_written_at`] で判定し直す。`rg_scanned_at` より古ければ UI は「書き込み
//! 未反映」として見せる

use rusqlite::{params, Connection, OptionalExtension as _};

use super::Result;
use crate::domain::replaygain::file_matches;
use crate::domain::tags::{Codec, TagSet};

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
    /// 音声が同じでもタグの書き換えで size / mtime は動くので、一致しなければ再スキャン待ち。
    /// dev は照合しない（マウントのたびに振り直されうる。D-62）
    pub fn matches(&self, st: &crate::fsroot::Stat) -> bool {
        self.inode == Some(st.inode as i64)
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

pub use crate::domain::replaygain::Values;

/// 解析結果をまとめて書く（呼び出し側のトランザクション内）。`rg_scanned_at` を `now` にし、
/// 版は動かさない。走査中に missing になった行は書かない。値が変わるときの `rg_scanned_at` は
/// 前回より必ず大きくする（同じ秒に再解析して値が変わっても Derived の `src_rg_scanned_at` との
/// 比較で世代が進んで見えるように。P1-10 / D-51）。
///
/// `rg_written_at` は「ファイルのタグが解析値と一致していると確認した時刻」なので、値が
/// 1 つでも変われば NULL にする（ファイルは旧値のまま）。時刻が秒単位のため、同じ秒に確認と
/// 再解析が起きると `rg_written_at < rg_scanned_at` では検出できない。値が全て同じで確認が
/// 有効（`rg_written_at >= rg_scanned_at`）なら確認は成り立ったままなので `now` へ進める。
///
/// どちらの分岐でも `rg_scanned_at` は**前の値より小さくしない**（同じ秒に [`set_album_gain`] が
/// `now + 1` へ進めた直後に、その前から走っていた解析が同じ値で保存すると巻き戻り、Derived の
/// `src_rg_scanned_at` との差分が消えて追随が抜ける。D-74）
pub fn store(conn: &Connection, results: &[(i64, Values)], now: i64) -> Result<usize> {
    let mut st = conn.prepare_cached(
        "UPDATE tracks
            SET rg_written_at = CASE
                  WHEN rg_track_gain IS ?2 AND rg_track_peak IS ?3
                   AND rg_album_gain IS ?4 AND rg_album_peak IS ?5
                   AND rg_written_at IS NOT NULL AND rg_written_at >= rg_scanned_at
                  THEN MAX(?6, rg_written_at) ELSE NULL END,
                rg_scanned_at = CASE
                  WHEN rg_track_gain IS ?2 AND rg_track_peak IS ?3
                   AND rg_album_gain IS ?4 AND rg_album_peak IS ?5
                  THEN MAX(?6, COALESCE(rg_scanned_at, 0)) ELSE MAX(?6, COALESCE(rg_scanned_at, 0) + 1) END,
                rg_track_gain = ?2, rg_track_peak = ?3, rg_album_gain = ?4, rg_album_peak = ?5
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

// ---------------------------------------------------------------- album gain の属性（D-74、P4-5）

/// album gain を計算・書き出しする album か（D-74）。無い album は false
pub fn album_gain_enabled(conn: &Connection, album_id: i64) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT album_gain FROM albums WHERE id = ?1",
            [album_id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0)
        == 1)
}

/// [`set_album_gain`] の結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumGainChange {
    /// 属性が変わった（同じ値なら false で `cleared` は空）
    pub changed: bool,
    /// off にして album の値を消した track（Derived の追随を投入する対象）
    pub cleared: Vec<i64>,
}

/// album gain の属性を変える（D-74）。off にするときは構成トラックの `rg_album_*` を NULL にし、
/// `rg_written_at` を NULL に戻し（ファイルには album のキーが残っている）、`rg_scanned_at` を進める
/// （Derived の `src_rg_scanned_at` との差分でタグ上書きが走る）。on にするだけでは行を触らない
/// （次の album 解析で揃う。呼び出し側が rg を投入する）。album が無ければ None
pub fn set_album_gain(
    conn: &Connection,
    album_id: i64,
    on: bool,
    now: i64,
) -> Result<Option<AlbumGainChange>> {
    let current: Option<i64> = conn
        .query_row(
            "SELECT album_gain FROM albums WHERE id = ?1",
            [album_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(current) = current else {
        return Ok(None);
    };
    if (current == 1) == on {
        return Ok(Some(AlbumGainChange {
            changed: false,
            cleared: Vec::new(),
        }));
    }
    conn.execute(
        "UPDATE albums SET album_gain = ?2 WHERE id = ?1",
        params![album_id, i64::from(on)],
    )?;
    let mut cleared = Vec::new();
    if !on {
        let mut st = conn.prepare_cached(
            "SELECT id FROM tracks
              WHERE album_id = ?1 AND (rg_album_gain IS NOT NULL OR rg_album_peak IS NOT NULL)
              ORDER BY id",
        )?;
        cleared = st
            .query_map([album_id], |r| r.get(0))?
            .collect::<std::result::Result<Vec<i64>, _>>()?;
        conn.execute(
            "UPDATE tracks
                SET rg_album_gain = NULL, rg_album_peak = NULL, rg_written_at = NULL,
                    rg_scanned_at = MAX(?2, COALESCE(rg_scanned_at, 0) + 1)
              WHERE album_id = ?1 AND (rg_album_gain IS NOT NULL OR rg_album_peak IS NOT NULL)",
            params![album_id, now],
        )?;
    }
    Ok(Some(AlbumGainChange {
        changed: true,
        cleared,
    }))
}

/// 行の現在の `rg_album_gain` / `rg_album_peak`（track 単位の解析で album の値を保つのに使う）。
/// 行が無ければ None
pub fn album_values_of(
    conn: &Connection,
    track_id: i64,
) -> Result<Option<(Option<f64>, Option<f64>)>> {
    Ok(conn
        .query_row(
            "SELECT rg_album_gain, rg_album_peak FROM tracks WHERE id = ?1",
            [track_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// `track_ids` の active な行を解析単位に分ける（D-74）: `album_gain = 1` の album に属するものは
/// album_id（重複なし、昇順）、album を持たないものと `album_gain = 0` の album のものは track_id（昇順）
pub fn scopes_of(conn: &Connection, track_ids: &[i64]) -> Result<(Vec<i64>, Vec<i64>)> {
    let json =
        serde_json::to_string(track_ids).map_err(|e| super::DbError::Internal(e.to_string()))?;
    let mut st = conn.prepare_cached(
        "SELECT DISTINCT t.album_id FROM tracks t JOIN albums a ON a.id = t.album_id
          WHERE a.album_gain = 1 AND t.missing_since IS NULL
            AND t.id IN (SELECT value FROM json_each(?1))
          ORDER BY t.album_id",
    )?;
    let albums = st
        .query_map([&json], |r| r.get(0))?
        .collect::<std::result::Result<Vec<i64>, _>>()?;
    let mut st = conn.prepare_cached(
        "SELECT t.id FROM tracks t LEFT JOIN albums a ON a.id = t.album_id
          WHERE (t.album_id IS NULL OR a.album_gain = 0) AND t.missing_since IS NULL
            AND t.id IN (SELECT value FROM json_each(?1))
          ORDER BY t.id",
    )?;
    let tracks = st
        .query_map([&json], |r| r.get(0))?
        .collect::<std::result::Result<Vec<i64>, _>>()?;
    Ok((albums, tracks))
}

// ---------------------------------------------------------------- タグ書き込み（P1-2）

/// 書き込み対象の 1 行。`values` は解析済み（`rg_scanned_at IS NOT NULL`）のときだけ
#[derive(Debug, Clone, PartialEq)]
pub struct WriteRow {
    pub id: i64,
    pub codec: String,
    pub values: Option<Values>,
    pub missing: bool,
}

/// `track_ids` の行（missing も含む。存在しない id は返さない）を id 昇順で
pub fn write_rows(conn: &Connection, track_ids: &[i64]) -> Result<Vec<WriteRow>> {
    let json =
        serde_json::to_string(track_ids).map_err(|e| super::DbError::Internal(e.to_string()))?;
    let mut st = conn.prepare_cached(
        "SELECT id, codec, rg_track_gain, rg_track_peak, rg_album_gain, rg_album_peak,
                rg_scanned_at, missing_since IS NOT NULL
           FROM tracks WHERE id IN (SELECT value FROM json_each(?1)) ORDER BY id",
    )?;
    let rows = st
        .query_map([&json], |r| {
            let scanned: Option<i64> = r.get(6)?;
            let track_gain: Option<f64> = r.get(2)?;
            let track_peak: Option<f64> = r.get(3)?;
            let values = match (scanned, track_gain, track_peak) {
                (Some(_), Some(track_gain), Some(track_peak)) => Some(Values {
                    track_gain,
                    track_peak,
                    album_gain: r.get(4)?,
                    album_peak: r.get(5)?,
                }),
                _ => None,
            };
            Ok(WriteRow {
                id: r.get(0)?,
                codec: r.get(1)?,
                values,
                missing: r.get(7)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `rg_written_at` を `now` にする（ファイルが既に解析値を持つと分かった行）
pub fn set_written(conn: &Connection, track_ids: &[i64], now: i64) -> Result<usize> {
    let mut st = conn.prepare_cached(
        "UPDATE tracks SET rg_written_at = ?2 WHERE id = ?1 AND rg_scanned_at IS NOT NULL",
    )?;
    let mut n = 0;
    for id in track_ids {
        n += st.execute(params![id, now])?;
    }
    Ok(n)
}

/// 音声が差し替わった（`audio_version` が進んだ）トラックの解析値を捨てる（D-47）。値は残さず
/// `rg_scanned_at` / `rg_written_at` を NULL にし、次の解析までは未解析扱い（古い値を Derived や
/// 再生に使わない。album の他のトラックの `rg_album_*` は次の album 解析で揃う）
pub fn reset_analysis(conn: &Connection, track_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE tracks SET rg_track_gain = NULL, rg_track_peak = NULL, rg_album_gain = NULL,
                rg_album_peak = NULL, rg_scanned_at = NULL, rg_written_at = NULL
          WHERE id = ?1",
        [track_id],
    )?;
    Ok(())
}

/// ファイルの現在のタグ集合から `rg_written_at` を判定し直す。解析値と一致していれば `now`、
/// 一致しない（RG のキーが無い・別の値・未解析）なら NULL。返り値は一致したか
pub fn sync_written_at(
    conn: &Connection,
    track_id: i64,
    tags: &TagSet,
    reference: f64,
    now: i64,
) -> Result<bool> {
    let rows = write_rows(conn, &[track_id])?;
    let matched = rows.first().is_some_and(|row| {
        row.values.is_some_and(|v| {
            let codec = Codec::parse(&row.codec).unwrap_or(Codec::Flac);
            file_matches(codec, tags, &v, reference)
        })
    });
    let written: Option<i64> = matched.then_some(now);
    conn.execute(
        "UPDATE tracks SET rg_written_at = ?2 WHERE id = ?1",
        params![track_id, written],
    )?;
    Ok(matched)
}

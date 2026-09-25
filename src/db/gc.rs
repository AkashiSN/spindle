//! GC の判定と行削除の SQL（P1-11、D-56）。物理削除の判断はここではしない（`gc` モジュール）。
//! 削除の SQL は条件（`missing_since` / 参照の無さ）を再確認して、計画からの時間差で状態が
//! 変わった行を消さない

use std::collections::HashSet;

use rusqlite::{params, Connection, OptionalExtension};

use super::Result;

/// `missing_since <= cutoff` のトラック（id 昇順）
pub fn missing_tracks(conn: &Connection, cutoff: i64) -> Result<Vec<(i64, String)>> {
    let mut st = conn.prepare_cached(
        "SELECT id, rel_path FROM tracks WHERE missing_since IS NOT NULL AND missing_since <= ?1
         ORDER BY id",
    )?;
    let rows = st.query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// `missing_since <= cutoff` のアルバムと、その構成トラック id
pub fn missing_albums(conn: &Connection, cutoff: i64) -> Result<Vec<(i64, String, Vec<i64>)>> {
    let mut st = conn.prepare_cached(
        "SELECT id, rel_dir FROM albums WHERE missing_since IS NOT NULL AND missing_since <= ?1
         ORDER BY id",
    )?;
    let albums: Vec<(i64, String)> = st
        .query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut members = conn.prepare_cached("SELECT id FROM tracks WHERE album_id = ?1")?;
    let mut out = Vec::with_capacity(albums.len());
    for (id, rel_dir) in albums {
        let ids = members
            .query_map([id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        out.push((id, rel_dir, ids));
    }
    Ok(out)
}

/// `held` で期限を過ぎた退避ファイル（id 昇順）: `(id, rel_path)`
pub fn eligible_archived(conn: &Connection, now: i64) -> Result<Vec<(i64, String)>> {
    let mut st = conn.prepare_cached(
        "SELECT id, rel_path FROM archived_files WHERE state = 'held' AND eligible_after <= ?1
         ORDER BY id",
    )?;
    let rows = st.query_map([now], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// `derived_files` の `rel_path_key` 全件（`except` のトラックのものを除く）
pub fn derived_keys(conn: &Connection, except: &HashSet<i64>) -> Result<HashSet<String>> {
    let mut st = conn.prepare_cached("SELECT track_id, rel_path_key FROM derived_files")?;
    let mut keys = HashSet::new();
    for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
        let (track_id, key) = r?;
        if !except.contains(&track_id) {
            keys.insert(key);
        }
    }
    Ok(keys)
}

/// 「参照されている」の SQL 断片（`a` は `artwork`）: `albums.artwork_id` / `tracks.artwork_id`（D-61）
/// から参照されるか、編集履歴の `PICTURE` 値（旧 / 新。`<mime>:<sha256hex>` の配列）に現れる（巻き戻しに要る。D-60）。
/// 値は JSON 文字列なので hex の部分一致で引く（64 桁の hex は他の値と衝突しない）。
/// Inbox の下書きが差し替えに指定した画像（`tracks[].picture` の `<mime>:<sha256hex>`。D-86）も、配置まで
/// 要るので参照とみなす。下書きには任意のタグの値も入るので全文の部分一致にはせず、`picture` の値だけを
/// JSON として読んで `:` の後ろと照合する（壊れた JSON は空、`tracks` の object でない要素と文字列でない
/// `picture` は飛ばす。GC 全体を落とさない。codex 指摘）
/// CD の取り込みのために Cover Art Archive から取った表の画像（`inbox_items.caa_picture`。D-91）も、
/// 提案の初期値として配置まで要るので参照とみなす
const ARTWORK_REFERENCED: &str = "EXISTS (SELECT 1 FROM albums b WHERE b.artwork_id = a.id)
       OR EXISTS (SELECT 1 FROM tracks t WHERE t.artwork_id = a.id)
       OR EXISTS (SELECT 1 FROM edits e WHERE e.key = 'PICTURE'
                    AND (instr(e.old_value, lower(hex(a.sha256))) > 0
                      OR instr(e.new_value, lower(hex(a.sha256))) > 0))
       OR EXISTS (SELECT 1 FROM inbox_items i,
                    json_each(CASE WHEN json_valid(i.draft) THEN i.draft ELSE '{}' END, '$.tracks') d
                  WHERE i.draft IS NOT NULL
                    AND d.type = 'object'
                    AND (CASE WHEN d.type = 'object'
                              AND json_type(d.value, '$.picture') = 'text'
                         THEN lower(substr(json_extract(d.value, '$.picture'),
                                           instr(json_extract(d.value, '$.picture'), ':') + 1))
                         END) = lower(hex(a.sha256)))
       OR EXISTS (SELECT 1 FROM inbox_items i
                  WHERE i.caa_picture IS NOT NULL
                    AND lower(substr(i.caa_picture, instr(i.caa_picture, ':') + 1)) = lower(hex(a.sha256)))";

/// どこからも参照されない `artwork` 行: `(id, sha256)`（[`ARTWORK_REFERENCED`] の否定）
pub fn unreferenced_artwork(conn: &Connection) -> Result<Vec<(i64, Vec<u8>)>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT a.id, a.sha256 FROM artwork a WHERE NOT ({ARTWORK_REFERENCED}) ORDER BY a.id"
    ))?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// `artwork` の sha256 全件
pub fn artwork_hashes(conn: &Connection) -> Result<Vec<Vec<u8>>> {
    let mut st = conn.prepare_cached("SELECT sha256 FROM artwork")?;
    let rows = st.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// トラック行を消す。計画時の条件（missing のまま）を再確認する。消した件数
pub fn delete_tracks(conn: &Connection, ids: &[i64], cutoff: i64) -> Result<usize> {
    let mut st = conn.prepare_cached(
        "DELETE FROM tracks WHERE id = ?1 AND missing_since IS NOT NULL AND missing_since <= ?2",
    )?;
    let mut n = 0;
    for id in ids {
        n += st.execute(params![id, cutoff])?;
    }
    Ok(n)
}

/// アルバム行を消す。構成トラックが無いことを再確認する。消した件数
pub fn delete_albums(conn: &Connection, ids: &[i64], cutoff: i64) -> Result<usize> {
    let mut st = conn.prepare_cached(
        "DELETE FROM albums WHERE id = ?1 AND missing_since IS NOT NULL AND missing_since <= ?2
           AND NOT EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = albums.id)",
    )?;
    let mut n = 0;
    for id in ids {
        n += st.execute(params![id, cutoff])?;
    }
    Ok(n)
}

/// アートワーク行を消す。参照が無いことを再確認する。消した件数
pub fn delete_artwork(conn: &Connection, ids: &[i64]) -> Result<usize> {
    let mut st = conn.prepare_cached(&format!(
        "DELETE FROM artwork WHERE id = ?1
           AND NOT EXISTS (SELECT 1 FROM artwork a WHERE a.id = artwork.id AND ({ARTWORK_REFERENCED}))"
    ))?;
    let mut n = 0;
    for id in ids {
        n += st.execute([id])?;
    }
    Ok(n)
}

/// C の実行直前の再確認: 行がまだ `held` で期限を過ぎ、パスが計画時と同じか。同じなら `track_id`
/// （`Some(None)` はトラック無し）。`job_id` があればそのトラックのロックを取り、取れなければ
/// `Ok(None)`
pub fn recheck_archived(
    conn: &Connection,
    id: i64,
    rel_path: &str,
    now: i64,
    job_id: Option<i64>,
) -> Result<Option<Option<i64>>> {
    // track_id は FK でない（トラック削除後も台帳は残る）。行が無ければロックの対象でもない
    let row: Option<Option<i64>> = conn
        .query_row(
            "SELECT (SELECT t.id FROM tracks t WHERE t.id = a.track_id) FROM archived_files a
             WHERE a.id = ?1 AND a.rel_path = ?2 AND a.state = 'held' AND a.eligible_after <= ?3",
            params![id, rel_path, now],
            |r| r.get(0),
        )
        .optional()?;
    let Some(track_id) = row else {
        return Ok(None);
    };
    if let (Some(job_id), Some(track_id)) = (job_id, track_id) {
        // 巻き戻し（normalize ジョブ）が同じトラックを触っていれば飛ばす
        if !super::jobs::acquire_track_locks(conn, job_id, &[track_id], now)? {
            return Ok(None);
        }
    }
    Ok(Some(track_id))
}

/// C: unlink 後に `held` → `deleted`（CAS。他の状態に進んでいれば触らない）
pub fn mark_archived_deleted(conn: &Connection, id: i64, now: i64) -> Result<bool> {
    let n = conn.execute(
        "UPDATE archived_files SET state = 'deleted', state_at = ?2 WHERE id = ?1 AND state = 'held'",
        params![id, now],
    )?;
    Ok(n > 0)
}

/// その key を指す `derived_files` 行があるか
pub fn derived_has_row(conn: &Connection, key: &str) -> Result<bool> {
    let referenced: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM derived_files WHERE rel_path_key = ?1",
            [key],
            |r| r.get(0),
        )
        .optional()?;
    Ok(referenced.is_some())
}

/// D の実行直前: その key を指す `derived_files` 行が無ければ、transcode と同じ排他予約
/// （`derived_path_locks`。`track_id` は NULL）を GC のジョブで取る。取れれば true。running な
/// ジョブの予約があれば false、終わったジョブの残骸は奪う（`derived::lock_path` の規則）
pub fn lock_derived_for_gc(conn: &Connection, key: &str, job_id: i64, now: i64) -> Result<bool> {
    if derived_has_row(conn, key)? {
        return Ok(false);
    }
    super::derived::lock_path(conn, key, None, job_id, now)
}

/// E(dir) の実行直前の再確認: その hex の `artwork` 行が無いか
pub fn artwork_hex_is_orphan(conn: &Connection, hex: &str) -> Result<bool> {
    let row: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM artwork WHERE lower(hex(sha256)) = ?1",
            [hex],
            |r| r.get(0),
        )
        .optional()?;
    Ok(row.is_none())
}

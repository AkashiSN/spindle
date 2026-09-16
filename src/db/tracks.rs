//! トラック一覧・検索・selection の解決（SPEC §9、P0-7、D-39）。
//!
//! SQL はここで組み立てるが、列名と演算子は `domain::filter` の列挙型からしか出てこない。
//! 値はすべてバインドパラメータ（CLAUDE.md 禁止事項「生 SQL 文字列の組み立て」）。
//!
//! 一覧はキーセットページング。ソート式は `0002_tracks_sort_indexes.sql` の式索引と
//! 字面まで一致させる（`coalesce(t.title, '')` 等）。バッジ用の pending / 最新 op /
//! duplicate は行ごとの索引検索（LEFT JOIN と相関サブクエリ）で引き、`duplicate_groups`
//! ビューは使わない（ビューを LEFT JOIN すると毎回 GROUP BY の実体化と自動索引が走る）。

use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection, OptionalExtension, Row};
use serde::Serialize;

use super::Result;
use crate::domain::filter::{
    fts_phrase, like_pattern, uses_fts, Cursor, CursorValue, Filter, Flag, Query, Sort, SortKey,
};
use crate::domain::selection::{Selection, SnapshotRow};

/// 一覧の 1 行（SPEC §9 のレスポンス形）
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrackRow {
    pub id: i64,
    pub title: Option<String>,
    pub artist_display: Option<String>,
    pub album: Option<String>,
    pub albumartist: Option<String>,
    pub track_no: Option<i64>,
    pub disc_no: Option<i64>,
    pub date: Option<String>,
    pub category: Option<String>,
    pub duration_ms: Option<i64>,
    pub codec: String,
    pub lossless: bool,
    pub verification: String,
    pub rg_scanned_at: Option<i64>,
    pub rg_written_at: Option<i64>,
    pub derived: Option<Derived>,
    pub pending_batch_id: Option<i64>,
    pub conflict_batch_id: Option<i64>,
    /// `audio_md5` の hex（小文字）。重複でなければ None
    pub duplicate_group: Option<String>,
    pub hardlink: bool,
    pub missing_since: Option<i64>,
    pub rel_path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Derived {
    pub codec: String,
    pub stale_tags: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Page {
    pub items: Vec<TrackRow>,
    pub next_cursor: Option<String>,
    pub total: i64,
}

/// 行の SELECT 列。ソートキーの値はこの後ろに付く（カーソル生成用）
const ROW_COLUMNS: &str = "t.id, t.title, t.artist_display, t.album, t.albumartist, t.track_no, t.disc_no, t.date,
  (SELECT c.name FROM albums a JOIN categories c ON c.id = a.category_id WHERE a.id = t.album_id),
  t.duration_ms, t.codec, t.lossless, t.verification, t.rg_scanned_at, t.rg_written_at,
  d.codec, d.src_tag_version <> t.tag_version,
  po.batch_id,
  CASE WHEN lo.result = 'skipped_conflict' THEN lo.batch_id END,
  CASE WHEN t.audio_md5 IS NOT NULL AND t.missing_since IS NULL AND EXISTS (
         SELECT 1 FROM tracks t2 WHERE t2.audio_md5 = t.audio_md5 AND t2.missing_since IS NULL AND t2.id <> t.id)
       THEN lower(hex(t.audio_md5)) END,
  t.nlink > 1, t.missing_since, t.rel_path";
/// `ROW_COLUMNS` の列数。ソートキーの値はこの位置から始まる
const ROW_COLUMN_COUNT: usize = 23;

const ROW_JOINS: &str = "FROM tracks t
LEFT JOIN derived_files d ON d.track_id = t.id
LEFT JOIN edit_ops po ON po.track_id = t.id AND po.result = 'pending'
LEFT JOIN edit_ops lo ON lo.id = (SELECT max(o.id) FROM edit_ops o WHERE o.track_id = t.id)";

fn read_row(r: &Row) -> rusqlite::Result<TrackRow> {
    let derived_codec: Option<String> = r.get(15)?;
    let stale: Option<bool> = r.get(16)?;
    Ok(TrackRow {
        id: r.get(0)?,
        title: r.get(1)?,
        artist_display: r.get(2)?,
        album: r.get(3)?,
        albumartist: r.get(4)?,
        track_no: r.get(5)?,
        disc_no: r.get(6)?,
        date: r.get(7)?,
        category: r.get(8)?,
        duration_ms: r.get(9)?,
        codec: r.get(10)?,
        lossless: r.get(11)?,
        verification: r.get(12)?,
        rg_scanned_at: r.get(13)?,
        rg_written_at: r.get(14)?,
        derived: derived_codec.map(|codec| Derived {
            codec,
            stale_tags: stale.unwrap_or(false),
        }),
        pending_batch_id: r.get(17)?,
        conflict_batch_id: r.get(18)?,
        duplicate_group: r.get(19)?,
        hardlink: r.get(20)?,
        missing_since: r.get(21)?,
        rel_path: r.get(22)?,
    })
}

/// ソートキーごとの式。`0002_tracks_sort_indexes.sql` の索引式と一致させること
fn sort_exprs(key: SortKey) -> &'static [&'static str] {
    match key {
        SortKey::Album => &[
            "coalesce(t.albumartist, '')",
            "coalesce(t.album_id, 0)",
            "coalesce(t.disc_no, 0)",
            "coalesce(t.track_no, 0)",
        ],
        SortKey::Title => &["coalesce(t.title, '')"],
        SortKey::Artist => &["coalesce(t.artist_display, '')"],
        SortKey::AlbumTitle => &["coalesce(t.album, '')"],
        SortKey::AlbumArtist => &["coalesce(t.albumartist, '')"],
        SortKey::Date => &["coalesce(t.date, '')"],
        SortKey::Duration => &["coalesce(t.duration_ms, -1)"],
        SortKey::Codec => &["t.codec"],
        SortKey::RelPath => &["t.rel_path"],
        SortKey::Id => &[],
    }
}

/// WHERE 句の断片とパラメータ
#[derive(Default)]
struct Where {
    clauses: Vec<String>,
    params: Vec<Value>,
}

impl Where {
    fn push(&mut self, clause: &str, params: impl IntoIterator<Item = Value>) {
        self.clauses.push(clause.to_owned());
        self.params.extend(params);
    }

    fn sql(&self) -> String {
        if self.clauses.is_empty() {
            "1".to_owned()
        } else {
            self.clauses.join(" AND ")
        }
    }
}

fn filter_where(f: &Filter) -> Where {
    let mut w = Where::default();
    if let Some(c) = &f.category {
        w.push(
            "t.album_id IN (SELECT a.id FROM albums a JOIN categories c ON c.id = a.category_id WHERE c.name = ?)",
            [Value::from(c.clone())],
        );
    }
    if let Some(aa) = &f.albumartist {
        w.push("t.albumartist = ?", [Value::from(aa.clone())]);
    }
    if let Some(id) = f.album_id {
        w.push("t.album_id = ?", [Value::from(id)]);
    }
    if let Some(id) = f.playlist_id {
        w.push(
            "t.id IN (SELECT track_id FROM playlist_items WHERE playlist_id = ?)",
            [Value::from(id)],
        );
    }
    for flag in &f.flags {
        let clause = match flag {
            Flag::Unverified => "t.verification = 'not_attempted'",
            // duplicate は相関 EXISTS のまま（duplicate_groups ビューの IN は GROUP BY の実体化で 2 倍遅い）
            Flag::Duplicate => {
                "t.audio_md5 IS NOT NULL AND t.missing_since IS NULL AND EXISTS (
                   SELECT 1 FROM tracks t2 WHERE t2.audio_md5 = t.audio_md5 AND t2.missing_since IS NULL AND t2.id <> t.id)"
            }
            Flag::Missing => "t.missing_since IS NOT NULL",
            Flag::NoRg => "t.rg_scanned_at IS NULL",
            // pending / conflict は行ごとの相関サブクエリにしない。該当行は少数（数十〜数千）で
            // 全行を歩くと 6 万回の索引検索になる（計測で 120ms）。op 側から集合を作って IN で引く
            Flag::Pending => {
                "t.id IN (SELECT o.track_id FROM edit_ops o WHERE o.result = 'pending')"
            }
            Flag::Conflict => {
                "t.id IN (SELECT o.track_id FROM edit_ops o
                          WHERE o.result = 'skipped_conflict'
                            AND o.id = (SELECT max(o2.id) FROM edit_ops o2 WHERE o2.track_id = o.track_id))"
            }
            Flag::Hardlink => "t.nlink > 1",
        };
        w.push(clause, []);
    }
    if let Some(q) = &f.q {
        if uses_fts(q) {
            w.push(
                "t.id IN (SELECT rowid FROM tracks_fts WHERE tracks_fts MATCH ?)",
                [Value::from(fts_phrase(q))],
            );
        } else {
            let p = like_pattern(q);
            w.push(
                "(t.title LIKE ? ESCAPE '\\' OR t.artist_display LIKE ? ESCAPE '\\'
                  OR t.album LIKE ? ESCAPE '\\' OR t.albumartist LIKE ? ESCAPE '\\')",
                std::iter::repeat_n(Value::from(p), 4),
            );
        }
    }
    w
}

fn order_by(sort: Sort) -> String {
    let dir = if sort.desc { "DESC" } else { "ASC" };
    let mut terms: Vec<String> = sort_exprs(sort.key)
        .iter()
        .map(|e| format!("{e} {dir}"))
        .collect();
    terms.push(format!("t.id {dir}"));
    format!("ORDER BY {}", terms.join(", "))
}

/// キーセットの述語: 先頭キーの範囲条件（索引の入口）+ 全キーの行値比較（残りの絞り込み）。
/// 式索引には行値比較が直接は効かないので、範囲条件を別に置く
fn cursor_where(sort: Sort, cursor: &Cursor) -> Option<(String, Vec<Value>)> {
    let exprs = sort_exprs(sort.key);
    // 発行時のソート・キー数・値の型が現在のソートと一致しないカーソルは使わない
    if !cursor.matches(sort) || cursor.keys.len() != exprs.len() {
        return None;
    }
    let (range, cmp) = if sort.desc { ("<=", "<") } else { (">=", ">") };
    let to_value = |v: &CursorValue| match v {
        CursorValue::Int(i) => Value::from(*i),
        CursorValue::Text(s) => Value::from(s.clone()),
    };
    let mut params: Vec<Value> = Vec::new();
    let mut sql = String::new();
    let Some(first) = exprs.first() else {
        // id だけのソート。行値比較にすると rowid の範囲検索として認識されない
        return Some((format!("t.id {cmp} ?"), vec![Value::from(cursor.id)]));
    };
    sql.push_str(&format!("{first} {range} ? AND "));
    params.push(to_value(&cursor.keys[0]));
    let mut lhs: Vec<&str> = exprs.to_vec();
    lhs.push("t.id");
    let placeholders = vec!["?"; lhs.len()].join(", ");
    sql.push_str(&format!("({}) {cmp} ({placeholders})", lhs.join(", ")));
    params.extend(cursor.keys.iter().map(to_value));
    params.push(Value::from(cursor.id));
    Some((sql, params))
}

/// 一覧の SELECT 文（カーソル生成用にソートキーの値を末尾に付ける）
fn list_sql(q: &Query) -> (String, Vec<Value>) {
    let mut w = filter_where(&q.filter);
    if let Some(c) = &q.cursor {
        if let Some((sql, params)) = cursor_where(q.sort, c) {
            w.push(&sql, params);
        } else {
            // ソートとカーソルの形が合わない: 空ページを返す
            w.push("0", []);
        }
    }
    let key_cols = sort_exprs(q.sort.key)
        .iter()
        .map(|e| format!(", {e}"))
        .collect::<String>();
    let sql = format!(
        "SELECT {ROW_COLUMNS}{key_cols}\n{ROW_JOINS}\nWHERE {}\n{}\nLIMIT ?",
        w.sql(),
        order_by(q.sort)
    );
    let mut params = w.params;
    params.push(Value::from(q.limit as i64 + 1));
    (sql, params)
}

fn count_sql(f: &Filter) -> (String, Vec<Value>) {
    let w = filter_where(f);
    (
        format!("SELECT count(*) FROM tracks t WHERE {}", w.sql()),
        w.params,
    )
}

fn cursor_value(v: Value) -> rusqlite::Result<CursorValue> {
    match v {
        Value::Integer(i) => Ok(CursorValue::Int(i)),
        Value::Text(s) => Ok(CursorValue::Text(s)),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            other.data_type(),
            "ソートキーの値が TEXT / INTEGER でない".into(),
        )),
    }
}

/// 1 ページ取得。`total` は同じ読み取りスナップショットで数える
pub fn list(conn: &Connection, q: &Query) -> Result<Page> {
    let tx = conn.unchecked_transaction()?;
    let (sql, params) = list_sql(q);
    let mut stmt = tx.prepare_cached(&sql)?;
    let n_keys = sort_exprs(q.sort.key).len();
    let mut rows = stmt.query(params_from_iter(params))?;
    let mut items: Vec<TrackRow> = Vec::with_capacity(q.limit);
    let mut last_cursor: Option<Cursor> = None;
    let mut overflow = false;
    while let Some(r) = rows.next()? {
        if items.len() == q.limit {
            overflow = true;
            break;
        }
        let row = read_row(r)?;
        let mut keys = Vec::with_capacity(n_keys);
        for i in 0..n_keys {
            keys.push(cursor_value(r.get::<_, Value>(ROW_COLUMN_COUNT + i)?)?);
        }
        last_cursor = Some(Cursor {
            sort: q.sort,
            keys,
            id: row.id,
        });
        items.push(row);
    }
    drop(rows);
    drop(stmt);
    let total = count(&tx, &q.filter)?;
    tx.commit()?;
    let next_cursor = if overflow {
        last_cursor.map(|c| c.encode())
    } else {
        None
    };
    Ok(Page {
        items,
        next_cursor,
        total,
    })
}

/// フィルタに一致する件数
pub fn count(conn: &Connection, f: &Filter) -> Result<i64> {
    let (sql, params) = count_sql(f);
    let mut stmt = conn.prepare_cached(&sql)?;
    Ok(stmt.query_row(params_from_iter(params), |r| r.get(0))?)
}

/// 1 行を id で引く
pub fn get(conn: &Connection, id: i64) -> Result<Option<TrackRow>> {
    let sql = format!("SELECT {ROW_COLUMNS}\n{ROW_JOINS}\nWHERE t.id = ?");
    let mut stmt = conn.prepare_cached(&sql)?;
    Ok(stmt.query_row([id], read_row).optional()?)
}

/// `EXPLAIN QUERY PLAN` の各行（テストで temp B-tree が出ないことを固定する）
pub fn explain_list(conn: &Connection, q: &Query) -> Result<Vec<String>> {
    let (sql, params) = list_sql(q);
    explain(conn, &sql, params)
}

pub fn explain_count(conn: &Connection, f: &Filter) -> Result<Vec<String>> {
    let (sql, params) = count_sql(f);
    explain(conn, &sql, params)
}

fn explain(conn: &Connection, sql: &str, params: Vec<Value>) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
    let rows = stmt.query_map(params_from_iter(params), |r| r.get::<_, String>(3))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---------------------------------------------------------------- selection

const SNAPSHOT_COLUMNS: &str =
    "t.id, t.tag_version, t.audio_version, t.dev, t.inode, t.size, t.mtime_ns, t.ctime_ns, t.tag_hash, t.rel_path";

fn read_snapshot(r: &Row) -> rusqlite::Result<SnapshotRow> {
    Ok(SnapshotRow {
        id: r.get(0)?,
        tag_version: r.get(1)?,
        audio_version: r.get(2)?,
        dev: r.get(3)?,
        inode: r.get(4)?,
        size: r.get(5)?,
        mtime_ns: r.get(6)?,
        ctime_ns: r.get(7)?,
        tag_hash: r.get(8)?,
        rel_path: r.get(9)?,
    })
}

/// selection の 2 形を同じ行集合に解決する（SPEC §9、D-33）。
/// フィルタ形は ID を列挙せず SQL で解決し、`exclude_ids` だけをバインドする。
/// 存在しない id は黙って落ちる。結果は id 昇順
pub fn resolve_selection(conn: &Connection, sel: &Selection) -> Result<Vec<SnapshotRow>> {
    match sel {
        Selection::Ids(ids) => {
            // 大量の id を IN に並べず、JSON 配列 1 本を json_each で展開して JOIN する
            let json = serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_owned());
            let sql = format!(
                "SELECT {SNAPSHOT_COLUMNS} FROM json_each(?) j JOIN tracks t ON t.id = j.value ORDER BY t.id"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map([json], read_snapshot)?;
            let mut rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            rows.dedup_by_key(|r| r.id);
            Ok(rows)
        }
        Selection::Filter {
            filter,
            exclude_ids,
        } => {
            let mut w = filter_where(filter);
            if !exclude_ids.is_empty() {
                let json = serde_json::to_string(exclude_ids).unwrap_or_else(|_| "[]".to_owned());
                w.push(
                    "t.id NOT IN (SELECT value FROM json_each(?))",
                    [Value::from(json)],
                );
            }
            let sql = format!(
                "SELECT {SNAPSHOT_COLUMNS} FROM tracks t WHERE {} ORDER BY t.id",
                w.sql()
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(w.params), read_snapshot)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }
    }
}

// ---------------------------------------------------------------- albums

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AlbumRow {
    pub id: i64,
    pub rel_dir: String,
    pub category: Option<String>,
    pub albumartist: Option<String>,
    pub album: Option<String>,
    pub date: Option<String>,
    pub original_date: Option<String>,
    pub edition: Option<String>,
    pub mb_release_id: Option<String>,
    pub disc_count: Option<i64>,
    pub artwork_id: Option<i64>,
    /// active なトラック数
    pub track_count: i64,
    pub duration_ms: i64,
    pub missing_since: Option<i64>,
}

const ALBUM_SQL: &str = "SELECT a.id, a.rel_dir, c.name, a.albumartist, a.album, a.date, a.original_date, a.edition,
  a.mb_release_id, a.disc_count, a.artwork_id,
  (SELECT count(*) FROM tracks t WHERE t.album_id = a.id AND t.missing_since IS NULL),
  (SELECT coalesce(sum(t.duration_ms), 0) FROM tracks t WHERE t.album_id = a.id AND t.missing_since IS NULL),
  a.missing_since
FROM albums a
LEFT JOIN categories c ON c.id = a.category_id";

fn read_album(r: &Row) -> rusqlite::Result<AlbumRow> {
    Ok(AlbumRow {
        id: r.get(0)?,
        rel_dir: r.get(1)?,
        category: r.get(2)?,
        albumartist: r.get(3)?,
        album: r.get(4)?,
        date: r.get(5)?,
        original_date: r.get(6)?,
        edition: r.get(7)?,
        mb_release_id: r.get(8)?,
        disc_count: r.get(9)?,
        artwork_id: r.get(10)?,
        track_count: r.get(11)?,
        duration_ms: r.get(12)?,
        missing_since: r.get(13)?,
    })
}

/// 全アルバム。albumartist, album, id 順
pub fn list_albums(conn: &Connection) -> Result<Vec<AlbumRow>> {
    let sql = format!("{ALBUM_SQL}\nORDER BY a.albumartist, a.album, a.id");
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map([], read_album)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn get_album(conn: &Connection, id: i64) -> Result<Option<AlbumRow>> {
    let sql = format!("{ALBUM_SQL}\nWHERE a.id = ?");
    let mut stmt = conn.prepare_cached(&sql)?;
    Ok(stmt.query_row([id], read_album).optional()?)
}

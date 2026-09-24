//! 再生リストの購読 `playlist_subscriptions`（SPEC §7.7「再生リストの購読」、D-78、P4-16）。
//!
//! - 追記先の同一性は `album_id`（登録時は NULL。同期か配置が解決したときに CAS で束ねる。
//!   album 全体の移動は id を維持する。D-32）。`albumartist` / `album` / `category` は束ねる前の
//!   初期値と表示用で、変えると `album_id` は NULL に戻る（再解決）
//! - 同じ追記先の購読は 1 つだけ（`target_key` = canonical key の組。UNIQUE）。`album_id` も
//!   非 NULL の間は UNIQUE
//! - `sync_requested_at` は承認の後続・手動要求の latch。同期の開始（[`begin_attempt`]）で消し、
//!   終了時に立っていれば Requeue、残れば dispatcher が [`requested`] で回収する。時刻の比較には
//!   使わない（UNIX 秒精度で同秒の要求を落とすため）
//! - `last_attempted_at` は同期の開始時刻（成否を問わない。定期投入の基準）、`last_synced_at` は
//!   成功の終端

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::Result;
use crate::domain::pathgen::sanitize_component;
use crate::domain::relpath::canonical_key;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Subscription {
    pub id: i64,
    pub list_id: String,
    pub url: String,
    pub album_id: Option<i64>,
    pub albumartist: String,
    pub album: String,
    pub category: Option<String>,
    pub align: bool,
    pub enabled: bool,
    pub max_enqueue: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_attempted_at: Option<i64>,
    pub last_synced_at: Option<i64>,
    pub sync_requested_at: Option<i64>,
    pub last_result: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSubscription {
    pub list_id: String,
    pub url: String,
    pub albumartist: String,
    pub album: String,
    pub category: Option<String>,
    pub align: bool,
    pub enabled: bool,
    pub max_enqueue: i64,
}

/// 変更する欄だけ Some。`category` は `Some(None)` で消す
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Patch {
    pub albumartist: Option<String>,
    pub album: Option<String>,
    #[serde(default, with = "double_option")]
    pub category: Option<Option<String>>,
    pub align: Option<bool>,
    pub enabled: Option<bool>,
    pub max_enqueue: Option<i64>,
}

/// JSON の `"category": null`（消す）と欄なし（触らない）を区別する
mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D>(d: D) -> std::result::Result<Option<Option<String>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<String>::deserialize(d).map(Some)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Ok(i64),
    NotFound,
    /// 同じ `list_id` の購読がある
    DuplicateList,
    /// 同じ追記先（albumartist + album）の購読がある
    DuplicateTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindOutcome {
    Bound,
    NotFound,
    /// 既に束ねてある（その album_id）。別の album でも触らない
    AlreadyBound(i64),
    /// その album は別の購読が束ねている（その購読 id）
    TakenBy(i64),
}

/// 追記先の一意キー。Inbox の宛先ディレクトリ `<albumartist>/<album>`（要素は `sanitize_component`）と
/// 同じ同値関係（casefold + NFD）
pub fn target_key(albumartist: &str, album: &str) -> String {
    canonical_key(&format!(
        "{}/{}",
        sanitize_component(albumartist.trim()),
        sanitize_component(album.trim())
    ))
}

const COLUMNS: &str =
    "id, list_id, url, album_id, albumartist, album, category, align, enabled, max_enqueue,
     created_at, updated_at, last_attempted_at, last_synced_at, sync_requested_at, last_result";

fn row_to_subscription(r: &rusqlite::Row<'_>) -> rusqlite::Result<Subscription> {
    let last_result: Option<String> = r.get(15)?;
    Ok(Subscription {
        id: r.get(0)?,
        list_id: r.get(1)?,
        url: r.get(2)?,
        album_id: r.get(3)?,
        albumartist: r.get(4)?,
        album: r.get(5)?,
        category: r.get(6)?,
        align: r.get::<_, i64>(7)? == 1,
        enabled: r.get::<_, i64>(8)? == 1,
        max_enqueue: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
        last_attempted_at: r.get(12)?,
        last_synced_at: r.get(13)?,
        sync_requested_at: r.get(14)?,
        // CHECK (json_valid) なので読めないことはないが、壊れていたら None（無いのと同じ）
        last_result: last_result.and_then(|s| serde_json::from_str(&s).ok()),
    })
}

/// albumartist / album 順
pub fn list(conn: &Connection) -> Result<Vec<Subscription>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM playlist_subscriptions ORDER BY albumartist, album, id"
    ))?;
    let rows = st
        .query_map([], row_to_subscription)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Subscription>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM playlist_subscriptions WHERE id = ?1"
    ))?;
    Ok(st.query_row([id], row_to_subscription).optional()?)
}

/// `list_id` の購読（YouTube 画面の照合。D-87）。`list_id` は UNIQUE なので 0 / 1 件
pub fn by_list_id(conn: &Connection, list_id: &str) -> Result<Option<Subscription>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM playlist_subscriptions WHERE list_id = ?1"
    ))?;
    Ok(st.query_row([list_id], row_to_subscription).optional()?)
}

fn target_taken(conn: &Connection, key: &str, except: Option<i64>) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM playlist_subscriptions WHERE target_key = ?1 AND id IS NOT ?2",
        params![key, except],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// 登録する。`list_id` と追記先の重複は事前に検査する（書き込みは単一コネクションなので競合しない。
/// UNIQUE 制約は控え）
pub fn insert(conn: &Connection, s: &NewSubscription, now: i64) -> Result<WriteOutcome> {
    let exists: bool = conn.query_row(
        "SELECT count(*) FROM playlist_subscriptions WHERE list_id = ?1",
        [&s.list_id],
        |r| r.get::<_, i64>(0).map(|n| n > 0),
    )?;
    if exists {
        return Ok(WriteOutcome::DuplicateList);
    }
    let key = target_key(&s.albumartist, &s.album);
    if target_taken(conn, &key, None)? {
        return Ok(WriteOutcome::DuplicateTarget);
    }
    conn.execute(
        "INSERT INTO playlist_subscriptions
           (list_id, url, target_key, albumartist, album, category, align, enabled, max_enqueue,
            created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
        params![
            s.list_id,
            s.url,
            key,
            s.albumartist.trim(),
            s.album.trim(),
            s.category
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty()),
            s.align as i64,
            s.enabled as i64,
            s.max_enqueue,
            now,
        ],
    )?;
    Ok(WriteOutcome::Ok(conn.last_insert_rowid()))
}

/// 変更する。albumartist / album / category のどれかが変わると `album_id` を NULL に戻す（再解決）
pub fn update(conn: &Connection, id: i64, patch: &Patch, now: i64) -> Result<WriteOutcome> {
    let Some(cur) = get(conn, id)? else {
        return Ok(WriteOutcome::NotFound);
    };
    let albumartist = patch
        .albumartist
        .as_deref()
        .map(str::trim)
        .unwrap_or(&cur.albumartist)
        .to_owned();
    let album = patch
        .album
        .as_deref()
        .map(str::trim)
        .unwrap_or(&cur.album)
        .to_owned();
    let category = match &patch.category {
        Some(c) => c
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_owned),
        None => cur.category.clone(),
    };
    let key = target_key(&albumartist, &album);
    if target_taken(conn, &key, Some(id))? {
        return Ok(WriteOutcome::DuplicateTarget);
    }
    let target_changed =
        albumartist != cur.albumartist || album != cur.album || category != cur.category;
    conn.execute(
        "UPDATE playlist_subscriptions
            SET albumartist = ?2, album = ?3, category = ?4, target_key = ?5, align = ?6, enabled = ?7,
                max_enqueue = ?8, updated_at = ?9,
                album_id = CASE WHEN ?10 THEN NULL ELSE album_id END
          WHERE id = ?1",
        params![
            id,
            albumartist,
            album,
            category,
            key,
            patch.align.unwrap_or(cur.align) as i64,
            patch.enabled.unwrap_or(cur.enabled) as i64,
            patch.max_enqueue.unwrap_or(cur.max_enqueue),
            now,
            target_changed,
        ],
    )?;
    Ok(WriteOutcome::Ok(id))
}

pub fn delete(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.execute("DELETE FROM playlist_subscriptions WHERE id = ?1", [id])? > 0)
}

/// 追記先の album を束ねる（CAS: `album_id IS NULL` のときだけ）。他の購読が束ねている album なら
/// `TakenBy`
pub fn bind_album(conn: &Connection, id: i64, album_id: i64) -> Result<BindOutcome> {
    let Some(cur) = get(conn, id)? else {
        return Ok(BindOutcome::NotFound);
    };
    if let Some(bound) = cur.album_id {
        return Ok(BindOutcome::AlreadyBound(bound));
    }
    let taken: Option<i64> = conn
        .query_row(
            "SELECT id FROM playlist_subscriptions WHERE album_id = ?1",
            [album_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(other) = taken {
        return Ok(BindOutcome::TakenBy(other));
    }
    conn.execute(
        "UPDATE playlist_subscriptions SET album_id = ?2 WHERE id = ?1 AND album_id IS NULL",
        params![id, album_id],
    )?;
    Ok(BindOutcome::Bound)
}

/// 同期を要求する（latch を立てる）。立っていれば時刻だけ更新
pub fn request_sync(conn: &Connection, id: i64, now: i64) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE playlist_subscriptions SET sync_requested_at = ?2 WHERE id = ?1",
        params![id, now],
    )? > 0)
}

/// latch の立っている購読 id（dispatcher が拾う。enabled に関わらず）
pub fn requested(conn: &Connection) -> Result<Vec<i64>> {
    let mut st = conn.prepare_cached(
        "SELECT id FROM playlist_subscriptions WHERE sync_requested_at IS NOT NULL ORDER BY id",
    )?;
    let ids = st
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(ids)
}

pub fn is_requested(conn: &Connection, id: i64) -> Result<bool> {
    let v: Option<Option<i64>> = conn
        .query_row(
            "SELECT sync_requested_at FROM playlist_subscriptions WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(matches!(v, Some(Some(_))))
}

/// 同期の開始: latch を消し `last_attempted_at` を書いて、その行を返す（消えていれば None）
pub fn begin_attempt(conn: &Connection, id: i64, now: i64) -> Result<Option<Subscription>> {
    conn.execute(
        "UPDATE playlist_subscriptions SET last_attempted_at = ?2, sync_requested_at = NULL WHERE id = ?1",
        params![id, now],
    )?;
    get(conn, id)
}

/// 成功の終端: `last_synced_at` と結果
pub fn finish_attempt(
    conn: &Connection,
    id: i64,
    now: i64,
    result: &serde_json::Value,
) -> Result<()> {
    conn.execute(
        "UPDATE playlist_subscriptions SET last_synced_at = ?2, last_result = ?3 WHERE id = ?1",
        params![id, now, result.to_string()],
    )?;
    Ok(())
}

/// 結果だけ記録する（失敗。`last_synced_at` は進めない）
pub fn set_result(conn: &Connection, id: i64, result: &serde_json::Value) -> Result<()> {
    conn.execute(
        "UPDATE playlist_subscriptions SET last_result = ?2 WHERE id = ?1",
        params![id, result.to_string()],
    )?;
    Ok(())
}

/// 定期投入の対象: enabled で、一度も試していないか `last_attempted_at` から `interval_secs` 経った購読。
/// 時計が戻って `last_attempted_at` が未来なら due にしない
pub fn due(conn: &Connection, now: i64, interval_secs: i64) -> Result<Vec<i64>> {
    let mut st = conn.prepare_cached(
        "SELECT id FROM playlist_subscriptions
          WHERE enabled = 1
            AND (last_attempted_at IS NULL
                 OR (last_attempted_at <= ?1 AND ?1 - last_attempted_at >= ?2))
          ORDER BY id",
    )?;
    let ids = st
        .query_map(params![now, interval_secs], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(ids)
}

/// その購読の同期ジョブが queued / running か（PATCH は走行中を 409 にする。走行中の同期が古い追記先で
/// 揃えたり投入したりしないための規則。書き込みは単一コネクションなので、この検査と UPDATE を同じ閉包で
/// 行えば投入と競合しない）
pub fn sync_active(conn: &Connection, id: i64) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM jobs WHERE dedup_key = ?1 AND state IN ('queued', 'running')",
        [format!("playlist_sync:{id}")],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

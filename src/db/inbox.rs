//! Inbox の承認キュー（`inbox_items` / `inbox_files`。SPEC §7.8、D-68、P2-10）。
//! 正は Inbox のファイルで、ここは走査が写したキャッシュ。状態は
//! `pending` → `approved` → `placing` → `placed` | `failed`、`rejected`

use rusqlite::{params, Connection, OptionalExtension as _};
use serde::{Deserialize, Serialize};

use super::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemState {
    Pending,
    Approved,
    Placing,
    Placed,
    Rejected,
    Failed,
}

impl ItemState {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemState::Pending => "pending",
            ItemState::Approved => "approved",
            ItemState::Placing => "placing",
            ItemState::Placed => "placed",
            ItemState::Rejected => "rejected",
            ItemState::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<ItemState> {
        Some(match s {
            "pending" => ItemState::Pending,
            "approved" => ItemState::Approved,
            "placing" => ItemState::Placing,
            "placed" => ItemState::Placed,
            "rejected" => ItemState::Rejected,
            "failed" => ItemState::Failed,
            _ => return None,
        })
    }
}

/// 1 件（音声ファイルのあるディレクトリ）
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    pub id: i64,
    pub rel_dir: String,
    pub state: ItemState,
    pub detected_at: i64,
    pub seen_at: i64,
    pub approved_at: Option<i64>,
    pub draft: Option<serde_json::Value>,
    pub error: Option<String>,
    pub placed_album_id: Option<i64>,
    pub placed_at: Option<i64>,
    /// 破棄待ち（rejected の件の「削除」。GC が `[gc].retention_days` 経過後にファイルと行を消す。D-90）
    pub discard_requested_at: Option<i64>,
    /// Cover Art Archive から取った表の画像（`<mime>:<sha256hex>`。提案の picture の初期値。D-91）
    pub caa_picture: Option<String>,
    /// 表の画像の取得を試みた回数（[`CAA_MAX_TRIES`] で打ち止め。D-91）
    pub caa_tries: i64,
}

/// 表の画像の取得の上限回数（1 回目の失敗は次の走査でもう 1 回だけ試す。D-91）
pub const CAA_MAX_TRIES: i64 = 2;

/// 件の中の音声ファイル 1 本
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRow {
    pub rel_path: String,
    pub inode: i64,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub codec: String,
    pub lossless: bool,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub channels: Option<u32>,
    pub duration_ms: Option<u64>,
    /// 正規化済みタグ（キーは大文字。多値は反復）
    pub tags: Vec<(String, String)>,
}

const ITEM_COLS: &str = "id, rel_dir, state, detected_at, seen_at, approved_at, draft, error, placed_album_id, placed_at, discard_requested_at, caa_picture, caa_tries";

fn row_to_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    let state: String = r.get(2)?;
    let draft: Option<String> = r.get(6)?;
    Ok(Item {
        id: r.get(0)?,
        rel_dir: r.get(1)?,
        state: ItemState::parse(&state).unwrap_or(ItemState::Failed),
        detected_at: r.get(3)?,
        seen_at: r.get(4)?,
        approved_at: r.get(5)?,
        draft: draft.and_then(|s| serde_json::from_str(&s).ok()),
        error: r.get(7)?,
        placed_album_id: r.get(8)?,
        placed_at: r.get(9)?,
        discard_requested_at: r.get(10)?,
        caa_picture: r.get(11)?,
        caa_tries: r.get(12)?,
    })
}

/// 全件（新しい順）
pub fn list(conn: &Connection) -> Result<Vec<Item>> {
    let mut st = conn.prepare(&format!(
        "SELECT {ITEM_COLS} FROM inbox_items ORDER BY detected_at DESC, id DESC"
    ))?;
    let rows = st
        .query_map([], row_to_item)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Item>> {
    Ok(conn
        .query_row(
            &format!("SELECT {ITEM_COLS} FROM inbox_items WHERE id = ?1"),
            [id],
            row_to_item,
        )
        .optional()?)
}

pub fn find_by_dir_key(conn: &Connection, key: &str) -> Result<Option<Item>> {
    Ok(conn
        .query_row(
            &format!("SELECT {ITEM_COLS} FROM inbox_items WHERE rel_dir_key = ?1"),
            [key],
            row_to_item,
        )
        .optional()?)
}

/// `state` の件（id 順）
pub fn list_by_state(conn: &Connection, state: ItemState) -> Result<Vec<Item>> {
    let mut st = conn.prepare(&format!(
        "SELECT {ITEM_COLS} FROM inbox_items WHERE state = ?1 ORDER BY id"
    ))?;
    let rows = st
        .query_map([state.as_str()], row_to_item)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 状態ごとの件数（上部バーのバッジ用。P4-20）。一覧を読まずに数える
/// （`list` は下書き / 失敗理由の JSON まで読むので、60 秒ごとに叩く経路には重い）
pub fn count_by_state(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut st = conn.prepare("SELECT state, COUNT(*) FROM inbox_items GROUP BY state")?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn insert_item(conn: &Connection, rel_dir: &str, rel_dir_key: &str, now: i64) -> Result<i64> {
    conn.execute(
        "INSERT INTO inbox_items (rel_dir, rel_dir_key, state, detected_at, seen_at)
         VALUES (?1, ?2, 'pending', ?3, ?3)",
        params![rel_dir, rel_dir_key, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 走査で見た（`seen_at`）
pub fn touch(conn: &Connection, id: i64, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE inbox_items SET seen_at = ?2 WHERE id = ?1",
        params![id, now],
    )?;
    Ok(())
}

pub fn files(conn: &Connection, item_id: i64) -> Result<Vec<FileRow>> {
    let mut st = conn.prepare(
        "SELECT rel_path, inode, size, mtime_ns, ctime_ns, codec, lossless, sample_rate, bit_depth,
                channels, duration_ms, tags
           FROM inbox_files WHERE item_id = ?1 ORDER BY rel_path",
    )?;
    let rows = st
        .query_map([item_id], |r| {
            let tags: String = r.get(11)?;
            Ok(FileRow {
                rel_path: r.get(0)?,
                inode: r.get(1)?,
                size: r.get(2)?,
                mtime_ns: r.get(3)?,
                ctime_ns: r.get(4)?,
                codec: r.get(5)?,
                lossless: r.get::<_, i64>(6)? == 1,
                sample_rate: r.get(7)?,
                bit_depth: r.get(8)?,
                channels: r.get(9)?,
                duration_ms: r.get::<_, Option<i64>>(10)?.map(|d| d as u64),
                tags: serde_json::from_str(&tags).unwrap_or_default(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 件のファイルを入れ替える（`rel_path_key` は `rel_path` から作る）
pub fn replace_files(conn: &Connection, item_id: i64, files: &[FileRow]) -> Result<()> {
    conn.execute("DELETE FROM inbox_files WHERE item_id = ?1", [item_id])?;
    let mut st = conn.prepare_cached(
        "INSERT INTO inbox_files (item_id, rel_path, rel_path_key, inode, size, mtime_ns, ctime_ns,
                                  codec, lossless, sample_rate, bit_depth, channels, duration_ms, tags)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
    )?;
    for f in files {
        let tags = serde_json::to_string(&f.tags)
            .map_err(|e| super::DbError::Internal(format!("タグを JSON にできない: {e}")))?;
        st.execute(params![
            item_id,
            f.rel_path,
            crate::domain::relpath::canonical_key(&f.rel_path),
            f.inode,
            f.size,
            f.mtime_ns,
            f.ctime_ns,
            f.codec,
            f.lossless as i64,
            f.sample_rate,
            f.bit_depth,
            f.channels,
            f.duration_ms.map(|d| d as i64),
            tags,
        ])?;
    }
    Ok(())
}

/// 状態を変える。`approved` にすると `approved_at`、それ以外に戻すと `error` を置き換える。
/// rejected 以外にすると破棄待ちを解く（D-90）
pub fn set_state(
    conn: &Connection,
    id: i64,
    state: ItemState,
    error: Option<&str>,
    now: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE inbox_items
            SET state = ?2, error = ?3,
                approved_at = CASE WHEN ?2 = 'approved' THEN ?4 ELSE approved_at END,
                discard_requested_at = CASE WHEN ?2 = 'rejected' THEN discard_requested_at ELSE NULL END
          WHERE id = ?1",
        params![id, state.as_str(), error, now],
    )?;
    Ok(())
}

/// 状態遷移の CAS: 今の状態が `from` のどれかであるときだけ `to` にする。変えたら true。
/// 遷移は破棄待ちを解く（rejected → rejected は無いので、rejected から出る遷移で必ず NULL になる。D-90）。
/// 状態検査と更新を 1 文にして、読んでから書くまでの間に他（API / worker / 走査）が動かした
/// 件を上書きしない
pub fn transition(
    conn: &Connection,
    id: i64,
    from: &[ItemState],
    to: ItemState,
    error: Option<&str>,
    now: i64,
) -> Result<bool> {
    if from.is_empty() {
        return Ok(false);
    }
    // プレースホルダの数だけを組み立てる（値は全部バインド）
    let marks = (0..from.len())
        .map(|i| format!("?{}", i + 5))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "UPDATE inbox_items
            SET state = ?2, error = ?3,
                approved_at = CASE WHEN ?2 = 'approved' THEN ?4 ELSE approved_at END,
                discard_requested_at = NULL
          WHERE id = ?1 AND state IN ({marks})"
    );
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![
        Box::new(id),
        Box::new(to.as_str()),
        Box::new(error.map(str::to_owned)),
        Box::new(now),
    ];
    for f in from {
        params.push(Box::new(f.as_str()));
    }
    let n = conn.execute(&sql, rusqlite::params_from_iter(params.iter()))?;
    Ok(n == 1)
}

/// 破棄要求（D-90）: rejected で破棄待ちでない件に時刻を入れる。入れたら true
pub fn request_discard(conn: &Connection, id: i64, now: i64) -> Result<bool> {
    let n = conn.execute(
        "UPDATE inbox_items SET discard_requested_at = ?2
          WHERE id = ?1 AND state = 'rejected' AND discard_requested_at IS NULL",
        params![id, now],
    )?;
    Ok(n == 1)
}

/// 破棄待ちを解いて rejected に戻す（D-90）。`note` は `error` に残す理由（人の取り消しは None）。
/// 解いたら true
pub fn cancel_discard(conn: &Connection, id: i64, note: Option<&str>) -> Result<bool> {
    let n = conn.execute(
        "UPDATE inbox_items SET discard_requested_at = NULL, error = ?2
          WHERE id = ?1 AND state = 'rejected' AND discard_requested_at IS NOT NULL",
        params![id, note],
    )?;
    Ok(n == 1)
}

/// GC の対象: rejected で `discard_requested_at <= cutoff` の件（id 順。D-90）
pub fn discard_due(conn: &Connection, cutoff: i64) -> Result<Vec<Item>> {
    let mut st = conn.prepare(&format!(
        "SELECT {ITEM_COLS} FROM inbox_items
          WHERE state = 'rejected' AND discard_requested_at IS NOT NULL AND discard_requested_at <= ?1
          ORDER BY id"
    ))?;
    let rows = st
        .query_map([cutoff], row_to_item)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// GC が件を消す（CAS: まだ rejected で `discard_requested_at <= cutoff` のときだけ。D-90）。消したら true
pub fn delete_discarded(conn: &Connection, id: i64, cutoff: i64) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM inbox_items
          WHERE id = ?1 AND state = 'rejected' AND discard_requested_at IS NOT NULL
            AND discard_requested_at <= ?2",
        params![id, cutoff],
    )?;
    Ok(n == 1)
}

/// 起動 / ジョブ開始時の回復: 前のプロセスが配置の途中で落ちて `placing` のまま残った件を
/// `approved` に戻す（配置は冪等なので再実行してよい）。戻した件数を返す
pub fn recover_placing(conn: &Connection) -> Result<usize> {
    let n = conn.execute(
        "UPDATE inbox_items SET state = 'approved', error = NULL WHERE state = 'placing'",
        [],
    )?;
    Ok(n)
}

pub fn set_draft(conn: &Connection, id: i64, draft: &serde_json::Value) -> Result<()> {
    conn.execute(
        "UPDATE inbox_items SET draft = ?2 WHERE id = ?1",
        params![id, draft.to_string()],
    )?;
    Ok(())
}

/// 配置済み
pub fn set_placed(conn: &Connection, id: i64, album_id: i64, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE inbox_items SET state = 'placed', error = NULL, placed_album_id = ?2, placed_at = ?3
          WHERE id = ?1",
        params![id, album_id, now],
    )?;
    Ok(())
}

pub fn delete_item(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM inbox_items WHERE id = ?1", [id])?;
    Ok(())
}

/// 走査で見なかった件（`seen_at < seen_before`）のうち `placed` 以外
pub fn stale_items(conn: &Connection, seen_before: i64) -> Result<Vec<Item>> {
    let mut st = conn.prepare(&format!(
        "SELECT {ITEM_COLS} FROM inbox_items WHERE seen_at < ?1 AND state <> 'placed' ORDER BY id"
    ))?;
    let rows = st
        .query_map([seen_before], row_to_item)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 追記先の album にある、同じタイトル鍵の active なトラック（P4-19。承認画面の警告）。
/// 鍵の計算は `import::inbox::title_key`（DB 側で正規化はできないので、album の行を引いて Rust で畳む。
/// album 単位なので行数は多くない）
pub fn same_title_in_album(
    conn: &Connection,
    album_id: i64,
    title_key: &str,
) -> Result<Vec<crate::import::inbox::SameTitle>> {
    let mut st = conn.prepare(
        "SELECT t.id, t.rel_path, t.duration_ms, ti.value
           FROM tracks t
           JOIN track_tags ti ON ti.track_id = t.id AND ti.key = 'TITLE' AND ti.idx = 0
          WHERE t.album_id = ?1 AND t.missing_since IS NULL
          ORDER BY t.disc_no, t.track_no, t.id",
    )?;
    let rows = st.query_map([album_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<i64>>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, rel_path, duration_ms, title) = row?;
        if crate::import::inbox::title_key(&title) == title_key {
            out.push(crate::import::inbox::SameTitle {
                track_id: id,
                rel_path,
                duration_ms,
            });
        }
    }
    Ok(out)
}

/// 走査の他に inbox ジョブがやることが残っているか: 配置待ち（`approved`。承認 API の投入が Requeue や
/// 再起動で消えた後の保険）と、期限切れの `placed`（片付け）と、表の画像をまだ試していない承認前の取り込み
/// （1 回の実行で取る件数に上限があるので、残りを次の周回で続ける。D-91）。周期の監視が Inbox に変化が無くても
/// 投入する理由（P4-18）
/// 表の画像を数えるのは `include_covers`（inbox ジョブが Cover Art Archive を使えるとき）だけ。使えないのに
/// 数えると候補が減らず、毎周回投入し続ける
pub fn needs_attention(
    conn: &Connection,
    placed_before: i64,
    include_covers: bool,
) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM inbox_items
         WHERE state = 'approved'
            OR (state = 'placed' AND placed_at IS NOT NULL AND placed_at < ?1)
            OR (?3 AND state IN ('pending', 'failed') AND caa_picture IS NULL AND caa_tries < ?2)",
        params![placed_before, CAA_MAX_TRIES, include_covers],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// `placed` で `placed_at < placed_before` の件を消す。返り値は消した数
pub fn expire_placed(conn: &Connection, placed_before: i64) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM inbox_items WHERE state = 'placed' AND placed_at IS NOT NULL AND placed_at < ?1",
        [placed_before],
    )?)
}

// ---------------------------------------------------------------- 重複取り込みの判定（D-70）

/// `SOURCE_URL` タグが同じ値のファイルの所在
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceLocated {
    /// Library の active なトラック（rel_path）
    Library(String),
    /// Inbox の件のファイル（Inbox 相対）
    Inbox(String),
}

/// `SOURCE_URL = url` を持つファイルが Library（`track_tags`）か Inbox（`inbox_files.tags`）にあれば
/// その所在。ダウンローダが同じ動画を二度取り込まないための判定
pub fn find_source_url(conn: &Connection, url: &str) -> Result<Option<SourceLocated>> {
    let lib: Option<String> = conn
        .query_row(
            "SELECT t.rel_path FROM track_tags tt JOIN tracks t ON t.id = tt.track_id
              WHERE tt.key = 'SOURCE_URL' AND tt.value = ?1 AND t.missing_since IS NULL
              ORDER BY t.id LIMIT 1",
            [url],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(p) = lib {
        return Ok(Some(SourceLocated::Library(p)));
    }
    let inbox: Option<String> = conn
        .query_row(
            "SELECT f.rel_path FROM inbox_files f, json_each(f.tags) je
              WHERE json_extract(je.value, '$[0]') = 'SOURCE_URL'
                AND json_extract(je.value, '$[1]') = ?1
              ORDER BY f.item_id LIMIT 1",
            [url],
            |r| r.get(0),
        )
        .optional()?;
    Ok(inbox.map(SourceLocated::Inbox))
}

// ---------------------------------------------------------------- 再生リストの同期（P4-16、D-78）

/// `SOURCE_URL = url` を持つ Library の active な行（同期は URL ごとに全件見る。0 / 1 / 2 件以上を区別）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRow {
    pub track_id: i64,
    pub album_id: Option<i64>,
    pub rel_path: String,
}

pub fn library_rows_by_source_url(conn: &Connection, url: &str) -> Result<Vec<SourceRow>> {
    let mut st = conn.prepare_cached(
        "SELECT t.id, t.album_id, t.rel_path FROM track_tags tt JOIN tracks t ON t.id = tt.track_id
          WHERE tt.key = 'SOURCE_URL' AND tt.value = ?1 AND t.missing_since IS NULL
          ORDER BY t.album_id, t.id",
    )?;
    let rows = st
        .query_map([url], |r| {
            Ok(SourceRow {
                track_id: r.get(0)?,
                album_id: r.get(1)?,
                rel_path: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Inbox の件のファイルに `SOURCE_URL = url` があるか（取り込み中）
pub fn inbox_has_source_url(conn: &Connection, url: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM inbox_files f, json_each(f.tags) je
          WHERE json_extract(je.value, '$[0]') = 'SOURCE_URL'
            AND json_extract(je.value, '$[1]') = ?1",
        [url],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// album の active な行と `SOURCE_URL`（先頭値）。番号揃えの入力（id 順）
pub fn album_rows(
    conn: &Connection,
    album_id: i64,
) -> Result<Vec<crate::import::ytmusic::playlist::LibraryRow>> {
    let mut st = conn.prepare_cached(
        "SELECT t.id, t.disc_no, t.track_no,
                (SELECT value FROM track_tags WHERE track_id = t.id AND key = 'SOURCE_URL' AND idx = 0)
           FROM tracks t WHERE t.album_id = ?1 AND t.missing_since IS NULL ORDER BY t.id",
    )?;
    let rows = st
        .query_map([album_id], |r| {
            Ok(crate::import::ytmusic::playlist::LibraryRow {
                track_id: r.get(0)?,
                disc_no: r.get(1)?,
                track_no: r.get(2)?,
                source_url: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 表の画像を取りに行く件（D-91）: 承認前（pending / failed）で、まだ画像が無く、上限に達していないもの（id 順）
pub fn caa_candidates(conn: &Connection) -> Result<Vec<Item>> {
    let mut st = conn.prepare(&format!(
        "SELECT {ITEM_COLS} FROM inbox_items
          WHERE state IN ('pending', 'failed') AND caa_picture IS NULL AND caa_tries < ?1
          ORDER BY id"
    ))?;
    let rows = st
        .query_map([CAA_MAX_TRIES], row_to_item)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 表の画像の取得を 1 回分 claim する（D-91）。**外へ出る前に**回数を進めて永続化する（通信の途中で落ちても
/// 回数は戻らない = 上限を再起動で超えない）。`expected` は読んだときの回数で、承認前・画像なし・上限未満・
/// 回数が変わっていないときだけ進める（CAS）。進めたら新しい回数
pub fn claim_caa(conn: &Connection, id: i64, expected: i64) -> Result<Option<i64>> {
    let n = conn.execute(
        "UPDATE inbox_items SET caa_tries = caa_tries + 1
          WHERE id = ?1 AND caa_tries = ?2 AND caa_tries < ?3 AND caa_picture IS NULL
            AND state IN ('pending', 'failed')",
        params![id, expected, CAA_MAX_TRIES],
    )?;
    Ok((n == 1).then_some(expected + 1))
}

/// 表の画像の取得の結果を記録する（D-91）。`picture` があれば置き、回数は `tries` 以上にする（claim で進めた
/// 回数を戻さない）。既に画像がある件は触らない（CAS）。記録したら true
pub fn record_caa(conn: &Connection, id: i64, picture: Option<&str>, tries: i64) -> Result<bool> {
    let n = conn.execute(
        "UPDATE inbox_items SET caa_picture = ?2, caa_tries = max(caa_tries, ?3)
          WHERE id = ?1 AND caa_picture IS NULL",
        params![id, picture, tries],
    )?;
    Ok(n == 1)
}

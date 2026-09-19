//! スキャン用の DB 操作（SPEC §7.1）。`scan_runs` の生存期間、既存行のスナップショット、
//! Phase 4 の commit で使う行単位の更新。SQL はすべてバインドパラメータで、列名は固定文字列。

use rusqlite::{params, Connection, OptionalExtension};

use super::Result;
use crate::domain::relpath::canonical_key;
use crate::domain::tags::TagSet;
use crate::media::artwork::TrackPicture;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanKind {
    Incremental,
    Deep,
}

impl ScanKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ScanKind::Incremental => "incremental",
            ScanKind::Deep => "deep",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Completed,
    Failed,
    Cancelled,
}

impl RunState {
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Completed => "completed",
            RunState::Failed => "failed",
            RunState::Cancelled => "cancelled",
        }
    }
}

pub fn create_run(conn: &Connection, kind: ScanKind, now: i64) -> Result<i64> {
    conn.execute(
        "INSERT INTO scan_runs (kind, state, started_at) VALUES (?1, 'running', ?2)",
        params![kind.as_str(), now],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn finish_run(
    conn: &Connection,
    run_id: i64,
    state: RunState,
    files_seen: i64,
    errors: i64,
    now: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE scan_runs SET state = ?2, finished_at = ?3, files_seen = ?4, errors = ?5
         WHERE id = ?1",
        params![run_id, state.as_str(), now, files_seen, errors],
    )?;
    Ok(())
}

/// 最後に完了した deep scan の開始時刻
pub fn last_completed_deep_started_at(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT started_at FROM scan_runs WHERE kind = 'deep' AND state = 'completed'
             ORDER BY started_at DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?)
}

/// 既存トラック行のスナップショット（active と missing の両方）。pending op と版・ハッシュの
/// 現在値は commit トランザクションの中で読み直す（[`load_pending_ops`] / [`load_current_row`]）
#[derive(Debug, Clone)]
pub struct TrackSnap {
    pub id: i64,
    pub rel_path: String,
    pub rel_path_key: String,
    pub album_id: Option<i64>,
    pub dev: Option<u64>,
    pub inode: Option<u64>,
    pub nlink: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub audio_md5: Option<[u8; 16]>,
    pub audio_fp: Option<[u8; 32]>,
    pub tag_hash: Option<[u8; 32]>,
    pub codec: String,
    pub tag_version: i64,
    pub audio_version: i64,
    pub missing: bool,
    /// 画像をキャッシュへ置けなかったので、変更が無くても次のスキャンで読み直す（D-61）
    pub artwork_dirty: bool,
}

#[derive(Debug, Clone)]
pub struct PendingOp {
    pub op_id: i64,
    pub kind: String,
    /// 記録時点の物理パス（rename op の外部移動判定に使う。DB の `rel_path` は overlay で
    /// 先に変わっているかもしれない）
    pub expected_rel_path: Option<String>,
    /// op が置く先のパス（`edits.rel_path` の新値。rename / archive op）。archive op は DB を
    /// 先行更新しないので、宛先に現れたファイルはここで「自分の作業中」と判定する（P1-4）
    pub target_rel_path: Option<String>,
}

fn blob_array<const N: usize>(v: Option<Vec<u8>>) -> Option<[u8; N]> {
    v.and_then(|b| b.try_into().ok())
}

pub fn load_track_snapshot(conn: &Connection) -> Result<Vec<TrackSnap>> {
    let mut stmt = conn.prepare(
        "SELECT id, rel_path, rel_path_key, album_id, dev, inode, nlink, size,
                mtime_ns, ctime_ns, audio_md5, audio_fp, tag_hash, codec,
                tag_version, audio_version, missing_since, artwork_dirty
         FROM tracks",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(TrackSnap {
            id: r.get(0)?,
            rel_path: r.get(1)?,
            rel_path_key: r.get(2)?,
            album_id: r.get(3)?,
            dev: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
            inode: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
            nlink: r.get::<_, i64>(6)? as u64,
            size: r.get::<_, i64>(7)? as u64,
            mtime_ns: r.get(8)?,
            ctime_ns: r.get(9)?,
            audio_md5: blob_array(r.get(10)?),
            audio_fp: blob_array(r.get(11)?),
            tag_hash: blob_array(r.get(12)?),
            codec: r.get(13)?,
            tag_version: r.get(14)?,
            audio_version: r.get(15)?,
            missing: r.get::<_, Option<i64>>(16)?.is_some(),
            artwork_dirty: r.get::<_, i64>(17)? == 1,
        })
    })?;
    rows.map(|r| r.map_err(Into::into)).collect()
}

#[derive(Debug, Clone)]
pub struct AlbumSnap {
    pub id: i64,
    pub rel_dir: String,
    pub rel_dir_key: String,
    pub mb_release_id: Option<String>,
    pub discid: Option<String>,
    pub missing: bool,
}

pub fn load_album_snapshot(conn: &Connection) -> Result<Vec<AlbumSnap>> {
    let mut stmt = conn.prepare(
        "SELECT id, rel_dir, rel_dir_key, mb_release_id, discid, missing_since FROM albums",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(AlbumSnap {
            id: r.get(0)?,
            rel_dir: r.get(1)?,
            rel_dir_key: r.get(2)?,
            mb_release_id: r.get(3)?,
            discid: r.get(4)?,
            missing: r.get::<_, Option<i64>>(5)?.is_some(),
        })
    })?;
    rows.map(|r| r.map_err(Into::into)).collect()
}

/// 統制語彙: `(id, canonical_key(name))`
pub fn load_categories(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare("SELECT id, name FROM categories")?;
    let rows = stmt.query_map([], |r| {
        let name: String = r.get(1)?;
        Ok((r.get(0)?, canonical_key(&name)))
    })?;
    rows.map(|r| r.map_err(Into::into)).collect()
}

/// GENRE → category: `(canonical_key(genre), category_id)`
pub fn load_genre_map(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare("SELECT genre, category_id FROM genre_category_map")?;
    let rows = stmt.query_map([], |r| {
        let genre: String = r.get(0)?;
        Ok((canonical_key(&genre), r.get(1)?))
    })?;
    rows.map(|r| r.map_err(Into::into)).collect()
}

// ---------------------------------------------------------------- 行の更新

/// stat 由来の物理属性
#[derive(Debug, Clone, Copy)]
pub struct Physical {
    pub dev: u64,
    pub inode: u64,
    pub nlink: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

/// 表示・ソート用キャッシュ列（`album` は albums から同期するので含めない）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheColumns {
    pub title: Option<String>,
    pub artist_display: Option<String>,
    pub albumartist: Option<String>,
    pub track_no: Option<i64>,
    pub disc_no: Option<i64>,
    pub date: Option<String>,
}

/// タグ読込の結果のうち DB に書くもの
#[derive(Debug, Clone)]
pub struct TrackContent {
    pub codec: String,
    pub lossless: bool,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub channels: Option<u32>,
    pub bitrate: Option<u32>,
    pub duration_ms: Option<u64>,
    pub tags: TagSet,
    pub tag_hash: [u8; 32],
    pub cache: CacheColumns,
    /// トラック自身の埋め込み画像（`tracks.artwork_id`。D-61）
    pub picture: PictureState,
}

/// [`TrackContent::picture`]: 画像を実体ごと読んでキャッシュへ置いたかどうか
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PictureState {
    /// 読んでいない（store が無い、画像を読まない経路）。`artwork_id` は触らない
    #[default]
    Unread,
    /// 読んだが画像は無い（認識できないを含む）。`artwork_id` を NULL にする
    Absent,
    /// 読んでキャッシュへ置いた。`artwork` 行を upsert して `artwork_id` にする
    Found(TrackPicture),
    /// 読んだがキャッシュへ置けなかった（store の I/O 失敗）。`artwork_id` は触らず、
    /// `artwork_dirty` を立てて次のスキャンに読み直させる（物理属性に依らない再試行。D-61）
    Failed(String),
}

/// [`TrackContent::picture`] を `tracks.artwork_id` / `artwork_dirty` に反映する（`Unread` は何もしない）
fn apply_picture(conn: &Connection, id: i64, picture: &PictureState) -> Result<()> {
    let artwork_id = match picture {
        PictureState::Unread => return Ok(()),
        PictureState::Failed(_) => {
            conn.execute(
                "UPDATE tracks SET artwork_dirty = 1 WHERE id = ?1 AND artwork_dirty = 0",
                [id],
            )?;
            return Ok(());
        }
        PictureState::Absent => None,
        PictureState::Found(p) => Some(super::artwork::upsert(
            conn,
            &p.sha256,
            p.mime,
            Some(p.width),
            Some(p.height),
            p.bytes,
            "embedded",
        )?),
    };
    conn.execute(
        "UPDATE tracks SET artwork_id = ?2, artwork_dirty = 0
          WHERE id = ?1 AND (artwork_id IS NOT ?2 OR artwork_dirty = 1)",
        params![id, artwork_id],
    )?;
    Ok(())
}

/// 音声フィンガープリント（可逆は md5、非可逆は fp）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fingerprint {
    Md5(Option<[u8; 16]>),
    Fp(Option<[u8; 32]>),
}

/// 最速パス: `seen_at` / `seen_run_id` だけを更新する（FTS トリガは走らない）
pub fn touch_seen(conn: &Connection, id: i64, run_id: i64, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE tracks SET seen_at = ?2, seen_run_id = ?3, missing_since = NULL WHERE id = ?1",
        params![id, now, run_id],
    )?;
    Ok(())
}

/// 物理属性と seen を更新する（タグは触らない）
pub fn update_physical(
    conn: &Connection,
    id: i64,
    ph: &Physical,
    run_id: i64,
    now: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE tracks SET dev = ?2, inode = ?3, nlink = ?4, size = ?5, mtime_ns = ?6,
                ctime_ns = ?7, seen_at = ?8, seen_run_id = ?9, missing_since = NULL
         WHERE id = ?1",
        params![
            id,
            ph.dev as i64,
            ph.inode as i64,
            ph.nlink as i64,
            ph.size as i64,
            ph.mtime_ns,
            ph.ctime_ns,
            now,
            run_id
        ],
    )?;
    Ok(())
}

/// タグ・キャッシュ列・音声属性・`tag_hash` を置き換える。版は呼び出し側が決めて渡す
pub fn update_content(
    conn: &Connection,
    id: i64,
    c: &TrackContent,
    tag_version: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE tracks SET codec = ?2, lossless = ?3, sample_rate = ?4, bit_depth = ?5,
                channels = ?6, bitrate = ?7, duration_ms = ?8, tag_hash = ?9, tag_version = ?10,
                title = ?11, artist_display = ?12, albumartist = ?13, track_no = ?14,
                disc_no = ?15, date = ?16
         WHERE id = ?1",
        params![
            id,
            c.codec,
            c.lossless as i64,
            c.sample_rate,
            c.bit_depth,
            c.channels,
            c.bitrate,
            c.duration_ms.map(|d| d as i64),
            c.tag_hash.as_slice(),
            tag_version,
            c.cache.title,
            c.cache.artist_display,
            c.cache.albumartist,
            c.cache.track_no,
            c.cache.disc_no,
            c.cache.date,
        ],
    )?;
    replace_tags(conn, id, &c.tags)?;
    apply_picture(conn, id, &c.picture)
}

/// 音声フィンガープリントと版
pub fn update_fingerprint(
    conn: &Connection,
    id: i64,
    fp: Fingerprint,
    audio_version: i64,
) -> Result<()> {
    match fp {
        Fingerprint::Md5(m) => conn.execute(
            "UPDATE tracks SET audio_md5 = ?2, audio_fp = NULL, audio_version = ?3 WHERE id = ?1",
            params![id, m.map(|m| m.to_vec()), audio_version],
        )?,
        Fingerprint::Fp(f) => conn.execute(
            "UPDATE tracks SET audio_fp = ?2, audio_md5 = NULL, audio_version = ?3 WHERE id = ?1",
            params![id, f.map(|f| f.to_vec()), audio_version],
        )?,
    };
    Ok(())
}

pub fn replace_tags(conn: &Connection, id: i64, tags: &TagSet) -> Result<()> {
    conn.execute("DELETE FROM track_tags WHERE track_id = ?1", [id])?;
    let mut stmt = conn.prepare_cached(
        "INSERT INTO track_tags (track_id, key, idx, value) VALUES (?1, ?2, ?3, ?4)",
    )?;
    let mut last_key: Option<&str> = None;
    let mut idx = 0i64;
    for (key, value) in tags.items() {
        if last_key != Some(key.as_str()) {
            idx = 0;
            last_key = Some(key.as_str());
        }
        stmt.execute(params![id, key, idx, value])?;
        idx += 1;
    }
    Ok(())
}

/// 新規トラック。`album_id` は後で album 照合が設定する
#[allow(clippy::too_many_arguments)]
/// 新規トラックを登録する。`run_id` はスキャンの走査（rip の配置には走査が無いので `None`。
/// 次のスキャンが inode で見つけて claim する）
pub fn insert_track(
    conn: &Connection,
    rel_path: &str,
    rel_path_key: &str,
    ph: &Physical,
    c: &TrackContent,
    fp: Fingerprint,
    run_id: Option<i64>,
    now: i64,
) -> Result<i64> {
    let (md5, afp) = match fp {
        Fingerprint::Md5(m) => (m.map(|m| m.to_vec()), None),
        Fingerprint::Fp(f) => (None, f.map(|f| f.to_vec())),
    };
    conn.execute(
        "INSERT INTO tracks (rel_path, rel_path_key, dev, inode, nlink, size, mtime_ns, ctime_ns,
                             audio_md5, audio_fp, tag_hash, codec, lossless, sample_rate, bit_depth,
                             channels, bitrate, duration_ms, title, artist_display, albumartist,
                             track_no, disc_no, date, seen_at, seen_run_id, added_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
                 ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?25)",
        params![
            rel_path,
            rel_path_key,
            ph.dev as i64,
            ph.inode as i64,
            ph.nlink as i64,
            ph.size as i64,
            ph.mtime_ns,
            ph.ctime_ns,
            md5,
            afp,
            c.tag_hash.as_slice(),
            c.codec,
            c.lossless as i64,
            c.sample_rate,
            c.bit_depth,
            c.channels,
            c.bitrate,
            c.duration_ms.map(|d| d as i64),
            c.cache.title,
            c.cache.artist_display,
            c.cache.albumartist,
            c.cache.track_no,
            c.cache.disc_no,
            c.cache.date,
            now,
            run_id,
        ],
    )?;
    let id = conn.last_insert_rowid();
    replace_tags(conn, id, &c.tags)?;
    apply_picture(conn, id, &c.picture)?;
    Ok(id)
}

/// 予約済み一時 key。NUL は `RelPath` が拒否するので実 key と衝突しない
fn reserved_key(prefix: &str, id: i64) -> String {
    format!("\0{prefix}:{id}")
}

/// パスの 2 段階更新（swap / 循環でも UNIQUE を踏まない）。
/// `moves` は `(id, 新 rel_path, 新 rel_path_key)`
pub fn move_track_paths(conn: &Connection, moves: &[(i64, String, String)]) -> Result<()> {
    for (id, _, _) in moves {
        let tmp = reserved_key("track", *id);
        conn.execute(
            "UPDATE tracks SET rel_path = ?2, rel_path_key = ?2 WHERE id = ?1",
            params![id, tmp],
        )?;
    }
    for (id, path, key) in moves {
        conn.execute(
            "UPDATE tracks SET rel_path = ?2, rel_path_key = ?3 WHERE id = ?1",
            params![id, path, key],
        )?;
    }
    Ok(())
}

/// album の `rel_dir` の 2 段階更新
pub fn move_album_dirs(conn: &Connection, moves: &[(i64, String, String)]) -> Result<()> {
    for (id, _, _) in moves {
        let tmp = reserved_key("album", *id);
        conn.execute(
            "UPDATE albums SET rel_dir = ?2, rel_dir_key = ?2 WHERE id = ?1",
            params![id, tmp],
        )?;
    }
    for (id, dir, key) in moves {
        conn.execute(
            "UPDATE albums SET rel_dir = ?2, rel_dir_key = ?3 WHERE id = ?1",
            params![id, dir, key],
        )?;
    }
    Ok(())
}

/// album のメタデータ（構成トラックの最頻値）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlbumMeta {
    pub category_id: Option<i64>,
    pub albumartist: Option<String>,
    pub album: Option<String>,
    pub date: Option<String>,
    pub original_date: Option<String>,
    pub mb_release_id: Option<String>,
    pub discid: Option<String>,
    pub disc_count: Option<i64>,
}

pub fn insert_album(
    conn: &Connection,
    rel_dir: &str,
    rel_dir_key: &str,
    m: &AlbumMeta,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO albums (rel_dir, rel_dir_key, category_id, albumartist, album, date,
                             original_date, mb_release_id, discid, disc_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            rel_dir,
            rel_dir_key,
            m.category_id,
            m.albumartist,
            m.album,
            m.date,
            m.original_date,
            m.mb_release_id,
            m.discid,
            m.disc_count
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// album のメタデータを更新し、missing なら復活させる。`album` の変更は albums_au トリガで
/// `tracks.album` に伝わる
pub fn update_album_meta(conn: &Connection, id: i64, m: &AlbumMeta) -> Result<()> {
    conn.execute(
        "UPDATE albums SET category_id = ?2, albumartist = ?3, album = ?4, date = ?5,
                original_date = ?6, mb_release_id = ?7, discid = ?8, disc_count = ?9,
                missing_since = NULL
         WHERE id = ?1",
        params![
            id,
            m.category_id,
            m.albumartist,
            m.album,
            m.date,
            m.original_date,
            m.mb_release_id,
            m.discid,
            m.disc_count
        ],
    )?;
    Ok(())
}

/// トラックの album 所属とキャッシュ列 `album` を設定する
pub fn set_track_album(
    conn: &Connection,
    track_id: i64,
    album_id: i64,
    album: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE tracks SET album_id = ?2, album = ?3 WHERE id = ?1
           AND (album_id IS NOT ?2 OR album IS NOT ?3)",
        params![track_id, album_id, album],
    )?;
    Ok(())
}

/// 現在の pending op（track_id → op）。commit トランザクションの中で読み直す用
pub fn load_pending_ops(conn: &Connection) -> Result<std::collections::HashMap<i64, PendingOp>> {
    let mut stmt = conn.prepare(
        "SELECT o.track_id, o.id, o.kind, o.expected_rel_path,
                (SELECT json_extract(e.new_value, '$') FROM edits e
                  WHERE e.op_id = o.id AND e.key = 'rel_path')
         FROM edit_ops o WHERE o.result = 'pending'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            PendingOp {
                op_id: r.get(1)?,
                kind: r.get(2)?,
                expected_rel_path: r.get(3)?,
                target_rel_path: r.get(4)?,
            },
        ))
    })?;
    rows.map(|r| r.map_err(Into::into)).collect()
}

/// 行の現在値（commit トランザクションの中で読み直す用）。物理属性と rel_path が Phase 2 の
/// スナップショットから変わっていれば、別のジョブ（tagwrite / rename）に追い越されている
#[derive(Debug, Clone)]
pub struct CurrentRow {
    pub rel_path: String,
    pub physical: Physical,
    pub tag_hash: Option<[u8; 32]>,
    pub tag_version: i64,
    pub audio_md5: Option<[u8; 16]>,
    pub audio_fp: Option<[u8; 32]>,
    pub audio_version: i64,
}

impl CurrentRow {
    /// スナップショット `snap` を取った後に、この行の**ファイル側の状態**（rel_path / 物理属性）が
    /// 別の書き手（tagwrite の tmp + rename、rename ジョブ）に更新されたか。編集バッチの
    /// DB 先行更新（版・タグ、および pending の rename op が overlay で書く rel_path）は
    /// pending op として別に扱うのでここでは見ない
    pub fn overtaken(&self, snap: &TrackSnap, pending: Option<&PendingOp>) -> bool {
        let rename_pending = pending.is_some_and(|op| op.kind == "rename");
        (self.rel_path != snap.rel_path && !rename_pending)
            || Some(self.physical.dev) != snap.dev
            || Some(self.physical.inode) != snap.inode
            || self.physical.size != snap.size
            || self.physical.mtime_ns != snap.mtime_ns
            || self.physical.ctime_ns != snap.ctime_ns
    }
}

pub fn load_current_row(conn: &Connection, id: i64) -> Result<CurrentRow> {
    Ok(conn.query_row(
        "SELECT rel_path, dev, inode, nlink, size, mtime_ns, ctime_ns,
                tag_hash, tag_version, audio_md5, audio_fp, audio_version
         FROM tracks WHERE id = ?1",
        [id],
        |r| {
            Ok(CurrentRow {
                rel_path: r.get(0)?,
                physical: Physical {
                    dev: r.get::<_, Option<i64>>(1)?.unwrap_or(0) as u64,
                    inode: r.get::<_, Option<i64>>(2)?.unwrap_or(0) as u64,
                    nlink: r.get::<_, i64>(3)? as u64,
                    size: r.get::<_, i64>(4)? as u64,
                    mtime_ns: r.get(5)?,
                    ctime_ns: r.get(6)?,
                },
                tag_hash: blob_array(r.get(7)?),
                tag_version: r.get(8)?,
                audio_md5: blob_array(r.get(9)?),
                audio_fp: blob_array(r.get(10)?),
                audio_version: r.get(11)?,
            })
        },
    )?)
}

/// 宛先 key を占有していた未 claim 行に key を明け渡させる（行は残り、finalize で missing になる）
pub fn vacate_track_paths(conn: &Connection, ids: &[i64]) -> Result<()> {
    for id in ids {
        let vacated = reserved_key("vacated", *id);
        conn.execute(
            "UPDATE tracks SET rel_path = ?2, rel_path_key = ?2 WHERE id = ?1",
            params![id, vacated],
        )?;
    }
    Ok(())
}

/// album の所属を外す（root 直下へ移動した行）
pub fn clear_track_album(conn: &Connection, track_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE tracks SET album_id = NULL, album = NULL WHERE id = ?1 AND album_id IS NOT NULL",
        [track_id],
    )?;
    Ok(())
}

/// pending の rename op を外部 rename との衝突で `skipped_conflict` にする
pub fn conflict_pending_op(conn: &Connection, op_id: i64, error: &str, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE edit_ops SET result = 'skipped_conflict', error = ?2, applied_at = ?3
         WHERE id = ?1 AND result = 'pending'",
        params![op_id, error, now],
    )?;
    Ok(())
}

/// finalize（`completed` のときだけ）: この run で claim されなかった active 行に missing を立て、
/// 構成 0 の album に missing を立てる。戻り値は missing にしたトラック数
/// `track_ids` の行が現在属している album（重複なし）
pub fn album_ids_of_tracks(conn: &Connection, track_ids: &[i64]) -> Result<Vec<i64>> {
    let json =
        serde_json::to_string(track_ids).map_err(|e| super::DbError::Internal(e.to_string()))?;
    let mut st = conn.prepare_cached(
        "SELECT DISTINCT album_id FROM tracks
          WHERE album_id IS NOT NULL AND id IN (SELECT value FROM json_each(?1))",
    )?;
    let ids = st
        .query_map([&json], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
}

pub fn finalize_missing(conn: &Connection, run_id: i64, now: i64) -> Result<Vec<i64>> {
    // 立てた行の id を返す（SSE `library` イベントの変更行に含める）
    let mut stmt = conn.prepare_cached(
        "UPDATE tracks SET missing_since = ?2
         WHERE missing_since IS NULL AND (seen_run_id IS NULL OR seen_run_id <> ?1)
         RETURNING id",
    )?;
    let tracks = stmt
        .query_map(params![run_id, now], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    conn.execute(
        "UPDATE albums SET missing_since = ?1
         WHERE missing_since IS NULL
           AND NOT EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = albums.id AND t.missing_since IS NULL)",
        [now],
    )?;
    Ok(tracks)
}

/// トラックの `artwork_id`（行が無ければ None）
pub fn track_artwork_id(conn: &Connection, track_id: i64) -> Result<Option<i64>> {
    let v: Option<Option<i64>> = conn
        .query_row(
            "SELECT artwork_id FROM tracks WHERE id = ?1",
            [track_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.flatten())
}

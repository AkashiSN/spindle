//! Inbox 取り込み（SPEC §7.8、D-20 / D-68、P2-10）。
//!
//! ```text
//! scan_inbox   Inbox を歩き、音声ファイルのあるディレクトリを 1 件として inbox_items / inbox_files に
//!              写す。stat が変わったファイルだけタグを読み直す。消えたディレクトリの行は消す。
//!              approved の件でファイルが変わっていたら pending に戻す（再承認）
//! proposal     タグから下書き（InboxDraft）を作る。承認画面の初期値
//! InboxDraft   承認の入力。validate で不足（album / albumartist / title、番号）を弾く
//! place_item   approved の件を Library に配置して登録する（配置は cd::place と同じ規則）
//! ```
//!
//! 正は Inbox のファイルで、行はキャッシュ。承認の補正はファイルのタグに書いてから置く
//! （ファイルが正のまま再スキャンしても DB と一致する）

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::db::inbox::{self as dbinbox, FileRow, Item, ItemState};
use crate::db::{Db, DbError};
use crate::domain::relpath::{canonical_key, RelPath};
use crate::domain::tags::{read_audio_file, Codec};
use crate::fsroot::{FileKind, FsError, RootDir};

// ---------------------------------------------------------------- 下書き

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftTrack {
    /// Inbox 相対（件のファイルと 1:1）
    pub rel_path: String,
    pub disc_no: u32,
    pub track_no: u32,
    pub title: String,
    /// 空ならアルバムアーティスト
    #[serde(default)]
    pub artist: String,
}

/// 承認の入力（アルバム単位の補正とトラック単位の補正）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxDraft {
    /// 配置先の category（統制語彙の名前）。None なら `_Unsorted`
    #[serde(default)]
    pub category: Option<String>,
    pub albumartist: String,
    pub album: String,
    /// `YYYY[-MM[-DD]]`
    #[serde(default)]
    pub date: Option<String>,
    pub tracks: Vec<DraftTrack>,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum DraftError {
    #[error("アルバム名が空")]
    EmptyAlbum,
    #[error("アルバムアーティストが空")]
    EmptyAlbumArtist,
    #[error("タイトルが空: {rel_path}")]
    EmptyTitle { rel_path: String },
    #[error("トラック番号 / ディスク番号は 1 以上: {rel_path}")]
    BadNumber { rel_path: String },
    #[error("番号が重複: disc {disc_no} track {track_no}")]
    DuplicateNumber { disc_no: u32, track_no: u32 },
    #[error("件に無いファイル: {0}")]
    UnknownFile(String),
    #[error("下書きに無いファイル: {0}")]
    MissingFile(String),
    #[error("下書きに同じファイルが 2 回: {0}")]
    DuplicateFile(String),
    #[error("日付の形が不正: {0}（YYYY / YYYY-MM / YYYY-MM-DD）")]
    BadDate(String),
    #[error("category が空")]
    EmptyCategory,
}

/// `YYYY[-MM[-DD]]` か
fn is_valid_date(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.is_empty() || parts.len() > 3 {
        return false;
    }
    let digits = |p: &str, n: usize| p.len() == n && p.bytes().all(|b| b.is_ascii_digit());
    if !digits(parts[0], 4) {
        return false;
    }
    if let Some(m) = parts.get(1) {
        if !digits(m, 2) || !(1..=12).contains(&m.parse::<u32>().unwrap_or(0)) {
            return false;
        }
    }
    if let Some(d) = parts.get(2) {
        if !digits(d, 2) || !(1..=31).contains(&d.parse::<u32>().unwrap_or(0)) {
            return false;
        }
    }
    true
}

impl InboxDraft {
    /// 全部の問題（承認画面の警告に使う）。`files` は件のファイルの rel_path
    pub fn problems(&self, files: &[String]) -> Vec<DraftError> {
        let mut out = Vec::new();
        if self.album.trim().is_empty() {
            out.push(DraftError::EmptyAlbum);
        }
        if self.albumartist.trim().is_empty() {
            out.push(DraftError::EmptyAlbumArtist);
        }
        if let Some(c) = &self.category {
            if c.trim().is_empty() {
                out.push(DraftError::EmptyCategory);
            }
        }
        if let Some(d) = &self.date {
            if !is_valid_date(d.trim()) {
                out.push(DraftError::BadDate(d.clone()));
            }
        }
        let known: HashSet<String> = files.iter().map(|f| canonical_key(f)).collect();
        let mut seen_files: HashSet<String> = HashSet::new();
        let mut numbers: HashSet<(u32, u32)> = HashSet::new();
        for t in &self.tracks {
            let key = canonical_key(&t.rel_path);
            if !known.contains(&key) {
                out.push(DraftError::UnknownFile(t.rel_path.clone()));
            }
            if !seen_files.insert(key) {
                // 同じ音声を複数の行として配置させない
                out.push(DraftError::DuplicateFile(t.rel_path.clone()));
            }
            if t.title.trim().is_empty() {
                out.push(DraftError::EmptyTitle {
                    rel_path: t.rel_path.clone(),
                });
            }
            if t.disc_no == 0 || t.track_no == 0 {
                out.push(DraftError::BadNumber {
                    rel_path: t.rel_path.clone(),
                });
            } else if !numbers.insert((t.disc_no, t.track_no)) {
                out.push(DraftError::DuplicateNumber {
                    disc_no: t.disc_no,
                    track_no: t.track_no,
                });
            }
        }
        for f in files {
            if !seen_files.contains(&canonical_key(f)) {
                out.push(DraftError::MissingFile(f.clone()));
            }
        }
        out
    }

    /// 最初の問題（API の 400）
    pub fn validate(&self, files: &[String]) -> Result<(), DraftError> {
        match self.problems(files).into_iter().next() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// 枚数（disc_no の最大）
    pub fn disc_count(&self) -> u32 {
        self.tracks
            .iter()
            .map(|t| t.disc_no)
            .max()
            .unwrap_or(1)
            .max(1)
    }

    /// トラック `index` のアーティスト（空ならアルバムアーティスト）
    pub fn track_artist(&self, index: usize) -> &str {
        match self.tracks.get(index).map(|t| t.artist.trim()) {
            Some(a) if !a.is_empty() => a,
            _ => self.albumartist.trim(),
        }
    }

    /// 発売年
    pub fn year(&self) -> Option<String> {
        let d = self.date.as_deref()?.trim();
        let y: String = d.chars().take(4).collect();
        (y.len() == 4 && y.chars().all(|c| c.is_ascii_digit())).then_some(y)
    }
}

/// 値の最頻値（同数なら先に出たもの。空は数えない）
fn mode<'a>(values: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut order: Vec<(String, usize)> = Vec::new();
    for v in values {
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        match order.iter_mut().find(|(k, _)| k == v) {
            Some((_, n)) => *n += 1,
            None => order.push((v.to_owned(), 1)),
        }
    }
    let best = order.iter().map(|(_, n)| *n).max()?;
    order.into_iter().find(|(_, n)| *n == best).map(|(k, _)| k)
}

fn tag<'a>(f: &'a FileRow, key: &str) -> Option<&'a str> {
    f.tags
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn leading_int(s: &str) -> Option<u32> {
    let digits: String = s
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// タグから下書きを作る。albumartist / album / date は最頻値（ALBUMARTIST が無ければ ARTIST）、
/// category は GENRE の最頻値のうち `genre_map`（canonical key → category id）に当たるもの、
/// トラックは TRACKNUMBER（無ければ 0 = 不足）/ DISCNUMBER（無ければ 1）/ TITLE / ARTIST
pub fn proposal(
    files: &[FileRow],
    categories: &[(i64, String)],
    genre_map: &[(String, i64)],
) -> InboxDraft {
    let albumartist = mode(files.iter().filter_map(|f| tag(f, "ALBUMARTIST")))
        .or_else(|| mode(files.iter().filter_map(|f| tag(f, "ARTIST"))))
        .unwrap_or_default();
    let album = mode(files.iter().filter_map(|f| tag(f, "ALBUM"))).unwrap_or_default();
    let date = mode(files.iter().filter_map(|f| tag(f, "DATE")));
    let genre = mode(
        files
            .iter()
            .flat_map(|f| f.tags.iter().filter(|(k, _)| k == "GENRE"))
            .map(|(_, v)| v.as_str())
            .filter(|g| {
                let key = canonical_key(g);
                genre_map.iter().any(|(gk, _)| *gk == key)
            }),
    );
    let category = genre.and_then(|g| {
        let key = canonical_key(&g);
        let id = genre_map.iter().find(|(gk, _)| *gk == key)?.1;
        categories
            .iter()
            .find(|(cid, _)| *cid == id)
            .map(|(_, name)| name.clone())
    });
    let tracks = files
        .iter()
        .map(|f| DraftTrack {
            rel_path: f.rel_path.clone(),
            disc_no: tag(f, "DISCNUMBER").and_then(leading_int).unwrap_or(1),
            track_no: tag(f, "TRACKNUMBER").and_then(leading_int).unwrap_or(0),
            title: tag(f, "TITLE").unwrap_or("").trim().to_owned(),
            artist: tag(f, "ARTIST").unwrap_or("").trim().to_owned(),
        })
        .collect();
    InboxDraft {
        category,
        albumartist,
        album,
        date,
        tracks,
    }
}

/// 下書きの問題を人間向けの文字列で
pub fn warnings(files: &[FileRow], draft: &InboxDraft) -> Vec<String> {
    let names: Vec<String> = files.iter().map(|f| f.rel_path.clone()).collect();
    draft
        .problems(&names)
        .into_iter()
        .map(|e| e.to_string())
        .collect()
}

// ---------------------------------------------------------------- 走査

#[derive(Debug, thiserror::Error)]
pub enum InboxError {
    #[error("下書きが不正: {0}")]
    Draft(#[from] DraftError),
    #[error("配置先が衝突: {0}")]
    Conflict(String),
    #[error("Inbox 側が変わった: {0}")]
    Changed(String),
    #[error(transparent)]
    Placement(#[from] crate::import::placement::PlacementError),
    #[error("タグを書けない: {0}")]
    Tag(#[from] crate::domain::tags::TagWriteError),
    #[error(transparent)]
    Fs(#[from] FsError),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("キャンセルされた")]
    Cancelled,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanOutcome {
    pub items_seen: usize,
    pub items_new: usize,
    pub items_removed: usize,
    pub files_read: usize,
}

/// `placed` の行を残す時間
pub const PLACED_RETENTION_SECS: i64 = 86_400;

/// 走査で見た 1 ファイル
struct Seen {
    rel: RelPath,
    st: crate::fsroot::Stat,
}

/// Inbox を歩いて音声ファイルを dir_key ごとにまとめる
fn walk(root: &RootDir) -> Result<HashMap<String, (String, Vec<Seen>)>, FsError> {
    let mut groups: HashMap<String, (String, Vec<Seen>)> = HashMap::new();
    let mut stack: Vec<Option<RelPath>> = vec![None];
    while let Some(dir) = stack.pop() {
        let entries = match root.read_dir(dir.as_ref()) {
            Ok(e) => e,
            Err(FsError::NotFound) => continue,
            Err(e) => return Err(e),
        };
        for e in entries {
            let Some(name) = e.name.to_str() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let child = match &dir {
                Some(d) => d.join(name),
                None => RelPath::parse(name),
            };
            let Ok(child) = child else {
                continue;
            };
            match e.kind {
                FileKind::Dir => stack.push(Some(child)),
                FileKind::File => {
                    let is_audio = name
                        .rsplit_once('.')
                        .and_then(|(_, ext)| Codec::from_extension(ext))
                        .is_some();
                    if !is_audio {
                        continue;
                    }
                    let st = match root.stat(&child) {
                        Ok(st) if st.kind == FileKind::File => st,
                        Ok(_) | Err(FsError::NotFound) => continue,
                        Err(e) => return Err(e),
                    };
                    let (dir_str, dir_key) = match &dir {
                        Some(d) => (d.as_str().to_owned(), d.key()),
                        None => (String::new(), String::new()),
                    };
                    groups
                        .entry(dir_key)
                        .or_insert_with(|| (dir_str, Vec::new()))
                        .1
                        .push(Seen { rel: child, st });
                }
                FileKind::Symlink | FileKind::Other => {}
            }
        }
    }
    Ok(groups)
}

fn stat_matches(row: &FileRow, st: &crate::fsroot::Stat) -> bool {
    row.inode == st.inode as i64
        && row.size == st.size as i64
        && row.mtime_ns == st.mtime_ns
        && row.ctime_ns == st.ctime_ns
}

fn read_row(root: &RootDir, seen: &Seen) -> Result<FileRow, FsError> {
    let ext = seen.rel.file_name().rsplit_once('.').map(|(_, e)| e);
    let file = root.open_file(&seen.rel)?;
    let af = read_audio_file(file, ext)
        .map_err(|e| FsError::Io(std::io::Error::other(e.to_string())))?;
    Ok(FileRow {
        rel_path: seen.rel.as_str().to_owned(),
        inode: seen.st.inode as i64,
        size: seen.st.size as i64,
        mtime_ns: seen.st.mtime_ns,
        ctime_ns: seen.st.ctime_ns,
        codec: af.codec.as_str().to_owned(),
        lossless: af.lossless,
        sample_rate: af.sample_rate,
        bit_depth: af.bit_depth,
        channels: af.channels,
        duration_ms: af.duration_ms,
        tags: af.tags.items().to_vec(),
    })
}

/// 1 件の走査結果
struct Synced {
    dir: String,
    dir_key: String,
    /// 変わったときだけ Some（入れ替える全ファイル）
    files: Option<Vec<FileRow>>,
}

/// Inbox を走査して `inbox_items` / `inbox_files` を同期する
pub async fn scan_inbox(
    db: &Db,
    inbox: &Arc<RootDir>,
    now: i64,
) -> Result<ScanOutcome, InboxError> {
    // 1. 既存の行
    let existing: Vec<(Item, Vec<FileRow>)> = db
        .read(|c| {
            let items = dbinbox::list(c)?;
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                let files = dbinbox::files(c, it.id)?;
                out.push((it, files));
            }
            Ok(out)
        })
        .await?;
    let by_key: HashMap<String, (Item, HashMap<String, FileRow>)> = existing
        .into_iter()
        .map(|(it, files)| {
            let key = canonical_key(&it.rel_dir);
            let files = files
                .into_iter()
                .map(|f| (canonical_key(&f.rel_path), f))
                .collect();
            (key, (it, files))
        })
        .collect();
    // 2. 歩いて、変わったファイルだけ読む（blocking）
    let (synced, files_read) = {
        let root = Arc::clone(inbox);
        let by_key = Arc::new(by_key);
        let by_key2 = Arc::clone(&by_key);
        let res = tokio::task::spawn_blocking(move || -> Result<(Vec<Synced>, usize), InboxError> {
            let groups = walk(&root)?;
            let mut out = Vec::with_capacity(groups.len());
            let mut files_read = 0;
            let mut keys: Vec<&String> = groups.keys().collect();
            keys.sort();
            for key in keys {
                let (dir, seen) = &groups[key];
                let prev = by_key2.get(key).map(|(_, f)| f);
                let mut changed = prev.is_none_or(|p| p.len() != seen.len());
                let mut rows = Vec::with_capacity(seen.len());
                for s in seen {
                    let rk = s.rel.key();
                    match prev.and_then(|p| p.get(&rk)) {
                        Some(row) if stat_matches(row, &s.st) => rows.push(row.clone()),
                        _ => {
                            changed = true;
                            match read_row(&root, s) {
                                Ok(row) => {
                                    files_read += 1;
                                    rows.push(row);
                                }
                                Err(e) => {
                                    tracing::warn!(path = %s.rel, error = %e, "Inbox のファイルを読めない。今回は飛ばす");
                                }
                            }
                        }
                    }
                }
                if rows.is_empty() {
                    continue;
                }
                rows.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
                out.push(Synced {
                    dir: dir.clone(),
                    dir_key: key.clone(),
                    files: changed.then_some(rows),
                });
            }
            Ok((out, files_read))
        })
        .await
        .map_err(|e| std::io::Error::other(format!("Inbox の走査タスクが異常終了: {e}")))??;
        drop(by_key);
        res
    };
    // 3. DB に写す（1 トランザクション）
    let outcome = db
        .transaction(move |c| {
            let mut o = ScanOutcome {
                files_read,
                ..ScanOutcome::default()
            };
            for s in &synced {
                o.items_seen += 1;
                let item = match dbinbox::find_by_dir_key(c, &s.dir_key)? {
                    Some(it) => {
                        dbinbox::touch(c, it.id, now)?;
                        it
                    }
                    None => {
                        o.items_new += 1;
                        let id = dbinbox::insert_item(c, &s.dir, &s.dir_key, now)?;
                        dbinbox::get(c, id)?.ok_or_else(|| {
                            DbError::Internal(format!("挿入した inbox_items {id} が無い"))
                        })?
                    }
                };
                if item.state == ItemState::Placing {
                    // 配置中の件は触らない（同じジョブの中でしか placing にならない）
                    continue;
                }
                if let Some(files) = &s.files {
                    dbinbox::replace_files(c, item.id, files)?;
                    if item.state == ItemState::Approved {
                        dbinbox::set_state(
                            c,
                            item.id,
                            ItemState::Pending,
                            Some("ファイルが変わったので再承認が必要"),
                            now,
                        )?;
                    }
                }
                if item.state == ItemState::Placed {
                    // 配置済みの件のディレクトリに音声がある = 消せなかった原本か、配置の後に
                    // 置かれた（または差し替えられた）ファイル。placed の裏に隠さず件として出し直す
                    dbinbox::set_state(
                        c,
                        item.id,
                        ItemState::Pending,
                        Some("配置の後も Inbox に音声が残っている（再承認で配置し直す）"),
                        now,
                    )?;
                }
            }
            for it in dbinbox::stale_items(c, now)? {
                if it.state == ItemState::Placing {
                    continue;
                }
                dbinbox::delete_item(c, it.id)?;
                o.items_removed += 1;
            }
            o.items_removed += dbinbox::expire_placed(c, now - PLACED_RETENTION_SECS)?;
            Ok(o)
        })
        .await?;
    Ok(outcome)
}

// ---------------------------------------------------------------- 配置

use std::fs::File;
use std::io::Read as _;

use rusqlite::OptionalExtension as _;
use tokio_util::sync::CancellationToken;

use crate::config::LayoutConfig;
use crate::db::now_epoch;
use crate::db::scans::{self, AlbumMeta, Fingerprint, PictureState};
use crate::domain::pathgen::{self, PlanItem, Planned, Template, TrackFields};
use crate::domain::tags::{write_tag_changes, TagChange};
use crate::edit::Editor;
use crate::import::placement::{
    find_or_create_album, place_one, register_track, remove_placed, PlacedFile, PlacementError,
};
use crate::import::scanner::{read_fingerprint, track_content};
use crate::jobs::handlers::rg::new_album_job;
use crate::jobs::{Event, Jobs, LibraryEvent};

/// 配置に要する環境
pub struct PlaceItemEnv {
    pub db: Arc<Db>,
    pub library: Arc<RootDir>,
    pub inbox: Arc<RootDir>,
    pub jobs: Arc<Jobs>,
    pub layout: LayoutConfig,
    /// 無ければ normalize は投入しない
    pub editor: Option<Arc<Editor>>,
    pub wav_to_flac: bool,
    /// テスト用: Inbox 側を読んだ後・配置の前に呼ぶ（その間に原本が差し替えられた状況を作る）
    #[doc(hidden)]
    pub before_place: Option<crate::cd::place::PlaceHook>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemPlaced {
    pub album_id: i64,
    pub rel_dir: RelPath,
    /// draft の順
    pub track_ids: Vec<i64>,
    pub job_ids: Vec<i64>,
    /// 投入した normalize の編集バッチ
    pub normalize_batch: Option<i64>,
}

/// 計画（draft の順）
#[derive(Debug, Clone)]
struct ItemPlan {
    paths: Vec<RelPath>,
    rel_dir: RelPath,
    release: String,
    category_id: Option<i64>,
}

/// 配置前に Inbox 側で読んだ 1 ファイル（draft の順）
#[derive(Debug, Clone)]
struct Source {
    rel: RelPath,
    ext: Option<String>,
    fp: Fingerprint,
    changes: Vec<TagChange>,
}

fn parse_template(name: &str, s: &str) -> Result<Template, InboxError> {
    Template::parse(s).map_err(|e| InboxError::Conflict(format!("[layout].{name} が不正: {e}")))
}

/// 補正で変わるタグ（現在値と違うキーだけ）
fn tag_changes(draft: &InboxDraft, index: usize, current: &[(String, String)]) -> Vec<TagChange> {
    let t = &draft.tracks[index];
    let mut desired: Vec<(&str, String)> = vec![
        ("ALBUMARTIST", draft.albumartist.trim().to_owned()),
        ("ALBUM", draft.album.trim().to_owned()),
        ("TITLE", t.title.trim().to_owned()),
        ("ARTIST", draft.track_artist(index).to_owned()),
        ("TRACKNUMBER", t.track_no.to_string()),
        ("DISCNUMBER", t.disc_no.to_string()),
    ];
    if let Some(d) = draft
        .date
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        desired.push(("DATE", d.to_owned()));
    }
    if draft.disc_count() > 1 {
        desired.push(("DISCTOTAL", draft.disc_count().to_string()));
    }
    desired
        .into_iter()
        .filter(|(k, v)| {
            let now: Vec<&str> = current
                .iter()
                .filter(|(ck, _)| ck == k)
                .map(|(_, cv)| cv.as_str())
                .collect();
            now != [v.as_str()]
        })
        .map(|(k, v)| TagChange {
            key: k.to_owned(),
            values: Some(vec![v]),
        })
        .collect()
}

/// Inbox 側を読む（配置の前）: 行と stat が一致することを確かめ、音声のフィンガープリントと
/// 補正で変わるタグを出す
fn read_sources(
    inbox: &RootDir,
    draft: &InboxDraft,
    files: &HashMap<String, FileRow>,
) -> Result<Vec<Source>, InboxError> {
    let mut out = Vec::with_capacity(draft.tracks.len());
    for (i, t) in draft.tracks.iter().enumerate() {
        let row = files
            .get(&canonical_key(&t.rel_path))
            .ok_or_else(|| InboxError::Draft(DraftError::UnknownFile(t.rel_path.clone())))?;
        let rel = RelPath::parse(&t.rel_path)
            .map_err(|e| InboxError::Conflict(format!("{}: {e}", t.rel_path)))?;
        let st = inbox.stat(&rel)?;
        if !stat_matches(row, &st) {
            return Err(InboxError::Changed(t.rel_path.clone()));
        }
        let ext = rel
            .file_name()
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase());
        let af = read_audio_file(inbox.open_file(&rel)?, ext.as_deref())
            .map_err(|e| InboxError::Conflict(format!("{}: {e}", t.rel_path)))?;
        let fp = read_fingerprint(inbox, &rel, &af);
        out.push(Source {
            rel,
            ext,
            fp,
            changes: tag_changes(draft, i, &row.tags),
        });
    }
    Ok(out)
}

/// 自分の成果物（同じ音声の active な行。再実行で見分ける）
#[derive(Debug, Default)]
struct SelfRows {
    /// 行が属する album とそのリリースキー（最初に見つかったもの）
    album: Option<(i64, String)>,
    /// 行の rel_path_key
    keys: HashSet<String>,
}

/// 同じ音声（フィンガープリント一致）の active な行と、その album
fn self_album(conn: &rusqlite::Connection, sources: &[Source]) -> Result<SelfRows, InboxError> {
    let mut keys = HashSet::new();
    let mut album: Option<(i64, String)> = None;
    let mut st = conn.prepare_cached(
        "SELECT t.rel_path_key, t.album_id, a.mb_release_id, a.discid
           FROM tracks t LEFT JOIN albums a ON a.id = t.album_id
          WHERE t.missing_since IS NULL AND ((?1 IS NOT NULL AND t.audio_md5 = ?1)
                                           OR (?2 IS NOT NULL AND t.audio_fp = ?2))",
    )?;
    for src in sources {
        let (md5, afp): (Option<Vec<u8>>, Option<Vec<u8>>) = match src.fp {
            Fingerprint::Md5(Some(m)) => (Some(m.to_vec()), None),
            Fingerprint::Fp(Some(f)) => (None, Some(f.to_vec())),
            _ => continue,
        };
        let rows = st
            .query_map(rusqlite::params![md5, afp], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (key, album_id, mb, discid) in rows {
            keys.insert(key);
            if album.is_none() {
                if let Some(id) = album_id {
                    album = Some((
                        id,
                        crate::import::placement::release_key(id, mb.as_deref(), discid.as_deref()),
                    ));
                }
            }
        }
    }
    Ok(SelfRows { album, keys })
}

fn plan_item(
    conn: &rusqlite::Connection,
    layout: &LayoutConfig,
    item: &Item,
    draft: &InboxDraft,
    files: &[FileRow],
    sources: &[Source],
) -> Result<ItemPlan, InboxError> {
    let category = match draft.category.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => crate::db::categories::find_by_key(conn, name)?,
        _ => None,
    };
    let template = if category.is_none() {
        parse_template("unsorted", &layout.unsorted)?
    } else if draft.disc_count() > 1 {
        parse_template("multi_disc", &layout.multi_disc)?
    } else {
        parse_template("single_disc", &layout.single_disc)?
    };
    // 自分の成果物（同じ音声の行）は占有から外し、その album のリリースキーに揃える（再実行）
    let SelfRows {
        album: self_album,
        keys: self_keys,
    } = self_album(conn, sources)?;
    // リリースキー: MUSICBRAINZ_ALBUMID の最頻値があれば mb:、自分の成果物の album があればそれ、
    // 無ければ件ごとの新規
    let release = mode(files.iter().filter_map(|f| tag(f, "MUSICBRAINZ_ALBUMID")))
        .map(|m| format!("mb:{m}"))
        .or_else(|| self_album.map(|(_, k)| k))
        .unwrap_or_else(|| format!("inbox:{}", item.id));
    let items: Vec<PlanItem> = draft
        .tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let (stem, ext) = match t
                .rel_path
                .rsplit('/')
                .next()
                .unwrap_or(&t.rel_path)
                .rsplit_once('.')
            {
                Some((s, e)) => (s.to_owned(), e.to_ascii_lowercase()),
                None => (t.rel_path.clone(), String::new()),
            };
            PlanItem {
                track_id: -(i as i64) - 1,
                template: template.clone(),
                fields: TrackFields {
                    category: category.as_ref().map(|c| c.name.clone()),
                    albumartist: Some(draft.albumartist.trim().to_owned()),
                    artist: Some(draft.track_artist(i).to_owned()),
                    album: Some(draft.album.trim().to_owned()),
                    title: Some(t.title.trim().to_owned()),
                    disc_no: Some(i64::from(t.disc_no)),
                    track_no: Some(i64::from(t.track_no)),
                    year: draft.year(),
                    edition: None,
                    ext,
                    stem,
                },
                release: release.clone(),
                current_rel_path: String::new(),
            }
        })
        .collect();
    let mut occ = crate::edit::rename::load_occupancy(conn, &HashSet::new())?;
    for k in &self_keys {
        occ.path_keys.remove(k);
    }
    let mut paths = Vec::with_capacity(items.len());
    for (planned, t) in pathgen::plan(&items, &occ).into_iter().zip(&draft.tracks) {
        match planned {
            Planned::Path(p) => paths.push(p),
            Planned::Conflict(r) => {
                return Err(InboxError::Conflict(format!("{}: {r}", t.rel_path)))
            }
            Planned::Unchanged => {
                return Err(InboxError::Conflict(format!(
                    "{}: パスを決められない",
                    t.rel_path
                )))
            }
        }
    }
    let Some(rel_dir) = paths.first().and_then(RelPath::parent) else {
        return Err(InboxError::Conflict("宛先が root 直下になる".into()));
    };
    if paths.iter().any(|p| p.parent().as_ref() != Some(&rel_dir)) {
        return Err(InboxError::Conflict(
            "トラックの宛先が 1 つのディレクトリに揃わない".into(),
        ));
    }
    Ok(ItemPlan {
        paths,
        rel_dir,
        release,
        category_id: category.map(|c| c.id),
    })
}

/// 置いた結果（登録の材料）
struct Placed {
    tracks: Vec<(scans::Physical, scans::TrackContent, Fingerprint)>,
    placed_new: Vec<RelPath>,
    /// 作ったディレクトリのうち最上位（無ければ全部既存）。失敗時にここまで消す
    created_top: Option<RelPath>,
    /// 移した同梱ファイル（Inbox 側の rel）
    companions: Vec<RelPath>,
}

fn same_audio(a: Fingerprint, b: Fingerprint) -> bool {
    match (a, b) {
        (Fingerprint::Md5(Some(x)), Fingerprint::Md5(Some(y))) => x == y,
        (Fingerprint::Fp(Some(x)), Fingerprint::Fp(Some(y))) => x == y,
        _ => false,
    }
}

/// `dir` までを作り、新しく作った最上位のディレクトリを返す（失敗時の後始末に使う）
fn create_dirs(root: &RootDir, dir: &RelPath) -> Result<Option<RelPath>, FsError> {
    let mut top: Option<RelPath> = None;
    let mut prefix: Option<RelPath> = None;
    for name in dir.components() {
        let next = match &prefix {
            Some(p) => p.join(name),
            None => RelPath::parse(name),
        }
        .map_err(|e| FsError::Io(std::io::Error::other(e)))?;
        if top.is_none() && matches!(root.stat(&next), Err(FsError::NotFound)) {
            top = Some(next.clone());
        }
        prefix = Some(next);
    }
    root.create_dir_all(dir)?;
    Ok(top)
}

/// 失敗時の後始末: 置いたファイルを消し、作ったディレクトリを `created_top` まで消す
fn cleanup(root: &RootDir, dir: &RelPath, placed_new: &[RelPath], created_top: Option<&RelPath>) {
    remove_placed(root, dir, placed_new);
    let Some(top) = created_top else {
        return;
    };
    let mut cur = dir.parent();
    while let Some(d) = cur {
        if d.key().len() < top.key().len() {
            break;
        }
        let _ = root.remove_dir(&d);
        cur = d.parent();
    }
}

/// Inbox からコピーし、補正タグを書き、Library に置く
fn place_files(
    library: &RootDir,
    inbox: &RootDir,
    item: &Item,
    plan: &ItemPlan,
    files: &HashMap<String, FileRow>,
    sources: &[Source],
) -> Result<Placed, InboxError> {
    let created_top = create_dirs(library, &plan.rel_dir)?;
    let mut placed_new: Vec<RelPath> = Vec::new();
    let result = (|| -> Result<Placed, InboxError> {
        let mut tracks = Vec::with_capacity(plan.paths.len());
        for (target, src) in plan.paths.iter().zip(sources) {
            let ext_for_write = src.ext.clone();
            let changes = src.changes.clone();
            let src_fp = src.fp;
            // コピーに使う FD そのものを承認時の行（inode / size / mtime / ctime）と照合する。
            // read_sources の stat からここまでの間に差し替えられていれば Changed（D-68「コピー中に
            // stat が変わったら失敗」）。コピーの後にも同じ FD を見て、読んでいる間の変更を弾く
            let src_file = inbox.open_file(&src.rel)?;
            let before = crate::fsroot::fstat(&src_file)?;
            let row = files
                .get(&src.rel.key())
                .ok_or_else(|| InboxError::Changed(src.rel.to_string()))?;
            if !stat_matches(row, &before) {
                return Err(InboxError::Changed(src.rel.to_string()));
            }
            let outcome = place_one(
                library,
                &plan.rel_dir,
                target,
                &src_file,
                move |tmp: &mut File| {
                    if !changes.is_empty() {
                        write_tag_changes(tmp, ext_for_write.as_deref(), &changes, None).map_err(
                            |e| PlacementError::Io(std::io::Error::other(e.to_string())),
                        )?;
                    }
                    Ok(())
                },
                |_existing: File| {
                    // 宛先に既にあるファイルが同じ音声なら自分の成果物（再実行）
                    let ext = target.file_name().rsplit_once('.').map(|(_, e)| e);
                    let af = match read_audio_file(library.open_file(target)?, ext) {
                        Ok(af) => af,
                        Err(_) => return Ok(false),
                    };
                    Ok(same_audio(read_fingerprint(library, target, &af), src_fp))
                },
            )?;
            if outcome == PlacedFile::New {
                placed_new.push(target.clone());
            }
            let after = crate::fsroot::fstat(&src_file)?;
            if !stat_matches(row, &after) {
                return Err(InboxError::Changed(src.rel.to_string()));
            }
            let file = library.open_file(target)?;
            let ph = crate::fsroot::fstat(&file)?;
            let af = read_audio_file(file, src.ext.as_deref())
                .map_err(|e| InboxError::Conflict(format!("{target}: {e}")))?;
            let fp = read_fingerprint(library, target, &af);
            // 置いたものの音声が承認時に読んだ音声と同じことを確かめる（コピー中の書き換え、
            // 宛先の既存ファイルの採用、いずれも指紋で閉じる）
            if !same_audio(fp, src_fp) {
                return Err(InboxError::Changed(src.rel.to_string()));
            }
            let mut content = track_content(af);
            content.picture = PictureState::Unread;
            tracks.push((ph.into(), content, fp));
        }
        // 既知の同梱ファイル（cover 画像 / cue / toc / log）
        let mut companions = Vec::new();
        if !item.rel_dir.is_empty() {
            let dir =
                RelPath::parse(&item.rel_dir).map_err(|e| InboxError::Conflict(e.to_string()))?;
            for e in inbox.read_dir(Some(&dir))? {
                let Some(name) = e.name.to_str() else {
                    continue;
                };
                if e.kind != FileKind::File || !crate::cd::riplog::is_companion_name(name) {
                    continue;
                }
                let (Ok(from), Ok(to)) = (dir.join(name), plan.rel_dir.join(name)) else {
                    continue;
                };
                let mut body = Vec::new();
                inbox.open_file(&from)?.read_to_end(&mut body)?;
                let expected = body.clone();
                match place_one(
                    library,
                    &plan.rel_dir,
                    &to,
                    body.as_slice(),
                    |_| Ok(()),
                    move |mut f: File| {
                        let mut existing = Vec::new();
                        f.read_to_end(&mut existing)?;
                        Ok(existing == expected)
                    },
                ) {
                    Ok(PlacedFile::New) => {
                        placed_new.push(to);
                        companions.push(from);
                    }
                    Ok(PlacedFile::Reused) => companions.push(from),
                    Err(PlacementError::Conflict(r)) => {
                        tracing::warn!(reason = %r, "同梱ファイルは宛先に別の内容があるので Inbox に残す")
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
        library.fsync_dir(Some(&plan.rel_dir))?;
        Ok(Placed {
            tracks,
            placed_new: std::mem::take(&mut placed_new),
            created_top: created_top.clone(),
            companions,
        })
    })();
    match result {
        Ok(p) => Ok(p),
        Err(e) => {
            cleanup(library, &plan.rel_dir, &placed_new, created_top.as_ref());
            Err(e)
        }
    }
}

struct RegisteredItem {
    album_id: i64,
    track_ids: Vec<i64>,
    job_ids: Vec<i64>,
    /// 同じトランザクションで記録した normalize バッチ（WAV / ALAC / AIFF があり、有効なとき）
    normalize_batch: Option<i64>,
    batch_event: Option<crate::jobs::BatchEvent>,
}

/// 登録（1 トランザクション）: album / tracks / track_tags、rg / transcode の投入、WAV / ALAC / AIFF の
/// normalize バッチ（`normalize` のとき）、件の placed。commit の後に落ちても投入が欠けない
fn register_item(
    conn: &mut rusqlite::Connection,
    item_id: i64,
    plan: &ItemPlan,
    draft: &InboxDraft,
    placed: &Placed,
    files: &HashMap<String, FileRow>,
    normalize: bool,
) -> Result<Result<RegisteredItem, InboxError>, DbError> {
    let tx = conn.transaction()?;
    let now = now_epoch();
    let mb = mode(files.values().filter_map(|f| tag(f, "MUSICBRAINZ_ALBUMID")));
    let meta = AlbumMeta {
        category_id: plan.category_id,
        albumartist: Some(draft.albumartist.trim().to_owned()),
        album: Some(draft.album.trim().to_owned()),
        date: draft
            .date
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_owned),
        original_date: None,
        mb_release_id: mb,
        discid: None,
        disc_count: Some(i64::from(draft.disc_count())),
    };
    let album_id = match find_or_create_album(&tx, &plan.rel_dir, &plan.release, &meta) {
        Ok(Ok(id)) => id,
        Ok(Err(reason)) => {
            drop(tx);
            return Ok(Err(InboxError::Conflict(reason)));
        }
        Err(e) => {
            drop(tx);
            return Ok(Err(e.into()));
        }
    };
    let album_name: Option<String> = tx
        .query_row("SELECT album FROM albums WHERE id = ?1", [album_id], |r| {
            r.get(0)
        })
        .optional()?
        .flatten();
    let mut track_ids = Vec::with_capacity(plan.paths.len());
    for (rel, (ph, content, fp)) in plan.paths.iter().zip(&placed.tracks) {
        let id = match register_track(&tx, rel, ph, content, *fp, now) {
            Ok(Ok(r)) => r.id,
            Ok(Err(reason)) => {
                drop(tx);
                return Ok(Err(InboxError::Conflict(reason)));
            }
            Err(e) => {
                drop(tx);
                return Ok(Err(e.into()));
            }
        };
        scans::set_track_album(&tx, id, album_id, album_name.as_deref())?;
        scans::set_source_type(&tx, id, "download")?;
        track_ids.push(id);
    }
    let mut job_ids = vec![crate::db::jobs::enqueue(&tx, &new_album_job(album_id), now)?.id()];
    for &id in &track_ids {
        if let Some(j) = crate::db::derived::enqueue_if_stale(&tx, id, now)? {
            job_ids.push(j);
        }
    }
    // 後続の normalize（D-46 の予告。WAV / ALAC / AIFF を FLAC へ）も同じトランザクションで記録する。
    // 別トランザクションにすると、登録の commit からその間に落ちたとき投入が永久に欠ける
    let mut normalize_batch = None;
    let mut batch_event = None;
    if normalize {
        let ids: Vec<i64> = track_ids
            .iter()
            .zip(&draft.tracks)
            .filter(|(_, t)| {
                files
                    .get(&canonical_key(&t.rel_path))
                    .is_some_and(|f| matches!(f.codec.as_str(), "wav" | "alac" | "aiff"))
            })
            .map(|(id, _)| *id)
            .collect();
        if !ids.is_empty() {
            match record_normalize(&tx, &ids, now) {
                Ok(Some(p)) => {
                    job_ids.extend(p.job_ids);
                    normalize_batch = Some(p.batch_id);
                    batch_event = p.event;
                }
                Ok(None) => {}
                Err(e) => {
                    return Ok(Err(InboxError::Conflict(format!(
                        "normalize を投入できない: {e}"
                    ))))
                }
            }
        }
    }
    // 件の placed も同じトランザクションで確定する。commit の直後に落ちても placing のまま残らず
    // （Inbox の原本が残っていれば走査が pending に戻す）、24 時間の placed 表示も失わない
    dbinbox::set_placed(&tx, item_id, album_id, now)?;
    tx.commit()?;
    Ok(Ok(RegisteredItem {
        album_id,
        track_ids,
        job_ids,
        normalize_batch,
        batch_event,
    }))
}

/// 可逆（WAV / ALAC / AIFF）を FLAC に正規化する編集バッチを、登録のトランザクションの中で記録する。
/// 宛先が衝突するトラックは警告して外す。記録する対象が無ければ None
fn record_normalize(
    tx: &rusqlite::Connection,
    ids: &[i64],
    now: i64,
) -> Result<Option<crate::edit::Prepared>, crate::edit::EditError> {
    use crate::edit::{
        plan_normalize_tx, prepare_normalize_in, EditError, NormalizePlan, NormalizeTarget,
    };
    let targets: Vec<NormalizeTarget> = plan_normalize_tx(tx, ids)?
        .into_iter()
        .filter_map(|p| match p.planned {
            NormalizePlan::Path(new_rel_path) => Some(NormalizeTarget {
                track_id: p.track_id,
                new_rel_path,
                new_codec: "flac".to_owned(),
                expected: None,
                planned_conflict: None,
            }),
            NormalizePlan::Unchanged => None,
            NormalizePlan::Conflict(r) => {
                tracing::warn!(track_id = p.track_id, reason = %r, "normalize の宛先が衝突");
                None
            }
        })
        .collect();
    if targets.is_empty() {
        return Ok(None);
    }
    match prepare_normalize_in(tx, Some("Inbox 取り込み"), &targets, None, now) {
        Ok(p) => Ok(Some(p)),
        Err(EditError::NoChanges) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Inbox 側を消す（コピー中に変わっていなければ）。空になったディレクトリも消す
fn consume_inbox(
    inbox: &RootDir,
    item: &Item,
    files: &HashMap<String, FileRow>,
    sources: &[Source],
    companions: &[RelPath],
) {
    for src in sources {
        let unchanged = files
            .get(&src.rel.key())
            .zip(inbox.stat(&src.rel).ok())
            .is_some_and(|(row, st)| stat_matches(row, &st));
        if !unchanged {
            tracing::warn!(path = %src.rel, "Inbox のファイルが配置の間に変わったので残す");
            continue;
        }
        if let Err(e) = inbox.unlink(&src.rel) {
            tracing::warn!(path = %src.rel, error = %e, "Inbox のファイルを消せない");
        }
    }
    for rel in companions {
        if let Err(e) = inbox.unlink(rel) {
            tracing::warn!(path = %rel, error = %e, "Inbox の同梱ファイルを消せない");
        }
    }
    if !item.rel_dir.is_empty() {
        if let Ok(dir) = RelPath::parse(&item.rel_dir) {
            match inbox.remove_dir(&dir) {
                Ok(()) | Err(FsError::NotFound) => {}
                Err(FsError::Io(e))
                    if e.raw_os_error() == Some(rustix::io::Errno::NOTEMPTY.raw_os_error()) => {}
                Err(e) => tracing::warn!(dir = %dir, error = %e, "Inbox のディレクトリを消せない"),
            }
        }
    }
}

/// approved の件を Library に配置して登録する。`library` の排他は呼び出し側（ハンドラ）が取る
pub async fn place_item(
    env: &PlaceItemEnv,
    item: &Item,
    token: &CancellationToken,
) -> Result<ItemPlaced, InboxError> {
    let draft: InboxDraft = match &item.draft {
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|e| InboxError::Conflict(format!("下書きを読めない: {e}")))?,
        None => return Err(InboxError::Conflict("下書きが無い".into())),
    };
    let item_id = item.id;
    let rows = env.db.read(move |c| dbinbox::files(c, item_id)).await?;
    let names: Vec<String> = rows.iter().map(|f| f.rel_path.clone()).collect();
    draft.validate(&names)?;
    let files: Arc<HashMap<String, FileRow>> = Arc::new(
        rows.into_iter()
            .map(|f| (canonical_key(&f.rel_path), f))
            .collect(),
    );
    // 1. Inbox 側を読む（stat の照合、フィンガープリント、補正）
    let sources: Arc<Vec<Source>> = {
        let (inbox, draft, files) = (Arc::clone(&env.inbox), draft.clone(), Arc::clone(&files));
        Arc::new(
            tokio::task::spawn_blocking(move || read_sources(&inbox, &draft, &files))
                .await
                .map_err(|e| {
                    std::io::Error::other(format!("Inbox の読み取りタスクが異常終了: {e}"))
                })??,
        )
    };
    // 2. 計画
    let plan = {
        let (layout, item, draft, files, sources) = (
            env.layout.clone(),
            item.clone(),
            draft.clone(),
            Arc::clone(&files),
            Arc::clone(&sources),
        );
        env.db
            .read(move |c| {
                let list: Vec<FileRow> = files.values().cloned().collect();
                Ok(plan_item(c, &layout, &item, &draft, &list, &sources))
            })
            .await??
    };
    if token.is_cancelled() {
        return Err(InboxError::Cancelled);
    }
    if let Some(hook) = &env.before_place {
        hook();
    }
    // 3. 配置
    let placed = {
        let (library, inbox, item, plan, files, sources) = (
            Arc::clone(&env.library),
            Arc::clone(&env.inbox),
            item.clone(),
            plan.clone(),
            Arc::clone(&files),
            Arc::clone(&sources),
        );
        tokio::task::spawn_blocking(move || {
            place_files(&library, &inbox, &item, &plan, &files, &sources)
        })
        .await
        .map_err(|e| std::io::Error::other(format!("配置タスクが異常終了: {e}")))??
    };
    let placed_new = placed.placed_new.clone();
    let created_top = placed.created_top.clone();
    let companions = placed.companions.clone();
    // 4. 登録（normalize の投入と件の placed も同じトランザクション）
    let normalize = env.wav_to_flac && env.editor.as_ref().is_some_and(|e| e.can_normalize());
    let registered = {
        let (plan_tx, draft_tx, files_tx) = (plan.clone(), draft.clone(), Arc::clone(&files));
        env.db
            .write(move |c| {
                register_item(
                    c, item_id, &plan_tx, &draft_tx, &placed, &files_tx, normalize,
                )
            })
            .await
    };
    let registered: Result<RegisteredItem, InboxError> = match registered {
        Ok(inner) => inner,
        Err(e) => Err(InboxError::Db(e)),
    };
    let registered = match registered {
        Ok(r) => r,
        Err(e) => {
            let library = Arc::clone(&env.library);
            let dir = plan.rel_dir.clone();
            let _ = tokio::task::spawn_blocking(move || {
                cleanup(&library, &dir, &placed_new, created_top.as_ref())
            })
            .await;
            return Err(e);
        }
    };
    // 5. Inbox 側を消す
    {
        let (inbox, item, files, sources) = (
            Arc::clone(&env.inbox),
            item.clone(),
            Arc::clone(&files),
            Arc::clone(&sources),
        );
        let _ = tokio::task::spawn_blocking(move || {
            consume_inbox(&inbox, &item, &files, &sources, &companions)
        })
        .await;
    }
    // 6. 通知（投入は登録のトランザクションで済んでいる）
    env.jobs.notify_enqueued(&registered.job_ids).await;
    if let Some(ev) = registered.batch_event {
        env.jobs.publish(Event::Batch(ev));
    }
    env.jobs.publish(Event::Library(LibraryEvent::Ids {
        scan_run_id: 0,
        track_ids: registered.track_ids.clone(),
    }));
    tracing::info!(
        item_id = item.id,
        album_id = registered.album_id,
        dir = %plan.rel_dir,
        tracks = registered.track_ids.len(),
        "Inbox の件を配置した"
    );
    Ok(ItemPlaced {
        album_id: registered.album_id,
        rel_dir: plan.rel_dir,
        track_ids: registered.track_ids,
        job_ids: registered.job_ids,
        normalize_batch: registered.normalize_batch,
    })
}

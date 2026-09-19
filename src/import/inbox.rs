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
            seen_files.insert(key);
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
                if let Some(files) = &s.files {
                    if item.state == ItemState::Placing {
                        // 配置中の件は触らない（同じジョブの中でしか placing にならない）
                        continue;
                    }
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

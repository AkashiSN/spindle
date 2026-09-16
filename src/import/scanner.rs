//! 4 相スキャン（SPEC §7.1、§6、D-24 / D-26 / D-29 / D-30 / D-32、D-38）。
//!
//! ```text
//! Phase 1  inventory:  root を walk して (rel_path_key, dev, inode, nlink, size, mtime, ctime) を固定。
//!                      symlink・不正名は対象外として一覧へ。古い .spindle-tmp-* を回収
//! Phase 2  candidates: 既存行のスナップショットと inventory 全体を domain::identity::resolve に渡す
//! Phase 3  read:       変更あり / 新規 / deep のエントリだけタグとフィンガープリントを並列に読む
//! Phase 4  commit:     1 トランザクションで path の 2 段階更新 → 属性・タグ・版 → 新規挿入 →
//!                      album 照合 → finalize（missing）
//! ```
//!
//! ジョブ基盤には依存しない。`Db` と `RootDir` と進捗コールバックと CancellationToken だけで動く
//! （ジョブとしての起動は `jobs::handlers::scan`）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::db::scans::{
    self, AlbumMeta, AlbumSnap, CacheColumns, Fingerprint, Physical, RunState, TrackContent,
    TrackSnap,
};
use crate::db::{now_epoch, Db, DbError};
use crate::domain::identity::{self, Decision, Entry, Identity, Via};
use crate::domain::relpath::{canonical_key, RelPath};
use crate::domain::tags::{read_audio_file, tag_hash, AudioFile, Codec, TagSet};
use crate::fsroot::{FileKind, FsError, RootDir, TMP_PREFIX};
use crate::media::fingerprint;

pub use crate::db::scans::ScanKind;

/// これより古い `.spindle-tmp-*` は取り残しとみなして回収する（並走中の tagwrite の tmp を
/// 消さないための猶予。D-38）
pub const TMP_RECLAIM_AGE_SECS: i64 = 3600;
/// 多値 ARTIST の表示用区切り（D-38）
pub const ARTIST_SEPARATOR: &str = ", ";

pub type Progress = Arc<dyn Fn(u64, u64) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SkipReason {
    /// symlink は辿らない
    Symlink,
    /// SMB / exFAT 制約に反する名前、または UTF-8 でない名前
    InvalidName,
    /// canonical key が inventory 内の別のファイルと同じ（case-sensitive な FS で大小文字だけ
    /// 違う 2 ファイルなど。ZFS insensitive では起きない）。後から見つかった方を対象外にする
    DuplicateKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: String,
    pub reason: SkipReason,
}

#[derive(Debug, Default)]
pub struct ScanReport {
    pub run_id: i64,
    pub files_seen: u64,
    pub new: u64,
    pub updated: u64,
    pub unchanged: u64,
    pub moved: u64,
    pub revived: u64,
    pub missing_marked: u64,
    pub errors: u64,
    pub tmp_removed: u64,
    /// Phase 2 の後に別のジョブ（tagwrite / rename）が更新した行。今回の読み取りは捨てて
    /// `seen` だけ更新した（次回スキャンで整合する）
    pub overtaken: u64,
    pub skipped: Vec<Skipped>,
    /// この run で DB の内容が変わった行（新規・更新・移動・復活・明け渡し・missing）。
    /// SSE `library` イベントに使う（200 件以下なら `ids`、超えたら `bulk`。SPEC §9）
    pub changed_ids: Vec<i64>,
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("キャンセルされた")]
    Cancelled,
    #[error("走査に失敗: {path}: {source}")]
    Walk {
        path: String,
        #[source]
        source: FsError,
    },
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("内部エラー: {0}")]
    Internal(#[from] anyhow::Error),
}

impl From<tokio::task::JoinError> for ScanError {
    fn from(e: tokio::task::JoinError) -> Self {
        ScanError::Internal(anyhow::anyhow!("走査タスクが異常終了: {e}"))
    }
}

pub struct Scanner {
    db: Arc<Db>,
    root: Arc<RootDir>,
    parallelism: usize,
}

/// Phase 1 の 1 エントリ
#[derive(Debug, Clone)]
struct InvEntry {
    rel: RelPath,
    key: String,
    /// 親ディレクトリ（root 直下なら None）
    dir: Option<RelPath>,
    dir_key: String,
    ph: Physical,
    ext_codec: Codec,
}

#[derive(Debug, Default, Clone)]
struct Inventory {
    entries: Vec<InvEntry>,
    skipped: Vec<Skipped>,
    tmp_removed: u64,
}

/// Phase 3 の読み取り結果
struct ReadResult {
    content: TrackContent,
    fp: Fingerprint,
    hardlink: bool,
}

impl Scanner {
    pub fn new(db: Arc<Db>, root: Arc<RootDir>, parallelism: usize) -> Self {
        Self {
            db,
            root,
            parallelism: parallelism.max(1),
        }
    }

    /// 走査を 1 回実行する。失敗・キャンセル時は `scan_runs` を `failed` / `cancelled` にして返す
    pub async fn run(
        &self,
        kind: ScanKind,
        progress: Progress,
        token: CancellationToken,
    ) -> Result<ScanReport, ScanError> {
        let run_id = self
            .db
            .write(move |c| scans::create_run(c, kind, now_epoch()))
            .await?;
        tracing::info!(run_id, kind = kind.as_str(), "スキャン開始");
        match self.run_inner(run_id, kind, &progress, &token).await {
            Ok(report) => {
                tracing::info!(
                    run_id,
                    files = report.files_seen,
                    new = report.new,
                    updated = report.updated,
                    unchanged = report.unchanged,
                    moved = report.moved,
                    missing = report.missing_marked,
                    errors = report.errors,
                    skipped = report.skipped.len(),
                    "スキャン完了"
                );
                Ok(report)
            }
            Err(e) => {
                let state = match e {
                    ScanError::Cancelled => RunState::Cancelled,
                    _ => RunState::Failed,
                };
                tracing::warn!(run_id, error = %e, state = state.as_str(), "スキャンが完了しなかった");
                let r = self
                    .db
                    .write(move |c| scans::finish_run(c, run_id, state, 0, 0, now_epoch()))
                    .await;
                if let Err(db_err) = r {
                    tracing::error!(run_id, error = %db_err, "scan_runs の終端更新に失敗");
                }
                Err(e)
            }
        }
    }

    async fn run_inner(
        &self,
        run_id: i64,
        kind: ScanKind,
        progress: &Progress,
        token: &CancellationToken,
    ) -> Result<ScanReport, ScanError> {
        // Phase 1
        let inv = {
            let root = Arc::clone(&self.root);
            let token = token.clone();
            tokio::task::spawn_blocking(move || walk(&root, &token)).await??
        };
        if token.is_cancelled() {
            return Err(ScanError::Cancelled);
        }
        let files_seen = inv.entries.len() as u64;

        // Phase 2
        let (tracks, albums, categories, genre_map) = self
            .db
            .read(|c| {
                Ok((
                    scans::load_track_snapshot(c)?,
                    scans::load_album_snapshot(c)?,
                    scans::load_categories(c)?,
                    scans::load_genre_map(c)?,
                ))
            })
            .await?;
        let rows: Vec<identity::Row> = tracks
            .iter()
            .map(|t| identity::Row {
                id: t.id,
                key: t.rel_path_key.clone(),
                dev: t.dev,
                inode: t.inode,
                size: t.size,
                mtime_ns: t.mtime_ns,
                ctime_ns: t.ctime_ns,
                audio_md5: t.audio_md5,
                missing: t.missing,
            })
            .collect();
        let entries: Vec<Entry> = inv
            .entries
            .iter()
            .map(|e| Entry {
                key: e.key.clone(),
                dev: e.ph.dev,
                inode: e.ph.inode,
                nlink: e.ph.nlink,
                size: e.ph.size,
                mtime_ns: e.ph.mtime_ns,
                ctime_ns: e.ph.ctime_ns,
            })
            .collect();
        let inv = Arc::new(inv);
        let (decisions, md5_cache) = {
            let root = Arc::clone(&self.root);
            let inv = Arc::clone(&inv);
            tokio::task::spawn_blocking(move || {
                let mut cache: HashMap<usize, Option<[u8; 16]>> = HashMap::new();
                let decisions = identity::resolve(&entries, &rows, &mut |i| {
                    let e = &inv.entries[i];
                    let m = lossless_md5(&root, &e.rel, e.ext_codec);
                    cache.insert(i, m);
                    m
                });
                (decisions, cache)
            })
            .await?
        };
        if token.is_cancelled() {
            return Err(ScanError::Cancelled);
        }

        // Phase 3: 読む必要があるエントリ
        let deep = kind == ScanKind::Deep;
        let need_read: Vec<usize> = decisions
            .iter()
            .enumerate()
            .filter(|(_, d)| match &d.identity {
                Identity::New => true,
                Identity::Existing { changed, .. } => *changed || deep,
            })
            .map(|(i, _)| i)
            .collect();
        let total = need_read.len() as u64;
        progress(0, total);
        let results: Arc<Mutex<HashMap<usize, Result<ReadResult, String>>>> = Arc::default();
        let sem = Arc::new(Semaphore::new(self.parallelism));
        let done = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut handles = Vec::with_capacity(need_read.len());
        for i in need_read {
            if token.is_cancelled() {
                return Err(ScanError::Cancelled);
            }
            let permit = Arc::clone(&sem)
                .acquire_owned()
                .await
                .map_err(|e| anyhow::anyhow!("semaphore: {e}"))?;
            let root = Arc::clone(&self.root);
            let inv = Arc::clone(&inv);
            let results = Arc::clone(&results);
            let cached = md5_cache.get(&i).copied();
            let hardlink = decisions[i].hardlink;
            let done = Arc::clone(&done);
            let progress = Arc::clone(progress);
            handles.push(tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let r = read_entry(&root, &inv.entries[i], cached, hardlink);
                results
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(i, r);
                let n = done.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                progress(n, total);
            }));
        }
        for h in handles {
            h.await?;
        }
        if token.is_cancelled() {
            return Err(ScanError::Cancelled);
        }
        let results = Arc::try_unwrap(results)
            .map_err(|_| anyhow::anyhow!("読み取り結果の参照が残っている"))?
            .into_inner()
            .unwrap_or_else(|e| e.into_inner());

        // Phase 4
        let inv = Arc::try_unwrap(inv).unwrap_or_else(|a| (*a).clone());
        let commit = Commit {
            run_id,
            deep,
            inv,
            decisions,
            tracks,
            albums,
            categories,
            genre_map,
            results,
        };
        let mut report = self.db.write(move |c| commit.apply(c)).await?;
        report.files_seen = files_seen;
        Ok(report)
    }
}

// ---------------------------------------------------------------- Phase 1

fn walk(root: &RootDir, token: &CancellationToken) -> Result<Inventory, ScanError> {
    let mut inv = Inventory::default();
    let now = now_epoch();
    let mut seen_keys: HashSet<String> = HashSet::new();
    let mut stack: Vec<Option<RelPath>> = vec![None];
    while let Some(dir) = stack.pop() {
        if token.is_cancelled() {
            return Err(ScanError::Cancelled);
        }
        let dir_display = dir
            .as_ref()
            .map(|d| d.as_str().to_owned())
            .unwrap_or_default();
        let entries = root
            .read_dir(dir.as_ref())
            .map_err(|source| ScanError::Walk {
                path: dir_display.clone(),
                source,
            })?;
        for e in entries {
            let Some(name) = e.name.to_str() else {
                inv.skipped.push(Skipped {
                    path: join_display(&dir_display, &e.name.to_string_lossy()),
                    reason: SkipReason::InvalidName,
                });
                continue;
            };
            let child = match &dir {
                Some(d) => d.join(name),
                None => RelPath::parse(name),
            };
            match e.kind {
                FileKind::Symlink => {
                    inv.skipped.push(Skipped {
                        path: join_display(&dir_display, name),
                        reason: SkipReason::Symlink,
                    });
                    continue;
                }
                FileKind::Other => continue,
                FileKind::Dir | FileKind::File => {}
            }
            let child = match child {
                Ok(c) => c,
                Err(_) => {
                    // 隠しファイルの不正名は黙って飛ばす（macOS の ._x 等）
                    if !name.starts_with('.') || e.kind == FileKind::Dir {
                        inv.skipped.push(Skipped {
                            path: join_display(&dir_display, name),
                            reason: SkipReason::InvalidName,
                        });
                    }
                    continue;
                }
            };
            if e.kind == FileKind::Dir {
                stack.push(Some(child));
                continue;
            }
            if name.starts_with(TMP_PREFIX) {
                reclaim_tmp(root, &child, now, &mut inv);
                continue;
            }
            if name.starts_with('.') {
                continue;
            }
            let Some(ext_codec) = child
                .file_name()
                .rsplit_once('.')
                .and_then(|(_, ext)| Codec::from_extension(ext))
            else {
                continue;
            };
            let st = match root.stat(&child) {
                Ok(st) => st,
                Err(FsError::NotFound) => continue, // walk 中に消えた
                Err(source) => {
                    return Err(ScanError::Walk {
                        path: child.as_str().to_owned(),
                        source,
                    })
                }
            };
            if st.kind != FileKind::File {
                continue;
            }
            let dir_key = dir.as_ref().map(RelPath::key).unwrap_or_default();
            let key = child.key();
            if !seen_keys.insert(key.clone()) {
                inv.skipped.push(Skipped {
                    path: child.as_str().to_owned(),
                    reason: SkipReason::DuplicateKey,
                });
                continue;
            }
            inv.entries.push(InvEntry {
                key,
                dir: dir.clone(),
                dir_key,
                rel: child,
                ph: Physical {
                    dev: st.dev,
                    inode: st.inode,
                    nlink: st.nlink,
                    size: st.size,
                    mtime_ns: st.mtime_ns,
                    ctime_ns: st.ctime_ns,
                },
                ext_codec,
            });
        }
    }
    Ok(inv)
}

fn join_display(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// 取り残された tmp を回収する。新しいものは並走中の書き込みかもしれないので残す
fn reclaim_tmp(root: &RootDir, rel: &RelPath, now: i64, inv: &mut Inventory) {
    let Ok(st) = root.stat(rel) else {
        return;
    };
    if st.kind != FileKind::File {
        return;
    }
    let age = now - st.mtime_ns / 1_000_000_000;
    if age < TMP_RECLAIM_AGE_SECS {
        return;
    }
    match root.unlink(rel) {
        Ok(()) => {
            tracing::info!(path = %rel, age_secs = age, "取り残された一時ファイルを回収した");
            inv.tmp_removed += 1;
        }
        Err(e) => tracing::warn!(path = %rel, error = %e, "一時ファイルを消せない"),
    }
}

// ---------------------------------------------------------------- Phase 2 / 3 の読み取り

/// 可逆の `audio_md5`。非可逆・未設定・読めないときは None
fn lossless_md5(root: &RootDir, rel: &RelPath, ext_codec: Codec) -> Option<[u8; 16]> {
    let ext = rel.file_name().rsplit_once('.').map(|(_, x)| x);
    let open = || root.open_file(rel).ok();
    match ext_codec {
        Codec::Flac => fingerprint::flac_streaminfo_md5(open()?)
            .map_err(|err| tracing::debug!(path = %rel, error = %err, "STREAMINFO を読めない"))
            .ok()
            .flatten(),
        Codec::Wav | Codec::Aiff => fingerprint::decoded_pcm_md5(open()?, ext)
            .map_err(|err| tracing::warn!(path = %rel, error = %err, "PCM の MD5 を計算できない"))
            .ok(),
        Codec::Aac => {
            // m4a は中身が ALAC のときだけ可逆
            let af = read_audio_file(open()?, ext).ok()?;
            if af.codec != Codec::Alac {
                return None;
            }
            fingerprint::decoded_pcm_md5(open()?, ext)
                .map_err(
                    |err| tracing::warn!(path = %rel, error = %err, "ALAC の MD5 を計算できない"),
                )
                .ok()
        }
        _ => None,
    }
}

/// 読み取ったファイルの音声フィンガープリント（可逆は `audio_md5`、非可逆は `audio_fp`）。
/// 計算できなければ `None` 入り（呼び出し側は旧値を残す）。編集バッチが外部差し替えを
/// 採用するときにもスキャナと同じ計算をするために公開する
pub fn read_fingerprint(root: &RootDir, rel: &RelPath, af: &AudioFile) -> Fingerprint {
    let ext = rel.file_name().rsplit_once('.').map(|(_, x)| x);
    if af.lossless {
        let ext_codec = ext.and_then(Codec::from_extension).unwrap_or(af.codec);
        Fingerprint::Md5(lossless_md5(root, rel, ext_codec))
    } else {
        let f = root.open_file(rel).ok().and_then(|f| {
            fingerprint::packet_fp(f, ext)
                .map_err(|err| tracing::warn!(path = %rel, error = %err, "audio_fp を計算できない"))
                .ok()
        });
        Fingerprint::Fp(f)
    }
}

/// 音声の変化: 同種のフィンガープリントが違う、または種類が変わった（可逆 ⇔ 非可逆の
/// 差し替え。旧値が片方しか無いので同種比較では見えない）
pub fn audio_changed(
    new: Fingerprint,
    old_md5: Option<[u8; 16]>,
    old_fp: Option<[u8; 32]>,
) -> bool {
    match (new, old_md5, old_fp) {
        (Fingerprint::Md5(Some(new)), Some(old), _) => new != old,
        (Fingerprint::Fp(Some(new)), _, Some(old)) => new != old,
        (Fingerprint::Md5(Some(_)), None, Some(_)) => true,
        (Fingerprint::Fp(Some(_)), Some(_), None) => true,
        _ => false,
    }
}

/// DB に書くフィンガープリント。計算できなかった（None）ときは旧値を残す
pub fn effective_fingerprint(
    new: Fingerprint,
    old_md5: Option<[u8; 16]>,
    old_fp: Option<[u8; 32]>,
) -> Fingerprint {
    match new {
        Fingerprint::Md5(None) => Fingerprint::Md5(old_md5),
        Fingerprint::Fp(None) => Fingerprint::Fp(old_fp),
        other => other,
    }
}

fn read_entry(
    root: &RootDir,
    e: &InvEntry,
    cached_md5: Option<Option<[u8; 16]>>,
    hardlink: bool,
) -> Result<ReadResult, String> {
    let ext = e.rel.file_name().rsplit_once('.').map(|(_, x)| x);
    let file = root.open_file(&e.rel).map_err(|err| err.to_string())?;
    let af = read_audio_file(file, ext).map_err(|err| err.to_string())?;
    let fp = match (af.lossless, cached_md5) {
        (true, Some(m)) => Fingerprint::Md5(m),
        (true, None) => Fingerprint::Md5(lossless_md5(root, &e.rel, e.ext_codec)),
        (false, _) => read_fingerprint(root, &e.rel, &af),
    };
    Ok(ReadResult {
        content: track_content(af),
        fp,
        hardlink,
    })
}

/// 読み取ったファイルのうち DB に書く内容（`tag_hash` とキャッシュ列を含む）
pub fn track_content(af: AudioFile) -> TrackContent {
    let cache = cache_columns(&af.tags);
    TrackContent {
        codec: af.codec.as_str().to_owned(),
        lossless: af.lossless,
        sample_rate: af.sample_rate,
        bit_depth: af.bit_depth,
        channels: af.channels,
        bitrate: af.bitrate,
        duration_ms: af.duration_ms,
        tag_hash: tag_hash(&af.tags),
        tags: af.tags,
        cache,
    }
}

/// 先頭の整数（"3/12" → 3）
fn leading_int(s: &str) -> Option<i64> {
    let digits: String = s
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// 表示・ソート用キャッシュ列をタグ集合から求める（編集バッチの overlay でも使う）
pub fn cache_columns(tags: &TagSet) -> CacheColumns {
    let artists: Vec<&str> = tags.values("ARTIST").collect();
    CacheColumns {
        title: tags.first("TITLE").map(str::to_owned),
        artist_display: (!artists.is_empty()).then(|| artists.join(ARTIST_SEPARATOR)),
        albumartist: tags
            .first("ALBUMARTIST")
            .or_else(|| artists.first().copied())
            .map(str::to_owned),
        track_no: tags.first("TRACKNUMBER").and_then(leading_int),
        disc_no: tags.first("DISCNUMBER").and_then(leading_int),
        date: tags.first("DATE").map(str::to_owned),
    }
}

// ---------------------------------------------------------------- Phase 4

struct Commit {
    run_id: i64,
    deep: bool,
    inv: Inventory,
    decisions: Vec<Decision>,
    tracks: Vec<TrackSnap>,
    albums: Vec<AlbumSnap>,
    categories: Vec<(i64, String)>,
    genre_map: Vec<(String, i64)>,
    results: HashMap<usize, Result<ReadResult, String>>,
}

/// album 照合のためのディレクトリ単位の集計
#[derive(Default)]
struct DirGroup {
    dir: Option<RelPath>,
    track_ids: Vec<i64>,
    /// 直前まで属していた album（既存行のみ）
    previous_albums: Vec<i64>,
    /// 何か変わった（新規・更新・移動）か。false なら最速パスだけの album で照合を省く
    dirty: bool,
}

impl Commit {
    fn apply(self, conn: &mut Connection) -> crate::db::Result<ScanReport> {
        let tx = conn.transaction()?;
        let now = now_epoch();
        let run_id = self.run_id;
        let snap: HashMap<i64, &TrackSnap> = self.tracks.iter().map(|t| (t.id, t)).collect();
        let mut report = ScanReport {
            run_id,
            tmp_removed: self.inv.tmp_removed,
            skipped: self.inv.skipped.clone(),
            ..Default::default()
        };
        let mut groups: HashMap<String, DirGroup> = HashMap::new();

        // Phase 2 のスナップショット後に編集バッチが pending op を作っているかもしれないので、
        // pending と版・ハッシュはこのトランザクションの中で読み直す（D-24）
        let pending = scans::load_pending_ops(&tx)?;

        // Phase 2 の後に別の書き手（tagwrite / rename）が更新した行は、今回の inventory と
        // 読み取りが古い。パスも属性もタグも触らず seen だけにして次回スキャンに委ねる
        let mut overtaken: HashSet<i64> = HashSet::new();
        for (i, d) in self.decisions.iter().enumerate() {
            let Identity::Existing {
                track_id, changed, ..
            } = d.identity
            else {
                continue;
            };
            let row = snap[&track_id];
            let path_differs = row.rel_path != self.inv.entries[i].rel.as_str();
            if !(changed || self.deep || path_differs) {
                continue; // 最速パスは seen だけなので照合不要
            }
            let current = scans::load_current_row(&tx, track_id)?;
            if current.overtaken(row, pending.get(&track_id)) {
                tracing::info!(track_id, path = %self.inv.entries[i].rel, "スナップショット後に別のジョブが更新した。今回は seen だけにする");
                overtaken.insert(track_id);
            }
        }
        report.overtaken = overtaken.len() as u64;

        // a. パスの 2 段階更新（pending の rename op があれば据え置いて衝突にする）
        let mut moves: Vec<(i64, String, String)> = Vec::new();
        for (i, d) in self.decisions.iter().enumerate() {
            let Identity::Existing { track_id, .. } = d.identity else {
                continue;
            };
            let e = &self.inv.entries[i];
            let row = snap[&track_id];
            if overtaken.contains(&track_id) {
                continue;
            }
            if let Some(op) = pending.get(&track_id) {
                if op.kind == "rename" {
                    // rel_path は rename op が所有する（overlay で先に変わっているかもしれない）。
                    // 外部移動の判定は op 記録時点の物理パスと比べる。自分の rename ジョブの
                    // 作業中の所在（source / 一時名 / 最終名）は衝突ではない（D-43）
                    let expected = op.expected_rel_path.as_deref().unwrap_or(&row.rel_path);
                    let ours = crate::edit::in_progress_keys(op.op_id, expected, &row.rel_path);
                    if !ours.contains(&e.key) {
                        scans::conflict_pending_op(
                            &tx,
                            op.op_id,
                            &format!("外部で {} へ移動されていた", e.rel),
                            now,
                        )?;
                        tracing::warn!(track_id, path = %e.rel, "pending の rename op と外部 rename が衝突");
                        // pending → conflict でバッジが変わる。表に通知する
                        report.changed_ids.push(track_id);
                        // op は終端になったので、以後は rel_path も実在パスに追随させる
                        // （overlay の最終名のまま残すと、rename ジョブは pending しか見ないので
                        // 次回スキャンまで DB が実在と食い違う）
                    } else {
                        continue;
                    }
                }
            }
            if row.rel_path == e.rel.as_str() {
                continue;
            }
            moves.push((track_id, e.rel.as_str().to_owned(), e.key.clone()));
        }
        // 宛先 key を今回 claim されなかった別の行（missing 行を含む）が占有していれば、その行の
        // key を同じトランザクションで明け渡させる。行は残り、finalize で missing になる（SPEC §7.1）
        let claimed_ids: HashSet<i64> = self
            .decisions
            .iter()
            .filter_map(|d| match d.identity {
                Identity::Existing { track_id, .. } => Some(track_id),
                Identity::New => None,
            })
            .collect();
        let by_key: HashMap<&str, &TrackSnap> = self
            .tracks
            .iter()
            .map(|t| (t.rel_path_key.as_str(), t))
            .collect();
        let mut evicted: Vec<i64> = Vec::new();
        for (id, _, key) in &moves {
            if let Some(holder) = by_key.get(key.as_str()) {
                if holder.id != *id && !claimed_ids.contains(&holder.id) {
                    evicted.push(holder.id);
                    tracing::warn!(
                        track_id = holder.id,
                        path = %holder.rel_path,
                        new_holder = id,
                        "パスを別のトラックに明け渡した（元の行は missing になる）"
                    );
                }
            }
        }
        scans::vacate_track_paths(&tx, &evicted)?;
        scans::move_track_paths(&tx, &moves)?;
        let moved_ids: HashSet<i64> = moves.iter().map(|(id, _, _)| *id).collect();
        report.moved = moves.len() as u64;
        report.changed_ids.extend(evicted.iter().copied());
        report.changed_ids.extend(moved_ids.iter().copied());

        // b/c. 既存行の更新と新規挿入
        for (i, d) in self.decisions.iter().enumerate() {
            let e = &self.inv.entries[i];
            if let Identity::Existing { track_id, .. } = &d.identity {
                if overtaken.contains(track_id) {
                    // 所在も album も DB の現在値が正しい。古い inventory の集計に入れない
                    scans::touch_seen(&tx, *track_id, run_id, now)?;
                    continue;
                }
            }
            let group = groups.entry(e.dir_key.clone()).or_insert_with(|| DirGroup {
                dir: e.dir.clone(),
                ..Default::default()
            });
            match &d.identity {
                Identity::Existing {
                    track_id,
                    via,
                    changed,
                    revived,
                } => {
                    let row = snap[track_id];
                    if *revived {
                        report.revived += 1;
                    }
                    if d.hardlink {
                        tracing::warn!(track_id, path = %e.rel, "hardlink（nlink > 1）。path だけで解決した");
                    }
                    if let Some(prev) = row.album_id {
                        group.previous_albums.push(prev);
                    }
                    group.track_ids.push(*track_id);
                    let read = self.results.get(&i);
                    if *revived && !moved_ids.contains(track_id) {
                        report.changed_ids.push(*track_id);
                    }
                    if !*changed && !self.deep {
                        scans::touch_seen(&tx, *track_id, run_id, now)?;
                        if *revived || moved_ids.contains(track_id) || *via != Via::Inode {
                            group.dirty = true;
                        } else {
                            report.unchanged += 1;
                        }
                        continue;
                    }
                    group.dirty = true;
                    scans::update_physical(&tx, *track_id, &e.ph, run_id, now)?;
                    match read {
                        Some(Ok(r)) => {
                            self.apply_content(&tx, row, pending.get(track_id), r)?;
                            report.updated += 1;
                            if !*revived && !moved_ids.contains(track_id) {
                                report.changed_ids.push(*track_id);
                            }
                        }
                        Some(Err(err)) => {
                            tracing::warn!(track_id, path = %e.rel, error = %err, "ファイルを読めない。物理属性だけ更新した");
                            report.errors += 1;
                            // nlink（hardlink バッジ）等の表示値は update_physical で変わっている
                            report.changed_ids.push(*track_id);
                        }
                        None => {
                            // 読む対象ではなかった（deep でない changed=false は上で処理済み）
                            report.unchanged += 1;
                        }
                    }
                }
                Identity::New => {
                    group.dirty = true;
                    match self.results.get(&i) {
                        Some(Ok(r)) => {
                            let id = scans::insert_track(
                                &tx,
                                e.rel.as_str(),
                                &e.key,
                                &e.ph,
                                &r.content,
                                r.fp,
                                run_id,
                                now,
                            )?;
                            if r.hardlink {
                                tracing::warn!(track_id = id, path = %e.rel, "hardlink（nlink > 1）の新規トラック");
                            }
                            group.track_ids.push(id);
                            report.new += 1;
                            report.changed_ids.push(id);
                        }
                        Some(Err(err)) => {
                            tracing::warn!(path = %e.rel, error = %err, "新規ファイルを読めない。登録しない");
                            report.errors += 1;
                        }
                        None => {
                            report.errors += 1;
                        }
                    }
                }
            }
        }

        // e. album 照合
        self.resolve_albums(&tx, groups, now)?;

        // f. finalize
        let missing = scans::finalize_missing(&tx, run_id, now)?;
        report.missing_marked = missing.len() as u64;
        report.changed_ids.extend(missing);
        // 1 行が複数の経路（conflict + 物理更新など）で入ることがある
        report.changed_ids.sort_unstable();
        report.changed_ids.dedup();
        scans::finish_run(
            &tx,
            run_id,
            RunState::Completed,
            self.inv.entries.len() as i64,
            report.errors as i64,
            now,
        )?;
        tx.commit()?;
        Ok(report)
    }

    /// 変更ありの既存行にタグ・フィンガープリント・版を反映する（pending の tags op は
    /// 論理フィールドを据え置く。D-24）
    ///
    /// 呼び出し側が「行はスナップショットから変わっていない」ことを確認済み（追い越された行は
    /// ここへ来ない）なので、版・ハッシュはスナップショットの値と比較してよい。pending は
    /// トランザクション内で読み直した現在値
    fn apply_content(
        &self,
        tx: &Connection,
        row: &TrackSnap,
        pending: Option<&scans::PendingOp>,
        r: &ReadResult,
    ) -> crate::db::Result<()> {
        let tags_pending = pending.is_some_and(|op| op.kind == "tags");
        if !tags_pending {
            let tag_version = match row.tag_hash {
                Some(old) if old != r.content.tag_hash => row.tag_version + 1,
                _ => row.tag_version,
            };
            scans::update_content(tx, row.id, &r.content, tag_version)?;
        }
        let audio_version = if audio_changed(r.fp, row.audio_md5, row.audio_fp) {
            row.audio_version + 1
        } else {
            row.audio_version
        };
        let fp = effective_fingerprint(r.fp, row.audio_md5, row.audio_fp);
        scans::update_fingerprint(tx, row.id, fp, audio_version)?;
        Ok(())
    }

    /// ディレクトリごとに album を引き当てる（SPEC §7.1「アルバムの照合」、D-32、D-38）。
    ///
    /// 候補の順は MBID / DiscID が 1 件一致 → 構成トラックの過半数が属していた album →
    /// そのディレクトリに既にある album → 新規。既存 album を別のディレクトリへ寄せられるのは
    /// 「旧 rel_dir が inventory に無い」か「旧 rel_dir の現在の構成の過半数が別の album に
    /// 属している」（= swap で入れ替わった）ときだけ
    fn resolve_albums(
        &self,
        tx: &Connection,
        groups: HashMap<String, DirGroup>,
        now: i64,
    ) -> crate::db::Result<()> {
        let albums_by_key: HashMap<&str, &AlbumSnap> = self
            .albums
            .iter()
            .map(|a| (a.rel_dir_key.as_str(), a))
            .collect();
        let albums_by_id: HashMap<i64, &AlbumSnap> =
            self.albums.iter().map(|a| (a.id, a)).collect();
        let majority_at: HashMap<&str, Option<i64>> = groups
            .iter()
            .map(|(k, g)| {
                (
                    k.as_str(),
                    majority_album(&g.previous_albums, g.track_ids.len()),
                )
            })
            .collect();
        // album A を他のディレクトリへ寄せてよいか: 旧 rel_dir が inventory に無い、または
        // 旧 rel_dir の現在の構成の過半数が**別の** album に属している（過半数なしは不可。D-38）
        let available = |a: &AlbumSnap| -> bool {
            match majority_at.get(a.rel_dir_key.as_str()) {
                None => true,
                Some(Some(other)) => *other != a.id,
                Some(None) => false,
            }
        };
        let mut claimed: HashSet<i64> = HashSet::new();

        let mut ordered: Vec<(&String, &DirGroup)> = groups.iter().collect();
        ordered.sort_by(|a, b| a.0.cmp(b.0));
        // 変更のないディレクトリは既存の album をそのまま保持する
        for (dir_key, group) in &ordered {
            if !group.dirty {
                if let Some(a) = albums_by_key.get(dir_key.as_str()) {
                    claimed.insert(a.id);
                }
            }
        }

        enum Target {
            Existing(i64),
            Rename(i64),
            New,
        }
        let mut plan: Vec<(&String, &DirGroup, Target, AlbumMeta)> = Vec::new();
        for (dir_key, group) in &ordered {
            let Some(dir) = &group.dir else {
                // root 直下のファイルは album を持たない。album から移ってきた行は所属を外す
                if group.dirty {
                    for track_id in &group.track_ids {
                        scans::clear_track_album(tx, *track_id)?;
                    }
                }
                continue;
            };
            let at_dir = albums_by_key.get(dir_key.as_str()).copied();
            if !group.dirty && at_dir.is_some() {
                continue;
            }
            let meta = self.album_meta(tx, dir, &group.track_ids)?;
            let target_for = |a: &AlbumSnap| {
                if a.rel_dir_key == **dir_key {
                    Target::Existing(a.id)
                } else {
                    Target::Rename(a.id)
                }
            };
            // 1. MBID / DiscID がちょうど 1 件一致
            let by_ids: Vec<&AlbumSnap> = self
                .albums
                .iter()
                .filter(|a| {
                    !claimed.contains(&a.id)
                        && (a.rel_dir_key == **dir_key || available(a))
                        && ((meta.mb_release_id.is_some() && a.mb_release_id == meta.mb_release_id)
                            || (meta.discid.is_some() && a.discid == meta.discid))
                })
                .collect();
            if let [a] = by_ids.as_slice() {
                claimed.insert(a.id);
                plan.push((dir_key, group, target_for(a), meta));
                continue;
            }
            // 2. 過半数が属していた album
            if let Some(a) = majority_at
                .get(dir_key.as_str())
                .copied()
                .flatten()
                .and_then(|id| albums_by_id.get(&id))
            {
                if !claimed.contains(&a.id) && (a.rel_dir_key == **dir_key || available(a)) {
                    claimed.insert(a.id);
                    plan.push((dir_key, group, target_for(a), meta));
                    continue;
                }
            }
            // 3. そのディレクトリにある album
            if let Some(a) = at_dir {
                if !claimed.contains(&a.id) {
                    claimed.insert(a.id);
                    plan.push((dir_key, group, Target::Existing(a.id), meta));
                    continue;
                }
            }
            plan.push((dir_key, group, Target::New, meta));
        }

        let mut renames: Vec<(i64, String, String)> = plan
            .iter()
            .filter_map(|(dir_key, g, t, _)| match t {
                Target::Rename(id) => Some((
                    *id,
                    g.dir
                        .as_ref()
                        .map(|d| d.as_str().to_owned())
                        .unwrap_or_default(),
                    (*dir_key).clone(),
                )),
                _ => None,
            })
            .collect();
        // ディレクトリを別の album に明け渡した album（誰にも claim されなかった）は rel_dir を
        // 空けておく。構成 0 なので finalize で missing になる
        for (dir_key, _, target, _) in &plan {
            if let Some(a) = albums_by_key.get(dir_key.as_str()) {
                let kept = matches!(target, Target::Existing(id) if *id == a.id);
                if !kept && !claimed.contains(&a.id) {
                    let displaced = format!("\0displaced:{}", a.id);
                    renames.push((a.id, displaced.clone(), displaced));
                    tracing::warn!(album_id = a.id, dir = %a.rel_dir, "ディレクトリが別の album に置き換わった");
                }
            }
        }
        scans::move_album_dirs(tx, &renames)?;

        for (dir_key, group, target, meta) in plan {
            let dir = group
                .dir
                .as_ref()
                .map(|d| d.as_str().to_owned())
                .unwrap_or_default();
            let album_id = match target {
                Target::Existing(id) | Target::Rename(id) => {
                    scans::update_album_meta(tx, id, &meta)?;
                    id
                }
                Target::New => scans::insert_album(tx, &dir, dir_key, &meta)?,
            };
            for track_id in &group.track_ids {
                scans::set_track_album(tx, *track_id, album_id, meta.album.as_deref())?;
            }
        }
        let _ = now;
        Ok(())
    }

    /// 構成トラックのタグから album のメタデータ（最頻値）を出す
    fn album_meta(
        &self,
        tx: &Connection,
        dir: &RelPath,
        track_ids: &[i64],
    ) -> crate::db::Result<AlbumMeta> {
        let values = load_tag_values(tx, track_ids)?;
        let mode = |key: &str| -> Option<String> {
            let mut counts: HashMap<&str, usize> = HashMap::new();
            for (k, v) in &values {
                if k == key {
                    *counts.entry(v.as_str()).or_default() += 1;
                }
            }
            counts
                .into_iter()
                .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
                .map(|(v, _)| v.to_owned())
        };
        let albumartist = mode("ALBUMARTIST").or_else(|| mode("ARTIST"));
        let category_id = self.infer_category(dir, &values);
        Ok(AlbumMeta {
            category_id,
            albumartist,
            album: mode("ALBUM"),
            date: mode("DATE"),
            original_date: mode("ORIGINALDATE"),
            mb_release_id: mode("MUSICBRAINZ_ALBUMID"),
            discid: mode("MUSICBRAINZ_DISCID"),
            disc_count: mode("DISCTOTAL").and_then(|s| leading_int(&s)),
        })
    }

    /// category: 先頭ディレクトリ名が語彙に一致 → GENRE → map → NULL（D-38）
    fn infer_category(&self, dir: &RelPath, values: &[(String, String)]) -> Option<i64> {
        let top = dir.components().next().map(canonical_key)?;
        if let Some((id, _)) = self.categories.iter().find(|(_, key)| *key == top) {
            return Some(*id);
        }
        for (k, v) in values {
            if k == "GENRE" {
                let gk = canonical_key(v);
                if let Some((_, id)) = self.genre_map.iter().find(|(g, _)| *g == gk) {
                    return Some(*id);
                }
            }
        }
        None
    }
}

fn majority_album(previous: &[i64], group_size: usize) -> Option<i64> {
    let mut counts: HashMap<i64, usize> = HashMap::new();
    for id in previous {
        *counts.entry(*id).or_default() += 1;
    }
    counts
        .into_iter()
        .filter(|(_, n)| *n * 2 > group_size)
        .max_by_key(|(_, n)| *n)
        .map(|(id, _)| id)
}

/// album メタデータの算出に使うキーだけを読む
fn load_tag_values(
    conn: &Connection,
    track_ids: &[i64],
) -> crate::db::Result<Vec<(String, String)>> {
    const KEYS: [&str; 8] = [
        "ALBUMARTIST",
        "ARTIST",
        "ALBUM",
        "DATE",
        "ORIGINALDATE",
        "MUSICBRAINZ_ALBUMID",
        "MUSICBRAINZ_DISCID",
        "DISCTOTAL",
    ];
    let mut stmt = conn.prepare_cached(
        "SELECT key, value FROM track_tags WHERE track_id = ?1 AND idx = 0
           AND key IN ('ALBUMARTIST','ARTIST','ALBUM','DATE','ORIGINALDATE',
                       'MUSICBRAINZ_ALBUMID','MUSICBRAINZ_DISCID','DISCTOTAL','GENRE')",
    )?;
    let _ = KEYS;
    let mut out = Vec::new();
    for id in track_ids {
        let rows = stmt.query_map([id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            out.push(row?);
        }
    }
    Ok(out)
}

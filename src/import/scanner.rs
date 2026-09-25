//! 4 相スキャン（SPEC §7.1、§6、D-24 / D-26 / D-29 / D-30 / D-32、D-38）。
//!
//! ```text
//! Phase 1  inventory:  root を walk して (rel_path_key, dev, inode, nlink, size, mtime, ctime) を固定。
//!                      symlink・不正名は対象外として一覧へ。古い .spindle-tmp-* を回収
//! Phase 2  candidates: 既存行のスナップショットと inventory 全体を domain::identity::resolve に渡す。
//!                      解決が要求しうる audio_md5 だけを先に並列で計算する（初回・移動なしなら 0 件）
//! Phase 3  read:       変更あり / 新規 / deep のエントリだけタグとフィンガープリントを並列に読む
//! Phase 4  commit:     1 トランザクションで path の 2 段階更新 → 属性・タグ・版 → 新規挿入 →
//!                      album 照合 → finalize（missing）
//! Phase 5  artwork:    構成が変わった / 同梱カバー画像が変わった / 未解決の album について
//!                      アートワークを解決し、原画像をキャッシュへ置いて thumbnail ジョブを投入
//!                      （`ArtworkStore` があるときだけ。P1-3、D-49）
//! ```
//!
//! ジョブ基盤には依存しない。`Db` と `RootDir` と進捗コールバックと CancellationToken だけで動く
//! （ジョブとしての起動は `jobs::handlers::scan`）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension as _};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::db::artwork::{self as dbart, AlbumArtworkState, CoverStat};
use crate::db::jobs as dbjobs;
use crate::db::replaygain as dbrg;
use crate::db::scans::{
    self, AlbumMeta, AlbumSnap, CacheColumns, Fingerprint, Physical, PictureState, RunState,
    TrackContent, TrackSnap,
};
use crate::db::{now_epoch, Db, DbError};
use crate::domain::identity::{self, Decision, Entry, Identity, Via};
use crate::domain::relpath::{canonical_key, RelPath};
use crate::domain::tags::{
    read_audio_file, read_audio_file_with_pictures, read_transfer_tags, tag_hash, AudioFile, Codec,
    TagSet,
};
use crate::fsroot::{self, FileKind, FsError, RootDir, TMP_PREFIX};
use crate::jobs::handlers::thumbnail::new_thumbnail_job;
use crate::media::artwork::{
    cover_rank, pick_embedded, register_track_picture, sniff, ArtworkStore, ImageInfo,
    MAX_COVER_BYTES,
};
use crate::media::fingerprint;

pub use crate::db::scans::ScanKind;

/// これより古い `.spindle-tmp-*` は取り残しとみなして回収する（並走中の tagwrite の tmp を
/// 消さないための猶予。D-38）
pub const TMP_RECLAIM_AGE_SECS: i64 = 3600;
/// 多値 ARTIST の表示用区切り（D-38）
pub const ARTIST_SEPARATOR: &str = ", ";

/// 進捗を出す相。Phase 2 の md5 計算は要求があるときだけ出る
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanPhase {
    /// Phase 2: 同一性解決が要求した audio_md5 の並列計算
    Md5,
    /// Phase 3: 変更あり / 新規 / deep のエントリの読み取り
    Read,
}

/// 進捗コールバック `(phase, done, total)`。各相は `(phase, 0, total)` で始まる
pub type Progress = Arc<dyn Fn(ScanPhase, u64, u64) + Send + Sync>;

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
    /// Phase 5 でアートワークを解決し直した album 数
    pub artwork_resolved: u64,
    /// Phase 5 で投入したジョブ（thumbnail）。呼び出し側がワーカーを起こす
    pub enqueued_jobs: Vec<i64>,
    /// Phase 5 が失敗した（run は completed のまま。予約は DB に残る）
    pub artwork_error: Option<String>,
    /// Library 直下のディレクトリから category の語彙に登録した数（D-92）
    pub categories_registered: u64,
    /// category が NULL だった album に直下のディレクトリ名で付けた数（D-92）
    pub albums_categorized: u64,
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

/// 未分類の置き場の既定名（`[layout].unsorted` の既定の先頭。D-92）
const DEFAULT_UNSORTED_DIR: &str = "_Unsorted";

pub struct Scanner {
    db: Arc<Db>,
    root: Arc<RootDir>,
    parallelism: usize,
    /// アートワークのキャッシュ（P1-3）。無ければ Phase 5 を行わない
    artwork: Option<Arc<ArtworkStore>>,
    /// ReplayGain の内部基準（LUFS。`[replaygain].reference_lufs`）。外部のタグ変更を取り込んだときの
    /// `rg_written_at` の判定に使う（D-48）
    rg_reference: f64,
    /// テスト用: Phase 5 の予約の前後で呼ぶ（[`BeforeArtworkHook`]）
    before_artwork: Mutex<Option<BeforeArtworkHook>>,
    /// category の語彙に自動で登録しない Library 直下のディレクトリ名（canonical key）。
    /// 既定は `_Unsorted`、`[layout].unsorted` の先頭の固定部分も足す（D-92）
    category_skip: Vec<String>,
}

/// Phase 5 のフック（テスト用。Phase 4 の commit 後の cancel / 停止を起こす）。引数は呼ばれる位置:
/// `"before_reserve"`（Phase 4 の commit 直後・Phase 5 の予約前）、`"after_reserve"`（候補を予約した
/// 直後・解決を始める前）
pub type BeforeArtworkHook = Arc<dyn Fn(&str) + Send + Sync>;

/// Phase 1 で見つけた同梱カバー画像（ディレクトリごとに最も優先度の高い 1 つ）
#[derive(Debug, Clone)]
struct CoverEntry {
    rel: RelPath,
    rank: u8,
    stat: CoverStat,
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
    /// dir_key → 同梱カバー画像
    covers: HashMap<String, CoverEntry>,
    /// spindle の rip.log（先頭行が署名）があるディレクトリの dir_key。ここに新規登録する行は
    /// `source_type = 'cd_rip'`（DB を消しても出自が戻る。D-67）
    rip_dirs: HashSet<String>,
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
            artwork: None,
            rg_reference: -18.0,
            before_artwork: Mutex::new(None),
            category_skip: vec![canonical_key(DEFAULT_UNSORTED_DIR)],
        }
    }

    /// `[layout].unsorted` の先頭の固定部分（`_Unsorted/{albumartist}/…` の `_Unsorted`）を、category の
    /// 語彙に自動で登録しないディレクトリに足す（D-92）。先頭がプレースホルダを含むなら何もしない
    pub fn with_unsorted_layout(mut self, template: &str) -> Self {
        if let Some(first) = template.split('/').next() {
            if !first.is_empty() && !first.contains('{') {
                let key = canonical_key(first);
                if !self.category_skip.contains(&key) {
                    self.category_skip.push(key);
                }
            }
        }
        self
    }

    /// ReplayGain の内部基準を設定する（既定 -18 LUFS。`Editor::with_replaygain_reference` と同じ値にする）
    pub fn with_replaygain_reference(mut self, reference_lufs: f64) -> Self {
        self.rg_reference = reference_lufs;
        self
    }

    /// アートワークの解決（Phase 5）を有効にする
    pub fn with_artwork(mut self, store: Arc<ArtworkStore>) -> Self {
        self.artwork = Some(store);
        self
    }

    /// テスト用: Phase 5 の予約の前後で呼ばれるフックを置く（[`BeforeArtworkHook`]）
    #[doc(hidden)]
    pub fn set_before_artwork_hook(&self, hook: BeforeArtworkHook) {
        *self
            .before_artwork
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(hook);
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
        // 同一性解決が要求しうる audio_md5 を先に並列で計算する（P1-0）。初回スキャンと移動の無い
        // 増分スキャンでは要求が無い（`identity::md5_requests`）。可逆のデコードを伴うので進捗を出す
        let requests = identity::md5_requests(&entries, &rows);
        let md5_cache: HashMap<usize, Option<[u8; 16]>> =
            self.compute_md5s(&inv, &requests, progress, token).await?;
        let decisions = {
            let cache = md5_cache.clone();
            tokio::task::spawn_blocking(move || {
                identity::resolve(&entries, &rows, &mut |i| cache.get(&i).copied().flatten())
            })
            .await?
        };
        if token.is_cancelled() {
            return Err(ScanError::Cancelled);
        }

        // 画像をキャッシュへ置けなかった行（`artwork_dirty`）は、物理属性が同じでも読み直す（D-61）
        let dirty_pictures: HashSet<i64> = tracks
            .iter()
            .filter(|t| t.artwork_dirty)
            .map(|t| t.id)
            .collect();
        let decisions: Vec<Decision> = decisions
            .into_iter()
            .map(|mut d| {
                if let Identity::Existing {
                    track_id, changed, ..
                } = &mut d.identity
                {
                    if dirty_pictures.contains(track_id) {
                        *changed = true;
                    }
                }
                d
            })
            .collect();

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
        progress(ScanPhase::Read, 0, total);
        let results: Arc<Mutex<HashMap<usize, Result<ReadResult, String>>>> = Arc::default();
        let done = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let work = {
            let root = Arc::clone(&self.root);
            let inv = Arc::clone(&inv);
            let results = Arc::clone(&results);
            let md5_cache = Arc::new(md5_cache.clone());
            let hardlinks: Arc<Vec<bool>> =
                Arc::new(decisions.iter().map(|d| d.hardlink).collect());
            let progress = Arc::clone(progress);
            let store = self.artwork.clone();
            Arc::new(move |i: usize| {
                let r = read_entry(
                    &root,
                    store.as_deref(),
                    deep,
                    &inv.entries[i],
                    md5_cache.get(&i).copied(),
                    hardlinks[i],
                );
                results
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(i, r);
                let n = done.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                progress(ScanPhase::Read, n, total);
            })
        };
        self.run_blocking_pool(need_read, token, work).await?;
        let results = Arc::try_unwrap(results)
            .map_err(|_| anyhow::anyhow!("読み取り結果の参照が残っている"))?
            .into_inner()
            .unwrap_or_else(|e| e.into_inner());

        // Phase 4
        let mut inv = Arc::try_unwrap(inv).unwrap_or_else(|a| (*a).clone());
        let covers = std::mem::take(&mut inv.covers);
        let commit = Commit {
            run_id,
            deep,
            rg_reference: self.rg_reference,
            inv,
            decisions,
            tracks,
            albums,
            categories,
            genre_map,
            category_skip: self.category_skip.clone(),
            results,
        };
        let mut report = self.db.write(move |c| commit.apply(c)).await?;
        report.files_seen = files_seen;

        // Phase 5。Phase 4 は commit 済み（missing の確定と scan_runs = completed を含む）なので、
        // ここでの cancel / 失敗は run の状態を戻さない。再解決の予約は DB に残っているので
        // 次のスキャンで続きを行う
        if let Some(store) = &self.artwork {
            self.artwork_hook("before_reserve");
            match self.resolve_artwork(store, deep, covers, token).await {
                Ok((resolved, jobs)) => {
                    report.artwork_resolved = resolved;
                    report.enqueued_jobs.extend(jobs);
                }
                Err(ScanError::Cancelled) => {
                    tracing::info!(
                        run_id,
                        "アートワークの解決をキャンセルした（次のスキャンで続きを行う）"
                    );
                }
                Err(e) => {
                    tracing::warn!(run_id, error = %e, "アートワークの解決に失敗した（次のスキャンでやり直す）");
                    report.artwork_error = Some(e.to_string());
                }
            }
        }
        Ok(report)
    }

    // ------------------------------------------------------------ 並列実行

    /// `items` を並列度の上限で `spawn_blocking` に流し、**起動した分は全部終わるまで待つ**。
    /// cancel は permit 待ちを中断し、起動済みの仕事の完了を待ってから `Cancelled` を返す
    /// （`spawn_blocking` は途中で止められないので、放置すると run が cancelled になった後も
    /// デコードと進捗が続き、直後の再実行と重なって並列度の上限を超える）。JoinError も同様に
    /// 残りを待ってから返す
    async fn run_blocking_pool(
        &self,
        items: Vec<usize>,
        token: &CancellationToken,
        work: Arc<dyn Fn(usize) + Send + Sync>,
    ) -> Result<(), ScanError> {
        let sem = Arc::new(Semaphore::new(self.parallelism));
        let mut handles = Vec::with_capacity(items.len());
        let mut outcome: Result<(), ScanError> = Ok(());
        for i in items {
            let permit = tokio::select! {
                _ = token.cancelled() => {
                    outcome = Err(ScanError::Cancelled);
                    break;
                }
                p = Arc::clone(&sem).acquire_owned() => match p {
                    Ok(p) => p,
                    Err(e) => {
                        outcome = Err(anyhow::anyhow!("semaphore: {e}").into());
                        break;
                    }
                },
            };
            // permit を待っている間に cancel されていれば起動しない
            if token.is_cancelled() {
                outcome = Err(ScanError::Cancelled);
                break;
            }
            let work = Arc::clone(&work);
            handles.push(tokio::task::spawn_blocking(move || {
                let _permit = permit;
                work(i);
            }));
        }
        for h in handles {
            if let Err(e) = h.await {
                if outcome.is_ok() {
                    outcome = Err(e.into());
                }
            }
        }
        if outcome.is_ok() && token.is_cancelled() {
            outcome = Err(ScanError::Cancelled);
        }
        outcome
    }

    // ------------------------------------------------------------ Phase 2

    /// `requests` のエントリの `audio_md5` を Phase 3 と同じ並列度で計算する。結果は Phase 3 の
    /// フィンガープリント計算にも渡す（同じファイルを二度デコードしない）
    async fn compute_md5s(
        &self,
        inv: &Arc<Inventory>,
        requests: &[usize],
        progress: &Progress,
        token: &CancellationToken,
    ) -> Result<HashMap<usize, Option<[u8; 16]>>, ScanError> {
        let total = requests.len() as u64;
        if total == 0 {
            return Ok(HashMap::new());
        }
        progress(ScanPhase::Md5, 0, total);
        let results: Arc<Mutex<HashMap<usize, Option<[u8; 16]>>>> = Arc::default();
        let done = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let work = {
            let root = Arc::clone(&self.root);
            let inv = Arc::clone(inv);
            let results = Arc::clone(&results);
            let progress = Arc::clone(progress);
            Arc::new(move |i: usize| {
                let e = &inv.entries[i];
                let m = lossless_md5(&root, &e.rel, e.ext_codec);
                results
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(i, m);
                let n = done.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                progress(ScanPhase::Md5, n, total);
            })
        };
        self.run_blocking_pool(requests.to_vec(), token, work)
            .await?;
        Ok(Arc::try_unwrap(results)
            .map_err(|_| anyhow::anyhow!("md5 の結果の参照が残っている"))?
            .into_inner()
            .unwrap_or_else(|e| e.into_inner()))
    }

    // ------------------------------------------------------------ Phase 5

    fn artwork_hook(&self, point: &str) {
        if let Some(hook) = self
            .before_artwork
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            hook(point);
        }
    }

    /// album のアートワークを解決し直す（SPEC §7.1「アートワーク」、D-49）。対象は deep なら全 album、
    /// それ以外は「未解決（`artwork_resolved_at IS NULL`。Phase 4 が行の変わったトラックの新旧 album を
    /// 予約する）」「同梱カバー画像の有無・stat が前回と違う」「参照中の原画像がキャッシュに無い」
    /// album。missing の album は触らない。画像の読み取りは並列（Phase 3 と同じ並列度）、DB 更新は
    /// 1 トランザクション。決められなかった album（I/O 失敗）は状態を動かさない
    async fn resolve_artwork(
        &self,
        store: &Arc<ArtworkStore>,
        deep: bool,
        covers: HashMap<String, CoverEntry>,
        token: &CancellationToken,
    ) -> Result<(u64, Vec<i64>), ScanError> {
        let states = self.db.read(dbart::album_states).await?;
        let candidates: Vec<(AlbumArtworkState, Option<CoverEntry>)> = states
            .into_iter()
            .filter(|s| !s.missing)
            .filter_map(|s| {
                let cover = covers.get(&s.rel_dir_key).cloned();
                let cover_changed = match (&s.cover, &cover) {
                    (None, None) => false,
                    (Some(a), Some(b)) => *a != b.stat,
                    _ => true,
                };
                // 参照中の原画像が無い・長さが違う（同じ長さの破損は deep のハッシュ照合で直す）
                let orig_lost = match (&s.artwork_sha256, &s.artwork_mime, s.artwork_bytes) {
                    (Some(sha), Some(mime), Some(len)) => {
                        !store.has_original_of_len(sha, mime, len as u64)
                    }
                    _ => false,
                };
                (deep || s.resolved_at.is_none() || cover_changed || orig_lost)
                    .then_some((s, cover))
            })
            .collect();
        if candidates.is_empty() {
            return Ok((0, Vec::new()));
        }
        // 候補を全件、解決を始める前に予約する（`artwork_resolved_at = NULL`）。成功した album だけ
        // `set_album_artwork` が予約を消すので、ここから先の cancel・DB エラー・panic・プロセス停止の
        // どこで止まっても、残りは次のスキャンで続きになる（deep だけを理由に対象になった album を含む）
        let ids: Vec<i64> = candidates.iter().map(|(s, _)| s.id).collect();
        let paths: HashMap<i64, Vec<String>> = self
            .db
            .write(move |c| {
                let tx = c.transaction()?;
                dbart::mark_unresolved(&tx, &ids)?;
                let mut out = HashMap::with_capacity(ids.len());
                for id in ids {
                    out.insert(id, dbart::album_track_paths(&tx, id)?);
                }
                tx.commit()?;
                Ok(out)
            })
            .await?;
        self.artwork_hook("after_reserve");
        if token.is_cancelled() {
            return Err(ScanError::Cancelled);
        }

        let sem = Arc::new(Semaphore::new(self.parallelism));
        let mut handles = Vec::with_capacity(candidates.len());
        for (state, cover) in candidates {
            if token.is_cancelled() {
                return Err(ScanError::Cancelled);
            }
            let permit = Arc::clone(&sem)
                .acquire_owned()
                .await
                .map_err(|e| anyhow::anyhow!("semaphore: {e}"))?;
            let root = Arc::clone(&self.root);
            let store = Arc::clone(store);
            let tracks = paths.get(&state.id).cloned().unwrap_or_default();
            handles.push(tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let album_id = state.id;
                (
                    album_id,
                    resolve_album_artwork(&root, &store, album_id, cover, &tracks),
                )
            }));
        }
        let mut resolved: Vec<ResolvedArtwork> = Vec::with_capacity(handles.len());
        // 決められなかった album は予約（NULL）のまま残る
        for h in handles {
            if let (_, Some(r)) = h.await? {
                resolved.push(r);
            }
        }
        if token.is_cancelled() {
            return Err(ScanError::Cancelled);
        }
        let n = resolved.len() as u64;
        let job_ids = self
            .db
            .write(move |c| {
                let tx = c.transaction()?;
                let now = now_epoch();
                let mut job_ids = Vec::new();
                for r in &resolved {
                    let artwork_id = match &r.found {
                        Some(f) => {
                            let id = dbart::upsert(
                                &tx,
                                &f.hash,
                                f.info.mime,
                                Some(f.info.width),
                                Some(f.info.height),
                                f.bytes,
                                f.origin,
                            )?;
                            if f.needs_thumbs {
                                if let dbjobs::EnqueueResult::Inserted(job_id) =
                                    dbjobs::enqueue(&tx, &new_thumbnail_job(id), now)?
                                {
                                    job_ids.push(job_id);
                                }
                            }
                            Some(id)
                        }
                        None => None,
                    };
                    dbart::set_album_artwork(&tx, r.album_id, artwork_id, r.cover.as_ref(), now)?;
                }
                tx.commit()?;
                Ok(job_ids)
            })
            .await?;
        tracing::info!(
            albums = n,
            thumbnail_jobs = job_ids.len(),
            "アートワークを解決した"
        );
        Ok((n, job_ids))
    }
}

/// 1 album のアートワークを**今**解決する（Inbox の配置直後に呼ぶ。P3-4、D-68）。スキャンの Phase 5 と
/// 同じ規則（同梱カバー画像 → 構成トラックの埋め込み画像）で、同じ DB 反映（`artwork` の upsert、
/// thumbnail の投入、`set_album_artwork`）をその album だけに行う。始める前に予約
/// （`artwork_resolved_at = NULL`）するので、途中で失敗しても次のスキャンが拾う。投入した job id を返す
pub async fn resolve_album_artwork_now(
    db: &Db,
    root: &Arc<RootDir>,
    store: &Arc<ArtworkStore>,
    album_id: i64,
) -> Result<Vec<i64>, DbError> {
    let (rel_dir, tracks) = db
        .write(move |c| {
            let tx = c.transaction()?;
            let rel_dir: Option<String> = tx
                .query_row(
                    "SELECT rel_dir FROM albums WHERE id = ?1 AND missing_since IS NULL",
                    [album_id],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(rel_dir) = rel_dir else {
                return Ok(None);
            };
            dbart::mark_unresolved(&tx, &[album_id])?;
            let tracks = dbart::album_track_paths(&tx, album_id)?;
            tx.commit()?;
            Ok(Some((rel_dir, tracks)))
        })
        .await?
        .unzip();
    let Some(rel_dir) = rel_dir else {
        return Ok(Vec::new());
    };
    let tracks = tracks.unwrap_or_default();
    let root = Arc::clone(root);
    let store = Arc::clone(store);
    let resolved = tokio::task::spawn_blocking(move || {
        let cover = match find_cover(&root, &rel_dir) {
            Ok(c) => c,
            Err(e) => {
                // 探索の I/O 失敗は「同梱カバーなし」ではない（Phase 1 なら Walk で止まる）。決めない
                tracing::warn!(album_id, dir = rel_dir, error = %e, "同梱カバー画像を探せない");
                return None;
            }
        };
        resolve_album_artwork(&root, &store, album_id, cover, &tracks)
    })
    .await?;
    let Some(r) = resolved else {
        // 決められなかった（読めない・読んでいる間に変わった）。予約のまま残し、次のスキャンで続き
        return Ok(Vec::new());
    };
    db.write(move |c| {
        let tx = c.transaction()?;
        let now = now_epoch();
        let mut job_ids = Vec::new();
        let artwork_id = match &r.found {
            Some(f) => {
                let id = dbart::upsert(
                    &tx,
                    &f.hash,
                    f.info.mime,
                    Some(f.info.width),
                    Some(f.info.height),
                    f.bytes,
                    f.origin,
                )?;
                if f.needs_thumbs {
                    if let dbjobs::EnqueueResult::Inserted(job_id) =
                        dbjobs::enqueue(&tx, &new_thumbnail_job(id), now)?
                    {
                        job_ids.push(job_id);
                    }
                }
                Some(id)
            }
            None => None,
        };
        dbart::set_album_artwork(&tx, r.album_id, artwork_id, r.cover.as_ref(), now)?;
        tx.commit()?;
        Ok(job_ids)
    })
    .await
}

/// `rel_dir` にある同梱カバー画像のうち最も優先度の高い 1 つ（Phase 1 と同じ規則）。ディレクトリが
/// 無ければ `Ok(None)`、一覧や stat の I/O 失敗は Err（Phase 1 の `Walk` に相当。「なし」とは区別する）
fn find_cover(root: &RootDir, rel_dir: &str) -> Result<Option<CoverEntry>, FsError> {
    let dir = if rel_dir.is_empty() {
        None
    } else {
        Some(RelPath::parse(rel_dir).map_err(|e| FsError::Io(std::io::Error::other(e)))?)
    };
    let entries = match root.read_dir(dir.as_ref()) {
        Ok(e) => e,
        Err(FsError::NotFound) => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut best: Option<CoverEntry> = None;
    for e in entries {
        if e.kind != FileKind::File {
            continue;
        }
        let Some(name) = e.name.to_str() else {
            continue;
        };
        let Some(rank) = cover_rank(name) else {
            continue;
        };
        if best.as_ref().is_some_and(|c| rank >= c.rank) {
            continue;
        }
        let rel = match &dir {
            Some(d) => d.join(name),
            None => RelPath::parse(name),
        }
        .map_err(|e| FsError::Io(std::io::Error::other(e)))?;
        match root.stat(&rel) {
            Ok(st) if st.kind == FileKind::File => {
                best = Some(CoverEntry {
                    rel,
                    rank,
                    stat: CoverStat {
                        inode: st.inode as i64,
                        size: st.size as i64,
                        mtime_ns: st.mtime_ns,
                        ctime_ns: st.ctime_ns,
                    },
                });
            }
            // 一覧と stat の間に消えた
            Ok(_) | Err(FsError::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(best)
}

/// 解決した画像（キャッシュに置いた後）
struct FoundArtwork {
    hash: [u8; 32],
    info: ImageInfo,
    bytes: usize,
    origin: &'static str,
    /// サムネイルがまだ無い（thumbnail ジョブを投入する）
    needs_thumbs: bool,
}

struct ResolvedArtwork {
    album_id: i64,
    found: Option<FoundArtwork>,
    /// 今回見つけた同梱カバー画像の stat（無ければ None）
    cover: Option<CoverStat>,
}

/// 1 album のアートワークを決める（ブロッキング）。同梱カバー画像 → 構成トラック（順に）の
/// 埋め込み画像。決められないとき（I/O 失敗、読んでいる間にファイルが変わった、構成トラックを
/// 読めない）は None を返し、album の状態を動かさない（次のスキャンでやり直す）。
/// 構成トラックが 1 本でも読めなければ「画像なし」も「後続が最初」も確定できないので None
fn resolve_album_artwork(
    root: &RootDir,
    store: &ArtworkStore,
    album_id: i64,
    cover: Option<CoverEntry>,
    tracks: &[String],
) -> Option<ResolvedArtwork> {
    let mut cover_stat: Option<CoverStat> = None;
    if let Some(c) = cover {
        match root.open_file(&c.rel) {
            Ok(mut file) => {
                let st = match fsroot::fstat(&file) {
                    Ok(st) => st,
                    Err(e) => {
                        tracing::warn!(path = %c.rel, error = %e, "同梱カバー画像の stat を取れない");
                        return None;
                    }
                };
                cover_stat = Some(CoverStat {
                    inode: st.inode as i64,
                    size: st.size as i64,
                    mtime_ns: st.mtime_ns,
                    ctime_ns: st.ctime_ns,
                });
                if st.size <= MAX_COVER_BYTES {
                    // 上限 + 1 までしか読まない（読んでいる間に伸びても無制限にはならない）
                    let mut bytes = Vec::with_capacity(st.size as usize);
                    let read = std::io::Read::read_to_end(
                        &mut std::io::Read::take(&mut file, MAX_COVER_BYTES + 1),
                        &mut bytes,
                    );
                    if let Err(e) = read {
                        tracing::warn!(path = %c.rel, error = %e, "同梱カバー画像を読めない");
                        return None;
                    }
                    // 読んでいる間に書き換えられていたら決めない（次のスキャンで stat が違うので読み直す）
                    match fsroot::fstat(&file) {
                        Ok(after) if same_cover(&st, &after) && bytes.len() as u64 == st.size => {}
                        Ok(_) => {
                            tracing::info!(path = %c.rel, "同梱カバー画像が読んでいる間に変わった");
                            return None;
                        }
                        Err(e) => {
                            tracing::warn!(path = %c.rel, error = %e, "同梱カバー画像の stat を取れない");
                            return None;
                        }
                    }
                    match sniff(&bytes) {
                        Some(info) => {
                            return register_artwork(store, bytes, info, "file").map(|found| {
                                ResolvedArtwork {
                                    album_id,
                                    found: Some(found),
                                    cover: cover_stat,
                                }
                            });
                        }
                        None => {
                            tracing::warn!(path = %c.rel, "同梱カバー画像として認識できない");
                        }
                    }
                } else {
                    tracing::warn!(path = %c.rel, size = st.size, "同梱カバー画像が大きすぎる");
                }
            }
            Err(FsError::NotFound) => {}
            Err(e) => {
                tracing::warn!(path = %c.rel, error = %e, "同梱カバー画像を開けない");
                return None;
            }
        }
    }
    for rel_path in tracks {
        let Ok(rel) = RelPath::parse(rel_path) else {
            tracing::warn!(path = rel_path, "構成トラックのパスが不正");
            return None;
        };
        let ext = rel.file_name().rsplit_once('.').map(|(_, x)| x);
        let file = match root.open_file(&rel) {
            Ok(f) => f,
            Err(e) => {
                tracing::info!(path = %rel, error = %e, "構成トラックを開けないのでアートワークを決めない");
                return None;
            }
        };
        let pictures = match read_transfer_tags(file, ext) {
            Ok(t) => t.pictures,
            Err(e) => {
                tracing::info!(path = %rel, error = %e, "構成トラックのタグを読めないのでアートワークを決めない");
                return None;
            }
        };
        let Some(pic) = pick_embedded(&pictures) else {
            continue;
        };
        let Some(info) = sniff(pic.data()) else {
            tracing::warn!(path = %rel, "埋め込み画像として認識できない");
            continue;
        };
        let bytes = pic.data().to_vec();
        return register_artwork(store, bytes, info, "embedded").map(|found| ResolvedArtwork {
            album_id,
            found: Some(found),
            cover: cover_stat,
        });
    }
    Some(ResolvedArtwork {
        album_id,
        found: None,
        cover: cover_stat,
    })
}

/// 同じ実体・同じ内容か（inode / size / mtime / ctime）
fn same_cover(a: &fsroot::Stat, b: &fsroot::Stat) -> bool {
    a.inode == b.inode && a.size == b.size && a.mtime_ns == b.mtime_ns && a.ctime_ns == b.ctime_ns
}

/// 画像をキャッシュへ置く。置けなければ None
fn register_artwork(
    store: &ArtworkStore,
    bytes: Vec<u8>,
    info: ImageInfo,
    origin: &'static str,
) -> Option<FoundArtwork> {
    let hash = ArtworkStore::hash_of(&bytes);
    if let Err(e) = store.put_original(&hash, info.mime, &bytes) {
        tracing::warn!(hash = %ArtworkStore::hex(&hash), error = %e, "原画像をキャッシュへ置けない");
        return None;
    }
    Some(FoundArtwork {
        hash,
        info,
        bytes: bytes.len(),
        origin,
        needs_thumbs: !store.missing_thumbs(&hash).is_empty(),
    })
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
            if crate::cd::riplog::is_rip_log_name(name) {
                let dir_key = dir.as_ref().map(RelPath::key).unwrap_or_default();
                if !inv.rip_dirs.contains(&dir_key) && is_spindle_rip_log(root, &child) {
                    inv.rip_dirs.insert(dir_key);
                }
                continue;
            }
            if let Some(rank) = cover_rank(name) {
                // 同梱カバー画像。ディレクトリごとに最も優先度の高い 1 つだけ覚える
                let dir_key = dir.as_ref().map(RelPath::key).unwrap_or_default();
                let better = inv.covers.get(&dir_key).is_none_or(|c| rank < c.rank);
                if better {
                    match root.stat(&child) {
                        Ok(st) if st.kind == FileKind::File => {
                            inv.covers.insert(
                                dir_key,
                                CoverEntry {
                                    rel: child,
                                    rank,
                                    stat: CoverStat {
                                        inode: st.inode as i64,
                                        size: st.size as i64,
                                        mtime_ns: st.mtime_ns,
                                        ctime_ns: st.ctime_ns,
                                    },
                                },
                            );
                        }
                        Ok(_) | Err(FsError::NotFound) => {}
                        Err(source) => {
                            return Err(ScanError::Walk {
                                path: child.as_str().to_owned(),
                                source,
                            })
                        }
                    }
                }
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
/// 先頭行が spindle の署名（[`crate::cd::riplog::RIP_LOG_SIGNATURE`]）の rip.log か。
/// 読めなければ false（出自を推定しないだけで、走査は止めない）
fn is_spindle_rip_log(root: &RootDir, rel: &RelPath) -> bool {
    use std::io::Read as _;
    let sig = crate::cd::riplog::RIP_LOG_SIGNATURE.as_bytes();
    let mut head = vec![0u8; sig.len() + 1];
    match root.open_file(rel) {
        Ok(mut f) => {
            let mut n = 0;
            while n < head.len() {
                match f.read(&mut head[n..]) {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(_) => return false,
                }
            }
            n > sig.len() && &head[..sig.len()] == sig && matches!(head[sig.len()], b'\n' | b'\r')
        }
        Err(e) => {
            tracing::debug!(path = %rel, error = %e, "rip.log を読めない");
            false
        }
    }
}

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
    lossless_md5_with(&|| root.open_file(rel).ok(), rel, ext, ext_codec)
}

/// [`lossless_md5`] の本体。`open` は読むたびに先頭から読める File を返す
fn lossless_md5_with(
    open: &dyn Fn() -> Option<std::fs::File>,
    path: &dyn std::fmt::Display,
    ext: Option<&str>,
    ext_codec: Codec,
) -> Option<[u8; 16]> {
    match ext_codec {
        Codec::Flac => fingerprint::flac_streaminfo_md5(open()?)
            .map_err(|err| tracing::debug!(path = %path, error = %err, "STREAMINFO を読めない"))
            .ok()
            .flatten(),
        Codec::Wav | Codec::Aiff => fingerprint::decoded_pcm_md5(open()?, ext)
            .map_err(|err| tracing::warn!(path = %path, error = %err, "PCM の MD5 を計算できない"))
            .ok(),
        Codec::Aac => {
            // m4a は中身が ALAC のときだけ可逆
            let af = read_audio_file(open()?, ext).ok()?;
            if af.codec != Codec::Alac {
                return None;
            }
            fingerprint::decoded_pcm_md5(open()?, ext)
                .map_err(
                    |err| tracing::warn!(path = %path, error = %err, "ALAC の MD5 を計算できない"),
                )
                .ok()
        }
        _ => None,
    }
}

fn fingerprint_with(
    open: &dyn Fn() -> Option<std::fs::File>,
    path: &dyn std::fmt::Display,
    ext: Option<&str>,
    af: &AudioFile,
) -> Fingerprint {
    if af.lossless {
        let ext_codec = ext.and_then(Codec::from_extension).unwrap_or(af.codec);
        Fingerprint::Md5(lossless_md5_with(open, path, ext, ext_codec))
    } else {
        let f = open().and_then(|f| {
            fingerprint::packet_fp(f, ext)
                .map_err(
                    |err| tracing::warn!(path = %path, error = %err, "audio_fp を計算できない"),
                )
                .ok()
        });
        Fingerprint::Fp(f)
    }
}

/// 読み取ったファイルの音声フィンガープリント（可逆は `audio_md5`、非可逆は `audio_fp`）。
/// 計算できなければ `None` 入り（呼び出し側は旧値を残す）。編集バッチが外部差し替えを
/// 採用するときにもスキャナと同じ計算をするために公開する
pub fn read_fingerprint(root: &RootDir, rel: &RelPath, af: &AudioFile) -> Fingerprint {
    let ext = rel.file_name().rsplit_once('.').map(|(_, x)| x);
    fingerprint_with(&|| root.open_file(rel).ok(), rel, ext, af)
}

/// [`read_fingerprint`] を、パスを開き直さずに開いている `file` そのもの（の複製 FD）で計算する。
/// タグと音声を同じ実体で確かめたいとき（Inbox の再実行の宛先の照合。D-86、codex 指摘）
pub fn read_fingerprint_fd(file: &std::fs::File, path: &RelPath, af: &AudioFile) -> Fingerprint {
    use std::io::{Seek, SeekFrom};
    let ext = path.file_name().rsplit_once('.').map(|(_, x)| x);
    let open = || {
        let mut f = file.try_clone().ok()?;
        f.seek(SeekFrom::Start(0)).ok()?;
        Some(f)
    };
    fingerprint_with(&open, path, ext, af)
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
    store: Option<&ArtworkStore>,
    verify_pictures: bool,
    e: &InvEntry,
    cached_md5: Option<Option<[u8; 16]>>,
    hardlink: bool,
) -> Result<ReadResult, String> {
    let ext = e.rel.file_name().rsplit_once('.').map(|(_, x)| x);
    let file = root.open_file(&e.rel).map_err(|err| err.to_string())?;
    let (af, pictures) = read_audio_file_with_pictures(file, ext).map_err(|err| err.to_string())?;
    let fp = match (af.lossless, cached_md5) {
        (true, Some(m)) => Fingerprint::Md5(m),
        (true, None) => Fingerprint::Md5(lossless_md5(root, &e.rel, e.ext_codec)),
        (false, _) => read_fingerprint(root, &e.rel, &af),
    };
    Ok(ReadResult {
        content: track_content_with_pictures(af, &pictures, store, verify_pictures),
        fp,
        hardlink,
    })
}

/// 読み取ったファイルのうち DB に書く内容（`tag_hash` とキャッシュ列を含む）。画像は読んでいない
/// 扱い（[`PictureState::Unread`]。`artwork_id` を触らない）
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
        picture: PictureState::Unread,
    }
}

/// [`track_content`] に加えて、トラック自身の埋め込み画像をキャッシュへ置いて記録する（D-61）。
/// `store` が無ければ画像は読んでいない扱い。キャッシュへ置けなければ [`PictureState::Failed`]
/// （`artwork_id` は据え置き、`artwork_dirty` で次のスキャンが読み直す）。`verify` は
/// [`register_track_picture`] を見よ
pub fn track_content_with_pictures(
    af: AudioFile,
    pictures: &[lofty::picture::Picture],
    store: Option<&ArtworkStore>,
    verify: bool,
) -> TrackContent {
    let mut c = track_content(af);
    if let Some(store) = store {
        c.picture = match register_track_picture(store, pictures, verify) {
            Ok(Some(p)) => PictureState::Found(p),
            Ok(None) => PictureState::Absent,
            Err(e) => {
                tracing::warn!(error = %e, "埋め込み画像をキャッシュへ置けない。次のスキャンで読み直す");
                PictureState::Failed(e.to_string())
            }
        };
    }
    c
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
    /// ReplayGain の内部基準（`rg_written_at` の判定）
    rg_reference: f64,
    inv: Inventory,
    decisions: Vec<Decision>,
    tracks: Vec<TrackSnap>,
    albums: Vec<AlbumSnap>,
    categories: Vec<(i64, String)>,
    genre_map: Vec<(String, i64)>,
    /// 語彙に自動で登録しない直下のディレクトリ（canonical key。D-92）
    category_skip: Vec<String>,
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
    fn apply(mut self, conn: &mut Connection) -> crate::db::Result<ScanReport> {
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

        // pending の archive op（ロスレス正規化。P1-4）は DB を先行更新せず、ジョブが元と宛先の
        // 両方を一時的に Library に置く。元（記録時点のパス）と宛先の key にあるエントリは
        // 「自分の作業中」なので、新規登録も移動も missing 判定もせず seen だけにする
        let mut in_progress: HashMap<String, i64> = HashMap::new();
        for (track_id, op) in &pending {
            if op.kind != "archive" {
                continue;
            }
            let (Some(expected), Some(target)) = (&op.expected_rel_path, &op.target_rel_path)
            else {
                continue;
            };
            for key in crate::edit::normalize_in_progress_keys(op.op_id, expected, target) {
                in_progress.insert(key, *track_id);
            }
        }
        let mut skipped_entries: HashSet<usize> = HashSet::new();
        for (i, e) in self.inv.entries.iter().enumerate() {
            if let Some(track_id) = in_progress.get(&e.key) {
                tracing::debug!(track_id, path = %e.rel, "正規化の作業中のパス。今回は seen だけにする");
                scans::touch_seen(&tx, *track_id, run_id, now)?;
                skipped_entries.insert(i);
            }
        }

        // Phase 2 の後に別の書き手（tagwrite / rename）が更新した行は、今回の inventory と
        // 読み取りが古い。パスも属性もタグも触らず seen だけにして次回スキャンに委ねる
        let mut overtaken: HashSet<i64> = HashSet::new();
        for (i, d) in self.decisions.iter().enumerate() {
            if skipped_entries.contains(&i) {
                continue;
            }
            let Identity::Existing {
                track_id, changed, ..
            } = d.identity
            else {
                continue;
            };
            let row = snap[&track_id];
            let e = &self.inv.entries[i];
            let path_differs = row.rel_path != e.rel.as_str();
            // dev の付け替え（D-62）は changed = false でも物理属性を書くので照合が要る
            let dev_differs = row.dev != Some(e.ph.dev);
            if !(changed || self.deep || path_differs || dev_differs) {
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
            if skipped_entries.contains(&i) {
                continue;
            }
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
            if skipped_entries.contains(&i) {
                continue;
            }
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
                        if row.dev != Some(e.ph.dev) {
                            // dev の付け替え（D-62）: 同じ実体なので読み直さないが、次回の段 1 が
                            // 引けるよう dev を現在値へ直す（他の物理属性は一致している）
                            scans::update_physical(&tx, *track_id, &e.ph, run_id, now)?;
                        } else {
                            scans::touch_seen(&tx, *track_id, run_id, now)?;
                        }
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
                            let jobs =
                                self.apply_content(&tx, row, pending.get(track_id), r, now)?;
                            report.enqueued_jobs.extend(jobs);
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
                    // Phase 2 の後に別の書き手（正規化の確定など）がこの key を持つ行を作って
                    // いれば、今回の判定は古い。挿入すると rel_path_key の UNIQUE を踏んで走査
                    // 全体が失敗するので、その行を seen だけにして次回に委ねる
                    let holder: Option<i64> = tx
                        .query_row(
                            "SELECT id FROM tracks WHERE rel_path_key = ?1",
                            [&e.key],
                            |r| r.get(0),
                        )
                        .optional()?;
                    if let Some(holder) = holder {
                        tracing::info!(track_id = holder, path = %e.rel, "スナップショット後に別のジョブがこのパスの行を作った。今回は seen だけにする");
                        scans::touch_seen(&tx, holder, run_id, now)?;
                        report.overtaken += 1;
                        continue;
                    }
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
                                Some(run_id),
                                now,
                            )?;
                            if self.inv.rip_dirs.contains(&e.dir_key) {
                                scans::set_source_type(&tx, id, "cd_rip")?;
                            }
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

        // e. album 照合。先に Library 直下のディレクトリ名を category の語彙に登録し（D-92）、
        //    照合で付ける category に間に合わせる
        let registered = self.register_top_categories(&tx, &groups)?;
        if registered > 0 {
            self.categories = scans::load_categories(&tx)?;
        }
        report.categories_registered = registered;
        self.resolve_albums(&tx, groups, now)?;
        // category が NULL のまま残っている album（語彙が後から入った既存の DB・変化の無い album）を
        // 直下のディレクトリ名で埋める。人が付けた値（NULL でない）は触らない（D-92）
        report.albums_categorized = self.fill_album_categories(&tx)?;

        // f. finalize
        let missing = scans::finalize_missing(&tx, run_id, now)?;
        report.missing_marked = missing.len() as u64;
        report.changed_ids.extend(missing);
        // 1 行が複数の経路（conflict + 物理更新など）で入ることがある
        report.changed_ids.sort_unstable();
        report.changed_ids.dedup();
        // g. アートワークの再解決を同じトランザクションで予約する（D-49）。行が変わったトラックの
        //    現在の album と、直前まで属していた album（分割・移動で構成を失った側）の両方。
        //    Phase 5 が cancel / 失敗 / 停止で終わっても DB に残るので、次のスキャンでやり直せる
        let mut dirty_albums: Vec<i64> = report
            .changed_ids
            .iter()
            .filter_map(|id| snap.get(id).and_then(|t| t.album_id))
            .collect();
        dirty_albums.extend(scans::album_ids_of_tracks(&tx, &report.changed_ids)?);
        dirty_albums.sort_unstable();
        dirty_albums.dedup();
        dbart::mark_unresolved(&tx, &dirty_albums)?;
        if self.deep {
            // deep は全 album を解決し直す。Phase 5 の前（この commit の直後）に止まっても
            // 予約が残るよう、ここで active な album 全件を予約する
            dbart::mark_all_unresolved(&tx)?;
        }
        // トラック自身の画像でサムネイルがまだ無いものは thumbnail ジョブを投入する（D-61。
        // dedup は artwork_id 単位なので同じ画像の数千トラックでも 1 本）
        let mut want_thumbs: Vec<[u8; 32]> = self
            .results
            .values()
            .filter_map(|r| r.as_ref().ok())
            .filter_map(|r| match &r.content.picture {
                PictureState::Found(p) if p.needs_thumbs => Some(p.sha256),
                _ => None,
            })
            .collect();
        want_thumbs.sort_unstable();
        want_thumbs.dedup();
        for sha in want_thumbs {
            if let Some(art) = dbart::get_by_sha256(&tx, &sha)? {
                report
                    .enqueued_jobs
                    .push(dbjobs::enqueue(&tx, &new_thumbnail_job(art.id), now)?.id());
            }
        }
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
    /// 論理フィールドを据え置く。D-24）。返り値は積んだジョブ id（音声の差し替えで RG の解析し直し）
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
        now: i64,
    ) -> crate::db::Result<Vec<i64>> {
        let tags_pending = pending.is_some_and(|op| op.kind == "tags");
        if !tags_pending {
            let tags_changed = row.tag_hash.is_some_and(|old| old != r.content.tag_hash);
            let tag_version = if tags_changed {
                row.tag_version + 1
            } else {
                row.tag_version
            };
            scans::update_content(tx, row.id, &r.content, tag_version)?;
            if tags_changed {
                // 外部のタグ変更: RG タグが消えた / 書き換わった / 解析値と一致する値が書かれた
                // を `rg_written_at` に反映する（D-48）
                dbrg::sync_written_at(tx, row.id, &r.content.tags, self.rg_reference, now)?;
            }
        }
        let mut jobs = Vec::new();
        if audio_changed(r.fp, row.audio_md5, row.audio_fp) {
            // 外部で音声が差し替わった: 解析値は古いので捨て、解析し直すジョブを積む（D-47、P4-22）
            jobs = dbrg::reset_and_reanalyze(tx, row.id, now)?;
            let fp = effective_fingerprint(r.fp, row.audio_md5, row.audio_fp);
            scans::update_fingerprint(tx, row.id, fp, row.audio_version + 1)?;
        } else {
            let fp = effective_fingerprint(r.fp, row.audio_md5, row.audio_fp);
            scans::update_fingerprint(tx, row.id, fp, row.audio_version)?;
        }
        Ok(jobs)
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
            // 1. MBID がちょうど 1 件一致（DiscID は 1 枚ごとの値なので album の鍵にしない。D-67 追記 3）
            let by_ids: Vec<&AlbumSnap> = self
                .albums
                .iter()
                .filter(|a| {
                    !claimed.contains(&a.id)
                        && (a.rel_dir_key == **dir_key || available(a))
                        && match (&meta.mb_release_id, &a.mb_release_id) {
                            // 大文字小文字は同じ MBID（D-84）
                            (Some(m), Some(x)) => m.eq_ignore_ascii_case(x),
                            _ => false,
                        }
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
            disc_count: mode("DISCTOTAL").and_then(|s| leading_int(&s)),
        })
    }

    /// Library 直下のディレクトリ名のうち、音声を含む album ディレクトリ（直下 + 1 段以上。
    /// `Anime/<albumartist>/<album>` の `Anime`）の親になっているものを、語彙に無ければ登録する（D-92）。
    /// root 直下のファイルと、直下のディレクトリそのものに置かれたファイル（`<dir>/01.flac`）は
    /// category を名乗らないので対象外。`category_skip`（`_Unsorted` 等）も登録しない。返り値は登録した数
    fn register_top_categories(
        &self,
        tx: &Connection,
        groups: &HashMap<String, DirGroup>,
    ) -> crate::db::Result<u64> {
        let mut tops: Vec<&str> = groups
            .values()
            .filter(|g| !g.track_ids.is_empty())
            .filter_map(|g| {
                let dir = g.dir.as_ref()?;
                let mut comps = dir.components();
                let first = comps.next()?;
                comps.next().map(|_| first)
            })
            .collect();
        tops.sort_unstable();
        tops.dedup();
        let mut known: HashSet<String> = self.categories.iter().map(|(_, k)| k.clone()).collect();
        let mut registered = 0;
        for name in tops {
            let key = canonical_key(name);
            if self.category_skip.contains(&key) || known.contains(&key) {
                continue;
            }
            if crate::db::categories::insert(tx, name)?.is_some() {
                registered += 1;
                tracing::info!(
                    category = name,
                    "Library 直下のディレクトリを category の語彙に登録した"
                );
            }
            known.insert(key);
        }
        Ok(registered)
    }

    /// category が NULL の active な album に、直下のディレクトリ名に当たる語彙を付ける（D-92）。
    /// 対象は NULL の行だけなので、一度埋まった後は `_Unsorted` 等の数件しか読まない
    fn fill_album_categories(&self, tx: &Connection) -> crate::db::Result<u64> {
        let mut filled = 0;
        for (album_id, rel_dir) in scans::albums_without_category(tx)? {
            let mut comps = rel_dir.split('/');
            let (Some(first), Some(_)) = (comps.next(), comps.next()) else {
                continue;
            };
            let key = canonical_key(first);
            if self.category_skip.contains(&key) {
                continue;
            }
            if let Some((id, _)) = self.categories.iter().find(|(_, k)| *k == key) {
                if scans::set_album_category_if_null(tx, album_id, *id)? {
                    filled += 1;
                }
            }
        }
        Ok(filled)
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

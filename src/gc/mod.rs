//! GC（SPEC §6「論理削除」/ §8、P1-11、D-56）。**物理削除を行う唯一の経路。**
//!
//! 判定（[`plan`]）と実行（[`execute_all`] と区分ごとの `execute_*`）を分ける。dry-run は
//! `GET /api/gc/preview` が [`plan`] を同期で呼んで返す。5 区分:
//!
//! - A `tracks`: `missing_since <= now - retention` かつ Library に実体が無い → 行を消す
//! - B `albums`: 同条件で構成トラック 0（A の削除後）→ 行を消す
//! - C Archive: `archived_files` の `held` で期限超 → unlink して `deleted`
//! - D Derived: `derived_files` に無い実体（A のトラックの行も無いものとして扱う）。
//!   `.spindle-tmp-*` と [`ORPHAN_GRACE_SECS`] 以内の実体は除外 → unlink、空ディレクトリも消す
//! - E artwork: `albums.artwork_id` からも編集履歴の `PICTURE` 値からも参照されない行（dir が
//!   [`ORPHAN_GRACE_SECS`] 以内なら残す。D-60）と、行の無い `thumbs/<hex>/`
//!
//! 実行順は A → B → E(行) を 1 トランザクション → C → D → E(dir)。ファイル削除は 1 件ずつ、
//! 失敗はログして続行し、最後に区分ごとの件数・バイト数を出す

use std::collections::HashSet;
use std::sync::Arc;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::db::{gc as dbgc, jobs as dbjobs, now_epoch, Db, DbError};
use crate::domain::relpath::{canonical_key, RelPath};
use crate::fsroot::{FileKind, FsError, RootDir, Stat, TMP_PREFIX};
use crate::media::artwork::ArtworkStore;

/// Derived の孤児として消すまでの猶予（in-flight の書き込みと手作業で置いた実体の保護）
pub const ORPHAN_GRACE_SECS: i64 = 24 * 3600;

/// GC が触る root
pub struct GcRoots {
    pub library: Arc<RootDir>,
    pub archive: Arc<RootDir>,
    pub derived: Arc<RootDir>,
    pub artwork: Arc<ArtworkStore>,
}

#[derive(Debug, thiserror::Error)]
pub enum GcError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("{what}を読めない: {source}")]
    Fs { what: String, source: FsError },
    #[error("キャンセルされた")]
    Cancelled,
    #[error("ブロッキングタスクが異常終了: {0}")]
    Join(#[from] tokio::task::JoinError),
}

// ---------------------------------------------------------------- 計画

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedTrack {
    pub id: i64,
    pub rel_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedAlbum {
    pub id: i64,
    pub rel_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedArchive {
    pub id: i64,
    pub rel_path: String,
    /// 実体が無ければ 0
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedFile {
    pub rel_path: String,
    pub bytes: u64,
    #[serde(skip)]
    pub inode: u64,
    #[serde(skip)]
    pub mtime_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedArtwork {
    pub id: i64,
    pub hex: String,
}

/// 消すものの一覧。[`plan`] は何も変更しない
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub now: i64,
    /// これ以前の `missing_since` が対象
    pub cutoff: i64,
    pub tracks: Vec<PlannedTrack>,
    pub albums: Vec<PlannedAlbum>,
    pub archived: Vec<PlannedArchive>,
    pub derived: Vec<PlannedFile>,
    /// Derived で見たディレクトリ（深い順。実行後に空なら消す）
    #[serde(skip)]
    pub derived_dirs: Vec<String>,
    pub artwork_rows: Vec<PlannedArtwork>,
    /// 行の無い `thumbs/<hex>`
    pub artwork_dirs: Vec<String>,
}

/// 消すものを決める（読み取りのみ）。`retention_secs` は `[gc].retention_days` を秒にしたもの
pub async fn plan(
    db: &Db,
    roots: &GcRoots,
    retention_secs: i64,
    now: i64,
) -> Result<Plan, GcError> {
    let cutoff = now.saturating_sub(retention_secs);
    let (missing, albums, archived, artwork_rows, hashes) = db
        .read(move |c| {
            Ok((
                dbgc::missing_tracks(c, cutoff)?,
                dbgc::missing_albums(c, cutoff)?,
                dbgc::eligible_archived(c, now)?,
                dbgc::unreferenced_artwork(c)?,
                dbgc::artwork_hashes(c)?,
            ))
        })
        .await?;

    // A: 実体が戻っていれば飛ばす（次のスキャンが復活させる）
    let library = Arc::clone(&roots.library);
    let tracks = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        for (id, rel_path) in missing {
            match RelPath::parse(&rel_path).map(|r| library.stat(&r)) {
                Ok(Err(FsError::NotFound)) => out.push(PlannedTrack { id, rel_path }),
                Ok(Ok(_)) => tracing::debug!(id, rel_path, "実体が戻っているので残す"),
                Ok(Err(e)) => {
                    tracing::warn!(id, rel_path, error = %e, "実体を確認できないので残す")
                }
                Err(e) => tracing::warn!(id, rel_path, error = %e, "rel_path が不正なので残す"),
            }
        }
        out
    })
    .await?;
    let track_ids: HashSet<i64> = tracks.iter().map(|t| t.id).collect();

    // B: 構成が A で消えるトラックだけなら消える
    let albums = albums
        .into_iter()
        .filter(|(_, _, members)| members.iter().all(|m| track_ids.contains(m)))
        .map(|(id, rel_dir, _)| PlannedAlbum { id, rel_dir })
        .collect();

    // C
    let archive = Arc::clone(&roots.archive);
    let archived = tokio::task::spawn_blocking(move || {
        archived
            .into_iter()
            .map(|(id, rel_path)| {
                let bytes = RelPath::parse(&rel_path)
                    .ok()
                    .and_then(|r| archive.stat(&r).ok())
                    .map_or(0, |st| st.size);
                PlannedArchive {
                    id,
                    rel_path,
                    bytes,
                }
            })
            .collect::<Vec<_>>()
    })
    .await?;

    // D
    let keys = db.read(move |c| dbgc::derived_keys(c, &track_ids)).await?;
    let derived_root = Arc::clone(&roots.derived);
    let (derived, derived_dirs) =
        tokio::task::spawn_blocking(move || walk_derived(&derived_root, &keys, now)).await??;

    // E: 行の無い hex と、この実行で行が消える hex
    let live: HashSet<String> = hashes.iter().map(|h| ArtworkStore::hex(h)).collect();
    let store = Arc::clone(&roots.artwork);
    let unreferenced: Vec<PlannedArtwork> = artwork_rows
        .into_iter()
        .map(|(id, sha)| PlannedArtwork {
            id,
            hex: ArtworkStore::hex(&sha),
        })
        .collect();
    let (aged_dirs, artwork_rows) = tokio::task::spawn_blocking(move || {
        let aged = list_thumb_dirs(&store, now);
        // 参照の無い行でも、dir が猶予内（アップロード直後・参照前）なら残す。dir が無い行は
        // 実体が無く使えないので猶予に関係なく消す
        let rows: Vec<PlannedArtwork> = unreferenced
            .into_iter()
            .filter(|a| aged.contains(&a.hex) || !store.dir().join(&a.hex).is_dir())
            .collect();
        (aged, rows)
    })
    .await?;
    let dropping: HashSet<&str> = artwork_rows.iter().map(|a| a.hex.as_str()).collect();
    let artwork_dirs = aged_dirs
        .iter()
        .filter(|hex| !live.contains(*hex) || dropping.contains(hex.as_str()))
        .cloned()
        .collect();

    Ok(Plan {
        now,
        cutoff,
        tracks,
        albums,
        archived,
        derived,
        derived_dirs,
        artwork_rows,
        artwork_dirs,
    })
}

/// Derived を再帰で走査し、行の無い実体（tmp と猶予内を除く）と見たディレクトリ（深い順）を返す
fn walk_derived(
    root: &RootDir,
    keys: &HashSet<String>,
    now: i64,
) -> Result<(Vec<PlannedFile>, Vec<String>), GcError> {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    let mut stack: Vec<Option<RelPath>> = vec![None];
    let grace_ns = now
        .saturating_sub(ORPHAN_GRACE_SECS)
        .saturating_mul(1_000_000_000);
    while let Some(dir) = stack.pop() {
        let entries = match root.read_dir(dir.as_ref()) {
            Ok(v) => v,
            // root が読めなければ計画できない。サブディレクトリはその下だけ飛ばす
            Err(e) => match &dir {
                None => {
                    return Err(GcError::Fs {
                        what: "Derived".to_owned(),
                        source: e,
                    })
                }
                Some(d) => {
                    tracing::warn!(rel_path = d.as_str(), error = %e, "Derived のディレクトリを読めないので飛ばす");
                    continue;
                }
            },
        };
        for entry in entries {
            let Some(name) = entry.name.to_str() else {
                tracing::warn!(?entry.name, "UTF-8 でない名前は触らない");
                continue;
            };
            let rel = match &dir {
                Some(d) => d.join(name),
                None => RelPath::parse(name),
            };
            let Ok(rel) = rel else { continue };
            match entry.kind {
                FileKind::Dir => {
                    dirs.push(rel.as_str().to_owned());
                    stack.push(Some(rel));
                }
                FileKind::File => {
                    if name.starts_with(TMP_PREFIX) || keys.contains(&canonical_key(rel.as_str())) {
                        continue;
                    }
                    let st = match root.stat(&rel) {
                        Ok(st) => st,
                        Err(e) => {
                            tracing::warn!(rel_path = rel.as_str(), error = %e, "stat できない");
                            continue;
                        }
                    };
                    if st.mtime_ns > grace_ns {
                        continue;
                    }
                    files.push(PlannedFile {
                        rel_path: rel.as_str().to_owned(),
                        bytes: st.size,
                        inode: st.inode,
                        mtime_ns: st.mtime_ns,
                    });
                }
                FileKind::Symlink | FileKind::Other => {}
            }
        }
    }
    // 深い順（子を先に消す）
    dirs.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    Ok((files, dirs))
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// [`ORPHAN_GRACE_SECS`] の境界（これ以前に触られた実体は回収してよい）
fn grace_boundary(now: i64) -> std::time::SystemTime {
    std::time::UNIX_EPOCH
        + std::time::Duration::from_secs(now.saturating_sub(ORPHAN_GRACE_SECS).max(0) as u64)
}

/// `thumbs/<hex>/` の行を消してよいか: dir が無い（実体が無く使えない）か、mtime が猶予を過ぎている。
/// 猶予内（置いた・上げ直した直後）なら残す
fn thumb_dir_is_collectable(store: &ArtworkStore, hex: &str, now: i64) -> bool {
    match std::fs::metadata(store.dir().join(hex)) {
        Ok(m) if m.is_dir() => m.modified().is_ok_and(|t| t <= grace_boundary(now)),
        _ => true,
    }
}

/// `thumbs/` 直下の hex 名のディレクトリ。[`ORPHAN_GRACE_SECS`] 以内に触られたものは除く
/// （スキャンの Phase 5 が原画像を置いてから行を入れるまでの間に消さない）
fn list_thumb_dirs(store: &ArtworkStore, now: i64) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(store.dir()) else {
        return Vec::new();
    };
    let grace = grace_boundary(now);
    rd.filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| {
            e.metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t <= grace)
        })
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| is_hex64(n))
        .collect()
}

// ---------------------------------------------------------------- 実行

/// 区分ごとの集計
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub planned: usize,
    pub deleted: usize,
    /// 消したバイト数（行だけの区分は 0）
    pub bytes: u64,
    /// 計画後に状態が変わっていて消さなかった
    pub skipped: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub tracks: Counts,
    pub albums: Counts,
    pub archived: Counts,
    pub derived: Counts,
    pub artwork_rows: Counts,
    pub artwork_dirs: Counts,
}

impl Summary {
    pub fn total_deleted(&self) -> usize {
        self.tracks.deleted
            + self.albums.deleted
            + self.archived.deleted
            + self.derived.deleted
            + self.artwork_rows.deleted
            + self.artwork_dirs.deleted
    }

    pub fn total_bytes(&self) -> u64 {
        self.archived.bytes + self.derived.bytes + self.artwork_dirs.bytes
    }
}

fn cancelled(token: &CancellationToken) -> Result<(), GcError> {
    if token.is_cancelled() {
        Err(GcError::Cancelled)
    } else {
        Ok(())
    }
}

/// A → B → E(行) を 1 トランザクションで消す。E(行) は削除の直前に、書き込みコネクションを
/// 持ったまま `thumbs/<hex>/` の猶予を確認し直す（計画の後に同じ画像が再アップロードされていれば
/// dir の mtime が今になっている）。アップロードは touch（FS）→ upsert（同じ writer）の順なので、
/// touch が先なら fresh を見て残し、この commit が先なら後続の upsert が行を作り直す
pub async fn execute_rows(db: &Db, roots: &GcRoots, plan: &Plan) -> Result<Summary, GcError> {
    let track_ids: Vec<i64> = plan.tracks.iter().map(|t| t.id).collect();
    let album_ids: Vec<i64> = plan.albums.iter().map(|a| a.id).collect();
    let artwork: Vec<PlannedArtwork> = plan.artwork_rows.clone();
    let cutoff = plan.cutoff;
    let now = plan.now;
    let store = Arc::clone(&roots.artwork);
    let (t, a, w) = db
        .transaction(move |c| {
            let t = dbgc::delete_tracks(c, &track_ids, cutoff)?;
            let a = dbgc::delete_albums(c, &album_ids, cutoff)?;
            let artwork_ids: Vec<i64> = artwork
                .iter()
                .filter(|a| thumb_dir_is_collectable(&store, &a.hex, now))
                .map(|a| a.id)
                .collect();
            let w = dbgc::delete_artwork(c, &artwork_ids)?;
            Ok((t, a, w))
        })
        .await?;
    let counts = |planned: usize, deleted: usize| Counts {
        planned,
        deleted,
        skipped: planned - deleted,
        ..Counts::default()
    };
    Ok(Summary {
        tracks: counts(plan.tracks.len(), t),
        albums: counts(plan.albums.len(), a),
        artwork_rows: counts(plan.artwork_rows.len(), w),
        ..Summary::default()
    })
}

/// C: Archive の退避ファイルを消して `deleted` に進める。1 件ごとに、行がまだ `held` で期限超か
/// （計画後の巻き戻し・再退避）を再確認し、`job_id` があればそのトラックのロックを取ってから
/// unlink する（巻き戻しの normalize ジョブと同時に走らない）。`deleted` への遷移は `held` からだけ
pub async fn execute_archive(
    db: &Db,
    roots: &GcRoots,
    plan: &Plan,
    token: &CancellationToken,
    job_id: Option<i64>,
) -> Result<Counts, GcError> {
    let mut counts = Counts {
        planned: plan.archived.len(),
        ..Counts::default()
    };
    for item in &plan.archived {
        cancelled(token)?;
        let (id, rel_path) = (item.id, item.rel_path.clone());
        let rechecked = db
            .write(move |c| dbgc::recheck_archived(c, id, &rel_path, now_epoch(), job_id))
            .await?;
        if rechecked.is_none() {
            counts.skipped += 1;
            tracing::info!(
                id,
                rel_path = item.rel_path,
                "計画の後に状態が変わったので残す"
            );
            continue;
        }
        let root = Arc::clone(&roots.archive);
        let rel_path = item.rel_path.clone();
        let unlinked = tokio::task::spawn_blocking(move || -> Result<Option<u64>, FsError> {
            let rel =
                RelPath::parse(&rel_path).map_err(|e| FsError::Io(std::io::Error::other(e)))?;
            match root.stat(&rel) {
                Ok(st) => {
                    root.unlink(&rel)?;
                    Ok(Some(st.size))
                }
                Err(FsError::NotFound) => Ok(None),
                Err(e) => Err(e),
            }
        })
        .await?;
        match unlinked {
            Ok(size) => {
                if let Some(size) = size {
                    counts.bytes += size;
                } else {
                    tracing::warn!(
                        id = item.id,
                        rel_path = item.rel_path,
                        "退避ファイルが既に無い"
                    );
                }
                let marked = db
                    .write(move |c| {
                        let marked = dbgc::mark_archived_deleted(c, id, now_epoch())?;
                        if let Some(job_id) = job_id {
                            crate::db::jobs::release_track_locks(c, job_id)?;
                        }
                        Ok(marked)
                    })
                    .await?;
                if marked {
                    counts.deleted += 1;
                } else {
                    counts.skipped += 1;
                }
            }
            Err(e) => {
                counts.failed += 1;
                tracing::warn!(id = item.id, rel_path = item.rel_path, error = %e, "退避ファイルを消せない");
                if let Some(job_id) = job_id {
                    db.write(move |c| crate::db::jobs::release_track_locks(c, job_id))
                        .await?;
                }
            }
        }
    }
    Ok(counts)
}

/// D: Derived の孤児を消し、空になったディレクトリを消す。unlink の前に、その key を指す
/// `derived_files` 行が無いことを確認して transcode と同じ排他予約（`derived_path_locks`）を
/// GC のジョブで取り（transcode は予約無しに宛先へ書かないので、持っている間は置き換えられない）、
/// stat し直して一覧時と inode / mtime が同じときだけ消す。予約は件ごとに解放する。
/// `job_id` が無い（ジョブ外）ときは予約せず行の有無だけ見る
pub async fn execute_derived(
    db: &Db,
    roots: &GcRoots,
    plan: &Plan,
    token: &CancellationToken,
    job_id: Option<i64>,
) -> Result<Counts, GcError> {
    let mut counts = Counts {
        planned: plan.derived.len(),
        ..Counts::default()
    };
    for item in &plan.derived {
        cancelled(token)?;
        let key = canonical_key(&item.rel_path);
        let locked = match job_id {
            Some(job_id) => {
                db.write(move |c| dbgc::lock_derived_for_gc(c, &key, job_id, now_epoch()))
                    .await?
            }
            None => !db.read(move |c| dbgc::derived_has_row(c, &key)).await?,
        };
        if !locked {
            counts.skipped += 1;
            tracing::info!(rel_path = item.rel_path, "行か予約が付いているので残す");
            continue;
        }
        let root = Arc::clone(&roots.derived);
        let planned = item.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<Option<u64>, FsError> {
            let rel = RelPath::parse(&planned.rel_path)
                .map_err(|e| FsError::Io(std::io::Error::other(e)))?;
            let st: Stat = root.stat(&rel)?;
            if st.inode != planned.inode || st.mtime_ns != planned.mtime_ns {
                return Ok(None);
            }
            root.unlink(&rel)?;
            Ok(Some(st.size))
        })
        .await?;
        if let Some(job_id) = job_id {
            db.write(move |c| crate::db::derived::unlock_paths(c, job_id))
                .await?;
        }
        match result {
            Ok(Some(size)) => {
                counts.deleted += 1;
                counts.bytes += size;
            }
            Ok(None) => {
                counts.skipped += 1;
                tracing::info!(rel_path = item.rel_path, "一覧の後に置き換えられたので残す");
            }
            Err(FsError::NotFound) => counts.skipped += 1,
            Err(e) => {
                counts.failed += 1;
                tracing::warn!(rel_path = item.rel_path, error = %e, "Derived の孤児を消せない");
            }
        }
    }
    // 空になったディレクトリ（深い順）。空でなければそのまま
    let root = Arc::clone(&roots.derived);
    let dirs = plan.derived_dirs.clone();
    tokio::task::spawn_blocking(move || {
        for d in dirs {
            let Ok(rel) = RelPath::parse(&d) else {
                continue;
            };
            match root.remove_dir(&rel) {
                Ok(()) => tracing::info!(rel_path = d, "空になった Derived のディレクトリを消した"),
                Err(FsError::NotFound) => {}
                Err(FsError::Io(e))
                    if e.raw_os_error() == Some(rustix::io::Errno::NOTEMPTY.raw_os_error()) => {}
                Err(e) => tracing::warn!(rel_path = d, error = %e, "ディレクトリを消せない"),
            }
        }
    })
    .await?;
    Ok(counts)
}

/// E(dir): 行の無い `thumbs/<hex>/` を再帰削除する。直前に、その hex の `artwork` 行が無いこと
/// （E(行) が参照の出現で skip されたとき・スキャンが行を入れたとき）と、dir がまだ猶予を過ぎて
/// いること（スキャンが触った）を再確認する
pub async fn execute_artwork_dirs(
    db: &Db,
    roots: &GcRoots,
    plan: &Plan,
    token: &CancellationToken,
) -> Result<Counts, GcError> {
    let mut counts = Counts {
        planned: plan.artwork_dirs.len(),
        ..Counts::default()
    };
    let grace = std::time::UNIX_EPOCH
        + std::time::Duration::from_secs(
            now_epoch().saturating_sub(ORPHAN_GRACE_SECS).max(0) as u64
        );
    for hex in &plan.artwork_dirs {
        cancelled(token)?;
        if !is_hex64(hex) {
            counts.skipped += 1;
            continue;
        }
        let h = hex.clone();
        if !db.read(move |c| dbgc::artwork_hex_is_orphan(c, &h)).await? {
            counts.skipped += 1;
            tracing::info!(hex, "計画の後に artwork 行が付いたので残す");
            continue;
        }
        let path = roots.artwork.dir().join(hex);
        let result = tokio::task::spawn_blocking(move || -> std::io::Result<Option<u64>> {
            if std::fs::metadata(&path)?.modified()? > grace {
                return Ok(None);
            }
            let mut bytes = 0;
            for e in std::fs::read_dir(&path)?.flatten() {
                bytes += e.metadata().map_or(0, |m| m.len());
            }
            std::fs::remove_dir_all(&path)?;
            Ok(Some(bytes))
        })
        .await?;
        match result {
            Ok(Some(bytes)) => {
                counts.deleted += 1;
                counts.bytes += bytes;
            }
            Ok(None) => {
                counts.skipped += 1;
                tracing::info!(hex, "計画の後に触られたので残す");
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => counts.skipped += 1,
            Err(e) => {
                counts.failed += 1;
                tracing::warn!(hex, error = %e, "アートワークのキャッシュを消せない");
            }
        }
    }
    Ok(counts)
}

/// 全区分を順に実行して集計を返す（ログも出す）。`job_id` はロック・予約の持ち主（無ければ取らない）
pub async fn execute_all(
    db: &Db,
    roots: &GcRoots,
    plan: &Plan,
    token: &CancellationToken,
    job_id: Option<i64>,
) -> Result<Summary, GcError> {
    cancelled(token)?;
    let mut summary = execute_rows(db, roots, plan).await?;
    summary.archived = execute_archive(db, roots, plan, token, job_id).await?;
    summary.derived = execute_derived(db, roots, plan, token, job_id).await?;
    summary.artwork_dirs = execute_artwork_dirs(db, roots, plan, token).await?;
    log_summary(&summary);
    Ok(summary)
}

/// 削除件数のログ（必須。D-56）
pub fn log_summary(s: &Summary) {
    tracing::info!(
        tracks = s.tracks.deleted,
        albums = s.albums.deleted,
        archived = s.archived.deleted,
        archived_bytes = s.archived.bytes,
        derived = s.derived.deleted,
        derived_bytes = s.derived.bytes,
        artwork_rows = s.artwork_rows.deleted,
        artwork_dirs = s.artwork_dirs.deleted,
        artwork_bytes = s.artwork_dirs.bytes,
        skipped = s.tracks.skipped
            + s.albums.skipped
            + s.archived.skipped
            + s.derived.skipped
            + s.artwork_rows.skipped
            + s.artwork_dirs.skipped,
        failed = s.archived.failed + s.derived.failed + s.artwork_dirs.failed,
        "GC を実行した"
    );
}

// ---------------------------------------------------------------- ジョブ行の掃除（P4-18）

/// 終端のジョブ行の保持期間（`[gc].jobs_done_days` / `jobs_failed_days`）。None は消さない
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobsRetention {
    pub done_secs: Option<i64>,
    pub failed_secs: Option<i64>,
}

impl JobsRetention {
    /// 日数から。0 は「消さない」
    pub fn from_days(done_days: u32, failed_days: u32) -> Self {
        let secs = |d: u32| (d > 0).then(|| i64::from(d) * 86_400);
        Self {
            done_secs: secs(done_days),
            failed_secs: secs(failed_days),
        }
    }

    fn cutoffs(&self, now: i64) -> (Option<i64>, Option<i64>) {
        (
            self.done_secs.map(|s| now.saturating_sub(s)),
            self.failed_secs.map(|s| now.saturating_sub(s)),
        )
    }
}

/// 保持期間を過ぎた終端のジョブ行を消す。返り値は (done + cancelled, failed)。
/// 実行中の gc ジョブ自身は running なので対象にならない
pub async fn prune_jobs(
    db: &Db,
    retention: &JobsRetention,
    now: i64,
) -> Result<(usize, usize), GcError> {
    let (done_before, failed_before) = retention.cutoffs(now);
    Ok(db
        .write(move |c| dbjobs::prune_terminal(c, done_before, failed_before))
        .await?)
}

/// [`prune_jobs`] が消す数（dry-run。`GET /api/gc/preview`）
pub async fn prunable_jobs(
    db: &Db,
    retention: &JobsRetention,
    now: i64,
) -> Result<(usize, usize), GcError> {
    let (done_before, failed_before) = retention.cutoffs(now);
    Ok(db
        .read(move |c| dbjobs::count_prunable(c, done_before, failed_before))
        .await?)
}

/// `GET /api/gc/preview` の応答（件数・バイト数と先頭 [`PREVIEW_SAMPLE`] 件）
pub const PREVIEW_SAMPLE: usize = 50;

#[derive(Debug, Clone, Serialize)]
pub struct PreviewSection {
    pub count: usize,
    pub bytes: u64,
    pub sample: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Preview {
    pub now: i64,
    pub cutoff: i64,
    pub tracks: PreviewSection,
    pub albums: PreviewSection,
    pub archived: PreviewSection,
    pub derived: PreviewSection,
    pub artwork_rows: PreviewSection,
    pub artwork_dirs: PreviewSection,
    /// 掃除するジョブ行の数（P4-18）
    pub jobs: PreviewJobs,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct PreviewJobs {
    pub done: usize,
    pub failed: usize,
}

impl Preview {
    pub fn with_jobs(mut self, (done, failed): (usize, usize)) -> Self {
        self.jobs = PreviewJobs { done, failed };
        self
    }

    pub fn of(plan: &Plan) -> Self {
        fn section<T>(items: &[T], bytes: u64, name: impl Fn(&T) -> String) -> PreviewSection {
            PreviewSection {
                count: items.len(),
                bytes,
                sample: items.iter().take(PREVIEW_SAMPLE).map(name).collect(),
            }
        }
        Self {
            now: plan.now,
            cutoff: plan.cutoff,
            tracks: section(&plan.tracks, 0, |t| t.rel_path.clone()),
            albums: section(&plan.albums, 0, |a| a.rel_dir.clone()),
            archived: section(
                &plan.archived,
                plan.archived.iter().map(|a| a.bytes).sum(),
                |a| a.rel_path.clone(),
            ),
            derived: section(
                &plan.derived,
                plan.derived.iter().map(|f| f.bytes).sum(),
                |f| f.rel_path.clone(),
            ),
            artwork_rows: section(&plan.artwork_rows, 0, |a| a.hex.clone()),
            artwork_dirs: section(&plan.artwork_dirs, 0, Clone::clone),
            jobs: PreviewJobs::default(),
        }
    }
}

//! G アップロード（設計 docs/development/2026-11-25-upload-design.md §5.8）。ステージング
//! `.spindle-upload-<id>/` を物理削除するのはここだけ。
//!
//! - (a) [`UploadGcReason::Discarded`]: `discarded_at` から `retention` 経ったもの → ステージングごと消す
//! - (b) [`UploadGcReason::Broken`]: マニフェストの無い・読めないもの（捨てる印の有無を問わない）で
//!   ディレクトリの mtime から `retention` 経ったもの → ステージングごと消す
//! - (c) [`UploadGcReason::Garbage`]: 生きているアップロードの `garbage` の項で、実体の mtime から
//!   `retention` 経ったもの（展開中の世代は除く） → その下を消して項を外す
//! - (d) [`UploadGcReason::MarkStale`]: 公開も破棄もされず `updated_at` から `[upload].stale_days`
//!   経ったもの → `discarded_at` を入れるだけ（消すのは後日の (a)）
//!
//! 計画時の根拠（[`UploadBasis`]）を持ち、消す直前にアップロードの鍵を取って読み直し、変わっていれば
//! 何もしない（D-90 と同じ）。ステージングごと消すときは鍵だけを取り（マニフェストは書き戻さない）、
//! `garbage` と放置は [`with_manifest`] の中で行う。鍵は std の Mutex なので必ず `spawn_blocking` の中で取る

use std::sync::Arc;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use super::{cancelled, Counts, GcError, GcRoots, Plan};
use crate::config::UploadConfig;
use crate::db::{now_epoch, Db};
use crate::domain::relpath::RelPath;
use crate::fsroot::{FileKind, FsError, RootDir};
use crate::import::upload::{
    list_uploads, read_manifest, with_manifest, with_upload_lock, Listed, Manifest, UploadError,
    UploadId, UploadLocks, MANIFEST_NAME,
};

const DAY_SECS: i64 = 86_400;
const NS: i64 = 1_000_000_000;

/// アップロードの GC に要るもの。`locks` は main が作った 1 つの表の clone（別に作ると直列化が効かない）
#[derive(Debug, Clone)]
pub struct GcUploads {
    pub locks: UploadLocks,
    pub cfg: UploadConfig,
}

/// 何をするか
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UploadGcReason {
    /// 捨てた（公開後の残りを含む）ステージングを消す
    Discarded,
    /// マニフェストの無い・読めないステージングを消す
    Broken,
    /// `garbage` の項（ステージング相対のパス）を消して外す
    Garbage { path: String },
    /// 放置したアップロードを破棄扱いにする（消さない）
    MarkStale,
}

/// 計画時の根拠。実行時に読み直して一致しなければ何もしない
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UploadBasis {
    /// マニフェストの `updated_at`（Broken は None）
    pub updated_at: Option<i64>,
    /// マニフェストの `discarded_at`
    pub discarded_at: Option<i64>,
    /// Discarded / Broken はステージング、Garbage はその項の実体の mtime（実体が無かったら 0）。
    /// MarkStale は記録だけで照合しない（同じ GC が先に `garbage` を外して書き戻すとステージングの
    /// mtime が変わるため。判定はマニフェストの中身で足りる）
    pub mtime_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedUpload {
    /// Inbox 直下のステージングの名前（`.spindle-upload-<id>`）
    pub dir: String,
    pub reason: UploadGcReason,
    #[serde(skip)]
    pub basis: UploadBasis,
    /// 消す見込みのバイト数（MarkStale は 0）
    pub bytes: u64,
}

impl PlannedUpload {
    /// preview の 1 行
    pub fn label(&self) -> String {
        match &self.reason {
            UploadGcReason::Discarded | UploadGcReason::Broken => self.dir.clone(),
            UploadGcReason::Garbage { path } => format!("{}/{path}", self.dir),
            UploadGcReason::MarkStale => format!("{}（放置。破棄扱いにする）", self.dir),
        }
    }
}

/// 対象を決める（読み取りのみ）。一覧できなければ警告して空（他の区分は続ける）
pub(super) fn plan_uploads(
    inbox: &RootDir,
    cfg: &UploadConfig,
    cutoff: i64,
    now: i64,
) -> Vec<PlannedUpload> {
    let listed = match list_uploads(inbox) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "アップロードのステージングを一覧できないので飛ばす");
            return Vec::new();
        }
    };
    let cutoff_ns = cutoff.saturating_mul(NS);
    let stale_secs = i64::from(cfg.stale_days).saturating_mul(DAY_SECS);
    let mut out = Vec::new();
    for item in listed {
        match item {
            Listed::Broken { dir, mtime_ns, .. } => {
                if mtime_ns <= cutoff_ns {
                    out.push(PlannedUpload {
                        dir: dir.as_str().to_owned(),
                        reason: UploadGcReason::Broken,
                        basis: UploadBasis {
                            updated_at: None,
                            discarded_at: None,
                            mtime_ns,
                        },
                        bytes: tree_bytes(inbox, &dir),
                    });
                }
            }
            Listed::Ok(m) => plan_manifest(inbox, &m, cutoff, cutoff_ns, now, stale_secs, &mut out),
        }
    }
    out
}

fn plan_manifest(
    inbox: &RootDir,
    m: &Manifest,
    cutoff: i64,
    cutoff_ns: i64,
    now: i64,
    stale_secs: i64,
    out: &mut Vec<PlannedUpload>,
) {
    let Ok(id) = UploadId::parse(&m.id) else {
        return;
    };
    let dir = id.staging_dir();
    let staging_mtime = match inbox.stat(&dir) {
        Ok(st) => st.mtime_ns,
        Err(e) => {
            tracing::warn!(upload = id.as_str(), error = %e, "ステージングを stat できないので飛ばす");
            return;
        }
    };
    let basis = |mtime_ns| UploadBasis {
        updated_at: Some(m.updated_at),
        discarded_at: m.discarded_at,
        mtime_ns,
    };
    if let Some(at) = m.discarded_at {
        // (a)。期限前なら何もしない（garbage もステージングごと消えるのを待つ）
        if at <= cutoff {
            out.push(PlannedUpload {
                dir: dir.as_str().to_owned(),
                reason: UploadGcReason::Discarded,
                basis: basis(staging_mtime),
                bytes: tree_bytes(inbox, &dir),
            });
        }
        return;
    }
    // (c)。放置の印より先に並べる（印を先に付けると discarded_at の照合で項を消せない）
    let unpacking = unpacking_paths(m);
    for g in &m.garbage {
        if unpacking.contains(&g.path) {
            continue;
        }
        let Some(rel) = garbage_rel(&id, &g.path) else {
            tracing::warn!(
                upload = id.as_str(),
                path = g.path,
                "garbage のパスが不正なので残す"
            );
            continue;
        };
        let (mtime_ns, bytes) = match inbox.stat(&rel) {
            Ok(st) if st.mtime_ns <= cutoff_ns => (st.mtime_ns, tree_bytes(inbox, &rel)),
            Ok(_) => continue,
            // 実体が無い（展開がディレクトリを作る前に止まった）項は、載せてから期限が来たら外す
            Err(FsError::NotFound) if g.added_at <= cutoff => (0, 0),
            Err(FsError::NotFound) => continue,
            Err(e) => {
                tracing::warn!(upload = id.as_str(), path = g.path, error = %e, "garbage を stat できないので残す");
                continue;
            }
        };
        out.push(PlannedUpload {
            dir: dir.as_str().to_owned(),
            reason: UploadGcReason::Garbage {
                path: g.path.clone(),
            },
            basis: basis(mtime_ns),
            bytes,
        });
    }
    // (d)
    if m.publish_requested_at.is_none() && is_stale(m, stale_secs, now) {
        out.push(PlannedUpload {
            dir: dir.as_str().to_owned(),
            reason: UploadGcReason::MarkStale,
            basis: basis(staging_mtime),
            bytes: 0,
        });
    }
}

fn is_stale(m: &Manifest, stale_secs: i64, now: i64) -> bool {
    m.updated_at.saturating_add(stale_secs) <= now
}

/// 展開中の世代のパス（`garbage` に載っていても触らない）
fn unpacking_paths(m: &Manifest) -> Vec<String> {
    m.origins
        .iter()
        .filter_map(|o| {
            let g = o.zip.as_ref()?.unpacking?;
            o.unpack_dir(g).ok().map(|p| p.as_str().to_owned())
        })
        .collect()
}

/// `garbage` の項のパス（ステージング相対）を Inbox 相対にする
fn garbage_rel(id: &UploadId, path: &str) -> Option<RelPath> {
    RelPath::parse(path).ok().and_then(|p| id.staged(&p).ok())
}

/// `rel` の下の通常ファイルの合計（symlink は辿らない。読めないところは数えない）
fn tree_bytes(root: &RootDir, rel: &RelPath) -> u64 {
    let mut total = 0;
    let mut stack = vec![rel.clone()];
    while let Some(p) = stack.pop() {
        let Ok(st) = root.stat(&p) else { continue };
        match st.kind {
            FileKind::File => total += st.size,
            FileKind::Dir => {
                let Ok(entries) = root.read_dir(Some(&p)) else {
                    continue;
                };
                for e in entries {
                    if let Some(child) = e.name.to_str().and_then(|n| p.join(n).ok()) {
                        stack.push(child);
                    }
                }
            }
            FileKind::Symlink | FileKind::Other => {}
        }
    }
    total
}

/// `rel` の下を深さ優先で消す（ファイル・symlink・その他は unlink、ディレクトリは中を消してから rmdir。
/// symlink は辿らない）。消した通常ファイルのバイト数を返す。途中で失敗したらそこで止める（残りは次の GC）
fn remove_tree(root: &RootDir, rel: &RelPath) -> Result<u64, FsError> {
    let mut bytes = 0;
    // (パス, 中を積み終えたか)
    let mut stack = vec![(rel.clone(), false)];
    while let Some((p, expanded)) = stack.pop() {
        if expanded {
            root.remove_dir(&p)?;
            continue;
        }
        let st = root.stat(&p)?;
        if st.kind != FileKind::Dir {
            root.unlink(&p)?;
            if st.kind == FileKind::File {
                bytes += st.size;
            }
            continue;
        }
        stack.push((p.clone(), true));
        for e in root.read_dir(Some(&p))? {
            let child = e
                .name
                .to_str()
                .ok_or_else(|| FsError::Io(std::io::Error::other("UTF-8 でない名前")))
                .and_then(|n| p.join(n).map_err(|e| FsError::Io(std::io::Error::other(e))))?;
            stack.push((child, false));
        }
    }
    Ok(bytes)
}

/// 1 件の結果
enum Outcome {
    Deleted(u64),
    Marked,
    Skipped(&'static str),
    Failed(String),
}

/// G: アップロードの区分を実行する。1 件ごとに鍵を取って根拠を確かめ直す。失敗はその件だけ止めて
/// 残りは次の GC に任せる
pub async fn execute_uploads(
    _db: &Db,
    roots: &GcRoots,
    locks: &UploadLocks,
    cfg: &UploadConfig,
    plan: &Plan,
    token: &CancellationToken,
) -> Result<Counts, GcError> {
    let mut counts = Counts {
        planned: plan.uploads.len(),
        ..Counts::default()
    };
    let Some(inbox) = roots.inbox.clone() else {
        counts.skipped = counts.planned;
        return Ok(counts);
    };
    let stale_secs = i64::from(cfg.stale_days).saturating_mul(DAY_SECS);
    for item in &plan.uploads {
        cancelled(token)?;
        let inbox = Arc::clone(&inbox);
        let locks = locks.clone();
        let planned = item.clone();
        let outcome =
            tokio::task::spawn_blocking(move || execute_one(&inbox, &locks, &planned, stale_secs))
                .await?;
        let reason = &item.reason;
        match outcome {
            Outcome::Deleted(bytes) => {
                counts.deleted += 1;
                counts.bytes += bytes;
                tracing::info!(
                    dir = item.dir,
                    ?reason,
                    bytes,
                    "アップロードのステージングを消した"
                );
            }
            Outcome::Marked => {
                counts.marked += 1;
                tracing::info!(dir = item.dir, "放置したアップロードを破棄扱いにした");
            }
            Outcome::Skipped(why) => {
                counts.skipped += 1;
                tracing::info!(
                    dir = item.dir,
                    ?reason,
                    why,
                    "計画の後に状態が変わったので残す"
                );
            }
            Outcome::Failed(e) => {
                counts.failed += 1;
                tracing::warn!(
                    dir = item.dir,
                    ?reason,
                    error = e,
                    "アップロードのステージングを片付けられない"
                );
            }
        }
    }
    Ok(counts)
}

fn execute_one(
    inbox: &RootDir,
    locks: &UploadLocks,
    item: &PlannedUpload,
    stale_secs: i64,
) -> Outcome {
    let Some(id) = UploadId::from_dir_name(&item.dir) else {
        return Outcome::Skipped("ステージングの名前が不正");
    };
    let basis = &item.basis;
    let result = match &item.reason {
        UploadGcReason::Discarded => with_upload_lock(locks, &id, || {
            let m = match read_manifest(inbox, &id) {
                Ok(m) => m,
                Err(UploadError::NotFound) => return Ok(Outcome::Skipped("既に無い")),
                Err(_) => return Ok(Outcome::Skipped("マニフェストが読めなくなった")),
            };
            if !same_manifest(&m, basis) {
                return Ok(Outcome::Skipped("マニフェストが変わった"));
            }
            remove_staging(inbox, &id, basis)
        }),
        UploadGcReason::Broken => with_upload_lock(locks, &id, || {
            match read_manifest(inbox, &id) {
                Ok(_) => return Ok(Outcome::Skipped("マニフェストが読めるようになった")),
                Err(UploadError::NotFound) => return Ok(Outcome::Skipped("既に無い")),
                Err(_) => {}
            }
            remove_staging(inbox, &id, basis)
        }),
        UploadGcReason::Garbage { path } => with_manifest(inbox, locks, &id, |m| {
            if !same_manifest(m, basis) {
                return Ok(Outcome::Skipped("マニフェストが変わった"));
            }
            if !m.garbage.iter().any(|g| &g.path == path) {
                return Ok(Outcome::Skipped("garbage から外れた"));
            }
            if unpacking_paths(m).contains(path) {
                return Ok(Outcome::Skipped("展開中の世代"));
            }
            let Some(rel) = garbage_rel(&id, path) else {
                return Ok(Outcome::Skipped("パスが不正"));
            };
            let bytes = match inbox.stat(&rel) {
                Ok(st) if st.mtime_ns != basis.mtime_ns => {
                    return Ok(Outcome::Skipped("計画の後に触られた"))
                }
                // 失敗したら項を残す（マニフェストは変えないので書かれない）
                Ok(_) => match remove_tree(inbox, &rel) {
                    Ok(b) => b,
                    Err(e) => return Ok(Outcome::Failed(e.to_string())),
                },
                // 実体が無い（前回の GC が消した後に書き戻せなかった・作られる前に止まった）
                Err(FsError::NotFound) => 0,
                Err(e) => return Ok(Outcome::Failed(e.to_string())),
            };
            m.garbage.retain(|g| &g.path != path);
            Ok(Outcome::Deleted(bytes))
        }),
        UploadGcReason::MarkStale => with_manifest(inbox, locks, &id, |m| {
            if m.updated_at != basis.updated_at.unwrap_or(i64::MIN)
                || m.discarded_at.is_some()
                || m.publish_requested_at.is_some()
            {
                return Ok(Outcome::Skipped("マニフェストが変わった"));
            }
            let now = now_epoch();
            if !is_stale(m, stale_secs, now) {
                return Ok(Outcome::Skipped("放置の期限前"));
            }
            m.discarded_at = Some(now);
            Ok(Outcome::Marked)
        }),
    };
    match result {
        Ok(o) => o,
        Err(UploadError::NotFound) => Outcome::Skipped("既に無い"),
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

fn same_manifest(m: &Manifest, basis: &UploadBasis) -> bool {
    Some(m.updated_at) == basis.updated_at && m.discarded_at == basis.discarded_at
}

/// 鍵の中で呼ぶ。ステージングの mtime が計画時と同じならステージングごと消す（書き戻さない）
fn remove_staging(
    inbox: &RootDir,
    id: &UploadId,
    basis: &UploadBasis,
) -> Result<Outcome, UploadError> {
    let dir = id.staging_dir();
    match inbox.stat(&dir) {
        Ok(st) if st.kind != FileKind::Dir => return Ok(Outcome::Skipped("ディレクトリでない")),
        Ok(st) if st.mtime_ns != basis.mtime_ns => {
            return Ok(Outcome::Skipped("計画の後にステージングが変わった"))
        }
        Ok(_) => {}
        Err(FsError::NotFound) => return Ok(Outcome::Skipped("既に無い")),
        Err(e) => return Ok(Outcome::Failed(e.to_string())),
    }
    // マニフェストを最後に消す: 途中で失敗しても次の GC がまだ「捨てた」ものとして続きを消せる
    // （先に消えると壊れたものに見え、mtime からまた retention を待つ）
    let removed = (|| -> Result<u64, FsError> {
        let manifest = dir
            .join(MANIFEST_NAME)
            .map_err(|e| FsError::Io(std::io::Error::other(e)))?;
        let mut bytes = 0;
        for e in inbox.read_dir(Some(&dir))? {
            if e.name.to_str() == Some(MANIFEST_NAME) {
                continue;
            }
            let child = e
                .name
                .to_str()
                .ok_or_else(|| FsError::Io(std::io::Error::other("UTF-8 でない名前")))
                .and_then(|n| {
                    dir.join(n)
                        .map_err(|e| FsError::Io(std::io::Error::other(e)))
                })?;
            bytes += remove_tree(inbox, &child)?;
        }
        match inbox.stat(&manifest) {
            Ok(_) => bytes += remove_tree(inbox, &manifest)?,
            Err(FsError::NotFound) => {}
            Err(e) => return Err(e),
        }
        inbox.remove_dir(&dir)?;
        Ok(bytes)
    })();
    Ok(match removed {
        Ok(bytes) => {
            if let Err(e) = inbox.fsync_dir(None) {
                tracing::warn!(dir = dir.as_str(), error = %e, "Inbox を fsync できない");
            }
            Outcome::Deleted(bytes)
        }
        Err(e) => Outcome::Failed(e.to_string()),
    })
}

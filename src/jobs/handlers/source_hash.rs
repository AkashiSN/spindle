//! `source_hash` ジョブ（docs/superpowers/specs/2026-09-29-device-delivery-design.md ③、D-95）。payload は
//! `{"track_id", "source"}`（source は `master` / `opus` / `aac`）、dedup `source_hash:<track_id>:<source>`。
//!
//! 送る元を root の dirfd から `openat2` で開き（`RootDir::open_file`）、開いた FD の identity（inode・size・
//! mtime・ctime。dev は見ない。D-62）を記録してから全体を読んで SHA-256 を取る。読み終えた後の `fstat` が
//! 開いた時と同じで、かつ DB の意味トークンが変わっていないときだけ `source_hashes` に保存する（途中で
//! 差し替わったら捨てて再投入）。何度実行しても結果は同じ。
//!
//! 原本（`SourceKind::Master`）だけは、読み始める前にもう一段確認する: 走査がまだ `tracks` の
//! `(inode, size, mtime_ns, ctime_ns)` を更新できていない間はハッシュを取らない。取ってしまうと
//! `db::devices::track_inputs` の事前条件（`tracks` の現在の物理同一性と一致すること）を満たせない
//! ハッシュを計算するだけ無駄になる。この場合は失敗ではなく `Outcome::Done`（走査が `tracks` を更新すれば
//! 次の差分計算で改めて投入される）。**この確認は実際にハッシュを取る FD 自身の `fstat`（開いた直後、
//! 読む前）に対して行う**。別 FD で確認してから改めて開き直すと、その間に rename で入れ替わった
//! ファイルをハッシュしてしまう恐れがある（レビュー指摘）。対象が読む前から無ければ（走査待ちの間に
//! リネーム・削除された）同じく走査待ち扱いにする（`Outcome::Done`。再試行を消費しない）

use std::fs::File;
use std::io::{self, Read as _};
use std::sync::Arc;

use rusqlite::OptionalExtension as _;
use sha2::{Digest, Sha256};

use crate::db::{derived as dbderived, devices, now_epoch};
use crate::domain::device::{semantic_derived, semantic_master, SourceHash, SourceKind};
use crate::domain::relpath::RelPath;
use crate::fsroot::{self, RootDir};
use crate::jobs::{BoxFuture, Handler, HandlerResult, JobContext, JobError, Outcome};

pub struct SourceHashHandler {
    library: Arc<RootDir>,
    derived: Arc<RootDir>,
}

impl SourceHashHandler {
    pub fn new(library: Arc<RootDir>, derived: Arc<RootDir>) -> Self {
        Self { library, derived }
    }
}

fn failed(msg: impl Into<String>) -> JobError {
    JobError::Failed(anyhow::anyhow!(msg.into()))
}

fn identity(st: &fsroot::Stat) -> (u64, u64, i64, i64) {
    (st.inode, st.size, st.mtime_ns, st.ctime_ns)
}

/// [`hash_source_inner`] の結果
#[derive(Debug, Clone, PartialEq, Eq)]
enum HashOutcome {
    /// 読み取り前後で identity が変わらず、ハッシュが取れた
    Hashed(SourceHash),
    /// 読んでいる間に identity が変わった（再試行すべき失敗）
    Changed,
    /// `expected`（`tracks` の物理同一性）と、開いた FD の `fstat` が読む前から一致しない。
    /// あるいは `expected` が指定されていて対象が無い（走査待ち。失敗ではない）
    ScanPending,
    /// Derived（`expected` が無い）の実ファイルが無い。`derived_files` の行だけが残っている drift
    SourceMissing,
}

/// 開いて読み、途中で変わっていなければハッシュを返す。変わっていれば `Ok(None)`
pub fn hash_source(
    root: &RootDir,
    rel: &RelPath,
    semantic: &str,
) -> Result<Option<SourceHash>, JobError> {
    hash_source_with_hook(root, rel, semantic, &mut || {})
}

/// [`hash_source`] の読み取りの途中（最初の塊を読んだ後）で `hook` を呼ぶ版（テスト用）
pub fn hash_source_with_hook(
    root: &RootDir,
    rel: &RelPath,
    semantic: &str,
    hook: &mut dyn FnMut(),
) -> Result<Option<SourceHash>, JobError> {
    match hash_source_inner(root, rel, semantic, None, hook)? {
        HashOutcome::Hashed(h) => Ok(Some(h)),
        HashOutcome::Changed | HashOutcome::ScanPending => Ok(None),
        HashOutcome::SourceMissing => Err(failed(format!(
            "送る元を開けない {}: 見つからない",
            rel.as_str()
        ))),
    }
}

/// [`hash_source_with_hook`] の内部実装。`expected` が `Some`（原本のみ）なら、開いた FD 自身の
/// `fstat`（読む前）を `tracks` 行の物理同一性と比較し、一致しなければ読まずに [`HashOutcome::ScanPending`]
/// を返す。対象が無い（`NotFound`）場合も `expected` があれば同じく `ScanPending`（`None` なら
/// [`HashOutcome::SourceMissing`]。公開関数はこれをエラーに写す）。`tracks` を知らない `hash_source` / `hash_source_with_hook` からは常に `expected = None` で呼ばれる
fn hash_source_inner(
    root: &RootDir,
    rel: &RelPath,
    semantic: &str,
    expected: Option<&MasterIdentity>,
    hook: &mut dyn FnMut(),
) -> Result<HashOutcome, JobError> {
    let mut file = match root.open_file(rel) {
        Ok(f) => f,
        Err(fsroot::FsError::NotFound) if expected.is_some() => {
            return Ok(HashOutcome::ScanPending);
        }
        Err(fsroot::FsError::NotFound) => return Ok(HashOutcome::SourceMissing),
        Err(e) => return Err(failed(format!("送る元を開けない {}: {e}", rel.as_str()))),
    };
    let before = fsroot::fstat(&file).map_err(|e| failed(format!("fstat に失敗: {e}")))?;
    if let Some(expected) = expected {
        if !identity_matches(&before, expected) {
            return Ok(HashOutcome::ScanPending);
        }
    }
    let sha256 = hash_file(&mut file, hook).map_err(|e| failed(format!("読み取りに失敗: {e}")))?;
    let after = fsroot::fstat(&file).map_err(|e| failed(format!("fstat に失敗: {e}")))?;
    if identity(&before) != identity(&after) {
        return Ok(HashOutcome::Changed);
    }
    Ok(HashOutcome::Hashed(SourceHash {
        semantic: semantic.to_string(),
        inode: before.inode,
        size: before.size,
        mtime_ns: before.mtime_ns,
        ctime_ns: before.ctime_ns,
        sha256,
    }))
}

fn hash_file(file: &mut File, hook: &mut dyn FnMut()) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut first = true;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        if first {
            hook();
            first = false;
        }
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// `tracks` 行から読んだ、原本のハッシュを取る前に確かめる物理同一性（D-62。dev は含まない）
#[derive(Debug, Clone, Copy)]
struct MasterIdentity {
    inode: Option<i64>,
    size: i64,
    mtime_ns: i64,
    ctime_ns: i64,
}

/// 開いた FD の `fstat` が、`tracks` 行の現在の物理同一性と一致するか。`inode` が NULL（走査未解決）
/// なら常に不一致扱い
fn identity_matches(st: &fsroot::Stat, expected: &MasterIdentity) -> bool {
    expected.inode.is_some_and(|inode| inode as u64 == st.inode)
        && expected.size as u64 == st.size
        && expected.mtime_ns == st.mtime_ns
        && expected.ctime_ns == st.ctime_ns
}

/// 送る元の現在の意味トークンと root 相対パス。原本（Master）だけは `tracks` 行の物理同一性も併せて返す
/// （読む前の事前確認に使う。dev は含まない。D-62）。対象が無ければ None
fn resolve(
    conn: &rusqlite::Connection,
    track_id: i64,
    kind: SourceKind,
) -> crate::db::Result<Option<(String, String, Option<MasterIdentity>)>> {
    match kind {
        SourceKind::Master => Ok(conn
            .query_row(
                "SELECT rel_path, audio_version, tag_version, inode, size, mtime_ns, ctime_ns
                 FROM tracks WHERE id = ?1 AND missing_since IS NULL",
                [track_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        MasterIdentity {
                            inode: r.get(3)?,
                            size: r.get(4)?,
                            mtime_ns: r.get(5)?,
                            ctime_ns: r.get(6)?,
                        },
                    ))
                },
            )
            .optional()?
            .map(|(rel, a, t, identity)| (semantic_master(a, t), rel, Some(identity)))),
        SourceKind::Derived(v) => Ok(dbderived::get(conn, track_id, v)?
            .map(|row| (semantic_derived(v, &row), row.rel_path, None))),
    }
}

/// 読んでいる間に意味トークンが変わっていなければ `hash` を保存する（変わっていたら次の差分計算で
/// 再投入されるので何もしない）。保存したら `true`
pub fn save_if_current(
    conn: &rusqlite::Connection,
    track_id: i64,
    kind: SourceKind,
    hash: &SourceHash,
    now: i64,
) -> crate::db::Result<bool> {
    match resolve(conn, track_id, kind)? {
        Some((current, _, _)) if current == hash.semantic => {
            devices::put_source_hash(conn, track_id, kind, hash, now)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

impl Handler for SourceHashHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let library = Arc::clone(&self.library);
        let derived = Arc::clone(&self.derived);
        Box::pin(async move {
            let p = &ctx.job.payload;
            let (Some(track_id), Some(kind)) = (
                p.get("track_id").and_then(|v| v.as_i64()),
                p.get("source")
                    .and_then(|v| v.as_str())
                    .and_then(SourceKind::parse),
            ) else {
                return Err(failed(format!("payload が不正: {p}")));
            };
            let Some((semantic, rel, master_identity)) =
                ctx.db().read(move |c| resolve(c, track_id, kind)).await?
            else {
                tracing::info!(
                    track_id,
                    source = kind.as_str(),
                    "送る元が無いので何もしない"
                );
                return Ok(Outcome::Done);
            };
            let rel_path =
                RelPath::parse(&rel).map_err(|e| failed(format!("パスが不正 {rel}: {e}")))?;
            // Derived の行の rel_path は `opus/…` を含む Derived root 相対、原本は Library root 相対
            let root = match kind {
                SourceKind::Master => library,
                SourceKind::Derived(_) => derived,
            };
            ctx.check_cancel().await?;
            // 原本のみ、ハッシュを取る FD 自身の fstat（開いた直後・読む前）を tracks の物理同一性と
            // 比較する（同じ FD で行うので、確認と読み取りの間に rename が入れ替わる隙が無い）
            let sem = semantic.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                hash_source_inner(&root, &rel_path, &sem, master_identity.as_ref(), &mut || {})
            })
            .await
            .map_err(|e| failed(format!("ハッシュのタスクが落ちた: {e}")))??;
            let hash = match outcome {
                HashOutcome::ScanPending => {
                    tracing::info!(track_id, source = kind.as_str(), "スキャン待ち");
                    return Ok(Outcome::Done);
                }
                HashOutcome::SourceMissing => {
                    let SourceKind::Derived(variant) = kind else {
                        return Err(failed(format!("送る元を開けない {rel}")));
                    };
                    let rel_for_tx = rel.clone();
                    let now = now_epoch();
                    // 同じ書き込みトランザクションの中で「transcode が動いていない」「行が読んだパスのまま」を
                    // 確かめてから消す
                    let result = ctx
                        .db()
                        .transaction(move |c| {
                            if dbderived::has_active_job(c, track_id, variant)? {
                                return Ok(None);
                            }
                            if !dbderived::delete_if_path(c, track_id, variant, &rel_for_tx)? {
                                return Ok(Some(Vec::new()));
                            }
                            dbderived::enqueue_if_stale(c, track_id, now).map(Some)
                        })
                        .await?;
                    return match result {
                        None => Err(failed(format!(
                            "Derived の実ファイルが無い（transcode が動いているので待つ）: {rel}"
                        ))),
                        Some(ids) => {
                            if !ids.is_empty() {
                                ctx.jobs().notify_enqueued(&ids).await;
                            }
                            tracing::warn!(
                                track_id,
                                rel,
                                "Derived の実ファイルが無いので作り直しを投入した（D-51 の drift）"
                            );
                            Ok(Outcome::Done)
                        }
                    };
                }
                HashOutcome::Changed => {
                    return Err(failed(format!(
                        "読んでいる間に {rel} が変わった（再試行する）"
                    )));
                }
                HashOutcome::Hashed(hash) => hash,
            };
            let now = now_epoch();
            // 読んでいる間に版が進んでいたら保存しない（次の差分計算で再投入される）
            ctx.db()
                .write(move |c| save_if_current(c, track_id, kind, &hash, now))
                .await?;
            Ok(Outcome::Done)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsroot::FileKind;

    fn stat(inode: u64, size: u64, mtime_ns: i64, ctime_ns: i64) -> fsroot::Stat {
        fsroot::Stat {
            kind: FileKind::File,
            dev: 1,
            inode,
            nlink: 1,
            size,
            mtime_ns,
            ctime_ns,
        }
    }

    #[test]
    fn identity_matches_when_equal() {
        let st = stat(5, 100, 10, 20);
        let expected = MasterIdentity {
            inode: Some(5),
            size: 100,
            mtime_ns: 10,
            ctime_ns: 20,
        };
        assert!(identity_matches(&st, &expected));
    }

    #[test]
    fn identity_mismatches_when_inode_null() {
        let st = stat(5, 100, 10, 20);
        let expected = MasterIdentity {
            inode: None,
            size: 100,
            mtime_ns: 10,
            ctime_ns: 20,
        };
        assert!(!identity_matches(&st, &expected));
    }

    #[test]
    fn identity_mismatches_when_size_differs() {
        let st = stat(5, 100, 10, 20);
        let expected = MasterIdentity {
            inode: Some(5),
            size: 99,
            mtime_ns: 10,
            ctime_ns: 20,
        };
        assert!(!identity_matches(&st, &expected));
    }

    #[test]
    fn identity_mismatches_when_mtime_differs() {
        let st = stat(5, 100, 10, 20);
        let expected = MasterIdentity {
            inode: Some(5),
            size: 100,
            mtime_ns: 11,
            ctime_ns: 20,
        };
        assert!(!identity_matches(&st, &expected));
    }

    #[test]
    fn identity_mismatches_when_ctime_differs() {
        let st = stat(5, 100, 10, 20);
        let expected = MasterIdentity {
            inode: Some(5),
            size: 100,
            mtime_ns: 10,
            ctime_ns: 21,
        };
        assert!(!identity_matches(&st, &expected));
    }

    /// tracks の物理同一性が今のファイルと食い違っていれば（走査未反映）、ハッシュを取らずに
    /// `ScanPending` を返す。開いた FD 自身の `fstat` に対して比較するので、確認用に別 FD を
    /// 開き直す隙が無い（レビュー指摘）
    #[test]
    fn hash_source_inner_reports_scan_pending_when_tracks_identity_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.opus"), b"hello").unwrap();
        let root = RootDir::open(dir.path()).unwrap();
        let rel = RelPath::parse("x.opus").unwrap();
        // 走査未反映を装う: 実際の inode とは違う値を tracks の期待値として渡す
        let expected = MasterIdentity {
            inode: Some(-1),
            size: 5,
            mtime_ns: 0,
            ctime_ns: 0,
        };
        let outcome = hash_source_inner(&root, &rel, "s", Some(&expected), &mut || {}).unwrap();
        assert_eq!(outcome, HashOutcome::ScanPending);
    }

    /// `tracks` が期待する識別子が対象と一致すれば、`expected` 付きでも通常どおりハッシュが取れる
    #[test]
    fn hash_source_inner_hashes_when_tracks_identity_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.opus"), b"hello").unwrap();
        let root = RootDir::open(dir.path()).unwrap();
        let rel = RelPath::parse("x.opus").unwrap();
        let st = root.stat(&rel).unwrap();
        let expected = MasterIdentity {
            inode: Some(st.inode as i64),
            size: st.size as i64,
            mtime_ns: st.mtime_ns,
            ctime_ns: st.ctime_ns,
        };
        let outcome = hash_source_inner(&root, &rel, "s", Some(&expected), &mut || {}).unwrap();
        assert!(matches!(outcome, HashOutcome::Hashed(_)));
    }
}

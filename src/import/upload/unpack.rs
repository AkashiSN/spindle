//! zip の展開（設計 §5.3）。
//!
//! - 展開は `next_generation` を進めて予約した**新しい世代** `x/<origin>/g<N>/` へ書き、成功したら
//!   マニフェストの成功した世代を切り替える。失敗した世代・古い世代は `garbage` に入れて GC に任せる
//!   （このモジュールは unlink / rmdir をしない）
//! - マニフェストの鍵（std の Mutex）は短い区間だけ持つ: 始める前の検査と世代の予約、次のブロックの
//!   予約、終わりの記録。展開そのもの（zip の読みと書き込み）は鍵の外
//! - 予算: **書く前に**鍵の中で次のブロック（[`RESERVE_BLOCK`]）を展開中の世代の `garbage` の項に
//!   足してから書く。送信と展開が同時に走っても合計で上限を守る。終わったら実際に書いた量に直す
//! - 取り消しと予算はコピーのバッファごとに見る（巨大な 1 エントリでも止まる）

use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read, Write};

use tokio_util::sync::CancellationToken;
use zip::result::ZipError as ZipLibError;
use zip::{CompressionMethod, ZipArchive};

use crate::config::UploadConfig;
use crate::db::now_epoch;
use crate::domain::relpath::{canonical_key, RelPath};
use crate::fsroot::{FsError, RootDir};

use super::budget::used_bytes;
use super::manifest::{
    with_manifest, FileClass, Garbage, Rejected, UploadError, UploadId, UploadLocks, ZipError,
    ZipState,
};
use super::receive::{sync_with_disk, validate_client_path};

/// 書く前に鍵の中で予約するブロックの大きさ（予算の残りが少なければその残りまで）
pub const RESERVE_BLOCK: u64 = 8 << 20;
/// 展開の途中で空き容量を確かめる間隔（書いたバイト数）。空きがこれを下回れば止める
pub const FREE_CHECK_BYTES: u64 = 64 << 20;
/// コピーのバッファ（取り消しと予算はこの単位で見る）
const COPY_BUF: usize = 256 << 10;
/// macOS の圧縮が付けるメタデータの置き場（展開しない）
const MACOSX_DIR: &str = "__MACOSX";
/// unix mode のファイル種別
const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;

/// [`unpack`] の結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnpackOutcome {
    /// 世代 `generation` へ展開した
    Done {
        generation: u32,
        files: usize,
        rejected: usize,
    },
    /// zip 全体の失敗（`ZipState.error` にも記録した）
    Failed { reason: String, retryable: bool },
    /// 何もしなかった: 破棄済み・未完了・外した・壊れた・既に成功（`generation > 0`）・
    /// 失敗済みで retry でない（やり直せない失敗は retry でも）
    Skipped,
}

/// 途中の位置（テスト用の差し込み。本番は `&|_| false`）。`true` を返すとその場で
/// 止まったことにする（マニフェストを片付けずに Err で返る = 再起動で止まった模擬）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnpackStep {
    /// 次のブロックを予約した後・書く前
    AfterReserve,
    /// バッファ 1 つを書いた後（`written` はこの世代で書いた合計）
    Wrote { written: u64 },
}

/// zip の origin を新しい世代へ展開する（同期関数。ジョブから spawn_blocking で呼ぶ）。
///
/// `retry` はやり直せる失敗（空き不足・I/O・取り消し）の後に「展開し直す」ときだけ true
pub fn unpack(
    inbox: &RootDir,
    locks: &UploadLocks,
    id: &UploadId,
    origin: &str,
    retry: bool,
    cfg: &UploadConfig,
    cancel: &CancellationToken,
) -> Result<UnpackOutcome, UploadError> {
    unpack_with(inbox, locks, id, origin, retry, cfg, cancel, &|_| false)
}

#[allow(clippy::too_many_arguments)]
fn unpack_with(
    inbox: &RootDir,
    locks: &UploadLocks,
    id: &UploadId,
    origin: &str,
    retry: bool,
    cfg: &UploadConfig,
    cancel: &CancellationToken,
    hook: &dyn Fn(UnpackStep) -> bool,
) -> Result<UnpackOutcome, UploadError> {
    let Some(started) = begin(inbox, locks, id, origin, retry)? else {
        return Ok(UnpackOutcome::Skipped);
    };
    let mut run = Run {
        inbox,
        locks,
        id,
        cfg,
        cancel,
        hook,
        garbage_path: started.garbage_path.clone(),
        reserved: 0,
        written: 0,
        next_free_check: 0,
    };
    let result = match run.extract(&started) {
        Ok(stats) => Ok(stats),
        Err(Stop::Fail { reason, retryable }) => Err((reason, retryable)),
        // 止まった模擬: マニフェストは予約したまま（再実行が garbage に残して新しい世代で始める）
        Err(Stop::Abort(e)) => return Err(e),
    };
    finish(inbox, locks, id, origin, &started, run.written, result)
}

/// 予約した世代
struct Started {
    generation: u32,
    /// Inbox 相対の zip 本体
    zip: RelPath,
    /// Inbox 相対の展開先（`.spindle-upload-<id>/x/<origin>/g<N>`）
    dest: RelPath,
    /// ステージング内の展開先（`x/<origin>/g<N>`。`garbage` の項のパス）
    garbage_path: String,
}

/// 鍵の中で始める前の検査をし、新しい世代を予約する。何もしないなら `None`
fn begin(
    inbox: &RootDir,
    locks: &UploadLocks,
    id: &UploadId,
    origin: &str,
    retry: bool,
) -> Result<Option<Started>, UploadError> {
    with_manifest(inbox, locks, id, |m| {
        if m.discarded_at.is_some() {
            return Ok(None);
        }
        // rename の後・マニフェストを書く前に落ちた zip を完了に進める
        sync_with_disk(inbox, m)?;
        let now = now_epoch();
        let o = m
            .origins
            .iter_mut()
            .find(|o| o.id == origin)
            .ok_or(UploadError::OriginNotFound)?;
        if o.class != FileClass::Zip {
            return Err(UploadError::Conflict(format!("{origin} は zip でない")));
        }
        if !o.complete || o.removed || o.damaged {
            return Ok(None);
        }
        let zip_rel = id.staged(&o.zip_path()?)?;
        let z = o.zip.get_or_insert_with(ZipState::default);
        if z.generation > 0 {
            return Ok(None);
        }
        if let Some(e) = &z.error {
            if !(retry && e.retryable) {
                return Ok(None);
            }
            z.error = None;
        }
        // 前の実行が展開中のまま止まった世代は再開できない。garbage に残したまま捨てる
        let stale = z.unpacking.take();
        let generation = z.next_generation.max(1);
        z.next_generation = generation
            .checked_add(1)
            .ok_or_else(|| UploadError::Limit("展開の世代の番号".into()))?;
        z.unpacking = Some(generation);
        z.rejected.clear();

        if let Some(s) = stale {
            let path = o.unpack_dir(s)?.as_str().to_string();
            if !m.garbage.iter().any(|g| g.path == path) {
                // 予約と同じ書き込みで載せるので普通は起きない
                tracing::warn!(
                    upload = id.as_str(),
                    path = path.as_str(),
                    "止まった展開の世代が garbage に無いので載せ直す"
                );
                m.garbage.push(Garbage {
                    path,
                    bytes: 0,
                    added_at: now,
                });
            }
        }
        let dir = o.unpack_dir(generation)?;
        let garbage_path = dir.as_str().to_string();
        // 先に garbage に載せる: 失敗・停止しても GC が拾い、予約したバイトは予算に数える
        m.garbage.push(Garbage {
            path: garbage_path.clone(),
            bytes: 0,
            added_at: now,
        });
        Ok(Some(Started {
            generation,
            zip: zip_rel,
            dest: id.staged(&dir)?,
            garbage_path,
        }))
    })
}

/// 鍵の中で結果を記録する
fn finish(
    inbox: &RootDir,
    locks: &UploadLocks,
    id: &UploadId,
    origin: &str,
    started: &Started,
    written: u64,
    result: Result<Stats, (String, bool)>,
) -> Result<UnpackOutcome, UploadError> {
    let generation = started.generation;
    let gpath = started.garbage_path.as_str();
    with_manifest(inbox, locks, id, |m| {
        let now = now_epoch();
        let o = m
            .origins
            .iter_mut()
            .find(|o| o.id == origin)
            .ok_or(UploadError::OriginNotFound)?;
        let z = o.zip.get_or_insert_with(ZipState::default);
        if z.unpacking != Some(generation) {
            // 別の実行がこの世代を止まったものとして捨てた。予算の項を実際の量に直すだけ
            if let Some(g) = m.garbage.iter_mut().find(|g| g.path == gpath) {
                g.bytes = written;
            }
            return Ok(UnpackOutcome::Skipped);
        }
        z.unpacking = None;
        match result {
            Ok(stats) => {
                let prev = (z.generation, z.written_bytes);
                let files = stats.files;
                let rejected = stats.rejected.len();
                z.generation = generation;
                z.written_bytes = written;
                z.rejected = stats.rejected;
                z.error = None;
                m.garbage.retain(|g| g.path != gpath);
                if prev.0 > 0 {
                    m.garbage.push(Garbage {
                        path: o.unpack_dir(prev.0)?.as_str().to_string(),
                        bytes: prev.1,
                        added_at: now,
                    });
                }
                tracing::info!(
                    upload = id.as_str(),
                    origin,
                    generation,
                    files,
                    rejected,
                    written,
                    "zip を展開した"
                );
                Ok(UnpackOutcome::Done {
                    generation,
                    files,
                    rejected,
                })
            }
            Err((reason, retryable)) => {
                z.error = Some(ZipError {
                    reason: reason.clone(),
                    retryable,
                });
                if let Some(g) = m.garbage.iter_mut().find(|g| g.path == gpath) {
                    g.bytes = written;
                }
                tracing::warn!(
                    upload = id.as_str(),
                    origin,
                    generation,
                    retryable,
                    reason = reason.as_str(),
                    "zip の展開に失敗した"
                );
                Ok(UnpackOutcome::Failed { reason, retryable })
            }
        }
    })
}

/// 展開の結果
struct Stats {
    files: usize,
    rejected: Vec<Rejected>,
}

/// 展開を止める理由
enum Stop {
    /// zip 全体の失敗
    Fail { reason: String, retryable: bool },
    /// 止まった模擬（テストの差し込み）
    Abort(UploadError),
}

fn permanent(reason: impl Into<String>) -> Stop {
    Stop::Fail {
        reason: reason.into(),
        retryable: false,
    }
}

fn retryable(reason: impl Into<String>) -> Stop {
    Stop::Fail {
        reason: reason.into(),
        retryable: true,
    }
}

/// zip の読みのエラー。OS のエラー（EIO など）はやり直せる、それ以外（展開器・CRC・申告サイズ
/// 超過などの中身の不整合）は壊れた zip としてやり直せない
fn read_fail(e: std::io::Error) -> Stop {
    if e.raw_os_error().is_some() {
        retryable(format!("zip を読めない: {e}"))
    } else {
        permanent(format!("zip が壊れている: {e}"))
    }
}

fn zip_fail(e: ZipLibError) -> Stop {
    match e {
        ZipLibError::Io(io) => read_fail(io),
        ZipLibError::UnsupportedArchive(ZipLibError::PASSWORD_REQUIRED)
        | ZipLibError::InvalidPassword => permanent("暗号化された zip は展開できない"),
        ZipLibError::CompressionMethodNotSupported(m) => {
            permanent(format!("対応しない圧縮方式（{m}）"))
        }
        other => permanent(format!("zip が壊れている: {other}")),
    }
}

/// 書き込みのエラー（すべてやり直せる）
fn write_fail(e: std::io::Error) -> Stop {
    if e.kind() == ErrorKind::StorageFull {
        retryable("空き容量が足りない")
    } else {
        retryable(format!("書き込めない: {e}"))
    }
}

fn fs_fail(e: FsError) -> Stop {
    retryable(format!("書き込めない: {e}"))
}

/// 1 回の展開の進み具合
struct Run<'a> {
    inbox: &'a RootDir,
    locks: &'a UploadLocks,
    id: &'a UploadId,
    cfg: &'a UploadConfig,
    cancel: &'a CancellationToken,
    hook: &'a dyn Fn(UnpackStep) -> bool,
    garbage_path: String,
    /// 展開中の世代の `garbage` の項に足した量
    reserved: u64,
    /// 実際に書いた量
    written: u64,
    /// 次に空き容量を確かめる `written`
    next_free_check: u64,
}

/// 先に出てきたパス（canonical key ごと）
struct Seen {
    dir: bool,
    /// 最初の綴り（大小文字だけ違う親で別のディレクトリを作らない）
    spelling: String,
}

impl Run<'_> {
    fn extract(&mut self, started: &Started) -> Result<Stats, Stop> {
        let file = match self.inbox.open_file(&started.zip) {
            Ok(f) => f,
            Err(FsError::NotFound) => return Err(permanent("zip の本体が無い")),
            Err(e) => return Err(retryable(format!("zip を開けない: {e}"))),
        };
        let mut archive = ZipArchive::new(file).map_err(|e| match e {
            ZipLibError::Io(io) if io.raw_os_error().is_some() => {
                retryable(format!("zip を読めない: {io}"))
            }
            other => permanent(format!("zip の中央ディレクトリが読めない: {other}")),
        })?;
        let count = archive.len();
        if u64::try_from(count).map_or(true, |n| n > self.cfg.max_zip_entries) {
            return Err(permanent(format!(
                "zip のエントリ数 {count} が上限 {} を超える",
                self.cfg.max_zip_entries
            )));
        }
        // 暗号化・対応外の圧縮方式は書き始める前に zip 全体で断る
        for i in 0..count {
            let e = archive.by_index_data(i).map_err(zip_fail)?;
            if e.encrypted() {
                return Err(permanent("暗号化された zip は展開できない"));
            }
            match e.compression() {
                CompressionMethod::Stored | CompressionMethod::Deflated => {}
                other => return Err(permanent(format!("対応しない圧縮方式（{other:?}）"))),
            }
        }

        self.inbox.create_dir_all(&started.dest).map_err(fs_fail)?;
        let mut made: HashSet<RelPath> = HashSet::new();
        made.insert(started.dest.clone());
        let mut seen: HashMap<String, Seen> = HashMap::new();
        let mut rejected = Vec::new();
        let mut files = 0usize;
        let mut buf = vec![0u8; COPY_BUF];

        for i in 0..count {
            self.check_cancel()?;
            let (raw, crate_dir, mode, crate_symlink) = {
                let e = archive.by_index_data(i).map_err(zip_fail)?;
                (
                    e.name_raw().to_vec(),
                    e.is_dir(),
                    e.unix_mode(),
                    e.is_symlink(),
                )
            };
            let name = match String::from_utf8(raw) {
                Ok(s) => s,
                Err(e) => {
                    rejected.push(Rejected {
                        entry: String::from_utf8_lossy(e.as_bytes()).into_owned(),
                        reason: "名前が UTF-8 として読めない".into(),
                    });
                    continue;
                }
            };
            let mut reject = |reason: String| {
                rejected.push(Rejected {
                    entry: name.clone(),
                    reason,
                });
            };
            let kind = mode.map_or(0, |m| m & S_IFMT);
            let trimmed = name.strip_suffix('/').unwrap_or(&name);
            if trimmed.split('/').next() == Some(MACOSX_DIR) {
                continue;
            }
            let rel = match validate_client_path(trimmed) {
                Ok(r) => r,
                Err(e) => {
                    reject(e.to_string());
                    continue;
                }
            };
            if crate_symlink || kind == S_IFLNK {
                reject("symlink は展開しない".into());
                continue;
            }
            if kind != 0 && kind != S_IFREG && kind != S_IFDIR {
                reject("特殊ファイルは展開しない".into());
                continue;
            }
            let is_dir = crate_dir || name.ends_with('/') || kind == S_IFDIR;
            if let Err(why) = claim(&mut seen, &rel, is_dir) {
                reject(why.into());
                continue;
            }

            let dest = under(&started.dest, &rel).map_err(|e| retryable(e.to_string()))?;
            if is_dir {
                self.make_dir(&mut made, dest)?;
                continue;
            }
            if let Some(parent) = dest.parent() {
                self.make_dir(&mut made, parent)?;
            }
            let mut out = self.inbox.create_truncate(&dest).map_err(fs_fail)?;
            let mut entry = archive.by_index(i).map_err(zip_fail)?;
            loop {
                self.check_cancel()?;
                let n = match entry.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => return Err(read_fail(e)),
                };
                let len = n as u64;
                self.reserve(len)?;
                self.check_free()?;
                out.write_all(&buf[..n]).map_err(write_fail)?;
                self.written = self.written.saturating_add(len);
                if (self.hook)(UnpackStep::Wrote {
                    written: self.written,
                }) {
                    return Err(Stop::Abort(UploadError::Io(std::io::Error::other("fault"))));
                }
            }
            out.sync_all().map_err(write_fail)?;
            files += 1;
        }

        // 作ったディレクトリのエントリを永続化する（展開先の親 `x/<origin>` も）
        if let Some(parent) = started.dest.parent() {
            made.insert(parent);
        }
        for dir in &made {
            self.inbox.fsync_dir(Some(dir)).map_err(fs_fail)?;
        }
        Ok(Stats { files, rejected })
    }

    fn make_dir(&self, made: &mut HashSet<RelPath>, dir: RelPath) -> Result<(), Stop> {
        if made.contains(&dir) {
            return Ok(());
        }
        self.inbox.create_dir_all(&dir).map_err(fs_fail)?;
        made.insert(dir);
        Ok(())
    }

    fn check_cancel(&self) -> Result<(), Stop> {
        if self.cancel.is_cancelled() {
            return Err(retryable("取り消した"));
        }
        Ok(())
    }

    /// 空き容量を [`FREE_CHECK_BYTES`] ごとに確かめる
    fn check_free(&mut self) -> Result<(), Stop> {
        if self.written < self.next_free_check {
            return Ok(());
        }
        let free = self.inbox.free_bytes().map_err(fs_fail)?;
        if free < FREE_CHECK_BYTES {
            return Err(retryable("空き容量が足りない"));
        }
        self.next_free_check = self.written.saturating_add(FREE_CHECK_BYTES);
        Ok(())
    }

    /// `len` バイトを書く前に、足りなければ鍵の中で次のブロックを予約する
    fn reserve(&mut self, len: u64) -> Result<(), Stop> {
        let need = self.written.saturating_add(len);
        if need <= self.reserved {
            return Ok(());
        }
        let short = need - self.reserved;
        let gpath = self.garbage_path.as_str();
        let max = self.cfg.max_upload_bytes;
        let got = with_manifest(self.inbox, self.locks, self.id, |m| {
            let room = max.saturating_sub(used_bytes(m));
            if room < short {
                return Err(UploadError::Limit(format!(
                    "アップロード全体の上限 {max} バイトを超える"
                )));
            }
            // 1 ブロックぶん先に取る。残りが少なければその残りまで（予約しすぎで断らない）
            let add = RESERVE_BLOCK.max(short).min(room);
            let g = m
                .garbage
                .iter_mut()
                .find(|g| g.path == gpath)
                .ok_or_else(|| UploadError::Conflict("展開中の世代が garbage に無い".into()))?;
            g.bytes = g.bytes.saturating_add(add);
            Ok(add)
        });
        match got {
            Ok(add) => self.reserved = self.reserved.saturating_add(add),
            Err(UploadError::Limit(why)) => return Err(permanent(why)),
            Err(e) => return Err(retryable(e.to_string())),
        }
        if (self.hook)(UnpackStep::AfterReserve) {
            return Err(Stop::Abort(UploadError::Io(std::io::Error::other("fault"))));
        }
        Ok(())
    }
}

/// `base` の下に `rel` の要素を 1 つずつ足す
fn under(base: &RelPath, rel: &RelPath) -> Result<RelPath, UploadError> {
    let mut p = base.clone();
    for c in rel.components() {
        p = p.join(c)?;
    }
    Ok(p)
}

/// エントリのパスを先のエントリと照らし、衝突しなければ記録する。衝突なら拒否の理由
fn claim(
    seen: &mut HashMap<String, Seen>,
    rel: &RelPath,
    is_dir: bool,
) -> Result<(), &'static str> {
    let comps: Vec<&str> = rel.components().collect();
    let mut prefix = String::new();
    let mut new = Vec::new();
    for (i, c) in comps.iter().enumerate() {
        if i > 0 {
            prefix.push('/');
        }
        prefix.push_str(c);
        let last = i + 1 == comps.len();
        let want_dir = !last || is_dir;
        let key = canonical_key(&prefix);
        match seen.get(&key) {
            None => new.push((key, prefix.clone(), want_dir)),
            Some(s) if !last => {
                if !s.dir {
                    return Err("親が先のエントリでファイルになっている");
                }
                if s.spelling != prefix {
                    return Err("大小文字などだけ違う名前が先にある");
                }
            }
            Some(s) => {
                if s.dir != want_dir {
                    return Err("ファイルとディレクトリが衝突する");
                }
                // 同じ綴りのディレクトリの重複は受ける
                if !want_dir {
                    return Err("同じ名前（大小文字などの違いを含む）が先にある");
                }
                if s.spelling != prefix {
                    return Err("大小文字などだけ違う名前が先にある");
                }
            }
        }
    }
    for (key, spelling, dir) in new {
        seen.insert(key, Seen { dir, spelling });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::upload::manifest::{create_upload, read_manifest, Manifest, Origin};
    use crate::import::upload::receive::{begin_origin, receive_chunk};
    use std::cell::{Cell, RefCell};
    use std::io::Cursor;
    use std::path::{Path, PathBuf};
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    const MIB: u64 = 1 << 20;

    struct Env {
        dir: tempfile::TempDir,
        inbox: RootDir,
        locks: UploadLocks,
        id: UploadId,
    }

    impl Env {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let inbox = RootDir::open(dir.path()).unwrap();
            let m = create_upload(&inbox, 1).unwrap();
            let id = UploadId::parse(&m.id).unwrap();
            Self {
                dir,
                inbox,
                locks: UploadLocks::default(),
                id,
            }
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.dir
                .path()
                .join(self.id.staging_dir().as_str())
                .join(rel)
        }

        /// zip を送り終えた origin にする
        fn send(&self, name: &str, bytes: &[u8], cfg: &UploadConfig) -> String {
            let o = begin_origin(
                &self.inbox,
                &self.locks,
                &self.id,
                name,
                bytes.len() as u64,
                cfg,
                1,
            )
            .unwrap();
            let r = receive_chunk(
                &self.inbox,
                &self.locks,
                &self.id,
                &o.id,
                0,
                bytes,
                1,
                &|_| false,
            )
            .unwrap();
            assert!(r.complete);
            o.id
        }

        fn run(
            &self,
            origin: &str,
            retry: bool,
            cfg: &UploadConfig,
        ) -> Result<UnpackOutcome, UploadError> {
            unpack(
                &self.inbox,
                &self.locks,
                &self.id,
                origin,
                retry,
                cfg,
                &CancellationToken::new(),
            )
        }

        fn run_with(
            &self,
            origin: &str,
            retry: bool,
            cfg: &UploadConfig,
            cancel: &CancellationToken,
            hook: &dyn Fn(UnpackStep) -> bool,
        ) -> Result<UnpackOutcome, UploadError> {
            unpack_with(
                &self.inbox,
                &self.locks,
                &self.id,
                origin,
                retry,
                cfg,
                cancel,
                hook,
            )
        }

        fn manifest(&self) -> Manifest {
            read_manifest(&self.inbox, &self.id).unwrap()
        }

        fn origin(&self, oid: &str) -> Origin {
            self.manifest()
                .origins
                .into_iter()
                .find(|o| o.id == oid)
                .unwrap()
        }

        fn zip(&self, oid: &str) -> ZipState {
            self.origin(oid).zip.unwrap()
        }

        fn garbage(&self, path: &str) -> Option<Garbage> {
            self.manifest().garbage.into_iter().find(|g| g.path == path)
        }
    }

    enum E<'a> {
        File(&'a str, Vec<u8>),
        Dir(&'a str),
        Symlink(&'a str, &'a str),
    }

    fn build(entries: &[E]) -> Vec<u8> {
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for e in entries {
            match e {
                E::File(name, data) => {
                    w.start_file(*name, opts).unwrap();
                    w.write_all(data).unwrap();
                }
                E::Dir(name) => w.add_directory(*name, opts).unwrap(),
                E::Symlink(name, target) => w.add_symlink(*name, *target, opts).unwrap(),
            }
        }
        w.finish().unwrap().into_inner()
    }

    fn zeros(n: u64) -> Vec<u8> {
        vec![0u8; n as usize]
    }

    /// `sig` で始まるヘッダの位置をすべて返す
    fn headers(bytes: &[u8], sig: &[u8; 4]) -> Vec<usize> {
        bytes
            .windows(4)
            .enumerate()
            .filter(|(_, w)| w == sig)
            .map(|(i, _)| i)
            .collect()
    }
    const LOCAL: &[u8; 4] = b"PK\x03\x04";
    const CENTRAL: &[u8; 4] = b"PK\x01\x02";

    fn cfg_max(max: u64) -> UploadConfig {
        UploadConfig {
            max_upload_bytes: max,
            ..UploadConfig::default()
        }
    }

    fn files_under(root: &Path) -> Vec<String> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let e = e.unwrap();
                let p = e.path();
                let ft = std::fs::symlink_metadata(&p).unwrap().file_type();
                if ft.is_dir() {
                    walk(base, &p, out);
                } else {
                    out.push(p.strip_prefix(base).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        let mut out = Vec::new();
        if root.exists() {
            walk(root, root, &mut out);
        }
        out.sort();
        out
    }

    fn disk_size(root: &Path) -> u64 {
        files_under(root)
            .iter()
            .map(|f| std::fs::metadata(root.join(f)).unwrap().len())
            .sum()
    }

    #[test]
    fn unpacks_into_new_generation() {
        let env = Env::new();
        let cfg = UploadConfig::default();
        let zip = build(&[
            E::Dir("Album/"),
            E::File("Album/01.flac", b"abc".to_vec()),
            E::File("Album/cover.jpg", b"jpeg!".to_vec()),
        ]);
        let o = env.send("mora.zip", &zip, &cfg);
        let out = env.run(&o, false, &cfg).unwrap();
        assert_eq!(
            out,
            UnpackOutcome::Done {
                generation: 1,
                files: 2,
                rejected: 0
            }
        );
        assert_eq!(
            std::fs::read(env.path("x/o1/g1/Album/01.flac")).unwrap(),
            b"abc"
        );
        assert_eq!(
            files_under(&env.path("x/o1/g1")),
            vec!["Album/01.flac", "Album/cover.jpg"]
        );
        let z = env.zip(&o);
        assert_eq!(z.generation, 1);
        assert_eq!(z.next_generation, 2);
        assert_eq!(z.written_bytes, 8);
        assert_eq!(z.unpacking, None);
        assert!(z.error.is_none());
        assert!(z.rejected.is_empty());
        assert!(env.manifest().garbage.is_empty());
    }

    #[test]
    fn rerun_after_success_is_skipped() {
        let env = Env::new();
        let cfg = UploadConfig::default();
        let o = env.send("a.zip", &build(&[E::File("a.flac", b"x".to_vec())]), &cfg);
        assert!(matches!(
            env.run(&o, false, &cfg).unwrap(),
            UnpackOutcome::Done { .. }
        ));
        let before = env.manifest();
        assert_eq!(env.run(&o, false, &cfg).unwrap(), UnpackOutcome::Skipped);
        assert_eq!(env.run(&o, true, &cfg).unwrap(), UnpackOutcome::Skipped);
        assert_eq!(env.manifest(), before);
        assert!(!env.path("x/o1/g2").exists());
    }

    #[test]
    fn retry_after_retryable_failure_uses_next_generation() {
        let env = Env::new();
        let cfg = UploadConfig::default();
        let o = env.send("a.zip", &build(&[E::File("a.flac", b"xyz".to_vec())]), &cfg);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let out = env.run_with(&o, false, &cfg, &cancel, &|_| false).unwrap();
        assert!(matches!(
            out,
            UnpackOutcome::Failed {
                retryable: true,
                ..
            }
        ));
        let z = env.zip(&o);
        assert_eq!(z.generation, 0);
        assert_eq!(z.unpacking, None);
        assert!(z.error.as_ref().unwrap().retryable);
        assert!(env.garbage("x/o1/g1").is_some());

        // retry でなければ何もしない
        assert_eq!(env.run(&o, false, &cfg).unwrap(), UnpackOutcome::Skipped);
        // 展開し直す: 失敗した g1 と衝突せず g2
        let out = env.run(&o, true, &cfg).unwrap();
        assert_eq!(
            out,
            UnpackOutcome::Done {
                generation: 2,
                files: 1,
                rejected: 0
            }
        );
        assert_eq!(std::fs::read(env.path("x/o1/g2/a.flac")).unwrap(), b"xyz");
        assert!(env.path("x/o1/g1").is_dir(), "失敗した世代は消さない");
        assert!(env.garbage("x/o1/g1").is_some());
        assert!(env.garbage("x/o1/g2").is_none());
        let z = env.zip(&o);
        assert!(z.error.is_none());
        assert_eq!(z.next_generation, 3);
    }

    fn encrypted_zip() -> Vec<u8> {
        let mut zip = build(&[E::File("a.flac", b"secret".to_vec())]);
        for at in headers(&zip, LOCAL) {
            zip[at + 6] |= 1;
        }
        for at in headers(&zip, CENTRAL) {
            zip[at + 8] |= 1;
        }
        zip
    }

    #[test]
    fn permanent_failure_not_retried() {
        let env = Env::new();
        let cfg = UploadConfig::default();
        let o = env.send("a.zip", &encrypted_zip(), &cfg);
        let out = env.run(&o, false, &cfg).unwrap();
        assert!(matches!(
            out,
            UnpackOutcome::Failed {
                retryable: false,
                ..
            }
        ));
        let before = env.manifest();
        assert_eq!(env.run(&o, true, &cfg).unwrap(), UnpackOutcome::Skipped);
        assert_eq!(env.manifest(), before);
        assert!(!env.zip(&o).error.unwrap().retryable);
    }

    #[test]
    fn repeated_failures_count_garbage_in_budget() {
        let env = Env::new();
        let zip = build(&[E::File("a.flac", zeros(10 * MIB))]);
        let cfg = cfg_max(zip.len() as u64 + 12 * MIB);
        let o = env.send("a.zip", &zip, &cfg);

        // 1 回目: 5 MiB 書いたところで取り消す（やり直せる失敗）
        let cancel = CancellationToken::new();
        let hook = |s: UnpackStep| {
            if let UnpackStep::Wrote { written } = s {
                if written >= 5 * MIB {
                    cancel.cancel();
                }
            }
            false
        };
        let out = env.run_with(&o, false, &cfg, &cancel, &hook).unwrap();
        assert!(matches!(
            out,
            UnpackOutcome::Failed {
                retryable: true,
                ..
            }
        ));
        let g1 = env.garbage("x/o1/g1").unwrap();
        assert!(g1.bytes >= 5 * MIB);
        assert_eq!(g1.bytes, disk_size(&env.path("x/o1/g1")));

        // 2 回目: g1 が予算に残るので 10 MiB は入らない（数えなければ 10 <= 12 で通ってしまう）
        let out = env.run(&o, true, &cfg).unwrap();
        assert!(
            matches!(
                out,
                UnpackOutcome::Failed {
                    retryable: false,
                    ..
                }
            ),
            "{out:?}"
        );
        let m = env.manifest();
        assert!(used_bytes(&m) <= cfg.max_upload_bytes);
        let g2 = env.garbage("x/o1/g2").unwrap();
        assert_eq!(g2.bytes, disk_size(&env.path("x/o1/g2")));
        assert!(env.garbage("x/o1/g1").is_some());
    }

    #[test]
    fn budget_holds_with_concurrent_put() {
        let env = Env::new();
        let zip = build(&[E::File("a.flac", zeros(12 * MIB))]);
        let max = zip.len() as u64 + 16 * MIB;
        let cfg = cfg_max(max);
        let o = env.send("a.zip", &zip, &cfg);

        let fired = Cell::new(false);
        let results = RefCell::new(Vec::new());
        let hook = |s: UnpackStep| {
            if s == UnpackStep::AfterReserve && !fired.replace(true) {
                // 予約の後・書く前に送信が割り込む
                for (name, total) in [("big.flac", 9 * MIB), ("small.flac", 6 * MIB)] {
                    let r = begin_origin(&env.inbox, &env.locks, &env.id, name, total, &cfg, 1);
                    results.borrow_mut().push(r.map(|_| ()));
                    let m = read_manifest(&env.inbox, &env.id).unwrap();
                    assert!(used_bytes(&m) <= max);
                }
            }
            false
        };
        let out = env
            .run_with(&o, false, &cfg, &CancellationToken::new(), &hook)
            .unwrap();
        let results = results.into_inner();
        assert!(matches!(results[0], Err(UploadError::Limit(_))));
        assert!(results[1].is_ok());
        assert!(
            matches!(
                out,
                UnpackOutcome::Failed {
                    retryable: false,
                    ..
                }
            ),
            "{out:?}"
        );
        let m = env.manifest();
        assert!(used_bytes(&m) <= max);
        assert!(disk_size(&env.path("x/o1/g1")) <= 10 * MIB);
    }

    #[test]
    fn budget_not_double_counted() {
        let env = Env::new();
        let cfg = cfg_max(32 * MIB);
        let zip = build(&[E::File("a.flac", zeros(30 * MIB))]);
        assert!(zip.len() < MIB as usize);
        let o = env.send("a.zip", &zip, &cfg);
        let out = env.run(&o, false, &cfg).unwrap();
        assert!(matches!(out, UnpackOutcome::Done { .. }), "{out:?}");
        let m = env.manifest();
        assert_eq!(used_bytes(&m), zip.len() as u64 + 30 * MIB);
        assert!(m.garbage.is_empty());
    }

    #[test]
    fn stale_unpacking_restarts_in_new_generation() {
        let env = Env::new();
        let cfg = UploadConfig::default();
        let data: Vec<u8> = (0..3 * MIB).map(|i| (i % 251) as u8).collect();
        let o = env.send("a.zip", &build(&[E::File("a.flac", data.clone())]), &cfg);
        // エントリの途中で止まる（再起動の模擬）
        let r = env.run_with(&o, false, &cfg, &CancellationToken::new(), &|s| {
            matches!(s, UnpackStep::Wrote { .. })
        });
        assert!(r.is_err());
        let z = env.zip(&o);
        assert_eq!(z.unpacking, Some(1));
        let g1 = env.garbage("x/o1/g1").unwrap();
        assert!(g1.bytes > 0);

        let out = env.run(&o, false, &cfg).unwrap();
        assert_eq!(
            out,
            UnpackOutcome::Done {
                generation: 2,
                files: 1,
                rejected: 0
            }
        );
        assert_eq!(std::fs::read(env.path("x/o1/g2/a.flac")).unwrap(), data);
        let z = env.zip(&o);
        assert_eq!(z.unpacking, None);
        assert_eq!(z.generation, 2);
        assert_eq!(env.garbage("x/o1/g1").unwrap().bytes, g1.bytes);
        assert!(env.path("x/o1/g1").is_dir());
    }

    #[test]
    fn cancel_inside_large_entry() {
        let env = Env::new();
        let cfg = UploadConfig::default();
        let o = env.send("a.zip", &build(&[E::File("a.flac", zeros(20 * MIB))]), &cfg);
        let cancel = CancellationToken::new();
        let hook = |s: UnpackStep| {
            if matches!(s, UnpackStep::Wrote { written } if written >= MIB) {
                cancel.cancel();
            }
            false
        };
        let out = env.run_with(&o, false, &cfg, &cancel, &hook).unwrap();
        assert!(matches!(
            out,
            UnpackOutcome::Failed {
                retryable: true,
                ..
            }
        ));
        let size = disk_size(&env.path("x/o1/g1"));
        assert!((MIB..20 * MIB).contains(&size), "{size}");
        assert_eq!(env.garbage("x/o1/g1").unwrap().bytes, size);
        assert_eq!(env.zip(&o).unpacking, None);
    }

    #[test]
    fn non_utf8_name_rejected() {
        let env = Env::new();
        let cfg = UploadConfig::default();
        let mut zip = build(&[
            E::File("aXb.flac", b"1".to_vec()),
            E::File("ok.flac", b"2".to_vec()),
        ]);
        let pat = b"aXb.flac";
        let hits: Vec<usize> = zip
            .windows(pat.len())
            .enumerate()
            .filter(|(_, w)| w == pat)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(hits.len(), 2);
        for at in hits {
            zip[at + 1] = 0xff;
        }
        let o = env.send("a.zip", &zip, &cfg);
        let out = env.run(&o, false, &cfg).unwrap();
        assert_eq!(
            out,
            UnpackOutcome::Done {
                generation: 1,
                files: 1,
                rejected: 1
            }
        );
        let z = env.zip(&o);
        assert!(z.rejected[0].reason.contains("UTF-8"));
        assert_eq!(files_under(&env.path("x/o1/g1")), vec!["ok.flac"]);
    }

    #[test]
    fn rejects_entries_and_continues() {
        let env = Env::new();
        let cfg = UploadConfig::default();
        let zip = build(&[
            E::File("../a.flac", b"1".to_vec()),
            E::File("/abs.flac", b"1".to_vec()),
            E::File("A\\b.flac", b"1".to_vec()),
            E::Symlink("link.flac", "/etc/passwd"),
            E::File("Album/01.flac", b"ok".to_vec()),
            E::File("album/01.flac", b"case".to_vec()),
            E::File("Album/01.FLAC", b"case".to_vec()),
            E::File("X", b"file".to_vec()),
            E::File("X/y.flac", b"under file".to_vec()),
            E::File("Y/z.flac", b"ok".to_vec()),
            E::File("Y", b"file over dir".to_vec()),
            E::File("ok.flac", b"ok".to_vec()),
        ]);
        let o = env.send("a.zip", &zip, &cfg);
        let out = env.run(&o, false, &cfg).unwrap();
        assert_eq!(
            out,
            UnpackOutcome::Done {
                generation: 1,
                files: 4,
                rejected: 8
            }
        );
        let all = env.zip(&o).rejected;
        let link = all.iter().find(|r| r.entry == "link.flac").unwrap();
        assert!(link.reason.contains("symlink"), "{}", link.reason);
        let mut rejected: Vec<String> = all.into_iter().map(|r| r.entry).collect();
        rejected.sort();
        let mut want = vec![
            "../a.flac",
            "/abs.flac",
            "A\\b.flac",
            "link.flac",
            "album/01.flac",
            "Album/01.FLAC",
            "X/y.flac",
            "Y",
        ];
        want.sort();
        assert_eq!(rejected, want);
        assert_eq!(
            files_under(&env.path("x/o1/g1")),
            vec!["Album/01.flac", "X", "Y/z.flac", "ok.flac"]
        );
        // root の外にも他の置き場にも何も無い
        assert!(!env.dir.path().parent().unwrap().join("a.flac").exists());
        assert!(!env.dir.path().join("a.flac").exists());
        assert_eq!(
            files_under(&env.path("x")),
            vec![
                "o1/g1/Album/01.flac",
                "o1/g1/X",
                "o1/g1/Y/z.flac",
                "o1/g1/ok.flac"
            ]
        );
        assert_eq!(files_under(&env.path("files")), Vec::<String>::new());
    }

    #[test]
    fn encrypted_zip_fails_whole() {
        let env = Env::new();
        let cfg = UploadConfig::default();
        let o = env.send("a.zip", &encrypted_zip(), &cfg);
        let out = env.run(&o, false, &cfg).unwrap();
        match out {
            UnpackOutcome::Failed { reason, retryable } => {
                assert!(!retryable);
                assert!(reason.contains("暗号化"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(files_under(&env.path("x/o1/g1")), Vec::<String>::new());
    }

    #[test]
    fn broken_central_directory_fails_whole() {
        let env = Env::new();
        let cfg = UploadConfig::default();
        let mut zip = build(&[E::File("a.flac", b"abc".to_vec())]);
        for at in headers(&zip, CENTRAL) {
            zip[at..at + 4].copy_from_slice(b"XXXX");
        }
        let o = env.send("a.zip", &zip, &cfg);
        let out = env.run(&o, false, &cfg).unwrap();
        assert!(
            matches!(
                out,
                UnpackOutcome::Failed {
                    retryable: false,
                    ..
                }
            ),
            "{out:?}"
        );
        assert!(!env.zip(&o).error.unwrap().retryable);
    }

    #[test]
    fn entry_limit_fails_whole() {
        let env = Env::new();
        let cfg = UploadConfig {
            max_zip_entries: 2,
            ..UploadConfig::default()
        };
        let zip = build(&[
            E::File("a.flac", b"1".to_vec()),
            E::File("b.flac", b"2".to_vec()),
            E::File("c.flac", b"3".to_vec()),
        ]);
        let o = env.send("a.zip", &zip, &cfg);
        match env.run(&o, false, &cfg).unwrap() {
            UnpackOutcome::Failed { reason, retryable } => {
                assert!(!retryable);
                assert!(reason.contains("エントリ数"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cumulative_budget_across_zips() {
        let env = Env::new();
        let zip = build(&[E::File("a.flac", zeros(20 * MIB))]);
        let max = 2 * zip.len() as u64 + 30 * MIB;
        let cfg = cfg_max(max);
        let a = env.send("a.zip", &zip, &cfg);
        let b = env.send("b.zip", &zip, &cfg);
        assert!(matches!(
            env.run(&a, false, &cfg).unwrap(),
            UnpackOutcome::Done { .. }
        ));
        let out = env.run(&b, false, &cfg).unwrap();
        assert!(
            matches!(
                out,
                UnpackOutcome::Failed {
                    retryable: false,
                    ..
                }
            ),
            "{out:?}"
        );
        let m = env.manifest();
        assert!(used_bytes(&m) <= max);
        assert_eq!(
            env.garbage("x/o2/g1").unwrap().bytes,
            disk_size(&env.path("x/o2/g1"))
        );
    }

    #[test]
    fn size_counts_written_not_declared() {
        let env = Env::new();
        let mut zip = build(&[E::File("a.flac", zeros(20 * MIB))]);
        // 申告サイズ（非圧縮）を 1 KiB に偽る
        let fake = 1024u32.to_le_bytes();
        for at in headers(&zip, LOCAL) {
            zip[at + 22..at + 26].copy_from_slice(&fake);
        }
        for at in headers(&zip, CENTRAL) {
            zip[at + 24..at + 28].copy_from_slice(&fake);
        }
        let max = zip.len() as u64 + 8 * MIB;
        let cfg = cfg_max(max);
        let o = env.send("a.zip", &zip, &cfg);
        let out = env.run(&o, false, &cfg).unwrap();
        assert!(
            matches!(
                out,
                UnpackOutcome::Failed {
                    retryable: false,
                    ..
                }
            ),
            "{out:?}"
        );
        let size = disk_size(&env.path("x/o1/g1"));
        assert!(size <= 8 * MIB, "{size}");
        assert_eq!(env.garbage("x/o1/g1").unwrap().bytes, size);
        assert!(used_bytes(&env.manifest()) <= max);
    }

    #[test]
    fn skips_macosx() {
        let env = Env::new();
        let cfg = UploadConfig::default();
        let zip = build(&[
            E::Dir("__MACOSX/"),
            E::File("__MACOSX/._a.flac", b"meta".to_vec()),
            E::File("__MACOSX/Album/x.flac", b"meta".to_vec()),
            E::File("a.flac", b"a".to_vec()),
        ]);
        let o = env.send("a.zip", &zip, &cfg);
        assert_eq!(
            env.run(&o, false, &cfg).unwrap(),
            UnpackOutcome::Done {
                generation: 1,
                files: 1,
                rejected: 0
            }
        );
        assert_eq!(files_under(&env.path("x/o1/g1")), vec!["a.flac"]);
        assert!(!env.path("x/o1/g1/__MACOSX").exists());
    }

    #[test]
    fn keeps_zip_body() {
        let env = Env::new();
        let cfg = UploadConfig::default();
        let zip = build(&[E::File("a.flac", b"a".to_vec())]);
        let o = env.send("a.zip", &zip, &cfg);
        env.run(&o, false, &cfg).unwrap();
        assert_eq!(std::fs::read(env.path("zips/o1.zip")).unwrap(), zip);
    }

    #[test]
    fn discarded_upload_is_skipped() {
        let env = Env::new();
        let cfg = UploadConfig::default();
        let o = env.send("a.zip", &build(&[E::File("a.flac", b"a".to_vec())]), &cfg);
        with_manifest(&env.inbox, &env.locks, &env.id, |m| {
            m.discarded_at = Some(5);
            Ok(())
        })
        .unwrap();
        let before = env.manifest();
        assert_eq!(env.run(&o, false, &cfg).unwrap(), UnpackOutcome::Skipped);
        assert_eq!(env.manifest(), before);
        assert!(!env.path("x/o1").exists());
    }

    #[test]
    fn failed_generation_left_in_garbage() {
        let env = Env::new();
        let zip = build(&[E::File("a.flac", zeros(12 * MIB))]);
        let cfg = cfg_max(zip.len() as u64 + 4 * MIB);
        let o = env.send("a.zip", &zip, &cfg);
        let out = env.run(&o, false, &cfg).unwrap();
        assert!(matches!(
            out,
            UnpackOutcome::Failed {
                retryable: false,
                ..
            }
        ));
        let z = env.zip(&o);
        assert_eq!(z.generation, 0);
        assert_eq!(z.unpacking, None);
        assert_eq!(z.written_bytes, 0);
        let g = env.garbage("x/o1/g1").unwrap();
        assert!(g.bytes > 0);
        assert_eq!(g.bytes, disk_size(&env.path("x/o1/g1")));
        assert!(env.path("x/o1/g1/a.flac").is_file());
    }
}

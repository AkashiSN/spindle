//! ローカルの root（`~/Music/spindle`）。`.spindle-device` の照合、tmp → fsync → rename → 親の fsync での
//! 配置、一覧、sha256、空き容量。パスはすべて root 相対（`/` 区切り）で受け取り、root の dirfd から
//! シンボリックリンクを辿らずに 1 要素ずつ開く

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rustix::fs::{
    fstat, fsync, mkdirat, openat, renameat, statat, unlinkat, AtFlags, Dir, FileType, Mode,
    OFlags, Stat, CWD,
};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::pathkey::{check_rel, to_abs};
use crate::{Error, Result};

pub const MARKER: &str = ".spindle-device";
pub const MOVING_DIR: &str = ".moving";
pub const TMP_SUFFIX: &str = ".spindle-tmp";

/// エージェントの予約（印・バッチの置き場・取得中の一時ファイル）
pub fn is_reserved(rel: &str) -> bool {
    rel == MARKER || rel.starts_with(&format!("{MOVING_DIR}/")) || rel.ends_with(TMP_SUFFIX)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub device_uuid: String,
    pub nonce: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    pub size: u64,
    pub inode: u64,
    pub mtime_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    Same,
    Changed(FileStat, String),
    Missing,
}

/// root の dirfd から 1 要素ずつ辿る（D-100 の境界。`openat2` の無い macOS でも同じに振る舞う）。
/// 中間・最後のどの要素のシンボリックリンクも辿らない。読みでは「無い・通常ファイルでない」と
/// 扱い、書き・rename・削除はエラーで断る（root の外には決して手を出さない）
pub struct LocalRoot {
    root: PathBuf,
}

/// 親ディレクトリを辿った結果
enum Parent {
    /// 親の dirfd と最後の要素の名前
    Dir(OwnedFd, String),
    /// 途中の要素（か root）が無い
    Missing,
    /// 途中の要素がシンボリックリンクかディレクトリでないもの
    Blocked,
}

fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn file_mode() -> Mode {
    Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH
}

fn dir_mode() -> Mode {
    Mode::RWXU | Mode::RWXG | Mode::RWXO
}

fn io(e: Errno) -> Error {
    Error::Io(e.into())
}

/// 辿らないシンボリックリンク（かディレクトリでないもの）が途中にあって書けない
fn blocked(rel: &str) -> Error {
    Error::Stop(format!(
        "root の下のシンボリックリンク（またはディレクトリでないもの）は辿らないため扱えません（{rel}）"
    ))
}

fn not_found(rel: &str) -> Error {
    Error::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("ファイルが無い（{rel}）"),
    ))
}

/// 開けなかった理由がシンボリックリンク・ディレクトリでないものか
fn is_blocked(e: Errno) -> bool {
    e == Errno::LOOP || e == Errno::NOTDIR
}

#[allow(clippy::unnecessary_cast)]
fn file_stat(st: &Stat) -> Option<FileStat> {
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
        return None;
    }
    Some(FileStat {
        size: st.st_size as u64,
        inode: st.st_ino as u64,
        mtime_ns: (st.st_mtime as i64) * 1_000_000_000 + st.st_mtime_nsec as i64,
    })
}

impl LocalRoot {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// ミュージック.app へ渡す絶対パス。ファイルの読み書きには使わない（root の dirfd から辿る）
    pub fn abs(&self, rel: &str) -> Result<PathBuf> {
        to_abs(&self.root, rel)
    }

    /// root の dirfd（root 自身は設定どおりに開く）。無ければ None
    fn open_root(&self) -> Result<Option<OwnedFd>> {
        match openat(
            CWD,
            &self.root,
            dir_flags() - OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(fd) => Ok(Some(fd)),
            Err(Errno::NOENT) => Ok(None),
            Err(e) => Err(io(e)),
        }
    }

    /// `rel` の親を辿る。`create` なら無いディレクトリを作る（root も）
    fn parent(&self, rel: &str, create: bool) -> Result<Parent> {
        check_rel(rel).map_err(Error::Stop)?;
        if create {
            std::fs::create_dir_all(&self.root)?;
        }
        let Some(mut dir) = self.open_root()? else {
            return Ok(Parent::Missing);
        };
        let mut parts: Vec<&str> = rel.split('/').collect();
        let Some(name) = parts.pop() else {
            return Err(Error::Stop(format!("パスが不正（{rel}）")));
        };
        for c in parts {
            let next = match openat(&dir, c, dir_flags(), Mode::empty()) {
                Ok(fd) => fd,
                Err(Errno::NOENT) if create => {
                    match mkdirat(&dir, c, dir_mode()) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(e) => return Err(io(e)),
                    }
                    match openat(&dir, c, dir_flags(), Mode::empty()) {
                        Ok(fd) => fd,
                        Err(e) if is_blocked(e) => return Ok(Parent::Blocked),
                        Err(e) => return Err(io(e)),
                    }
                }
                Err(Errno::NOENT) => return Ok(Parent::Missing),
                Err(e) if is_blocked(e) => return Ok(Parent::Blocked),
                Err(e) => return Err(io(e)),
            };
            dir = next;
        }
        Ok(Parent::Dir(dir, name.to_owned()))
    }

    /// 書くための親（無いディレクトリは作る）。途中のシンボリックリンクはエラー
    fn parent_for_write(&self, rel: &str) -> Result<(OwnedFd, String)> {
        match self.parent(rel, true)? {
            Parent::Dir(d, n) => Ok((d, n)),
            Parent::Missing => Err(not_found(rel)),
            Parent::Blocked => Err(blocked(rel)),
        }
    }

    /// 既にあるはずのものの親。無ければ NotFound、途中のシンボリックリンクはエラー
    fn parent_existing(&self, rel: &str) -> Result<(OwnedFd, String)> {
        match self.parent(rel, false)? {
            Parent::Dir(d, n) => Ok((d, n)),
            Parent::Missing => Err(not_found(rel)),
            Parent::Blocked => Err(blocked(rel)),
        }
    }

    /// 通常ファイルを読みで開く（シンボリックリンク・通常ファイルでないもの・無いものは None）
    fn open_read(&self, rel: &str) -> Result<Option<File>> {
        let Parent::Dir(dir, name) = self.parent(rel, false)? else {
            return Ok(None);
        };
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let fd = match openat(&dir, name.as_str(), flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(e) if is_blocked(e) => return Ok(None),
            Err(e) => return Err(io(e)),
        };
        if file_stat(&fstat(&fd).map_err(io)?).is_none() {
            return Ok(None);
        }
        Ok(Some(File::from(fd)))
    }

    /// root が無いか、中身が空か
    pub fn is_empty_or_missing(&self) -> Result<bool> {
        match std::fs::read_dir(&self.root) {
            Ok(mut it) => Ok(it.next().is_none()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(e) => Err(e.into()),
        }
    }

    pub fn read_marker(&self) -> Result<Option<Marker>> {
        let Some(root) = self.open_root()? else {
            return Ok(None);
        };
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = match openat(&root, MARKER, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(e) if is_blocked(e) => return Err(blocked(MARKER)),
            Err(e) => return Err(io(e)),
        };
        let mut b = Vec::new();
        File::from(fd).read_to_end(&mut b)?;
        serde_json::from_slice(&b)
            .map(Some)
            .map_err(|e| Error::Stop(format!("{MARKER} を読めない: {e}")))
    }

    /// root 直下へ tmp → fsync → rename → root の fsync で書く
    pub fn write_marker(&self, m: &Marker) -> Result<()> {
        let bytes = serde_json::to_vec(m).map_err(|e| Error::State(e.to_string()))?;
        let (root, _) = self.parent_for_write(MARKER)?;
        let tmp = format!(".{MARKER}.tmp");
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = match openat(&root, tmp.as_str(), flags, file_mode()) {
            Ok(fd) => fd,
            Err(e) if is_blocked(e) => return Err(blocked(&tmp)),
            Err(e) => return Err(io(e)),
        };
        let mut f = File::from(fd);
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        renameat(&root, tmp.as_str(), &root, MARKER).map_err(io)?;
        fsync(&root).map_err(io)?;
        Ok(())
    }

    /// 何かがあるか（シンボリックリンクもあるとみなす。途中にシンボリックリンクがあれば無い）
    pub fn exists(&self, rel: &str) -> Result<bool> {
        let Parent::Dir(dir, name) = self.parent(rel, false)? else {
            return Ok(false);
        };
        match statat(&dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(true),
            Err(Errno::NOENT) => Ok(false),
            Err(e) => Err(io(e)),
        }
    }

    /// 通常ファイルの stat。無い・通常ファイルでないもの（途中にシンボリックリンクがあるものも）は None
    pub fn stat(&self, rel: &str) -> Result<Option<FileStat>> {
        let Parent::Dir(dir, name) = self.parent(rel, false)? else {
            return Ok(None);
        };
        match statat(&dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => Ok(file_stat(&st)),
            Err(Errno::NOENT) => Ok(None),
            Err(e) => Err(io(e)),
        }
    }

    pub fn sha256(&self, rel: &str) -> Result<Option<String>> {
        let Some(mut f) = self.open_read(rel)? else {
            return Ok(None);
        };
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Ok(Some(
            h.finalize().iter().map(|b| format!("{b:02x}")).collect(),
        ))
    }

    /// 記録の stat と照合する。一致すれば sha256 を読まない（D-100）。`Changed` の sha256 と記録の
    /// 照合は呼び出し側が行う
    pub fn check(&self, rel: &str, recorded: &FileStat) -> Result<Check> {
        let Some(st) = self.stat(rel)? else {
            return Ok(Check::Missing);
        };
        if st == *recorded {
            return Ok(Check::Same);
        }
        let Some(sha) = self.sha256(rel)? else {
            return Ok(Check::Missing);
        };
        Ok(Check::Changed(st, sha))
    }

    /// `<rel>.spindle-tmp` を作り直して開く（親ディレクトリを作る）
    pub fn create_tmp(&self, rel: &str) -> Result<(String, File)> {
        let tmp = format!("{rel}{TMP_SUFFIX}");
        let f = self.create(&tmp)?;
        Ok((tmp, f))
    }

    /// `rel` を空にして書きで開く（無ければ作る。親ディレクトリも作る）。シンボリックリンクはエラー
    pub fn create(&self, rel: &str) -> Result<File> {
        let (dir, name) = self.parent_for_write(rel)?;
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        match openat(&dir, name.as_str(), flags, file_mode()) {
            Ok(fd) => Ok(File::from(fd)),
            Err(e) if is_blocked(e) => Err(blocked(rel)),
            Err(e) => Err(io(e)),
        }
    }

    /// 書き終えた tmp を fsync → rename → 親の fsync
    pub fn place(&self, tmp: &str, rel: &str) -> Result<()> {
        let (dir, name) = self.parent_existing(tmp)?;
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let fd = match openat(&dir, name.as_str(), flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Err(not_found(tmp)),
            Err(e) if is_blocked(e) => return Err(blocked(tmp)),
            Err(e) => return Err(io(e)),
        };
        if file_stat(&fstat(&fd).map_err(io)?).is_none() {
            return Err(blocked(tmp));
        }
        fsync(&fd).map_err(io)?;
        self.rename(tmp, rel)
    }

    /// rename（行き先の親を作る）と両方の親の fsync
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (fd_from, n_from) = self.parent_existing(from)?;
        let (fd_to, n_to) = self.parent_for_write(to)?;
        renameat(&fd_from, n_from.as_str(), &fd_to, n_to.as_str()).map_err(|e| match e {
            Errno::NOENT => not_found(from),
            e => io(e),
        })?;
        fsync(&fd_from).map_err(io)?;
        fsync(&fd_to).map_err(io)?;
        Ok(())
    }

    /// 消す（無ければ何もしない）と親の fsync。途中にシンボリックリンクがあればエラー
    pub fn remove(&self, rel: &str) -> Result<()> {
        let (dir, name) = match self.parent(rel, false)? {
            Parent::Dir(d, n) => (d, n),
            Parent::Missing => return Ok(()),
            Parent::Blocked => return Err(blocked(rel)),
        };
        match unlinkat(&dir, name.as_str(), AtFlags::empty()) {
            Ok(()) => fsync(&dir).map_err(io),
            Err(Errno::NOENT) => Ok(()),
            Err(e) => Err(io(e)),
        }
    }

    /// root の下の通常ファイル（root 相対。`.spindle-device` は除く。`.moving/` と tmp は含める）。
    /// シンボリックリンクのディレクトリには降りない
    pub fn list_files(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let Some(root) = self.open_root()? else {
            return Ok(out);
        };
        walk(&root, "", &mut out)?;
        out.retain(|p| p != MARKER);
        Ok(out)
    }

    /// 空になったディレクトリを消す（root 自身は残す）
    pub fn prune_empty_dirs(&self) -> Result<()> {
        if let Some(root) = self.open_root()? {
            prune(&root)?;
        }
        Ok(())
    }

    pub fn free_bytes(&self) -> Result<u64> {
        let probe = if self.root.exists() {
            self.root.clone()
        } else {
            self.root
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default()
        };
        let s = rustix::fs::statvfs(&probe).map_err(|e| Error::Io(e.into()))?;
        Ok(s.f_bavail.saturating_mul(s.f_frsize))
    }
}

/// ディレクトリの要素（`.` と `..` と UTF-8 でない名前を除く）と種類
fn entries(dir: &OwnedFd) -> Result<Vec<(String, FileType)>> {
    let mut out = Vec::new();
    for e in Dir::read_from(dir).map_err(io)? {
        let e = e.map_err(io)?;
        let Ok(name) = e.file_name().to_str() else {
            continue;
        };
        if name == "." || name == ".." {
            continue;
        }
        let mut ft = e.file_type();
        if ft == FileType::Unknown {
            ft = match statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) => FileType::from_raw_mode(st.st_mode),
                Err(Errno::NOENT) => continue,
                Err(e) => return Err(io(e)),
            };
        }
        out.push((name.to_owned(), ft));
    }
    Ok(out)
}

/// 子ディレクトリをシンボリックリンクを辿らずに開く。開けなければ（消えた・差し替わった）None
fn open_child_dir(dir: &OwnedFd, name: &str) -> Result<Option<OwnedFd>> {
    match openat(dir, name, dir_flags(), Mode::empty()) {
        Ok(fd) => Ok(Some(fd)),
        Err(Errno::NOENT) => Ok(None),
        Err(e) if is_blocked(e) => Ok(None),
        Err(e) => Err(io(e)),
    }
}

fn walk(dir: &OwnedFd, prefix: &str, out: &mut Vec<String>) -> Result<()> {
    for (name, ft) in entries(dir)? {
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if ft == FileType::Directory {
            if let Some(child) = open_child_dir(dir, &name)? {
                walk(&child, &rel, out)?;
            }
        } else if ft == FileType::RegularFile {
            out.push(rel);
        }
    }
    Ok(())
}

/// `dir` の下の空のディレクトリを消す。`dir` 自身が空になったら true（消すのは呼び出し側）
fn prune(dir: &OwnedFd) -> Result<bool> {
    let mut empty = true;
    for (name, ft) in entries(dir)? {
        if ft != FileType::Directory {
            empty = false;
            continue;
        }
        let Some(child) = open_child_dir(dir, &name)? else {
            empty = false;
            continue;
        };
        if !prune(&child)? {
            empty = false;
            continue;
        }
        match unlinkat(dir, name.as_str(), AtFlags::REMOVEDIR) {
            Ok(()) | Err(Errno::NOENT) => {}
            // 片付けの間に何か置かれた
            Err(Errno::NOTEMPTY) | Err(Errno::EXIST) => empty = false,
            Err(e) => return Err(io(e)),
        }
    }
    Ok(empty)
}

/// 多重起動の防止（`<state_dir>/lock` の flock）。落ちれば OS が解放する
pub struct Lock {
    _file: File,
}

pub fn lock(state_dir: &Path) -> Result<Lock> {
    std::fs::create_dir_all(state_dir)?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state_dir.join("lock"))?;
    match rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Lock { _file: f }),
        Err(rustix::io::Errno::WOULDBLOCK) => Err(Error::Stop(
            "別の spindle-agent が実行中です。終わってからやり直してください".to_owned(),
        )),
        Err(e) => Err(Error::Io(e.into())),
    }
}

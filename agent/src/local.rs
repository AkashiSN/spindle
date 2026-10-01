//! ローカルの root（`~/Music/spindle`）。`.spindle-device` の照合、tmp → fsync → rename → 親の fsync での
//! 配置、一覧、sha256、空き容量。パスはすべて root 相対（`/` 区切り）で受け取る

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::pathkey::to_abs;
use crate::state::write_durable;
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

pub struct LocalRoot {
    root: PathBuf,
}

impl LocalRoot {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn abs(&self, rel: &str) -> Result<PathBuf> {
        to_abs(&self.root, rel)
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
        match std::fs::read(self.root.join(MARKER)) {
            Ok(b) => serde_json::from_slice(&b)
                .map(Some)
                .map_err(|e| Error::Stop(format!("{MARKER} を読めない: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn write_marker(&self, m: &Marker) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let bytes = serde_json::to_vec(m).map_err(|e| Error::State(e.to_string()))?;
        write_durable(&self.root, MARKER, &bytes)
    }

    pub fn exists(&self, rel: &str) -> Result<bool> {
        match std::fs::symlink_metadata(self.abs(rel)?) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// 通常ファイルの stat。無い・通常ファイルでないものは None
    pub fn stat(&self, rel: &str) -> Result<Option<FileStat>> {
        match std::fs::symlink_metadata(self.abs(rel)?) {
            Ok(m) if m.is_file() => Ok(Some(FileStat {
                size: m.len(),
                inode: m.ino(),
                mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
            })),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn sha256(&self, rel: &str) -> Result<Option<String>> {
        if self.stat(rel)?.is_none() {
            return Ok(None);
        }
        let mut f = File::open(self.abs(rel)?)?;
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
        let abs = self.abs(&tmp)?;
        if let Some(p) = abs.parent() {
            std::fs::create_dir_all(p)?;
        }
        let f = File::create(&abs)?;
        Ok((tmp, f))
    }

    /// 書き終えた tmp を fsync → rename → 親の fsync
    pub fn place(&self, tmp: &str, rel: &str) -> Result<()> {
        File::open(self.abs(tmp)?)?.sync_all()?;
        self.rename(tmp, rel)
    }

    /// rename（行き先の親を作る）と両方の親の fsync
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (a, b) = (self.abs(from)?, self.abs(to)?);
        if let Some(p) = b.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::rename(&a, &b)?;
        sync_parent(&a)?;
        sync_parent(&b)?;
        Ok(())
    }

    /// 消す（無ければ何もしない）と親の fsync
    pub fn remove(&self, rel: &str) -> Result<()> {
        let abs = self.abs(rel)?;
        match std::fs::remove_file(&abs) {
            Ok(()) => sync_parent(&abs),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// root の下の通常ファイル（root 相対。`.spindle-device` は除く。`.moving/` と tmp は含める）
    pub fn list_files(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        if !self.root.exists() {
            return Ok(out);
        }
        walk(&self.root, &self.root, &mut out)?;
        out.retain(|p| p != MARKER);
        Ok(out)
    }

    /// 空になったディレクトリを消す（root 自身は残す）
    pub fn prune_empty_dirs(&self) -> Result<()> {
        if self.root.exists() {
            prune(&self.root, true)?;
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

fn sync_parent(p: &Path) -> Result<()> {
    if let Some(dir) = p.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let ft = e.file_type()?;
        let p = e.path();
        if ft.is_dir() {
            walk(root, &p, out)?;
        } else if ft.is_file() {
            if let Some(rel) = crate::pathkey::to_rel(root, &p) {
                out.push(rel);
            }
        }
    }
    Ok(())
}

/// 空なら消して true
fn prune(dir: &Path, is_root: bool) -> Result<bool> {
    let mut empty = true;
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        if e.file_type()?.is_dir() {
            if !prune(&e.path(), false)? {
                empty = false;
            }
        } else {
            empty = false;
        }
    }
    if empty && !is_root {
        std::fs::remove_dir(dir)?;
        return Ok(true);
    }
    Ok(false)
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

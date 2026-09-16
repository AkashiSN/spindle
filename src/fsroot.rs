//! root dirfd 基準のファイル操作（SPEC §5「パスの表現と境界」、D-31）。
//!
//! Library / Derived / Archive / Inbox / Playlists の各 root を [`RootDir`] として開き、以後の
//! ファイル操作はすべて root の dirfd から `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` で行う。
//! パス文字列を結合して `File::open` しない。canonicalize → prefix 比較は symlink の差し替え
//! （TOCTOU）に負ける。symlink はディレクトリもファイルも辿らない。
//!
//! `openat2` は Linux 5.6+ が必要。使えなければ [`RootDir::open`] が失敗し、起動が止まる
//! （fail-fast。フォールバックは設けない）。非 Linux では実装を持たず、[`RootDir::open`] が
//! 常に [`FsError::Openat2Unsupported`] を返す（開発機でビルドとユニットテストは通し、
//! 実ファイルを触る結合テストは Linux の CI で走らせる。SPEC §5）。

use std::ffi::OsString;
use std::path::PathBuf;

use crate::domain::relpath::RelPath;

#[cfg(target_os = "linux")]
pub use linux::{copy_attrs, fstat, RootDir};
#[cfg(not(target_os = "linux"))]
pub use stub::{copy_attrs, fstat, RootDir};

/// 書き込みの一時ファイル名の前置き。対象と同じディレクトリに `O_EXCL` で作る
pub const TMP_PREFIX: &str = ".spindle-tmp-";

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("openat2 が使えない（Linux 5.6+ が必要）")]
    Openat2Unsupported,
    #[error("root がディレクトリではない: {0}")]
    NotDirectory(PathBuf),
    #[error("パスの途中または末尾に symlink がある")]
    Symlink,
    #[error("パスが root の外を指す")]
    Escaped,
    #[error("存在しない")]
    NotFound,
    #[error("既に存在する")]
    Exists,
    #[error("I/O エラー: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    /// デバイス・FIFO・ソケットなど。スキャンでは対象外
    Other,
}

/// symlink を辿らない stat の結果。時刻はナノ秒の UNIX epoch
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub kind: FileKind,
    pub dev: u64,
    pub inode: u64,
    pub nlink: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

/// ディレクトリの 1 エントリ。名前は UTF-8 とは限らない（変換は呼び出し側）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: OsString,
    pub kind: FileKind,
}

/// 設定された全 root。起動時に開き、`openat2` が使えなければここで失敗する（fail-fast）。
/// `data` はアプリ自身の領域（DB・tmp）で、ライブラリのパス境界の対象ではないので含めない
#[derive(Debug)]
pub struct Roots {
    pub library: RootDir,
    pub derived: RootDir,
    pub archive: RootDir,
    pub inbox: RootDir,
    pub playlists: RootDir,
}

impl Roots {
    pub fn open(paths: &crate::config::PathsConfig) -> Result<Self, FsError> {
        Ok(Self {
            library: RootDir::open(&paths.library)?,
            derived: RootDir::open(&paths.derived)?,
            archive: RootDir::open(&paths.archive)?,
            inbox: RootDir::open(&paths.inbox)?,
            playlists: RootDir::open(&paths.playlists)?,
        })
    }
}

/// Linux 実装。`openat2` と `renameat2(RENAME_NOREPLACE)` は Linux 固有
#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsString;
    use std::fs::File;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::io::{AsFd, OwnedFd};
    use std::path::{Path, PathBuf};

    use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, RenameFlags, ResolveFlags};
    use rustix::io::Errno;

    use super::{DirEntry, FileKind, FsError, RelPath, Stat, TMP_PREFIX};

    /// `O_EXCL` が EEXIST で失敗したときに名前を引き直す回数
    const TMP_RETRIES: usize = 8;

    impl From<Errno> for FsError {
        fn from(e: Errno) -> Self {
            match e {
                Errno::LOOP => FsError::Symlink,
                Errno::XDEV => FsError::Escaped,
                Errno::NOENT => FsError::NotFound,
                Errno::EXIST => FsError::Exists,
                Errno::NOSYS => FsError::Openat2Unsupported,
                other => FsError::Io(std::io::Error::from(other)),
            }
        }
    }

    impl From<FileType> for FileKind {
        fn from(t: FileType) -> Self {
            match t {
                FileType::RegularFile => FileKind::File,
                FileType::Directory => FileKind::Dir,
                FileType::Symlink => FileKind::Symlink,
                _ => FileKind::Other,
            }
        }
    }

    impl From<rustix::fs::Stat> for Stat {
        fn from(st: rustix::fs::Stat) -> Self {
            Stat {
                kind: FileType::from_raw_mode(st.st_mode).into(),
                dev: st.st_dev,
                inode: st.st_ino,
                nlink: st.st_nlink,
                size: st.st_size as u64,
                mtime_ns: st.st_mtime * 1_000_000_000 + st.st_mtime_nsec as i64,
                ctime_ns: st.st_ctime * 1_000_000_000 + st.st_ctime_nsec as i64,
            }
        }
    }

    /// 開いた root ディレクトリ
    #[derive(Debug)]
    pub struct RootDir {
        fd: OwnedFd,
        path: PathBuf,
    }

    const RESOLVE: ResolveFlags = ResolveFlags::BENEATH.union(ResolveFlags::NO_SYMLINKS);

    impl RootDir {
        /// root を開き、`openat2` が使えることを確かめる
        pub fn open(path: &Path) -> Result<Self, FsError> {
            let fd = rustix::fs::open(
                path,
                OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| match e {
                Errno::NOTDIR => FsError::NotDirectory(path.to_path_buf()),
                other => FsError::from(other),
            })?;
            let root = RootDir {
                fd,
                path: path.to_path_buf(),
            };
            // openat2 の有無をここで確かめる（ENOSYS なら起動時に落とす）
            root.open_at(".", OFlags::PATH | OFlags::DIRECTORY)?;
            Ok(root)
        }

        pub fn path(&self) -> &Path {
            &self.path
        }

        fn open_at(&self, rel: &str, flags: OFlags) -> Result<OwnedFd, FsError> {
            Ok(rustix::fs::openat2(
                &self.fd,
                rel,
                // O_NOFOLLOW は付けない。O_PATH と組むと末尾の symlink 自体が開けてしまう。
                // 末尾を含む全段の symlink 拒否は RESOLVE_NO_SYMLINKS（ELOOP）に任せる
                flags | OFlags::CLOEXEC,
                Mode::empty(),
                RESOLVE,
            )?)
        }

        /// `rel` の親ディレクトリを `O_PATH` で開く。root 直下なら root 自身の複製ではなく `.`
        fn open_parent(&self, rel: &RelPath) -> Result<OwnedFd, FsError> {
            let parent = rel.parent();
            let dir = parent.as_ref().map(RelPath::as_str).unwrap_or(".");
            self.open_at(dir, OFlags::PATH | OFlags::DIRECTORY)
        }

        /// 読み取り用に開く。途中・末尾の symlink は [`FsError::Symlink`]
        pub fn open_file(&self, rel: &RelPath) -> Result<File, FsError> {
            let fd = self.open_at(rel.as_str(), OFlags::RDONLY)?;
            Ok(File::from(fd))
        }

        /// symlink を辿らない stat。末尾が symlink なら symlink 自身の属性を返す
        pub fn stat(&self, rel: &RelPath) -> Result<Stat, FsError> {
            let parent = self.open_parent(rel)?;
            let st = rustix::fs::statat(&parent, rel.file_name(), AtFlags::SYMLINK_NOFOLLOW)?;
            Ok(st.into())
        }

        /// ディレクトリの一覧（`.` / `..` を除く）。`None` は root 自身
        pub fn read_dir(&self, rel: Option<&RelPath>) -> Result<Vec<DirEntry>, FsError> {
            let dir = rel.map(RelPath::as_str).unwrap_or(".");
            let fd = self.open_at(dir, OFlags::RDONLY | OFlags::DIRECTORY)?;
            let mut entries = Vec::new();
            for entry in Dir::read_from(fd.as_fd())? {
                let entry = entry?;
                let name = entry.file_name().to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                entries.push(DirEntry {
                    name: OsString::from_vec(name.to_vec()),
                    kind: entry.file_type().into(),
                });
            }
            Ok(entries)
        }

        /// `dir`（`None` は root）に `.spindle-tmp-<random>` を `O_EXCL` で作り、書き込み用に開く
        pub fn create_tmp(&self, dir: Option<&RelPath>) -> Result<(RelPath, File), FsError> {
            let parent = match dir {
                Some(d) => self.open_at(d.as_str(), OFlags::PATH | OFlags::DIRECTORY)?,
                None => self.open_at(".", OFlags::PATH | OFlags::DIRECTORY)?,
            };
            for _ in 0..TMP_RETRIES {
                let name = format!("{TMP_PREFIX}{}", random_hex()?);
                // RDWR: 書いた後に同じ FD から読み戻して tag_hash を確定する（tagwrite）
                let flags = OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC;
                match rustix::fs::openat(&parent, &name, flags, Mode::from_raw_mode(0o644)) {
                    Ok(fd) => {
                        let rel = match dir {
                            Some(d) => d.join(&name),
                            None => RelPath::parse(&name),
                        }
                        .map_err(|e| FsError::Io(std::io::Error::other(e)))?;
                        return Ok((rel, File::from(fd)));
                    }
                    Err(Errno::EXIST) => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            Err(FsError::Exists)
        }

        /// `RENAME_NOREPLACE` で rename。宛先があれば [`FsError::Exists`]（衝突の最終判定は FS に任せる）
        pub fn rename_noreplace(&self, from: &RelPath, to: &RelPath) -> Result<(), FsError> {
            let from_dir = self.open_parent(from)?;
            let to_dir = self.open_parent(to)?;
            rustix::fs::renameat_with(
                &from_dir,
                from.file_name(),
                &to_dir,
                to.file_name(),
                RenameFlags::NOREPLACE,
            )?;
            Ok(())
        }

        /// ファイルを削除する（ディレクトリは対象外）
        pub fn unlink(&self, rel: &RelPath) -> Result<(), FsError> {
            let parent = self.open_parent(rel)?;
            rustix::fs::unlinkat(&parent, rel.file_name(), AtFlags::empty())?;
            Ok(())
        }

        /// tmp を対象へ**置き換え**る rename（tmp + rename の最終段。SPEC §7.5）。宛先は同じ
        /// ディレクトリにある前提で、rename 後に親ディレクトリを fsync して電源断でも
        /// エントリが残るようにする
        pub fn replace_file(&self, tmp: &RelPath, dst: &RelPath) -> Result<(), FsError> {
            let from_dir = self.open_parent(tmp)?;
            let to_dir = self.open_parent(dst)?;
            rustix::fs::renameat(&from_dir, tmp.file_name(), &to_dir, dst.file_name())?;
            // O_PATH の fd は fsync できないので開き直す
            let dir = self.open_at(
                dst.parent().as_ref().map(RelPath::as_str).unwrap_or("."),
                OFlags::RDONLY | OFlags::DIRECTORY,
            )?;
            rustix::fs::fsync(&dir)?;
            Ok(())
        }
    }

    /// 開いた FD の stat（事前条件の確認は open した FD に対して行う。SPEC §7.5）
    pub fn fstat(file: &File) -> Result<Stat, FsError> {
        Ok(rustix::fs::fstat(file)?.into())
    }

    /// tmp + rename で置き換える前に、元ファイルの属性を tmp へ写す（D-41）。
    /// mode は必須（失敗したらエラー）。所有者と xattr（NFSv4 ACL を含む）は best-effort で、
    /// 権限や FS の都合で写せなければ警告して続行する
    pub fn copy_attrs(src: &File, dst: &File) -> Result<(), FsError> {
        let st = rustix::fs::fstat(src)?;
        rustix::fs::fchmod(dst, Mode::from_raw_mode(st.st_mode))?;
        if let Err(e) = rustix::fs::fchown(
            dst,
            Some(rustix::fs::Uid::from_raw(st.st_uid)),
            Some(rustix::fs::Gid::from_raw(st.st_gid)),
        ) {
            if e != Errno::PERM {
                tracing::warn!(error = %e, "所有者を写せない");
            }
        }
        let names = match list_xattr_names(src) {
            Ok(n) => n,
            Err(Errno::NOTSUP) => return Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, "xattr を列挙できない");
                return Ok(());
            }
        };
        for name in names {
            let value = match get_xattr(src, &name) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(xattr = %name, error = %e, "xattr を読めない");
                    continue;
                }
            };
            if let Err(e) =
                rustix::fs::fsetxattr(dst, &name, &value, rustix::fs::XattrFlags::empty())
            {
                // security.* / trusted.* は権限が要る。写せなくても書き込み自体は続ける
                tracing::warn!(xattr = %name, error = %e, "xattr を写せない");
            }
        }
        Ok(())
    }

    fn list_xattr_names(file: &File) -> Result<Vec<String>, Errno> {
        let mut empty: [u8; 0] = [];
        let needed = rustix::fs::flistxattr(file, &mut empty[..])?;
        if needed == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; needed];
        let n = rustix::fs::flistxattr(file, &mut buf[..])?;
        Ok(buf[..n]
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect())
    }

    fn get_xattr(file: &File, name: &str) -> Result<Vec<u8>, Errno> {
        let mut empty: [u8; 0] = [];
        let needed = rustix::fs::fgetxattr(file, name, &mut empty[..])?;
        let mut buf = vec![0u8; needed];
        let n = rustix::fs::fgetxattr(file, name, &mut buf[..])?;
        buf.truncate(n);
        Ok(buf)
    }

    fn random_hex() -> Result<String, FsError> {
        let mut raw = [0u8; 8];
        getrandom::fill(&mut raw).map_err(|e| FsError::Io(std::io::Error::other(e)))?;
        Ok(raw.iter().map(|b| format!("{b:02x}")).collect())
    }
}

/// 非 Linux 向けの空実装。ビルドは通すが、開くことはできない
#[cfg(not(target_os = "linux"))]
mod stub {
    use std::fs::File;
    use std::path::{Path, PathBuf};

    use super::{DirEntry, FsError, RelPath, Stat};

    #[derive(Debug)]
    pub struct RootDir {
        path: PathBuf,
    }

    impl RootDir {
        pub fn open(path: &Path) -> Result<Self, FsError> {
            let _ = path;
            Err(FsError::Openat2Unsupported)
        }

        pub fn path(&self) -> &Path {
            &self.path
        }

        pub fn open_file(&self, _rel: &RelPath) -> Result<File, FsError> {
            Err(FsError::Openat2Unsupported)
        }

        pub fn stat(&self, _rel: &RelPath) -> Result<Stat, FsError> {
            Err(FsError::Openat2Unsupported)
        }

        pub fn read_dir(&self, _rel: Option<&RelPath>) -> Result<Vec<DirEntry>, FsError> {
            Err(FsError::Openat2Unsupported)
        }

        pub fn create_tmp(&self, _dir: Option<&RelPath>) -> Result<(RelPath, File), FsError> {
            Err(FsError::Openat2Unsupported)
        }

        pub fn rename_noreplace(&self, _from: &RelPath, _to: &RelPath) -> Result<(), FsError> {
            Err(FsError::Openat2Unsupported)
        }

        pub fn unlink(&self, _rel: &RelPath) -> Result<(), FsError> {
            Err(FsError::Openat2Unsupported)
        }

        pub fn replace_file(&self, _tmp: &RelPath, _dst: &RelPath) -> Result<(), FsError> {
            Err(FsError::Openat2Unsupported)
        }
    }

    pub fn fstat(_file: &File) -> Result<Stat, FsError> {
        Err(FsError::Openat2Unsupported)
    }

    pub fn copy_attrs(_src: &File, _dst: &File) -> Result<(), FsError> {
        Err(FsError::Openat2Unsupported)
    }
}

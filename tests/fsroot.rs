//! root dirfd 基準のファイル操作（SPEC §5「パスの表現と境界」、D-31）。
//! `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`、`O_EXCL` の tmp、`RENAME_NOREPLACE`。
//! 受け入れ: docs/TASKS.md P0-5 (i)

#![cfg(target_os = "linux")]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{symlink, MetadataExt};

use spindle::domain::relpath::RelPath;
use spindle::fsroot::{FileKind, FsError, RootDir};

fn rp(s: &str) -> RelPath {
    RelPath::parse(s).unwrap()
}

/// root/Album/x.flac と root/Album/-x.flac を持つ一時ツリー
fn tree() -> (tempfile::TempDir, RootDir) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("Album")).unwrap();
    fs::write(dir.path().join("Album/x.flac"), b"xflac").unwrap();
    fs::write(dir.path().join("Album/-x.flac"), b"dash").unwrap();
    let root = RootDir::open(dir.path()).unwrap();
    (dir, root)
}

// ---------------------------------------------------------------- open

#[test]
fn open_root_requires_directory() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("f"), b"").unwrap();
    assert!(RootDir::open(&dir.path().join("f")).is_err());
    assert!(RootDir::open(&dir.path().join("missing")).is_err());
    assert_eq!(RootDir::open(dir.path()).unwrap().path(), dir.path());
}

#[test]
fn open_file_reads_regular_file() {
    let (_d, root) = tree();
    let mut f = root.open_file(&rp("Album/x.flac")).unwrap();
    let mut s = String::new();
    f.read_to_string(&mut s).unwrap();
    assert_eq!(s, "xflac");
    // 先頭 `-` のファイル名も普通に開ける
    let mut f = root.open_file(&rp("Album/-x.flac")).unwrap();
    s.clear();
    f.read_to_string(&mut s).unwrap();
    assert_eq!(s, "dash");
}

#[test]
fn open_file_reports_not_found() {
    let (_d, root) = tree();
    assert!(matches!(
        root.open_file(&rp("Album/none.flac")),
        Err(FsError::NotFound)
    ));
}

#[test]
fn open_file_refuses_symlink_as_final_component() {
    let (d, root) = tree();
    symlink("x.flac", d.path().join("Album/link.flac")).unwrap();
    assert!(matches!(
        root.open_file(&rp("Album/link.flac")),
        Err(FsError::Symlink)
    ));
}

#[test]
fn open_file_refuses_symlink_directory_in_the_middle() {
    let (d, root) = tree();
    symlink("Album", d.path().join("Alias")).unwrap();
    assert!(matches!(
        root.open_file(&rp("Alias/x.flac")),
        Err(FsError::Symlink)
    ));
}

#[test]
fn open_file_refuses_symlink_escaping_root() {
    let (d, root) = tree();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), b"s").unwrap();
    symlink(
        outside.path().join("secret"),
        d.path().join("Album/esc.flac"),
    )
    .unwrap();
    symlink(outside.path(), d.path().join("Out")).unwrap();
    assert!(matches!(
        root.open_file(&rp("Album/esc.flac")),
        Err(FsError::Symlink)
    ));
    assert!(matches!(
        root.open_file(&rp("Out/secret")),
        Err(FsError::Symlink)
    ));
}

// ---------------------------------------------------------------- stat / read_dir

#[test]
fn stat_returns_physical_attributes_without_following_symlinks() {
    let (d, root) = tree();
    let meta = fs::metadata(d.path().join("Album/x.flac")).unwrap();
    let st = root.stat(&rp("Album/x.flac")).unwrap();
    assert_eq!(st.kind, FileKind::File);
    assert_eq!(st.dev, meta.dev());
    assert_eq!(st.inode, meta.ino());
    assert_eq!(st.nlink, 1);
    assert_eq!(st.size, 5);
    assert_eq!(
        st.mtime_ns,
        meta.mtime() * 1_000_000_000 + meta.mtime_nsec()
    );
    assert_eq!(
        st.ctime_ns,
        meta.ctime() * 1_000_000_000 + meta.ctime_nsec()
    );

    assert_eq!(root.stat(&rp("Album")).unwrap().kind, FileKind::Dir);

    symlink("x.flac", d.path().join("Album/link.flac")).unwrap();
    let st = root.stat(&rp("Album/link.flac")).unwrap();
    assert_eq!(st.kind, FileKind::Symlink);
    assert_ne!(st.inode, meta.ino());

    // 途中の symlink は stat でも辿らない
    symlink("Album", d.path().join("Alias")).unwrap();
    assert!(matches!(
        root.stat(&rp("Alias/x.flac")),
        Err(FsError::Symlink)
    ));
    assert!(matches!(
        root.stat(&rp("Album/none")),
        Err(FsError::NotFound)
    ));
}

#[test]
fn stat_reports_hardlink_count() {
    let (d, root) = tree();
    fs::hard_link(d.path().join("Album/x.flac"), d.path().join("Album/y.flac")).unwrap();
    assert_eq!(root.stat(&rp("Album/x.flac")).unwrap().nlink, 2);
    assert_eq!(root.stat(&rp("Album/y.flac")).unwrap().nlink, 2);
}

#[test]
fn read_dir_lists_entries_with_kind() {
    let (d, root) = tree();
    symlink("x.flac", d.path().join("Album/link.flac")).unwrap();
    fs::create_dir(d.path().join("Album/Sub")).unwrap();

    let mut entries = root.read_dir(Some(&rp("Album"))).unwrap();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let got: Vec<(String, FileKind)> = entries
        .iter()
        .map(|e| (e.name.to_string_lossy().into_owned(), e.kind))
        .collect();
    assert_eq!(
        got,
        [
            ("-x.flac".to_owned(), FileKind::File),
            ("Sub".to_owned(), FileKind::Dir),
            ("link.flac".to_owned(), FileKind::Symlink),
            ("x.flac".to_owned(), FileKind::File),
        ]
    );

    let top = root.read_dir(None).unwrap();
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].name, "Album");
    assert_eq!(top[0].kind, FileKind::Dir);
}

// ---------------------------------------------------------------- tmp / rename / unlink

#[test]
fn create_tmp_makes_unique_exclusive_file_in_same_directory() {
    let (d, root) = tree();
    let (p1, mut f1) = root.create_tmp(Some(&rp("Album"))).unwrap();
    let (p2, _f2) = root.create_tmp(Some(&rp("Album"))).unwrap();
    assert_ne!(p1, p2);
    assert_eq!(p1.parent().unwrap().as_str(), "Album");
    assert!(p1.file_name().starts_with(".spindle-tmp-"));
    f1.write_all(b"data").unwrap();
    assert_eq!(fs::read(d.path().join(p1.as_str())).unwrap(), b"data");

    let (p3, _f3) = root.create_tmp(None).unwrap();
    assert!(p3.parent().is_none());
    assert!(d.path().join(p3.as_str()).exists());
}

#[test]
fn rename_noreplace_moves_and_refuses_to_overwrite() {
    let (d, root) = tree();
    fs::create_dir(d.path().join("Other")).unwrap();

    root.rename_noreplace(&rp("Album/x.flac"), &rp("Other/y.flac"))
        .unwrap();
    assert!(!d.path().join("Album/x.flac").exists());
    assert_eq!(fs::read(d.path().join("Other/y.flac")).unwrap(), b"xflac");

    // 宛先が存在すれば衝突として失敗し、両方とも無傷
    let r = root.rename_noreplace(&rp("Other/y.flac"), &rp("Album/-x.flac"));
    assert!(matches!(r, Err(FsError::Exists)));
    assert_eq!(fs::read(d.path().join("Other/y.flac")).unwrap(), b"xflac");
    assert_eq!(fs::read(d.path().join("Album/-x.flac")).unwrap(), b"dash");

    assert!(matches!(
        root.rename_noreplace(&rp("Album/none"), &rp("Album/z")),
        Err(FsError::NotFound)
    ));
}

#[test]
fn rename_noreplace_refuses_symlink_parent() {
    let (d, root) = tree();
    symlink("Album", d.path().join("Alias")).unwrap();
    assert!(matches!(
        root.rename_noreplace(&rp("Alias/x.flac"), &rp("Album/z.flac")),
        Err(FsError::Symlink)
    ));
    assert!(matches!(
        root.rename_noreplace(&rp("Album/x.flac"), &rp("Alias/z.flac")),
        Err(FsError::Symlink)
    ));
    assert!(d.path().join("Album/x.flac").exists());
}

#[test]
fn unlink_removes_file_but_not_through_symlink() {
    let (d, root) = tree();
    root.unlink(&rp("Album/x.flac")).unwrap();
    assert!(!d.path().join("Album/x.flac").exists());
    assert!(matches!(
        root.unlink(&rp("Album/x.flac")),
        Err(FsError::NotFound)
    ));

    symlink("Album", d.path().join("Alias")).unwrap();
    assert!(matches!(
        root.unlink(&rp("Alias/-x.flac")),
        Err(FsError::Symlink)
    ));
    assert!(d.path().join("Album/-x.flac").exists());
}

// ---------------------------------------------------------------- Roots（起動時）

#[test]
fn roots_open_every_configured_root_and_fail_on_missing_one() {
    use spindle::fsroot::Roots;

    let dir = tempfile::tempdir().unwrap();
    for name in ["library", "derived", "archive", "inbox", "playlists"] {
        fs::create_dir(dir.path().join(name)).unwrap();
    }
    let mk = |base: &std::path::Path| spindle::config::PathsConfig {
        library: base.join("library"),
        derived: base.join("derived"),
        archive: base.join("archive"),
        inbox: base.join("inbox"),
        playlists: base.join("playlists"),
        data: base.join("data"),
    };
    let roots = Roots::open(&mk(dir.path())).unwrap();
    assert_eq!(roots.library.path(), dir.path().join("library"));
    assert_eq!(roots.playlists.path(), dir.path().join("playlists"));

    fs::remove_dir(dir.path().join("inbox")).unwrap();
    assert!(Roots::open(&mk(dir.path())).is_err());
}

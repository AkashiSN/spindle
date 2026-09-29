//! `source_hash` ジョブ（仕様 ③「送る元のハッシュ」）。実ファイルを tempdir に置いて確かめる

use std::io::Write as _;

use spindle::domain::device::{semantic_master, sha256_hex};
use spindle::domain::relpath::RelPath;
use spindle::fsroot::RootDir;
use spindle::jobs::handlers::source_hash::{hash_source, hash_source_with_hook};

#[test]
fn hash_matches_content_and_records_identity() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("A")).unwrap();
    std::fs::write(dir.path().join("A/x.opus"), b"hello").unwrap();
    let root = RootDir::open(dir.path()).unwrap();
    let rel = RelPath::parse("A/x.opus").unwrap();
    let h = hash_source(&root, &rel, &semantic_master(1, 1))
        .unwrap()
        .unwrap();
    assert_eq!(h.sha256, sha256_hex(b"hello"));
    assert_eq!(h.size, 5);
    assert_eq!(h.semantic, semantic_master(1, 1));
    assert!(h.inode > 0);
}

#[test]
fn hash_is_discarded_when_file_changes_during_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.opus");
    std::fs::write(&path, b"hello").unwrap();
    let root = RootDir::open(dir.path()).unwrap();
    let rel = RelPath::parse("x.opus").unwrap();
    // 読んでいる途中で追記する（size / mtime が変わる）
    let mut hook = || {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"!").unwrap();
    };
    let got = hash_source_with_hook(&root, &rel, "s", &mut hook).unwrap();
    assert!(got.is_none(), "途中で変わったハッシュは保存しない");
}

#[test]
fn symlinks_are_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("real.opus"), b"x").unwrap();
    std::os::unix::fs::symlink(dir.path().join("real.opus"), dir.path().join("link.opus")).unwrap();
    let root = RootDir::open(dir.path()).unwrap();
    let rel = RelPath::parse("link.opus").unwrap();
    assert!(hash_source(&root, &rel, "s").is_err());
}

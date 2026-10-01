use std::io::Write;

use spindle_agent::local::*;
use spindle_agent::Error;

fn root() -> (tempfile::TempDir, LocalRoot) {
    let d = tempfile::tempdir().unwrap();
    let r = LocalRoot::new(d.path().join("spindle"));
    (d, r)
}

#[test]
fn marker_round_trip_and_emptiness() {
    let (_d, r) = root();
    assert!(r.is_empty_or_missing().unwrap());
    assert_eq!(r.read_marker().unwrap(), None);
    let m = Marker {
        device_uuid: "u".into(),
        nonce: "n".into(),
    };
    r.write_marker(&m).unwrap();
    assert_eq!(r.read_marker().unwrap(), Some(m));
    // marker だけなら「空」ではない（やり直しは marker の一致で続ける）
    assert!(!r.is_empty_or_missing().unwrap());
}

#[test]
fn place_moves_tmp_into_place() {
    let (_d, r) = root();
    let (tmp, mut f) = r.create_tmp("A/b.m4a").unwrap();
    assert_eq!(tmp, format!("A/b.m4a{TMP_SUFFIX}"));
    f.write_all(b"abc").unwrap();
    drop(f);
    r.place(&tmp, "A/b.m4a").unwrap();
    assert!(!r.exists(&tmp).unwrap());
    assert_eq!(r.stat("A/b.m4a").unwrap().unwrap().size, 3);
    assert_eq!(
        r.sha256("A/b.m4a").unwrap().as_deref(),
        Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
}

#[test]
fn check_uses_stat_cache_and_detects_changes() {
    let (_d, r) = root();
    let (tmp, mut f) = r.create_tmp("x.m4a").unwrap();
    f.write_all(b"abc").unwrap();
    drop(f);
    r.place(&tmp, "x.m4a").unwrap();
    let st = r.stat("x.m4a").unwrap().unwrap();
    let sha = r.sha256("x.m4a").unwrap().unwrap();
    // stat が一致すれば sha256 は読まない
    assert_eq!(r.check("x.m4a", &st).unwrap(), Check::Same);
    std::fs::write(r.path().join("x.m4a"), b"abcd").unwrap();
    match r.check("x.m4a", &st).unwrap() {
        Check::Changed(st2, sha2) => {
            assert_eq!(st2.size, 4);
            assert_ne!(sha2, sha);
        }
        other => panic!("{other:?}"),
    }
    r.remove("x.m4a").unwrap();
    assert_eq!(r.check("x.m4a", &st).unwrap(), Check::Missing);
    // 無いファイルの削除は成功
    r.remove("x.m4a").unwrap();
}

#[test]
fn list_files_and_reserved() {
    let (_d, r) = root();
    r.write_marker(&Marker {
        device_uuid: "u".into(),
        nonce: "n".into(),
    })
    .unwrap();
    for p in ["A/a.m4a", ".moving/b-o", "A/c.m4a.spindle-tmp"] {
        let (tmp, f) = r.create_tmp(p).unwrap();
        drop(f);
        r.place(&tmp, p).unwrap();
    }
    let mut files = r.list_files().unwrap();
    files.sort();
    assert_eq!(files, vec![".moving/b-o", "A/a.m4a", "A/c.m4a.spindle-tmp"]);
    assert!(is_reserved(".moving/b-o"));
    assert!(is_reserved("A/c.m4a.spindle-tmp"));
    assert!(is_reserved(MARKER));
    assert!(!is_reserved("A/a.m4a"));
}

#[test]
fn rename_creates_parents_and_prune_removes_empty_dirs() {
    let (_d, r) = root();
    let (tmp, f) = r.create_tmp("A/B/x.m4a").unwrap();
    drop(f);
    r.place(&tmp, "A/B/x.m4a").unwrap();
    r.rename("A/B/x.m4a", "C/D/y.m4a").unwrap();
    assert!(r.exists("C/D/y.m4a").unwrap());
    r.prune_empty_dirs().unwrap();
    assert!(!r.path().join("A").exists());
    assert!(r.path().join("C/D").exists());
}

#[test]
fn lock_is_exclusive() {
    let d = tempfile::tempdir().unwrap();
    let l = lock(d.path()).unwrap();
    assert!(matches!(lock(d.path()), Err(Error::Stop(_))));
    drop(l);
    lock(d.path()).unwrap();
}

#[test]
fn file_secrets_are_private() {
    use spindle_agent::secrets::{FileSecrets, Secrets};
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let s = FileSecrets::new(d.path());
    assert_eq!(s.get().unwrap(), None);
    s.set("sel.secret").unwrap();
    assert_eq!(s.get().unwrap().as_deref(), Some("sel.secret"));
    let mode = std::fs::metadata(d.path().join("token"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// root の外に `x.m4a` を置き、root の `A/` をそこへのシンボリックリンクにする
fn root_with_symlinked_dir() -> (tempfile::TempDir, LocalRoot, std::path::PathBuf) {
    let (d, r) = root();
    let outside = d.path().join("outside");
    std::fs::create_dir_all(outside.join("empty")).unwrap();
    std::fs::write(outside.join("x.m4a"), b"outside").unwrap();
    std::fs::create_dir_all(r.path()).unwrap();
    std::os::unix::fs::symlink(&outside, r.path().join("A")).unwrap();
    (d, r, outside)
}

#[test]
fn reads_do_not_follow_symlinked_intermediate_dir() {
    let (_d, r, outside) = root_with_symlinked_dir();
    assert_eq!(r.stat("A/x.m4a").unwrap(), None);
    assert_eq!(r.sha256("A/x.m4a").unwrap(), None);
    assert!(!r.exists("A/x.m4a").unwrap());
    let st = FileStat {
        size: 7,
        inode: 1,
        mtime_ns: 0,
    };
    assert_eq!(r.check("A/x.m4a", &st).unwrap(), Check::Missing);
    // 一覧はシンボリックリンクのディレクトリに降りない
    assert!(r.list_files().unwrap().is_empty());
    // 片付けも外の空ディレクトリに手を出さない
    r.prune_empty_dirs().unwrap();
    assert!(outside.join("empty").is_dir());
    assert!(r.path().join("A").symlink_metadata().is_ok());
}

#[test]
fn writes_refuse_symlinked_intermediate_dir() {
    let (_d, r, outside) = root_with_symlinked_dir();
    let unchanged = |outside: &std::path::Path| {
        assert_eq!(std::fs::read(outside.join("x.m4a")).unwrap(), b"outside");
        let mut names: Vec<_> = std::fs::read_dir(outside)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, vec!["empty", "x.m4a"]);
    };
    // tmp を作れない
    assert!(matches!(r.create_tmp("A/x.m4a"), Err(Error::Stop(_))));
    assert!(matches!(r.create("A/y.m4a"), Err(Error::Stop(_))));
    unchanged(&outside);
    // root の中の tmp を外へ置けない
    let (tmp, mut f) = r.create_tmp("t.m4a").unwrap();
    f.write_all(b"new").unwrap();
    drop(f);
    assert!(matches!(r.place(&tmp, "A/x.m4a"), Err(Error::Stop(_))));
    assert!(matches!(r.rename(&tmp, "A/x.m4a"), Err(Error::Stop(_))));
    assert!(r.exists(&tmp).unwrap());
    unchanged(&outside);
    // 外のファイルを動かせない・消せない
    assert!(matches!(r.rename("A/x.m4a", "z.m4a"), Err(Error::Stop(_))));
    assert!(!r.exists("z.m4a").unwrap());
    assert!(matches!(r.remove("A/x.m4a"), Err(Error::Stop(_))));
    unchanged(&outside);
}

#[test]
fn final_symlink_is_not_followed() {
    let (_d, r, outside) = root_with_symlinked_dir();
    std::os::unix::fs::symlink(outside.join("x.m4a"), r.path().join("l.m4a")).unwrap();
    assert_eq!(r.stat("l.m4a").unwrap(), None);
    assert_eq!(r.sha256("l.m4a").unwrap(), None);
    // 何かがあるので、置き先としては埋まっている
    assert!(r.exists("l.m4a").unwrap());
    assert!(matches!(r.create("l.m4a"), Err(Error::Stop(_))));
    assert!(!r.list_files().unwrap().contains(&"l.m4a".to_owned()));
    let link_intact = |r: &LocalRoot| {
        let l = r.path().join("l.m4a");
        assert!(l.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_link(&l).unwrap(), outside.join("x.m4a"));
        assert_eq!(std::fs::read(outside.join("x.m4a")).unwrap(), b"outside");
    };
    // リンクそのものも消さない・動かさない・上書きしない
    assert!(matches!(r.remove("l.m4a"), Err(Error::Stop(_))));
    assert!(matches!(r.rename("l.m4a", "m.m4a"), Err(Error::Stop(_))));
    assert!(!r.exists("m.m4a").unwrap());
    let (tmp, mut f) = r.create_tmp("t.m4a").unwrap();
    f.write_all(b"new").unwrap();
    drop(f);
    assert!(matches!(r.place(&tmp, "l.m4a"), Err(Error::Stop(_))));
    assert!(matches!(r.rename(&tmp, "l.m4a"), Err(Error::Stop(_))));
    assert!(r.exists(&tmp).unwrap());
    // tmp の名前にリンクがあっても作り直さない
    std::os::unix::fs::symlink(outside.join("x.m4a"), r.path().join("u.m4a.spindle-tmp")).unwrap();
    assert!(matches!(r.create_tmp("u.m4a"), Err(Error::Stop(_))));
    link_intact(&r);
    assert_eq!(std::fs::read(outside.join("x.m4a")).unwrap(), b"outside");
    // 印がシンボリックリンクなら読まない
    std::os::unix::fs::symlink(outside.join("x.m4a"), r.path().join(MARKER)).unwrap();
    assert!(matches!(r.read_marker(), Err(Error::Stop(_))));
    let m = Marker {
        device_uuid: "u".into(),
        nonce: "n".into(),
    };
    assert!(matches!(r.write_marker(&m), Err(Error::Stop(_))));
    assert!(r
        .path()
        .join(MARKER)
        .symlink_metadata()
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read(outside.join("x.m4a")).unwrap(), b"outside");
}

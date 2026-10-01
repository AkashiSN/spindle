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

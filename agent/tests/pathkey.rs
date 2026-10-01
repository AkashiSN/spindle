use std::path::Path;

use spindle_agent::pathkey::{canonical_key, check_rel, random_id, to_abs, to_rel};

#[test]
fn canonical_key_folds_case_and_normalization() {
    // NFC の「が」と NFD の「か + ゛」、大小文字が同じ鍵になる
    assert_eq!(
        canonical_key("J-Pop/が.m4a"),
        canonical_key("j-pop/か\u{3099}.M4A")
    );
    assert_ne!(canonical_key("a.m4a"), canonical_key("b.m4a"));
}

#[test]
fn check_rel_rejects_escapes() {
    assert!(check_rel("J-Pop/YOASOBI/1-01 群青.m4a").is_ok());
    for bad in [
        "",
        "/abs.m4a",
        "a/../b.m4a",
        "./a.m4a",
        "a//b.m4a",
        "a/",
        "a\0b",
    ] {
        assert!(check_rel(bad).is_err(), "{bad:?} を通してしまった");
    }
}

#[test]
fn abs_and_rel_round_trip() {
    let root = Path::new("/Users/me/Music/spindle");
    let abs = to_abs(root, "J-Pop/x.m4a").unwrap();
    assert_eq!(abs, Path::new("/Users/me/Music/spindle/J-Pop/x.m4a"));
    assert_eq!(to_rel(root, &abs).as_deref(), Some("J-Pop/x.m4a"));
    assert_eq!(to_rel(root, Path::new("/Users/me/Music/other/x.m4a")), None);
    assert!(to_abs(root, "../x.m4a").is_err());
}

#[test]
fn random_id_is_128_bit_hex() {
    let a = random_id().unwrap();
    assert_eq!(a.len(), 32);
    assert!(a
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    assert_ne!(a, random_id().unwrap());
}

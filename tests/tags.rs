//! 正規化タグ集合の SHA-256（SPEC §6「変更検出と版の遷移」）。
//! `tag_version` は実差分があるときだけ進めるので、同じ内容は表記差があっても同じ値になること。

use spindle::domain::tags::{normalize_tags, tag_hash, TagSet};

fn set(items: &[(&str, &str)]) -> TagSet {
    normalize_tags(
        items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
    )
}

#[test]
fn hash_is_32_bytes_and_stable() {
    let h = tag_hash(&set(&[("TITLE", "曲"), ("ARTIST", "人")]));
    assert_eq!(h.len(), 32);
    assert_eq!(h, tag_hash(&set(&[("TITLE", "曲"), ("ARTIST", "人")])));
}

#[test]
fn key_order_does_not_matter() {
    let a = set(&[("TITLE", "曲"), ("ARTIST", "人")]);
    let b = set(&[("ARTIST", "人"), ("TITLE", "曲")]);
    assert_eq!(tag_hash(&a), tag_hash(&b));
}

#[test]
fn keys_are_uppercased() {
    assert_eq!(
        tag_hash(&set(&[("title", "曲")])),
        tag_hash(&set(&[("TITLE", "曲")]))
    );
}

#[test]
fn values_are_nfc_normalized() {
    let nfc = set(&[("TITLE", "が")]);
    let nfd = set(&[("TITLE", "か\u{3099}")]);
    assert_eq!(tag_hash(&nfc), tag_hash(&nfd));
    assert_eq!(nfc.items()[0].1, "が");
}

#[test]
fn multi_value_order_is_preserved_and_significant() {
    let ab = set(&[("ARTIST", "A"), ("ARTIST", "B")]);
    let ba = set(&[("ARTIST", "B"), ("ARTIST", "A")]);
    assert_ne!(tag_hash(&ab), tag_hash(&ba));
    assert_eq!(
        ab.items(),
        &[
            ("ARTIST".to_owned(), "A".to_owned()),
            ("ARTIST".to_owned(), "B".to_owned())
        ]
    );
}

#[test]
fn value_and_key_boundaries_are_unambiguous() {
    // 連結の仕方が曖昧だと別の集合が同じ hash になる
    let a = set(&[("A", "B=C")]);
    let b = set(&[("A", "B"), ("C", "")]);
    assert_ne!(tag_hash(&a), tag_hash(&b));
    let c = set(&[("TITLE", "ab"), ("TITLE", "c")]);
    let d = set(&[("TITLE", "a"), ("TITLE", "bc")]);
    assert_ne!(tag_hash(&c), tag_hash(&d));
}

#[test]
fn different_values_differ_and_empty_set_is_defined() {
    assert_ne!(
        tag_hash(&set(&[("TITLE", "a")])),
        tag_hash(&set(&[("TITLE", "b")]))
    );
    let empty = tag_hash(&set(&[]));
    assert_eq!(empty.len(), 32);
    assert_ne!(empty, tag_hash(&set(&[("TITLE", "")])));
}

#[test]
fn embedded_picture_is_hashed_as_a_tag() {
    let mut a = set(&[("TITLE", "曲")]);
    let mut b = set(&[("TITLE", "曲")]);
    a.add_picture("image/jpeg", b"\xff\xd8jpeg-bytes");
    b.add_picture("image/jpeg", b"\xff\xd8other");
    assert_ne!(tag_hash(&a), tag_hash(&b));
    assert_ne!(tag_hash(&a), tag_hash(&set(&[("TITLE", "曲")])));
    // 同じ画像なら同じ
    let mut c = set(&[("TITLE", "曲")]);
    c.add_picture("image/jpeg", b"\xff\xd8jpeg-bytes");
    assert_eq!(tag_hash(&a), tag_hash(&c));
}

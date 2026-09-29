//! 端末配信の純粋関数（仕様 ③）。DB を使わない

use serde_json::json;
use spindle::domain::derived::{Current, Variant};
use spindle::domain::device::*;

fn row() -> Current {
    Current {
        rel_path: "opus/A/B/1-01 x.opus".into(),
        src_audio_version: 3,
        src_tag_version: 5,
        src_artwork_id: Some(7),
        src_rg_scanned_at: None,
        audio_profile: "opus:256:v1".into(),
        tag_profile: "opus:v1".into(),
    }
}

#[test]
fn canonical_sha256_ignores_key_order_and_whitespace() {
    let a = canonical_sha256(&json!({"b": 1, "a": [1, null]}));
    let b = canonical_sha256(&serde_json::from_str(r#"{ "a" : [1,null], "b":1 }"#).unwrap());
    assert_eq!(a, b);
    assert_eq!(a.len(), 64);
    assert!(a
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
}

#[test]
fn semantic_tokens_differ_by_kind_and_fields() {
    let m = semantic_master(3, 5);
    assert_eq!(m, semantic_master(3, 5));
    assert_ne!(m, semantic_master(3, 6));
    let d = semantic_derived(Variant::Opus, &row());
    assert_ne!(d, m);
    assert_ne!(d, semantic_derived(Variant::Aac, &row()));
    let mut r = row();
    r.src_rg_scanned_at = Some(1);
    assert_ne!(d, semantic_derived(Variant::Opus, &r));
    // rel_path は内容ではないので意味トークンに入らない
    let mut r = row();
    r.rel_path = "opus/other.opus".into();
    assert_eq!(d, semantic_derived(Variant::Opus, &r));
}

#[test]
fn delivery_token_changes_with_sha_even_if_semantic_is_same() {
    let s = semantic_master(1, 1);
    assert_ne!(delivery_token(&s, "aa"), delivery_token(&s, "bb"));
    assert_eq!(delivery_token(&s, "aa"), delivery_token(&s, "aa"));
}

#[test]
fn playlist_token_covers_name_and_content() {
    let t = playlist_token(5, "通勤", "aa");
    assert_ne!(t, playlist_token(5, "通勤2", "aa"));
    assert_ne!(t, playlist_token(5, "通勤", "bb"));
    assert_ne!(t, playlist_token(6, "通勤", "aa"));
}

#[test]
fn sha256_hex_of_empty() {
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

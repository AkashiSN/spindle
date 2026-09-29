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

fn settings(variant: Variant, enabled: bool) -> spindle::domain::derived::VariantSettings {
    spindle::domain::derived::VariantSettings {
        variant,
        enabled,
        audio_profile: "opus:256:v1".into(),
        tag_profile: "opus:v1".into(),
        lossy_sources: variant == Variant::Aac,
        multi_value_separator: " & ".into(),
    }
}

fn input(id: i64, rel: &str, lossless: bool) -> TrackInput {
    TrackInput {
        track_id: id,
        rel_path: rel.into(),
        lossless,
        channels: Some(2),
        audio_version: 3,
        tag_version: 5,
        rg_ready: true,
        derived: None,
        hash_master: None,
        hash_derived: None,
    }
}

#[test]
fn opus_lossy_original_is_sent_as_master() {
    let t = input(1, "YT/a/b/x.opus", false);
    let s = decide_source(&settings(Variant::Opus, true), &t).unwrap();
    assert_eq!(s.kind, SourceKind::Master);
    assert_eq!(s.root_rel_path, "YT/a/b/x.opus");
    assert_eq!(s.semantic, semantic_master(3, 5));
    assert_eq!(dest_path(&t, s.kind), "YT/a/b/x.opus");
}

#[test]
fn up_to_date_derived_is_sent_and_path_keeps_library_layout() {
    let mut t = input(1, "J-Pop/A/B/1-01 x.flac", true);
    t.derived = Some(row());
    let s = decide_source(&settings(Variant::Opus, true), &t).unwrap();
    assert_eq!(s.kind, SourceKind::Derived(Variant::Opus));
    assert_eq!(s.root_rel_path, "opus/A/B/1-01 x.opus");
    assert_eq!(dest_path(&t, s.kind), "J-Pop/A/B/1-01 x.opus");
}

#[test]
fn stale_tags_are_still_sent() {
    let mut t = input(1, "a.flac", true);
    let mut r = row();
    r.src_tag_version = 4; // タグだけ古い（D-25）
    t.derived = Some(r);
    assert!(decide_source(&settings(Variant::Opus, true), &t).is_ok());
}

#[test]
fn stale_audio_or_profile_waits_when_enabled() {
    let mut t = input(1, "a.flac", true);
    let mut r = row();
    r.src_audio_version = 2;
    t.derived = Some(r);
    assert_eq!(
        decide_source(&settings(Variant::Opus, true), &t),
        Err(Wait::AudioStale)
    );
    let mut r = row();
    r.audio_profile = "opus:128:v1".into();
    t.derived = Some(r);
    assert_eq!(
        decide_source(&settings(Variant::Opus, true), &t),
        Err(Wait::AudioStale)
    );
}

#[test]
fn frozen_variant_keeps_old_profile_but_never_serves_stale_audio() {
    let mut t = input(1, "a.flac", true);
    let mut r = row();
    r.audio_profile = "opus:128:v1".into(); // 凍結中は行の profile を有効とみなす
    t.derived = Some(r.clone());
    assert!(decide_source(&settings(Variant::Opus, false), &t).is_ok());
    r.src_audio_version = 2;
    t.derived = Some(r);
    assert_eq!(
        decide_source(&settings(Variant::Opus, false), &t),
        Err(Wait::AudioStale)
    );
}

#[test]
fn missing_derived_waits_with_reason() {
    let t = input(1, "a.flac", true);
    assert_eq!(
        decide_source(&settings(Variant::Opus, true), &t),
        Err(Wait::NoDerived)
    );
    let mut t = input(1, "a.flac", true);
    t.rg_ready = false;
    assert_eq!(
        decide_source(&settings(Variant::Aac, true), &t),
        Err(Wait::RgPending)
    );
    // aac は非可逆も Derived から（原本は送らない）
    let t = input(2, "x.opus", false);
    assert_eq!(
        decide_source(&settings(Variant::Aac, true), &t),
        Err(Wait::NoDerived)
    );
}

#[test]
fn utf16_len_counts_surrogates() {
    assert_eq!(utf16_len("abc"), 3);
    assert_eq!(utf16_len("群青"), 2);
    assert_eq!(utf16_len("𝄞"), 2);
}

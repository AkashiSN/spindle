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

fn hash_for(semantic: &str, sha: &str) -> SourceHash {
    SourceHash {
        semantic: semantic.into(),
        inode: 1,
        size: 10,
        mtime_ns: 0,
        ctime_ns: 0,
        sha256: sha.into(),
    }
}

/// 送れる状態の Derived 曲
fn ready(id: i64, rel: &str) -> TrackInput {
    let mut t = input(id, rel, true);
    let mut r = row();
    r.rel_path = format!("opus/{}", dest_path(&t, SourceKind::Derived(Variant::Opus)));
    t.derived = Some(r.clone());
    t.hash_derived = Some(hash_for(
        &semantic_derived(Variant::Opus, &r),
        &format!("sha{id}"),
    ));
    t
}

#[test]
fn manifest_puts_ready_tracks_in_desired_with_delivery_token() {
    let m = build_manifest(&settings(Variant::Opus, true), 20, &[ready(1, "A/x.flac")]);
    assert_eq!(m.desired.len(), 1);
    let d = &m.desired[0];
    assert_eq!(d.dest_path, "A/x.opus");
    assert_eq!(d.sha256, "sha1");
    assert_eq!(d.size, 10);
    assert_eq!(d.token, delivery_token(&d.source.semantic, "sha1"));
    assert!(m.hold.is_empty());
    assert!(m.needs_hash.is_empty());
}

#[test]
fn missing_or_stale_hash_is_hashing_and_requests_a_job() {
    let mut t = ready(1, "A/x.flac");
    t.hash_derived = None;
    let m = build_manifest(&settings(Variant::Opus, true), 0, &[t.clone()]);
    assert_eq!(m.hold, vec![(1, Hold::Wait(Wait::Hashing))]);
    assert_eq!(m.needs_hash, vec![(1, SourceKind::Derived(Variant::Opus))]);
    // 意味トークンが違うハッシュは古い
    t.hash_derived = Some(hash_for("old", "x"));
    let m = build_manifest(&settings(Variant::Opus, true), 0, &[t]);
    assert_eq!(m.hold, vec![(1, Hold::Wait(Wait::Hashing))]);
}

#[test]
fn colliding_dest_paths_hold_both() {
    // x.flac（Derived → x.opus）と X.opus（原本）は casefold で同じ端末パス
    let a = ready(1, "A/x.flac");
    let mut b = input(2, "A/X.opus", false);
    b.hash_master = Some(hash_for(&semantic_master(3, 5), "m"));
    let c = ready(3, "A/y.flac");
    let m = build_manifest(&settings(Variant::Opus, true), 0, &[a, b, c]);
    assert_eq!(
        m.desired.iter().map(|d| d.track_id).collect::<Vec<_>>(),
        vec![3]
    );
    assert_eq!(
        m.hold,
        vec![
            (1, Hold::Error(ItemError::PathCollision)),
            (2, Hold::Error(ItemError::PathCollision))
        ]
    );
}

#[test]
fn too_long_path_is_an_error() {
    let long = format!("A/{}.flac", "あ".repeat(230));
    let m = build_manifest(&settings(Variant::Opus, true), 20, &[ready(1, &long)]);
    assert_eq!(m.hold, vec![(1, Hold::Error(ItemError::PathTooLong))]);
}

#[test]
fn m3u8_is_relative_from_playlists_dir() {
    let body = render_playlist(Transport::Adb, &[(1, "A/x.opus"), (2, "B/y.opus")]);
    assert_eq!(
        String::from_utf8(body).unwrap(),
        "#EXTM3U\n../A/x.opus\n../B/y.opus\n"
    );
}

#[test]
fn playlist_uses_current_path_for_held_copies_and_skips_absent() {
    let m = build_manifest(&settings(Variant::Opus, true), 0, &[ready(1, "A/x.flac")]);
    // 2 は hold だが端末に古い写しがある、3 は端末に無い
    let mut held = m.clone();
    held.hold.push((2, Hold::Wait(Wait::NoDerived)));
    held.hold.push((3, Hold::Wait(Wait::NoDerived)));
    let current = vec![DeviceItem {
        track_id: 2,
        dest_path: "Old/z.opus".into(),
        token: "t".into(),
        size: 1,
        sha256: "s".into(),
    }];
    let pl = vec![PlaylistInput {
        playlist_id: 9,
        name: "通勤".into(),
        track_ids: vec![3, 2, 1],
    }];
    let (out, errors) = build_playlists(Transport::Adb, &pl, &held, &current);
    assert!(errors.is_empty());
    assert_eq!(out[0].dest_path, "Playlists/通勤.m3u8");
    assert_eq!(
        String::from_utf8(out[0].body.clone()).unwrap(),
        "#EXTM3U\n../Old/z.opus\n../A/x.opus\n"
    );
    assert_eq!(
        out[0].token,
        playlist_token(9, "通勤", &sha256_hex(&out[0].body))
    );
}

#[test]
fn playlist_name_collisions_and_reserved_names_are_errors() {
    let m = Manifest::default();
    let pl = vec![
        PlaylistInput {
            playlist_id: 1,
            name: "Drive".into(),
            track_ids: vec![],
        },
        PlaylistInput {
            playlist_id: 2,
            name: "drive".into(),
            track_ids: vec![],
        },
        PlaylistInput {
            playlist_id: 3,
            name: ".spindle".into(),
            track_ids: vec![],
        },
        PlaylistInput {
            playlist_id: 4,
            name: "ok".into(),
            track_ids: vec![],
        },
    ];
    let (out, errors) = build_playlists(Transport::Adb, &pl, &m, &[]);
    assert_eq!(
        out.iter().map(|p| p.playlist_id).collect::<Vec<_>>(),
        vec![4]
    );
    assert_eq!(
        errors.iter().map(|e| e.0).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

fn cur(id: i64, path: &str, token: &str) -> DeviceItem {
    DeviceItem {
        track_id: id,
        dest_path: path.into(),
        token: token.into(),
        size: 1,
        sha256: "s".into(),
    }
}

fn ops(d: &Diff) -> Vec<(OpKind, i64)> {
    d.items.iter().map(|o| (o.kind, o.track_id)).collect()
}

#[test]
fn classifies_add_update_move_update_move_and_delete() {
    let s = settings(Variant::Opus, true);
    let m = build_manifest(
        &s,
        0,
        &[
            ready(1, "A/a.flac"),
            ready(2, "A/b.flac"),
            ready(3, "A/c.flac"),
            ready(4, "A/d.flac"),
        ],
    );
    let t = |id: i64| {
        m.desired
            .iter()
            .find(|d| d.track_id == id)
            .unwrap()
            .token
            .clone()
    };
    let current = vec![
        cur(2, "A/b.opus", "old"),   // 更新
        cur(3, "Old/c.opus", &t(3)), // 移動
        cur(4, "Old/d.opus", "old"), // 更新 + 移動
        cur(9, "Z/z.opus", "x"),     // 削除（選曲外）
    ];
    let d = diff(&m, &current, &[], &[], vec![]);
    assert_eq!(
        ops(&d),
        vec![
            (OpKind::Delete, 9),
            (OpKind::Move, 3),
            (OpKind::UpdateMove, 4),
            (OpKind::Update, 2),
            (OpKind::Add, 1),
        ]
    );
    let um = d.items.iter().find(|o| o.track_id == 4).unwrap();
    assert_eq!(um.from.as_deref(), Some("Old/d.opus"));
    assert_eq!(um.to.as_deref(), Some("A/d.opus"));
}

#[test]
fn same_semantic_new_sha_is_update() {
    let s = settings(Variant::Opus, true);
    let t = ready(1, "A/a.flac");
    let m = build_manifest(&s, 0, std::slice::from_ref(&t));
    let old = delivery_token(&t.hash_derived.as_ref().unwrap().semantic, "previous-sha");
    let d = diff(&m, &[cur(1, "A/a.opus", &old)], &[], &[], vec![]);
    assert_eq!(ops(&d), vec![(OpKind::Update, 1)]);
}

#[test]
fn hold_items_are_never_deleted() {
    let s = settings(Variant::Opus, true);
    let mut waiting = input(1, "A/a.flac", true); // Derived 未生成
    waiting.derived = None;
    let m = build_manifest(&s, 0, &[waiting]);
    let d = diff(&m, &[cur(1, "A/a.opus", "old")], &[], &[], vec![]);
    assert!(d.items.is_empty(), "hold の写しは消さない: {:?}", d.items);
    assert_eq!(
        d.held,
        vec![HeldItem {
            track_id: 1,
            hold: Hold::Wait(Wait::NoDerived),
            has_copy: true
        }]
    );
}

#[test]
fn destination_occupied_by_a_stationary_item_is_a_collision() {
    let s = settings(Variant::Opus, true);
    // 1 は A/b.opus へ移りたいが、2（hold で動かない）がそこにいる
    let m1 = build_manifest(&s, 0, &[ready(1, "A/b.flac")]);
    let mut m = m1.clone();
    m.hold.push((2, Hold::Wait(Wait::NoDerived)));
    let t1 = m.desired[0].token.clone();
    let d = diff(
        &m,
        &[cur(1, "A/a.opus", &t1), cur(2, "A/b.opus", "x")],
        &[],
        &[],
        vec![],
    );
    assert!(d.items.is_empty());
    assert!(d.held.contains(&HeldItem {
        track_id: 1,
        hold: Hold::Error(ItemError::PathCollision),
        has_copy: true
    }));
}

#[test]
fn swap_is_two_moves() {
    let s = settings(Variant::Opus, true);
    let m = build_manifest(&s, 0, &[ready(1, "A/b.flac"), ready(2, "A/a.flac")]);
    let t = |id: i64| {
        m.desired
            .iter()
            .find(|d| d.track_id == id)
            .unwrap()
            .token
            .clone()
    };
    let d = diff(
        &m,
        &[cur(1, "A/a.opus", &t(1)), cur(2, "A/b.opus", &t(2))],
        &[],
        &[],
        vec![],
    );
    assert_eq!(ops(&d), vec![(OpKind::Move, 1), (OpKind::Move, 2)]);
}

#[test]
fn swap_with_one_side_held_holds_both() {
    let s = settings(Variant::Opus, true);
    // 1: a→b に移りたい。2: b→a に移りたいが、2 の新しい行き先は別の曲 3（動かない）と衝突
    let m0 = build_manifest(&s, 0, &[ready(1, "A/b.flac"), ready(2, "A/a.flac")]);
    let t = |id: i64| {
        m0.desired
            .iter()
            .find(|d| d.track_id == id)
            .unwrap()
            .token
            .clone()
    };
    let mut m = m0.clone();
    // 2 をエラーで保留にする（例: 名前の衝突）
    m.desired.retain(|d| d.track_id != 2);
    m.hold.push((2, Hold::Error(ItemError::PathCollision)));
    let d = diff(
        &m,
        &[cur(1, "A/a.opus", &t(1)), cur(2, "A/b.opus", &t(2))],
        &[],
        &[],
        vec![],
    );
    // 2 は動かず A/b.opus に居続けるので、1 は b へ移れない
    assert!(d.items.is_empty(), "{:?}", d.items);
    assert!(d
        .held
        .iter()
        .any(|h| h.track_id == 1 && h.hold == Hold::Error(ItemError::PathCollision)));
}

#[test]
fn playlists_are_diffed_by_token_and_removed_marks_are_deleted() {
    let pl = vec![DesiredPlaylist {
        playlist_id: 1,
        name: "a".into(),
        dest_path: "Playlists/a.m3u8".into(),
        body: vec![],
        token: "new".into(),
    }];
    let cur_pl = vec![
        PlaylistState {
            playlist_id: 1,
            dest_path: "Playlists/a.m3u8".into(),
            token: "old".into(),
        },
        PlaylistState {
            playlist_id: 2,
            dest_path: "Playlists/b.m3u8".into(),
            token: "t".into(),
        },
    ];
    let d = diff(&Manifest::default(), &[], &pl, &cur_pl, vec![]);
    let kinds: Vec<_> = d
        .playlists
        .iter()
        .map(|p| (p.kind, p.playlist_id))
        .collect();
    assert_eq!(
        kinds,
        vec![(PlaylistOpKind::Update, 1), (PlaylistOpKind::Delete, 2)]
    );
}

#[test]
fn plan_token_changes_with_generation_and_ops() {
    let s = settings(Variant::Opus, true);
    let m = build_manifest(&s, 0, &[ready(1, "A/a.flac")]);
    let d = diff(&m, &[], &[], &[], vec![]);
    let p = plan_token(1, &d);
    assert_eq!(p, plan_token(1, &d));
    assert_ne!(p, plan_token(2, &d));
    let d2 = diff(&m, &[cur(9, "z.opus", "x")], &[], &[], vec![]);
    assert_ne!(p, plan_token(1, &d2));
}

/// 行き先が動かない保留の曲に占められて保留になった曲は、プレイリストに行き先のパスで載せない。
/// 既存の写しがあればその現在のパス、無ければ載せない（`resolve_collisions` を通してから組み立てる）
#[test]
fn playlist_after_collision_uses_copy_path_or_omits_blocked_track() {
    let s = settings(Variant::Opus, true);
    let mut m = build_manifest(&s, 0, &[ready(1, "A/b.flac")]);
    m.hold.push((2, Hold::Wait(Wait::NoDerived))); // 2 は A/b.opus に居続ける
    let t1 = m.desired[0].token.clone();
    let pl = vec![PlaylistInput {
        playlist_id: 9,
        name: "p".into(),
        track_ids: vec![1, 2],
    }];

    // (a) 1 に既存の写し（A/a.opus）がある
    let current = vec![cur(1, "A/a.opus", &t1), cur(2, "A/b.opus", "x")];
    let resolved = resolve_collisions(&m, &current);
    assert!(resolved.desired.is_empty());
    assert!(resolved
        .hold
        .contains(&(1, Hold::Error(ItemError::PathCollision))));
    let (out, _) = build_playlists(Transport::Adb, &pl, &resolved, &current);
    assert_eq!(
        String::from_utf8(out[0].body.clone()).unwrap(),
        "#EXTM3U\n../A/a.opus\n../A/b.opus\n",
        "1 は塞がれた行き先ではなく今の写しのパスで載る"
    );

    // (b) 1 の写しが無い
    let current = vec![cur(2, "A/b.opus", "x")];
    let resolved = resolve_collisions(&m, &current);
    let (out, _) = build_playlists(Transport::Adb, &pl, &resolved, &current);
    assert_eq!(
        String::from_utf8(out[0].body.clone()).unwrap(),
        "#EXTM3U\n../A/b.opus\n",
        "写しの無い保留の曲は載せない"
    );
}

#[test]
fn resolve_collisions_is_idempotent_and_keeps_needs_hash() {
    let s = settings(Variant::Opus, true);
    let mut unhashed = ready(3, "B/c.flac");
    unhashed.hash_derived = None;
    let mut m = build_manifest(&s, 0, &[ready(1, "A/b.flac"), unhashed]);
    m.hold.push((2, Hold::Wait(Wait::NoDerived)));
    m.hold.sort_by_key(|(id, _)| *id);
    let current = vec![cur(1, "A/a.opus", "t"), cur(2, "A/b.opus", "x")];
    let once = resolve_collisions(&m, &current);
    assert_eq!(once.needs_hash, m.needs_hash);
    assert!(!once.needs_hash.is_empty());
    assert_eq!(resolve_collisions(&once, &current), once);
}

/// 大文字小文字だけの改名は、同じ曲が同じキーを占めるので衝突ではなく移動
#[test]
fn case_only_rename_is_a_move_not_a_collision() {
    let s = settings(Variant::Opus, true);
    let m = build_manifest(&s, 0, &[ready(1, "A/X.flac")]);
    let t1 = m.desired[0].token.clone();
    let d = diff(&m, &[cur(1, "A/x.opus", &t1)], &[], &[], vec![]);
    assert_eq!(ops(&d), vec![(OpKind::Move, 1)]);
    assert!(d.held.is_empty(), "{:?}", d.held);
    let mv = &d.items[0];
    assert_eq!(mv.from.as_deref(), Some("A/x.opus"));
    assert_eq!(mv.to.as_deref(), Some("A/X.opus"));
}

fn op(kind: OpKind, id: i64, from: Option<&str>, to: Option<&str>, size: u64) -> ItemOp {
    ItemOp {
        kind,
        track_id: id,
        from: from.map(str::to_owned),
        to: to.map(str::to_owned),
        token: to.map(|_| "tok".to_owned()),
        size,
        sha256: to.map(|_| "ab".repeat(32)),
    }
}

fn item(id: i64, path: &str, size: u64) -> DeviceItem {
    DeviceItem {
        track_id: id,
        dest_path: path.into(),
        token: "old".into(),
        size,
        sha256: "cd".repeat(32),
    }
}

#[test]
fn track_states_classify_every_kind() {
    let diff = Diff {
        items: vec![
            op(OpKind::Delete, 1, Some("a"), None, 0),
            op(OpKind::Move, 2, Some("b"), Some("b2"), 10),
            op(OpKind::Update, 3, None, Some("c"), 10),
            op(OpKind::Add, 4, None, Some("d"), 10),
        ],
        held: vec![
            HeldItem {
                track_id: 5,
                hold: Hold::Wait(Wait::Hashing),
                has_copy: true,
            },
            HeldItem {
                track_id: 6,
                hold: Hold::Error(ItemError::PathCollision),
                has_copy: false,
            },
        ],
        ..Default::default()
    };
    let current = vec![
        item(1, "a", 1),
        item(2, "b", 1),
        item(3, "c", 1),
        item(5, "e", 1),
        item(7, "f", 1),
    ];
    let synced: std::collections::HashMap<i64, i64> = [(7, 100)].into_iter().collect();
    let s = track_states(&diff, &current, &[(4, "前回 No space".into())], &synced);
    assert_eq!(s[&1], TrackState::Removing);
    assert_eq!(
        s[&2],
        TrackState::Pending {
            op: "move",
            reason: None
        }
    );
    assert_eq!(
        s[&3],
        TrackState::Pending {
            op: "update",
            reason: None
        }
    );
    assert_eq!(
        s[&4],
        TrackState::Pending {
            op: "add",
            reason: Some("前回 No space".into())
        }
    );
    assert_eq!(
        s[&5],
        TrackState::Waiting {
            reason: "ハッシュ計算中",
            has_copy: true
        }
    );
    assert_eq!(
        s[&6],
        TrackState::Error {
            reason: "パス衝突".into(),
            has_copy: false
        }
    );
    assert_eq!(
        s[&7],
        TrackState::Synced {
            synced_at: Some(100)
        }
    );
    let c = counts(&s);
    assert_eq!(
        (c.add, c.update, c.r#move, c.delete, c.waiting, c.error, c.synced),
        (1, 1, 1, 1, 1, 1, 1)
    );
}

#[test]
fn held_track_with_copy_is_not_removing() {
    let diff = Diff {
        held: vec![HeldItem {
            track_id: 9,
            hold: Hold::Error(ItemError::PathTooLong),
            has_copy: true,
        }],
        ..Default::default()
    };
    let s = track_states(&diff, &[item(9, "x", 1)], &[], &Default::default());
    assert_eq!(
        s[&9],
        TrackState::Error {
            reason: "パスが長すぎる".into(),
            has_copy: true
        }
    );
}

#[test]
fn reported_error_on_synced_track_is_error() {
    let s = track_states(
        &Diff::default(),
        &[item(3, "c", 1)],
        &[(3, "転送に失敗".into())],
        &Default::default(),
    );
    assert_eq!(
        s[&3],
        TrackState::Error {
            reason: "転送に失敗".into(),
            has_copy: true
        }
    );
}

#[test]
fn estimate_follows_execution_order() {
    // 現状: 1(100) を削除、2(50) を更新 + 移動で 80 に、3(30) を更新で 40 に、4 を 20 で追加
    let current = vec![item(1, "a", 100), item(2, "b", 50), item(3, "c", 30)];
    let diff = Diff {
        items: vec![
            op(OpKind::Delete, 1, Some("a"), None, 0),
            op(OpKind::UpdateMove, 2, Some("b"), Some("b2"), 80),
            op(OpKind::Update, 3, None, Some("c"), 40),
            op(OpKind::Add, 4, None, Some("d"), 20),
        ],
        ..Default::default()
    };
    let e = estimate(&diff, &current);
    assert_eq!(e.transfer_bytes, 80 + 40 + 20);
    // 削除で -100 → prepared で +80（-20）→ vacating で -50（-70）→ 更新 tmp +40（-30）→ 旧 -30（-60）→ 追加 +20（-40）
    // 今より増えることは無いので 0
    assert_eq!(e.peak_bytes, 0);
    let only_adds = Diff {
        items: vec![
            op(OpKind::Add, 4, None, Some("d"), 20),
            op(OpKind::Add, 5, None, Some("e"), 5),
        ],
        ..Default::default()
    };
    assert_eq!(estimate(&only_adds, &[]).peak_bytes, 25);
    let update_peak = Diff {
        items: vec![op(OpKind::Update, 3, None, Some("c"), 40)],
        ..Default::default()
    };
    assert_eq!(
        estimate(&update_peak, &current).peak_bytes,
        40,
        "tmp と旧版が並ぶ瞬間"
    );
}

#[test]
fn pending_sets_lookup_defaults_to_empty() {
    let p = PendingSets::default();
    assert!(p.for_key("iphone").is_empty());
    assert!(p.for_id(1).is_empty());
}

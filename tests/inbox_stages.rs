//! Inbox の承認済みの件の段（可視化 B）

use rusqlite::{params, Connection};
use spindle::config::{DerivedConfig, OpusVariantConfig};
use spindle::db::derived::{self, Profiles, TagState};
use spindle::db::devices::Snapshot;
use spindle::db::inbox::{self as dbinbox, ItemState};
use spindle::db::open_memory_connection;
use spindle::db::stages::{stages, status_of, StageStatus};
use spindle::domain::derived::Variant;

fn empty() -> Snapshot {
    Snapshot { devices: vec![] }
}

#[test]
fn status_rules() {
    assert_eq!(status_of(0, 0, false), StageStatus::Na);
    assert_eq!(status_of(3, 3, false), StageStatus::Done);
    assert_eq!(status_of(1, 3, false), StageStatus::Running);
    assert_eq!(status_of(0, 3, true), StageStatus::Running);
    assert_eq!(status_of(0, 3, false), StageStatus::Todo);
}

fn conn_with_tracks() -> Connection {
    let c = open_memory_connection().unwrap();
    for id in 1..=2 {
        c.execute(
            "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless, channels,
                                 audio_version, tag_version, seen_at, rg_scanned_at)
             VALUES (?1, ?2, ?2, 1, 0, 0, 'flac', 1, 2, 1, 1, 0, CASE WHEN ?1 = 1 THEN 5 END)",
            params![id, format!("a/{id}.flac")],
        )
        .unwrap();
    }
    c
}

#[test]
fn placed_item_counts_rg_and_omits_unconfigured_variants() {
    let c = conn_with_tracks();
    let got = stages(&c, ItemState::Placed, &[1, 2], &empty()).unwrap();
    let by = |k: &str| got.iter().find(|s| s.key == k).unwrap().clone();
    assert_eq!(by("place").status, StageStatus::Done);
    let rg = by("rg");
    assert_eq!((rg.done, rg.total, rg.status), (1, 2, StageStatus::Running));
    // derived_variants が空（系統が設定に無い）なら系統の段は出さない
    assert!(got.iter().all(|s| s.key != "opus" && s.key != "aac"));
}

#[test]
fn enabled_variant_counts_only_tracks_with_matching_derived_row() {
    let c = conn_with_tracks();
    let cfg = DerivedConfig {
        opus: OpusVariantConfig {
            enabled: true,
            bitrate: 256,
        },
        aac: Default::default(),
    };
    derived::sync_variants(&c, &cfg, 0).unwrap();
    let pr = Profiles::of(&derived::settings_of(&c, Variant::Opus).unwrap().unwrap());
    // 曲 1 だけ、現在の世代（RG 解析の世代も含む）で作ってある
    let tags = TagState {
        src_tag_version: 1,
        src_artwork_id: None,
        src_rg_scanned_at: Some(5),
    };
    derived::upsert(
        &c,
        1,
        Variant::Opus,
        "opus/a/1.opus",
        Some(256),
        1,
        tags,
        &pr,
        0,
    )
    .unwrap();

    let got = stages(&c, ItemState::Placed, &[1, 2], &empty()).unwrap();
    let by = |k: &str| got.iter().find(|s| s.key == k).unwrap().clone();
    let opus = by("opus");
    assert_eq!((opus.done, opus.total), (1, 2));
    assert_eq!(opus.status, StageStatus::Running);
    // aac は節省略の既定（無効）
    let aac = by("aac");
    assert_eq!(aac.status, StageStatus::Na);
    assert!(aac.label.contains("無効"));
}

#[test]
fn approved_item_shows_only_place_todo() {
    let c = conn_with_tracks();
    let got = stages(&c, ItemState::Approved, &[], &empty()).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].key, "place");
    assert_eq!(got[0].status, StageStatus::Todo);
}

#[test]
fn placed_tracks_are_replaced_and_cascade() {
    let c = conn_with_tracks();
    c.execute(
        "INSERT INTO inbox_items (id, rel_dir, rel_dir_key, state, detected_at, seen_at)
         VALUES (1, 'x', 'x', 'placed', 0, 0)",
        [],
    )
    .unwrap();
    dbinbox::set_placed_tracks(&c, 1, &[1, 2]).unwrap();
    dbinbox::set_placed_tracks(&c, 1, &[2]).unwrap();
    assert_eq!(dbinbox::placed_tracks(&c, 1).unwrap(), vec![2]);
    c.execute("DELETE FROM inbox_items WHERE id = 1", [])
        .unwrap();
    assert!(dbinbox::placed_tracks(&c, 1).unwrap().is_empty());
}

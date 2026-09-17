//! `playlists` / `playlist_items` / `playlist_exports` の読み書き（docs/TASKS.md P1-6、D-53）。
//! `:memory:` にマイグレーションを流し、tracks を直接 INSERT する

use rusqlite::{params, Connection};

use spindle::db::open_memory_connection;
use spindle::db::playlists::{self, MoveError, Rename};
use spindle::playlist::export::Source;

fn conn() -> Connection {
    open_memory_connection().unwrap()
}

fn insert_track(c: &Connection, id: i64, rel: &str, missing: bool) {
    c.execute(
        "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                             title, artist_display, duration_ms, audio_version, tag_version, seen_at, missing_since)
         VALUES (?1, ?2, lower(?2), 1, 0, 0, 'flac', 1, 'T' || ?1, 'A', 1000 * ?1, 1, 1, 0, ?3)",
        params![id, rel, if missing { Some(1) } else { None }],
    )
    .unwrap();
}

fn items(c: &Connection, id: i64) -> Vec<i64> {
    playlists::items(c, id).unwrap()
}

fn positions(c: &Connection, id: i64) -> Vec<i64> {
    let mut st = c
        .prepare("SELECT position FROM playlist_items WHERE playlist_id = ? ORDER BY position")
        .unwrap();
    st.query_map([id], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn setup() -> (Connection, i64) {
    let c = conn();
    for i in 1..=5 {
        insert_track(&c, i, &format!("A/{i}.flac"), false);
    }
    insert_track(&c, 9, "A/9.flac", true);
    let p = playlists::create(&c, "通勤", 100).unwrap().unwrap();
    (c, p.id)
}

#[test]
fn create_returns_manual_playlist_and_rejects_duplicate_name() {
    let c = conn();
    let p = playlists::create(&c, "通勤", 100).unwrap().unwrap();
    assert_eq!(p.name, "通勤");
    assert_eq!(p.kind, "manual");
    assert_eq!((p.created_at, p.updated_at), (100, 100));
    assert!(playlists::create(&c, "通勤", 101).unwrap().is_none());
    assert_eq!(playlists::list(&c).unwrap().len(), 1);
}

#[test]
fn names_that_collide_on_the_filesystem_are_duplicates() {
    // ZFS の insensitive + formD では Foo.m3u8 と foo.m3u8、NFC と NFD の同名が同じ実体。
    // 名前の一意性は rel_path_key と同じ canonical key で判定する（D-53）
    let c = conn();
    let p = playlists::create(&c, "Foo", 100).unwrap().unwrap();
    assert!(playlists::create(&c, "foo", 101).unwrap().is_none());
    assert!(playlists::create(&c, "FOO", 101).unwrap().is_none());
    let nfc = "\u{304c}"; // が
    let nfd = "\u{304b}\u{3099}"; // か + 濁点
    playlists::create(&c, nfc, 102).unwrap().unwrap();
    assert!(playlists::create(&c, nfd, 103).unwrap().is_none());
    assert_eq!(
        playlists::rename(&c, p.id, nfd, 200).unwrap(),
        Rename::Duplicate
    );
    // 自分自身の大小文字違いへの改名は通る（同じ実体）
    assert_eq!(playlists::rename(&c, p.id, "FOO", 200).unwrap(), Rename::Ok);
    assert_eq!(playlists::get(&c, p.id).unwrap().unwrap().name, "FOO");
    let key: String = c
        .query_row("SELECT name_key FROM playlists WHERE id = ?", [p.id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(key, "foo");
}

#[test]
fn append_keeps_order_and_skips_already_present_and_unknown_tracks() {
    let (c, id) = setup();
    let r = playlists::append(&c, id, &[3, 1, 3, 42], 200)
        .unwrap()
        .unwrap();
    assert_eq!((r.added, r.skipped), (2, 2));
    assert_eq!(items(&c, id), vec![3, 1]);
    let r = playlists::append(&c, id, &[1, 2], 201).unwrap().unwrap();
    assert_eq!((r.added, r.skipped), (1, 1));
    assert_eq!(items(&c, id), vec![3, 1, 2]);
    assert_eq!(positions(&c, id), vec![0, 1, 2]);
    assert_eq!(playlists::get(&c, id).unwrap().unwrap().updated_at, 201);
}

#[test]
fn append_to_unknown_playlist_is_none() {
    let (c, _) = setup();
    assert!(playlists::append(&c, 999, &[1], 200).unwrap().is_none());
}

#[test]
fn remove_renumbers_positions() {
    let (c, id) = setup();
    playlists::append(&c, id, &[1, 2, 3, 4], 200).unwrap();
    let n = playlists::remove(&c, id, &[2, 4, 42], 300)
        .unwrap()
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(items(&c, id), vec![1, 3]);
    assert_eq!(positions(&c, id), vec![0, 1]);
}

#[test]
fn move_places_tracks_before_target_keeping_their_relative_order() {
    let (c, id) = setup();
    playlists::append(&c, id, &[1, 2, 3, 4, 5], 200).unwrap();
    // 5 と 2 を 1 の前へ。移動する側は現在の並び（2, 5）を保つ
    playlists::move_items(&c, id, &[5, 2], Some(1), 300)
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(items(&c, id), vec![2, 5, 1, 3, 4]);
    assert_eq!(positions(&c, id), vec![0, 1, 2, 3, 4]);
}

#[test]
fn move_to_end_when_before_is_none() {
    let (c, id) = setup();
    playlists::append(&c, id, &[1, 2, 3], 200).unwrap();
    playlists::move_items(&c, id, &[1], None, 300)
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(items(&c, id), vec![2, 3, 1]);
}

#[test]
fn move_rejects_target_outside_or_inside_the_moved_set() {
    let (c, id) = setup();
    playlists::append(&c, id, &[1, 2, 3], 200).unwrap();
    assert_eq!(
        playlists::move_items(&c, id, &[1], Some(42), 300).unwrap(),
        Some(Err(MoveError::BeforeNotInPlaylist))
    );
    assert_eq!(
        playlists::move_items(&c, id, &[1, 2], Some(2), 300).unwrap(),
        Some(Err(MoveError::BeforeInMovedSet))
    );
    // 集合に無いトラックは無視する（何も動かない）
    playlists::move_items(&c, id, &[42], Some(1), 300)
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(items(&c, id), vec![1, 2, 3]);
}

#[test]
fn rename_updates_name_and_rejects_duplicate_or_unknown() {
    let c = conn();
    let a = playlists::create(&c, "a", 100).unwrap().unwrap();
    playlists::create(&c, "b", 100).unwrap().unwrap();
    assert_eq!(playlists::rename(&c, a.id, "c", 200).unwrap(), Rename::Ok);
    assert_eq!(playlists::get(&c, a.id).unwrap().unwrap().name, "c");
    assert_eq!(
        playlists::rename(&c, a.id, "b", 200).unwrap(),
        Rename::Duplicate
    );
    assert_eq!(
        playlists::rename(&c, 999, "z", 200).unwrap(),
        Rename::NotFound
    );
}

#[test]
fn delete_cascades_items_and_exports() {
    let (c, id) = setup();
    playlists::append(&c, id, &[1, 2], 200).unwrap();
    let internal = playlists::profile_by_name(&c, "internal").unwrap().unwrap();
    playlists::record_export(&c, id, internal.id, "internal/通勤.m3u8", 300).unwrap();
    assert!(playlists::delete(&c, id).unwrap());
    assert!(!playlists::delete(&c, id).unwrap());
    let n: i64 = c
        .query_row("SELECT count(*) FROM playlist_items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
    let n: i64 = c
        .query_row("SELECT count(*) FROM playlist_exports", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn list_counts_active_tracks_and_carries_export_records() {
    let (c, id) = setup();
    playlists::append(&c, id, &[1, 2, 9], 200).unwrap();
    let internal = playlists::profile_by_name(&c, "internal").unwrap().unwrap();
    playlists::record_export(&c, id, internal.id, "internal/通勤.m3u8", 300).unwrap();
    let rows = playlists::list(&c).unwrap();
    assert_eq!(rows.len(), 1);
    let p = &rows[0];
    assert_eq!(p.track_count, 3);
    assert_eq!(p.missing_count, 1);
    assert_eq!(p.duration_ms, 3000);
    assert_eq!(p.exports.len(), 1);
    assert_eq!(p.exports[0].profile, "internal");
    assert_eq!(p.exports[0].out_path, "internal/通勤.m3u8");
    assert_eq!(p.exports[0].exported_at, Some(300));
    // 同じプロファイルの再書き出しは上書き
    playlists::record_export(&c, id, internal.id, "internal/通勤.m3u8", 400).unwrap();
    let rows = playlists::list(&c).unwrap();
    assert_eq!(rows[0].exports[0].exported_at, Some(400));
}

#[test]
fn export_tracks_skip_missing_and_follow_source() {
    let (c, id) = setup();
    // 1 は Derived が現在値、2 は Derived の音声版が不一致（再エンコード待ち）、3 は Derived 無し、
    // 4 は Derived のタグ版だけ不一致（配るが stale_tags に数える）
    c.execute(
        "INSERT INTO derived_files (track_id, rel_path, rel_path_key, codec, src_audio_version, src_tag_version, generated_at)
         VALUES (1, 'A/1.opus', 'a/1.opus', 'opus', 1, 1, 0),
                (2, 'A/2.opus', 'a/2.opus', 'opus', 2, 1, 0),
                (4, 'A/4.opus', 'a/4.opus', 'opus', 1, 2, 0)",
        [],
    )
    .unwrap();
    playlists::append(&c, id, &[1, 9, 2, 3, 4], 200).unwrap();
    let set = playlists::export_tracks(&c, id, Source::Master)
        .unwrap()
        .unwrap();
    assert_eq!(set.skipped_missing, 1);
    // master は Derived を見ないので stale_tags は常に 0
    assert_eq!(set.stale_tags, 0);
    let paths: Vec<&str> = set.tracks.iter().map(|t| t.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "Library/A/1.flac",
            "Library/A/2.flac",
            "Library/A/3.flac",
            "Library/A/4.flac"
        ]
    );
    assert_eq!(set.tracks[0].title.as_deref(), Some("T1"));
    assert_eq!(set.tracks[0].artist.as_deref(), Some("A"));
    assert_eq!(set.tracks[0].duration_ms, Some(1000));
    let set = playlists::export_tracks(&c, id, Source::Delivery)
        .unwrap()
        .unwrap();
    let paths: Vec<&str> = set.tracks.iter().map(|t| t.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "Derived/A/1.opus",
            "Library/A/2.flac",
            "Library/A/3.flac",
            "Derived/A/4.opus"
        ]
    );
    assert_eq!(set.skipped_missing, 1);
    assert_eq!(set.stale_tags, 1);
    assert!(playlists::export_tracks(&c, 999, Source::Master)
        .unwrap()
        .is_none());
}

#[test]
fn profiles_are_seeded_and_parse_into_export_profile() {
    let c = conn();
    let all = playlists::profiles(&c).unwrap();
    let names: Vec<&str> = all.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["foobar", "android", "internal"]);
    let fb = playlists::profile_by_name(&c, "foobar").unwrap().unwrap();
    assert_eq!(fb.profile.path_prefix.as_deref(), Some(r"\\TRUENAS\music\"));
    assert_eq!(fb.profile.path_sep, r"\");
    assert!(playlists::profile_by_name(&c, "nope").unwrap().is_none());
}

#[test]
fn import_candidates_cover_all_tracks_with_active_flag() {
    let (c, _) = setup();
    let cands = playlists::import_candidates(&c).unwrap();
    assert_eq!(cands.len(), 6);
    let missing = cands.iter().find(|x| x.track_id == 9).unwrap();
    assert!(!missing.active);
    assert_eq!(missing.rel_path_key, "a/9.flac");
}

#[test]
fn foobar_prefix_is_synced_from_config() {
    let c = conn();
    // 起動時に config の [export].fb2k_prefix で foobar 行の path_prefix を上書きする（D-55）
    let changed = playlists::sync_foobar_prefix(&c, r"\\nas\share\music\").unwrap();
    assert!(changed);
    let p = playlists::profile_by_name(&c, "foobar").unwrap().unwrap();
    assert_eq!(
        p.profile.path_prefix.as_deref(),
        Some(r"\\nas\share\music\")
    );
    // 同じ値なら変更なし
    assert!(!playlists::sync_foobar_prefix(&c, r"\\nas\share\music\").unwrap());
    // 他のプロファイルは触らない
    let android = playlists::profile_by_name(&c, "android").unwrap().unwrap();
    assert_eq!(android.profile.path_prefix, None);
}

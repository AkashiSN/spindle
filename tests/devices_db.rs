//! 端末配信の DB 層（仕様 ②③）。`:memory:` に全マイグレーションを流して確かめる

use rusqlite::{params, Connection};
use spindle::config::{DerivedConfig, OpusVariantConfig};
use spindle::db::devices::{self, NewDevice, Selection};
use spindle::db::{derived, open_memory_connection};
use spindle::domain::derived::Variant;
use spindle::domain::device::*;

fn conn() -> Connection {
    let c = open_memory_connection().unwrap();
    let cfg = DerivedConfig {
        opus: OpusVariantConfig {
            enabled: true,
            bitrate: 256,
        },
        aac: Default::default(),
    };
    derived::sync_variants(&c, &cfg, false, 0).unwrap();
    c
}

/// `inode` は既定で NULL（未設定なら物理同一性なしのトラック）。ハッシュを有効なものにしたいテストは
/// `inode = Some(id)` を渡し、`SourceHash` 側の identity（size/mtime/ctime）をそれに合わせる
fn insert_track_with_inode(c: &Connection, id: i64, rel: &str, codec: &str, inode: Option<i64>) {
    let lossless = i64::from(!matches!(codec, "opus" | "mp3" | "aac" | "ogg"));
    c.execute(
        "INSERT INTO tracks (id, rel_path, rel_path_key, inode, size, mtime_ns, ctime_ns, codec, lossless,
                             channels, audio_version, tag_version, seen_at)
         VALUES (?1, ?2, lower(?2), ?5, 1, 0, 0, ?3, ?4, 2, 1, 1, 0)",
        params![id, rel, codec, lossless, inode],
    )
    .unwrap();
}

fn insert_track(c: &Connection, id: i64, rel: &str, codec: &str) {
    insert_track_with_inode(c, id, rel, codec, None);
}

fn adb(c: &Connection, selection: Selection) -> devices::Device {
    devices::create(
        c,
        &NewDevice {
            name: "Xperia",
            transport: Transport::Adb,
            variant: Variant::Opus,
            selection,
            adb: Some(("SER1", "emulated", "Music/spindle")),
        },
        10,
    )
    .unwrap()
}

#[test]
fn create_assigns_random_uuid_and_rejects_duplicate_names() {
    let c = conn();
    let d = adb(&c, Selection::All);
    assert_eq!(d.uuid.len(), 32);
    assert!(d.uuid.chars().all(|ch| ch.is_ascii_hexdigit()));
    assert_eq!(d.generation, 1);
    let dup = devices::create(
        &c,
        &NewDevice {
            name: "xperia",
            transport: Transport::Agent,
            variant: Variant::Aac,
            selection: Selection::Playlists,
            adb: None,
        },
        10,
    );
    assert!(dup.is_err(), "名前は canonical_key で一意");
}

#[test]
fn set_playlists_bumps_generation() {
    let c = conn();
    let d = adb(&c, Selection::Playlists);
    c.execute("INSERT INTO playlists (id, name, name_key, created_at, updated_at) VALUES (5, 'p', 'p', 0, 0)", []).unwrap();
    devices::set_playlists(&c, d.id, &[5], 20).unwrap();
    assert_eq!(devices::playlist_ids(&c, d.id).unwrap(), vec![5]);
    assert_eq!(devices::get(&c, d.id).unwrap().unwrap().generation, 2);
}

#[test]
fn compute_marks_unhashed_tracks_and_enqueues_jobs_once() {
    let c = conn();
    let d = adb(&c, Selection::All);
    insert_track(&c, 1, "YT/a.opus", "opus"); // 原本を送る
    let got = devices::compute(&c, d.id).unwrap().unwrap();
    assert!(got.diff.items.is_empty());
    assert_eq!(got.needs_hash, vec![(1, SourceKind::Master)]);
    assert_eq!(
        devices::enqueue_source_hashes(&c, &got.needs_hash, 30)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        devices::enqueue_source_hashes(&c, &got.needs_hash, 31)
            .unwrap()
            .len(),
        0,
        "未完了の間は dedup"
    );
}

#[test]
fn compute_adds_hashed_tracks_and_excludes_missing() {
    let c = conn();
    let d = adb(&c, Selection::All);
    insert_track_with_inode(&c, 1, "YT/a.opus", "opus", Some(1));
    insert_track(&c, 2, "YT/b.opus", "opus");
    c.execute("UPDATE tracks SET missing_since = 5 WHERE id = 2", [])
        .unwrap();
    // tracks(1) の identity (inode=1, size=1, mtime_ns=0, ctime_ns=0) と一致させる
    let h = SourceHash {
        semantic: semantic_master(1, 1),
        inode: 1,
        size: 1,
        mtime_ns: 0,
        ctime_ns: 0,
        sha256: "s1".into(),
    };
    devices::put_source_hash(&c, 1, SourceKind::Master, &h, 40).unwrap();
    // 2 は missing だが端末に写しがある → 削除
    devices::replace_items(
        &c,
        d.id,
        &[DeviceItem {
            track_id: 2,
            dest_path: "YT/b.opus".into(),
            token: "t".into(),
            size: 1,
            sha256: "x".into(),
        }],
        50,
    )
    .unwrap();
    let got = devices::compute(&c, d.id).unwrap().unwrap();
    let ops: Vec<_> = got
        .diff
        .items
        .iter()
        .map(|o| (o.kind, o.track_id))
        .collect();
    assert_eq!(ops, vec![(OpKind::Delete, 2), (OpKind::Add, 1)]);
    assert_eq!(got.plan_token, plan_token(1, &got.diff));
}

/// プレイリストは衝突を解いた後の desired から組み立てる。1 は YT/b.opus に移りたいが、保留の 2 が
/// そこに居続けるので 1 はパス衝突で保留になり、m3u8 には 1 の今の写し（YT/a.opus）が載る
#[test]
fn compute_builds_playlists_after_resolving_collisions() {
    let c = conn();
    let d = adb(&c, Selection::All);
    insert_track_with_inode(&c, 1, "YT/b.opus", "opus", Some(1));
    insert_track(&c, 2, "YT/c.opus", "opus"); // ハッシュが無いので保留
    let h = SourceHash {
        semantic: semantic_master(1, 1),
        inode: 1,
        size: 1,
        mtime_ns: 0,
        ctime_ns: 0,
        sha256: "s1".into(),
    };
    devices::put_source_hash(&c, 1, SourceKind::Master, &h, 40).unwrap();
    let it = |id: i64, path: &str| DeviceItem {
        track_id: id,
        dest_path: path.into(),
        token: "t".into(),
        size: 1,
        sha256: "x".into(),
    };
    devices::replace_items(&c, d.id, &[it(1, "YT/a.opus"), it(2, "YT/b.opus")], 50).unwrap();
    c.execute("INSERT INTO playlists (id, name, name_key, created_at, updated_at) VALUES (5, 'p', 'p', 0, 0)", []).unwrap();
    c.execute(
        "INSERT INTO playlist_items (playlist_id, position, track_id) VALUES (5, 0, 1), (5, 1, 2)",
        [],
    )
    .unwrap();
    devices::set_playlists(&c, d.id, &[5], 60).unwrap();

    let got = devices::compute(&c, d.id).unwrap().unwrap();
    assert!(got.diff.items.is_empty(), "{:?}", got.diff.items);
    assert!(got.desired.is_empty());
    assert!(got
        .diff
        .held
        .iter()
        .any(|h| h.track_id == 1 && h.hold == Hold::Error(ItemError::PathCollision)));
    assert_eq!(got.playlists.len(), 1);
    assert_eq!(
        String::from_utf8(got.playlists[0].body.clone()).unwrap(),
        "#EXTM3U\n../YT/a.opus\n../YT/b.opus\n"
    );
}

#[test]
fn compute_exposes_resolved_desired_with_source() {
    let c = conn();
    let d = adb(&c, Selection::All);
    insert_track_with_inode(&c, 1, "YT/a.opus", "opus", Some(1));
    let h = SourceHash {
        semantic: semantic_master(1, 1),
        inode: 1,
        size: 1,
        mtime_ns: 0,
        ctime_ns: 0,
        sha256: "s1".into(),
    };
    devices::put_source_hash(&c, 1, SourceKind::Master, &h, 40).unwrap();
    let got = devices::compute(&c, d.id).unwrap().unwrap();
    assert_eq!(got.desired.len(), 1);
    let src = &got.desired[0].source;
    assert_eq!(src.kind, SourceKind::Master);
    assert_eq!(src.root_rel_path, "YT/a.opus");
    assert_eq!(src.semantic, semantic_master(1, 1));
}

/// channels が NULL の曲は選曲の外。端末に写しがあれば削除になる
#[test]
fn track_with_null_channels_is_deleted_from_device() {
    let c = conn();
    let d = adb(&c, Selection::All);
    insert_track(&c, 1, "YT/a.opus", "opus");
    c.execute("UPDATE tracks SET channels = NULL WHERE id = 1", [])
        .unwrap();
    devices::replace_items(
        &c,
        d.id,
        &[DeviceItem {
            track_id: 1,
            dest_path: "YT/a.opus".into(),
            token: "t".into(),
            size: 1,
            sha256: "x".into(),
        }],
        50,
    )
    .unwrap();
    let got = devices::compute(&c, d.id).unwrap().unwrap();
    let ops: Vec<_> = got
        .diff
        .items
        .iter()
        .map(|o| (o.kind, o.track_id))
        .collect();
    assert_eq!(ops, vec![(OpKind::Delete, 1)]);
    assert!(got.needs_hash.is_empty());
}

fn device_with(transport: Transport, adb: Option<(&str, &str)>) -> devices::Device {
    devices::Device {
        id: 1,
        uuid: "u".into(),
        name: "n".into(),
        transport,
        variant: Variant::Opus,
        selection: Selection::All,
        generation: 1,
        adb_serial: adb.map(|_| "SER".to_string()),
        adb_volume: adb.map(|(v, _)| v.to_string()),
        adb_root: adb.map(|(_, r)| r.to_string()),
        last_synced_at: None,
    }
}

#[test]
fn root_prefix_utf16_per_transport_and_volume() {
    // /storage/emulated/0/Music/spindle/
    assert_eq!(
        devices::root_prefix_utf16(&device_with(
            Transport::Adb,
            Some(("emulated", "Music/spindle"))
        )),
        "/storage/emulated/0/Music/spindle/".len()
    );
    // SD カード: /storage/1234-ABCD/音楽/
    assert_eq!(
        devices::root_prefix_utf16(&device_with(Transport::Adb, Some(("1234-ABCD", "音楽")))),
        "/storage/1234-ABCD/".len() + 2 + 1
    );
    assert_eq!(
        devices::root_prefix_utf16(&device_with(Transport::Agent, None)),
        "Music/spindle/".len()
    );
}

#[test]
fn playlist_selection_uses_marked_playlists_only() {
    let c = conn();
    let d = adb(&c, Selection::Playlists);
    insert_track(&c, 1, "YT/a.opus", "opus");
    insert_track(&c, 2, "YT/b.opus", "opus");
    c.execute("INSERT INTO playlists (id, name, name_key, created_at, updated_at) VALUES (5, 'p', 'p', 0, 0)", []).unwrap();
    c.execute(
        "INSERT INTO playlist_items (playlist_id, position, track_id) VALUES (5, 0, 2)",
        [],
    )
    .unwrap();
    devices::set_playlists(&c, d.id, &[5], 20).unwrap();
    let inputs = devices::track_inputs(&c, &devices::get(&c, d.id).unwrap().unwrap()).unwrap();
    assert_eq!(
        inputs.iter().map(|t| t.track_id).collect::<Vec<_>>(),
        vec![2]
    );
    let pl = devices::playlist_inputs(&c, d.id).unwrap();
    assert_eq!(
        pl,
        vec![PlaylistInput {
            playlist_id: 5,
            name: "p".into(),
            track_ids: vec![2]
        }]
    );
}

#[test]
fn replace_items_is_a_full_replace() {
    let c = conn();
    let d = adb(&c, Selection::All);
    let it = |id: i64| DeviceItem {
        track_id: id,
        dest_path: format!("{id}.opus"),
        token: "t".into(),
        size: 1,
        sha256: "s".into(),
    };
    devices::replace_items(&c, d.id, &[it(1), it(2)], 1).unwrap();
    devices::replace_items(&c, d.id, &[it(3)], 2).unwrap();
    assert_eq!(
        devices::items(&c, d.id)
            .unwrap()
            .iter()
            .map(|i| i.track_id)
            .collect::<Vec<_>>(),
        vec![3]
    );
}

#[test]
fn materialize_advances_evaluated_at_even_if_unchanged() {
    let c = conn();
    c.execute("INSERT INTO playlists (id, name, name_key, kind, created_at, updated_at) VALUES (5, 'p', 'p', 'smart', 0, 0)", []).unwrap();
    spindle::db::playlists::materialize(&c, 5, &[], 100).unwrap();
    spindle::db::playlists::materialize(&c, 5, &[], 200).unwrap();
    let at: i64 = c
        .query_row("SELECT evaluated_at FROM playlists WHERE id = 5", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(at, 200);
}

/// `materialize` は項目の置き換えと `evaluated_at` の更新を同じ SAVEPOINT で確定する。
/// 存在しないトラック id は FK 違反で `rewrite_items` が失敗するので、失敗全体が巻き戻り、
/// `playlist_items` も `evaluated_at` も呼び出し前のまま残らなければならない
#[test]
fn materialize_rolls_back_evaluated_at_when_item_rewrite_fails() {
    let c = conn();
    c.execute("INSERT INTO playlists (id, name, name_key, kind, created_at, updated_at) VALUES (5, 'p', 'p', 'smart', 0, 0)", []).unwrap();
    insert_track(&c, 1, "YT/a.opus", "opus");
    spindle::db::playlists::materialize(&c, 5, &[1], 100).unwrap();

    let before_at: i64 = c
        .query_row("SELECT evaluated_at FROM playlists WHERE id = 5", [], |r| {
            r.get(0)
        })
        .unwrap();
    let before_items = spindle::db::playlists::items(&c, 5).unwrap();

    // 999 は存在しないトラック id → INSERT が FK 違反で失敗する
    let err = spindle::db::playlists::materialize(&c, 5, &[1, 999], 200);
    assert!(err.is_err(), "存在しない track_id は FK 違反で失敗する");

    assert_eq!(
        spindle::db::playlists::items(&c, 5).unwrap(),
        before_items,
        "失敗した置換の前の項目がそのまま残る"
    );
    let after_at: i64 = c
        .query_row("SELECT evaluated_at FROM playlists WHERE id = 5", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        after_at, before_at,
        "評価時刻も失敗前のまま残らなければならない"
    );
}

/// 仕様 ③「送る元のハッシュ」: ハッシュ行の identity が `tracks` の現在の物理同一性
/// (inode, size, mtime_ns, ctime_ns。dev は見ない。D-62) とずれていれば、外部がファイルを
/// 書き換えた可能性があるので使わず、再ハッシュを要求する
#[test]
fn stale_master_identity_forces_rehash() {
    let c = conn();
    let d = adb(&c, Selection::All);
    insert_track_with_inode(&c, 1, "YT/a.opus", "opus", Some(7));
    let h = SourceHash {
        semantic: semantic_master(1, 1),
        inode: 7,
        size: 1,
        mtime_ns: 0,
        ctime_ns: 0,
        sha256: "s1".into(),
    };
    devices::put_source_hash(&c, 1, SourceKind::Master, &h, 40).unwrap();

    let got = devices::compute(&c, d.id).unwrap().unwrap();
    assert!(
        got.needs_hash.is_empty(),
        "identity が一致していれば再ハッシュ不要"
    );

    c.execute("UPDATE tracks SET mtime_ns = mtime_ns + 1 WHERE id = 1", [])
        .unwrap();
    let got = devices::compute(&c, d.id).unwrap().unwrap();
    assert_eq!(
        got.needs_hash,
        vec![(1, SourceKind::Master)],
        "外部書き換えで identity がずれたら再ハッシュ"
    );
}

/// `set_playlists` は DELETE → INSERT → generation++ を 1 つの SAVEPOINT にまとめる。
/// 未知の playlist_id は `INSERT OR IGNORE` では黙らせられない FK 違反になるので、
/// 印も generation も呼び出し前のまま残らなければならない
#[test]
fn set_playlists_with_unknown_id_leaves_marks_and_generation_unchanged() {
    let c = conn();
    let d = adb(&c, Selection::Playlists);
    c.execute("INSERT INTO playlists (id, name, name_key, created_at, updated_at) VALUES (5, 'p', 'p', 0, 0)", []).unwrap();
    devices::set_playlists(&c, d.id, &[5], 20).unwrap();

    assert!(
        devices::set_playlists(&c, d.id, &[999], 30).is_err(),
        "存在しない playlist_id は FK 違反で失敗する"
    );

    assert_eq!(
        devices::playlist_ids(&c, d.id).unwrap(),
        vec![5],
        "印は直前のまま"
    );
    assert_eq!(
        devices::get(&c, d.id).unwrap().unwrap().generation,
        2,
        "generation も直前のまま（失敗した呼び出し分は進まない）"
    );
}

/// `replace_items` は DELETE → INSERT を 1 つの SAVEPOINT にまとめる。autocommit のコネクションで
/// `dest_path_key`（`canonical_key`。casefold）の UNIQUE 違反が起きても、直前の内容が空・部分的に
/// ならず残らなければならない
#[test]
fn replace_items_rejects_dest_path_collision_and_keeps_previous_items() {
    let c = conn();
    let d = adb(&c, Selection::All);
    let previous = DeviceItem {
        track_id: 1,
        dest_path: "old.opus".into(),
        token: "t".into(),
        size: 1,
        sha256: "s".into(),
    };
    devices::replace_items(&c, d.id, std::slice::from_ref(&previous), 1).unwrap();

    // "A/x.opus" と "a/X.opus" は canonical_key で衝突する
    let colliding = [
        DeviceItem {
            track_id: 2,
            dest_path: "A/x.opus".into(),
            token: "t".into(),
            size: 1,
            sha256: "s".into(),
        },
        DeviceItem {
            track_id: 3,
            dest_path: "a/X.opus".into(),
            token: "t".into(),
            size: 1,
            sha256: "s".into(),
        },
    ];
    assert!(
        devices::replace_items(&c, d.id, &colliding, 2).is_err(),
        "dest_path_key の UNIQUE 違反で失敗する"
    );

    assert_eq!(
        devices::items(&c, d.id).unwrap(),
        vec![previous],
        "失敗した置換の前の内容がそのまま残る"
    );
}

#[test]
fn track_inputs_joins_derived_and_hashes_in_one_pass() {
    let c = conn();
    let d = adb(&c, Selection::All);
    insert_track_with_inode(&c, 1, "A/a.flac", "flac", Some(1));
    insert_track_with_inode(&c, 2, "YT/b.opus", "opus", Some(2));
    insert_track_with_inode(&c, 3, "YT/c.opus", "opus", Some(3));
    c.execute(
        "INSERT INTO derived_files (track_id, variant, rel_path, rel_path_key, codec, src_audio_version,
                                    src_tag_version, generated_at, src_artwork_id, src_rg_scanned_at,
                                    audio_profile, tag_profile)
         VALUES (1, 'opus', 'opus/A/a.opus', 'opus/a/a.opus', 'opus', 1, 1, 0, NULL, NULL, 'p', 't')",
        [],
    )
    .unwrap();
    let h = |sem: &str, inode: u64| SourceHash {
        semantic: sem.into(),
        inode,
        size: 1,
        mtime_ns: 0,
        ctime_ns: 0,
        sha256: "ab".repeat(32),
    };
    devices::put_source_hash(&c, 1, SourceKind::Derived(Variant::Opus), &h("d1", 99), 5).unwrap();
    devices::put_source_hash(&c, 2, SourceKind::Master, &h("m2", 2), 5).unwrap();
    // 3 は identity が tracks 行と違う（inode 7 != 3）ので hash_master は None になる
    devices::put_source_hash(&c, 3, SourceKind::Master, &h("m3", 7), 5).unwrap();

    let got = devices::track_inputs(&c, &d).unwrap();
    assert_eq!(
        got.iter().map(|t| t.track_id).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(got[0].derived.as_ref().unwrap().rel_path, "opus/A/a.opus");
    assert_eq!(got[0].derived.as_ref().unwrap().audio_profile, "p");
    assert_eq!(got[0].hash_derived.as_ref().unwrap().semantic, "d1");
    assert!(got[0].hash_master.is_none());
    assert!(got[1].derived.is_none());
    assert_eq!(got[1].hash_master.as_ref().unwrap().semantic, "m2");
    assert!(got[2].hash_master.is_none(), "identity 不一致は無効");
}

/// 選曲の見積もりは「現在の送る元」のハッシュが有効な曲だけ bytes に数える（build_manifest と同じ判定）。
/// Derived の意味トークンが古い・Derived が無い・非可逆原本のハッシュが無いものは unhashed
#[test]
fn estimate_selection_counts_only_valid_current_source_hashes() {
    let c = conn();
    let d = adb(&c, Selection::All);
    let profile = derived::settings_of(&c, Variant::Opus)
        .unwrap()
        .unwrap()
        .audio_profile;
    for id in 1..=5 {
        let (rel, codec) = match id {
            3 | 4 => (format!("YT/{id}.opus"), "opus"),
            _ => (format!("A/{id}.flac"), "flac"),
        };
        insert_track_with_inode(&c, id, &rel, codec, Some(id));
    }
    // 1, 2 は現在の設定で作った Derived がある。5 は Derived が無い（古いハッシュの行だけ残っている）
    for id in [1, 2] {
        c.execute(
            "INSERT INTO derived_files (track_id, variant, rel_path, rel_path_key, codec, src_audio_version,
                                        src_tag_version, generated_at, src_artwork_id, src_rg_scanned_at,
                                        audio_profile, tag_profile)
             VALUES (?1, 'opus', ?2, lower(?2), 'opus', 1, 1, 0, NULL, NULL, ?3, 't')",
            params![id, format!("opus/A/{id}.opus"), profile],
        )
        .unwrap();
    }
    // 3 の原本は size 300（原本のハッシュは tracks 行の identity と一致しないと無効）
    c.execute("UPDATE tracks SET size = 300 WHERE id = 3", [])
        .unwrap();
    let inputs = devices::track_inputs(&c, &d).unwrap();
    let sem_d1 = semantic_derived(Variant::Opus, inputs[0].derived.as_ref().unwrap());
    let h = |semantic: String, inode: u64, size: u64| SourceHash {
        semantic,
        inode,
        size,
        mtime_ns: 0,
        ctime_ns: 0,
        sha256: "ab".repeat(32),
    };
    let opus = SourceKind::Derived(Variant::Opus);
    devices::put_source_hash(&c, 1, opus, &h(sem_d1.clone(), 11, 100), 5).unwrap();
    devices::put_source_hash(&c, 2, opus, &h("old".into(), 12, 200), 5).unwrap();
    devices::put_source_hash(
        &c,
        3,
        SourceKind::Master,
        &h(semantic_master(1, 1), 3, 300),
        5,
    )
    .unwrap();
    devices::put_source_hash(&c, 5, opus, &h(sem_d1, 15, 500), 5).unwrap();

    let e = devices::estimate_selection(&c, &d, Selection::All, &[]).unwrap();
    assert_eq!(e.tracks, 5);
    assert_eq!(
        e.bytes,
        100 + 300,
        "有効なハッシュ（1 の Derived、3 の原本）だけ合計"
    );
    assert_eq!(
        e.unhashed, 3,
        "2（意味トークンが古い）・4（非可逆原本のハッシュ無し）・5（Derived 無し）"
    );

    // 原本が外部で書き換えられて identity がずれたら、3 のハッシュも無効
    c.execute("UPDATE tracks SET mtime_ns = 1 WHERE id = 3", [])
        .unwrap();
    let e = devices::estimate_selection(&c, &d, Selection::All, &[]).unwrap();
    assert_eq!((e.bytes, e.unhashed), (100, 4));
}

#[test]
fn pair_code_lifecycle() {
    use spindle::db::devices::{PairCodeIssue, PairReserve};
    let c = conn();
    let x = adb(&c, Selection::All);
    assert_eq!(
        devices::issue_pair_code(&c, x.id, "sel", "h", 100).unwrap(),
        PairCodeIssue::NotAgent
    );
    assert_eq!(
        devices::issue_pair_code(&c, 999, "sel", "h", 100).unwrap(),
        PairCodeIssue::NotFound
    );
    let d = devices::create(
        &c,
        &NewDevice {
            name: "iPhone",
            transport: Transport::Agent,
            variant: Variant::Aac,
            selection: Selection::All,
            adb: None,
        },
        10,
    )
    .unwrap();
    assert_eq!(
        devices::issue_pair_code(&c, d.id, "sel", "h", 100).unwrap(),
        PairCodeIssue::Issued { expires_at: 700 }
    );
    // 引けないセレクタは何も書かない
    assert_eq!(
        devices::reserve_pair_attempt(&c, "nope", 100).unwrap(),
        PairReserve::Rejected
    );
    // 上限まで予約でき、上限で失効する
    for _ in 0..5 {
        assert_eq!(
            devices::reserve_pair_attempt(&c, "sel", 100).unwrap(),
            PairReserve::Reserved {
                device_id: d.id,
                code_hash: "h".into()
            }
        );
    }
    assert_eq!(
        devices::reserve_pair_attempt(&c, "sel", 100).unwrap(),
        PairReserve::Rejected
    );
    devices::fail_pair_attempt(&c, d.id, "sel").unwrap();
    let sel: Option<String> = c
        .query_row(
            "SELECT pair_selector FROM devices WHERE id = ?1",
            [d.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sel, None, "上限に達したコードは失効");
    // 期限切れはコードを消す
    devices::issue_pair_code(&c, d.id, "sel2", "h2", 100).unwrap();
    assert_eq!(
        devices::reserve_pair_attempt(&c, "sel2", 700).unwrap(),
        PairReserve::Rejected
    );
    let sel: Option<String> = c
        .query_row(
            "SELECT pair_selector FROM devices WHERE id = ?1",
            [d.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sel, None, "期限切れのコードは消える");
    // 消費は同じコードが載っているときだけ（CAS）。トークンを載せ、引ける
    devices::issue_pair_code(&c, d.id, "sel3", "h3", 100).unwrap();
    assert!(!devices::consume_pair_code(&c, d.id, "sel3", "other", "tsel", "th", 101).unwrap());
    assert!(devices::consume_pair_code(&c, d.id, "sel3", "h3", "tsel", "th", 101).unwrap());
    assert!(!devices::consume_pair_code(&c, d.id, "sel3", "h3", "tsel", "th", 101).unwrap());
    let a = devices::agent_by_selector(&c, "tsel").unwrap().unwrap();
    assert_eq!(
        (a.device_id, a.name.as_str(), a.secret_hash.as_str()),
        (d.id, "iPhone", "th")
    );
    // 発行し直すと旧トークンは引けない
    devices::issue_pair_code(&c, d.id, "sel4", "h4", 200).unwrap();
    assert!(devices::agent_by_selector(&c, "tsel").unwrap().is_none());
}

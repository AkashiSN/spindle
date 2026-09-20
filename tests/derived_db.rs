//! `derived_files` の読み書きと transcode ジョブの投入判定（docs/TASKS.md P1-10、D-51）。
//! `:memory:` にマイグレーションを流し、tracks / albums / artwork を直接 INSERT して判定だけを見る

use rusqlite::{params, Connection};

use spindle::config::OpusVariantConfig;
use spindle::db::derived::{Profiles, TagState};
use spindle::db::{derived, open_memory_connection, replaygain as dbrg};
use spindle::domain::derived::{Variant, VariantSettings};
use spindle::domain::replaygain::Values;

const O: Variant = Variant::Opus;

/// opus 系統を 256k で on にした接続（起動時の sync_variants と同じ）
fn conn() -> Connection {
    let c = open_memory_connection().unwrap();
    sync(&c, true, 256);
    c
}

fn sync(c: &Connection, enabled: bool, bitrate: u32) {
    derived::sync_variants(c, &OpusVariantConfig { enabled, bitrate }, 0).unwrap();
}

/// 現在の opus 系統の設定と同じ世代
fn profiles(c: &Connection) -> Profiles {
    Profiles::of(&derived::settings_of(c, O).unwrap().unwrap())
}

/// `upsert` の短縮（現在の設定の世代で）
fn put(
    c: &Connection,
    id: i64,
    rel: &str,
    bitrate: Option<i64>,
    av: i64,
    tags: TagState,
    now: i64,
) {
    let pr = profiles(c);
    derived::upsert(c, id, O, rel, bitrate, av, tags, &pr, now).unwrap();
}

fn insert_track(c: &Connection, id: i64, rel: &str, codec: &str, channels: Option<i64>) {
    let lossless = i64::from(!matches!(codec, "opus" | "mp3" | "aac" | "ogg"));
    c.execute(
        "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                             channels, audio_version, tag_version, seen_at)
         VALUES (?1, ?2, lower(?2), 1, 0, 0, ?3, ?4, ?5, 1, 1, 0)",
        params![id, rel, codec, lossless, channels],
    )
    .unwrap();
}

fn insert_artwork(c: &Connection, id: i64) {
    c.execute(
        "INSERT INTO artwork (id, sha256, mime, bytes, origin)
         VALUES (?1, zeroblob(32), 'image/png', 1, 'embedded')",
        [id],
    )
    .unwrap();
    // sha256 は UNIQUE なので id ごとに変える
    c.execute(
        "UPDATE artwork SET sha256 = CAST(printf('%032d', ?1) AS BLOB) WHERE id = ?1",
        [id],
    )
    .unwrap();
}

fn tags(tv: i64, art: Option<i64>) -> TagState {
    TagState {
        src_tag_version: tv,
        src_artwork_id: art,
        src_rg_scanned_at: None,
    }
}

fn job_track_ids(c: &Connection, ids: &[i64]) -> Vec<i64> {
    let mut tracks: Vec<i64> = ids
        .iter()
        .map(|id| {
            c.query_row(
                "SELECT json_extract(payload, '$.track_id') FROM jobs WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        })
        .collect();
    tracks.sort();
    tracks
}

#[test]
fn load_target_reads_album_artwork() {
    let c = conn();
    insert_artwork(&c, 7);
    c.execute(
        "INSERT INTO albums (id, rel_dir, rel_dir_key, album, artwork_id) VALUES (3, 'A/B', 'a/b', 'B', 7)",
        [],
    )
    .unwrap();
    insert_track(&c, 1, "A/B/01.flac", "flac", Some(2));
    c.execute("UPDATE tracks SET album_id = 3 WHERE id = 1", [])
        .unwrap();
    let t = derived::load_target(&c, 1).unwrap().unwrap();
    assert_eq!(t.artwork_id, Some(7));
    assert_eq!(t.library_rel_path, "A/B/01.flac");
    assert!(t.lossless && !t.missing);
    assert_eq!((t.audio_version, t.tag_version), (1, 1));
    assert!(derived::load_target(&c, 99).unwrap().is_none());
    // album 無し
    insert_track(&c, 2, "A/02.flac", "flac", Some(2));
    assert_eq!(
        derived::load_target(&c, 2).unwrap().unwrap().artwork_id,
        None
    );
}

#[test]
fn enqueue_if_stale_only_for_eligible_and_stale() {
    let c = conn();
    insert_track(&c, 1, "A/01.flac", "flac", Some(2));
    insert_track(&c, 2, "A/02.opus", "opus", Some(2));
    insert_track(&c, 3, "A/03.flac", "flac", Some(6));
    // 非可逆・6ch・無いトラックは投入しない
    assert!(derived::enqueue_if_stale(&c, 2, 0).unwrap().is_empty());
    assert!(derived::enqueue_if_stale(&c, 3, 0).unwrap().is_empty());
    assert!(derived::enqueue_if_stale(&c, 99, 0).unwrap().is_empty());
    // Derived が無ければ投入。同じ audio_version の間は dedup
    let id = derived::enqueue_if_stale(&c, 1, 0).unwrap()[0];
    assert!(derived::enqueue_if_stale(&c, 1, 0).unwrap().is_empty());
    let (ty, key, payload): (String, String, String) = c
        .query_row(
            "SELECT type, dedup_key, payload FROM jobs WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (ty.as_str(), key.as_str()),
        ("transcode", "transcode:1:opus:1")
    );
    let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(payload["track_id"], 1);
    assert_eq!(payload["variant"], "opus");
    assert_eq!(payload["audio_version"], 1);
    assert_eq!(payload["tag_version"], 1);
    // 揃っていれば投入しない
    c.execute("UPDATE jobs SET state = 'done' WHERE id = ?1", [id])
        .unwrap();
    put(&c, 1, "opus/A/01.opus", Some(256), 1, tags(1, None), 0);
    assert!(derived::enqueue_if_stale(&c, 1, 0).unwrap().is_empty());
    // タグ版が進めば投入
    c.execute("UPDATE tracks SET tag_version = 2 WHERE id = 1", [])
        .unwrap();
    assert_eq!(derived::enqueue_if_stale(&c, 1, 0).unwrap().len(), 1);
}

/// 設定の世代（D-75）: ビットレートを変えれば audio_profile の差分で投入、凍結（enabled=false）なら
/// 何も投入しない。表に無い系統（aac は P4-8）も投入しない
#[test]
fn enqueue_follows_profiles_and_freeze() {
    let c = conn();
    insert_track(&c, 1, "A/01.flac", "flac", Some(2));
    put(&c, 1, "opus/A/01.opus", Some(256), 1, tags(1, None), 0);
    assert!(derived::enqueue_if_stale(&c, 1, 0).unwrap().is_empty());
    // 0018 が移した旧ルート直下の 128k の行 → 256 設定で投入
    let legacy = Profiles {
        audio_profile: "opus:128:v1".into(),
        tag_profile: "opus:v1".into(),
    };
    derived::upsert(
        &c,
        1,
        O,
        "A/01.opus",
        Some(128),
        1,
        tags(1, None),
        &legacy,
        0,
    )
    .unwrap();
    let ids = derived::enqueue_if_stale(&c, 1, 0).unwrap();
    assert_eq!(ids.len(), 1);
    c.execute("UPDATE jobs SET state = 'done'", []).unwrap();
    // 設定を 128 に戻せば profile 一致。パスの差分（ルート直下 → opus/）だけなので投入はする（Move）
    sync(&c, true, 128);
    assert_eq!(derived::enqueue_if_stale(&c, 1, 0).unwrap().len(), 1);
    c.execute("UPDATE jobs SET state = 'done'", []).unwrap();
    // 凍結: 行が古くても投入しない。一括投入も同じ
    sync(&c, false, 256);
    assert!(derived::enqueue_if_stale(&c, 1, 0).unwrap().is_empty());
    assert!(derived::enqueue_all_stale(&c, 0).unwrap().is_empty());
    // 設定を戻せば拾う
    sync(&c, true, 256);
    assert_eq!(derived::enqueue_all_stale(&c, 0).unwrap().len(), 1);
    // 表の中身
    let all = derived::variant_settings(&c).unwrap();
    assert_eq!(
        all,
        vec![VariantSettings {
            variant: O,
            enabled: true,
            audio_profile: "opus:256:v1".into(),
            tag_profile: "opus:v1".into(),
        }]
    );
    assert!(derived::settings_of(&c, Variant::Aac).unwrap().is_none());
}

#[test]
fn enqueue_all_stale_covers_missing_moved_and_artwork() {
    let c = conn();
    insert_artwork(&c, 7);
    c.execute(
        "INSERT INTO albums (id, rel_dir, rel_dir_key, album, artwork_id) VALUES (3, 'C', 'c', 'C', 7)",
        [],
    )
    .unwrap();
    insert_track(&c, 1, "A/01.flac", "flac", Some(2)); // Derived 無し
    insert_track(&c, 2, "A/02.flac", "flac", Some(2)); // 揃っている
    insert_track(&c, 3, "B/03.flac", "flac", Some(2)); // パスがずれた
    insert_track(&c, 4, "A/04.flac", "flac", Some(2)); // missing
    insert_track(&c, 5, "A/05.wav", "wav", None); // channels 不明は対象外
    insert_track(&c, 6, "C/06.flac", "flac", Some(2)); // カバーが付いた
    insert_track(&c, 7, "A/07.mp3", "mp3", Some(2)); // 非可逆
    insert_track(&c, 8, "A/08.flac", "flac", Some(2)); // RG が解析された
    c.execute("UPDATE tracks SET missing_since = 1 WHERE id = 4", [])
        .unwrap();
    c.execute("UPDATE tracks SET album_id = 3 WHERE id = 6", [])
        .unwrap();
    put(&c, 2, "opus/A/02.opus", None, 1, tags(1, None), 0);
    put(&c, 3, "opus/A/03.opus", None, 1, tags(1, None), 0);
    put(&c, 4, "opus/A/04.opus", None, 1, tags(1, None), 0);
    put(&c, 6, "opus/C/06.opus", None, 1, tags(1, None), 0);
    put(&c, 8, "opus/A/08.opus", None, 1, tags(1, None), 0);
    c.execute("UPDATE tracks SET rg_scanned_at = 5 WHERE id = 8", [])
        .unwrap();
    let ids = derived::enqueue_all_stale(&c, 0).unwrap();
    assert_eq!(job_track_ids(&c, &ids), vec![1, 3, 6, 8]);
    // 二度目は dedup で 0 件
    assert!(derived::enqueue_all_stale(&c, 0).unwrap().is_empty());
}

#[test]
fn holder_path_tag_state_and_delete() {
    let c = conn();
    insert_track(&c, 1, "A/01.flac", "flac", Some(2));
    put(&c, 1, "opus/A/01.opus", None, 1, tags(1, None), 0);
    assert_eq!(
        derived::holder_of_key(&c, "opus/a/01.opus").unwrap(),
        Some(1)
    );
    assert_eq!(derived::holder_of_key(&c, "zzz").unwrap(), None);
    derived::set_path(&c, 1, O, "opus/B/01.opus").unwrap();
    assert_eq!(derived::holder_of_key(&c, "opus/a/01.opus").unwrap(), None);
    assert_eq!(
        derived::holder_of_key(&c, "opus/b/01.opus").unwrap(),
        Some(1)
    );
    // artwork 9 は無い（FK）
    derived::set_tag_state(&c, 1, O, tags(5, Some(9)), "opus:v1", 1).unwrap_err();
    insert_artwork(&c, 9);
    derived::set_tag_state(
        &c,
        1,
        O,
        TagState {
            src_rg_scanned_at: Some(42),
            ..tags(5, Some(9))
        },
        "opus:v2",
        1,
    )
    .unwrap();
    let cur = derived::get(&c, 1, O).unwrap().unwrap();
    assert_eq!(cur.rel_path, "opus/B/01.opus");
    assert_eq!((cur.src_audio_version, cur.src_tag_version), (1, 5));
    assert_eq!(cur.src_artwork_id, Some(9));
    assert_eq!(cur.src_rg_scanned_at, Some(42));
    assert_eq!(cur.tag_profile, "opus:v2");
    // holder_state は系統付き
    let h = derived::holder_state(&c, "opus/b/01.opus")
        .unwrap()
        .unwrap();
    assert_eq!(
        (h.track_id, h.variant, h.missing, h.stale),
        (1, O, false, true)
    );
    // upsert は既存行を置き換える。aac の行は別に持てる
    put(&c, 1, "opus/C/01.opus", Some(96), 2, tags(6, None), 2);
    let cur = derived::get(&c, 1, O).unwrap().unwrap();
    assert_eq!(cur.rel_path, "opus/C/01.opus");
    assert_eq!((cur.src_audio_version, cur.src_tag_version), (2, 6));
    let aac = Profiles {
        audio_profile: "aac:256:v1".into(),
        tag_profile: "aac:v1".into(),
    };
    derived::upsert(
        &c,
        1,
        Variant::Aac,
        "aac/C/01.m4a",
        None,
        2,
        tags(6, None),
        &aac,
        2,
    )
    .unwrap();
    assert!(derived::get(&c, 1, Variant::Aac).unwrap().is_some());
    assert!(!derived::track_is_missing(&c, 1).unwrap());
    c.execute("UPDATE tracks SET missing_since = 1 WHERE id = 1", [])
        .unwrap();
    assert!(derived::track_is_missing(&c, 1).unwrap());
    assert!(derived::delete(&c, 1, O).unwrap());
    assert!(!derived::delete(&c, 1, O).unwrap());
    assert!(derived::get(&c, 1, O).unwrap().is_none());
    assert!(
        derived::get(&c, 1, Variant::Aac).unwrap().is_some(),
        "系統ごとに消す"
    );
}

#[test]
fn rg_store_advances_scanned_at_even_within_the_same_second() {
    let c = conn();
    insert_track(&c, 1, "A/01.flac", "flac", Some(2));
    let v = |g: f64| Values {
        track_gain: g,
        track_peak: 0.5,
        album_gain: None,
        album_peak: None,
    };
    let scanned = |c: &Connection| -> i64 {
        c.query_row("SELECT rg_scanned_at FROM tracks WHERE id = 1", [], |r| {
            r.get(0)
        })
        .unwrap()
    };
    dbrg::store(&c, &[(1, v(-6.0))], 100).unwrap();
    assert_eq!(scanned(&c), 100);
    put(
        &c,
        1,
        "opus/A/01.opus",
        None,
        1,
        TagState {
            src_rg_scanned_at: Some(100),
            ..tags(1, None)
        },
        0,
    );
    assert!(derived::enqueue_if_stale(&c, 1, 0).unwrap().is_empty());
    // 同じ秒に値が変わる再解析 → 世代は 101 に進み、Derived が古いと判定される
    dbrg::store(&c, &[(1, v(-7.0))], 100).unwrap();
    assert_eq!(scanned(&c), 101);
    assert_eq!(derived::enqueue_if_stale(&c, 1, 0).unwrap().len(), 1);
    // 時刻が進んでいればその値
    dbrg::store(&c, &[(1, v(-8.0))], 500).unwrap();
    assert_eq!(scanned(&c), 500);
    // 同じ値の再解析は now（同じ秒なら据え置き相当）
    dbrg::store(&c, &[(1, v(-8.0))], 500).unwrap();
    assert_eq!(scanned(&c), 500);
}

/// Derived に埋める画像はトラック自身の `artwork_id`、無ければ album の `artwork_id`（D-61）
#[test]
fn target_artwork_prefers_the_tracks_own_picture_over_the_album() {
    let c = conn();
    insert_artwork(&c, 7);
    insert_artwork(&c, 8);
    c.execute(
        "INSERT INTO albums (id, rel_dir, rel_dir_key, album, artwork_id) VALUES (3, 'A', 'a', 'A', 7)",
        [],
    )
    .unwrap();
    insert_track(&c, 1, "A/01.flac", "flac", Some(2));
    insert_track(&c, 2, "A/02.flac", "flac", Some(2));
    c.execute(
        "UPDATE tracks SET album_id = 3, artwork_id = CASE id WHEN 1 THEN 8 ELSE NULL END",
        [],
    )
    .unwrap();
    assert_eq!(
        derived::load_target(&c, 1).unwrap().unwrap().artwork_id,
        Some(8)
    );
    assert_eq!(
        derived::load_target(&c, 2).unwrap().unwrap().artwork_id,
        Some(7)
    );

    // 一括投入も同じ判定: track 1 は Derived が album の絵（7）で作られていれば stale、
    // track 2 は一致なので投入しない
    put(&c, 1, "opus/A/01.opus", None, 1, tags(1, Some(7)), 0);
    put(&c, 2, "opus/A/02.opus", None, 1, tags(1, Some(7)), 0);
    let ids = derived::enqueue_all_stale(&c, 100).unwrap();
    assert_eq!(job_track_ids(&c, &ids), vec![1]);
}

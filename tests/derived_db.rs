//! `derived_files` の読み書きと transcode ジョブの投入判定（docs/TASKS.md P1-10、D-51）。
//! `:memory:` にマイグレーションを流し、tracks / albums / artwork を直接 INSERT して判定だけを見る

use rusqlite::{params, Connection};

use spindle::db::derived::TagState;
use spindle::db::{derived, open_memory_connection, replaygain as dbrg};
use spindle::domain::replaygain::Values;

fn conn() -> Connection {
    open_memory_connection().unwrap()
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
    assert_eq!(derived::enqueue_if_stale(&c, 2, 0).unwrap(), None);
    assert_eq!(derived::enqueue_if_stale(&c, 3, 0).unwrap(), None);
    assert_eq!(derived::enqueue_if_stale(&c, 99, 0).unwrap(), None);
    // Derived が無ければ投入。同じ audio_version の間は dedup
    let id = derived::enqueue_if_stale(&c, 1, 0).unwrap().unwrap();
    assert_eq!(derived::enqueue_if_stale(&c, 1, 0).unwrap(), None);
    let (ty, key, payload): (String, String, String) = c
        .query_row(
            "SELECT type, dedup_key, payload FROM jobs WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((ty.as_str(), key.as_str()), ("transcode", "transcode:1:1"));
    let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(payload["track_id"], 1);
    assert_eq!(payload["audio_version"], 1);
    assert_eq!(payload["tag_version"], 1);
    // 揃っていれば投入しない
    c.execute("UPDATE jobs SET state = 'done' WHERE id = ?1", [id])
        .unwrap();
    derived::upsert(&c, 1, "A/01.opus", "opus", Some(128), 1, tags(1, None), 0).unwrap();
    assert_eq!(derived::enqueue_if_stale(&c, 1, 0).unwrap(), None);
    // タグ版が進めば投入
    c.execute("UPDATE tracks SET tag_version = 2 WHERE id = 1", [])
        .unwrap();
    assert!(derived::enqueue_if_stale(&c, 1, 0).unwrap().is_some());
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
    derived::upsert(&c, 2, "A/02.opus", "opus", None, 1, tags(1, None), 0).unwrap();
    derived::upsert(&c, 3, "A/03.opus", "opus", None, 1, tags(1, None), 0).unwrap();
    derived::upsert(&c, 4, "A/04.opus", "opus", None, 1, tags(1, None), 0).unwrap();
    derived::upsert(&c, 6, "C/06.opus", "opus", None, 1, tags(1, None), 0).unwrap();
    derived::upsert(&c, 8, "A/08.opus", "opus", None, 1, tags(1, None), 0).unwrap();
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
    derived::upsert(&c, 1, "A/01.opus", "opus", None, 1, tags(1, None), 0).unwrap();
    assert_eq!(derived::holder_of_key(&c, "a/01.opus").unwrap(), Some(1));
    assert_eq!(derived::holder_of_key(&c, "zzz").unwrap(), None);
    derived::set_path(&c, 1, "B/01.opus").unwrap();
    assert_eq!(derived::holder_of_key(&c, "a/01.opus").unwrap(), None);
    assert_eq!(derived::holder_of_key(&c, "b/01.opus").unwrap(), Some(1));
    // artwork 9 は無い（FK）
    derived::set_tag_state(&c, 1, tags(5, Some(9)), 1).unwrap_err();
    insert_artwork(&c, 9);
    derived::set_tag_state(
        &c,
        1,
        TagState {
            src_rg_scanned_at: Some(42),
            ..tags(5, Some(9))
        },
        1,
    )
    .unwrap();
    let cur = derived::get(&c, 1).unwrap().unwrap();
    assert_eq!(cur.rel_path, "B/01.opus");
    assert_eq!((cur.src_audio_version, cur.src_tag_version), (1, 5));
    assert_eq!(cur.src_artwork_id, Some(9));
    assert_eq!(cur.src_rg_scanned_at, Some(42));
    // upsert は既存行を置き換える
    derived::upsert(&c, 1, "C/01.opus", "opus", Some(96), 2, tags(6, None), 2).unwrap();
    let cur = derived::get(&c, 1).unwrap().unwrap();
    assert_eq!(cur.rel_path, "C/01.opus");
    assert_eq!((cur.src_audio_version, cur.src_tag_version), (2, 6));
    assert!(!derived::track_is_missing(&c, 1).unwrap());
    c.execute("UPDATE tracks SET missing_since = 1 WHERE id = 1", [])
        .unwrap();
    assert!(derived::track_is_missing(&c, 1).unwrap());
    assert!(derived::delete(&c, 1).unwrap());
    assert!(!derived::delete(&c, 1).unwrap());
    assert!(derived::get(&c, 1).unwrap().is_none());
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
    derived::upsert(
        &c,
        1,
        "A/01.opus",
        "opus",
        None,
        1,
        TagState {
            src_rg_scanned_at: Some(100),
            ..tags(1, None)
        },
        0,
    )
    .unwrap();
    assert_eq!(derived::enqueue_if_stale(&c, 1, 0).unwrap(), None);
    // 同じ秒に値が変わる再解析 → 世代は 101 に進み、Derived が古いと判定される
    dbrg::store(&c, &[(1, v(-7.0))], 100).unwrap();
    assert_eq!(scanned(&c), 101);
    assert!(derived::enqueue_if_stale(&c, 1, 0).unwrap().is_some());
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
    derived::upsert(&c, 1, "A/01.opus", "opus", None, 1, tags(1, Some(7)), 0).unwrap();
    derived::upsert(&c, 2, "A/02.opus", "opus", None, 1, tags(1, Some(7)), 0).unwrap();
    let ids = derived::enqueue_all_stale(&c, 100).unwrap();
    assert_eq!(job_track_ids(&c, &ids), vec![1]);
}

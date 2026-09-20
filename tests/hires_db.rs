//! `db::hires`（SPEC §7.10、P3-5、D-71）: 対象の条件、版付きの記録、投入
use rusqlite::Connection;
use spindle::db::hires::{self, Status};
use spindle::db::migrations;

fn conn() -> Connection {
    let mut c = Connection::open_in_memory().unwrap();
    c.pragma_update(None, "foreign_keys", "ON").unwrap();
    migrations::apply_list(&mut c, &migrations::embedded().unwrap()).unwrap();
    c
}

fn insert(
    c: &Connection,
    id: i64,
    codec: &str,
    lossless: bool,
    rate: Option<i64>,
    bits: Option<i64>,
    missing: bool,
) {
    c.execute(
        "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                             sample_rate, bit_depth, missing_since, inode, audio_version,
                             title, artist_display, album, albumartist, seen_at)
         VALUES (?1, ?2, ?2, 10, 1, 1, ?3, ?4, ?5, ?6, ?7, ?1, 1, 't', 'a', 'al', 'aa', 0)",
        rusqlite::params![
            id,
            format!("a/{id}.{codec}"),
            codec,
            lossless,
            rate,
            bits,
            if missing { Some(1) } else { None::<i64> }
        ],
    )
    .unwrap();
}

#[test]
fn only_lossless_hires_active_tracks_are_targets() {
    let c = conn();
    insert(&c, 1, "flac", true, Some(96_000), Some(24), false); // 対象
    insert(&c, 2, "flac", true, Some(44_100), Some(24), false); // 対象（bit のみ）
    insert(&c, 3, "flac", true, Some(96_000), Some(16), false); // 対象（rate のみ）
    insert(&c, 4, "flac", true, Some(44_100), Some(16), false); // 対象外
    insert(&c, 5, "opus", false, Some(96_000), None, false); // 対象外（非可逆）
    insert(&c, 6, "flac", true, Some(96_000), Some(24), true); // 対象外（missing）
    let ids = hires::enqueue_all_unchecked(&c, 1).unwrap();
    assert_eq!(ids.len(), 3);
    let types: Vec<(String, String)> = c
        .prepare("SELECT type, dedup_key FROM jobs ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        types,
        vec![
            ("hirescheck".into(), "hirescheck:1:1".into()),
            ("hirescheck".into(), "hirescheck:2:1".into()),
            ("hirescheck".into(), "hirescheck:3:1".into()),
        ]
    );
    // 再投入は dedup で 0 件
    assert!(hires::enqueue_all_unchecked(&c, 2).unwrap().is_empty());
    // selection: 対象外は skipped、投入済みは duplicates
    let (ids, skipped, dup) = hires::enqueue_selection(&c, &[1, 4, 5, 6], 3).unwrap();
    assert!(ids.is_empty());
    assert_eq!((skipped, dup), (3, 1));
}

#[test]
fn record_is_versioned_and_keeps_measurements() {
    let c = conn();
    insert(&c, 1, "flac", true, Some(96_000), Some(24), false);
    assert!(hires::record(
        &c,
        1,
        1,
        Status::Upsampled,
        None,
        Some(22_050),
        Some(48.5),
        Some(24),
        100
    )
    .unwrap());
    type Row = (
        String,
        i64,
        i64,
        Option<String>,
        Option<i64>,
        Option<f64>,
        Option<i64>,
    );
    let row: Row = c
        .query_row(
            "SELECT hires_check, hires_checked_at, hires_check_version, hires_check_error,
                    hires_cutoff_hz, hires_cliff_db, hires_effective_bits FROM tracks WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        row,
        (
            "upsampled".into(),
            100,
            1,
            None,
            Some(22_050),
            Some(48.5),
            Some(24)
        )
    );
    // 版が進んでいれば書かない。結果が版付きなので enqueue_all_unchecked の対象に戻る
    c.execute("UPDATE tracks SET audio_version = 2 WHERE id = 1", [])
        .unwrap();
    assert!(!hires::record(&c, 1, 1, Status::Ok, None, None, None, None, 101).unwrap());
    assert_eq!(hires::enqueue_all_unchecked(&c, 1).unwrap().len(), 1);
    // decode_error はエラー文と NULL の計測値
    assert!(hires::record(
        &c,
        1,
        2,
        Status::DecodeError,
        Some("boom"),
        None,
        None,
        None,
        102
    )
    .unwrap());
    let t = hires::load_target(&c, 1).unwrap().unwrap();
    assert!(t.eligible());
    assert_eq!(t.audio_version, 2);
    assert!(hires::load_target(&c, 99).unwrap().is_none());
    for s in [
        Status::Ok,
        Status::Upsampled,
        Status::Padded,
        Status::Both,
        Status::Inconclusive,
        Status::DecodeError,
    ] {
        assert_eq!(Status::parse(s.as_str()), Some(s));
    }
    assert_eq!(Status::parse("bogus"), None);
}

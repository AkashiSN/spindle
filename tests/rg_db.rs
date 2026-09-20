//! `db::replaygain` の album gain 属性（D-74、P4-5）。投入単位と off にしたときの片付け

use rusqlite::Connection;
use spindle::db::migrations;
use spindle::db::replaygain as dbrg;

fn conn() -> Connection {
    let mut c = Connection::open_in_memory().unwrap();
    c.pragma_update(None, "foreign_keys", "ON").unwrap();
    migrations::apply_list(&mut c, &migrations::embedded().unwrap()).unwrap();
    c.execute_batch(
        "INSERT INTO albums (id, rel_dir, rel_dir_key, album_gain) VALUES (1, 'A', 'a', 1), (2, 'B', 'b', 0);
         INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                             title, artist_display, album, albumartist, seen_at, album_id,
                             rg_track_gain, rg_track_peak, rg_album_gain, rg_album_peak, rg_scanned_at, rg_written_at)
           VALUES (11, 'A/1.flac', 'a/1.flac', 0, 0, 0, 'flac', 1, 't', 'a', 'al', 'aa', 0, 1, -1.0, 0.9, -2.0, 0.95, 100, 100),
                  (12, 'A/2.flac', 'a/2.flac', 0, 0, 0, 'flac', 1, 't', 'a', 'al', 'aa', 0, 1, NULL, NULL, NULL, NULL, NULL, NULL),
                  (21, 'B/1.flac', 'b/1.flac', 0, 0, 0, 'flac', 1, 't', 'a', 'al', 'aa', 0, 2, -1.0, 0.9, NULL, NULL, 100, 100),
                  (31, 'C/1.flac', 'c/1.flac', 0, 0, 0, 'flac', 1, 't', 'a', 'al', 'aa', 0, NULL, NULL, NULL, NULL, NULL, NULL, NULL);",
    )
    .unwrap();
    c
}

#[test]
fn scopes_follow_the_album_attribute() {
    let c = conn();
    let (albums, tracks) = dbrg::scopes_of(&c, &[11, 12, 21, 31, 999]).unwrap();
    assert_eq!(albums, vec![1], "album_gain=1 の album だけ album 単位");
    assert_eq!(
        tracks,
        vec![21, 31],
        "album_gain=0 の album のトラックと album 無しは track 単位"
    );
}

#[test]
fn enabled_reads_the_attribute() {
    let c = conn();
    assert!(dbrg::album_gain_enabled(&c, 1).unwrap());
    assert!(!dbrg::album_gain_enabled(&c, 2).unwrap());
    assert!(
        !dbrg::album_gain_enabled(&c, 999).unwrap(),
        "無い album は false"
    );
}

#[test]
fn turning_off_clears_album_values_and_bumps_generation() {
    let c = conn();
    let ch = dbrg::set_album_gain(&c, 1, false, 500).unwrap().unwrap();
    assert!(ch.changed);
    assert_eq!(ch.cleared, vec![11], "album 値を持っていた行だけ");
    let (ag, ap, scanned, written): (Option<f64>, Option<f64>, Option<i64>, Option<i64>) = c
        .query_row(
            "SELECT rg_album_gain, rg_album_peak, rg_scanned_at, rg_written_at FROM tracks WHERE id = 11",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!((ag, ap, written), (None, None, None));
    assert_eq!(scanned, Some(500), "rg_scanned_at は max(now, 前 + 1)");
    assert!(!dbrg::album_gain_enabled(&c, 1).unwrap());
    // 同じ値をもう一度: 変化なし
    let again = dbrg::set_album_gain(&c, 1, false, 501).unwrap().unwrap();
    assert!(!again.changed);
    assert!(again.cleared.is_empty());
}

#[test]
fn turning_on_only_flips_the_attribute() {
    let c = conn();
    let ch = dbrg::set_album_gain(&c, 2, true, 500).unwrap().unwrap();
    assert!(ch.changed);
    assert!(ch.cleared.is_empty());
    assert!(dbrg::album_gain_enabled(&c, 2).unwrap());
    let written: Option<i64> = c
        .query_row("SELECT rg_written_at FROM tracks WHERE id = 21", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        written,
        Some(100),
        "on にしただけでは行を触らない（次の album 解析で揃う）"
    );
    assert!(dbrg::set_album_gain(&c, 999, true, 500).unwrap().is_none());
}

#[test]
fn album_values_of_returns_current_row_values() {
    let c = conn();
    assert_eq!(
        dbrg::album_values_of(&c, 11).unwrap(),
        Some((Some(-2.0), Some(0.95)))
    );
    assert_eq!(dbrg::album_values_of(&c, 21).unwrap(), Some((None, None)));
    assert_eq!(dbrg::album_values_of(&c, 999).unwrap(), None);
}

/// off の直後に、その前から走っていた解析が同じ値で保存しても `rg_scanned_at` は巻き戻らない
/// （Derived の `src_rg_scanned_at` との差分が消えると追随が抜ける。D-74）
#[test]
fn store_never_moves_rg_scanned_at_backwards() {
    use spindle::db::replaygain::Values;
    let c = conn();
    // 11: (-1.0, 0.9, -2.0, 0.95, scanned 100, written 100)。off で 101 に進む
    let ch = dbrg::set_album_gain(&c, 1, false, 100).unwrap().unwrap();
    assert_eq!(ch.cleared, vec![11]);
    let scanned = |id: i64| -> (Option<i64>, Option<i64>) {
        c.query_row(
            "SELECT rg_scanned_at, rg_written_at FROM tracks WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    assert_eq!(scanned(11), (Some(101), None));
    // 同じ秒（now = 100）に、属性を読み直して album 値 NULL・track 値は同じで保存
    let same = Values {
        track_gain: -1.0,
        track_peak: 0.9,
        album_gain: None,
        album_peak: None,
    };
    assert_eq!(dbrg::store(&c, &[(11, same)], 100).unwrap(), 1);
    assert_eq!(scanned(11), (Some(101), None), "同じ値でも 101 のまま");
    // 値が変わる保存も前より小さくならない
    let changed = Values {
        track_gain: -1.5,
        ..same
    };
    assert_eq!(dbrg::store(&c, &[(11, changed)], 100).unwrap(), 1);
    assert_eq!(scanned(11), (Some(102), None));
    // 確認済み（written >= scanned）の行に同じ値を小さい now で保存しても written は下がらない
    assert_eq!(scanned(21), (Some(100), Some(100)));
    assert_eq!(dbrg::store(&c, &[(21, same)], 50).unwrap(), 1);
    assert_eq!(scanned(21), (Some(100), Some(100)));
}

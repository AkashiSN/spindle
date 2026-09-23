//! `db/migrations/*.sql` がバイナリへ埋め込まれ、連番で列挙できること。
//! 適用は P0-2。ここでは埋め込みと命名規則だけを固定する。

use spindle::db::migrations;

#[test]
fn embedded_migrations_start_at_0001_init() {
    let list = migrations::embedded().expect("埋め込みマイグレーションが列挙できる");
    assert!(!list.is_empty());
    assert_eq!(list[0].version, 1);
    assert_eq!(list[0].name, "init");
    assert_eq!(list[0].file_name, "0001_init.sql");
    assert!(list[0].sql.contains("CREATE TABLE schema_version"));
}

#[test]
fn embedded_migrations_are_contiguous_and_sorted() {
    let list = migrations::embedded().unwrap();
    for (i, m) in list.iter().enumerate() {
        assert_eq!(
            m.version,
            (i + 1) as u32,
            "{} が連番になっていない",
            m.file_name
        );
        assert!(!m.sql.trim().is_empty(), "{} が空", m.file_name);
    }
}

#[test]
fn embedded_matches_directory_on_disk() {
    // Dockerfile の COPY 範囲（db/migrations/）とビルド時の埋め込みが一致すること
    let mut on_disk: Vec<String> =
        std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/db/migrations"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".sql"))
            .collect();
    on_disk.sort();
    let embedded: Vec<String> = migrations::embedded()
        .unwrap()
        .into_iter()
        .map(|m| m.file_name)
        .collect();
    assert_eq!(embedded, on_disk);
}

#[test]
fn file_name_parsing_rejects_bad_names() {
    use migrations::parse_file_name;
    assert_eq!(
        parse_file_name("0001_init.sql"),
        Some((1, "init".to_string()))
    );
    assert_eq!(
        parse_file_name("0042_add_thing.sql"),
        Some((42, "add_thing".to_string()))
    );
    assert_eq!(parse_file_name("1_init.sql"), None, "4 桁ゼロ埋め必須");
    assert_eq!(parse_file_name("0001-init.sql"), None);
    assert_eq!(parse_file_name("0001_init.txt"), None);
    assert_eq!(parse_file_name("0000_init.sql"), None, "0 は使わない");
    assert_eq!(parse_file_name("0001_.sql"), None, "名前が空");
}

// ---------------------------------------------------------------- 0006 playlists.name_key（P1-6、D-53）

/// 版 5 相当の DB に既存のプレイリストがある状態から 0006 を当てると、name_key が Rust の
/// canonical_key と同じ値で埋まり、FS 上で同じファイルになる名前は改名されて一意になる
#[test]
fn upgrade_to_0006_backfills_name_key_with_canonical_key_and_renames_collisions() {
    use rusqlite::Connection;
    use spindle::domain::relpath::canonical_key;

    let list = migrations::embedded().unwrap();
    let upto5: Vec<_> = list.iter().take(5).cloned().collect();
    let mut conn = Connection::open_in_memory().unwrap();
    migrations::apply_list(&mut conn, &upto5).unwrap();
    assert_eq!(migrations::current_version(&conn).unwrap(), Some(5));
    // NFC の「が」、大小文字違い、無関係な 1 本、改名先と二次衝突する既存名、上限付近の名前
    let long = "x".repeat(250); // + ".m3u8" でちょうど 255 バイト
    let long_upper = format!("X{}", "x".repeat(249));
    conn.execute(
        "INSERT INTO playlists (id, name, created_at, updated_at) VALUES
           (1, 'Foo', 1, 1), (2, 'foo', 1, 1), (3, '\u{304c}', 1, 1), (4, 'Bar', 1, 1), (5, 'FOO', 1, 1),
           (6, 'foo (2)', 1, 1), (7, ?1, 1, 1), (8, ?2, 1, 1)",
        [&long, &long_upper],
    )
    .unwrap();

    migrations::apply_list(&mut conn, &list).unwrap();

    let rows: Vec<(i64, String, String)> = conn
        .prepare("SELECT id, name, name_key FROM playlists ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    // 同じ key の最初の 1 本（id 最小）は名前を保ち、後続は空いている " (n)" を付けて残す（消さない）。
    // "foo (2)" は id=6 が元から持っているので id=2 は "foo (3)" へ
    assert_eq!(rows[0], (1, "Foo".to_owned(), "foo".to_owned()));
    assert_eq!(rows[1], (2, "foo (3)".to_owned(), "foo (3)".to_owned()));
    assert_eq!(rows[3], (4, "Bar".to_owned(), "bar".to_owned()));
    assert_eq!(rows[4], (5, "FOO (4)".to_owned(), "foo (4)".to_owned()));
    assert_eq!(rows[5], (6, "foo (2)".to_owned(), "foo (2)".to_owned()));
    // 上限付近: 改名しても ".m3u8" 込みで 255 バイトに収まる（名前側を削る）
    assert_eq!(rows[6].1, long);
    assert!(rows[7].1.ends_with(" (2)"), "{}", rows[7].1);
    assert!(
        rows[7].1.len() + ".m3u8".len() <= 255,
        "{}",
        rows[7].1.len()
    );
    assert_ne!(rows[7].2, rows[6].2);
    // key はすべて一意
    let keys: std::collections::HashSet<&str> = rows.iter().map(|r| r.2.as_str()).collect();
    assert_eq!(keys.len(), rows.len());
    let idx: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_playlists_name_key'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx, 1);
    // NFC は NFD に畳まれる（Rust 側と同じ）
    assert_eq!(rows[2].1, "\u{304c}");
    assert_eq!(rows[2].2, canonical_key("\u{304c}"));
    assert_eq!(rows[2].2, "\u{304b}\u{3099}");
    // 以後の作成は同じ key で弾かれる
    let dup = conn.execute(
        "INSERT INTO playlists (name, name_key, created_at, updated_at) VALUES ('か゛', ?1, 1, 1)",
        [canonical_key("\u{304b}\u{3099}")],
    );
    assert!(dup.is_err());
}

#[test]
fn upgrade_to_0011_widens_edit_ops_kind_and_keeps_referencing_rows() {
    use rusqlite::Connection;

    // 0011 は edit_ops の CHECK を広げるために表を作り直す。edits（ON DELETE CASCADE）と
    // archived_files（ON DELETE SET NULL）が参照しているので、FK を切らずに DROP すると履歴が消える
    let list = migrations::embedded().unwrap();
    let upto10: Vec<_> = list.iter().take(10).cloned().collect();
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    migrations::apply_list(&mut conn, &upto10).unwrap();
    conn.execute_batch(
        "INSERT INTO edit_batches (id, created_at, state, affected) VALUES (1, 1, 'applied', 1);
         INSERT INTO edit_ops (id, batch_id, ordinal, track_id, kind, result, expected_rel_path)
           VALUES (10, 1, 0, 5, 'tags', 'applied', 'a/b.flac');
         INSERT INTO edits (op_id, key, old_value, new_value) VALUES (10, 'TITLE', '[\"x\"]', '[\"y\"]');
         INSERT INTO archived_files (id, track_id, op_id, rel_path, rel_path_key, source_rel_path, reason,
                                     archived_at, eligible_after, state)
           VALUES (1, 5, 10, 'a/b.wav', 'a/b.wav', 'a/b.wav', 'normalize', 1, 2, 'held');",
    )
    .unwrap();
    // 0011 の前は md5 が通らない
    assert!(conn
        .execute(
            "INSERT INTO edit_ops (batch_id, ordinal, track_id, kind) VALUES (1, 1, 6, 'md5')",
            []
        )
        .is_err());

    migrations::apply_list(&mut conn, &list).unwrap();
    assert!(migrations::current_version(&conn).unwrap().unwrap() >= 11);

    // 参照している行が残っている（CASCADE / SET NULL が走っていない）
    let edits: i64 = conn
        .query_row("SELECT count(*) FROM edits WHERE op_id = 10", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(edits, 1);
    let op_id: Option<i64> = conn
        .query_row("SELECT op_id FROM archived_files WHERE id = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(op_id, Some(10));
    let (kind, path): (String, String) = conn
        .query_row(
            "SELECT kind, expected_rel_path FROM edit_ops WHERE id = 10",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((kind.as_str(), path.as_str()), ("tags", "a/b.flac"));
    // FK は有効なまま（作り直しの後で戻っている）
    let fk: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fk, 1);
    // md5 が通り、索引（pending の一意制約）も生きている
    conn.execute(
        "INSERT INTO edit_ops (batch_id, ordinal, track_id, kind) VALUES (1, 1, 6, 'md5')",
        [],
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO edit_ops (batch_id, ordinal, track_id, kind) VALUES (1, 2, 6, 'md5')",
            []
        )
        .is_err(),
        "同じトラックの pending は 1 つ（idx_edit_ops_pending）"
    );
    // FK も効いている（無い batch）
    assert!(conn
        .execute(
            "INSERT INTO edit_ops (batch_id, ordinal, track_id, kind) VALUES (99, 0, 7, 'tags')",
            []
        )
        .is_err());
}

#[test]
fn upgrade_to_0012_adds_track_artwork_with_set_null_on_delete() {
    use rusqlite::Connection;

    let list = migrations::embedded().unwrap();
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    migrations::apply_list(&mut conn, &list).unwrap();
    assert!(migrations::current_version(&conn).unwrap().unwrap() >= 12);
    conn.execute_batch(
        "INSERT INTO artwork (id, sha256, mime, bytes, origin) VALUES (7, zeroblob(32), 'image/jpeg', 1, 'embedded');
         INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                             audio_version, tag_version, seen_at, artwork_id)
           VALUES (1, 'a.flac', 'a.flac', 1, 0, 0, 'flac', 1, 1, 1, 0, 7);",
    )
    .unwrap();
    // 既定は NULL、artwork 行が消えれば NULL に戻る（行は消えない）。artwork_dirty の既定は 0
    conn.execute("DELETE FROM artwork WHERE id = 7", [])
        .unwrap();
    let (v, dirty): (Option<i64>, i64) = conn
        .query_row(
            "SELECT artwork_id, artwork_dirty FROM tracks WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((v, dirty), (None, 0));
}

// ---------------------------------------------------------------- 0015 jobs.type に ytdl（P3-3、D-70）

/// 0015 は jobs の CHECK に ytdl を足すために表を作り直す。jobs を参照する子表（edit_ops SET NULL、
/// derived_path_locks / job_mutexes / track_locks CASCADE、album_verifications SET NULL）の行が残り、
/// FK と索引が戻ること
#[test]
fn upgrade_to_0015_adds_ytdl_and_keeps_rows_referencing_jobs() {
    use rusqlite::Connection;

    let list = migrations::embedded().unwrap();
    let upto14: Vec<_> = list.iter().take(14).cloned().collect();
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    migrations::apply_list(&mut conn, &upto14).unwrap();
    conn.execute_batch(
        "INSERT INTO jobs (id, type, dedup_key, payload, state, created_at)
           VALUES (1, 'scan', 'scan', '{}', 'done', 1), (2, 'rg', 'rg:1', '{}', 'queued', 2);
         INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                             title, artist_display, album, albumartist, seen_at)
           VALUES (5, 'a/b.flac', 'a/b.flac', 0, 0, 0, 'flac', 1, 't', 'a', 'al', 'aa', 0);
         INSERT INTO albums (id, rel_dir, rel_dir_key) VALUES (3, 'a', 'a');
         INSERT INTO edit_batches (id, created_at, state, affected) VALUES (1, 1, 'applied', 1);
         INSERT INTO edit_ops (id, batch_id, ordinal, track_id, kind, result, job_id)
           VALUES (10, 1, 0, 5, 'tags', 'applied', 1);
         INSERT INTO derived_path_locks (rel_path_key, track_id, job_id, acquired_at) VALUES ('x', 5, 2, 1);
         INSERT INTO job_mutexes (name, job_id, acquired_at) VALUES ('library', 2, 1);
         INSERT INTO track_locks (track_id, job_id, acquired_at) VALUES (5, 2, 1);
         INSERT INTO album_verifications (album_id, method, result, source, verified_at, disc_no, job_id)
           VALUES (3, 'ctdb', 'verified', 'retro', 1, 1, 1);",
    )
    .unwrap();
    // 0015 の前は ytdl が通らない
    assert!(conn
        .execute(
            "INSERT INTO jobs (type, payload, created_at) VALUES ('ytdl', '{}', 1)",
            []
        )
        .is_err());

    migrations::apply_list(&mut conn, &list).unwrap();
    assert!(migrations::current_version(&conn).unwrap().unwrap() >= 15);

    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT count(*) FROM jobs"), 2);
    assert_eq!(count("SELECT count(*) FROM edit_ops WHERE job_id = 1"), 1);
    assert_eq!(
        count("SELECT count(*) FROM derived_path_locks WHERE job_id = 2"),
        1
    );
    assert_eq!(
        count("SELECT count(*) FROM job_mutexes WHERE job_id = 2"),
        1
    );
    assert_eq!(
        count("SELECT count(*) FROM track_locks WHERE job_id = 2"),
        1
    );
    assert_eq!(
        count("SELECT count(*) FROM album_verifications WHERE job_id = 1"),
        1
    );
    assert_eq!(count("PRAGMA foreign_keys"), 1);
    assert_eq!(
        count("SELECT count(*) FROM pragma_foreign_key_check"),
        0,
        "参照の整合"
    );
    // ytdl が通り、dedup の部分一意索引が生きている
    conn.execute(
        "INSERT INTO jobs (type, dedup_key, payload, created_at) VALUES ('ytdl', 'ytdl:u', '{}', 1)",
        [],
    )
    .unwrap();
    assert!(conn
        .execute(
            "INSERT INTO jobs (type, dedup_key, payload, created_at) VALUES ('ytdl', 'ytdl:u', '{}', 1)",
            []
        )
        .is_err());
    // FK も効いている（無い job を参照する子行、jobs を消すと CASCADE）
    assert!(conn
        .execute(
            "INSERT INTO track_locks (track_id, job_id, acquired_at) VALUES (5, 99, 1)",
            []
        )
        .is_err());
    conn.execute("DELETE FROM jobs WHERE id = 2", []).unwrap();
    assert_eq!(count("SELECT count(*) FROM track_locks"), 0);
    conn.execute("DELETE FROM jobs WHERE id = 1", []).unwrap();
    assert_eq!(
        count("SELECT edit_ops.job_id IS NULL FROM edit_ops WHERE id = 10"),
        1
    );
}

// ---------------------------------------------------------------- 0016 tracks.hires_* と jobs.type に hirescheck（P3-5、D-71）

/// 0016 は tracks に偽ハイレゾ検出の列を足し、jobs の CHECK に hirescheck を足すために表を作り直す
/// （0015 と同じ手順）。子表の行と FK・索引が戻ること
#[test]
fn upgrade_to_0016_adds_hires_columns_and_hirescheck_type() {
    use rusqlite::Connection;

    let list = migrations::embedded().unwrap();
    let upto15: Vec<_> = list.iter().take(15).cloned().collect();
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    migrations::apply_list(&mut conn, &upto15).unwrap();
    conn.execute_batch(
        "INSERT INTO jobs (id, type, dedup_key, payload, state, created_at)
           VALUES (1, 'scan', 'scan', '{}', 'done', 1), (2, 'rg', 'rg:1', '{}', 'queued', 2);
         INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                             title, artist_display, album, albumartist, seen_at)
           VALUES (5, 'a/b.flac', 'a/b.flac', 0, 0, 0, 'flac', 1, 't', 'a', 'al', 'aa', 0);
         INSERT INTO track_locks (track_id, job_id, acquired_at) VALUES (5, 2, 1);",
    )
    .unwrap();
    assert!(conn
        .execute(
            "INSERT INTO jobs (type, payload, created_at) VALUES ('hirescheck', '{}', 1)",
            []
        )
        .is_err());

    migrations::apply_list(&mut conn, &list).unwrap();
    assert!(migrations::current_version(&conn).unwrap().unwrap() >= 16);
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT count(*) FROM jobs"), 2);
    assert_eq!(
        count("SELECT count(*) FROM track_locks WHERE job_id = 2"),
        1
    );
    assert_eq!(count("SELECT count(*) FROM pragma_foreign_key_check"), 0);
    // 新しい列は NULL で始まり、CHECK が効く
    assert_eq!(
        count(
            "SELECT count(*) FROM tracks WHERE hires_check IS NULL AND hires_cutoff_hz IS NULL
               AND hires_cliff_db IS NULL AND hires_effective_bits IS NULL"
        ),
        1
    );
    conn.execute(
        "UPDATE tracks SET hires_check = 'upsampled', hires_checked_at = 1, hires_check_version = 1,
                           hires_cutoff_hz = 22050, hires_cliff_db = 48.5, hires_effective_bits = 16 WHERE id = 5",
        [],
    )
    .unwrap();
    assert!(conn
        .execute("UPDATE tracks SET hires_check = 'bogus' WHERE id = 5", [])
        .is_err());
    conn.execute(
        "INSERT INTO jobs (type, dedup_key, payload, created_at) VALUES ('hirescheck', 'hirescheck:5:1', '{}', 1)",
        [],
    )
    .unwrap();
    assert!(conn
        .execute(
            "INSERT INTO jobs (type, dedup_key, payload, created_at) VALUES ('hirescheck', 'hirescheck:5:1', '{}', 1)",
            []
        )
        .is_err());
    conn.execute("DELETE FROM jobs WHERE id = 2", []).unwrap();
    assert_eq!(count("SELECT count(*) FROM track_locks"), 0);
}

// ---------------------------------------------------------------- 0017 albums.album_gain（P4-5、D-74）

/// 0017 は albums に album_gain（既定 0）を足し、既存の rg_album_* を NULL に揃える。値を持っていた行は
/// rg_written_at が NULL に戻り rg_scanned_at が 1 進む（Derived の追随）。持っていなかった行は触らない
#[test]
fn upgrade_to_0017_adds_album_gain_and_clears_album_values() {
    use rusqlite::Connection;

    let list = migrations::embedded().unwrap();
    let upto16: Vec<_> = list.iter().take(16).cloned().collect();
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    migrations::apply_list(&mut conn, &upto16).unwrap();
    conn.execute_batch(
        "INSERT INTO albums (id, rel_dir, rel_dir_key) VALUES (1, 'A/B', 'a/b');
         INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                             title, artist_display, album, albumartist, seen_at, album_id,
                             rg_track_gain, rg_track_peak, rg_album_gain, rg_album_peak,
                             rg_scanned_at, rg_written_at)
           VALUES (5, 'A/B/1.flac', 'a/b/1.flac', 0, 0, 0, 'flac', 1, 't', 'a', 'al', 'aa', 0, 1,
                   -1.5, 0.9, -2.0, 0.95, 100, 100),
                  (6, 'A/B/2.flac', 'a/b/2.flac', 0, 0, 0, 'flac', 1, 't', 'a', 'al', 'aa', 0, 1,
                   -1.0, 0.8, NULL, NULL, 100, 100);",
    )
    .unwrap();

    migrations::apply_list(&mut conn, &list).unwrap();
    assert!(migrations::current_version(&conn).unwrap().unwrap() >= 17);
    let album_gain: i64 = conn
        .query_row("SELECT album_gain FROM albums WHERE id = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(album_gain, 0, "既存の album は off");
    assert!(
        conn.execute("UPDATE albums SET album_gain = 2 WHERE id = 1", [])
            .is_err(),
        "CHECK (0, 1)"
    );
    let row = |id: i64| -> (Option<f64>, Option<f64>, Option<i64>, Option<i64>) {
        conn.query_row(
            "SELECT rg_album_gain, rg_album_peak, rg_scanned_at, rg_written_at FROM tracks WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
    };
    assert_eq!(
        row(5),
        (None, None, Some(101), None),
        "album 値を持っていた行"
    );
    assert_eq!(
        row(6),
        (None, None, Some(100), Some(100)),
        "持っていなかった行は据え置き"
    );
}

// ---------------------------------------------------------------- 0018 derived_files を系統ごとに（P4-7、D-75）

/// 0018 は delivery を落として derived_files を (track_id, variant) 主キーで作り直し、既存行を opus
/// 系統（audio_profile は当時のビットレート）として移し、delivery を opus 固定で作り直す
#[test]
fn upgrade_to_0018_rebuilds_derived_files_per_variant() {
    use rusqlite::Connection;

    let list = migrations::embedded().unwrap();
    let upto17: Vec<_> = list.iter().take(17).cloned().collect();
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    migrations::apply_list(&mut conn, &upto17).unwrap();
    conn.execute_batch(
        "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                             title, artist_display, album, albumartist, seen_at, audio_version, tag_version)
           VALUES (5, 'A/B/1.flac', 'a/b/1.flac', 0, 0, 0, 'flac', 1, 't', 'a', 'al', 'aa', 0, 1, 2),
                  (6, 'A/B/2.flac', 'a/b/2.flac', 0, 0, 0, 'flac', 1, 't', 'a', 'al', 'aa', 0, 1, 1);
         INSERT INTO derived_files (track_id, rel_path, rel_path_key, codec, bitrate, src_audio_version,
                                    src_tag_version, generated_at, src_rg_scanned_at)
           VALUES (5, 'A/B/1.opus', 'a/b/1.opus', 'opus', 128, 1, 1, 10, 7);",
    )
    .unwrap();

    migrations::apply_list(&mut conn, &list).unwrap();
    assert!(migrations::current_version(&conn).unwrap().unwrap() >= 18);
    let row: (String, String, i64, String, String, Option<i64>) = conn
        .query_row(
            "SELECT variant, rel_path, bitrate, audio_profile, tag_profile, src_rg_scanned_at
               FROM derived_files WHERE track_id = 5",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        row,
        (
            "opus".into(),
            "A/B/1.opus".into(),
            128,
            "opus:128:v1".into(),
            "opus:v1".into(),
            Some(7)
        ),
        "既存行は opus 系統、パスはルート直下のまま"
    );
    // delivery は opus 系統を指し、タグ版が違うので stale_tags
    let d: (String, String, i64) = conn
        .query_row(
            "SELECT path, codec, stale_tags FROM delivery WHERE track_id = 5",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(d, ("Derived/A/B/1.opus".into(), "opus".into(), 1));
    let d6: (String, String) = conn
        .query_row(
            "SELECT path, codec FROM delivery WHERE track_id = 6",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(d6, ("Library/A/B/2.flac".into(), "flac".into()));
    // 同じトラックに aac 系統の 2 行目を持てる。aac は delivery に影響しない
    conn.execute(
        "INSERT INTO derived_files (track_id, variant, rel_path, rel_path_key, codec, bitrate,
                                    src_audio_version, src_tag_version, generated_at, audio_profile, tag_profile)
           VALUES (5, 'aac', 'aac/A/B/1.m4a', 'aac/a/b/1.m4a', 'aac', 256, 1, 2, 11, 'aac:256:v1', 'aac:v1')",
        [],
    )
    .unwrap();
    let d: (String, i64) = conn
        .query_row(
            "SELECT path, stale_tags FROM delivery WHERE track_id = 5",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(d, ("Derived/A/B/1.opus".into(), 1));
    assert!(conn
        .execute(
            "INSERT INTO derived_files (track_id, variant, rel_path, rel_path_key, codec, src_audio_version,
                                        src_tag_version, generated_at, audio_profile, tag_profile)
               VALUES (6, 'ogg', 'x', 'x', 'ogg', 1, 1, 1, 'a', 'b')",
            []
        )
        .is_err(), "variant の CHECK");
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(
        count("SELECT count(*) FROM derived_variants"),
        0,
        "設定は起動時に写す"
    );
    // トラックを消すと系統ごとの行も消える（CASCADE）
    conn.execute("DELETE FROM tracks WHERE id = 5", []).unwrap();
    assert_eq!(
        count("SELECT count(*) FROM derived_files WHERE track_id = 5"),
        0
    );
}

/// 0019: `derived_variants` に aac 系統の `lossy_sources` / `multi_value_separator`（D-75、P4-8）。
/// 既存行（opus）は既定値で埋まり、CHECK が効く
#[test]
fn upgrade_to_0019_adds_variant_options_with_defaults() {
    use rusqlite::Connection;

    let list = migrations::embedded().unwrap();
    let upto18: Vec<_> = list.iter().take(18).cloned().collect();
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    migrations::apply_list(&mut conn, &upto18).unwrap();
    conn.execute_batch(
        "INSERT INTO derived_variants (variant, enabled, audio_profile, tag_profile, codec, bitrate, updated_at)
           VALUES ('opus', 1, 'opus:256:v1', 'opus:v1', 'opus', 256, 1);",
    )
    .unwrap();

    migrations::apply_list(&mut conn, &list).unwrap();
    assert!(migrations::current_version(&conn).unwrap().unwrap() >= 19);
    let row: (i64, String) = conn
        .query_row(
            "SELECT lossy_sources, multi_value_separator FROM derived_variants WHERE variant = 'opus'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(row, (0, " & ".into()), "既存行は既定値");
    conn.execute(
        "INSERT INTO derived_variants (variant, enabled, audio_profile, tag_profile, codec, bitrate,
                                       updated_at, lossy_sources, multi_value_separator)
           VALUES ('aac', 1, 'aac:256:48k:bake1', 'aac:sep= & :itunnorm0:v1', 'aac', 256, 1, 1, ' / ')",
        [],
    )
    .unwrap();
    let bad = conn.execute(
        "UPDATE derived_variants SET lossy_sources = 2 WHERE variant = 'aac'",
        [],
    );
    assert!(bad.is_err(), "lossy_sources は 0 / 1 のみ");
}

/// P4-13: ジョブの完了時の結果 1 行（`note`）。既存行は NULL のまま
#[test]
fn upgrade_to_0020_adds_jobs_note() {
    use rusqlite::Connection;

    let list = migrations::embedded().unwrap();
    let upto19: Vec<_> = list.iter().take(19).cloned().collect();
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    migrations::apply_list(&mut conn, &upto19).unwrap();
    conn.execute_batch(
        "INSERT INTO jobs (type, dedup_key, payload, state, created_at) VALUES ('scan', 'scan', '{}', 'done', 1);",
    )
    .unwrap();
    migrations::apply_list(&mut conn, &list).unwrap();
    assert!(migrations::current_version(&conn).unwrap().unwrap() >= 20);
    let note: Option<String> = conn
        .query_row("SELECT note FROM jobs WHERE dedup_key = 'scan'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(note, None);
    conn.execute(
        "UPDATE jobs SET note = 'Inbox に置いた: x' WHERE dedup_key = 'scan'",
        [],
    )
    .unwrap();
}

/// P4-16: 再生リストの購読（`playlist_subscriptions`）と jobs.type に playlist_sync。既存の jobs 行
/// （note 込み）と参照する子表は保つ
#[test]
fn upgrade_to_0021_adds_playlist_subscriptions_and_playlist_sync_type() {
    use rusqlite::Connection;

    let list = migrations::embedded().unwrap();
    let upto20: Vec<_> = list.iter().take(20).cloned().collect();
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    migrations::apply_list(&mut conn, &upto20).unwrap();
    conn.execute_batch(
        "INSERT INTO jobs (id, type, dedup_key, payload, state, note, created_at)
           VALUES (7, 'ytdl', 'ytdl:u', '{}', 'done', 'Inbox に置いた: x', 1);
         INSERT INTO job_mutexes (name, job_id, acquired_at) VALUES ('library', 7, 1);",
    )
    .unwrap();
    // 0021 の前は playlist_sync が通らない
    assert!(conn
        .execute(
            "INSERT INTO jobs (type, payload, created_at) VALUES ('playlist_sync', '{}', 1)",
            [],
        )
        .is_err());
    migrations::apply_list(&mut conn, &list).unwrap();
    assert!(migrations::current_version(&conn).unwrap().unwrap() >= 21);
    let (note, mutex_job): (Option<String>, i64) = conn
        .query_row(
            "SELECT j.note, m.job_id FROM jobs j JOIN job_mutexes m ON m.job_id = j.id WHERE j.id = 7",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(note.as_deref(), Some("Inbox に置いた: x"));
    assert_eq!(mutex_job, 7);
    conn.execute(
        "INSERT INTO jobs (type, dedup_key, payload, created_at) VALUES ('playlist_sync', 'playlist_sync:1', '{}', 1)",
        [],
    )
    .unwrap();
    assert!(conn
        .execute(
            "INSERT INTO jobs (type, dedup_key, payload, created_at) VALUES ('playlist_sync', 'playlist_sync:1', '{}', 1)",
            [],
        )
        .is_err(), "dedup の部分一意索引が生きている");
    let fk: i64 = conn
        .query_row("PRAGMA foreign_key_check", [], |_| Ok(1))
        .unwrap_or(0);
    assert_eq!(fk, 0, "参照の整合が崩れていない");

    // 購読: list_id と target_key は UNIQUE、album_id は非 NULL の間だけ UNIQUE、既定値
    conn.execute(
        "INSERT INTO playlist_subscriptions (list_id, url, target_key, albumartist, album, created_at, updated_at)
           VALUES ('PL1', 'https://www.youtube.com/playlist?list=PL1', 'a/b', 'A', 'B', 1, 1)",
        [],
    )
    .unwrap();
    let (align, enabled, max_enqueue, album_id): (i64, i64, i64, Option<i64>) = conn
        .query_row(
            "SELECT align, enabled, max_enqueue, album_id FROM playlist_subscriptions WHERE list_id = 'PL1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!((align, enabled, max_enqueue, album_id), (1, 1, 50, None));
    assert!(conn
        .execute(
            "INSERT INTO playlist_subscriptions (list_id, url, target_key, albumartist, album, created_at, updated_at)
               VALUES ('PL1', 'u', 'c/d', 'C', 'D', 1, 1)",
            [],
        )
        .is_err(), "list_id は UNIQUE");
    assert!(conn
        .execute(
            "INSERT INTO playlist_subscriptions (list_id, url, target_key, albumartist, album, created_at, updated_at)
               VALUES ('PL2', 'u', 'a/b', 'A', 'B', 1, 1)",
            [],
        )
        .is_err(), "target_key は UNIQUE");
    conn.execute(
        "INSERT INTO playlist_subscriptions (list_id, url, target_key, albumartist, album, created_at, updated_at)
           VALUES ('PL2', 'u', 'c/d', 'C', 'D', 1, 1)",
        [],
    )
    .unwrap();
    conn.execute_batch(
        "INSERT INTO albums (id, rel_dir, rel_dir_key, albumartist, album) VALUES (5, 'A/B', 'a/b', 'A', 'B');
         UPDATE playlist_subscriptions SET album_id = 5 WHERE list_id = 'PL1';",
    )
    .unwrap();
    assert!(
        conn.execute(
            "UPDATE playlist_subscriptions SET album_id = 5 WHERE list_id = 'PL2'",
            [],
        )
        .is_err(),
        "album_id は非 NULL の間 UNIQUE"
    );
    // album を消しても購読は残り、album_id が NULL に戻る
    conn.execute("DELETE FROM albums WHERE id = 5", []).unwrap();
    let album_id: Option<i64> = conn
        .query_row(
            "SELECT album_id FROM playlist_subscriptions WHERE list_id = 'PL1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(album_id, None);
}

/// P4-16（レビュー）: 購読の id は DELETE 後も再利用しない（AUTOINCREMENT。ジョブの payload とサイドカーが
/// 裸の id を持つ）。既存行は id を保つ
#[test]
fn upgrade_to_0022_makes_subscription_ids_non_reusable() {
    use rusqlite::Connection;

    let list = migrations::embedded().unwrap();
    let upto21: Vec<_> = list.iter().take(21).cloned().collect();
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    migrations::apply_list(&mut conn, &upto21).unwrap();
    conn.execute_batch(
        "INSERT INTO albums (id, rel_dir, rel_dir_key) VALUES (5, 'A/B', 'a/b');
         INSERT INTO playlist_subscriptions (id, list_id, url, album_id, target_key, albumartist, album, created_at, updated_at, last_result)
           VALUES (7, 'PL1', 'u', 5, 'a/b', 'A', 'B', 1, 2, '{\"state\":\"done\"}');",
    )
    .unwrap();
    migrations::apply_list(&mut conn, &list).unwrap();
    assert!(migrations::current_version(&conn).unwrap().unwrap() >= 22);
    let (album_id, result): (Option<i64>, Option<String>) = conn
        .query_row(
            "SELECT album_id, last_result FROM playlist_subscriptions WHERE id = 7",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (album_id, result.as_deref()),
        (Some(5), Some("{\"state\":\"done\"}"))
    );
    conn.execute("DELETE FROM playlist_subscriptions WHERE id = 7", [])
        .unwrap();
    conn.execute(
        "INSERT INTO playlist_subscriptions (list_id, url, target_key, albumartist, album, created_at, updated_at)
           VALUES ('PL2', 'u', 'c/d', 'C', 'D', 1, 1)",
        [],
    )
    .unwrap();
    assert_eq!(conn.last_insert_rowid(), 8, "消した 7 を再利用しない");
    // UNIQUE と SET NULL は保つ
    assert!(conn
        .execute(
            "INSERT INTO playlist_subscriptions (list_id, url, target_key, albumartist, album, created_at, updated_at)
               VALUES ('PL2', 'u', 'e/f', 'E', 'F', 1, 1)",
            [],
        )
        .is_err());
    conn.execute(
        "UPDATE playlist_subscriptions SET album_id = 5 WHERE id = 8",
        [],
    )
    .unwrap();
    conn.execute("DELETE FROM albums WHERE id = 5", []).unwrap();
    let album_id: Option<i64> = conn
        .query_row(
            "SELECT album_id FROM playlist_subscriptions WHERE id = 8",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(album_id, None);
}

/// D-67 追記 3: `albums.discid` を落とす（DiscID は 1 枚ごとの値で、album の列には収まらない）。
/// 既存の album 行と他の列は保つ
#[test]
fn upgrade_to_0024_drops_albums_discid() {
    use rusqlite::Connection;

    let list = migrations::embedded().unwrap();
    let upto23: Vec<_> = list.iter().take(23).cloned().collect();
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    migrations::apply_list(&mut conn, &upto23).unwrap();
    conn.execute_batch(
        "INSERT INTO albums (id, rel_dir, rel_dir_key, album, mb_release_id, discid, disc_count)
           VALUES (5, 'a/b', 'a/b', 'B', 'mbid-1', 'disc-1', 2);",
    )
    .unwrap();
    migrations::apply_list(&mut conn, &list).unwrap();
    assert!(migrations::current_version(&conn).unwrap().unwrap() >= 24);
    let cols: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('albums')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!cols.iter().any(|c| c == "discid"), "{cols:?}");
    let idx: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'idx_albums_discid'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx, 0);
    let row: (String, Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT album, mb_release_id, disc_count FROM albums WHERE id = 5",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(row, ("B".into(), Some("mbid-1".into()), Some(2)));
}

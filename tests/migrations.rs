//! `db/migrations/*.sql` がバイナリへ埋め込まれ、連番で列挙できること。
//! 適用は P0-2。ここでは埋め込みと命名規則と、空 DB に流した最終スキーマの性質を固定する
//! （0001〜0024 を 0001 に畳んだ時点で、各版のアップグレード試験が見ていた性質をここへ移した。D-88）。

use rusqlite::Connection;
use spindle::db::{migrations, open_memory_connection};

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

// ---------------------------------------------------------------- 最終スキーマの性質

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// 最小の tracks 行
fn insert_track(conn: &Connection, id: i64) {
    conn.execute(
        "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                             title, artist_display, album, albumartist, seen_at)
         VALUES (?1, ?2, ?2, 0, 0, 0, 'flac', 1, 't', 'a', 'al', 'aa', 0)",
        rusqlite::params![id, format!("a/{id}.flac")],
    )
    .unwrap();
}

/// 空 DB に流した直後の不変条件: FK が有効で整合し、種データは書き出しプロファイルの 3 本だけ
#[test]
fn fresh_schema_has_foreign_keys_on_and_seed_profiles() {
    let conn = open_memory_connection().unwrap();
    assert_eq!(count(&conn, "PRAGMA foreign_keys"), 1);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM pragma_foreign_key_check"),
        0
    );
    let names: Vec<String> = conn
        .prepare("SELECT name FROM export_profiles ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(names, ["foobar", "android", "internal"]);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM derived_variants"),
        0,
        "設定は起動時に写す"
    );
}

/// playlists.name_key（canonical key）は UNIQUE。FS 上で同じファイルになる名前は 2 本目が弾かれる（D-53）
#[test]
fn playlist_name_key_is_unique() {
    use spindle::domain::relpath::canonical_key;

    let conn = open_memory_connection().unwrap();
    conn.execute(
        "INSERT INTO playlists (name, name_key, created_at, updated_at) VALUES ('\u{304c}', ?1, 1, 1)",
        [canonical_key("\u{304c}")],
    )
    .unwrap();
    let dup = conn.execute(
        "INSERT INTO playlists (name, name_key, created_at, updated_at) VALUES ('か\u{3099}', ?1, 1, 1)",
        [canonical_key("\u{304b}\u{3099}")],
    );
    assert!(dup.is_err(), "NFC と NFD の同名は同じ key");
}

/// edit_ops.kind は md5 を受け、pending は 1 トラック 1 つ。edits は CASCADE、archived_files は SET NULL
#[test]
fn edit_ops_accepts_md5_and_keeps_pending_unique() {
    let conn = open_memory_connection().unwrap();
    conn.execute_batch(
        "INSERT INTO edit_batches (id, created_at, state, affected) VALUES (1, 1, 'applied', 1);
         INSERT INTO edit_ops (id, batch_id, ordinal, track_id, kind, result) VALUES (10, 1, 0, 5, 'tags', 'applied');
         INSERT INTO edits (op_id, key, old_value, new_value) VALUES (10, 'TITLE', '[\"x\"]', '[\"y\"]');
         INSERT INTO archived_files (id, track_id, op_id, rel_path, rel_path_key, source_rel_path, reason,
                                     archived_at, eligible_after, state)
           VALUES (1, 5, 10, 'a/b.wav', 'a/b.wav', 'a/b.wav', 'restore', 1, 2, 'held');",
    )
    .unwrap();
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
    assert!(
        conn.execute(
            "INSERT INTO edit_ops (batch_id, ordinal, track_id, kind) VALUES (1, 3, 7, 'bogus')",
            []
        )
        .is_err(),
        "kind の CHECK"
    );
    assert!(
        conn.execute(
            "INSERT INTO edit_ops (batch_id, ordinal, track_id, kind) VALUES (99, 0, 7, 'tags')",
            []
        )
        .is_err(),
        "無い batch"
    );
    conn.execute("DELETE FROM edit_ops WHERE id = 10", [])
        .unwrap();
    assert_eq!(
        count(&conn, "SELECT count(*) FROM edits WHERE op_id = 10"),
        0
    );
    assert_eq!(
        count(
            &conn,
            "SELECT op_id IS NULL FROM archived_files WHERE id = 1"
        ),
        1
    );
}

/// tracks.artwork_id は artwork 行が消えれば NULL（行は残る）。artwork_dirty / added_at の既定は 0、
/// flac_check / hires_* は NULL で始まり CHECK が効く
#[test]
fn tracks_later_columns_have_defaults_and_checks() {
    let conn = open_memory_connection().unwrap();
    conn.execute_batch(
        "INSERT INTO artwork (id, sha256, mime, bytes, origin) VALUES (7, zeroblob(32), 'image/jpeg', 1, 'embedded');
         INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                             audio_version, tag_version, seen_at, artwork_id)
           VALUES (1, 'a.flac', 'a.flac', 1, 0, 0, 'flac', 1, 1, 1, 0, 7);",
    )
    .unwrap();
    conn.execute("DELETE FROM artwork WHERE id = 7", [])
        .unwrap();
    let row: (Option<i64>, i64, i64) = conn
        .query_row(
            "SELECT artwork_id, artwork_dirty, added_at FROM tracks WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(row, (None, 0, 0));
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM tracks WHERE flac_check IS NULL AND hires_check IS NULL
               AND hires_cutoff_hz IS NULL AND hires_cliff_db IS NULL AND hires_effective_bits IS NULL"
        ),
        1
    );
    conn.execute(
        "UPDATE tracks SET flac_check = 'md5_missing', hires_check = 'upsampled', hires_checked_at = 1,
                           hires_check_version = 1, hires_cutoff_hz = 22050, hires_cliff_db = 48.5,
                           hires_effective_bits = 16 WHERE id = 1",
        [],
    )
    .unwrap();
    assert!(conn
        .execute("UPDATE tracks SET flac_check = 'bogus' WHERE id = 1", [])
        .is_err());
    assert!(conn
        .execute("UPDATE tracks SET hires_check = 'bogus' WHERE id = 1", [])
        .is_err());
}

/// jobs.type は ytdl / hirescheck / playlist_sync を受け、dedup の部分一意索引が効く。jobs を参照する
/// 子表は CASCADE（ロック類）/ SET NULL（edit_ops・album_verifications）
#[test]
fn jobs_accept_all_types_and_children_follow_deletes() {
    let conn = open_memory_connection().unwrap();
    for t in ["ytdl", "hirescheck", "playlist_sync"] {
        conn.execute(
            "INSERT INTO jobs (type, dedup_key, payload, created_at) VALUES (?1, ?1 || ':1', '{}', 1)",
            [t],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO jobs (type, dedup_key, payload, created_at) VALUES (?1, ?1 || ':1', '{}', 1)",
                [t],
            )
            .is_err(),
            "{t}: 未完了の間 dedup_key は一意"
        );
    }
    assert!(conn
        .execute(
            "INSERT INTO jobs (type, payload, created_at) VALUES ('bogus', '{}', 1)",
            []
        )
        .is_err());
    insert_track(&conn, 5);
    conn.execute_batch(
        "INSERT INTO jobs (id, type, payload, state, note, created_at) VALUES (101, 'scan', '{}', 'done', 'x', 1),
                                                                         (102, 'rg', '{}', 'queued', NULL, 2);
         INSERT INTO albums (id, rel_dir, rel_dir_key) VALUES (3, 'a', 'a');
         INSERT INTO edit_batches (id, created_at, state, affected) VALUES (1, 1, 'applied', 1);
         INSERT INTO edit_ops (id, batch_id, ordinal, track_id, kind, result, job_id)
           VALUES (10, 1, 0, 5, 'tags', 'applied', 101);
         INSERT INTO derived_path_locks (rel_path_key, track_id, job_id, acquired_at) VALUES ('x', 5, 102, 1);
         INSERT INTO derived_path_locks (rel_path_key, track_id, job_id, acquired_at) VALUES ('y', NULL, 102, 1);
         INSERT INTO job_mutexes (name, job_id, acquired_at) VALUES ('library', 102, 1);
         INSERT INTO track_locks (track_id, job_id, acquired_at) VALUES (5, 102, 1);
         INSERT INTO album_verifications (album_id, method, result, source, verified_at, disc_no, job_id)
           VALUES (3, 'ctdb', 'verified', 'retro', 1, 1, 101);",
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO album_verifications (album_id, method, result, source, verified_at, disc_no, job_id)
               VALUES (3, 'ctdb', 'verified', 'retro', 2, 1, 101)",
            []
        )
        .is_err(),
        "同じ (job_id, disc_no, method) は 1 行（idx_alb_verif_job）"
    );
    assert!(conn
        .execute(
            "INSERT INTO track_locks (track_id, job_id, acquired_at) VALUES (5, 99, 1)",
            []
        )
        .is_err());
    conn.execute("DELETE FROM jobs WHERE id = 102", []).unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM track_locks"), 0);
    assert_eq!(count(&conn, "SELECT count(*) FROM derived_path_locks"), 0);
    assert_eq!(count(&conn, "SELECT count(*) FROM job_mutexes"), 0);
    conn.execute("DELETE FROM jobs WHERE id = 101", []).unwrap();
    assert_eq!(
        count(&conn, "SELECT job_id IS NULL FROM edit_ops WHERE id = 10"),
        1
    );
    assert_eq!(
        count(&conn, "SELECT job_id IS NULL FROM album_verifications"),
        1
    );
}

/// albums.album_gain は既定 0 で 0 / 1 のみ。discid 列は無い（D-67 追記 3）
#[test]
fn albums_album_gain_defaults_off_and_has_no_discid() {
    let conn = open_memory_connection().unwrap();
    conn.execute(
        "INSERT INTO albums (id, rel_dir, rel_dir_key) VALUES (1, 'A/B', 'a/b')",
        [],
    )
    .unwrap();
    assert_eq!(
        count(&conn, "SELECT album_gain FROM albums WHERE id = 1"),
        0
    );
    assert!(conn
        .execute("UPDATE albums SET album_gain = 2 WHERE id = 1", [])
        .is_err());
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM pragma_table_info('albums') WHERE name = 'discid'"
        ),
        0
    );
}

/// derived_files は (track_id, variant) 主キーで、系統ごとに 1 行。delivery は opus 系統を指し、
/// タグ版が違えば stale_tags。トラックを消すと系統ごとの行も消える
#[test]
fn derived_files_are_per_variant_and_delivery_uses_opus() {
    let conn = open_memory_connection().unwrap();
    insert_track(&conn, 5);
    insert_track(&conn, 6);
    conn.execute("UPDATE tracks SET tag_version = 2 WHERE id = 5", [])
        .unwrap();
    conn.execute_batch(
        "INSERT INTO derived_files (track_id, variant, rel_path, rel_path_key, codec, bitrate,
                                    src_audio_version, src_tag_version, generated_at, audio_profile, tag_profile)
           VALUES (5, 'opus', 'opus/a/5.opus', 'opus/a/5.opus', 'opus', 256, 1, 1, 10, 'opus:256:v1', 'opus:v1'),
                  (5, 'aac', 'aac/a/5.m4a', 'aac/a/5.m4a', 'aac', 256, 1, 2, 11, 'aac:256:v1', 'aac:v1');",
    )
    .unwrap();
    let d: (String, String, i64) = conn
        .query_row(
            "SELECT path, codec, stale_tags FROM delivery WHERE track_id = 5",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(d, ("Derived/opus/a/5.opus".into(), "opus".into(), 1));
    let d6: (String, String) = conn
        .query_row(
            "SELECT path, codec FROM delivery WHERE track_id = 6",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(d6, ("Library/a/6.flac".into(), "flac".into()));
    assert!(
        conn.execute(
            "INSERT INTO derived_files (track_id, variant, rel_path, rel_path_key, codec, src_audio_version,
                                        src_tag_version, generated_at, audio_profile, tag_profile)
               VALUES (5, 'opus', 'z', 'z', 'opus', 1, 1, 1, 'a', 'b')",
            []
        )
        .is_err(),
        "同じ系統は 1 行"
    );
    assert!(
        conn.execute(
            "INSERT INTO derived_files (track_id, variant, rel_path, rel_path_key, codec, src_audio_version,
                                        src_tag_version, generated_at, audio_profile, tag_profile)
               VALUES (6, 'ogg', 'x', 'x', 'ogg', 1, 1, 1, 'a', 'b')",
            []
        )
        .is_err(),
        "variant の CHECK"
    );
    conn.execute("DELETE FROM tracks WHERE id = 5", []).unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM derived_files"), 0);
}

/// derived_variants の lossy_sources / multi_value_separator は既定値で埋まり、CHECK が効く
#[test]
fn derived_variants_options_have_defaults() {
    let conn = open_memory_connection().unwrap();
    conn.execute(
        "INSERT INTO derived_variants (variant, enabled, audio_profile, tag_profile, codec, bitrate, updated_at)
           VALUES ('opus', 1, 'opus:256:v1', 'opus:v1', 'opus', 256, 1)",
        [],
    )
    .unwrap();
    let row: (i64, String) = conn
        .query_row(
            "SELECT lossy_sources, multi_value_separator FROM derived_variants WHERE variant = 'opus'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(row, (0, " & ".into()));
    assert!(conn
        .execute(
            "UPDATE derived_variants SET lossy_sources = 2 WHERE variant = 'opus'",
            []
        )
        .is_err());
}

/// 購読: list_id / target_key は UNIQUE、album_id は非 NULL の間 UNIQUE で album が消えれば NULL、
/// 既定値。id は DELETE 後も再利用しない（AUTOINCREMENT。ジョブの payload とサイドカーが裸の id を持つ）
#[test]
fn playlist_subscriptions_constraints_and_ids_not_reused() {
    let conn = open_memory_connection().unwrap();
    let insert = |list_id: &str, target: &str| {
        conn.execute(
            "INSERT INTO playlist_subscriptions (list_id, url, target_key, albumartist, album, created_at, updated_at)
               VALUES (?1, 'u', ?2, 'A', 'B', 1, 1)",
            [list_id, target],
        )
    };
    insert("PL1", "a/b").unwrap();
    let defaults: (i64, i64, i64, Option<i64>) = conn
        .query_row(
            "SELECT align, enabled, max_enqueue, album_id FROM playlist_subscriptions WHERE list_id = 'PL1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(defaults, (1, 1, 50, None));
    assert!(insert("PL1", "c/d").is_err(), "list_id は UNIQUE");
    assert!(insert("PL2", "a/b").is_err(), "target_key は UNIQUE");
    insert("PL2", "c/d").unwrap();
    conn.execute_batch(
        "INSERT INTO albums (id, rel_dir, rel_dir_key) VALUES (5, 'A/B', 'a/b');
         UPDATE playlist_subscriptions SET album_id = 5 WHERE list_id = 'PL1';",
    )
    .unwrap();
    assert!(
        conn.execute(
            "UPDATE playlist_subscriptions SET album_id = 5 WHERE list_id = 'PL2'",
            []
        )
        .is_err(),
        "album_id は非 NULL の間 UNIQUE"
    );
    conn.execute("DELETE FROM albums WHERE id = 5", []).unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT album_id IS NULL FROM playlist_subscriptions WHERE list_id = 'PL1'"
        ),
        1
    );
    let last = count(&conn, "SELECT max(id) FROM playlist_subscriptions");
    conn.execute("DELETE FROM playlist_subscriptions WHERE id = ?1", [last])
        .unwrap();
    insert("PL3", "e/f").unwrap();
    assert_eq!(
        conn.last_insert_rowid(),
        last + 1,
        "消した id を再利用しない"
    );
}

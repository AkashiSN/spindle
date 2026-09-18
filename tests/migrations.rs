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

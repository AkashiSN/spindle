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

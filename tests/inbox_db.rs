//! `inbox_items` / `inbox_files` の DB 層（`db::inbox`。D-68、P2-10）

use spindle::db::inbox::{self, FileRow, ItemState};
use spindle::db::open_memory_connection;

fn file(rel: &str, title: &str) -> FileRow {
    FileRow {
        rel_path: rel.to_owned(),
        inode: 1,
        size: 10,
        mtime_ns: 0,
        ctime_ns: 0,
        codec: "flac".into(),
        lossless: true,
        sample_rate: Some(44100),
        bit_depth: Some(16),
        channels: Some(2),
        duration_ms: Some(1000),
        tags: vec![("TITLE".into(), title.into())],
    }
}

#[test]
fn items_and_files_round_trip() {
    let c = open_memory_connection().unwrap();
    let a = inbox::insert_item(&c, "AlbumA", "albuma", 100).unwrap();
    let b = inbox::insert_item(&c, "", "", 200).unwrap();
    let items = inbox::list(&c).unwrap();
    assert_eq!(items.iter().map(|i| i.id).collect::<Vec<_>>(), [b, a]);
    let got = inbox::get(&c, a).unwrap().unwrap();
    assert_eq!(got.rel_dir, "AlbumA");
    assert_eq!(got.state, ItemState::Pending);
    assert_eq!((got.detected_at, got.seen_at), (100, 100));
    assert_eq!(inbox::find_by_dir_key(&c, "albuma").unwrap().unwrap().id, a);
    assert!(inbox::find_by_dir_key(&c, "nope").unwrap().is_none());

    inbox::replace_files(
        &c,
        a,
        &[file("AlbumA/02.flac", "two"), file("AlbumA/01.flac", "one")],
    )
    .unwrap();
    let files = inbox::files(&c, a).unwrap();
    assert_eq!(
        files
            .iter()
            .map(|f| f.rel_path.as_str())
            .collect::<Vec<_>>(),
        ["AlbumA/01.flac", "AlbumA/02.flac"]
    );
    assert_eq!(files[0].tags, vec![("TITLE".to_owned(), "one".to_owned())]);
    assert_eq!(files[0].duration_ms, Some(1000));
    // 入れ替え
    inbox::replace_files(&c, a, &[file("AlbumA/03.flac", "three")]).unwrap();
    assert_eq!(inbox::files(&c, a).unwrap().len(), 1);
    // rel_path_key は casefold + NFD
    let key: String = c
        .query_row(
            "SELECT rel_path_key FROM inbox_files WHERE item_id = ?1",
            [a],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(key, "albuma/03.flac");
}

#[test]
fn state_transitions_and_draft() {
    let c = open_memory_connection().unwrap();
    let a = inbox::insert_item(&c, "A", "a", 100).unwrap();
    inbox::set_draft(&c, a, &serde_json::json!({ "album": "X" })).unwrap();
    inbox::set_state(&c, a, ItemState::Approved, None, 150).unwrap();
    let i = inbox::get(&c, a).unwrap().unwrap();
    assert_eq!(i.state, ItemState::Approved);
    assert_eq!(i.approved_at, Some(150));
    assert_eq!(i.draft, Some(serde_json::json!({ "album": "X" })));
    inbox::set_state(&c, a, ItemState::Pending, Some("ファイルが変わった"), 160).unwrap();
    let i = inbox::get(&c, a).unwrap().unwrap();
    assert_eq!(i.state, ItemState::Pending);
    assert_eq!(i.error.as_deref(), Some("ファイルが変わった"));
    assert_eq!(i.approved_at, Some(150)); // approved 以外への遷移では触らない
    assert_eq!(
        inbox::list_by_state(&c, ItemState::Pending).unwrap().len(),
        1
    );
    assert_eq!(
        inbox::list_by_state(&c, ItemState::Approved).unwrap().len(),
        0
    );

    c.execute(
        "INSERT INTO albums (rel_dir, rel_dir_key, album) VALUES ('L/A', 'l/a', 'A')",
        [],
    )
    .unwrap();
    let album = c.last_insert_rowid();
    inbox::set_placed(&c, a, album, 300).unwrap();
    let i = inbox::get(&c, a).unwrap().unwrap();
    assert_eq!(i.state, ItemState::Placed);
    assert_eq!(i.placed_album_id, Some(album));
    assert_eq!(i.placed_at, Some(300));
    assert_eq!(i.error, None);
}

#[test]
fn stale_and_expire_and_cascade() {
    let c = open_memory_connection().unwrap();
    let a = inbox::insert_item(&c, "A", "a", 100).unwrap();
    let b = inbox::insert_item(&c, "B", "b", 100).unwrap();
    let p = inbox::insert_item(&c, "P", "p", 100).unwrap();
    inbox::replace_files(&c, a, &[file("A/01.flac", "x")]).unwrap();
    c.execute(
        "INSERT INTO albums (rel_dir, rel_dir_key, album) VALUES ('L/P', 'l/p', 'P')",
        [],
    )
    .unwrap();
    inbox::set_placed(&c, p, c.last_insert_rowid(), 100).unwrap();
    inbox::touch(&c, b, 200).unwrap();
    // 200 の走査で見なかったのは a と p。placed は stale に含めない
    let stale = inbox::stale_items(&c, 200).unwrap();
    assert_eq!(stale.iter().map(|i| i.id).collect::<Vec<_>>(), [a]);
    // placed の期限切れ
    assert_eq!(inbox::expire_placed(&c, 100).unwrap(), 0);
    assert_eq!(inbox::expire_placed(&c, 101).unwrap(), 1);
    assert!(inbox::get(&c, p).unwrap().is_none());
    // 件を消すと files も消える
    inbox::delete_item(&c, a).unwrap();
    let n: i64 = c
        .query_row("SELECT count(*) FROM inbox_files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}

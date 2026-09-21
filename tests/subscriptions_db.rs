//! `playlist_subscriptions` の DB 層（`db::subscriptions`。P4-16、D-78）

use spindle::db::open_memory_connection;
use spindle::db::subscriptions::{self, BindOutcome, NewSubscription, Patch, WriteOutcome};

fn new_sub(list_id: &str, artist: &str, album: &str) -> NewSubscription {
    NewSubscription {
        list_id: list_id.to_owned(),
        url: format!("https://www.youtube.com/playlist?list={list_id}"),
        albumartist: artist.to_owned(),
        album: album.to_owned(),
        category: Some("神椿".to_owned()),
        align: true,
        enabled: true,
        max_enqueue: 50,
    }
}

#[test]
fn target_key_folds_case_and_normalization_like_paths() {
    assert_eq!(
        subscriptions::target_key("Kafu", "Kafu no Outa"),
        subscriptions::target_key("KAFU", "kafu no outa")
    );
    assert_ne!(
        subscriptions::target_key("A", "B/C"),
        subscriptions::target_key("A/B", "C")
    );
}

#[test]
fn insert_list_get_delete_round_trip_with_defaults() {
    let c = open_memory_connection().unwrap();
    let id = match subscriptions::insert(&c, &new_sub("PL1", "花譜", "花譜のお歌"), 100).unwrap()
    {
        WriteOutcome::Ok(id) => id,
        other => panic!("{other:?}"),
    };
    let s = subscriptions::get(&c, id).unwrap().unwrap();
    assert_eq!(s.list_id, "PL1");
    assert_eq!(s.album_id, None);
    assert_eq!(s.category.as_deref(), Some("神椿"));
    assert!(s.align && s.enabled);
    assert_eq!(s.max_enqueue, 50);
    assert_eq!((s.created_at, s.updated_at), (100, 100));
    assert_eq!(s.last_attempted_at, None);
    assert_eq!(s.sync_requested_at, None);
    assert_eq!(s.last_result, None);
    // 一覧は albumartist / album 順
    subscriptions::insert(&c, &new_sub("PL2", "VALIS", "VALISのお歌"), 101).unwrap();
    let names: Vec<String> = subscriptions::list(&c)
        .unwrap()
        .into_iter()
        .map(|s| s.albumartist)
        .collect();
    assert_eq!(names, ["VALIS", "花譜"]);
    assert!(subscriptions::delete(&c, id).unwrap());
    assert!(!subscriptions::delete(&c, id).unwrap());
    assert!(subscriptions::get(&c, id).unwrap().is_none());
}

#[test]
fn insert_rejects_duplicate_list_and_duplicate_target() {
    let c = open_memory_connection().unwrap();
    subscriptions::insert(&c, &new_sub("PL1", "花譜", "花譜のお歌"), 1).unwrap();
    assert_eq!(
        subscriptions::insert(&c, &new_sub("PL1", "X", "Y"), 2).unwrap(),
        WriteOutcome::DuplicateList
    );
    // 追記先は表記が違っても同じなら 1 つだけ
    assert_eq!(
        subscriptions::insert(&c, &new_sub("PL2", "花譜", "花譜のお歌"), 2).unwrap(),
        WriteOutcome::DuplicateTarget
    );
    assert_eq!(subscriptions::list(&c).unwrap().len(), 1);
}

#[test]
fn update_changes_fields_and_resets_album_binding_when_target_changes() {
    let c = open_memory_connection().unwrap();
    c.execute_batch(
        "INSERT INTO albums (id, rel_dir, rel_dir_key, albumartist, album) VALUES (5, 'A/B', 'a/b', 'A', 'B');",
    )
    .unwrap();
    let WriteOutcome::Ok(id) = subscriptions::insert(&c, &new_sub("PL1", "A", "B"), 1).unwrap()
    else {
        panic!()
    };
    assert_eq!(
        subscriptions::bind_album(&c, id, 5).unwrap(),
        BindOutcome::Bound
    );
    // 追記先に関係ない変更は album_id を保つ
    let patch = Patch {
        align: Some(false),
        enabled: Some(false),
        max_enqueue: Some(10),
        ..Patch::default()
    };
    assert_eq!(
        subscriptions::update(&c, id, &patch, 2).unwrap(),
        WriteOutcome::Ok(id)
    );
    let s = subscriptions::get(&c, id).unwrap().unwrap();
    assert!(!s.align && !s.enabled);
    assert_eq!((s.max_enqueue, s.album_id, s.updated_at), (10, Some(5), 2));
    // albumartist / album / category を変えると NULL に戻る（再解決）
    let patch = Patch {
        album: Some("C".to_owned()),
        ..Patch::default()
    };
    assert_eq!(
        subscriptions::update(&c, id, &patch, 3).unwrap(),
        WriteOutcome::Ok(id)
    );
    let s = subscriptions::get(&c, id).unwrap().unwrap();
    assert_eq!((s.album.as_str(), s.album_id), ("C", None));
    let patch = Patch {
        category: Some(None),
        ..Patch::default()
    };
    subscriptions::bind_album(&c, id, 5).unwrap();
    subscriptions::update(&c, id, &patch, 4).unwrap();
    let s = subscriptions::get(&c, id).unwrap().unwrap();
    assert_eq!((s.category, s.album_id), (None, None));
    // 別の購読と同じ追記先には変えられない
    subscriptions::insert(&c, &new_sub("PL2", "X", "Y"), 5).unwrap();
    let patch = Patch {
        albumartist: Some("x".to_owned()),
        album: Some("y".to_owned()),
        ..Patch::default()
    };
    assert_eq!(
        subscriptions::update(&c, id, &patch, 6).unwrap(),
        WriteOutcome::DuplicateTarget
    );
    assert_eq!(
        subscriptions::update(&c, 999, &Patch::default(), 6).unwrap(),
        WriteOutcome::NotFound
    );
}

#[test]
fn bind_album_is_compare_and_set_and_unique_across_subscriptions() {
    let c = open_memory_connection().unwrap();
    c.execute_batch(
        "INSERT INTO albums (id, rel_dir, rel_dir_key, albumartist, album) VALUES (5, 'A/B', 'a/b', 'A', 'B');
         INSERT INTO albums (id, rel_dir, rel_dir_key, albumartist, album) VALUES (6, 'C/D', 'c/d', 'C', 'D');",
    )
    .unwrap();
    let WriteOutcome::Ok(a) = subscriptions::insert(&c, &new_sub("PL1", "A", "B"), 1).unwrap()
    else {
        panic!()
    };
    let WriteOutcome::Ok(b) = subscriptions::insert(&c, &new_sub("PL2", "C", "D"), 1).unwrap()
    else {
        panic!()
    };
    assert_eq!(
        subscriptions::bind_album(&c, a, 5).unwrap(),
        BindOutcome::Bound
    );
    // 束ねてあれば触らない（別の album でも）
    assert_eq!(
        subscriptions::bind_album(&c, a, 6).unwrap(),
        BindOutcome::AlreadyBound(5)
    );
    // 他の購読が束ねている album には束ねられない
    assert_eq!(
        subscriptions::bind_album(&c, b, 5).unwrap(),
        BindOutcome::TakenBy(a)
    );
    assert_eq!(
        subscriptions::bind_album(&c, 999, 6).unwrap(),
        BindOutcome::NotFound
    );
    assert_eq!(subscriptions::get(&c, b).unwrap().unwrap().album_id, None);
}

#[test]
fn sync_latch_attempt_and_result_round_trip() {
    let c = open_memory_connection().unwrap();
    let WriteOutcome::Ok(id) = subscriptions::insert(&c, &new_sub("PL1", "A", "B"), 1).unwrap()
    else {
        panic!()
    };
    assert!(subscriptions::request_sync(&c, id, 10).unwrap());
    assert!(!subscriptions::request_sync(&c, 999, 10).unwrap());
    assert_eq!(subscriptions::requested(&c).unwrap(), [id]);
    // 開始で latch を消し last_attempted_at を書く。行を返す
    let s = subscriptions::begin_attempt(&c, id, 20).unwrap().unwrap();
    assert_eq!(s.last_attempted_at, Some(20));
    assert_eq!(s.sync_requested_at, None);
    assert!(subscriptions::requested(&c).unwrap().is_empty());
    assert!(!subscriptions::is_requested(&c, id).unwrap());
    // 走行中の要求は残る
    subscriptions::request_sync(&c, id, 21).unwrap();
    assert!(subscriptions::is_requested(&c, id).unwrap());
    // 成功の終端
    let result = serde_json::json!({ "entries": 3 });
    subscriptions::finish_attempt(&c, id, 30, &result).unwrap();
    let s = subscriptions::get(&c, id).unwrap().unwrap();
    assert_eq!(s.last_synced_at, Some(30));
    assert_eq!(s.last_result, Some(result));
    assert_eq!(s.sync_requested_at, Some(21), "終端は latch を触らない");
    // 失敗の記録は last_synced_at を進めない
    let failed = serde_json::json!({ "error": "x" });
    subscriptions::set_result(&c, id, &failed).unwrap();
    let s = subscriptions::get(&c, id).unwrap().unwrap();
    assert_eq!((s.last_synced_at, s.last_result), (Some(30), Some(failed)));
    assert!(subscriptions::begin_attempt(&c, 999, 40).unwrap().is_none());
}

#[test]
fn due_lists_enabled_subscriptions_by_last_attempt() {
    let c = open_memory_connection().unwrap();
    let WriteOutcome::Ok(a) = subscriptions::insert(&c, &new_sub("PL1", "A", "B"), 1).unwrap()
    else {
        panic!()
    };
    let WriteOutcome::Ok(b) = subscriptions::insert(&c, &new_sub("PL2", "C", "D"), 1).unwrap()
    else {
        panic!()
    };
    let WriteOutcome::Ok(off) = subscriptions::insert(&c, &new_sub("PL3", "E", "F"), 1).unwrap()
    else {
        panic!()
    };
    subscriptions::update(
        &c,
        off,
        &Patch {
            enabled: Some(false),
            ..Patch::default()
        },
        1,
    )
    .unwrap();
    // 一度も試していなければ due。試してから interval 未満なら due でない
    subscriptions::begin_attempt(&c, a, 1000).unwrap();
    assert_eq!(subscriptions::due(&c, 1500, 3600).unwrap(), [b]);
    assert_eq!(subscriptions::due(&c, 4600, 3600).unwrap(), [a, b]);
    // 時計が戻っていれば due にしない
    assert_eq!(subscriptions::due(&c, 900, 3600).unwrap(), [b]);
}

#![cfg(feature = "fake")]
mod support;

use spindle_agent::exec;
use spindle_agent::plan::{current_of, runnable};
use spindle_agent::recover::recover;
use spindle_agent::server::Server;
use spindle_agent::Error;
use support::Env;

fn exec_all(env: &mut Env) -> spindle_agent::Result<exec::Outcome> {
    let (_p, r, m) = env.confirm_all();
    env.with_ctx(|cx| exec::run(cx, &r, &m))
}

/// 落ちた後: 回復 → 再発見 → 同じ計画の再開
fn resume(env: &mut Env) {
    let m = env.server.manifest().unwrap();
    env.with_ctx(recover).unwrap();
    env.with_ctx(|cx| spindle_agent::rediscover::rediscover(cx, &m))
        .unwrap();
    if let Some(p) = env.server.open() {
        let (c, cp) = current_of(&env.state());
        let r = runnable(&p, &c, &cp, &m.diff);
        env.with_ctx(|cx| exec::run(cx, &r, &m)).unwrap();
    }
}

/// (パス, 中身, persistent ID) の一覧
fn snapshot(env: &Env) -> Vec<(String, Vec<u8>, String)> {
    let mut v: Vec<_> = env
        .music_paths()
        .into_iter()
        .map(|(p, pid)| {
            let bytes = std::fs::read(env.root.abs(&p).unwrap()).unwrap_or_default();
            (p, bytes, pid)
        })
        .collect();
    v.sort();
    v
}

#[test]
fn swap_moves_both_and_keeps_tracks() {
    let mut env = Env::new();
    env.paired();
    let a = env.synced_track(1, "x.m4a", b"one");
    let b = env.synced_track(2, "y.m4a", b"two");
    env.music.set_play_count(&a.persistent_id, 3);
    env.server.put_track(1, "y.m4a", b"one");
    env.server.put_track(2, "x.m4a", b"two");
    exec_all(&mut env).unwrap();
    assert_eq!(
        snapshot(&env),
        vec![
            ("x.m4a".into(), b"two".to_vec(), b.persistent_id.clone()),
            ("y.m4a".into(), b"one".to_vec(), a.persistent_id.clone()),
        ]
    );
    assert_eq!(
        env.music
            .tracks()
            .iter()
            .find(|t| t.persistent_id == a.persistent_id)
            .unwrap()
            .play_count,
        3
    );
    let s = env.state();
    assert!(s.pending_batches.is_empty());
    assert_eq!(s.tracks[&1].path, "y.m4a");
    assert!(env
        .root
        .list_files()
        .unwrap()
        .iter()
        .all(|f| !f.starts_with(".moving/")));
}

#[test]
fn update_move_places_new_content_and_removes_old_before_done() {
    let mut env = Env::new();
    env.paired();
    let a = env.synced_track(1, "A/x.m4a", b"old");
    env.server.put_track(1, "B/x.m4a", b"new!");
    env.fp.arm("batch.cleaned", 1);
    assert!(matches!(exec_all(&mut env), Err(Error::Crash(_))));
    // done の前に旧版は消えている
    let files = env.root.list_files().unwrap();
    assert!(
        files.iter().all(|f| !f.starts_with(".moving/")),
        "{files:?}"
    );
    assert_eq!(env.state().pending_batches.len(), 1);
    resume(&mut env);
    assert_eq!(
        snapshot(&env),
        vec![("B/x.m4a".into(), b"new!".to_vec(), a.persistent_id)]
    );
    assert!(env.music.calls().contains(&"refresh".to_owned()));
}

/// 入れ替え・循環・更新 + 移動の混在を、全中断点で落としても最後は同じになる
#[test]
fn swap_crash_at_every_point_completes() {
    let names = [
        "state.saved",
        "music.set_location",
        "music.set_location:after",
        "music.refresh:after",
        "batch.prepared",
        "batch.vacated_one",
        "batch.place.checked",
        "batch.placed_one",
        "batch.cleaned",
    ];
    let want = |a: &str, b: &str, c: &str| -> Vec<(String, Vec<u8>, String)> {
        vec![
            ("x.m4a".to_owned(), b"two".to_vec(), b.to_owned()),
            ("y.m4a".to_owned(), b"one".to_vec(), a.to_owned()),
            ("z/w.m4a".to_owned(), b"three-new".to_vec(), c.to_owned()),
        ]
    };
    for name in names {
        let mut crashed = false;
        for nth in 1..=12 {
            let mut env = Env::new();
            env.paired();
            let a = env.synced_track(1, "x.m4a", b"one");
            let b = env.synced_track(2, "y.m4a", b"two");
            let c = env.synced_track(3, "z.m4a", b"three");
            env.server.put_track(1, "y.m4a", b"one");
            env.server.put_track(2, "x.m4a", b"two");
            env.server.put_track(3, "z/w.m4a", b"three-new");
            env.fp.arm(name, nth);
            if !matches!(exec_all(&mut env), Err(Error::Crash(_))) {
                continue;
            }
            resume(&mut env);
            crashed = true;
            assert_eq!(
                snapshot(&env),
                want(&a.persistent_id, &b.persistent_id, &c.persistent_id),
                "{name}#{nth}"
            );
            let s = env.state();
            assert!(s.pending_batches.is_empty(), "{name}#{nth}");
            assert_eq!(env.music.tracks().len(), 3, "{name}#{nth}");
            assert!(
                env.root
                    .list_files()
                    .unwrap()
                    .iter()
                    .all(|f| !f.starts_with(".moving/")),
                "{name}#{nth}"
            );
        }
        assert!(crashed, "{name} で一度も落ちなかった");
    }
}

#[test]
fn crash_before_vacating_is_aborted_in_recovery() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "A/x.m4a", b"old");
    env.server.put_track(1, "B/x.m4a", b"new!");
    // Sealed の保存（1 回目）→ Prepared の保存（2 回目）の直後で落とす
    env.fp.arm("state.saved", 2);
    assert!(matches!(exec_all(&mut env), Err(Error::Crash(_))));
    env.with_ctx(recover).unwrap();
    let s = env.state();
    assert!(s.pending_batches.is_empty());
    assert_eq!(s.tracks[&1].path, "A/x.m4a");
    assert!(env
        .root
        .list_files()
        .unwrap()
        .iter()
        .all(|f| !f.starts_with(".moving/")));
    assert!(env.root.exists("A/x.m4a").unwrap());
}

#[test]
fn prepare_failure_shrinks_by_component() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "x.m4a", b"one");
    env.synced_track(2, "y.m4a", b"two");
    env.synced_track(3, "p.m4a", b"three");
    // 1 ↔ 2 の入れ替え（2 は更新 + 移動で中身が取れない）と、無関係な 3 の移動
    env.server.put_track(1, "y.m4a", b"one");
    env.server.put_track(2, "x.m4a", b"two-new");
    env.server.put_track(3, "q.m4a", b"three");
    env.server.fail_fetch_changed(2, 1);
    exec_all(&mut env).unwrap();
    let s = env.state();
    assert_eq!(s.tracks[&1].path, "x.m4a");
    assert_eq!(s.tracks[&2].path, "y.m4a");
    assert_eq!(s.tracks[&3].path, "q.m4a");
    assert!(s.pending_batches.is_empty());
    assert!(env
        .root
        .list_files()
        .unwrap()
        .iter()
        .all(|f| !f.starts_with(".moving/")));
}

#[test]
fn unmanaged_destination_drops_component() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "x.m4a", b"one");
    env.server.put_track(1, "y.m4a", b"one");
    env.write_local("y.m4a", b"mine");
    let (_p, r, m) = env.confirm_all();
    let errors = env.with_ctx(|cx| {
        exec::run(cx, &r, &m).unwrap();
        cx.errors.clone()
    });
    assert_eq!(errors[0].reason, exec::UNMANAGED_COLLISION);
    assert_eq!(
        std::fs::read(env.root.abs("y.m4a").unwrap()).unwrap(),
        b"mine"
    );
    assert_eq!(env.state().tracks[&1].path, "x.m4a");
}

#[test]
fn case_only_rename_works() {
    let mut env = Env::new();
    env.paired();
    let a = env.synced_track(1, "A/song.m4a", b"one");
    env.server.put_track(1, "A/Song.m4a", b"one");
    exec_all(&mut env).unwrap();
    assert_eq!(env.state().tracks[&1].path, "A/Song.m4a");
    assert_eq!(env.music.tracks()[0].persistent_id, a.persistent_id);
    assert!(env
        .root
        .list_files()
        .unwrap()
        .contains(&"A/Song.m4a".to_owned()));
}

#[test]
fn corrupted_digest_before_vacating_is_aborted() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "A/x.m4a", b"old");
    env.server.put_track(1, "B/x.m4a", b"old");
    env.fp.arm("state.saved", 1);
    assert!(matches!(exec_all(&mut env), Err(Error::Crash(_))));
    let mut s = env.state();
    s.pending_batches[0].digest = "bad".into();
    env.file().save(&s).unwrap();
    env.with_ctx(recover).unwrap();
    assert!(env.state().pending_batches.is_empty());
}

/// バッチの途中で track が手で消された: 回復は止まらずにファイルを揃え、次の再発見が state から外す
#[test]
fn track_deleted_mid_batch_does_not_wedge_recovery() {
    let mut env = Env::new();
    env.paired();
    let a = env.synced_track(1, "x.m4a", b"one");
    let b = env.synced_track(2, "y.m4a", b"two");
    env.server.put_track(1, "y.m4a", b"one");
    env.server.put_track(2, "x.m4a", b"two");
    env.fp.arm("batch.vacated_one", 1);
    assert!(matches!(exec_all(&mut env), Err(Error::Crash(_))));
    env.music.remove_track(&a.persistent_id);
    env.with_ctx(recover).unwrap();
    let s = env.state();
    assert!(s.pending_batches.is_empty());
    assert_eq!(s.tracks[&2].path, "x.m4a");
    assert_eq!(
        snapshot(&env),
        vec![("x.m4a".into(), b"two".to_vec(), b.persistent_id.clone())]
    );
    assert!(env
        .root
        .list_files()
        .unwrap()
        .iter()
        .all(|f| !f.starts_with(".moving/")));
    let m = env.server.manifest().unwrap();
    env.with_ctx(|cx| spindle_agent::rediscover::rediscover(cx, &m))
        .unwrap();
    let s = env.state();
    assert!(!s.tracks.contains_key(&1));
    assert_eq!(s.tracks[&2].path, "x.m4a");
}

/// 動かない管理下の曲が行き先にいる: 上書きせず、その成分ごと外す
#[test]
fn managed_track_at_destination_drops_component() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "x.m4a", b"one");
    env.synced_track(2, "y.m4a", b"two");
    env.synced_track(3, "p.m4a", b"three");
    // 1 は 2 の場所へ動くが、2 は動かない（計画の外。たとえば再開で外れた削除）
    let (_p, mut r, m) = {
        env.server.put_track(1, "y.m4a", b"one");
        env.server.put_track(3, "q.m4a", b"three");
        env.confirm_all()
    };
    r.items.retain(|i| i.track_id != 2);
    let errors = env.with_ctx(|cx| {
        exec::run(cx, &r, &m).unwrap();
        cx.errors.clone()
    });
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].ref_id, 1);
    assert_eq!(errors[0].reason, exec::PATH_OCCUPIED);
    assert_eq!(
        std::fs::read(env.root.abs("y.m4a").unwrap()).unwrap(),
        b"two"
    );
    assert_eq!(
        std::fs::read(env.root.abs("x.m4a").unwrap()).unwrap(),
        b"one"
    );
    let s = env.state();
    assert_eq!(s.tracks[&1].path, "x.m4a");
    assert_eq!(s.tracks[&2].path, "y.m4a");
    assert_eq!(s.tracks[&3].path, "q.m4a");
}

/// 準備の後・空ける前に行き先へ管理外のファイルが現れた: その成分ごと外して上書きしない
#[test]
fn file_appearing_at_destination_during_prepare_drops_component() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "x.m4a", b"one");
    env.synced_track(2, "p.m4a", b"two");
    env.server.put_track(1, "y.m4a", b"one-new");
    env.server.put_track(2, "q.m4a", b"two");
    let abs = env.root.abs("y.m4a").unwrap();
    env.fp.on("batch.prepared", 1, move || {
        std::fs::write(&abs, b"mine").unwrap();
    });
    let (_p, r, m) = env.confirm_all();
    let errors = env.with_ctx(|cx| {
        exec::run(cx, &r, &m).unwrap();
        cx.errors.clone()
    });
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].reason, exec::UNMANAGED_COLLISION);
    assert_eq!(
        std::fs::read(env.root.abs("y.m4a").unwrap()).unwrap(),
        b"mine"
    );
    let s = env.state();
    assert_eq!(s.tracks[&1].path, "x.m4a");
    assert_eq!(s.tracks[&2].path, "q.m4a");
    assert!(s.pending_batches.is_empty());
    assert!(env
        .root
        .list_files()
        .unwrap()
        .iter()
        .all(|f| !f.starts_with(".moving/")));
}

/// 空けた後・置く前に行き先へ管理外のファイルが現れた: 上書きせずに止め、バッチは残す。
/// 利用者が退ければ次の回復で完遂する
#[test]
fn file_appearing_at_destination_while_placing_is_not_overwritten() {
    let mut env = Env::new();
    env.paired();
    let a = env.synced_track(1, "x.m4a", b"one");
    env.server.put_track(1, "y.m4a", b"one");
    let abs = env.root.abs("y.m4a").unwrap();
    env.fp.on("batch.vacated_one", 1, move || {
        std::fs::write(&abs, b"mine").unwrap();
    });
    let res = exec_all(&mut env);
    assert!(matches!(res, Err(Error::Stop(_))), "{res:?}");
    assert_eq!(
        std::fs::read(env.root.abs("y.m4a").unwrap()).unwrap(),
        b"mine"
    );
    assert_eq!(env.state().pending_batches.len(), 1);
    // 回復も上書きしない
    assert!(matches!(env.with_ctx(recover), Err(Error::Stop(_))));
    assert_eq!(
        std::fs::read(env.root.abs("y.m4a").unwrap()).unwrap(),
        b"mine"
    );
    std::fs::remove_file(env.root.abs("y.m4a").unwrap()).unwrap();
    env.with_ctx(recover).unwrap();
    let s = env.state();
    assert!(s.pending_batches.is_empty());
    assert_eq!(s.tracks[&1].path, "y.m4a");
    assert_eq!(
        snapshot(&env),
        vec![("y.m4a".into(), b"one".to_vec(), a.persistent_id.clone())]
    );
}

/// どのバッチにも属さない `.moving/` のファイル（破棄の途中で落ちた new など）は回復が片付ける
#[test]
fn orphaned_moving_files_are_removed_in_recovery() {
    let mut env = Env::new();
    env.paired();
    env.write_local(".moving/deadbeef-op.new", b"junk");
    env.write_local(".moving/deadbeef-op", b"junk");
    env.with_ctx(recover).unwrap();
    assert!(env
        .root
        .list_files()
        .unwrap()
        .iter()
        .all(|f| !f.starts_with(".moving/")));
}

/// 移動元がシンボリックリンクに差し替わった: `.moving/` へ動かさずに止める
#[test]
fn vacate_does_not_move_final_symlink() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "x.m4a", b"one");
    let outside = env.base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let target = outside.join("t.m4a");
    std::fs::write(&target, b"outside").unwrap();
    let link = env.root.path().join("x.m4a");
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    env.server.put_track(1, "z.m4a", b"one");
    match exec_all(&mut env) {
        Err(Error::Stop(msg)) => assert!(msg.contains("x.m4a"), "{msg}"),
        other => panic!("{other:?}"),
    }
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read_link(&link).unwrap(), target);
    assert_eq!(std::fs::read(&target).unwrap(), b"outside");
    assert!(env.root.list_files().unwrap().is_empty());
    assert!(!env.root.path().join("z.m4a").exists());
}

/// 行き先の確認の後・rename の前に管理外のファイルが現れた: rename が上書きを断り、確認と同じく止める
#[test]
fn file_appearing_at_destination_after_check_is_not_overwritten() {
    let mut env = Env::new();
    env.paired();
    let a = env.synced_track(1, "x.m4a", b"one");
    env.server.put_track(1, "y.m4a", b"one");
    let abs = env.root.abs("y.m4a").unwrap();
    env.fp.on("batch.place.checked", 1, move || {
        std::fs::write(&abs, b"mine").unwrap();
    });
    let res = exec_all(&mut env);
    assert!(
        matches!(&res, Err(Error::Stop(msg)) if msg.contains(spindle_agent::batch::FOREIGN_AT_DESTINATION)),
        "{res:?}"
    );
    assert_eq!(
        std::fs::read(env.root.abs("y.m4a").unwrap()).unwrap(),
        b"mine"
    );
    assert_eq!(env.state().pending_batches.len(), 1);
    std::fs::remove_file(env.root.abs("y.m4a").unwrap()).unwrap();
    env.with_ctx(recover).unwrap();
    let s = env.state();
    assert!(s.pending_batches.is_empty());
    assert_eq!(s.tracks[&1].path, "y.m4a");
    assert_eq!(
        snapshot(&env),
        vec![("y.m4a".into(), b"one".to_vec(), a.persistent_id.clone())]
    );
}

/// 置き先へ rename した直後（location を付け替える前）に落ちた: 1 回目の set_location は空けるとき、
/// 2 回目は置いた後
fn crash_right_after_place_rename(env: &mut Env) -> String {
    env.paired();
    let a = env.synced_track(1, "x.m4a", b"one");
    env.server.put_track(1, "y.m4a", b"one");
    env.fp.arm("music.set_location", 2);
    assert!(matches!(exec_all(env), Err(Error::Crash(_))));
    assert_eq!(
        std::fs::read(env.root.abs("y.m4a").unwrap()).unwrap(),
        b"one"
    );
    a.persistent_id
}

fn location_of(env: &Env, pid: &str) -> String {
    env.music_paths()
        .into_iter()
        .find(|(_, p)| p == pid)
        .map(|(rel, _)| rel)
        .unwrap()
}

/// 置いた後で落ち、行き先が別の中身に差し替えられた: 回復は置き済みとみなさず止め、ファイルも
/// track の場所も変えない。バッチは残り、利用者が退けた後の回復で片付く
#[test]
fn recovery_does_not_adopt_replaced_destination() {
    let mut env = Env::new();
    let pid = crash_right_after_place_rename(&mut env);
    let staging = location_of(&env, &pid);
    assert!(staging.starts_with(".moving/"), "{staging}");
    std::fs::write(env.root.abs("y.m4a").unwrap(), b"mine").unwrap();
    let res = env.with_ctx(recover);
    assert!(
        matches!(&res, Err(Error::Stop(msg)) if msg.contains(spindle_agent::batch::FOREIGN_AT_DESTINATION)),
        "{res:?}"
    );
    assert_eq!(
        std::fs::read(env.root.abs("y.m4a").unwrap()).unwrap(),
        b"mine"
    );
    assert_eq!(location_of(&env, &pid), staging);
    assert!(!env.music.calls().contains(&"refresh".to_owned()));
    assert_eq!(env.state().pending_batches.len(), 1);
    // 同じ大きさの別の中身でも採用しない
    std::fs::write(env.root.abs("y.m4a").unwrap(), b"two").unwrap();
    assert!(matches!(env.with_ctx(recover), Err(Error::Stop(_))));
    assert_eq!(location_of(&env, &pid), staging);
    // 退ければ回復は片付く（中身は失われたので state から外れ、次の sync が取り直す）
    std::fs::remove_file(env.root.abs("y.m4a").unwrap()).unwrap();
    env.with_ctx(recover).unwrap();
    let s = env.state();
    assert!(s.pending_batches.is_empty());
    assert!(!s.tracks.contains_key(&1));
}

/// 置いた後で落ち、行き先がそのまま: 回復は中身を確かめて完遂する
#[test]
fn recovery_adopts_destination_with_expected_content() {
    let mut env = Env::new();
    let pid = crash_right_after_place_rename(&mut env);
    env.with_ctx(recover).unwrap();
    let s = env.state();
    assert!(s.pending_batches.is_empty());
    assert_eq!(s.tracks[&1].path, "y.m4a");
    assert_eq!(location_of(&env, &pid), "y.m4a");
}

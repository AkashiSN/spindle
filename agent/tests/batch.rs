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
        "batch.vacated_one",
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

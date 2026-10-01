#![cfg(feature = "fake")]
mod support;

use spindle_agent::exec::{self, PATH_OCCUPIED, UNMANAGED_COLLISION};
use spindle_agent::recover::recover;
use spindle_agent::server::Server;
use spindle_agent::Error;
use support::Env;

fn exec_all(env: &mut Env) -> spindle_agent::Result<exec::Outcome> {
    let (_p, r, m) = env.confirm_all();
    env.with_ctx(|cx| exec::run(cx, &r, &m))
}

#[test]
fn add_places_file_and_adds_track() {
    let mut env = Env::new();
    env.paired();
    let it = env.server.put_track(1, "A/a.m4a", b"aaa");
    let o = exec_all(&mut env).unwrap();
    assert_eq!(o.executed, 1);
    assert_eq!(
        std::fs::read(env.root.abs("A/a.m4a").unwrap()).unwrap(),
        b"aaa"
    );
    let s = env.state();
    assert_eq!(s.tracks[&1].token, it.token);
    assert!(s.pending_ops.is_empty());
    assert_eq!(env.music_paths().len(), 1);
}

#[test]
fn update_replaces_file_and_refreshes_same_track() {
    let mut env = Env::new();
    env.paired();
    let e = env.synced_track(1, "A/a.m4a", b"aaa");
    env.music.set_play_count(&e.persistent_id, 7);
    let it = env.server.put_track(1, "A/a.m4a", b"bbbb");
    exec_all(&mut env).unwrap();
    assert_eq!(
        std::fs::read(env.root.abs("A/a.m4a").unwrap()).unwrap(),
        b"bbbb"
    );
    let t = &env.music.tracks()[0];
    assert_eq!(t.persistent_id, e.persistent_id);
    assert_eq!(t.play_count, 7);
    assert!(env.music.calls().contains(&"refresh".to_owned()));
    assert_eq!(env.state().tracks[&1].token, it.token);
}

#[test]
fn delete_removes_track_then_file() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "A/a.m4a", b"aaa");
    env.server.remove_track(1);
    exec_all(&mut env).unwrap();
    assert!(env.music.tracks().is_empty());
    assert!(!env.root.exists("A/a.m4a").unwrap());
    assert!(env.state().tracks.is_empty());
}

#[test]
fn unmanaged_file_is_held() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.server.put_track(2, "A/b.m4a", b"bbb");
    env.write_local("A/a.m4a", b"mine");
    let (_p, r, m) = env.confirm_all();
    let errors = env.with_ctx(|cx| {
        exec::run(cx, &r, &m).unwrap();
        cx.errors.clone()
    });
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].ref_id, 1);
    assert_eq!(errors[0].reason, UNMANAGED_COLLISION);
    assert_eq!(
        std::fs::read(env.root.abs("A/a.m4a").unwrap()).unwrap(),
        b"mine"
    );
    let s = env.state();
    assert!(!s.tracks.contains_key(&1));
    assert!(s.tracks.contains_key(&2));
}

#[test]
fn unmanaged_track_at_destination_is_held() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.music.insert_track(&env.root.abs("A/a.m4a").unwrap());
    let (_p, r, m) = env.confirm_all();
    let errors = env.with_ctx(|cx| {
        exec::run(cx, &r, &m).unwrap();
        cx.errors.clone()
    });
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].reason, UNMANAGED_COLLISION);
}

#[test]
fn add_onto_a_path_held_by_another_track_is_occupied() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "A/a.m4a", b"aaa");
    // 1 は hold（移動しない）。2 が同じパスへ追加
    env.server.hold(1, "待ち");
    env.server.put_track(2, "A/a.m4a", b"zzz");
    let (_p, r, m) = env.confirm_all();
    let errors = env.with_ctx(|cx| {
        exec::run(cx, &r, &m).unwrap();
        cx.errors.clone()
    });
    assert_eq!(errors[0].reason, PATH_OCCUPIED);
    assert_eq!(
        std::fs::read(env.root.abs("A/a.m4a").unwrap()).unwrap(),
        b"aaa"
    );
}

#[test]
fn changed_source_is_dropped_not_errored() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.server.fail_fetch_changed(1, 1);
    let (_p, r, m) = env.confirm_all();
    let (o, errors) = env.with_ctx(|cx| (exec::run(cx, &r, &m).unwrap(), cx.errors.clone()));
    assert_eq!(o.dropped, 1);
    assert!(errors.is_empty());
    assert!(!env.root.exists("A/a.m4a.spindle-tmp").unwrap());
    assert!(env.state().pending_ops.is_empty());
}

#[test]
fn cut_download_resumes_with_range() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"0123456789");
    env.server.cut_fetch(1, 2);
    exec_all(&mut env).unwrap();
    assert_eq!(
        std::fs::read(env.root.abs("A/a.m4a").unwrap()).unwrap(),
        b"0123456789"
    );
    assert_eq!(env.server.fetch_log(), vec![(1, 0), (1, 5), (1, 7)]);
}

#[test]
fn copy_on_add_is_deleted_via_normal_path_and_stops() {
    let mut env = Env::new();
    env.paired();
    env.music.set_copy_on(true);
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.server.put_track(2, "A/b.m4a", b"bbb");
    let o = exec_all(&mut env).unwrap();
    assert!(o.stop.unwrap().contains("コピー"));
    assert!(env.music.tracks().is_empty());
    assert!(env.state().tracks.is_empty());
    assert!(env.state().pending_ops.is_empty());
    assert!(!env.root.exists("A/a.m4a").unwrap());
}

#[test]
fn not_enough_space_stops_before_writing() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", &[0u8; 16]);
    // サイズを空きより大きく見せる: 偽の項目の size を巨大にした計画で
    let (p, mut r, m) = env.confirm_all();
    let _ = p;
    r.items[0].size = u64::MAX / 2;
    let res = env.with_ctx(|cx| exec::run(cx, &r, &m));
    assert!(matches!(res, Err(Error::Stop(msg)) if msg.contains("空き容量")));
    assert!(env.state().pending_ops.is_empty());
}

/// 追加・更新・削除の全中断点（state の保存の直後・ミュージック.app の操作の前後・ファイルの配置の直後）で
/// 落としてから、回復 → 再発見 → 同じ計画の再開、で最終状態が同じになる
#[test]
fn basic_changes_survive_every_crash_point() {
    let names = [
        "state.saved",
        "music.add",
        "music.add:after",
        "music.refresh",
        "music.refresh:after",
        "music.delete_track",
        "music.delete_track:after",
        "exec.add.placed",
        "exec.update.placed",
        "exec.delete.track_deleted",
    ];
    for name in names {
        for nth in 1..=8 {
            let mut env = Env::new();
            env.paired();
            env.synced_track(1, "A/a.m4a", b"aaa");
            env.synced_track(2, "A/b.m4a", b"bbb");
            env.server.put_track(1, "A/a.m4a", b"a2");
            env.server.remove_track(2);
            env.server.put_track(3, "A/c.m4a", b"ccc");
            env.fp.arm(name, nth);
            let first = exec_all(&mut env);
            if !matches!(first, Err(Error::Crash(_))) {
                continue; // その名前の nth 回目は無かった
            }
            // やり直し: 回復 → 再発見 → 同じ計画で再開（偽サーバの計画は open のまま）
            let m = env.server.manifest().unwrap();
            env.with_ctx(recover).unwrap();
            env.with_ctx(|cx| spindle_agent::rediscover::rediscover(cx, &m))
                .unwrap();
            let p = env.server.open().unwrap();
            let (c, cp) = spindle_agent::plan::current_of(&env.state());
            let r = spindle_agent::plan::runnable(&p, &c, &cp, &m.diff);
            env.with_ctx(|cx| exec::run(cx, &r, &m)).unwrap();
            let s = env.state();
            assert!(s.pending_ops.is_empty(), "{name}#{nth}");
            assert_eq!(
                s.tracks.keys().copied().collect::<Vec<_>>(),
                vec![1, 3],
                "{name}#{nth}"
            );
            assert_eq!(
                std::fs::read(env.root.abs("A/a.m4a").unwrap()).unwrap(),
                b"a2",
                "{name}#{nth}"
            );
            assert!(!env.root.exists("A/b.m4a").unwrap(), "{name}#{nth}");
            let paths = env.music_paths();
            assert_eq!(
                paths.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
                vec!["A/a.m4a", "A/c.m4a"],
                "{name}#{nth}: 重複・取り残しがある"
            );
            let files = env.root.list_files().unwrap();
            assert!(
                files.iter().all(|f| !f.ends_with(".spindle-tmp")),
                "{name}#{nth}"
            );
        }
    }
}

#[test]
fn crash_after_add_with_copy_on_shows_candidates_and_deletes_nothing() {
    let mut env = Env::new();
    env.paired();
    env.music.set_copy_on(true);
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.fp.arm("music.add:after", 1);
    assert!(matches!(exec_all(&mut env), Err(Error::Crash(_))));
    let res = env.with_ctx(recover);
    let Err(Error::Stop(msg)) = res else {
        panic!("{res:?}")
    };
    assert!(msg.contains("resolve"));
    // 何も消していない
    assert_eq!(env.music.tracks().len(), 1);
    let s = env.state();
    assert_eq!(s.pending_ops.len(), 1);
    assert_eq!(s.pending_ops[0].candidates.as_ref().unwrap().len(), 1);
}

#[test]
fn crash_after_add_without_copy_adopts_in_recovery() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.fp.arm("music.add:after", 1);
    assert!(matches!(exec_all(&mut env), Err(Error::Crash(_))));
    env.with_ctx(recover).unwrap();
    let s = env.state();
    assert!(s.pending_ops.is_empty());
    assert!(s.tracks.contains_key(&1));
    assert!(s.needs_report);
    assert_eq!(env.music.tracks().len(), 1);
}

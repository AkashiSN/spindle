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
        "exec.add.downloaded",
        "exec.add.checked",
        "exec.add.placed",
        "exec.update.placed",
        "exec.delete.track_deleted",
    ];
    for name in names {
        let mut crashed = false;
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
            crashed = true;
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
        assert!(crashed, "{name} で一度も落ちなかった");
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

/// 更新を置いた直後に落ち、その後で track が手で消された: 回復は止まらず、再発見が state から外す
#[test]
fn update_recovery_survives_track_deleted_by_hand() {
    let mut env = Env::new();
    env.paired();
    let e = env.synced_track(1, "A/a.m4a", b"aaa");
    env.server.put_track(1, "A/a.m4a", b"bbbb");
    env.fp.arm("exec.update.placed", 1);
    assert!(matches!(exec_all(&mut env), Err(Error::Crash(_))));
    env.music.remove_track(&e.persistent_id);
    env.with_ctx(recover).unwrap();
    assert!(env.state().pending_ops.is_empty());
    let m = env.server.manifest().unwrap();
    env.with_ctx(|cx| spindle_agent::rediscover::rediscover(cx, &m))
        .unwrap();
    assert!(!env.state().tracks.contains_key(&1));
}

/// 更新の実行中（置いた直後）に track が手で消された: refresh せずに終える
#[test]
fn update_does_not_fail_when_track_vanishes_mid_run() {
    let mut env = Env::new();
    env.paired();
    let e = env.synced_track(1, "A/a.m4a", b"aaa");
    env.server.put_track(1, "A/a.m4a", b"bbbb");
    let music = env.music.clone();
    let pid = e.persistent_id.clone();
    env.fp
        .on("exec.update.placed", 1, move || music.remove_track(&pid));
    exec_all(&mut env).unwrap();
    assert!(env.state().pending_ops.is_empty());
}

/// 取得の後・配置の前に置き先へ管理外のファイルが現れた: 上書きせずに「管理外と衝突」にする
#[test]
fn add_does_not_overwrite_file_created_during_download() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    let abs = env.root.abs("A/a.m4a").unwrap();
    env.fp.on("exec.add.downloaded", 1, move || {
        std::fs::write(&abs, b"mine").unwrap();
    });
    let (_p, r, m) = env.confirm_all();
    let errors = env.with_ctx(|cx| {
        exec::run(cx, &r, &m).unwrap();
        cx.errors.clone()
    });
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].reason, UNMANAGED_COLLISION);
    assert_eq!(
        std::fs::read(env.root.abs("A/a.m4a").unwrap()).unwrap(),
        b"mine"
    );
    assert!(!env.root.exists("A/a.m4a.spindle-tmp").unwrap());
    let s = env.state();
    assert!(s.pending_ops.is_empty());
    assert!(!s.tracks.contains_key(&1));
    assert!(env.music.tracks().is_empty());
}

/// 取得中に落ちた後で置き先に管理外のファイルが置かれた: 回復はそれを消さない
#[test]
fn add_fetching_recovery_keeps_foreign_file_at_destination() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.fp.arm("exec.add.downloaded", 1);
    assert!(matches!(exec_all(&mut env), Err(Error::Crash(_))));
    env.write_local("A/a.m4a", b"mine");
    env.with_ctx(recover).unwrap();
    assert_eq!(
        std::fs::read(env.root.abs("A/a.m4a").unwrap()).unwrap(),
        b"mine"
    );
    assert!(env.state().pending_ops.is_empty());
}

/// add の直前に落ち、置いたファイルが外で差し替えられた: 回復は（中身が違うので）消さない
#[test]
fn add_adding_recovery_keeps_replaced_file_at_destination() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.fp.arm("music.add", 1);
    assert!(matches!(exec_all(&mut env), Err(Error::Crash(_))));
    env.write_local("A/a.m4a", b"mine");
    env.with_ctx(recover).unwrap();
    assert_eq!(
        std::fs::read(env.root.abs("A/a.m4a").unwrap()).unwrap(),
        b"mine"
    );
    assert!(env.state().pending_ops.is_empty());
}

/// 移動と削除で空いたディレクトリは片付け、root は残す
#[test]
fn empty_dirs_are_pruned_after_run() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "A/a.m4a", b"aaa");
    env.synced_track(2, "C/c.m4a", b"ccc");
    env.server.put_track(1, "B/a.m4a", b"aaa");
    env.server.remove_track(2);
    exec_all(&mut env).unwrap();
    assert!(!env.root.path().join("A").exists());
    assert!(!env.root.path().join("C").exists());
    assert!(!env.root.path().join(".moving").exists());
    assert!(env.root.exists("B/a.m4a").unwrap());
    assert!(env.root.exists(".spindle-device").unwrap());
}

/// 削除だけで root が空になっても root は残す
#[test]
fn prune_keeps_root() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "a.m4a", b"aaa");
    env.server.remove_track(1);
    std::fs::remove_file(env.root.abs(".spindle-device").unwrap()).unwrap();
    exec_all(&mut env).unwrap();
    assert!(env.root.path().is_dir());
}

/// 管理中の曲の親ディレクトリが root の外へのシンボリックリンクに差し替わった（更新）: 外のファイルに
/// 手を出さずにエラーで止める
#[test]
fn update_does_not_follow_symlinked_parent_dir() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "A/a.m4a", b"aaa");
    let outside = symlink_parent_outside(&env);
    env.server.put_track(1, "A/a.m4a", b"bbbb");
    assert!(matches!(exec_all(&mut env), Err(Error::Stop(_))));
    assert_outside_untouched(&outside);
}

/// 同じく（削除）: 外のファイルを消さずにエラーで止める
#[test]
fn delete_does_not_follow_symlinked_parent_dir() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "A/a.m4a", b"aaa");
    let outside = symlink_parent_outside(&env);
    env.server.remove_track(1);
    assert!(matches!(exec_all(&mut env), Err(Error::Stop(_))));
    assert_outside_untouched(&outside);
    // 回復も外のファイルを消さない
    assert!(env.with_ctx(recover).is_err());
    assert_outside_untouched(&outside);
}

/// root の `A/` を外の同じ中身のディレクトリへのシンボリックリンクにする
fn symlink_parent_outside(env: &Env) -> std::path::PathBuf {
    let outside = env.base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("a.m4a"), b"aaa").unwrap();
    let a = env.root.path().join("A");
    std::fs::remove_dir_all(&a).unwrap();
    std::os::unix::fs::symlink(&outside, &a).unwrap();
    outside
}

fn assert_outside_untouched(outside: &std::path::Path) {
    assert_eq!(std::fs::read(outside.join("a.m4a")).unwrap(), b"aaa");
    let names: Vec<_> = std::fs::read_dir(outside)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec!["a.m4a"]);
}

/// 管理中の曲のパスが root の外のファイルへのシンボリックリンクに差し替わった
fn symlink_final_outside(env: &Env, rel: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let outside = env.base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let target = outside.join("t.m4a");
    std::fs::write(&target, b"outside").unwrap();
    let link = env.root.path().join(rel);
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    (link, target)
}

fn assert_link_untouched(link: &std::path::Path, target: &std::path::Path) {
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read_link(link).unwrap(), target);
    assert_eq!(std::fs::read(target).unwrap(), b"outside");
}

/// 更新: 置き先のリンクを上書きせずに止める
#[test]
fn update_does_not_overwrite_final_symlink() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "A/a.m4a", b"aaa");
    let (link, target) = symlink_final_outside(&env, "A/a.m4a");
    env.server.put_track(1, "A/a.m4a", b"bbbb");
    assert!(matches!(exec_all(&mut env), Err(Error::Stop(_))));
    assert_link_untouched(&link, &target);
}

/// 削除: リンクを消さずに止める
#[test]
fn delete_does_not_remove_final_symlink() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "A/a.m4a", b"aaa");
    let (link, target) = symlink_final_outside(&env, "A/a.m4a");
    env.server.remove_track(1);
    assert!(matches!(exec_all(&mut env), Err(Error::Stop(_))));
    assert_link_untouched(&link, &target);
}

/// 削除の途中で落ちた後にリンクへ差し替わった: 回復もリンクを消さない
#[test]
fn delete_recovery_does_not_remove_final_symlink() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "A/a.m4a", b"aaa");
    env.server.remove_track(1);
    env.fp.arm("exec.delete.track_deleted", 1);
    assert!(matches!(exec_all(&mut env), Err(Error::Crash(_))));
    let (link, target) = symlink_final_outside(&env, "A/a.m4a");
    assert!(env.with_ctx(recover).is_err());
    assert_link_untouched(&link, &target);
}

/// 置き先の確認の後・rename の前に管理外のファイルが現れた: rename が上書きを断り、確認と同じく
/// 「管理外と衝突」にする
#[test]
fn add_does_not_overwrite_file_created_after_check() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    let abs = env.root.abs("A/a.m4a").unwrap();
    env.fp.on("exec.add.checked", 1, move || {
        std::fs::write(&abs, b"mine").unwrap();
    });
    let (_p, r, m) = env.confirm_all();
    let errors = env.with_ctx(|cx| {
        exec::run(cx, &r, &m).unwrap();
        cx.errors.clone()
    });
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].reason, UNMANAGED_COLLISION);
    assert_eq!(
        std::fs::read(env.root.abs("A/a.m4a").unwrap()).unwrap(),
        b"mine"
    );
    assert!(!env.root.exists("A/a.m4a.spindle-tmp").unwrap());
    let s = env.state();
    assert!(s.pending_ops.is_empty());
    assert!(!s.tracks.contains_key(&1));
    assert!(env.music.tracks().is_empty());
}

#[test]
fn add_and_update_record_manifest_origin_in_state() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.server.set_origin(1, "Lib/a.flac", "opus");
    exec_all(&mut env).unwrap();
    let e = env.state().tracks[&1].clone();
    assert_eq!(e.rel_path, "Lib/a.flac");
    assert_eq!(e.source, "opus");
}

#[test]
fn update_records_manifest_origin_in_state() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "A/a.m4a", b"aaa");
    env.server.put_track(1, "A/a.m4a", b"bbbb");
    env.server.set_origin(1, "Lib/b.flac", "aac");
    exec_all(&mut env).unwrap();
    let e = env.state().tracks[&1].clone();
    assert_eq!(e.rel_path, "Lib/b.flac");
    assert_eq!(e.source, "aac");
}

#[test]
fn add_without_origin_in_manifest_is_item_error_and_skipped() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.server.put_track(2, "A/b.m4a", b"bbb");
    let (_p, r, mut m) = env.confirm_all();
    m.items.retain(|i| i.track_id != 1);
    let (o, errors) = env.with_ctx(|cx| {
        let o = exec::run(cx, &r, &m).unwrap();
        (o, cx.errors.clone())
    });
    assert_eq!(o.executed, 1);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].ref_id, 1);
    assert_eq!(errors[0].reason, exec::NO_ORIGIN);
    let s = env.state();
    assert!(!s.tracks.contains_key(&1));
    assert!(s.tracks.contains_key(&2));
    assert!(s.pending_ops.is_empty());
    assert!(!env.root.exists("A/a.m4a").unwrap());
}

#[test]
fn update_without_origin_in_manifest_is_item_error_and_skipped() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "A/a.m4a", b"aaa");
    env.server.put_track(1, "A/a.m4a", b"bbbb");
    let (_p, r, mut m) = env.confirm_all();
    m.items.clear();
    let errors = env.with_ctx(|cx| {
        exec::run(cx, &r, &m).unwrap();
        cx.errors.clone()
    });
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].reason, exec::NO_ORIGIN);
    assert_eq!(
        std::fs::read(env.root.abs("A/a.m4a").unwrap()).unwrap(),
        b"aaa"
    );
    assert!(env.state().pending_ops.is_empty());
}

#[test]
fn recovered_add_copies_origin_from_pending_op() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.server.set_origin(1, "Lib/a.flac", "opus");
    env.fp.arm("music.add:after", 1);
    assert!(matches!(exec_all(&mut env), Err(Error::Crash(_))));
    env.with_ctx(recover).unwrap();
    let e = env.state().tracks[&1].clone();
    assert_eq!(e.rel_path, "Lib/a.flac");
    assert_eq!(e.source, "opus");
}

#[test]
fn recovered_add_without_recorded_origin_is_item_error_and_not_in_state() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.fp.arm("music.add:after", 1);
    assert!(matches!(exec_all(&mut env), Err(Error::Crash(_))));
    let mut s = env.state();
    for o in &mut s.pending_ops {
        o.rel_path = None;
        o.source = None;
    }
    env.file().save(&s).unwrap();
    let errors = env.with_ctx(|cx| {
        recover(cx).unwrap();
        cx.errors.clone()
    });
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].reason, exec::NO_ORIGIN);
    let s = env.state();
    assert!(!s.tracks.contains_key(&1));
    assert!(s.pending_ops.is_empty());
}

/// 引き継ぎ直後の state の曲を `ADOPTED_TOKEN` にし、サーバ側に同じ中身の別トークンを置く
fn adopted_track(env: &Env, bytes_in_state: &[u8]) -> spindle_agent::state::TrackEntry {
    let e = env.synced_track(1, "A/a.m4a", bytes_in_state);
    let mut s = env.state();
    s.tracks.get_mut(&1).unwrap().token = spindle_agent::state::ADOPTED_TOKEN.to_owned();
    env.file().save(&s).unwrap();
    // サーバ側の current も同じ印にする（adopt はそう書く）
    let mut cur = env.server.current();
    for c in &mut cur {
        c.token = spindle_agent::state::ADOPTED_TOKEN.to_owned();
    }
    env.server.set_current(cur, vec![]);
    e
}

#[test]
fn adopted_update_with_same_content_rewrites_token_only() {
    let mut env = Env::new();
    env.paired();
    let before = adopted_track(&env, b"aaa");
    let it = env.server.put_track(1, "A/a.m4a", b"aaa");
    env.server.set_origin(1, "Lib/new.flac", "opus");
    let calls = env.music.calls().len();
    let o = exec_all(&mut env).unwrap();
    assert_eq!(o.executed, 1);
    assert!(env.server.fetch_log().is_empty());
    assert_eq!(env.music.calls().len(), calls, "{:?}", env.music.calls());
    let s = env.state();
    assert_eq!(s.tracks[&1].token, it.token);
    assert_eq!(s.tracks[&1].persistent_id, before.persistent_id);
    assert_eq!(s.tracks[&1].path, before.path);
    assert_eq!(s.tracks[&1].rel_path, "Lib/new.flac");
    assert_eq!(s.tracks[&1].source, "opus");
    assert!(s.pending_ops.is_empty());
}

#[test]
fn adopted_update_with_different_sha_downloads() {
    let mut env = Env::new();
    env.paired();
    adopted_track(&env, b"aaa");
    let it = env.server.put_track(1, "A/a.m4a", b"bbbb");
    exec_all(&mut env).unwrap();
    assert!(!env.server.fetch_log().is_empty());
    assert_eq!(
        std::fs::read(env.root.abs("A/a.m4a").unwrap()).unwrap(),
        b"bbbb"
    );
    assert_eq!(env.state().tracks[&1].token, it.token);
}

#[test]
fn adopted_update_with_same_sha_but_different_size_downloads() {
    let mut env = Env::new();
    env.paired();
    adopted_track(&env, b"aaa");
    // state の大きさだけが計画と違う（sha256 は同じ）。近道せずに取り直す
    let mut s = env.state();
    s.tracks.get_mut(&1).unwrap().size = 99;
    env.file().save(&s).unwrap();
    let it = env.server.put_track(1, "A/a.m4a", b"aaa");
    exec_all(&mut env).unwrap();
    assert!(!env.server.fetch_log().is_empty());
    let s = env.state();
    assert_eq!(s.tracks[&1].token, it.token);
    assert_eq!(s.tracks[&1].size, 3);
}

#[test]
fn non_adopted_update_with_same_content_takes_the_normal_path() {
    let mut env = Env::new();
    env.paired();
    let before = env.synced_track(1, "A/a.m4a", b"aaa");
    // 引き継ぎの印ではない古いトークン。中身は計画と同じでも近道しない
    let mut s = env.state();
    s.tracks.get_mut(&1).unwrap().token = "old.token".to_owned();
    env.file().save(&s).unwrap();
    let mut cur = env.server.current();
    for c in &mut cur {
        c.token = "old.token".to_owned();
    }
    env.server.set_current(cur, vec![]);
    let it = env.server.put_track(1, "A/a.m4a", b"aaa");
    assert_eq!(before.sha256, it.sha256);
    exec_all(&mut env).unwrap();
    assert!(!env.server.fetch_log().is_empty());
    assert_eq!(env.state().tracks[&1].token, it.token);
}

#[test]
fn adopted_unchanged_updates_are_not_counted_against_free_space() {
    let mut env = Env::new();
    env.paired();
    let before = adopted_track(&env, b"aaa");
    let it = env.server.put_track(1, "A/a.m4a", b"aaa");
    // 引き継いだ写しが空きより大きくても、近道に入る曲は受け取らないので容量の見積もりに入れない
    let huge = u64::MAX / 2;
    let mut s = env.state();
    s.tracks.get_mut(&1).unwrap().size = huge;
    env.file().save(&s).unwrap();
    let (_p, mut r, m) = env.confirm_all();
    assert_eq!(r.items.len(), 1);
    r.items[0].size = huge;
    let o = env.with_ctx(|cx| exec::run(cx, &r, &m)).unwrap();
    assert_eq!(o.executed, 1);
    assert!(o.stop.is_none());
    assert!(env.server.fetch_log().is_empty());
    let s = env.state();
    assert_eq!(s.tracks[&1].token, it.token);
    assert_eq!(s.tracks[&1].persistent_id, before.persistent_id);
    assert!(s.pending_ops.is_empty());
}

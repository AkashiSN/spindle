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

/// spindle フォルダの直下の (名前, 中身の persistent ID)
fn lists(env: &Env, folder: &str) -> Vec<(String, Vec<String>)> {
    let mut v: Vec<_> = env
        .music
        .playlists()
        .into_iter()
        .filter(|p| p.parent.as_deref() == Some(folder))
        .map(|p| (p.name, p.tracks))
        .collect();
    v.sort();
    v
}

#[test]
fn playlist_is_created_with_track_order() {
    let mut env = Env::new();
    let folder = env.paired();
    let a = env.synced_track(1, "a.m4a", b"a");
    let b = env.synced_track(2, "b.m4a", b"b");
    env.server.put_playlist(10, "Favs", &[2, 1]);
    exec_all(&mut env).unwrap();
    assert_eq!(
        lists(&env, &folder),
        vec![("Favs".into(), vec![b.persistent_id, a.persistent_id])]
    );
    assert_eq!(env.state().playlists[&10].name, "Favs");
}

#[test]
fn update_with_rename_swaps_in_a_new_playlist() {
    let mut env = Env::new();
    let folder = env.paired();
    env.synced_track(1, "a.m4a", b"a");
    let b = env.synced_track(2, "b.m4a", b"b");
    env.server.put_playlist(10, "Favs", &[1, 2]);
    exec_all(&mut env).unwrap();
    env.accept_state();
    let before = env.state().playlists[&10].persistent_id.clone();
    env.server.put_playlist(10, "Best", &[2]);
    exec_all(&mut env).unwrap();
    assert_eq!(
        lists(&env, &folder),
        vec![("Best".into(), vec![b.persistent_id])]
    );
    let after = env.state().playlists[&10].clone();
    assert_eq!(after.name, "Best");
    assert_ne!(after.persistent_id, before);
}

#[test]
fn delete_removes_only_the_managed_playlist() {
    let mut env = Env::new();
    let folder = env.paired();
    env.synced_track(1, "a.m4a", b"a");
    env.server.put_playlist(10, "Favs", &[1]);
    exec_all(&mut env).unwrap();
    env.accept_state();
    env.music.insert_playlist(Some(&folder), "Mine");
    env.server.remove_playlist(10);
    exec_all(&mut env).unwrap();
    assert_eq!(lists(&env, &folder), vec![("Mine".into(), vec![])]);
    assert!(env.state().playlists.is_empty());
}

#[test]
fn unmanaged_same_name_playlist_is_held() {
    let mut env = Env::new();
    let folder = env.paired();
    env.synced_track(1, "a.m4a", b"a");
    let theirs = env.music.insert_playlist(Some(&folder), "Favs");
    env.server.put_playlist(10, "Favs", &[1]);
    let (_p, r, m) = env.confirm_all();
    let errors = env.with_ctx(|cx| {
        exec::run(cx, &r, &m).unwrap();
        cx.errors.clone()
    });
    assert_eq!(errors[0].reason, exec::UNMANAGED_COLLISION);
    assert_eq!(lists(&env, &folder), vec![("Favs".into(), vec![])]);
    assert!(env
        .music
        .playlists()
        .iter()
        .any(|p| p.persistent_id == theirs));
    assert!(env.state().playlists.is_empty());
}

#[test]
fn interrupted_swap_cleans_only_recorded_temp() {
    for name in [
        "music.create_playlist:after",
        "music.set_playlist_tracks:after",
        "playlist.old_deleted",
        "music.rename_playlist:after",
    ] {
        let mut env = Env::new();
        let folder = env.paired();
        let a = env.synced_track(1, "a.m4a", b"a");
        // 別の人が作った「.tmp-」で始まるプレイリスト（消してはいけない）
        let decoy = env.music.insert_playlist(Some(&folder), ".tmp-not-ours");
        env.server.put_playlist(10, "Favs", &[1]);
        env.fp.arm(name, 1);
        assert!(matches!(exec_all(&mut env), Err(Error::Crash(_))), "{name}");
        resume(&mut env);
        let got = lists(&env, &folder);
        assert_eq!(
            got,
            vec![
                (".tmp-not-ours".into(), vec![]),
                ("Favs".into(), vec![a.persistent_id.clone()])
            ],
            "{name}"
        );
        assert!(
            env.music
                .playlists()
                .iter()
                .any(|p| p.persistent_id == decoy),
            "{name}"
        );
        assert!(env.state().pending_ops.is_empty(), "{name}");
    }
}

#[test]
fn interrupted_delete_is_finished_by_recover() {
    for name in ["music.delete_playlist", "music.delete_playlist:after"] {
        let mut env = Env::new();
        let folder = env.paired();
        env.synced_track(1, "a.m4a", b"a");
        env.server.put_playlist(10, "Favs", &[1]);
        exec_all(&mut env).unwrap();
        env.accept_state();
        env.server.remove_playlist(10);
        env.fp.arm(name, 1);
        assert!(matches!(exec_all(&mut env), Err(Error::Crash(_))), "{name}");
        env.with_ctx(recover).unwrap();
        let s = env.state();
        assert!(lists(&env, &folder).is_empty(), "{name}");
        assert!(s.playlists.is_empty(), "{name}");
        assert!(s.pending_ops.is_empty(), "{name}");
        assert!(s.needs_report, "{name}");
    }
}

#[test]
fn impossible_pending_phase_is_a_state_error() {
    use spindle_agent::state::{OpPhase, PendingKind, PendingOp};
    let mut env = Env::new();
    env.paired();
    let mut s = env.state();
    s.pending_ops.push(PendingOp {
        op_id: "f".repeat(32),
        op: PendingKind::Playlist,
        ref_id: 10,
        persistent_id: None,
        from: None,
        to: Some("Favs".into()),
        token: None,
        size: 0,
        sha256: None,
        phase: OpPhase::Adding,
        started_at: 0,
        max_database_id: None,
        candidates: None,
    });
    env.file().save(&s).unwrap();
    assert!(matches!(env.with_ctx(recover), Err(Error::State(_))));
    assert_eq!(env.state().pending_ops.len(), 1);
}

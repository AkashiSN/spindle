#![cfg(feature = "fake")]
mod support;

use spindle_agent::rediscover::rediscover;
use spindle_agent::server::Server;
use spindle_agent::state::STALE_TOKEN;
use spindle_agent::Error;
use support::Env;

fn run(env: &mut Env) -> spindle_agent::Result<()> {
    let m = env.server.manifest().unwrap();
    env.with_ctx(|cx| rediscover(cx, &m))
}

#[test]
fn reflected_track_stays_and_no_report_needed() {
    let mut env = Env::new();
    env.paired();
    let e = env.synced_track(1, "A/a.m4a", b"aaa");
    run(&mut env).unwrap();
    let s = env.state();
    assert_eq!(s.tracks[&1], e);
    assert!(!s.needs_report);
}

#[test]
fn crash_after_add_adopts_without_readding() {
    // add の後、state を書く前に落ちた: track とファイルはあるが state に無い
    let mut env = Env::new();
    env.paired();
    let item = env.server.put_track(1, "A/a.m4a", b"aaa");
    env.write_local("A/a.m4a", b"aaa");
    let pid = env.music.insert_track(&env.root.abs("A/a.m4a").unwrap());
    run(&mut env).unwrap();
    let s = env.state();
    assert_eq!(s.tracks[&1].persistent_id, pid);
    assert_eq!(s.tracks[&1].token, item.token);
    assert!(s.needs_report);
    assert!(!env.music.calls().contains(&"add".to_owned()));
}

#[test]
fn mismatching_file_is_not_adopted() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.write_local("A/a.m4a", b"zzz");
    env.music.insert_track(&env.root.abs("A/a.m4a").unwrap());
    run(&mut env).unwrap();
    assert!(env.state().tracks.is_empty());
}

#[test]
fn duplicate_location_is_error() {
    let mut env = Env::new();
    env.paired();
    let e = env.synced_track(1, "A/a.m4a", b"aaa");
    env.music.insert_track(&env.root.abs("A/a.m4a").unwrap());
    let before = env.state();
    assert!(matches!(run(&mut env), Err(Error::Stop(m)) if m.contains("複数")));
    assert_eq!(env.state(), before);
    assert_eq!(env.state().tracks[&1], e);
}

#[test]
fn externally_modified_file_becomes_stale_but_keeps_track() {
    let mut env = Env::new();
    env.paired();
    let e = env.synced_track(1, "A/a.m4a", b"aaa");
    env.write_local("A/a.m4a", b"edited-in-music-app");
    run(&mut env).unwrap();
    let s = env.state();
    assert_eq!(s.tracks[&1].token, STALE_TOKEN);
    assert_eq!(s.tracks[&1].persistent_id, e.persistent_id);
    assert_eq!(s.tracks[&1].size, 19);
    assert!(s.needs_report);
}

#[test]
fn touched_but_same_content_only_refreshes_stat_cache() {
    let mut env = Env::new();
    env.paired();
    let e = env.synced_track(1, "A/a.m4a", b"aaa");
    // 同じ中身で書き直す（inode か mtime が変わる）
    let p = env.root.abs("A/a.m4a").unwrap();
    std::fs::remove_file(&p).unwrap();
    std::fs::write(&p, b"aaa").unwrap();
    run(&mut env).unwrap();
    let s = env.state();
    assert_eq!(s.tracks[&1].token, e.token);
    assert!(!s.needs_report);
}

#[test]
fn track_deleted_in_music_app_is_dropped_with_its_file() {
    let mut env = Env::new();
    env.paired();
    let e = env.synced_track(1, "A/a.m4a", b"aaa");
    env.music.remove_track(&e.persistent_id);
    run(&mut env).unwrap();
    let s = env.state();
    assert!(s.tracks.is_empty());
    assert!(s.needs_report);
    assert!(!env.root.exists("A/a.m4a").unwrap());
}

#[test]
fn relocated_track_is_pointed_back_when_file_is_intact() {
    let mut env = Env::new();
    env.paired();
    let e = env.synced_track(1, "A/a.m4a", b"aaa");
    env.music
        .relocate(&e.persistent_id, std::path::Path::new("/elsewhere/a.m4a"));
    run(&mut env).unwrap();
    assert_eq!(
        env.music.tracks()[0].location.as_deref(),
        Some(env.root.abs("A/a.m4a").unwrap().as_path())
    );
    assert_eq!(env.state().tracks[&1], e);
}

#[test]
fn missing_playlist_is_dropped() {
    let mut env = Env::new();
    env.paired();
    let mut s = env.state();
    s.playlists.insert(
        5,
        spindle_agent::state::PlaylistEntry {
            persistent_id: "GONE".into(),
            name: "Favs".into(),
            token: "p".into(),
        },
    );
    env.file().save(&s).unwrap();
    run(&mut env).unwrap();
    assert!(env.state().playlists.is_empty());
    assert!(env.state().needs_report);
}

#[test]
fn missing_spindle_folder_stops() {
    let mut env = Env::new();
    env.paired();
    let mut s = env.state();
    s.setup.as_mut().unwrap().folder_pid = Some("NOPE".into());
    env.file().save(&s).unwrap();
    assert!(matches!(run(&mut env), Err(Error::Stop(_))));
}

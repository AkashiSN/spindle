#![cfg(feature = "fake")]
mod support;

use spindle_agent::pair::{continue_setup, pair};
use spindle_agent::secrets::{FileSecrets, Secrets};
use spindle_agent::state::SetupPhase;
use spindle_agent::Error;
use support::{Env, UUID};

fn do_pair(env: &mut Env) -> spindle_agent::Result<()> {
    let secrets = FileSecrets::new(&env.state_dir);
    env.with_ctx(|cx| pair(cx, &secrets, "https://music.example", false, "code"))
}

fn spindle_folders(env: &Env) -> Vec<String> {
    env.music
        .playlists()
        .into_iter()
        .filter(|p| p.is_folder)
        .map(|p| p.name)
        .collect()
}

#[test]
fn fresh_pair_sets_everything_up() {
    let mut env = Env::new();
    do_pair(&mut env).unwrap();
    let s = env.state();
    let setup = s.setup.unwrap();
    assert_eq!(setup.phase, SetupPhase::Done);
    assert_eq!(s.server.unwrap().device_uuid, UUID);
    assert_eq!(spindle_folders(&env), vec!["spindle"]);
    assert_eq!(env.root.read_marker().unwrap().unwrap().nonce, setup.nonce);
    assert_eq!(
        FileSecrets::new(&env.state_dir).get().unwrap().as_deref(),
        Some("fake.token")
    );
}

#[test]
fn existing_spindle_folder_stops_without_creating() {
    let mut env = Env::new();
    env.music.insert_folder("spindle");
    assert!(matches!(do_pair(&mut env), Err(Error::Stop(m)) if m.contains("spindle")));
    assert_eq!(spindle_folders(&env), vec!["spindle"]);
}

#[test]
fn non_empty_root_stops() {
    let mut env = Env::new();
    env.write_local("old.m4a", b"x");
    assert!(matches!(do_pair(&mut env), Err(Error::Stop(_))));
    assert!(spindle_folders(&env).is_empty());
}

#[test]
fn copy_setting_on_stops_before_using_the_code() {
    let mut env = Env::new();
    env.music.set_copy_setting(Some(true));
    assert!(matches!(do_pair(&mut env), Err(Error::Stop(m)) if m.contains("コピー")));
    assert!(env.state().server.is_none());
}

#[test]
fn every_crash_point_resumes_to_a_single_folder() {
    let names = [
        "state.saved",
        "pair.marker_written",
        "music.create_folder",
        "music.create_folder:after",
        "music.rename_playlist",
        "music.rename_playlist:after",
    ];
    for name in names {
        for nth in 1..=6 {
            for via_sync in [false, true] {
                let mut env = Env::new();
                env.fp.arm(name, nth);
                if !matches!(do_pair(&mut env), Err(Error::Crash(_))) {
                    continue;
                }
                if via_sync && env.state().setup.is_some() {
                    env.with_ctx(continue_setup).unwrap();
                } else {
                    do_pair(&mut env).unwrap();
                }
                let s = env.state();
                assert_eq!(
                    s.setup.as_ref().unwrap().phase,
                    SetupPhase::Done,
                    "{name}#{nth}"
                );
                assert_eq!(
                    spindle_folders(&env),
                    vec!["spindle"],
                    "{name}#{nth}/{via_sync}"
                );
                let folder = s.setup.unwrap().folder_pid.unwrap();
                assert_eq!(env.music.folder_name(&folder).as_deref(), Some("spindle"));
            }
        }
    }
}

#[test]
fn repair_with_same_device_only_replaces_token() {
    let mut env = Env::new();
    do_pair(&mut env).unwrap();
    let before = env.state();
    do_pair(&mut env).unwrap();
    assert_eq!(env.state().setup, before.setup);
    assert_eq!(spindle_folders(&env), vec!["spindle"]);
}

#[test]
fn repair_as_another_device_stops() {
    let mut env = Env::new();
    do_pair(&mut env).unwrap();
    env.server = spindle_agent::server::fake::FakeServer::new("other-uuid");
    assert!(matches!(do_pair(&mut env), Err(Error::Stop(_))));
    assert_eq!(env.state().server.unwrap().device_uuid, UUID);
}

#[test]
fn foreign_marker_stops() {
    let mut env = Env::new();
    env.root
        .write_marker(&spindle_agent::local::Marker {
            device_uuid: "x".into(),
            nonce: "y".into(),
        })
        .unwrap();
    assert!(matches!(do_pair(&mut env), Err(Error::Stop(_))));
}

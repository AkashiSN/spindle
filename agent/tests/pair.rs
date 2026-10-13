#![cfg(feature = "fake")]
mod support;

use spindle_agent::adopt::{pair_adopt, AdoptSummary};
use spindle_agent::exec;
use spindle_agent::pair::{continue_setup, pair};
use spindle_agent::secrets::{FileSecrets, Secrets};
use spindle_agent::server::fake::FakeServer;
use spindle_agent::state::{PlaylistEntry, SetupPhase, TrackEntry, ADOPTED_TOKEN, STALE_TOKEN};
use spindle_agent::sync::SyncOutcome;
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

#[test]
fn media_folder_root_stops_before_using_the_code() {
    let mut env = Env::new();
    env.write_local(".Media Preferences.plist", b"x");
    assert!(matches!(do_pair(&mut env), Err(Error::Stop(m)) if m.contains("メディアフォルダ")));
    assert!(env.state().server.is_none());
}

/// メディアフォルダが root の親（~/Music）: root はその中なので、コードを使う前に止める
#[test]
fn media_folder_parent_stops_before_using_the_code() {
    let mut env = Env::new();
    let parent = env.root.path().parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&parent).unwrap();
    std::fs::write(parent.join(".Media Preferences.plist"), b"x").unwrap();
    assert!(matches!(do_pair(&mut env), Err(Error::Stop(m)) if m.contains("メディアフォルダ")));
    assert!(env.state().server.is_none());
    assert!(!env.root.path().exists());
}

/// トークンを保存できない置き場（ssh 越しの Keychain の代わり）
struct Unwritable;

impl Secrets for Unwritable {
    fn get(&self) -> spindle_agent::Result<Option<String>> {
        Ok(None)
    }
    fn set(&self, _token: &str) -> spindle_agent::Result<()> {
        Err(Error::Stop("保存できない".into()))
    }
    fn check_writable(&self) -> spindle_agent::Result<()> {
        Err(Error::Stop("保存できない".into()))
    }
}

#[test]
fn unwritable_secrets_stop_before_using_the_code() {
    let mut env = Env::new();
    let res = env.with_ctx(|cx| pair(cx, &Unwritable, "https://music.example", false, "code"));
    assert!(matches!(res, Err(Error::Stop(m)) if m.contains("保存できない")));
    assert_eq!(env.server.pair_calls(), 0);
    assert!(env.state().server.is_none());
    // 置き場を直せば同じコードで pair できる
    do_pair(&mut env).unwrap();
    assert_eq!(env.server.pair_calls(), 1);
}

#[test]
fn file_secrets_check_writable() {
    let d = tempfile::tempdir().unwrap();
    let s = FileSecrets::new(&d.path().join("state"));
    s.check_writable().unwrap();
    // 試しのファイルを残さず、トークンも作らない
    assert_eq!(
        std::fs::read_dir(d.path().join("state")).unwrap().count(),
        0
    );
    assert_eq!(s.get().unwrap(), None);
    // 書けないディレクトリ（ファイルが塞いでいる）なら止める
    std::fs::write(d.path().join("blocked"), b"x").unwrap();
    let s = FileSecrets::new(&d.path().join("blocked"));
    assert!(matches!(s.check_writable(), Err(Error::Stop(_))));
}

/// 本番のトークンの置き場（`token` か `.token.tmp`）がディレクトリ: `set` が失敗するので、コードを使う前に止める
#[test]
fn file_token_path_blocked_stops_before_using_the_code() {
    for name in ["token", ".token.tmp"] {
        let mut env = Env::new();
        std::fs::create_dir_all(env.state_dir.join(name)).unwrap();
        let s = FileSecrets::new(&env.state_dir);
        assert!(
            matches!(s.check_writable(), Err(Error::Stop(m)) if m.contains(name)),
            "{name}"
        );
        assert!(matches!(do_pair(&mut env), Err(Error::Stop(_))), "{name}");
        assert_eq!(env.server.pair_calls(), 0, "{name}");
        assert!(env.state().server.is_none(), "{name}");
        // 直せば同じコードで pair できる
        std::fs::remove_dir(env.state_dir.join(name)).unwrap();
        do_pair(&mut env).unwrap();
        assert_eq!(env.server.pair_calls(), 1, "{name}");
    }
}

/// 既にトークンがあっても check_writable はそれに触らず、試しのファイルも残さない
#[test]
fn file_secrets_check_writable_keeps_existing_token() {
    let d = tempfile::tempdir().unwrap();
    let s = FileSecrets::new(d.path());
    s.set("old.token").unwrap();
    s.check_writable().unwrap();
    assert_eq!(s.get().unwrap().as_deref(), Some("old.token"));
    let names: Vec<_> = std::fs::read_dir(d.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("token")]);
}

fn do_pair_adopt(env: &mut Env) -> spindle_agent::Result<AdoptSummary> {
    let secrets = FileSecrets::new(&env.state_dir);
    env.with_ctx(|cx| pair_adopt(cx, &secrets, "https://new.example", true, "code"))
}

/// 同期済みの state（曲 1・2、プレイリスト 10）を作ってからサーバの DB 全損を模す。
/// 新しいサーバは空で、uuid も違う（pair の差し替えで前の uuid に戻る）
struct Synced {
    a: TrackEntry,
    b: TrackEntry,
    playlist_pid: String,
}

fn synced_then_db_lost(env: &mut Env) -> Synced {
    let folder = env.paired();
    let a = env.synced_track(1, "A/a.m4a", b"aaa");
    let b = env.synced_track(2, "B/b.m4a", b"bbb");
    let playlist_pid = env.music.insert_playlist(Some(&folder), "Favs");
    let mut s = env.state();
    s.playlists.insert(
        10,
        PlaylistEntry {
            persistent_id: playlist_pid.clone(),
            name: "Favs".into(),
            token: "pl.token".into(),
        },
    );
    // 2 は外で書き換えられた古い写し。前の計画の途中・報告待ちも残っている
    s.tracks.get_mut(&2).unwrap().token = STALE_TOKEN.into();
    s.plan_id = Some(5);
    s.needs_report = true;
    env.file().save(&s).unwrap();
    env.server = FakeServer::new("99999999999999999999999999999999");
    // CLI の pair と同じく、トークン無しのサーバで始める
    env.server.set_anonymous();
    Synced { a, b, playlist_pid }
}

#[test]
fn pair_adopt_remaps_state_and_keeps_persistent_ids() {
    let mut env = Env::new();
    let old = synced_then_db_lost(&mut env);
    env.server.put_track(7, "A/a.m4a", b"aaa");
    env.server.put_track(8, "B/b.m4a", b"bbb");
    env.server.put_playlist(20, "Favs", &[7, 8]);
    let sum = do_pair_adopt(&mut env).unwrap();
    assert_eq!(
        sum,
        AdoptSummary {
            tracks_adopted: 2,
            tracks_dropped: 0,
            playlists_adopted: 1,
            playlists_dropped: 0,
        }
    );
    // 前の登録の uuid で pair し、state の曲を送った
    assert_eq!(env.server.last_pair_adopt_uuid().as_deref(), Some(UUID));
    let calls = env.server.adopt_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tracks.len(), 2);
    assert!(!calls[0].tracks[0].stale);
    assert!(calls[0].tracks[1].stale);
    assert_eq!(calls[0].tracks[1].rel_path, "B/b.m4a");
    assert_eq!(calls[0].playlists[0].name, "Favs");

    let s = env.state();
    assert_eq!(s.tracks.keys().copied().collect::<Vec<_>>(), vec![7, 8]);
    assert_eq!(s.tracks[&7].persistent_id, old.a.persistent_id);
    assert_eq!(s.tracks[&7].token, ADOPTED_TOKEN);
    assert_eq!(s.tracks[&7].path, "A/a.m4a");
    assert_eq!(s.tracks[&8].persistent_id, old.b.persistent_id);
    assert_eq!(s.tracks[&8].token, STALE_TOKEN);
    assert_eq!(s.playlists.keys().copied().collect::<Vec<_>>(), vec![20]);
    assert_eq!(s.playlists[&20].persistent_id, old.playlist_pid);
    assert_eq!(s.playlists[&20].token, STALE_TOKEN);
    assert_eq!(s.plan_id, None);
    assert!(!s.needs_report);
    assert!(!s.adopt_pending);
    let server = s.server.unwrap();
    assert_eq!(server.device_uuid, UUID);
    assert_eq!(server.url, "https://new.example");
    assert!(server.insecure_http);
    assert_eq!(
        FileSecrets::new(&env.state_dir).get().unwrap().as_deref(),
        Some("fake.token")
    );
    // 印と初期化はそのまま
    assert_eq!(s.setup.unwrap().phase, SetupPhase::Done);
    assert_eq!(env.root.read_marker().unwrap().unwrap().device_uuid, UUID);
}

#[test]
fn pair_adopt_drops_unmatched_entries_but_keeps_files_and_tracks() {
    let mut env = Env::new();
    let old = synced_then_db_lost(&mut env);
    // 新しい Library には 1 の曲だけがあり、プレイリストは無い
    env.server.put_track(7, "A/a.m4a", b"aaa");
    let sum = do_pair_adopt(&mut env).unwrap();
    assert_eq!(
        sum,
        AdoptSummary {
            tracks_adopted: 1,
            tracks_dropped: 1,
            playlists_adopted: 0,
            playlists_dropped: 1,
        }
    );
    let s = env.state();
    assert_eq!(s.tracks.keys().copied().collect::<Vec<_>>(), vec![7]);
    assert!(s.playlists.is_empty());
    // 外したものは管理外にするだけ: ファイルもミュージック.app の track もプレイリストも残す
    assert!(env.root.exists("B/b.m4a").unwrap());
    let pids: Vec<_> = env.music_paths().into_iter().map(|(_, p)| p).collect();
    assert!(pids.contains(&old.b.persistent_id));
    assert_eq!(env.music.tracks().len(), 2);
    assert!(env
        .music
        .playlists()
        .iter()
        .any(|p| p.persistent_id == old.playlist_pid));
}

#[test]
fn adopt_stops_before_pairing_when_pending_remains() {
    let mut env = Env::new();
    env.paired();
    // コピー ON で add 直後に落ち、回復が候補を出して止まる（resolve 待ちの pending が残る）
    env.music.set_copy_on(true);
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.fp.arm("music.add:after", 1);
    let (_p, r, m) = env.confirm_all();
    assert!(env.with_ctx(|cx| exec::run(cx, &r, &m)).is_err());
    assert!(matches!(do_pair_adopt(&mut env), Err(Error::Stop(_))));
    assert_eq!(env.server.pair_calls(), 0);
    let s = env.state();
    assert_eq!(s.pending_ops.len(), 1);
    assert!(!s.adopt_pending);
}

#[test]
fn pair_adopt_requires_a_previous_registration() {
    let mut env = Env::new();
    let res = do_pair_adopt(&mut env);
    assert!(
        matches!(&res, Err(Error::Stop(m)) if m.contains("引き継ぐ前の登録がありません")),
        "{res:?}"
    );
    assert_eq!(env.server.pair_calls(), 0);
    assert!(env.state().server.is_none());
}

#[test]
fn sync_resumes_a_pending_adopt() {
    let mut env = Env::new();
    let old = synced_then_db_lost(&mut env);
    env.server.put_track(7, "A/a.m4a", b"aaa");
    env.server.put_track(8, "B/b.m4a", b"bbb");
    // pair の後、adopt の送信で落ちる
    env.server.fail_adopt(1);
    assert!(matches!(do_pair_adopt(&mut env), Err(Error::Server(_))));
    let s = env.state();
    assert!(s.adopt_pending);
    assert_eq!(s.tracks.keys().copied().collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(env.server.pair_calls(), 1);
    // 次の sync が manifest の前に付け替えを終える（差分の確認は断る）
    env.ui.answers.push_back(false);
    assert_eq!(env.sync().unwrap(), SyncOutcome::Declined);
    assert_eq!(env.server.adopt_calls().len(), 2);
    assert_eq!(env.server.pair_calls(), 1);
    let s = env.state();
    assert!(!s.adopt_pending);
    assert_eq!(s.tracks.keys().copied().collect::<Vec<_>>(), vec![7, 8]);
    assert_eq!(s.tracks[&7].persistent_id, old.a.persistent_id);
    assert_eq!(s.tracks[&7].token, ADOPTED_TOKEN);
    assert!(s.playlists.is_empty());
    assert!(env.ui.log.iter().any(|l| l.contains("引き継ぎ")));
}

#[test]
fn pair_without_adopt_still_stops_on_uuid_mismatch_and_suggests_adopt() {
    let mut env = Env::new();
    do_pair(&mut env).unwrap();
    env.server = FakeServer::new("other-uuid");
    let res = do_pair(&mut env);
    assert!(
        matches!(&res, Err(Error::Stop(m)) if m.contains("spindle-agent pair --adopt")),
        "{res:?}"
    );
    assert_eq!(env.server.last_pair_adopt_uuid(), None);
    assert_eq!(env.state().server.unwrap().device_uuid, UUID);
}

#[test]
fn pair_adopt_stops_on_a_foreign_marker_before_using_the_code() {
    let mut env = Env::new();
    synced_then_db_lost(&mut env);
    env.server.put_track(7, "A/a.m4a", b"aaa");
    env.root
        .write_marker(&spindle_agent::local::Marker {
            device_uuid: "x".into(),
            nonce: "y".into(),
        })
        .unwrap();
    assert!(matches!(do_pair_adopt(&mut env), Err(Error::Stop(_))));
    assert_eq!(env.server.pair_calls(), 0);
    assert!(!env.state().adopt_pending);
}

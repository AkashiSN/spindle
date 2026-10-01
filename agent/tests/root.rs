//! root のパスの解決。ミュージック.app は track の場所をシンボリックリンクを解いたパスで持つので、
//! root も解いておかないと前方一致（`tracks_under`・root 相対への変換）が外れる
#![cfg(feature = "fake")]
mod support;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use spindle_agent::music::Music;
use spindle_agent::sync::{resolve_root, Paths, SyncOutcome};
use spindle_agent::Error;
use support::Env;

/// `<base>/real/Music` を作り、`<base>/link` → `<base>/real` を張る
fn linked_music(base: &Path) -> PathBuf {
    std::fs::create_dir_all(base.join("real/Music")).unwrap();
    std::os::unix::fs::symlink(base.join("real"), base.join("link")).unwrap();
    base.join("link/Music/spindle")
}

#[test]
fn root_under_symlinked_parent_is_resolved() {
    let d = tempfile::tempdir().unwrap();
    let given = linked_music(d.path());
    let real = std::fs::canonicalize(d.path().join("real")).unwrap();
    // spindle はまだ無い: 在る祖先（link/Music）を解いた後ろに足す
    assert_eq!(resolve_root(&given).unwrap(), real.join("Music/spindle"));
    // 在るときも同じ
    std::fs::create_dir(real.join("Music/spindle")).unwrap();
    assert_eq!(resolve_root(&given).unwrap(), real.join("Music/spindle"));
    // 無い要素が複数でも
    assert_eq!(
        resolve_root(&d.path().join("link/Music/x/y")).unwrap(),
        real.join("Music/x/y")
    );
}

#[test]
fn root_itself_symlink_is_resolved() {
    let d = tempfile::tempdir().unwrap();
    let real = d.path().join("elsewhere");
    std::fs::create_dir(&real).unwrap();
    let link = d.path().join("spindle");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert_eq!(
        resolve_root(&link).unwrap(),
        std::fs::canonicalize(&real).unwrap()
    );
}

#[test]
fn relative_or_unresolvable_root_stops() {
    assert!(matches!(
        resolve_root(Path::new("Music/spindle")),
        Err(Error::Stop(m)) if m.contains("絶対パス")
    ));
    let d = tempfile::tempdir().unwrap();
    assert!(matches!(
        resolve_root(&d.path().join("missing/..")),
        Err(Error::Stop(_))
    ));
}

#[test]
fn paths_resolve_root_from_env_values() {
    let d = tempfile::tempdir().unwrap();
    linked_music(d.path());
    let real = std::fs::canonicalize(d.path().join("real")).unwrap();
    // 既定（$HOME/Music/spindle）で HOME がリンク越し
    let home = OsString::from(d.path().join("link"));
    let p = Paths::from_vars(Some(home.clone()), None, None).unwrap();
    assert_eq!(p.root, real.join("Music/spindle"));
    assert_eq!(
        p.state_dir,
        d.path()
            .join("link/Library/Application Support/spindle-agent")
    );
    // SPINDLE_AGENT_ROOT もリンク越しなら解く
    let root = OsString::from(d.path().join("link/Music/spindle"));
    let p = Paths::from_vars(None, Some("/s".into()), Some(root)).unwrap();
    assert_eq!(p.root, real.join("Music/spindle"));
    // 相対の SPINDLE_AGENT_ROOT は止める
    assert!(matches!(
        Paths::from_vars(Some(home), None, Some("rel/spindle".into())),
        Err(Error::Stop(_))
    ));
}

/// 本物のように場所を解いて持つミュージック.app でも、解いた root なら反映済みの曲を root の下に見る
#[test]
fn engine_sees_resolved_music_locations_under_resolved_root() {
    let mut given = PathBuf::new();
    let mut env = Env::new_at(|base| {
        given = linked_music(base);
        resolve_root(&given).unwrap()
    });
    env.music.set_resolve_symlinks(true);
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    assert!(matches!(
        env.sync().unwrap(),
        SyncOutcome::Applied {
            executed: 1,
            errors: 0,
            ..
        }
    ));
    // 場所は解いたパスで、解いた root の下にある（解かない root では前方一致が外れる）
    assert_eq!(env.music.tracks_under(env.root.path()).unwrap().len(), 1);
    assert!(env.music.tracks_under(&given).unwrap().is_empty());
    assert_eq!(env.music_paths().len(), 1);
    // 2 回目は何もしない（見失って取り直したりしない）
    assert_eq!(env.sync().unwrap(), SyncOutcome::NothingToDo);
    assert_eq!(env.music.tracks().len(), 1);
}

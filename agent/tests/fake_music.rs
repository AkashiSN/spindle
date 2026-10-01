#![cfg(feature = "fake")]

use spindle_agent::failpoint::Failpoints;
use spindle_agent::music::fake::FakeMusic;
use spindle_agent::music::Music;
use spindle_agent::Error;

fn setup() -> (tempfile::TempDir, FakeMusic, Failpoints) {
    let d = tempfile::tempdir().unwrap();
    let fp = Failpoints::armed();
    let m = FakeMusic::new(d.path().join("media"), fp.clone());
    std::fs::create_dir_all(d.path().join("root/A")).unwrap();
    std::fs::write(d.path().join("root/A/x.m4a"), b"abc").unwrap();
    (d, m, fp)
}

#[test]
fn add_returns_track_at_location_and_lists_under_root() {
    let (d, m, _) = setup();
    let root = d.path().join("root");
    let t = m.add(&root.join("A/x.m4a")).unwrap();
    assert_eq!(t.location.as_deref(), Some(root.join("A/x.m4a").as_path()));
    assert_eq!(t.size, Some(3));
    assert!(t.database_id > 0);
    let under = m.tracks_under(&root).unwrap();
    assert_eq!(under.len(), 1);
    assert_eq!(m.max_database_id().unwrap(), t.database_id);
    assert_eq!(m.tracks_added_after(t.database_id - 1).unwrap().len(), 1);
    assert!(m.tracks_added_after(t.database_id).unwrap().is_empty());
}

#[test]
fn copy_on_puts_track_outside_root() {
    let (d, m, _) = setup();
    m.set_copy_on(true);
    let root = d.path().join("root");
    let t = m.add(&root.join("A/x.m4a")).unwrap();
    let loc = t.location.unwrap();
    assert!(!loc.starts_with(&root));
    assert_eq!(std::fs::read(&loc).unwrap(), b"abc");
    assert!(m.tracks_under(&root).unwrap().is_empty());
}

#[test]
fn after_failpoint_applies_effect_then_fails() {
    let (d, m, fp) = setup();
    fp.arm("music.add:after", 1);
    let root = d.path().join("root");
    assert!(matches!(m.add(&root.join("A/x.m4a")), Err(Error::Crash(_))));
    assert_eq!(m.tracks().len(), 1);
}

#[test]
fn before_failpoint_has_no_effect() {
    let (d, m, fp) = setup();
    fp.arm("music.add", 1);
    let root = d.path().join("root");
    assert!(m.add(&root.join("A/x.m4a")).is_err());
    assert!(m.tracks().is_empty());
}

#[test]
fn set_location_refresh_and_delete() {
    let (d, m, _) = setup();
    let root = d.path().join("root");
    let t = m.add(&root.join("A/x.m4a")).unwrap();
    m.set_location(&t.persistent_id, &root.join("B/y.m4a"))
        .unwrap();
    assert_eq!(
        m.track(&t.persistent_id)
            .unwrap()
            .unwrap()
            .location
            .as_deref(),
        Some(root.join("B/y.m4a").as_path())
    );
    m.refresh(&t.persistent_id).unwrap();
    m.delete_track(&t.persistent_id).unwrap();
    assert!(m.track(&t.persistent_id).unwrap().is_none());
    // 無い track の操作は Music エラー
    assert!(matches!(m.refresh(&t.persistent_id), Err(Error::Music(_))));
}

#[test]
fn folders_and_playlists() {
    let (d, m, _) = setup();
    let root = d.path().join("root");
    let t = m.add(&root.join("A/x.m4a")).unwrap();
    let f = m.create_folder("spindle-setup-n").unwrap();
    assert_eq!(m.folders_named("SPINDLE-SETUP-N").unwrap().len(), 1);
    m.rename_playlist(&f.persistent_id, "spindle").unwrap();
    assert_eq!(m.folder(&f.persistent_id).unwrap().unwrap().name, "spindle");
    let p = m.create_playlist(&f.persistent_id, ".tmp-1").unwrap();
    m.set_playlist_tracks(&p.persistent_id, std::slice::from_ref(&t.persistent_id))
        .unwrap();
    m.rename_playlist(&p.persistent_id, "Favs").unwrap();
    let inside = m.playlists_in(&f.persistent_id).unwrap();
    assert_eq!(inside.len(), 1);
    assert_eq!(inside[0].name, "Favs");
    assert_eq!(m.playlists()[1].tracks, vec![t.persistent_id.clone()]);
    m.delete_playlist(&p.persistent_id).unwrap();
    assert!(m.playlists_in(&f.persistent_id).unwrap().is_empty());
}

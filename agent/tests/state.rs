use std::collections::BTreeMap;

use spindle_agent::state::*;

fn sample() -> State {
    let mut s = State {
        v: STATE_VERSION,
        server: Some(ServerInfo {
            url: "https://music.example".into(),
            insecure_http: false,
            device_uuid: "u-1".into(),
            device_name: "iPhone".into(),
        }),
        setup: Some(Setup {
            phase: SetupPhase::Done,
            nonce: "n".repeat(32),
            folder_pid: Some("F1".into()),
        }),
        tracks: BTreeMap::new(),
        playlists: BTreeMap::new(),
        pending_ops: vec![],
        pending_batches: vec![],
        plan_id: Some(7),
        needs_report: true,
    };
    s.tracks.insert(
        3,
        TrackEntry {
            persistent_id: "P3".into(),
            token: "t".into(),
            path: "J-Pop/a.m4a".into(),
            size: 10,
            sha256: "s".into(),
            inode: 5,
            mtime_ns: 6,
        },
    );
    s
}

#[test]
fn missing_file_loads_default() {
    let d = tempfile::tempdir().unwrap();
    let s = StateFile::new(d.path()).load().unwrap();
    assert_eq!(s, State::default());
    assert_eq!(s.v, STATE_VERSION);
}

#[test]
fn save_then_load_round_trips_and_leaves_no_tmp() {
    let d = tempfile::tempdir().unwrap();
    let f = StateFile::new(d.path());
    f.save(&sample()).unwrap();
    assert_eq!(f.load().unwrap(), sample());
    let names: Vec<_> = std::fs::read_dir(d.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, vec!["state.json"]);
}

#[test]
fn unknown_version_is_refused() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("state.json"), r#"{"v":99}"#).unwrap();
    assert!(StateFile::new(d.path()).load().is_err());
}

#[test]
fn digest_changes_with_members() {
    let m = BatchMember {
        op_id: "o".into(),
        op: MemberOp::Move,
        track_id: 1,
        persistent_id: "P".into(),
        from: "a.m4a".into(),
        staging: ".moving/b-o".into(),
        new: None,
        to: "b.m4a".into(),
        token: "t".into(),
        size: 1,
        sha256: "s".into(),
    };
    let mut m2 = m.clone();
    m2.to = "c.m4a".into();
    assert_ne!(member_digest(&[m]).unwrap(), member_digest(&[m2]).unwrap());
}

#[test]
fn phases_are_ordered() {
    assert!(BatchPhase::Sealed < BatchPhase::Prepared);
    assert!(BatchPhase::Vacating < BatchPhase::Vacated);
    assert!(BatchPhase::Vacated < BatchPhase::Placing);
    assert!(SetupPhase::Started < SetupPhase::Marker);
    assert!(SetupPhase::FolderRenamed < SetupPhase::Done);
}

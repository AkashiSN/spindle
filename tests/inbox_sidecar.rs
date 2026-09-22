//! サイドカー `spindle-inbox.json`（SPEC §7.7 / §7.8、D-70）。ダウンローダがタグに載らない情報
//! （category、ファイルごとの判定・メッセージ・URL）を Inbox へ渡す入れ物。読み書きと merge

use spindle::domain::relpath::RelPath;
use spindle::fsroot::RootDir;
use spindle::import::sidecar::{FileEntry, Sidecar, SidecarError, SIDECAR_NAME};

fn root() -> (tempfile::TempDir, RootDir) {
    let dir = tempfile::tempdir().unwrap();
    let root = RootDir::open(dir.path()).unwrap();
    (dir, root)
}

fn entry(verdict: &str) -> FileEntry {
    FileEntry {
        source: "youtube".into(),
        url: Some("https://www.youtube.com/watch?v=abc".into()),
        channel: Some("CH".into()),
        verdict: verdict.into(),
        message: (verdict != "ok").then(|| "見つからない".to_owned()),
        subscription_id: None,
        position: None,
    }
}

#[test]
fn sidecar_round_trips_and_rejects_other_versions() {
    let mut s = Sidecar::default();
    s.category = Some("Cat".into());
    s.files.insert("a.opus".into(), entry("ok"));
    let bytes = s.to_json();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.contains("\"version\": 1"), "{text}");
    let back = Sidecar::parse(&bytes).unwrap();
    assert_eq!(back.category.as_deref(), Some("Cat"));
    assert_eq!(back.files["a.opus"].verdict, "ok");
    assert_eq!(back.files["a.opus"].message, None);

    assert!(matches!(
        Sidecar::parse(br#"{"version": 2, "files": {}}"#),
        Err(SidecarError::Version(2))
    ));
    assert!(matches!(
        Sidecar::parse(b"{not json"),
        Err(SidecarError::Json(_))
    ));
    // 未知のフィールドは無視する（前方互換）
    let s =
        Sidecar::parse(br#"{"version": 1, "category": null, "files": {}, "extra": 1}"#).unwrap();
    assert!(s.files.is_empty());
}

#[test]
fn read_returns_none_when_absent_and_error_when_broken() {
    let (_d, root) = root();
    let dir = RelPath::parse("youtube/A/B").unwrap();
    assert!(Sidecar::read(&root, &dir).unwrap().is_none());
    root.create_dir_all(&dir).unwrap();
    assert!(Sidecar::read(&root, &dir).unwrap().is_none());
    std::fs::write(root.path().join("youtube/A/B").join(SIDECAR_NAME), b"{").unwrap();
    assert!(matches!(
        Sidecar::read(&root, &dir),
        Err(SidecarError::Json(_))
    ));
}

#[test]
fn upsert_merges_entries_and_replaces_atomically() {
    let (_d, root) = root();
    let dir = RelPath::parse("youtube/A/B").unwrap();
    root.create_dir_all(&dir).unwrap();
    Sidecar::upsert(&root, &dir, Some("Cat"), "1.opus", entry("ok")).unwrap();
    // 2 件目: 既存の項は残り、category は最後に書いたものが勝つ。None なら据え置き
    Sidecar::upsert(&root, &dir, None, "2.opus", entry("unmatched")).unwrap();
    let s = Sidecar::read(&root, &dir).unwrap().unwrap();
    assert_eq!(s.category.as_deref(), Some("Cat"));
    assert_eq!(s.files.len(), 2);
    assert_eq!(s.files["2.opus"].message.as_deref(), Some("見つからない"));
    Sidecar::upsert(&root, &dir, Some("Other"), "1.opus", entry("ok")).unwrap();
    let s = Sidecar::read(&root, &dir).unwrap().unwrap();
    assert_eq!(s.category.as_deref(), Some("Other"));
    assert_eq!(s.files.len(), 2);
    // tmp を残さない
    let names: Vec<String> = std::fs::read_dir(root.path().join("youtube/A/B"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec![SIDECAR_NAME.to_owned()], "{names:?}");
}

#[test]
fn upsert_refuses_to_overwrite_a_broken_sidecar() {
    // 壊れたものを黙って上書きすると人が書いた内容を失う。Err で止める
    let (_d, root) = root();
    let dir = RelPath::parse("x").unwrap();
    root.create_dir_all(&dir).unwrap();
    std::fs::write(root.path().join("x").join(SIDECAR_NAME), b"{").unwrap();
    assert!(matches!(
        Sidecar::upsert(&root, &dir, None, "1.opus", entry("ok")),
        Err(SidecarError::Json(_))
    ));
}

// ---------------------------------------------------------------- CD の吸い出しの記録（P2-5、D-67 追記）

mod common;

use spindle::import::sidecar::RipEntryError;

#[test]
fn rip_entry_round_trips_with_stable_json_keys() {
    let mut s = Sidecar::default();
    s.category = Some("Cat".into());
    s.rip = Some(common::rip_entry(&["01.flac", "02.flac"], &[true, false]));
    let bytes = s.to_json();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    // 形は Inbox に置かれたまま版をまたぎ得るので、キーと列挙の綴りを固定する
    let rip = &v["rip"];
    assert_eq!(rip["files"], serde_json::json!(["01.flac", "02.flac"]));
    assert_eq!(rip["log"], "rip.log");
    assert_eq!(rip["report"]["offset_source"], "table");
    assert_eq!(rip["report"]["read_offset"], 6);
    assert_eq!(rip["report"]["ctdb"]["outcome"], "mismatch");
    assert_eq!(rip["report"]["ctdb"]["tracks"][1]["matched"], false);
    assert_eq!(rip["report"]["crcs"][1]["ctdb"], 101);
    assert!(rip["report"]["accuraterip"].is_null());
    assert_eq!(rip["metadata"]["source"], "manual");
    let back = Sidecar::parse(&bytes).unwrap();
    assert_eq!((&back.category, &back.rip), (&s.category, &s.rip));
    assert!(back.rip.unwrap().check().is_ok());

    // rip の無いサイドカー（ダウンローダ）はキーを出さず、読んでも None
    let text = String::from_utf8(Sidecar::default().to_json()).unwrap();
    assert!(!text.contains("rip"), "{text}");
    let old = Sidecar::parse(br#"{"version": 1, "category": null, "files": {}}"#).unwrap();
    assert!(old.rip.is_none());
}

#[test]
fn rip_entry_check_rejects_broken_shapes() {
    let ok = common::rip_entry(&["01.flac", "02.flac", "03.flac"], &[true, true, true]);
    assert_eq!(ok.check().unwrap().audio_tracks().count(), 3);
    assert_eq!(ok.index_of("02.FLAC"), Some(1)); // 大小文字は同じ名前（ZFS の insensitive）
    assert_eq!(ok.index_of("04.flac"), None);

    let mut e = ok.clone();
    e.report.crcs.pop();
    assert_eq!(
        e.check(),
        Err(RipEntryError::Count {
            what: "crcs",
            expected: 3,
            got: 2
        })
    );
    let mut e = ok.clone();
    e.files.pop();
    assert!(matches!(
        e.check(),
        Err(RipEntryError::Count { what: "files", .. })
    ));
    let mut e = ok.clone();
    let tracks = &mut e.report.ctdb.as_mut().unwrap().tracks;
    tracks.push(tracks[0].clone());
    assert!(matches!(
        e.check(),
        Err(RipEntryError::Count {
            what: "ctdb.tracks",
            ..
        })
    ));
    let mut e = ok.clone();
    e.metadata.tracks.pop();
    assert!(matches!(
        e.check(),
        Err(RipEntryError::Count {
            what: "metadata.tracks",
            ..
        })
    ));
    let mut e = ok.clone();
    e.files[2] = "01.FLAC".into();
    assert_eq!(
        e.check(),
        Err(RipEntryError::DuplicateFile("01.FLAC".into()))
    );
    let mut e = ok.clone();
    e.files[0] = "sub/01.flac".into();
    assert!(matches!(e.check(), Err(RipEntryError::BadFileName(_))));
    let mut e = ok.clone();
    e.log = "01.flac".into();
    assert!(matches!(e.check(), Err(RipEntryError::DuplicateFile(_))));
    let mut e = ok;
    e.toc = "garbage".into();
    assert!(matches!(e.check(), Err(RipEntryError::Toc(_))));
}

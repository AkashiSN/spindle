//! 端末側の正本 `.spindle/manifest.json` の形式（SPEC §7.11 Android「端末側の正本」）

use spindle::device::ondevice::*;
use spindle::domain::device::{EntryKind, OpKind, PlaylistOpKind};

fn item(id: i64, path: &str) -> ManifestItem {
    ManifestItem {
        track_id: id,
        path: path.into(),
        token: format!("t{id}"),
        size: 10,
        sha256: format!("h{id}"),
        rel_path: format!("Lib/{id}.flac"),
        source: "master".into(),
    }
}

fn manifest(items: Vec<ManifestItem>) -> DeviceManifest {
    let mut m = DeviceManifest::new("u1", "emulated");
    m.items = items;
    m
}

/// 検査を通さずに書いた manifest（壊れた正本を parse が拒むかの試験用）
fn raw(m: &DeviceManifest) -> Vec<u8> {
    serde_json::to_vec(m).unwrap()
}

#[test]
fn round_trip() {
    let mut m = manifest(vec![item(2, "b.opus"), item(1, "a.opus")]);
    m.playlists.push(ManifestPlaylist {
        playlist_id: 5,
        path: "Playlists/通勤.m3u8".into(),
        token: "p".into(),
    });
    let bytes = render(&m).unwrap();
    assert_eq!(parse(&bytes).unwrap(), m);
}

#[test]
fn unknown_format_is_refused() {
    let err =
        parse(br#"{"format":2,"device_uuid":"u","volume":"emulated","items":[],"playlists":[]}"#)
            .unwrap_err();
    assert_eq!(err, ManifestError::UnknownFormat(2));
}

#[test]
fn unknown_fields_are_ignored() {
    let m = parse(br#"{"format":1,"device_uuid":"u","volume":"emulated","items":[],"playlists":[],"extra":1}"#)
        .unwrap();
    assert_eq!(m.device_uuid, "u");
}

#[test]
fn duplicates_and_bad_paths_are_refused() {
    let dup_id = manifest(vec![item(1, "a.opus"), item(1, "b.opus")]);
    assert_eq!(
        parse(&raw(&dup_id)).unwrap_err(),
        ManifestError::DuplicateTrack(1)
    );
    // casefold + NFD で同じパス
    let dup_path = manifest(vec![item(1, "A.opus"), item(2, "a.opus")]);
    assert!(matches!(
        parse(&raw(&dup_path)).unwrap_err(),
        ManifestError::DuplicatePath(_)
    ));
    for bad in [
        "../x.opus",
        "/abs.opus",
        ".spindle/manifest.json",
        ".SPINDLE/x",
    ] {
        let m = manifest(vec![item(1, bad)]);
        assert!(
            matches!(parse(&raw(&m)).unwrap_err(), ManifestError::BadPath(_)),
            "{bad}"
        );
    }
}

#[test]
fn too_many_entries_are_refused() {
    let items = (0..=MAX_ENTRIES as i64)
        .map(|i| item(i, &format!("{i}.opus")))
        .collect();
    let m = manifest(items);
    assert!(matches!(
        parse(&raw(&m)).unwrap_err(),
        ManifestError::TooMany(_)
    ));
}

#[test]
fn book_round_trip_sorts_by_id() {
    let m = manifest(vec![item(2, "b.opus"), item(1, "a.opus")]);
    let book = Book::from(m);
    let back = book.manifest();
    assert_eq!(
        back.items.iter().map(|i| i.track_id).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(book.device_items()[0].dest_path, "a.opus");
    assert!(book
        .path_keys()
        .contains(&spindle::domain::relpath::canonical_key("B.opus")));
}

#[test]
fn reserved_detection() {
    assert!(is_reserved(".spindle/journal"));
    assert!(is_reserved(".Spindle/x"));
    assert!(!is_reserved("Music/.spindle"));
}

#[test]
fn op_kinds_serialize_as_snake_case() {
    assert_eq!(
        serde_json::to_string(&OpKind::UpdateMove).unwrap(),
        "\"update_move\""
    );
    assert_eq!(
        serde_json::to_string(&PlaylistOpKind::Add).unwrap(),
        "\"add\""
    );
    assert_eq!(
        serde_json::to_string(&EntryKind::Playlist).unwrap(),
        "\"playlist\""
    );
    let k: OpKind = serde_json::from_str("\"delete\"").unwrap();
    assert_eq!(k, OpKind::Delete);
}

#[test]
fn render_refuses_what_parse_would_refuse() {
    let dup_path = manifest(vec![item(1, "A.opus"), item(2, "a.opus")]);
    assert!(matches!(
        render(&dup_path).unwrap_err(),
        ManifestError::DuplicatePath(_)
    ));
    let dup_id = manifest(vec![item(1, "a.opus"), item(1, "b.opus")]);
    assert_eq!(
        render(&dup_id).unwrap_err(),
        ManifestError::DuplicateTrack(1)
    );
    let reserved = manifest(vec![item(1, ".spindle/x.opus")]);
    assert!(matches!(
        render(&reserved).unwrap_err(),
        ManifestError::BadPath(_)
    ));
    // 曲とプレイリストは同じ名前空間
    let mut shared = manifest(vec![item(1, "x.m3u8")]);
    shared.playlists.push(ManifestPlaylist {
        playlist_id: 5,
        path: "X.m3u8".into(),
        token: "p".into(),
    });
    assert!(matches!(
        render(&shared).unwrap_err(),
        ManifestError::DuplicatePath(_)
    ));
}

fn item_json(extra: &str) -> String {
    format!(
        r#"{{"format":1,"device_uuid":"u1","volume":"emulated","items":[{{"track_id":1,"path":"A/a.opus","token":"t","size":4,"sha256":"s"{extra}}}],"playlists":[]}}"#
    )
}

#[test]
fn manifest_items_carry_rel_path_and_source() {
    let m = parse(item_json(r#","rel_path":"A/a.flac","source":"opus""#).as_bytes()).unwrap();
    assert_eq!(m.items[0].rel_path, "A/a.flac");
    assert_eq!(m.items[0].source, "opus");
    let again = parse(&render(&m).unwrap()).unwrap();
    assert_eq!(again, m);
}

#[test]
fn manifest_without_rel_path_or_source_is_unreadable() {
    assert!(matches!(
        parse(item_json("").as_bytes()),
        Err(ManifestError::Json(_))
    ));
}

#[test]
fn manifest_rejects_unknown_source_and_bad_rel_path() {
    let bad_source = item_json(r#","rel_path":"A/a.flac","source":"mp3""#);
    assert!(matches!(
        parse(bad_source.as_bytes()),
        Err(ManifestError::BadSource(_))
    ));
    let bad_rel = item_json(r#","rel_path":"../a.flac","source":"master""#);
    assert!(matches!(
        parse(bad_rel.as_bytes()),
        Err(ManifestError::BadRelPath(_))
    ));
}

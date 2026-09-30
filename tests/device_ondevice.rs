//! 端末側の正本 `.spindle/manifest.json` の形式（仕様 ⑤「端末側の正本」）

use spindle::device::ondevice::*;
use spindle::domain::device::{EntryKind, OpKind, PlaylistOpKind};

fn item(id: i64, path: &str) -> ManifestItem {
    ManifestItem {
        track_id: id,
        path: path.into(),
        token: format!("t{id}"),
        size: 10,
        sha256: format!("h{id}"),
    }
}

fn manifest(items: Vec<ManifestItem>) -> DeviceManifest {
    let mut m = DeviceManifest::new("u1", "emulated");
    m.items = items;
    m
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
        parse(&render(&dup_id).unwrap()).unwrap_err(),
        ManifestError::DuplicateTrack(1)
    );
    // casefold + NFD で同じパス
    let dup_path = manifest(vec![item(1, "A.opus"), item(2, "a.opus")]);
    assert!(matches!(
        parse(&render(&dup_path).unwrap()).unwrap_err(),
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
            matches!(
                parse(&render(&m).unwrap()).unwrap_err(),
                ManifestError::BadPath(_)
            ),
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
        parse(&render(&m).unwrap()).unwrap_err(),
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

mod device_support;

use device_support::FakeFs;
use spindle::device::adopt::{remap, rewrite, LibraryPlaylist, LibraryTrack};
use spindle::device::ondevice::{DeviceManifest, ManifestItem, ManifestPlaylist};
use spindle::device::recover::STALE_TOKEN;
use spindle::device::store;
use spindle::domain::device::ADOPTED_TOKEN;

fn item(id: i64, path: &str, rel: &str, token: &str) -> ManifestItem {
    ManifestItem {
        track_id: id,
        path: path.into(),
        token: token.into(),
        size: 4,
        sha256: "s".into(),
        rel_path: rel.into(),
        source: "opus".into(),
    }
}

fn manifest(items: Vec<ManifestItem>, playlists: Vec<ManifestPlaylist>) -> DeviceManifest {
    DeviceManifest {
        items,
        playlists,
        ..DeviceManifest::new("u-old", "emulated")
    }
}

fn t(id: i64, rel: &str) -> LibraryTrack {
    LibraryTrack {
        track_id: id,
        rel_path: rel.into(),
    }
}
fn p(id: i64, name: &str) -> LibraryPlaylist {
    LibraryPlaylist {
        playlist_id: id,
        name: name.into(),
    }
}

#[test]
fn remaps_track_ids_by_rel_path_and_marks_adopted() {
    let m = manifest(vec![item(7, "A/a.opus", "A/a.flac", "old")], vec![]);
    let r = remap(&m, &[t(1, "A/a.flac")], &[]);
    assert_eq!(
        r.manifest.items,
        vec![item(1, "A/a.opus", "A/a.flac", ADOPTED_TOKEN)]
    );
    assert_eq!((r.tracks_adopted, r.tracks_dropped), (1, 0));
    assert_eq!(r.manifest.device_uuid, "u-old");
}

#[test]
fn rel_path_match_ignores_case_and_normalization() {
    let m = manifest(vec![item(7, "A/a.opus", "A/A.flac", "old")], vec![]);
    let r = remap(&m, &[t(1, "a/a.flac")], &[]);
    assert_eq!(r.tracks_adopted, 1);
}

#[test]
fn unmatched_items_are_dropped() {
    let m = manifest(vec![item(7, "A/a.opus", "A/gone.flac", "old")], vec![]);
    let r = remap(&m, &[t(1, "A/a.flac")], &[]);
    assert!(r.manifest.items.is_empty());
    assert_eq!(r.tracks_dropped, 1);
}

#[test]
fn two_items_for_one_track_are_both_dropped() {
    let m = manifest(
        vec![
            item(7, "A/a.opus", "A/a.flac", "x"),
            item(8, "A/a.m4a", "A/a.flac", "y"),
        ],
        vec![],
    );
    let r = remap(&m, &[t(1, "A/a.flac")], &[]);
    assert!(r.manifest.items.is_empty());
    assert_eq!(r.tracks_dropped, 2);
}

#[test]
fn stale_items_stay_stale() {
    let m = manifest(vec![item(7, "A/a.opus", "A/a.flac", STALE_TOKEN)], vec![]);
    let r = remap(&m, &[t(1, "A/a.flac")], &[]);
    assert_eq!(r.manifest.items[0].token, STALE_TOKEN);
}

#[test]
fn playlists_are_remapped_by_name_and_made_stale() {
    let pl = |id, path: &str| ManifestPlaylist {
        playlist_id: id,
        path: path.into(),
        token: "t".into(),
    };
    let m = manifest(
        vec![],
        vec![
            pl(5, "Playlists/Drive.m3u8"),
            pl(6, "Playlists/Gone.m3u8"),
            pl(9, "x.m3u8"),
        ],
    );
    let r = remap(&m, &[], &[p(2, "drive"), p(3, "Other")]);
    assert_eq!(
        r.manifest.playlists,
        vec![ManifestPlaylist {
            playlist_id: 2,
            path: "Playlists/Drive.m3u8".into(),
            token: STALE_TOKEN.into(),
        }]
    );
    assert_eq!(
        (r.playlists_adopted.clone(), r.playlists_dropped),
        (vec![2], 2)
    );
}

#[test]
fn playlists_with_ambiguous_names_are_dropped() {
    let pl = ManifestPlaylist {
        playlist_id: 5,
        path: "Playlists/Drive.m3u8".into(),
        token: "t".into(),
    };
    let r = remap(
        &manifest(vec![], vec![pl]),
        &[],
        &[p(2, "Drive"), p(3, "DRIVE")],
    );
    assert!(r.manifest.playlists.is_empty());
}

#[test]
fn remap_is_idempotent() {
    let m = manifest(
        vec![
            item(7, "A/a.opus", "A/a.flac", "old"),
            item(8, "B/b.opus", "B/gone.flac", "x"),
        ],
        vec![],
    );
    let lib = [t(1, "A/a.flac")];
    let once = remap(&m, &lib, &[]);
    let twice = remap(&once.manifest, &lib, &[]);
    assert_eq!(once.manifest, twice.manifest);
}

#[tokio::test]
async fn rewrite_recovers_then_remaps_and_is_idempotent() {
    let fs = FakeFs::new(1 << 30);
    store::initialize(&fs, "u-old", "emulated").await.unwrap();
    fs.set("A/a.opus", b"AAAA");
    fs.set("A/b.opus", b"BBBBBB"); // サイズが manifest と違う
    let mut m = store::read_manifest(&fs).await.unwrap().unwrap();
    m.items = vec![
        item(7, "A/a.opus", "A/a.flac", "old"),
        item(8, "A/b.opus", "A/b.flac", "old"),
    ];
    store::write_manifest(&fs, &m).await.unwrap();
    let lib = [t(1, "A/a.flac"), t(2, "A/b.flac")];

    let r = rewrite(&fs, &m, &lib, &[]).await.unwrap();
    let on_device = store::read_manifest(&fs).await.unwrap().unwrap();
    assert_eq!(on_device, r.manifest);
    let tok = |id| {
        on_device
            .items
            .iter()
            .find(|i| i.track_id == id)
            .unwrap()
            .token
            .clone()
    };
    assert_eq!(
        (tok(1), tok(2)),
        (ADOPTED_TOKEN.to_owned(), STALE_TOKEN.to_owned())
    );

    // やり直し（行の作成の前に落ちた想定）でも同じ結果
    let again = rewrite(&fs, &on_device, &lib, &[]).await.unwrap();
    assert_eq!(again.manifest, r.manifest);
}

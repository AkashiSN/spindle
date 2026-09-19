//! 確定したディスクのメタデータ（`cd::metadata::DiscMetadata`。D-65 / D-67、P2-8）。web の
//! `DiscMetadata` と同じ形を受け、検証し、タグに写す。web の `albumTags` / `trackTags` と同じ写像

use spindle::cd::metadata::{
    DiscMetadata, DiscTrackMetadata, MetadataError, MetadataSource, TrackMbIds,
};
use spindle::cd::toc::Toc;

const NEVERMIND: &str =
    "0:22593:41700:58133:71920:91198:104468:115188:131988:143758:159678:174415:191880";

fn toc() -> Toc {
    Toc::parse(NEVERMIND).unwrap()
}

fn meta() -> DiscMetadata {
    let tracks = (1..=12u8)
        .map(|n| DiscTrackMetadata {
            number: n,
            title: format!("T{n}"),
            artist: String::new(),
            mb: None,
        })
        .collect();
    DiscMetadata {
        source: MetadataSource::Manual,
        release_id: None,
        release_group_id: None,
        album: "Nevermind".into(),
        album_artist: "Nirvana".into(),
        date: Some("1991-09-24".into()),
        label: Some("DGC".into()),
        catalog_number: Some("DGCD-24425".into()),
        barcode: Some("720642442524".into()),
        disc_no: 1,
        disc_count: 1,
        category: None,
        tracks,
    }
}

#[test]
fn valid_metadata_passes() {
    assert_eq!(meta().validate(&toc()), Ok(()));
}

#[test]
fn rejects_empty_album_artist_title_and_bad_numbers() {
    let mut m = meta();
    m.album = " ".into();
    assert_eq!(m.validate(&toc()), Err(MetadataError::EmptyAlbum));
    let mut m = meta();
    m.album_artist.clear();
    assert_eq!(m.validate(&toc()), Err(MetadataError::EmptyAlbumArtist));
    let mut m = meta();
    m.tracks[3].title = String::new();
    assert_eq!(
        m.validate(&toc()),
        Err(MetadataError::EmptyTitle { number: 4 })
    );
    let mut m = meta();
    m.tracks.pop();
    assert_eq!(
        m.validate(&toc()),
        Err(MetadataError::TrackCount {
            expected: 12,
            got: 11
        })
    );
    let mut m = meta();
    m.tracks[1].number = 5;
    assert_eq!(
        m.validate(&toc()),
        Err(MetadataError::TrackNumber {
            index: 1,
            expected: 2,
            got: 5
        })
    );
    let mut m = meta();
    m.disc_no = 2;
    m.disc_count = 1;
    assert_eq!(
        m.validate(&toc()),
        Err(MetadataError::BadDisc {
            disc_no: 2,
            disc_count: 1
        })
    );
    let mut m = meta();
    m.disc_no = 0;
    assert!(matches!(
        m.validate(&toc()),
        Err(MetadataError::BadDisc { .. })
    ));
    let mut m = meta();
    m.category = Some("  ".into());
    assert_eq!(m.validate(&toc()), Err(MetadataError::EmptyCategory));
}

#[test]
fn date_forms() {
    for ok in ["1991", "1991-09", "1991-09-24"] {
        let mut m = meta();
        m.date = Some(ok.into());
        assert_eq!(m.validate(&toc()), Ok(()), "{ok}");
    }
    for bad in ["91", "1991/09", "1991-13", "1991-09-32", "abcd", "1991-00"] {
        let mut m = meta();
        m.date = Some(bad.into());
        assert_eq!(
            m.validate(&toc()),
            Err(MetadataError::BadDate(bad.into())),
            "{bad}"
        );
    }
    let mut m = meta();
    m.date = None;
    assert_eq!(m.validate(&toc()), Ok(()));
    assert_eq!(meta().year().as_deref(), Some("1991"));
    m.date = Some("abcd".into());
    assert_eq!(m.year(), None);
}

#[test]
fn tags_follow_the_web_mapping() {
    let mut m = meta();
    m.source = MetadataSource::Musicbrainz;
    m.release_id = Some("r-1".into());
    m.release_group_id = Some("rg-1".into());
    m.tracks[0].artist = "Guest".into();
    m.tracks[0].mb = Some(TrackMbIds {
        recording_id: "rec-1".into(),
        track_id: "trk-1".into(),
        isrcs: vec!["USGF19110101".into(), "JPXX00000001".into()],
    });
    let t = toc();
    let tags = m.tags_for(&t, 0);
    let expect: Vec<(&str, &str)> = vec![
        ("ALBUM", "Nevermind"),
        ("ALBUMARTIST", "Nirvana"),
        ("DATE", "1991-09-24"),
        ("LABEL", "DGC"),
        ("CATALOGNUMBER", "DGCD-24425"),
        ("BARCODE", "720642442524"),
        ("DISCNUMBER", "1"),
        ("DISCTOTAL", "1"),
        ("MUSICBRAINZ_ALBUMID", "r-1"),
        ("MUSICBRAINZ_RELEASEGROUPID", "rg-1"),
        ("TRACKNUMBER", "1"),
        ("TITLE", "T1"),
        ("ARTIST", "Guest"),
        ("MUSICBRAINZ_TRACKID", "rec-1"),
        ("MUSICBRAINZ_RELEASETRACKID", "trk-1"),
        ("ISRC", "USGF19110101"),
        ("ISRC", "JPXX00000001"),
        ("TRACKTOTAL", "12"),
        ("MUSICBRAINZ_DISCID", "y6Br7t4P.bldLe_6Im2d9Z42IU4-"),
    ];
    let got: Vec<(&str, &str)> = tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(got, expect);
    // 空のアーティストはアルバムアーティスト、None / 空の項目は出ない、category はタグにしない
    let mut m2 = meta();
    m2.category = Some("Rock".into());
    m2.label = None;
    m2.catalog_number = Some("  ".into());
    let tags = m2.tags_for(&t, 1);
    assert!(tags.contains(&("ARTIST".to_owned(), "Nirvana".to_owned())));
    assert!(tags.contains(&("TRACKNUMBER".to_owned(), "2".to_owned())));
    assert!(!tags.iter().any(|(k, _)| k == "LABEL"
        || k == "CATALOGNUMBER"
        || k == "CATEGORY"
        || k.starts_with("MUSICBRAINZ_ALBUM")
        || k.starts_with("MUSICBRAINZ_RELEASE")
        || k == "ISRC"));
    assert_eq!(m2.track_artist(1), "Nirvana");
    assert_eq!(m.track_artist(0), "Guest");
}

#[test]
fn json_round_trip_matches_web_shape() {
    let j = serde_json::json!({
        "source": "musicbrainz", "release_id": "r", "release_group_id": null,
        "album": "A", "album_artist": "B", "date": null, "label": null, "catalog_number": null, "barcode": null,
        "disc_no": 1, "disc_count": 2, "category": "J-Pop",
        "tracks": [{ "number": 1, "title": "x", "artist": "", "mb": { "recording_id": "a", "track_id": "b", "isrcs": [] } }]
    });
    let m: DiscMetadata = serde_json::from_value(j.clone()).unwrap();
    assert_eq!(m.source, MetadataSource::Musicbrainz);
    assert_eq!(m.category.as_deref(), Some("J-Pop"));
    assert_eq!(m.tracks[0].mb.as_ref().unwrap().recording_id, "a");
    let back = serde_json::to_value(&m).unwrap();
    assert_eq!(back["tracks"][0]["mb"]["recording_id"], "a");
    assert_eq!(back["source"], "musicbrainz");
    // 省略可能な項目が無くても読める（手入力の最小形）
    let j = serde_json::json!({
        "source": "manual", "album": "A", "album_artist": "B", "disc_no": 1, "disc_count": 1,
        "tracks": [{ "number": 1, "title": "x" }]
    });
    let m: DiscMetadata = serde_json::from_value(j).unwrap();
    assert_eq!(m.tracks[0].artist, "");
    assert_eq!(m.tracks[0].mb, None);
    assert_eq!(m.category, None);
}

//! Inbox の下書きと承認の検証（`import::inbox::{InboxDraft, proposal, warnings}`。D-68、P2-10）

use spindle::db::inbox::FileRow;
use spindle::import::inbox::{
    merge_saved, number_missing, proposal, warnings, DraftError, DraftTrack, InboxDraft,
};

fn file(rel: &str, tags: &[(&str, &str)]) -> FileRow {
    FileRow {
        rel_path: rel.to_owned(),
        inode: 1,
        size: 1,
        mtime_ns: 0,
        ctime_ns: 0,
        codec: "flac".into(),
        lossless: true,
        sample_rate: Some(44100),
        bit_depth: Some(16),
        channels: Some(2),
        duration_ms: Some(1000),
        tags: tags
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

fn draft() -> InboxDraft {
    InboxDraft {
        category: None,
        albumartist: "Artist".into(),
        album: "Album".into(),
        date: Some("2024".into()),
        album_gain: false,
        tracks: vec![
            DraftTrack {
                rel_path: "A/01.flac".into(),
                disc_no: 1,
                track_no: 1,
                title: "One".into(),
                artist: String::new(),
            },
            DraftTrack {
                rel_path: "A/02.flac".into(),
                disc_no: 1,
                track_no: 2,
                title: "Two".into(),
                artist: "Guest".into(),
            },
        ],
    }
}

fn files() -> Vec<String> {
    vec!["A/01.flac".into(), "A/02.flac".into()]
}

#[test]
fn validate_accepts_complete_draft_and_reports_each_defect() {
    assert_eq!(draft().validate(&files()), Ok(()));
    assert_eq!(draft().disc_count(), 1);
    assert_eq!(draft().track_artist(0), "Artist");
    assert_eq!(draft().track_artist(1), "Guest");

    let mut d = draft();
    d.album = "  ".into();
    assert_eq!(d.validate(&files()), Err(DraftError::EmptyAlbum));
    let mut d = draft();
    d.albumartist.clear();
    assert_eq!(d.validate(&files()), Err(DraftError::EmptyAlbumArtist));
    let mut d = draft();
    d.tracks[1].title = " ".into();
    assert_eq!(
        d.validate(&files()),
        Err(DraftError::EmptyTitle {
            rel_path: "A/02.flac".into()
        })
    );
    let mut d = draft();
    d.tracks[0].track_no = 0;
    assert_eq!(
        d.validate(&files()),
        Err(DraftError::BadNumber {
            rel_path: "A/01.flac".into()
        })
    );
    let mut d = draft();
    d.tracks[1].track_no = 1;
    assert_eq!(
        d.validate(&files()),
        Err(DraftError::DuplicateNumber {
            disc_no: 1,
            track_no: 1
        })
    );
    // 別ディスクなら同じ番号でよい
    let mut d = draft();
    d.tracks[1].track_no = 1;
    d.tracks[1].disc_no = 2;
    assert_eq!(d.validate(&files()), Ok(()));
    assert_eq!(d.disc_count(), 2);
    let mut d = draft();
    d.tracks[1].rel_path = "A/03.flac".into();
    assert_eq!(
        d.validate(&files()),
        Err(DraftError::UnknownFile("A/03.flac".into()))
    );
    let mut d = draft();
    d.tracks.pop();
    assert_eq!(
        d.validate(&files()),
        Err(DraftError::MissingFile("A/02.flac".into()))
    );
    // 全ファイルを含んだ上で同じファイルをもう 1 行（別の番号で）足しても通さない
    let mut d = draft();
    let mut dup = d.tracks[0].clone();
    dup.rel_path = "a/01.FLAC".into();
    dup.track_no = 3;
    d.tracks.push(dup);
    assert_eq!(
        d.validate(&files()),
        Err(DraftError::DuplicateFile("a/01.FLAC".into()))
    );
    let mut d = draft();
    d.date = Some("2024/01".into());
    assert_eq!(
        d.validate(&files()),
        Err(DraftError::BadDate("2024/01".into()))
    );
    let mut d = draft();
    d.category = Some(" ".into());
    assert_eq!(d.validate(&files()), Err(DraftError::EmptyCategory));
}

#[test]
fn proposal_takes_the_mode_of_tags_and_maps_genre_to_category() {
    let fs = vec![
        file(
            "A/01.flac",
            &[
                ("TITLE", "One"),
                ("ARTIST", "Artist"),
                ("ALBUM", "Album"),
                ("ALBUMARTIST", "Artist"),
                ("TRACKNUMBER", "1/12"),
                ("DATE", "2024-03-09"),
                ("GENRE", "J-Pop"),
            ],
        ),
        file(
            "A/02.flac",
            &[
                ("TITLE", "Two"),
                ("ARTIST", "Guest"),
                ("ALBUM", "Album"),
                ("ALBUMARTIST", "Artist"),
                ("TRACKNUMBER", "02"),
                ("DISCNUMBER", "1"),
                ("DATE", "2024-03-09"),
                ("GENRE", "Rock"),
            ],
        ),
        file(
            "A/03.flac",
            &[("ALBUM", "Album (Deluxe)"), ("TRACKNUMBER", "3")],
        ),
    ];
    let categories = vec![(1, "J-Pop".to_owned()), (2, "Rock".to_owned())];
    let genre_map = vec![("j-pop".to_owned(), 1), ("rock".to_owned(), 2)];
    let p = proposal(&fs, &categories, &genre_map);
    assert_eq!(p.albumartist, "Artist");
    assert_eq!(p.album, "Album");
    assert_eq!(p.date.as_deref(), Some("2024-03-09"));
    // GENRE の最頻値は同数（J-Pop と Rock が 1 ずつ）→ 先に出た方
    assert_eq!(p.category.as_deref(), Some("J-Pop"));
    assert_eq!(p.tracks.len(), 3);
    assert_eq!((p.tracks[0].disc_no, p.tracks[0].track_no), (1, 1));
    assert_eq!(p.tracks[0].title, "One");
    assert_eq!(p.tracks[0].artist, "Artist");
    assert_eq!((p.tracks[1].disc_no, p.tracks[1].track_no), (1, 2));
    assert_eq!(p.tracks[1].artist, "Guest");
    // TITLE 無しは空のまま、ARTIST 無しは空（= アルバムアーティスト）
    assert_eq!(p.tracks[2].title, "");
    assert_eq!(p.tracks[2].artist, "");
    assert_eq!(p.tracks[2].track_no, 3);
    let w = warnings(&fs, &p);
    assert_eq!(w.len(), 1);
    assert!(w[0].contains("A/03.flac"), "{w:?}");

    // ALBUMARTIST が無ければ ARTIST の最頻値、TRACKNUMBER が無ければ 0（不足として警告）
    let fs = vec![
        file(
            "B/x.flac",
            &[("TITLE", "x"), ("ARTIST", "Solo"), ("ALBUM", "B")],
        ),
        file(
            "B/y.flac",
            &[("TITLE", "y"), ("ARTIST", "Solo"), ("ALBUM", "B")],
        ),
    ];
    let p = proposal(&fs, &[], &[]);
    assert_eq!(p.albumartist, "Solo");
    assert_eq!(p.category, None);
    assert_eq!(p.tracks[0].track_no, 0);
    let w = warnings(&fs, &p);
    assert!(!w.is_empty());
    // 語彙に無い GENRE は category にしない
    let fs = vec![file("C/x.flac", &[("TITLE", "x"), ("GENRE", "Nope")])];
    assert_eq!(proposal(&fs, &[(1, "Rock".into())], &[]).category, None);
}

// ---------------------------------------------------------------- 追記の採番と下書きの merge（D-70）

#[test]
fn number_missing_assigns_from_start_in_file_order_skipping_used_numbers() {
    let mut d = InboxDraft {
        category: None,
        albumartist: "A".into(),
        album: "B".into(),
        date: None,
        album_gain: false,
        tracks: vec![
            DraftTrack {
                rel_path: "x/20260902 b.opus".into(),
                disc_no: 1,
                track_no: 0,
                title: "b".into(),
                artist: String::new(),
            },
            DraftTrack {
                rel_path: "x/20260901 a.opus".into(),
                disc_no: 1,
                track_no: 0,
                title: "a".into(),
                artist: String::new(),
            },
            DraftTrack {
                rel_path: "x/c.opus".into(),
                disc_no: 1,
                track_no: 14,
                title: "c".into(),
                artist: String::new(),
            },
        ],
    };
    // 既存 album の最大が 12 → 13 から。名前順（a, b）に振り、既に使われている 14 は飛ばす
    number_missing(&mut d, 13);
    let by_name = |n: &str| {
        d.tracks
            .iter()
            .find(|t| t.rel_path.ends_with(n))
            .unwrap()
            .track_no
    };
    assert_eq!(by_name("a.opus"), 13);
    assert_eq!(by_name("b.opus"), 15);
    assert_eq!(by_name("c.opus"), 14);
    // 順序は変えない
    assert!(d.tracks[0].rel_path.ends_with("b.opus"));
}

#[test]
fn merge_saved_keeps_corrections_for_known_files_and_adds_new_ones() {
    let saved = InboxDraft {
        category: Some("Rock".into()),
        albumartist: "Fixed".into(),
        album: "Fixed Album".into(),
        date: Some("2020".into()),
        album_gain: true,
        tracks: vec![
            DraftTrack {
                rel_path: "A/01.flac".into(),
                disc_no: 1,
                track_no: 7,
                title: "Corrected".into(),
                artist: String::new(),
            },
            DraftTrack {
                rel_path: "A/gone.flac".into(),
                disc_no: 1,
                track_no: 8,
                title: "Gone".into(),
                artist: String::new(),
            },
        ],
    };
    let mut proposed = draft();
    proposed.tracks.push(DraftTrack {
        rel_path: "A/03.flac".into(),
        disc_no: 1,
        track_no: 0,
        title: "Three".into(),
        artist: String::new(),
    });
    let merged = merge_saved(&saved, &proposed);
    // アルバム単位の補正は保存した下書き
    assert_eq!(merged.category.as_deref(), Some("Rock"));
    assert_eq!(merged.albumartist, "Fixed");
    assert_eq!(merged.album, "Fixed Album");
    assert_eq!(merged.date.as_deref(), Some("2020"));
    assert!(merged.album_gain, "album gain も保存した下書き（D-74）");
    // 既知のファイルは補正を保ち、消えたファイルは落ち、新しいファイルは提案のまま
    let names: Vec<&str> = merged.tracks.iter().map(|t| t.rel_path.as_str()).collect();
    assert_eq!(names, vec!["A/01.flac", "A/02.flac", "A/03.flac"]);
    assert_eq!(merged.tracks[0].title, "Corrected");
    assert_eq!(merged.tracks[0].track_no, 7);
    assert_eq!(merged.tracks[1].title, "Two");
    assert_eq!(merged.tracks[2].track_no, 0);
}

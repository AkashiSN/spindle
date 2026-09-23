//! Inbox の下書きと承認の検証（`import::inbox::{InboxDraft, proposal, warnings}`。D-68、P2-10）

use spindle::db::inbox::FileRow;
use spindle::import::inbox::{
    bind_rip, merge_saved, number_missing, proposal, warnings, DraftError, DraftTrack, InboxDraft,
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
        release_id: None,
        release_group_id: None,
        tracks: vec![
            DraftTrack {
                rel_path: "A/01.flac".into(),
                disc_no: 1,
                track_no: 1,
                title: "One".into(),
                artist: String::new(),
                keep_artists: None,
            },
            DraftTrack {
                rel_path: "A/02.flac".into(),
                disc_no: 1,
                track_no: 2,
                title: "Two".into(),
                artist: "Guest".into(),
                keep_artists: None,
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
        release_id: None,
        release_group_id: None,
        tracks: vec![
            DraftTrack {
                rel_path: "x/20260902 b.opus".into(),
                disc_no: 1,
                track_no: 0,
                title: "b".into(),
                artist: String::new(),
                keep_artists: None,
            },
            DraftTrack {
                rel_path: "x/20260901 a.opus".into(),
                disc_no: 1,
                track_no: 0,
                title: "a".into(),
                artist: String::new(),
                keep_artists: None,
            },
            DraftTrack {
                rel_path: "x/c.opus".into(),
                disc_no: 1,
                track_no: 14,
                title: "c".into(),
                artist: String::new(),
                keep_artists: None,
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
        release_id: None,
        release_group_id: None,
        tracks: vec![
            DraftTrack {
                rel_path: "A/01.flac".into(),
                disc_no: 1,
                track_no: 7,
                title: "Corrected".into(),
                artist: String::new(),
                keep_artists: None,
            },
            DraftTrack {
                rel_path: "A/gone.flac".into(),
                disc_no: 1,
                track_no: 8,
                title: "Gone".into(),
                artist: String::new(),
                keep_artists: None,
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
        keep_artists: None,
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

// ---------------------------------------------------------------- CD の吸い出しの記録（P2-5、D-67 追記）

mod common;

fn cd_draft(tracks: &[(&str, u32, u32)]) -> InboxDraft {
    InboxDraft {
        category: None,
        albumartist: "A".into(),
        album: "X".into(),
        date: None,
        tracks: tracks
            .iter()
            .map(|(rel, disc_no, track_no)| DraftTrack {
                rel_path: rel.to_string(),
                disc_no: *disc_no,
                track_no: *track_no,
                title: "t".into(),
                artist: String::new(),
                keep_artists: None,
            })
            .collect(),
        album_gain: true,
        release_id: None,
        release_group_id: None,
    }
}

/// 対応はファイル名で決まり、下書きの並びや番号の付け替えに左右されない
#[test]
fn bind_rip_maps_tracks_by_file_name_not_by_order_or_number() {
    let entry = common::rip_entry(&["01.flac", "02.flac", "03.flac"], &[true, true, true]);
    // 並びも番号も入れ替えた下書き（承認画面で直した）
    let d = cd_draft(&[
        ("CD/03.flac", 2, 1),
        ("CD/01.FLAC", 2, 3),
        ("CD/02.flac", 2, 2),
    ]);
    let b = bind_rip(&entry, &d).unwrap();
    assert_eq!(b.index, [2, 0, 1]);
    assert_eq!(b.disc_no, 2); // 下書きの値（直したディスク番号）
}

#[test]
fn bind_rip_rejects_records_that_do_not_match_the_item() {
    let entry = common::rip_entry(&["01.flac", "02.flac"], &[true, true]);
    // 記録に無いファイル（件に後から足された）
    let e = bind_rip(
        &entry,
        &cd_draft(&[("CD/01.flac", 1, 1), ("CD/09.flac", 1, 2)]),
    )
    .unwrap_err();
    assert!(e.contains("記録に無いファイル"), "{e}");
    // ファイル数が違う
    let e = bind_rip(&entry, &cd_draft(&[("CD/01.flac", 1, 1)])).unwrap_err();
    assert!(e.contains("ファイル数"), "{e}");
    // 1 枚の吸い出しなのにディスク番号が割れた
    let e = bind_rip(
        &entry,
        &cd_draft(&[("CD/01.flac", 1, 1), ("CD/02.flac", 2, 1)]),
    )
    .unwrap_err();
    assert!(e.contains("ディスク番号"), "{e}");
    // 記録の CRC の件数がトラック数と合わない
    let mut broken = entry.clone();
    broken.report.crcs.pop();
    let e = bind_rip(
        &broken,
        &cd_draft(&[("CD/01.flac", 1, 1), ("CD/02.flac", 1, 2)]),
    )
    .unwrap_err();
    assert!(e.contains("crcs"), "{e}");
}

// ---------------------------------------------------------------- MusicBrainz のリリース（P4-21）

/// 下書きのリリース ID は UUID の形でなければ承認できない（タグにそのまま書くので）
#[test]
fn release_ids_must_be_uuids() {
    let mut d = draft();
    d.release_id = Some("f1223d63-f359-457d-b935-fc27eb24a6de".into());
    d.release_group_id = Some("0b3a4c5d-1111-2222-3333-444455556666".into());
    assert!(
        d.problems(&files()).is_empty(),
        "{:?}",
        d.problems(&files())
    );
    // 大文字の UUID も通す（foobar2000 などが書いた既存のタグ。値はスキャナと揃えるため正規化しない）
    d.release_id = Some("F1223D63-F359-457D-B935-FC27EB24A6DE".into());
    assert!(
        d.problems(&files()).is_empty(),
        "{:?}",
        d.problems(&files())
    );
    d.release_id =
        Some("https://musicbrainz.org/release/f1223d63-f359-457d-b935-fc27eb24a6de".into());
    d.release_group_id = Some("x".into());
    let p: Vec<String> = d.problems(&files()).iter().map(|e| e.to_string()).collect();
    assert_eq!(p.len(), 2, "{p:?}");
    assert!(p.iter().all(|m| m.contains("MusicBrainz")), "{p:?}");
}

/// 提案はファイルの MUSICBRAINZ_ALBUMID / RELEASEGROUPID の最頻値を持つ。保存した下書きに値があれば
/// それが勝ち、無ければ（旧下書き）提案の値
#[test]
fn proposal_and_merge_carry_release_ids() {
    let rows = vec![
        file(
            "A/01.flac",
            &[
                ("TITLE", "One"),
                (
                    "MUSICBRAINZ_ALBUMID",
                    "f1223d63-f359-457d-b935-fc27eb24a6de",
                ),
                (
                    "MUSICBRAINZ_RELEASEGROUPID",
                    "0b3a4c5d-1111-2222-3333-444455556666",
                ),
            ],
        ),
        file(
            "A/02.flac",
            &[
                ("TITLE", "Two"),
                (
                    "MUSICBRAINZ_ALBUMID",
                    "f1223d63-f359-457d-b935-fc27eb24a6de",
                ),
            ],
        ),
    ];
    let p = proposal(&rows, &[], &[]);
    assert_eq!(
        p.release_id.as_deref(),
        Some("f1223d63-f359-457d-b935-fc27eb24a6de")
    );
    assert_eq!(
        p.release_group_id.as_deref(),
        Some("0b3a4c5d-1111-2222-3333-444455556666")
    );
    let mut saved = draft();
    assert_eq!(
        merge_saved(&saved, &p).release_id.as_deref(),
        Some("f1223d63-f359-457d-b935-fc27eb24a6de")
    );
    saved.release_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
    assert_eq!(
        merge_saved(&saved, &p).release_id.as_deref(),
        Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
    );
}

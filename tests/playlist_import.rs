//! m3u8 の取り込み: 行の解析と Library のトラックへの解決（docs/TASKS.md P1-6、D-53）。
//! 旧ライブラリの `Playlists/m3u8/` は `../Anime/…/1.01. Crow Song.opus` のような旧 `Opus/`
//! 相対で、拡張子が今の Library（`.m4a` → 正規化後 `.flac`）と違う。完全一致 → stem 一致で解決する

use spindle::playlist::import::{parse_m3u8, resolve_entries, Candidate, Resolver};

#[test]
fn parse_skips_comments_blank_lines_and_bom() {
    let text =
        "\u{feff}#EXTM3U\r\n#EXTINF:280,Crow Song\r\n../Anime/A/01.opus\r\n\r\n  \n#\nB/02.flac\n";
    assert_eq!(parse_m3u8(text), vec!["../Anime/A/01.opus", "B/02.flac"]);
}

#[test]
fn parse_trims_surrounding_whitespace_but_keeps_inner() {
    assert_eq!(
        parse_m3u8("  A/01 Crow Song.flac  \n"),
        vec!["A/01 Crow Song.flac"]
    );
}

fn resolver() -> Resolver {
    Resolver::new(vec![
        Candidate {
            track_id: 1,
            rel_path_key: "anime/angel beats!/1.01. crow song.m4a".to_owned(),
            active: true,
        },
        Candidate {
            track_id: 2,
            rel_path_key: "j-pop/a/b/01 title.flac".to_owned(),
            active: true,
        },
        // 同じ stem で missing の行と active の行
        Candidate {
            track_id: 3,
            rel_path_key: "j-pop/a/b/02 dup.m4a".to_owned(),
            active: false,
        },
        Candidate {
            track_id: 4,
            rel_path_key: "j-pop/a/b/02 dup.flac".to_owned(),
            active: true,
        },
    ])
}

#[test]
fn exact_key_match_is_case_and_normalization_insensitive() {
    let r = resolver();
    assert_eq!(r.resolve("J-Pop/A/B/01 Title.flac"), Some(2));
    assert_eq!(r.resolve("J-POP/a/b/01 TITLE.flac"), Some(2));
}

#[test]
fn stem_match_ignores_extension_after_leading_dotdot_is_stripped() {
    let r = resolver();
    // 旧 Opus/Playlists/ 相対の行: ../ を剥がし、.opus → .m4a を stem で当てる
    assert_eq!(
        r.resolve("../Anime/Angel Beats!/1.01. Crow Song.opus"),
        Some(1)
    );
}

#[test]
fn backslashes_and_root_names_are_normalized() {
    let r = resolver();
    assert_eq!(r.resolve(r"..\..\Library\J-Pop\A\B\01 Title.flac"), Some(2));
    assert_eq!(r.resolve("../../Derived/J-Pop/A/B/01 Title.opus"), Some(2));
    assert_eq!(
        r.resolve(r"\\TRUENAS\music\Library\J-Pop\A\B\01 Title.flac"),
        Some(2)
    );
    assert_eq!(
        r.resolve("/mnt/ssd/media/Library/J-Pop/A/B/01 Title.flac"),
        Some(2)
    );
}

#[test]
fn windows_drive_absolute_paths_are_cut_at_the_root_name() {
    let r = resolver();
    assert_eq!(
        r.resolve(r"C:\music\Library\J-Pop\A\B\01 Title.flac"),
        Some(2)
    );
    assert_eq!(
        r.resolve("D:/media/Derived/J-Pop/A/B/01 Title.opus"),
        Some(2)
    );
}

#[test]
fn absolute_paths_cut_at_the_earliest_root_name_occurrence() {
    // Library と Derived が両方現れたら、配列順ではなく文字列中で先に現れた方
    let r = Resolver::new(vec![
        Candidate {
            track_id: 1,
            rel_path_key: "foo/library/x.flac".to_owned(),
            active: true,
        },
        Candidate {
            track_id: 2,
            rel_path_key: "x.flac".to_owned(),
            active: true,
        },
    ]);
    assert_eq!(r.resolve("/mnt/Derived/foo/Library/x.flac"), Some(1));
    assert_eq!(r.resolve("/mnt/Library/foo/Library/x.flac"), Some(1));
    assert_eq!(r.resolve("/mnt/Library/x.flac"), Some(2));
}

#[test]
fn absolute_paths_without_a_root_name_are_unresolved() {
    // 偶然同じ rel_path があっても、root の外の絶対パスは当てない
    let r = Resolver::new(vec![Candidate {
        track_id: 7,
        rel_path_key: "outside/a/x.flac".to_owned(),
        active: true,
    }]);
    assert_eq!(r.resolve("/outside/a/x.flac"), None);
    assert_eq!(r.resolve(r"\\server\share\outside\a\x.flac"), None);
    assert_eq!(r.resolve(r"C:\outside\a\x.flac"), None);
    // 相対ならそのまま当たる
    assert_eq!(r.resolve("outside/a/x.flac"), Some(7));
}

#[test]
fn stem_with_several_active_candidates_is_ambiguous_and_unresolved() {
    // 同じ stem の .flac と .m4a が両方 active: どちらか分からないので当てない（D-53）。
    // exact 一致なら迷わない
    let r = Resolver::new(vec![
        Candidate {
            track_id: 1,
            rel_path_key: "a/x.flac".to_owned(),
            active: true,
        },
        Candidate {
            track_id: 2,
            rel_path_key: "a/x.m4a".to_owned(),
            active: true,
        },
        Candidate {
            track_id: 3,
            rel_path_key: "a/x.wav".to_owned(),
            active: false,
        },
    ]);
    assert_eq!(r.resolve("a/x.opus"), None);
    assert_eq!(r.resolve("a/x.m4a"), Some(2));
    // active が 1 つだけなら missing が何本あっても決まる
    let r = Resolver::new(vec![
        Candidate {
            track_id: 1,
            rel_path_key: "a/x.flac".to_owned(),
            active: true,
        },
        Candidate {
            track_id: 3,
            rel_path_key: "a/x.wav".to_owned(),
            active: false,
        },
        Candidate {
            track_id: 4,
            rel_path_key: "a/x.m4a".to_owned(),
            active: false,
        },
    ]);
    assert_eq!(r.resolve("a/x.opus"), Some(1));
    // 全部 missing で複数あるときも同じく曖昧
    let r = Resolver::new(vec![
        Candidate {
            track_id: 9,
            rel_path_key: "a/x.flac".to_owned(),
            active: false,
        },
        Candidate {
            track_id: 3,
            rel_path_key: "a/x.wav".to_owned(),
            active: false,
        },
    ]);
    assert_eq!(r.resolve("a/x.opus"), None);
}

#[test]
fn stem_match_prefers_active_rows() {
    let r = resolver();
    assert_eq!(r.resolve("J-Pop/A/B/02 dup.opus"), Some(4));
}

#[test]
fn exact_match_wins_over_stem_even_if_missing() {
    let r = resolver();
    assert_eq!(r.resolve("J-Pop/A/B/02 dup.m4a"), Some(3));
}

#[test]
fn unknown_path_is_none() {
    let r = resolver();
    assert_eq!(r.resolve("Nope/x.flac"), None);
    assert_eq!(r.resolve("http://example.com/x.mp3"), None);
    assert_eq!(r.resolve(""), None);
}

#[test]
fn resolve_entries_keeps_order_drops_duplicates_and_reports_unresolved() {
    let r = resolver();
    let entries = [
        "J-Pop/A/B/02 dup.opus",
        "../Anime/Angel Beats!/1.01. Crow Song.opus",
        "Nope/x.flac",
        "J-Pop/A/B/02 dup.flac", // 同じトラック（1 プレイリストに 1 回だけ。D-53）
        "J-Pop/A/B/01 Title.flac",
    ];
    let res = resolve_entries(&r, entries.iter().map(|s| s.to_string()));
    assert_eq!(res.track_ids, vec![4, 1, 2]);
    assert_eq!(res.unresolved, vec!["Nope/x.flac"]);
    assert_eq!(res.duplicates, 1);
}

#[test]
fn relative_path_containing_a_root_named_directory_is_not_truncated() {
    let r = Resolver::new(vec![Candidate {
        track_id: 9,
        rel_path_key: "j-pop/library/x.flac".to_owned(),
        active: true,
    }]);
    assert_eq!(r.resolve("../../Library/J-Pop/Library/x.flac"), Some(9));
    assert_eq!(r.resolve("J-Pop/Library/x.flac"), Some(9));
}

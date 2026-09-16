//! パス生成（SPEC §5「パステンプレート」「ファイル名正規化」、D-7、docs/TASKS.md P0-11）。
//! テンプレート展開・置換テーブル・SMB 制約・切り詰め・衝突降格の単体テスト

use std::collections::{HashMap, HashSet};

use spindle::domain::pathgen::{
    plan, sanitize_component, AlbumVariant, Occupancy, PlanItem, Planned, Template, TrackFields,
};

fn fields() -> TrackFields {
    TrackFields {
        category: Some("J-Pop".to_owned()),
        albumartist: Some("花譜".to_owned()),
        artist: Some("花譜".to_owned()),
        album: Some("魔法".to_owned()),
        title: Some("過去を喰らう".to_owned()),
        disc_no: Some(1),
        track_no: Some(3),
        year: Some("2020".to_owned()),
        edition: None,
        ext: "flac".to_owned(),
        stem: "03 old".to_owned(),
    }
}

fn single() -> Template {
    Template::parse("{category}/{albumartist}/{album}/{track:02} {title}").unwrap()
}

fn multi() -> Template {
    Template::parse("{category}/{albumartist}/{album}/{disc}-{track:02} {title}").unwrap()
}

// ---------------------------------------------------------------- テンプレート展開

#[test]
fn renders_single_disc_template() {
    let p = single().render(&fields(), AlbumVariant::Plain).unwrap();
    assert_eq!(p.as_str(), "J-Pop/花譜/魔法/03 過去を喰らう.flac");
}

#[test]
fn renders_multi_disc_template_with_padding() {
    let mut f = fields();
    f.disc_no = Some(2);
    f.track_no = Some(7);
    let p = multi().render(&f, AlbumVariant::Plain).unwrap();
    assert_eq!(p.as_str(), "J-Pop/花譜/魔法/2-07 過去を喰らう.flac");
}

#[test]
fn reproduces_ytmusic_layout() {
    // ytmusic: <Cat>/<Artist>/<Album>/<track>. <title>.opus（連番は 0 埋めなし）
    let t = Template::parse("{category}/{albumartist}/{album}/{track}. {title}").unwrap();
    let f = TrackFields {
        category: Some("神椿Studio".to_owned()),
        albumartist: Some("花譜".to_owned()),
        artist: Some("花譜".to_owned()),
        album: Some("花譜のお歌".to_owned()),
        title: Some("そして花になる / 花譜 (Cover)".to_owned()),
        disc_no: None,
        track_no: Some(12),
        year: None,
        edition: None,
        ext: "opus".to_owned(),
        stem: "x".to_owned(),
    };
    let p = t.render(&f, AlbumVariant::Plain).unwrap();
    assert_eq!(
        p.as_str(),
        "神椿Studio/花譜/花譜のお歌/12. そして花になる ／ 花譜 (Cover).opus"
    );
}

#[test]
fn unknown_placeholder_is_rejected() {
    assert!(Template::parse("{category}/{genre}/{title}").is_err());
    assert!(Template::parse("{category}/{title").is_err());
}

#[test]
fn pad_must_be_zero_prefixed_and_only_on_integer_fields() {
    assert!(Template::parse("{album}/{track:2}").is_err());
    assert!(Template::parse("{album}/{track:0}").is_err());
    assert!(Template::parse("{album}/{track:x}").is_err());
    assert!(Template::parse("{album}/{title:02}").is_err());
    assert!(Template::parse("{album}/{disc:03}-{track:02}").is_ok());
}

#[test]
fn missing_values_fall_back() {
    let mut f = fields();
    f.albumartist = None;
    f.artist = None;
    f.album = None;
    f.title = None;
    f.track_no = None;
    let p = single().render(&f, AlbumVariant::Plain).unwrap();
    // albumartist → artist → Unknown Artist、album → Unknown Album、title → 現在のファイル名、track → 0
    assert_eq!(
        p.as_str(),
        "J-Pop/Unknown Artist/Unknown Album/00 03 old.flac"
    );
}

#[test]
fn albumartist_falls_back_to_artist() {
    let mut f = fields();
    f.albumartist = None;
    f.artist = Some("理芽".to_owned());
    let p = single().render(&f, AlbumVariant::Plain).unwrap();
    assert_eq!(p.as_str(), "J-Pop/理芽/魔法/03 過去を喰らう.flac");
}

// ---------------------------------------------------------------- 置換テーブルと SMB 制約

#[test]
fn replacement_table_matches_ytmusic_and_spec_additions() {
    assert_eq!(
        sanitize_component("a~b*c∕d:e>f<g?h"),
        "a～b＊c／d：e＞f＜g？h"
    );
    assert_eq!(sanitize_component("Øô Àèéë ゔ"), "Oo Aeee う");
}

#[test]
fn nfc_is_applied_before_replacement() {
    // NFD の e + 結合アクセント → NFC の é → e
    let nfd = "cafe\u{0301}";
    assert_eq!(sanitize_component(nfd), "cafe");
    // 置換対象でない文字は NFC に合成して保持する
    assert_eq!(sanitize_component("カ\u{3099}"), "ガ");
}

#[test]
fn remaining_forbidden_characters_are_widened() {
    assert_eq!(sanitize_component("a/b\\c|d\"e"), "a／b＼c｜d＂e");
}

#[test]
fn control_characters_are_removed() {
    assert_eq!(sanitize_component("a\u{0}b\tc\u{7f}d"), "abcd");
}

#[test]
fn trailing_dots_and_spaces_are_trimmed() {
    assert_eq!(sanitize_component("Title... "), "Title");
    assert_eq!(sanitize_component("Title . . "), "Title");
}

#[test]
fn reserved_names_are_suffixed() {
    assert_eq!(sanitize_component("CON"), "CON_");
    assert_eq!(sanitize_component("con.txt"), "con_.txt");
    assert_eq!(sanitize_component("LPT9"), "LPT9_");
    assert_eq!(sanitize_component("CONSOLE"), "CONSOLE");
}

#[test]
fn empty_component_becomes_underscore() {
    assert_eq!(sanitize_component(""), "_");
    assert_eq!(sanitize_component(" . "), "_");
}

#[test]
fn rendered_path_passes_relpath_validation_for_hostile_values() {
    let mut f = fields();
    f.title = Some("../..\\evil: <x>|\"y\"? .".to_owned());
    f.album = Some("NUL".to_owned());
    f.albumartist = Some(" ".to_owned());
    let p = single().render(&f, AlbumVariant::Plain).unwrap();
    assert_eq!(
        p.as_str(),
        "J-Pop/_/NUL_/03 ..／..＼evil： ＜x＞｜＂y＂？.flac"
    );
}

// ---------------------------------------------------------------- 切り詰め

#[test]
fn long_component_is_truncated_to_255_bytes_with_ellipsis() {
    let mut f = fields();
    f.title = Some("あ".repeat(200)); // 600 バイト
    let p = single().render(&f, AlbumVariant::Plain).unwrap();
    let name = p.file_name();
    assert!(name.len() <= 255, "{} bytes", name.len());
    assert!(name.starts_with("03 あ"));
    assert!(name.ends_with("….flac"), "{name}");
    // 拡張子は保たれる
    assert_eq!(name.rsplit_once('.').unwrap().1, "flac");
}

#[test]
fn long_directory_component_is_truncated_too() {
    let mut f = fields();
    f.album = Some("あ".repeat(100)); // 300 バイト、100 UTF-16 単位
    let p = single().render(&f, AlbumVariant::Plain).unwrap();
    let dir = p.components().nth(2).unwrap();
    assert!(dir.len() <= 255, "{} bytes", dir.len());
    assert!(dir.len() > 250);
    assert!(dir.ends_with('…'));
}

#[test]
fn whole_path_is_limited_to_240_utf16_units() {
    let mut f = fields();
    f.albumartist = Some("a".repeat(100));
    f.album = Some("b".repeat(100));
    f.title = Some("c".repeat(100));
    let p = single().render(&f, AlbumVariant::Plain).unwrap();
    let units = p.as_str().encode_utf16().count();
    assert!(units <= 240, "{units} units: {p}");
    // タイトル部だけが縮み、ディレクトリは保たれる
    assert!(p.as_str().starts_with(&format!(
        "J-Pop/{}/{}/03 c",
        "a".repeat(100),
        "b".repeat(100)
    )));
    assert!(p.file_name().ends_with("….flac"));
}

#[test]
fn directories_alone_over_the_path_limit_are_an_error() {
    let mut f = fields();
    f.albumartist = Some("a".repeat(120));
    f.album = Some("b".repeat(120));
    assert!(single().render(&f, AlbumVariant::Plain).is_err());
}

#[test]
fn extension_leaving_no_room_for_the_stem_is_an_error() {
    let mut f = fields();
    f.ext = "x".repeat(254);
    assert!(single().render(&f, AlbumVariant::Plain).is_err());
}

#[test]
fn truncation_that_cannot_keep_one_original_char_is_an_error() {
    // バイト: stem の予算 4 バイトに「あ」(3) + 「…」(3) は入らない
    let t = Template::parse("{title}").unwrap();
    let mut f = fields();
    f.title = Some("あ".repeat(10));
    f.ext = "x".repeat(250);
    assert!(t.render(&f, AlbumVariant::Plain).is_err());
    // UTF-16: stem の予算 2 単位に絵文字(2) + 「…」(1) は入らない
    let t = Template::parse("{category}/{albumartist}/{album}/{title}").unwrap();
    let mut f = fields();
    f.albumartist = Some("a".repeat(113));
    f.album = Some("b".repeat(112));
    f.title = Some("😀".repeat(5));
    let r = t.render(&f, AlbumVariant::Plain);
    assert!(r.is_err(), "{r:?}");
}

#[test]
fn utf16_limit_counts_surrogate_pairs() {
    let mut f = fields();
    f.title = Some("😀".repeat(200)); // 各 2 単位
    let p = single().render(&f, AlbumVariant::Plain).unwrap();
    assert!(p.as_str().encode_utf16().count() <= 240);
}

// ---------------------------------------------------------------- 衝突降格

#[test]
fn album_variants_append_year_and_edition() {
    let mut f = fields();
    f.edition = Some("Remaster".to_owned());
    assert_eq!(
        single()
            .render(&f, AlbumVariant::WithYear)
            .unwrap()
            .as_str(),
        "J-Pop/花譜/魔法 (2020)/03 過去を喰らう.flac"
    );
    assert_eq!(
        single()
            .render(&f, AlbumVariant::WithEdition)
            .unwrap()
            .as_str(),
        "J-Pop/花譜/魔法 (Remaster)/03 過去を喰らう.flac"
    );
}

#[test]
fn variant_without_value_is_an_error() {
    let mut f = fields();
    f.year = None;
    assert!(single().render(&f, AlbumVariant::WithYear).is_err());
    assert!(single().render(&f, AlbumVariant::WithEdition).is_err());
}

fn item(track_id: i64, release: &str, f: TrackFields, current: &str) -> PlanItem {
    PlanItem {
        track_id,
        template: single(),
        fields: f,
        release: release.to_owned(),
        current_rel_path: current.to_owned(),
    }
}

fn path(p: &Planned) -> &str {
    match p {
        Planned::Path(r) => r.as_str(),
        other => panic!("expected path, got {other:?}"),
    }
}

#[test]
fn same_release_shares_directory_without_demotion() {
    let mut f2 = fields();
    f2.track_no = Some(4);
    let items = vec![
        item(1, "mb:r1", fields(), "old/1.flac"),
        item(2, "mb:r1", f2, "old/2.flac"),
    ];
    let out = plan(&items, &Occupancy::default());
    assert_eq!(path(&out[0]), "J-Pop/花譜/魔法/03 過去を喰らう.flac");
    assert_eq!(path(&out[1]), "J-Pop/花譜/魔法/04 過去を喰らう.flac");
}

#[test]
fn different_releases_with_same_album_name_are_demoted_to_year() {
    let mut f2 = fields();
    f2.year = Some("2023".to_owned());
    let items = vec![
        item(1, "album:1", fields(), "old/1.flac"),
        item(2, "album:2", f2, "old/2.flac"),
    ];
    let out = plan(&items, &Occupancy::default());
    assert_eq!(path(&out[0]), "J-Pop/花譜/魔法 (2020)/03 過去を喰らう.flac");
    assert_eq!(path(&out[1]), "J-Pop/花譜/魔法 (2023)/03 過去を喰らう.flac");
}

#[test]
fn same_year_falls_back_to_edition() {
    let mut f1 = fields();
    f1.edition = Some("Deluxe".to_owned());
    let mut f2 = fields();
    f2.edition = Some("Remaster".to_owned());
    let items = vec![
        item(1, "album:1", f1, "old/1.flac"),
        item(2, "album:2", f2, "old/2.flac"),
    ];
    let out = plan(&items, &Occupancy::default());
    assert_eq!(
        path(&out[0]),
        "J-Pop/花譜/魔法 (Deluxe)/03 過去を喰らう.flac"
    );
    assert_eq!(
        path(&out[1]),
        "J-Pop/花譜/魔法 (Remaster)/03 過去を喰らう.flac"
    );
}

#[test]
fn unresolvable_collision_is_reported_not_merged() {
    // 同名・同年・edition なし → 降格しても解決しない。マージにはしない
    let items = vec![
        item(1, "album:1", fields(), "old/1.flac"),
        item(2, "album:2", fields(), "old/2.flac"),
    ];
    let out = plan(&items, &Occupancy::default());
    assert!(matches!(out[0], Planned::Conflict(_)), "{:?}", out[0]);
    assert!(matches!(out[1], Planned::Conflict(_)), "{:?}", out[1]);
}

#[test]
fn existing_directory_of_another_release_forces_demotion() {
    let mut occ = Occupancy::default();
    occ.dir_releases.insert(
        "j-pop/花譜/魔法".to_owned(),
        HashSet::from(["album:9".to_owned()]),
    );
    let items = vec![item(1, "album:1", fields(), "old/1.flac")];
    let out = plan(&items, &occ);
    assert_eq!(path(&out[0]), "J-Pop/花譜/魔法 (2020)/03 過去を喰らう.flac");
}

#[test]
fn joining_own_existing_directory_is_not_a_collision() {
    let mut occ = Occupancy::default();
    occ.dir_releases.insert(
        "j-pop/花譜/魔法".to_owned(),
        HashSet::from(["mb:r1".to_owned()]),
    );
    let items = vec![item(1, "mb:r1", fields(), "old/1.flac")];
    let out = plan(&items, &occ);
    assert_eq!(path(&out[0]), "J-Pop/花譜/魔法/03 過去を喰らう.flac");
}

#[test]
fn incumbent_release_keeps_directory_and_newcomer_is_demoted() {
    // 1 は既にその dir にいる。同名別リリースの 2 だけが降格する
    let mut f2 = fields();
    f2.year = Some("2023".to_owned());
    let items = vec![
        item(
            1,
            "album:1",
            fields(),
            "J-Pop/花譜/魔法/03 過去を喰らう.flac",
        ),
        item(2, "album:2", f2, "old/2.flac"),
    ];
    let out = plan(&items, &Occupancy::default());
    assert!(matches!(out[0], Planned::Unchanged), "{:?}", out[0]);
    assert_eq!(path(&out[1]), "J-Pop/花譜/魔法 (2023)/03 過去を喰らう.flac");
}

#[test]
fn case_only_difference_between_two_targets_is_a_collision() {
    let mut f1 = fields();
    f1.title = Some("ABC".to_owned());
    let mut f2 = fields();
    f2.title = Some("abc".to_owned());
    let items = vec![
        item(1, "mb:r1", f1, "old/1.flac"),
        item(2, "mb:r1", f2, "old/2.flac"),
    ];
    let out = plan(&items, &Occupancy::default());
    assert!(matches!(out[0], Planned::Conflict(_)));
    assert!(matches!(out[1], Planned::Conflict(_)));
}

#[test]
fn target_occupied_by_unselected_track_is_a_collision() {
    let mut occ = Occupancy::default();
    occ.path_keys
        .insert("j-pop/花譜/魔法/03 過去を喰らう.flac".to_owned());
    let items = vec![item(1, "mb:r1", fields(), "old/1.flac")];
    let out = plan(&items, &occ);
    assert!(matches!(out[0], Planned::Conflict(_)));
}

#[test]
fn unchanged_target_is_reported_as_unchanged() {
    let items = vec![item(
        1,
        "mb:r1",
        fields(),
        "J-Pop/花譜/魔法/03 過去を喰らう.flac",
    )];
    let out = plan(&items, &Occupancy::default());
    assert!(matches!(out[0], Planned::Unchanged));
}

#[test]
fn case_only_rename_of_itself_is_allowed() {
    let items = vec![item(
        1,
        "mb:r1",
        fields(),
        "j-pop/花譜/魔法/03 過去を喰らう.flac",
    )];
    let out = plan(&items, &Occupancy::default());
    assert_eq!(path(&out[0]), "J-Pop/花譜/魔法/03 過去を喰らう.flac");
}

#[test]
fn swap_targets_do_not_collide_with_each_other() {
    // 1 は 2 の現在パスへ、2 は 1 の現在パスへ。選択内の現在 key は占有とみなさない
    let mut f1 = fields();
    f1.track_no = Some(4);
    let mut f2 = fields();
    f2.track_no = Some(3);
    let items = vec![
        item(1, "mb:r1", f1, "J-Pop/花譜/魔法/03 過去を喰らう.flac"),
        item(2, "mb:r1", f2, "J-Pop/花譜/魔法/04 過去を喰らう.flac"),
    ];
    let out = plan(&items, &Occupancy::default());
    assert_eq!(path(&out[0]), "J-Pop/花譜/魔法/04 過去を喰らう.flac");
    assert_eq!(path(&out[1]), "J-Pop/花譜/魔法/03 過去を喰らう.flac");
}

#[test]
fn occupancy_can_be_built_from_maps() {
    let occ = Occupancy {
        dir_releases: HashMap::new(),
        path_keys: HashSet::new(),
    };
    assert!(occ.dir_releases.is_empty());
}

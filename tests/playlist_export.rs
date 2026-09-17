//! m3u8 の生成とパス写像（SPEC §10、docs/TASKS.md P1-6、D-53）。
//! 純粋関数: プロファイル + 行 → 文字列。ファイルへの書き出しは API テストで見る

use spindle::playlist::export::{render_m3u8, ExportProfile, ExportTrack, PathStyle, Source};

fn profile(source: Source, style: PathStyle, prefix: Option<&str>, sep: &str) -> ExportProfile {
    ExportProfile {
        name: "p".to_owned(),
        source,
        path_style: style,
        path_prefix: prefix.map(str::to_owned),
        path_sep: sep.to_owned(),
    }
}

fn track(path: &str) -> ExportTrack {
    ExportTrack {
        path: path.to_owned(),
        title: Some("Crow Song".to_owned()),
        artist: Some("Girls Dead Monster".to_owned()),
        duration_ms: Some(280_400),
    }
}

#[test]
fn internal_profile_writes_relative_paths_from_playlists_dir() {
    // Playlists/<profile>/ は Library/ と同じ深さなので ../../Library/… で解決する
    let p = profile(Source::Master, PathStyle::Relative, None, "/");
    let out = render_m3u8(&p, &[track("Library/Anime/Angel Beats!/01 Crow Song.flac")]);
    assert_eq!(
        out,
        "#EXTM3U\n#EXTINF:280,Girls Dead Monster - Crow Song\n../../Library/Anime/Angel Beats!/01 Crow Song.flac\n"
    );
}

#[test]
fn android_profile_keeps_derived_paths_relative() {
    let p = profile(Source::Delivery, PathStyle::Relative, None, "/");
    let out = render_m3u8(&p, &[track("Derived/Anime/Angel Beats!/01 Crow Song.opus")]);
    assert!(out.ends_with("../../Derived/Anime/Angel Beats!/01 Crow Song.opus\n"));
}

#[test]
fn foobar_profile_prepends_prefix_and_uses_backslash() {
    let p = profile(
        Source::Master,
        PathStyle::Absolute,
        Some(r"\\TRUENAS\music\"),
        r"\",
    );
    let out = render_m3u8(&p, &[track("Library/Anime/Angel Beats!/01 Crow Song.flac")]);
    assert!(
        out.ends_with("\\\\TRUENAS\\music\\Library\\Anime\\Angel Beats!\\01 Crow Song.flac\n"),
        "{out}"
    );
}

#[test]
fn absolute_without_prefix_starts_with_root_name() {
    let p = profile(Source::Master, PathStyle::Absolute, None, "/");
    let out = render_m3u8(&p, &[track("Library/x.flac")]);
    assert!(out.ends_with("\nLibrary/x.flac\n"), "{out}");
}

#[test]
fn extinf_falls_back_when_tags_are_missing() {
    let p = profile(Source::Master, PathStyle::Relative, None, "/");
    let t = ExportTrack {
        path: "Library/J-Pop/A/B/01 Untitled.flac".to_owned(),
        title: None,
        artist: None,
        duration_ms: None,
    };
    let out = render_m3u8(&p, &[t]);
    // 長さ不明は -1、表示名はファイル名の stem
    assert_eq!(
        out,
        "#EXTM3U\n#EXTINF:-1,01 Untitled\n../../Library/J-Pop/A/B/01 Untitled.flac\n"
    );
}

#[test]
fn title_without_artist_has_no_separator() {
    let p = profile(Source::Master, PathStyle::Relative, None, "/");
    let t = ExportTrack {
        artist: None,
        ..track("Library/x.flac")
    };
    let out = render_m3u8(&p, &[t]);
    assert!(out.contains("#EXTINF:280,Crow Song\n"), "{out}");
}

#[test]
fn empty_playlist_is_just_the_header_without_bom() {
    let p = profile(Source::Master, PathStyle::Relative, None, "/");
    let out = render_m3u8(&p, &[]);
    assert_eq!(out, "#EXTM3U\n");
    assert!(!out.starts_with('\u{feff}'));
}

#[test]
fn newlines_in_tags_do_not_break_the_line_structure() {
    // タグに改行があっても行数が崩れない（EXTINF は 1 行）
    let p = profile(Source::Master, PathStyle::Relative, None, "/");
    let t = ExportTrack {
        title: Some("A\nB".to_owned()),
        ..track("Library/x.flac")
    };
    let out = render_m3u8(&p, &[t]);
    assert_eq!(out.lines().count(), 3, "{out}");
}

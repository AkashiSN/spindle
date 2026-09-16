//! 相対パスの検証と canonical key（SPEC §5「パスの表現と境界」、D-31）。
//! 受け入れ: docs/TASKS.md P0-5 (i) (j)

use spindle::domain::relpath::{canonical_key, RelPath, RelPathError};

fn err(s: &str) -> RelPathError {
    RelPath::parse(s).expect_err(&format!("{s:?} は拒否されるべき"))
}

// ---------------------------------------------------------------- 受理

#[test]
fn accepts_ordinary_nested_path() {
    let p = RelPath::parse("Pop/Artist/Album/1-01 Title.flac").unwrap();
    assert_eq!(p.as_str(), "Pop/Artist/Album/1-01 Title.flac");
    assert_eq!(p.file_name(), "1-01 Title.flac");
    assert_eq!(p.parent().unwrap().as_str(), "Pop/Artist/Album");
    assert_eq!(
        p.components().collect::<Vec<_>>(),
        ["Pop", "Artist", "Album", "1-01 Title.flac"]
    );
}

#[test]
fn accepts_single_component_and_has_no_parent() {
    let p = RelPath::parse("x.flac").unwrap();
    assert_eq!(p.file_name(), "x.flac");
    assert!(p.parent().is_none());
}

#[test]
fn accepts_leading_dash_file_name() {
    // 先頭 `-` はパスとしては正当。外部コマンドへ渡す側で無害化する（受け入れ (i)）
    let p = RelPath::parse("Album/-x.flac").unwrap();
    assert_eq!(p.file_name(), "-x.flac");
}

#[test]
fn join_appends_one_component() {
    let dir = RelPath::parse("Pop/Artist").unwrap();
    assert_eq!(dir.join("Album").unwrap().as_str(), "Pop/Artist/Album");
    assert!(matches!(
        dir.join("a/b"),
        Err(RelPathError::ForbiddenChar('/'))
    ));
    assert!(matches!(dir.join(".."), Err(RelPathError::DotComponent)));
}

// ---------------------------------------------------------------- 拒否（受け入れ (i)）

#[test]
fn rejects_absolute_path() {
    assert!(matches!(err("/abs/x.flac"), RelPathError::Absolute));
}

#[test]
fn rejects_parent_and_current_dir_components() {
    assert!(matches!(err("../x.flac"), RelPathError::DotComponent));
    assert!(matches!(err("a/../x.flac"), RelPathError::DotComponent));
    assert!(matches!(err("a/./x.flac"), RelPathError::DotComponent));
    assert!(matches!(err("."), RelPathError::DotComponent));
}

#[test]
fn rejects_empty_and_empty_components() {
    assert!(matches!(err(""), RelPathError::Empty));
    assert!(matches!(err("a//b.flac"), RelPathError::EmptyComponent));
    assert!(matches!(err("a/b.flac/"), RelPathError::EmptyComponent));
}

#[test]
fn rejects_nul_and_backslash() {
    assert!(matches!(err("a\0b.flac"), RelPathError::Nul));
    assert!(matches!(err("a\\b.flac"), RelPathError::Backslash));
}

#[test]
fn rejects_smb_and_exfat_forbidden_chars() {
    for c in ['<', '>', ':', '"', '|', '?', '*', '\u{1}', '\u{1f}'] {
        assert!(
            matches!(err(&format!("a/b{c}c.flac")), RelPathError::ForbiddenChar(x) if x == c),
            "{c:?}"
        );
    }
}

#[test]
fn rejects_trailing_dot_or_space_in_any_component() {
    assert!(matches!(
        err("Album./x.flac"),
        RelPathError::TrailingDotOrSpace
    ));
    assert!(matches!(
        err("Album /x.flac"),
        RelPathError::TrailingDotOrSpace
    ));
    assert!(matches!(
        err("Album/x.flac "),
        RelPathError::TrailingDotOrSpace
    ));
    assert!(matches!(
        err("Album/x.flac."),
        RelPathError::TrailingDotOrSpace
    ));
}

#[test]
fn rejects_windows_reserved_names_case_insensitively() {
    for name in [
        "CON", "con", "Prn", "AUX", "NUL", "COM1", "com9", "LPT1", "lpt9",
    ] {
        assert!(
            matches!(
                err(&format!("{name}/x.flac")),
                RelPathError::ReservedName(_)
            ),
            "{name}"
        );
        // 拡張子付きも Windows では予約扱い
        assert!(
            matches!(
                err(&format!("a/{name}.flac")),
                RelPathError::ReservedName(_)
            ),
            "{name}.flac"
        );
    }
    // COM0 / LPT0 / CONSOLE は予約ではない
    for name in ["COM0", "LPT0", "CONSOLE", "NULL"] {
        assert!(RelPath::parse(&format!("{name}/x.flac")).is_ok(), "{name}");
    }
}

#[test]
fn rejects_component_over_255_bytes() {
    let ok = "あ".repeat(85); // 255 バイト
    assert!(RelPath::parse(&format!("{ok}/x.flac")).is_ok());
    let long = format!("{ok}a"); // 256 バイト
    assert!(matches!(
        err(&format!("{long}/x.flac")),
        RelPathError::ComponentTooLong
    ));
    // パス全体には要素数の上限を設けない（255 バイトは要素ごとの上限）
    let deep = std::iter::repeat_n(ok.as_str(), 4)
        .collect::<Vec<_>>()
        .join("/");
    assert!(RelPath::parse(&deep).is_ok());
}

// ---------------------------------------------------------------- canonical key（受け入れ (j)）

#[test]
fn key_folds_ascii_case() {
    assert_eq!(canonical_key("B/x.flac"), canonical_key("b/x.flac"));
    assert_eq!(canonical_key("B/x.flac"), "b/x.flac");
}

#[test]
fn key_unifies_nfc_and_nfd() {
    let nfc = "が"; // U+304C
    let nfd = "か\u{3099}"; // U+304B U+3099
    assert_ne!(nfc, nfd);
    assert_eq!(canonical_key(nfc), canonical_key(nfd));
    // key は NFD 側
    assert_eq!(canonical_key(nfc), nfd);
}

#[test]
fn key_does_not_apply_nfkc_fullwidth_is_distinct() {
    assert_ne!(canonical_key("Ｂ/x.flac"), canonical_key("B/x.flac"));
    // 全角の大小文字は畳む
    assert_eq!(canonical_key("Ｂ"), canonical_key("ｂ"));
}

#[test]
fn key_uses_full_case_folding_for_non_ascii() {
    // ß は full casefold で ss。spindle 側の保守的な同値規則（D-31 の限界として記録）
    assert_eq!(canonical_key("straße"), canonical_key("STRASSE"));
    assert_eq!(canonical_key("Ω"), canonical_key("ω"));
}

#[test]
fn key_is_idempotent() {
    // casefold の結果が NFD でない文字（U+1F88 → U+1F00 U+03B9 → 分解が必要）でも
    // key(key(x)) == key(x)
    for s in ["ᾈ", "Ǆ", "İstanbul", "ﬁ", "が/Ｂ/STRASSE"] {
        let k = canonical_key(s);
        assert_eq!(canonical_key(&k), k, "{s}");
    }
}

#[test]
fn relpath_key_matches_free_function() {
    let p = RelPath::parse("Ｐop/Ärtist/が.flac").unwrap();
    assert_eq!(p.key(), canonical_key("Ｐop/Ärtist/が.flac"));
    assert_eq!(p.parent().unwrap().key(), canonical_key("Ｐop/Ärtist"));
}

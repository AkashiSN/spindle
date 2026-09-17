//! スマートプレイリストのルール → foobar2000 Autoplaylist のクエリ + ソートパターン
//! （docs/DSL.md「foobar2000 へのエクスポート」、D-55）。
//! 写像表・演算子・引用規則・変換不能な項の脱落・ORDER BY の分離を固定する

use spindle::playlist::dsl::parse;
use spindle::playlist::fb2k::{convert, Fb2kQuery};

fn conv(src: &str) -> Fb2kQuery {
    convert(&parse(src).unwrap())
}

#[test]
fn standard_tags_map_to_same_name_and_albumartist_gets_a_space() {
    let q = conv("%albumartist% IS ヰ世界情緒 AND %title% HAS 花");
    assert_eq!(q.query, "%album artist% IS ヰ世界情緒 AND %title% HAS 花");
    assert_eq!(q.sort, None);
    assert!(q.notes.is_empty(), "{:?}", q.notes);
}

#[test]
fn each_standard_and_arbitrary_field_maps() {
    for (field, fb) in [
        ("title", "%title%"),
        ("artist", "%artist%"),
        ("album", "%album%"),
        ("albumartist", "%album artist%"),
        ("date", "%date%"),
        ("genre", "%genre%"),
        ("tracknumber", "%tracknumber%"),
        ("discnumber", "%discnumber%"),
        ("composer", "%composer%"),
        ("comment", "%comment%"),
        ("%Album Artist%", "%album artist%"),
        ("catalognumber", "%catalognumber%"),
    ] {
        let src = if field.starts_with('%') {
            format!("{field} IS x")
        } else {
            format!("%{field}% IS x")
        };
        let q = conv(&src);
        assert_eq!(q.query, format!("{fb} IS x"), "{src}");
        assert!(q.notes.is_empty(), "{src}: {:?}", q.notes);
    }
}

#[test]
fn technical_fields_map_to_raw_info_fields() {
    // 生の技術情報は `%__…%`（`%channels%` は mono / stereo の表示文字列になるので使わない）。
    // 長さは特殊フィールド `%length_seconds%`。演算子は保存経路の型検査が通る組合せだけ
    let q = conv("%codec% IS flac AND %codec% HAS fl");
    assert_eq!(q.query, "%__codec% IS flac AND %__codec% HAS fl");
    assert!(q.notes.is_empty(), "{:?}", q.notes);
    for (field, fb) in [
        ("samplerate", "%__samplerate%"),
        ("bitrate", "%__bitrate%"),
        ("channels", "%__channels%"),
        ("bitdepth", "%__bitspersample%"),
        ("duration", "%length_seconds%"),
    ] {
        for op in ["IS", "GREATER", "LESS"] {
            let q = conv(&format!("%{field}% {op} 1"));
            assert_eq!(q.query, format!("{fb} {op} 1"), "{field} {op}");
            assert!(q.notes.is_empty(), "{field} {op}: {:?}", q.notes);
        }
    }
}

#[test]
fn numeric_sort_fields_are_zero_padded_with_num() {
    // foobar のソートパターンは文字列比較なので、数値は `$num(…, 10)` で桁を揃える
    for (field, fb) in [
        ("tracknumber", "$num(%tracknumber%,10)"),
        ("discnumber", "$num(%discnumber%,10)"),
        ("samplerate", "$num(%__samplerate%,10)"),
        ("bitrate", "$num(%__bitrate%,10)"),
        ("channels", "$num(%__channels%,10)"),
        ("bitdepth", "$num(%__bitspersample%,10)"),
        ("duration", "$num(%length_seconds%,10)"),
    ] {
        let q = conv(&format!("%artist% IS x ORDER BY %{field}%"));
        assert_eq!(q.sort.as_deref(), Some(fb), "{field}");
        assert!(q.notes.is_empty(), "{field}: {:?}", q.notes);
    }
    // 文字列のフィールドはそのまま（date は YYYY-MM-DD の辞書順で foobar と一致）
    for (field, fb) in [
        ("codec", "%__codec%"),
        ("date", "%date%"),
        ("title", "%title%"),
    ] {
        let q = conv(&format!("%artist% IS x ORDER BY %{field}%"));
        assert_eq!(q.sort.as_deref(), Some(fb), "{field}");
    }
    // 比較には `$num` を付けない
    let q = conv("%bitrate% GREATER 96");
    assert_eq!(q.query, "%__bitrate% GREATER 96");
}

#[test]
fn presence_of_technical_info_fields_uses_raw_names_but_duration_is_dropped() {
    for (field, fb) in [
        ("codec", "%__codec%"),
        ("samplerate", "%__samplerate%"),
        ("bitrate", "%__bitrate%"),
        ("channels", "%__channels%"),
        ("bitdepth", "%__bitspersample%"),
    ] {
        let q = conv(&format!("PRESENT %{field}%"));
        assert_eq!(q.query, format!("{fb} PRESENT"), "{field}");
        let q = conv(&format!("MISSING %{field}%"));
        assert_eq!(q.query, format!("{fb} MISSING"), "{field}");
        assert!(q.notes.is_empty(), "{field}: {:?}", q.notes);
    }
    // `%length_seconds%` は技術情報でなく特殊フィールドなので有無を問えない
    let q = conv("%artist% IS x AND MISSING %duration%");
    assert_eq!(q.query, "%artist% IS x");
    assert_eq!(q.notes.len(), 1, "{:?}", q.notes);
    assert!(
        q.notes[0].starts_with("MISSING %duration% … "),
        "{:?}",
        q.notes
    );
}

#[test]
fn spindle_only_fields_are_dropped_with_a_note() {
    for field in [
        "verification",
        "category",
        "source_type",
        "lossless",
        "added",
        "has_derived",
        "missing",
    ] {
        let q = conv(&format!("%{field}% IS x"));
        assert_eq!(q.query, "", "{field}");
        assert_eq!(q.notes.len(), 2, "{field}: {:?}", q.notes);
        assert!(q.notes[0].contains(&format!("%{field}%")), "{:?}", q.notes);
        assert!(q.notes[0].contains("spindle 固有"), "{:?}", q.notes);
        assert!(q.notes[1].contains("空"), "{:?}", q.notes);
    }
}

#[test]
fn presence_becomes_postfix() {
    let q = conv("PRESENT %lyrics% AND MISSING %comment%");
    assert_eq!(q.query, "%lyrics% PRESENT AND %comment% MISSING");
    assert!(q.notes.is_empty());
}

#[test]
fn date_comparisons_become_after_and_before() {
    let q = conv("%date% GREATER 2020 AND %date% LESS 2024-01");
    assert_eq!(q.query, "%date% AFTER 2020 AND %date% BEFORE 2024-01");
    assert!(q.notes.is_empty());
    let q = conv("%tracknumber% GREATER 3 AND %samplerate% LESS 48000");
    assert_eq!(
        q.query,
        "%tracknumber% GREATER 3 AND %__samplerate% LESS 48000"
    );
}

#[test]
fn matches_is_dropped_with_a_note() {
    let q = conv(r#"%artist% IS x AND %title% MATCHES "^\d+""#);
    assert_eq!(q.query, "%artist% IS x");
    assert_eq!(q.notes.len(), 1, "{:?}", q.notes);
    assert!(q.notes[0].contains("MATCHES"), "{:?}", q.notes);
    assert!(q.notes[0].contains("%title%"), "{:?}", q.notes);
}

#[test]
fn values_are_quoted_when_needed() {
    // 空白
    let q = conv(r#"%album% IS "Angel Beats! PERFECT Vocal Collection""#);
    assert_eq!(
        q.query,
        r#"%album% IS "Angel Beats! PERFECT Vocal Collection""#
    );
    // 括弧
    let q = conv(r#"%title% HAS "(TV""#);
    assert_eq!(q.query, r#"%title% HAS "(TV""#);
    // 予約語（大小文字無視）
    let q = conv(r#"%title% IS "and""#);
    assert_eq!(q.query, r#"%title% IS "and""#);
    let q = conv("%artist% IS Or");
    assert_eq!(q.query, r#"%artist% IS "Or""#);
    // 空文字
    let q = conv(r#"%comment% IS """#);
    assert_eq!(q.query, r#"%comment% IS """#);
    // 引用の要らない値はそのまま
    let q = conv(r#"%artist% IS "ヰ世界情緒""#);
    assert_eq!(q.query, "%artist% IS ヰ世界情緒");
    assert!(q.notes.is_empty());
}

#[test]
fn double_quote_in_value_is_emitted_with_a_warning() {
    let q = conv(r#"%title% HAS "say \"hi\"""#);
    assert_eq!(q.query, r#"%title% HAS "say "hi"""#);
    assert_eq!(q.notes.len(), 1, "{:?}", q.notes);
    assert!(q.notes[0].contains('"'), "{:?}", q.notes);
    assert!(q.notes[0].contains("%title%"), "{:?}", q.notes);
}

#[test]
fn compound_children_are_parenthesized_and_not_is_kept() {
    let q = conv("%a% IS 1 AND (%b% IS 2 OR %c% IS 3) AND NOT %d% IS 4");
    assert_eq!(
        q.query,
        "%a% IS 1 AND (%b% IS 2 OR %c% IS 3) AND NOT %d% IS 4"
    );
    let q = conv("NOT (%a% IS 1 OR %b% IS 2)");
    assert_eq!(q.query, "NOT (%a% IS 1 OR %b% IS 2)");
}

#[test]
fn dropping_a_leaf_collapses_empty_parents() {
    // AND の中の 1 葉
    let q = conv("%albumartist% IS x AND %verification% IS verified_ctdb");
    assert_eq!(q.query, "%album artist% IS x");
    assert_eq!(q.notes.len(), 1, "{:?}", q.notes);
    // OR の中の 1 葉（結果は狭まるので notes に出るだけ）
    let q = conv("%artist% IS x OR %category% IS Anime");
    assert_eq!(q.query, "%artist% IS x");
    assert_eq!(q.notes.len(), 1, "{:?}", q.notes);
    // 括弧の中が 1 つになったら括弧も外れる
    let q = conv("%a% IS 1 AND (%b% IS 2 OR %missing% IS true)");
    assert_eq!(q.query, "%a% IS 1 AND %b% IS 2");
    // 括弧の中が全部落ちたら括弧ごと消える
    let q = conv("%a% IS 1 AND (%lossless% IS true OR %missing% IS true)");
    assert_eq!(q.query, "%a% IS 1");
    assert_eq!(q.notes.len(), 2, "{:?}", q.notes);
    // NOT の子が落ちたら NOT も落ちる
    let q = conv("%a% IS 1 AND NOT %has_derived% IS true");
    assert_eq!(q.query, "%a% IS 1");
    // 全部落ちたら空。空である旨の note が最後に付く
    let q = conv("NOT (%lossless% IS true AND %missing% IS false)");
    assert_eq!(q.query, "");
    assert_eq!(q.notes.len(), 3, "{:?}", q.notes);
    assert!(q.notes[2].contains("空"), "{:?}", q.notes);
}

#[test]
fn order_by_is_separated_into_a_sort_pattern() {
    let q = conv("%artist% IS x ORDER BY %date%");
    assert_eq!(q.sort.as_deref(), Some("%date%"));
    assert!(q.notes.is_empty());
    let q = conv("%artist% IS x ORDER BY %albumartist% ASC");
    assert_eq!(q.sort.as_deref(), Some("%album artist%"));
}

#[test]
fn descending_sort_is_noted() {
    let q = conv("%artist% IS x ORDER BY %date% DESC");
    assert_eq!(q.sort.as_deref(), Some("%date%"));
    assert_eq!(q.notes.len(), 1, "{:?}", q.notes);
    assert!(q.notes[0].contains("DESC"), "{:?}", q.notes);
}

#[test]
fn sort_by_spindle_only_field_is_dropped_with_a_note() {
    let q = conv("%artist% IS x ORDER BY %added% DESC");
    assert_eq!(q.sort, None);
    assert_eq!(q.notes.len(), 1, "{:?}", q.notes);
    assert!(q.notes[0].contains("ORDER BY %added%"), "{:?}", q.notes);
}

#[test]
fn random_and_limit_are_noted() {
    let q = conv("%artist% IS x ORDER BY random LIMIT 100");
    assert_eq!(q.query, "%artist% IS x");
    assert_eq!(q.sort, None);
    assert_eq!(q.notes.len(), 2, "{:?}", q.notes);
    assert!(q.notes[0].contains("random"), "{:?}", q.notes);
    assert!(q.notes[1].contains("LIMIT 100"), "{:?}", q.notes);
}

#[test]
fn dsl_md_example() {
    let q = conv(
        "%albumartist% IS ヰ世界情緒 AND %verification% IS verified_ctdb
  AND NOT %category% IS _Unsorted
ORDER BY %date% DESC LIMIT 100",
    );
    assert_eq!(q.query, "%album artist% IS ヰ世界情緒");
    assert_eq!(q.sort.as_deref(), Some("%date%"));
    assert_eq!(
        q.notes,
        vec![
            "%verification% IS verified_ctdb … spindle 固有フィールド（foobar に無い）".to_owned(),
            "NOT %category% IS _Unsorted … spindle 固有フィールド（foobar に無い）".to_owned(),
            "ORDER BY %date% DESC … ソートパターンでは降順を表せない（foobar 側で並びを反転する）"
                .to_owned(),
            "LIMIT 100 … foobar の Autoplaylist に相当機能なし".to_owned(),
        ]
    );
}

//! 一括編集の操作（SPEC §12.3、docs/TASKS.md P0-10）: 固定値代入 / フィールド参照 /
//! 正規表現置換 / 連番 / 削除。JSON から解釈し、正規化タグ集合へ順に適用する

use spindle::domain::tagops::{apply_ops, parse_ops, Op, TagOpsError};
use spindle::domain::tags::{normalize_tags, TagSet};

fn tags(items: &[(&str, &str)]) -> TagSet {
    normalize_tags(
        items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
    )
}

fn values(set: &TagSet, key: &str) -> Vec<String> {
    set.values(key).map(str::to_owned).collect()
}

fn ops(json: &str) -> Vec<Op> {
    parse_ops(&serde_json::from_str(json).unwrap()).unwrap()
}

#[test]
fn set_assigns_single_and_multi_values() {
    let t = tags(&[("TITLE", "a"), ("ARTIST", "x")]);
    let out = apply_ops(
        &ops(r#"[{"op":"set","key":"title","value":"b"},{"op":"set","key":"ARTIST","value":["A","B"]}]"#),
        &t,
        0,
    )
    .unwrap();
    assert_eq!(values(&out, "TITLE"), ["b"]);
    assert_eq!(values(&out, "ARTIST"), ["A", "B"]);
}

#[test]
fn set_with_empty_value_deletes_and_delete_removes_key() {
    let t = tags(&[("COMMENT", "c"), ("TITLE", "t")]);
    let out = apply_ops(
        &ops(r#"[{"op":"delete","key":"comment"},{"op":"set","key":"TITLE","value":""}]"#),
        &t,
        0,
    )
    .unwrap();
    assert!(values(&out, "COMMENT").is_empty());
    assert!(values(&out, "TITLE").is_empty());
}

#[test]
fn ref_expands_field_references_with_first_value() {
    let t = tags(&[("ARTIST", "A"), ("ARTIST", "B"), ("ALBUM", "Al")]);
    let out = apply_ops(
        &ops(r#"[{"op":"ref","key":"ALBUMARTIST","template":"%artist% / %album% / %missing%"}]"#),
        &t,
        0,
    )
    .unwrap();
    assert_eq!(values(&out, "ALBUMARTIST"), ["A / Al / "]);
}

#[test]
fn ops_apply_in_order_and_see_previous_results() {
    let t = tags(&[("ARTIST", "A")]);
    let out = apply_ops(
        &ops(r#"[{"op":"set","key":"ARTIST","value":"Z"},
                {"op":"ref","key":"ALBUMARTIST","template":"%artist%"}]"#),
        &t,
        0,
    )
    .unwrap();
    assert_eq!(values(&out, "ALBUMARTIST"), ["Z"]);
}

#[test]
fn replace_applies_regex_to_each_value_with_captures() {
    let t = tags(&[
        ("TITLE", "Song (Official Video)"),
        ("ARTIST", "feat. X"),
        ("ARTIST", "Y"),
    ]);
    let out = apply_ops(
        &ops(
            r#"[{"op":"replace","key":"TITLE","pattern":"\\s*\\(Official.*\\)$","replacement":""},
                {"op":"replace","key":"ARTIST","pattern":"^feat\\. (.*)$","replacement":"$1 (feat)"}]"#,
        ),
        &t,
        0,
    )
    .unwrap();
    assert_eq!(values(&out, "TITLE"), ["Song"]);
    assert_eq!(values(&out, "ARTIST"), ["X (feat)", "Y"]);
}

#[test]
fn replace_on_missing_key_is_noop_and_lookaround_is_supported() {
    let t = tags(&[("TITLE", "abc123")]);
    let out = apply_ops(
        &ops(
            r#"[{"op":"replace","key":"COMMENT","pattern":"x","replacement":"y"},
                {"op":"replace","key":"TITLE","pattern":"(?<=abc)\\d+","replacement":"N"}]"#,
        ),
        &t,
        0,
    )
    .unwrap();
    assert!(values(&out, "COMMENT").is_empty());
    assert_eq!(values(&out, "TITLE"), ["abcN"]);
}

#[test]
fn invalid_regex_is_rejected_at_parse() {
    let err = parse_ops(
        &serde_json::from_str(r#"[{"op":"replace","key":"TITLE","pattern":"(","replacement":""}]"#)
            .unwrap(),
    )
    .unwrap_err();
    assert!(matches!(err, TagOpsError::Regex { .. }), "{err:?}");
}

#[test]
fn number_assigns_sequence_from_index() {
    let t = tags(&[("TITLE", "t")]);
    let o = ops(r#"[{"op":"number","key":"TRACKNUMBER","start":1}]"#);
    assert_eq!(values(&apply_ops(&o, &t, 0).unwrap(), "TRACKNUMBER"), ["1"]);
    assert_eq!(values(&apply_ops(&o, &t, 4).unwrap(), "TRACKNUMBER"), ["5"]);
    let o = ops(r#"[{"op":"number","key":"TRACKNUMBER","start":10,"pad":2}]"#);
    assert_eq!(
        values(&apply_ops(&o, &t, 0).unwrap(), "TRACKNUMBER"),
        ["10"]
    );
    let o = ops(r#"[{"op":"number","key":"TRACKNUMBER","start":1,"pad":2}]"#);
    assert_eq!(
        values(&apply_ops(&o, &t, 2).unwrap(), "TRACKNUMBER"),
        ["03"]
    );
}

#[test]
fn unknown_op_or_bad_key_is_rejected() {
    for bad in [
        r#"[{"op":"frobnicate","key":"TITLE"}]"#,
        r#"[{"op":"set","key":"","value":"x"}]"#,
        r#"[{"op":"set","key":"TI=TLE","value":"x"}]"#,
        r#"[{"op":"set","key":"PICTURE","value":"x"}]"#,
        r#"[{"op":"number","key":"TRACKNUMBER","start":-1}]"#,
        r#"[{"op":"number","key":"TRACKNUMBER","start":1,"pad":7}]"#,
        r#"[{"op":"number","key":"TRACKNUMBER","start":1,"pad":-1}]"#,
        r#"{"op":"set","key":"TITLE","value":"x"}"#,
        r#"[]"#,
    ] {
        let v: serde_json::Value = serde_json::from_str(bad).unwrap();
        assert!(parse_ops(&v).is_err(), "{bad}");
    }
}

#[test]
fn keys_are_uppercased_and_values_normalized() {
    let t = tags(&[]);
    let out = apply_ops(&ops(r#"[{"op":"set","key":"title","value":"が"}]"#), &t, 0).unwrap();
    assert_eq!(values(&out, "TITLE"), ["\u{304c}"]);
    assert!(values(&out, "title").is_empty());
}

#[test]
fn ops_roundtrip_to_json_for_snapshot_comparison() {
    let v: serde_json::Value = serde_json::from_str(
        r#"[{"op":"set","key":"title","value":"b"},{"op":"number","key":"TRACKNUMBER","start":1}]"#,
    )
    .unwrap();
    let parsed = parse_ops(&v).unwrap();
    let back = serde_json::to_value(&parsed).unwrap();
    // 正規化（キー大文字化）を含む canonical な形になる
    assert_eq!(back[0]["key"], "TITLE");
    assert_eq!(parse_ops(&back).unwrap(), parsed);
}

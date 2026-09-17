//! スマートプレイリスト DSL のパース（docs/DSL.md、D-16、D-54）。
//! 文法・優先順位・値の引用・キーワードの大小文字・AST の JSON 形を固定する

use serde_json::json;
use spindle::playlist::dsl::{parse, Cmp, Expr, Order, OrderField, Rule};

fn where_of(src: &str) -> Expr {
    parse(src).unwrap().r#where
}

#[test]
fn single_comparison_with_bare_value() {
    let r = parse("%albumartist% IS ヰ世界情緒").unwrap();
    assert_eq!(
        r.r#where,
        Expr::Cmp {
            field: "albumartist".to_owned(),
            cmp: Cmp::Is,
            value: "ヰ世界情緒".to_owned(),
        }
    );
    assert_eq!(r.order, None);
    assert_eq!(r.limit, None);
}

#[test]
fn quoted_values_keep_spaces_and_escaped_quotes() {
    assert_eq!(
        where_of(r#"%album% IS "Angel Beats! PERFECT Vocal Collection""#),
        Expr::Cmp {
            field: "album".to_owned(),
            cmp: Cmp::Is,
            value: "Angel Beats! PERFECT Vocal Collection".to_owned(),
        }
    );
    // 引用の中の \" は " そのもの
    assert_eq!(
        where_of(r#"%title% HAS "say \"hi\"""#),
        Expr::Cmp {
            field: "title".to_owned(),
            cmp: Cmp::Has,
            value: r#"say "hi""#.to_owned(),
        }
    );
}

#[test]
fn keywords_are_case_insensitive_and_fields_normalize_spaces_and_case() {
    let r =
        parse("%Album Artist% is x and not %CATEGORY% Is y order by %Date% desc limit 5").unwrap();
    assert_eq!(
        r.r#where,
        Expr::And(vec![
            Expr::Cmp {
                field: "albumartist".to_owned(),
                cmp: Cmp::Is,
                value: "x".to_owned(),
            },
            Expr::Not(Box::new(Expr::Cmp {
                field: "category".to_owned(),
                cmp: Cmp::Is,
                value: "y".to_owned(),
            })),
        ])
    );
    assert_eq!(
        r.order,
        Some(Order {
            field: OrderField::Field("date".to_owned()),
            desc: true,
        })
    );
    assert_eq!(r.limit, Some(5));
}

#[test]
fn precedence_is_not_then_and_then_or_and_parentheses_override() {
    // a OR b AND NOT c  →  or(a, and(b, not c))
    let r = where_of("%a% IS 1 OR %b% IS 2 AND NOT %c% IS 3");
    assert_eq!(
        r,
        Expr::Or(vec![
            Expr::Cmp {
                field: "a".into(),
                cmp: Cmp::Is,
                value: "1".into()
            },
            Expr::And(vec![
                Expr::Cmp {
                    field: "b".into(),
                    cmp: Cmp::Is,
                    value: "2".into()
                },
                Expr::Not(Box::new(Expr::Cmp {
                    field: "c".into(),
                    cmp: Cmp::Is,
                    value: "3".into()
                })),
            ]),
        ])
    );
    // (a OR b) AND c
    let r = where_of("(%a% IS 1 OR %b% IS 2) AND %c% IS 3");
    assert!(matches!(r, Expr::And(ref v) if v.len() == 2 && matches!(v[0], Expr::Or(_))));
    // NOT (a AND b)
    let r = where_of("NOT (%a% IS 1 AND %b% IS 2)");
    assert!(matches!(r, Expr::Not(ref inner) if matches!(**inner, Expr::And(_))));
}

#[test]
fn all_operators_and_presence() {
    for (src, cmp) in [
        ("IS", Cmp::Is),
        ("HAS", Cmp::Has),
        ("GREATER", Cmp::Greater),
        ("LESS", Cmp::Less),
        ("MATCHES", Cmp::Matches),
    ] {
        assert_eq!(
            where_of(&format!("%x% {src} v")),
            Expr::Cmp {
                field: "x".into(),
                cmp,
                value: "v".into()
            }
        );
    }
    assert_eq!(where_of("PRESENT %genre%"), Expr::Present("genre".into()));
    assert_eq!(
        where_of("MISSING %comment%"),
        Expr::Missing("comment".into())
    );
}

#[test]
fn order_by_random_and_asc() {
    let r = parse("%lossless% IS true ORDER BY random").unwrap();
    assert_eq!(
        r.order,
        Some(Order {
            field: OrderField::Random,
            desc: false,
        })
    );
    let r = parse("%lossless% IS true ORDER BY %title% ASC LIMIT 10").unwrap();
    assert_eq!(
        r.order,
        Some(Order {
            field: OrderField::Field("title".into()),
            desc: false,
        })
    );
    assert_eq!(r.limit, Some(10));
}

#[test]
fn bare_values_stop_at_whitespace_and_closing_paren() {
    let r = where_of("(%a% IS x)AND %b% IS y");
    assert_eq!(
        r,
        Expr::And(vec![
            Expr::Cmp {
                field: "a".into(),
                cmp: Cmp::Is,
                value: "x".into()
            },
            Expr::Cmp {
                field: "b".into(),
                cmp: Cmp::Is,
                value: "y".into()
            },
        ])
    );
    // 値がキーワードと同じ綴りでも値として読む
    assert_eq!(
        where_of("%title% IS AND"),
        Expr::Cmp {
            field: "title".into(),
            cmp: Cmp::Is,
            value: "AND".into()
        }
    );
}

#[test]
fn keywords_may_touch_parentheses_and_quotes_but_not_words() {
    // `)AND` / `NOT(` / `IS"..."` は境界が明白なので通る
    let r = where_of("(%a% IS 1)AND NOT(%b% IS 2)");
    assert!(matches!(r, Expr::And(ref v) if v.len() == 2));
    assert_eq!(
        where_of("%a% IS\"x y\""),
        Expr::Cmp {
            field: "a".into(),
            cmp: Cmp::Is,
            value: "x y".into()
        }
    );
    assert_eq!(
        parse("%a% IS 1 LIMIT 9223372036854775807").unwrap().limit,
        Some(9223372036854775807)
    );
}

#[test]
fn syntax_errors_report_position() {
    for bad in [
        "",
        "%title%",
        "%title% IS",
        "title IS x",
        "%title% EQUALS x",
        "%title% IS x ORDER BY",
        "%title% IS x LIMIT ten",
        "%title% IS x LIMIT 0",
        "%title% IS x AND",
        "(%title% IS x",
        "%% IS x",
        "%title% IS \"unterminated",
        // キーワードの境界: 誤記が別の有効な式にならない
        "%title% ISLAND",
        "%title% IS x ANDROID %b% IS y",
        "%title% IS x ORDER BY randomDESC",
        "%title% IS x ORDERBY %date%",
        "NOTHING %title%",
        "%title% IS x LIMIT 5x",
        "%title% IS x LIMIT 18446744073709551615",
        "%title% IS x LIMIT 9223372036854775808",
    ] {
        let err = parse(bad).unwrap_err();
        assert!(err.line >= 1 && err.column >= 1, "{bad:?}: {err:?}");
        assert!(!err.message.is_empty(), "{bad:?}");
    }
}

#[test]
fn ast_json_shape_matches_dsl_md_and_round_trips() {
    let r = parse(
        "%albumartist% IS ヰ世界情緒 AND NOT %category% IS _Unsorted ORDER BY %date% DESC LIMIT 100",
    )
    .unwrap();
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(
        v,
        json!({
          "where": {
            "op": "and",
            "args": [
              { "op": "cmp", "field": "albumartist", "cmp": "is", "value": "ヰ世界情緒" },
              { "op": "not",
                "args": [{ "op": "cmp", "field": "category", "cmp": "is", "value": "_Unsorted" }] }
            ]
          },
          "order": { "field": "date", "dir": "desc" },
          "limit": 100
        })
    );
    let back: Rule = serde_json::from_value(v).unwrap();
    assert_eq!(back, r);
    // random と presence の形
    let r = parse("PRESENT %genre% OR MISSING %comment% ORDER BY random").unwrap();
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(
        v,
        json!({
          "where": { "op": "or", "args": [
            { "op": "present", "field": "genre" },
            { "op": "missing", "field": "comment" } ] },
          "order": { "field": "random" }
        })
    );
    assert_eq!(serde_json::from_value::<Rule>(v).unwrap(), r);
}

#[test]
fn referenced_fields_are_listed() {
    let r = parse("%a% IS 1 AND (NOT %b% HAS 2 OR PRESENT %c%) ORDER BY %d%").unwrap();
    let mut f = r.fields();
    f.sort();
    assert_eq!(f, vec!["a", "b", "c"]);
}

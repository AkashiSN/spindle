//! AST → foobar2000 Autoplaylist のクエリとソートパターン（docs/DSL.md「foobar2000 へのエクスポート」、
//! SPEC §10、D-55）。
//!
//! foobar の Autoplaylist はファイルとして保存できないので、クエリ文字列を出してユーザが貼る。
//! - フィールド名は写像表（[`field_name`]）。spindle 固有のフィールドと `MATCHES` は変換できないので
//!   その葉を落とし [`Fb2kQuery::notes`] に残す。落ちて空になった `AND` / `OR` と、子が落ちた `NOT` も落とす
//! - `PRESENT` / `MISSING` は foobar では後置。`date` の `GREATER` / `LESS` は `AFTER` / `BEFORE`
//! - `ORDER BY` は Autoplaylist のソートパターン（別欄）なので分離する。数値フィールドは文字列比較に
//!   ならないよう `$num(…, 10)` で桁を揃える。降順・`random`・`LIMIT` は表せないので notes に出す
//! - 複合式の子は常に括弧で囲み、foobar 側の優先順位に依存しない

use super::dsl::{Cmp, Expr, Order, OrderField, Rule};

/// 変換結果。`query` が空なら変換できる条件が無かった（notes にその旨が入る）
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Fb2kQuery {
    pub query: String,
    /// Autoplaylist の「ソートパターン」欄に貼る文字列
    pub sort: Option<String>,
    /// 変換できなかった指定と注意（人向け）
    pub notes: Vec<String>,
}

const NOTE_SPINDLE_ONLY: &str = "spindle 固有フィールド（foobar に無い）";
const NOTE_NO_EQUIVALENT: &str = "foobar の Autoplaylist に相当機能なし";

/// spindle のフィールド名 → foobar の `%…%`。spindle 固有なら `None`。
/// 技術情報は生の値を返す `%__…%`（`%channels%` は mono / stereo の表示文字列、`%codec%` 等も
/// 表示用に加工されうる）。長さは特殊フィールド `%length_seconds%`
pub fn field_name(field: &str) -> Option<String> {
    let name = match field {
        "albumartist" => "album artist",
        "bitdepth" => "__bitspersample",
        "duration" => "length_seconds",
        "codec" => "__codec",
        "samplerate" => "__samplerate",
        "bitrate" => "__bitrate",
        "channels" => "__channels",
        "verification" | "category" | "source_type" | "sourcetype" | "lossless" | "added"
        | "has_derived" | "hasderived" | "missing" | "hirescheck" | "hires_check" | "cutoff"
        | "cliff" | "effectivebits" | "effective_bits" => return None,
        other => other,
    };
    Some(format!("%{name}%"))
}

/// spindle の ORDER BY が数値順にするフィールド（`compile::resolve` の Int / Seconds）。
/// foobar のソートパターンは title-format の出力を文字列として比べるので、`$num` で桁を揃える
const NUMERIC_SORT_FIELDS: &[&str] = &[
    "tracknumber",
    "discnumber",
    "samplerate",
    "bitrate",
    "channels",
    "bitdepth",
    "duration",
];

/// `$num` のゼロ埋め桁数。samplerate（6 桁）や秒数（7 桁）に足りればよい
const NUM_WIDTH: u32 = 10;

/// ソートパターン用。数値フィールドは `$num(%…%,10)`、spindle 固有なら `None`
pub fn sort_pattern(field: &str) -> Option<String> {
    let name = field_name(field)?;
    if NUMERIC_SORT_FIELDS.contains(&field) {
        Some(format!("$num({name},{NUM_WIDTH})"))
    } else {
        Some(name)
    }
}

/// foobar のクエリで裸のまま置けない語（大小文字無視）
const KEYWORDS: &[&str] = &[
    "AND", "OR", "NOT", "IS", "HAS", "GREATER", "LESS", "EQUAL", "PRESENT", "MISSING", "BEFORE",
    "AFTER", "SINCE", "DURING", "MATCHES", "SORT", "BY",
];

fn needs_quotes(value: &str) -> bool {
    value.is_empty()
        || value.contains(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '"'))
        || KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(value))
}

fn quote(value: &str) -> String {
    if needs_quotes(value) {
        format!("\"{value}\"")
    } else {
        value.to_owned()
    }
}

/// 葉を spindle の記法のまま文字にする（notes 用）
fn leaf_source(e: &Expr) -> String {
    match e {
        Expr::Cmp { field, cmp, value } => {
            format!("%{field}% {} {}", cmp_keyword(*cmp), quote(value))
        }
        Expr::Present(f) => format!("PRESENT %{f}%"),
        Expr::Missing(f) => format!("MISSING %{f}%"),
        Expr::And(_) | Expr::Or(_) | Expr::Not(_) => String::new(),
    }
}

fn cmp_keyword(cmp: Cmp) -> &'static str {
    match cmp {
        Cmp::Is => "IS",
        Cmp::Has => "HAS",
        Cmp::Greater => "GREATER",
        Cmp::Less => "LESS",
        Cmp::Matches => "MATCHES",
    }
}

/// 変換した式。`compound` は括弧が要る（`AND` / `OR`）
struct Rendered {
    text: String,
    compound: bool,
}

/// `negated` は直接の親が `NOT`（落とすときの note に `NOT` を付ける）
fn render(e: &Expr, negated: bool, notes: &mut Vec<String>) -> Option<Rendered> {
    let drop = |notes: &mut Vec<String>, e: &Expr, why: &str| {
        let prefix = if negated { "NOT " } else { "" };
        notes.push(format!("{prefix}{} … {why}", leaf_source(e)));
        None
    };
    match e {
        Expr::And(v) | Expr::Or(v) => {
            let joiner = if matches!(e, Expr::And(_)) {
                " AND "
            } else {
                " OR "
            };
            let parts: Vec<String> = v
                .iter()
                .filter_map(|x| render(x, false, notes))
                .map(|r| {
                    if r.compound {
                        format!("({})", r.text)
                    } else {
                        r.text
                    }
                })
                .collect();
            match parts.len() {
                0 => None,
                1 => Some(Rendered {
                    text: parts.into_iter().next().unwrap_or_default(),
                    compound: false,
                }),
                _ => Some(Rendered {
                    text: parts.join(joiner),
                    compound: true,
                }),
            }
        }
        Expr::Not(x) => {
            let child_is_leaf = !matches!(**x, Expr::And(_) | Expr::Or(_) | Expr::Not(_));
            let r = render(x, child_is_leaf, notes)?;
            let text = if r.compound {
                format!("NOT ({})", r.text)
            } else {
                format!("NOT {}", r.text)
            };
            Some(Rendered {
                text,
                compound: false,
            })
        }
        Expr::Cmp { field, cmp, value } => {
            if *cmp == Cmp::Matches {
                return drop(notes, e, "MATCHES は spindle の独自拡張（foobar に無い）");
            }
            let Some(name) = field_name(field) else {
                return drop(notes, e, NOTE_SPINDLE_ONLY);
            };
            let op = match (cmp, field.as_str()) {
                (Cmp::Greater, "date") => "AFTER",
                (Cmp::Less, "date") => "BEFORE",
                (c, _) => cmp_keyword(*c),
            };
            if value.contains('"') {
                notes.push(format!(
                    "{name} {op} {} … 値の \" は foobar 側でエスケープできない",
                    quote(value)
                ));
            }
            Some(Rendered {
                text: format!("{name} {op} {}", quote(value)),
                compound: false,
            })
        }
        Expr::Present(f) | Expr::Missing(f) => {
            if f == "duration" {
                // `%length_seconds%` は技術情報でなく特殊フィールドなので有無を問えない
                return drop(notes, e, "foobar では長さの有無を問えない");
            }
            let Some(name) = field_name(f) else {
                return drop(notes, e, NOTE_SPINDLE_ONLY);
            };
            let kw = if matches!(e, Expr::Present(_)) {
                "PRESENT"
            } else {
                "MISSING"
            };
            Some(Rendered {
                text: format!("{name} {kw}"),
                compound: false,
            })
        }
    }
}

fn render_order(order: &Order, notes: &mut Vec<String>) -> Option<String> {
    let dir = if order.desc { " DESC" } else { "" };
    match &order.field {
        OrderField::Random => {
            notes.push(format!("ORDER BY random{dir} … {NOTE_NO_EQUIVALENT}"));
            None
        }
        OrderField::Field(f) => {
            let Some(name) = sort_pattern(f) else {
                notes.push(format!("ORDER BY %{f}%{dir} … {NOTE_SPINDLE_ONLY}"));
                return None;
            };
            if order.desc {
                notes.push(format!(
                    "ORDER BY %{f}% DESC … ソートパターンでは降順を表せない（foobar 側で並びを反転する）"
                ));
            }
            Some(name)
        }
    }
}

pub fn convert(rule: &Rule) -> Fb2kQuery {
    let mut notes = Vec::new();
    let query = match render(&rule.r#where, false, &mut notes) {
        Some(r) => r.text,
        None => {
            notes.push("変換できる条件が無く、クエリは空（全曲に一致する）".to_owned());
            String::new()
        }
    };
    let sort = rule
        .order
        .as_ref()
        .and_then(|o| render_order(o, &mut notes));
    if let Some(n) = rule.limit {
        notes.push(format!("LIMIT {n} … {NOTE_NO_EQUIVALENT}"));
    }
    Fb2kQuery { query, sort, notes }
}

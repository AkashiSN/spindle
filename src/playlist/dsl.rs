//! スマートプレイリスト DSL のパース（docs/DSL.md、D-16、D-54）。
//!
//! `pest` で `dsl.pest` の文法を読み、[`Rule`]（AST）にする。AST は DSL.md の JSON 形で
//! `playlists.rule_ast` に保存し、実行時に `playlist::compile` がパラメータ化 SQL へ変換する。
//! 原文は `rule_source` に別途残す（AST から逆生成すると整形が変わる）

use pest::iterators::Pair;
use pest::Parser;
use serde::{Deserialize, Serialize};

/// pest が生成する `Rule` 列挙が AST の [`Rule`] と衝突しないよう別モジュールに置く
mod grammar {
    use pest_derive::Parser;

    #[derive(Parser)]
    #[grammar = "playlist/dsl.pest"]
    pub struct DslParser;
}

use grammar::DslParser;
type GrammarRule = grammar::Rule;

/// 比較演算子
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cmp {
    Is,
    Has,
    Greater,
    Less,
    Matches,
}

/// WHERE 句の木。フィールド名は正規化済み（空白除去 + 小文字）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Cmp {
        field: String,
        cmp: Cmp,
        value: String,
    },
    Present(String),
    Missing(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderField {
    Field(String),
    Random,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub field: OrderField,
    pub desc: bool,
}

/// パース済みのルール（`rule_ast` の形）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub r#where: Expr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<Order>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

impl Rule {
    /// WHERE 句が参照するフィールド名（重複あり。ORDER BY は含まない）
    pub fn fields(&self) -> Vec<&str> {
        fn walk<'a>(e: &'a Expr, out: &mut Vec<&'a str>) {
            match e {
                Expr::And(v) | Expr::Or(v) => v.iter().for_each(|x| walk(x, out)),
                Expr::Not(x) => walk(x, out),
                Expr::Cmp { field, .. } | Expr::Present(field) | Expr::Missing(field) => {
                    out.push(field)
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.r#where, &mut out);
        out
    }
}

/// 構文エラー（1 始まりの行・桁）
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{line} 行 {column} 桁: {message}")]
pub struct ParseError {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

/// フィールド名の正規化: 空白を除き小文字に（`%Album Artist%` → `albumartist`）
pub fn normalize_field(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

pub fn parse(src: &str) -> Result<Rule, ParseError> {
    let mut pairs = DslParser::parse(GrammarRule::query, src).map_err(|e| {
        let (line, column) = match e.line_col {
            pest::error::LineColLocation::Pos((l, c)) => (l, c),
            pest::error::LineColLocation::Span((l, c), _) => (l, c),
        };
        ParseError {
            line,
            column,
            message: e.variant.message().into_owned(),
        }
    })?;
    let query = pairs.next().ok_or_else(|| ParseError {
        line: 1,
        column: 1,
        message: "空のルール".to_owned(),
    })?;
    let mut r#where = None;
    let mut order = None;
    let mut limit = None;
    for p in query.into_inner() {
        match p.as_rule() {
            GrammarRule::expr => r#where = Some(build_expr(p)),
            GrammarRule::order => order = Some(build_order(p)),
            GrammarRule::limit => {
                let (line, column) = p.line_col();
                // SQLite の LIMIT は i64。それを超える値は無制限（-1）に化けるので弾く
                let n: u64 = p
                    .into_inner()
                    .find(|i| i.as_rule() == GrammarRule::integer)
                    .and_then(|i| i.as_str().parse::<i64>().ok())
                    .and_then(|i| u64::try_from(i).ok())
                    .ok_or_else(|| ParseError {
                        line,
                        column,
                        message: "LIMIT は 1 以上 9223372036854775807 以下の整数".to_owned(),
                    })?;
                if n == 0 {
                    return Err(ParseError {
                        line,
                        column,
                        message: "LIMIT は 1 以上".to_owned(),
                    });
                }
                limit = Some(n);
            }
            GrammarRule::EOI => {}
            _ => {}
        }
    }
    Ok(Rule {
        r#where: r#where.ok_or_else(|| ParseError {
            line: 1,
            column: 1,
            message: "条件が無い".to_owned(),
        })?,
        order,
        limit,
    })
}

fn build_expr(p: Pair<GrammarRule>) -> Expr {
    debug_assert_eq!(p.as_rule(), GrammarRule::expr);
    let terms: Vec<Expr> = p
        .into_inner()
        .filter(|x| x.as_rule() == GrammarRule::term)
        .map(build_term)
        .collect();
    flatten(terms, Expr::Or)
}

fn build_term(p: Pair<GrammarRule>) -> Expr {
    let factors: Vec<Expr> = p
        .into_inner()
        .filter(|x| x.as_rule() == GrammarRule::factor)
        .map(build_factor)
        .collect();
    flatten(factors, Expr::And)
}

fn flatten(mut v: Vec<Expr>, make: fn(Vec<Expr>) -> Expr) -> Expr {
    if v.len() == 1 {
        v.pop().unwrap_or(Expr::And(Vec::new()))
    } else {
        make(v)
    }
}

fn build_factor(p: Pair<GrammarRule>) -> Expr {
    let mut negate = false;
    let mut inner = None;
    for x in p.into_inner() {
        match x.as_rule() {
            GrammarRule::not_op => negate = true,
            GrammarRule::group => {
                inner = x
                    .into_inner()
                    .find(|y| y.as_rule() == GrammarRule::expr)
                    .map(build_expr)
            }
            GrammarRule::compare => inner = Some(build_compare(x)),
            GrammarRule::presence => inner = Some(build_presence(x)),
            _ => {}
        }
    }
    let e = inner.unwrap_or(Expr::And(Vec::new()));
    if negate {
        Expr::Not(Box::new(e))
    } else {
        e
    }
}

fn field_name(p: Pair<GrammarRule>) -> String {
    // `%ident%` の両端を落として正規化
    let s = p.as_str();
    normalize_field(s.trim_matches('%'))
}

fn build_compare(p: Pair<GrammarRule>) -> Expr {
    let mut field = String::new();
    let mut cmp = Cmp::Is;
    let mut value = String::new();
    for x in p.into_inner() {
        match x.as_rule() {
            GrammarRule::field => field = field_name(x),
            GrammarRule::op => {
                cmp = match x.into_inner().next().map(|o| o.as_rule()) {
                    Some(GrammarRule::has_op) => Cmp::Has,
                    Some(GrammarRule::greater_op) => Cmp::Greater,
                    Some(GrammarRule::less_op) => Cmp::Less,
                    Some(GrammarRule::matches_op) => Cmp::Matches,
                    _ => Cmp::Is,
                }
            }
            GrammarRule::value => value = build_value(x),
            _ => {}
        }
    }
    Expr::Cmp { field, cmp, value }
}

fn build_value(p: Pair<GrammarRule>) -> String {
    match p.into_inner().next() {
        Some(x) if x.as_rule() == GrammarRule::quoted => unescape(x.as_str()),
        Some(x) => x.as_str().to_owned(),
        None => String::new(),
    }
}

/// `"..."` の両端を落とし、`\"` → `"`、`\\` → `\`。他の `\x` はそのまま
fn unescape(quoted: &str) -> String {
    let inner = &quoted[1..quoted.len().saturating_sub(1)];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('"') => {
                    out.push('"');
                    chars.next();
                }
                Some('\\') => {
                    out.push('\\');
                    chars.next();
                }
                _ => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn build_presence(p: Pair<GrammarRule>) -> Expr {
    let mut present = true;
    let mut field = String::new();
    for x in p.into_inner() {
        match x.as_rule() {
            GrammarRule::present_op => present = true,
            GrammarRule::missing_op => present = false,
            GrammarRule::field => field = field_name(x),
            _ => {}
        }
    }
    if present {
        Expr::Present(field)
    } else {
        Expr::Missing(field)
    }
}

fn build_order(p: Pair<GrammarRule>) -> Order {
    let mut field = OrderField::Random;
    let mut desc = false;
    for x in p.into_inner() {
        match x.as_rule() {
            GrammarRule::field => field = OrderField::Field(field_name(x)),
            GrammarRule::random => field = OrderField::Random,
            GrammarRule::desc => desc = true,
            GrammarRule::asc => desc = false,
            _ => {}
        }
    }
    Order { field, desc }
}

// ---------------------------------------------------------------- JSON 形（DSL.md）

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum ExprWire {
    And {
        args: Vec<ExprWire>,
    },
    Or {
        args: Vec<ExprWire>,
    },
    Not {
        args: Vec<ExprWire>,
    },
    Cmp {
        field: String,
        cmp: Cmp,
        value: String,
    },
    Present {
        field: String,
    },
    Missing {
        field: String,
    },
}

impl From<&Expr> for ExprWire {
    fn from(e: &Expr) -> Self {
        match e {
            Expr::And(v) => ExprWire::And {
                args: v.iter().map(ExprWire::from).collect(),
            },
            Expr::Or(v) => ExprWire::Or {
                args: v.iter().map(ExprWire::from).collect(),
            },
            Expr::Not(x) => ExprWire::Not {
                args: vec![ExprWire::from(&**x)],
            },
            Expr::Cmp { field, cmp, value } => ExprWire::Cmp {
                field: field.clone(),
                cmp: *cmp,
                value: value.clone(),
            },
            Expr::Present(f) => ExprWire::Present { field: f.clone() },
            Expr::Missing(f) => ExprWire::Missing { field: f.clone() },
        }
    }
}

impl TryFrom<ExprWire> for Expr {
    type Error = String;
    fn try_from(w: ExprWire) -> Result<Self, String> {
        Ok(match w {
            ExprWire::And { args } => Expr::And(
                args.into_iter()
                    .map(Expr::try_from)
                    .collect::<Result<_, _>>()?,
            ),
            ExprWire::Or { args } => Expr::Or(
                args.into_iter()
                    .map(Expr::try_from)
                    .collect::<Result<_, _>>()?,
            ),
            ExprWire::Not { mut args } => {
                if args.len() != 1 {
                    return Err("not の args は 1 つ".to_owned());
                }
                Expr::Not(Box::new(Expr::try_from(args.remove(0))?))
            }
            ExprWire::Cmp { field, cmp, value } => Expr::Cmp {
                field: normalize_field(&field),
                cmp,
                value,
            },
            ExprWire::Present { field } => Expr::Present(normalize_field(&field)),
            ExprWire::Missing { field } => Expr::Missing(normalize_field(&field)),
        })
    }
}

impl Serialize for Expr {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        ExprWire::from(self).serialize(s)
    }
}

impl<'de> Deserialize<'de> for Expr {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let w = ExprWire::deserialize(d)?;
        Expr::try_from(w).map_err(serde::de::Error::custom)
    }
}

#[derive(Serialize, Deserialize)]
struct OrderWire {
    field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dir: Option<String>,
}

impl Serialize for Order {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let w = match &self.field {
            OrderField::Random => OrderWire {
                field: "random".to_owned(),
                dir: None,
            },
            OrderField::Field(f) => OrderWire {
                field: f.clone(),
                dir: Some(if self.desc { "desc" } else { "asc" }.to_owned()),
            },
        };
        w.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Order {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let w = OrderWire::deserialize(d)?;
        let desc = match w.dir.as_deref() {
            None | Some("asc") => false,
            Some("desc") => true,
            Some(other) => return Err(serde::de::Error::custom(format!("dir が不正: {other}"))),
        };
        Ok(if w.field.eq_ignore_ascii_case("random") {
            Order {
                field: OrderField::Random,
                desc: false,
            }
        } else {
            Order {
                field: OrderField::Field(normalize_field(&w.field)),
                desc,
            }
        })
    }
}

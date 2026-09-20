//! AST → パラメータ化 SQL（docs/DSL.md「SQL 生成」、D-54）。
//!
//! - 列名はここのホワイトリスト（[`resolve`]）でだけ解決する。AST のフィールド名を SQL に埋めない
//! - 値はすべてバインドパラメータ
//! - キャッシュ列と拡張フィールド以外のタグは `EXISTS (SELECT 1 FROM track_tags …)`（キーは大文字）
//! - `MATCHES` は全コネクションに登録した `regexp(pattern, text)`（[`register_regexp`]）
//! - `missing` を参照しない限り `t.missing_since IS NULL` を暗黙に付ける
//!
//! 型に合わない演算子・値は [`check`] / [`compile`] がエラーにする（実行前に 400 にできる）

use std::cell::RefCell;
use std::collections::HashMap;

use rusqlite::types::Value;
use rusqlite::Connection;

use crate::domain::filter::like_pattern;

use super::dsl::{Cmp, Expr, Order, OrderField, Rule};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompileError {
    #[error("フィールド %{field}% に {what} は使えない")]
    Field { field: String, what: String },
    #[error("フィールド %{field}% の値 {value:?} が不正: {what}")]
    Value {
        field: String,
        value: String,
        what: String,
    },
}

/// フィールドの種別（ホワイトリスト）
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// 文字列の列・式
    Text(&'static str),
    /// 整数の列
    Int(&'static str),
    /// 実数の列（IS は等値、GREATER / LESS は数値比較）
    Float(&'static str),
    /// 秒で比較する ms の列
    Seconds(&'static str),
    /// 真偽の式（真のとき成立する SQL）
    Bool(&'static str),
    /// UNIX epoch 秒の列（日付として比較）
    Epoch(&'static str),
    /// 任意タグ（track_tags のキー）
    Tag(String),
}

fn resolve(field: &str) -> Kind {
    match field {
        "title" => Kind::Text("t.title"),
        "artist" => Kind::Text("t.artist_display"),
        "album" => Kind::Text("t.album"),
        "albumartist" => Kind::Text("t.albumartist"),
        "date" => Kind::Text("t.date"),
        "tracknumber" => Kind::Int("t.track_no"),
        "discnumber" => Kind::Int("t.disc_no"),
        "category" => Kind::Text(
            "(SELECT c.name FROM albums a JOIN categories c ON c.id = a.category_id WHERE a.id = t.album_id)",
        ),
        "verification" => Kind::Text("t.verification"),
        "source_type" | "sourcetype" => Kind::Text("t.source_type"),
        "codec" => Kind::Text("t.codec"),
        "lossless" => Kind::Bool("t.lossless = 1"),
        "has_derived" | "hasderived" => {
            // opus 系統（配布ビュー）の有無。aac だけのトラックは含めない（SPEC §7.6、D-75）
            Kind::Bool(
                "EXISTS (SELECT 1 FROM derived_files d WHERE d.track_id = t.id AND d.variant = 'opus')",
            )
        }
        "missing" => Kind::Bool("t.missing_since IS NOT NULL"),
        "samplerate" => Kind::Int("t.sample_rate"),
        "bitdepth" => Kind::Int("t.bit_depth"),
        "channels" => Kind::Int("t.channels"),
        "bitrate" => Kind::Int("t.bitrate"),
        "duration" => Kind::Seconds("t.duration_ms"),
        "hirescheck" | "hires_check" => Kind::Text("t.hires_check"),
        "cutoff" => Kind::Int("t.hires_cutoff_hz"),
        "cliff" => Kind::Float("t.hires_cliff_db"),
        "effectivebits" | "effective_bits" => Kind::Int("t.hires_effective_bits"),
        "added" => Kind::Epoch("t.added_at"),
        other => Kind::Tag(other.to_ascii_uppercase()),
    }
}

/// WHERE 句の断片とパラメータ
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Fragment {
    pub sql: String,
    pub params: Vec<Value>,
}

/// ルール全体のコンパイル結果
#[derive(Debug, Clone, PartialEq)]
pub struct Compiled {
    pub r#where: Fragment,
    /// `ORDER BY` の式（`t.id` のタイブレーク込み）
    pub order: Fragment,
    pub limit: Option<u64>,
}

/// 構文木の型検査だけを行う（保存前の検証）
pub fn check(rule: &Rule) -> Result<(), CompileError> {
    compile(rule).map(|_| ())
}

pub fn compile(rule: &Rule) -> Result<Compiled, CompileError> {
    Ok(Compiled {
        r#where: where_clause(rule)?,
        order: order_clause(rule.order.as_ref())?,
        limit: rule.limit,
    })
}

/// WHERE 句だけ（一覧の `filter.dsl` 用）。`missing` を参照しなければ active 限定を足す
pub fn where_clause(rule: &Rule) -> Result<Fragment, CompileError> {
    let mut f = Fragment::default();
    f.sql.push('(');
    expr(&rule.r#where, &mut f)?;
    f.sql.push(')');
    if !rule.fields().contains(&"missing") {
        f.sql.push_str(" AND t.missing_since IS NULL");
    }
    Ok(f)
}

fn expr(e: &Expr, f: &mut Fragment) -> Result<(), CompileError> {
    match e {
        Expr::And(v) | Expr::Or(v) => {
            let joiner = if matches!(e, Expr::And(_)) {
                " AND "
            } else {
                " OR "
            };
            if v.is_empty() {
                f.sql.push('1');
                return Ok(());
            }
            f.sql.push('(');
            for (i, x) in v.iter().enumerate() {
                if i > 0 {
                    f.sql.push_str(joiner);
                }
                expr(x, f)?;
            }
            f.sql.push(')');
        }
        Expr::Not(x) => {
            // NOT の中で NULL が出ると全体が NULL（偽）になる。coalesce で「成立しない」を偽に固定
            f.sql.push_str("NOT coalesce(");
            expr(x, f)?;
            f.sql.push_str(", 0)");
        }
        Expr::Cmp { field, cmp, value } => compare(field, *cmp, value, f)?,
        Expr::Present(field) => presence(field, true, f),
        Expr::Missing(field) => presence(field, false, f),
    }
    Ok(())
}

fn field_err(field: &str, what: &str) -> CompileError {
    CompileError::Field {
        field: field.to_owned(),
        what: what.to_owned(),
    }
}

fn value_err(field: &str, value: &str, what: &str) -> CompileError {
    CompileError::Value {
        field: field.to_owned(),
        value: value.to_owned(),
        what: what.to_owned(),
    }
}

fn cmp_name(c: Cmp) -> &'static str {
    match c {
        Cmp::Is => "IS",
        Cmp::Has => "HAS",
        Cmp::Greater => "GREATER",
        Cmp::Less => "LESS",
        Cmp::Matches => "MATCHES",
    }
}

fn parse_bool(field: &str, value: &str) -> Result<bool, CompileError> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(value_err(field, value, "真偽値（true / false）でない")),
    }
}

fn parse_int(field: &str, value: &str) -> Result<i64, CompileError> {
    value
        .trim()
        .parse()
        .map_err(|_| value_err(field, value, "整数でない（数値のフィールド）"))
}

fn parse_float(field: &str, value: &str) -> Result<f64, CompileError> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| value_err(field, value, "数値でない（数値のフィールド）"))
}

/// `YYYY-MM-DD`（UTC 0 時。暦日として妥当なもの、年は 1..=9999）か epoch 秒
fn parse_epoch(field: &str, value: &str) -> Result<i64, CompileError> {
    let v = value.trim();
    if let Ok(n) = v.parse::<i64>() {
        return Ok(n);
    }
    let bad = || value_err(field, value, "日付（YYYY-MM-DD か epoch 秒）でない");
    let mut it = v.split('-');
    let (y, m, d) = (
        it.next().and_then(|x| x.parse::<i64>().ok()),
        it.next().and_then(|x| x.parse::<i64>().ok()),
        it.next().and_then(|x| x.parse::<i64>().ok()),
    );
    let (Some(y), Some(m), Some(d), None) = (y, m, d, it.next()) else {
        return Err(bad());
    };
    if !(1..=9999).contains(&y) || !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return Err(bad());
    }
    days_from_civil(y, m, d).checked_mul(86_400).ok_or_else(bad)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

/// 暦日 → 1970-01-01 からの日数（Howard Hinnant の公式）
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn check_regex(field: &str, value: &str) -> Result<(), CompileError> {
    fancy_regex::Regex::new(value)
        .map(|_| ())
        .map_err(|e| value_err(field, value, &format!("正規表現として不正: {e}")))
}

/// 文字列式に対する比較（列でもタグの値でも同じ）
fn text_cmp(
    field: &str,
    col: &str,
    cmp: Cmp,
    value: &str,
    f: &mut Fragment,
) -> Result<(), CompileError> {
    match cmp {
        Cmp::Is => {
            f.sql.push_str(&format!("{col} = ? COLLATE NOCASE"));
            f.params.push(Value::from(value.to_owned()));
        }
        Cmp::Has => {
            f.sql.push_str(&format!("{col} LIKE ? ESCAPE '\\'"));
            f.params.push(Value::from(like_pattern(value)));
        }
        Cmp::Matches => {
            check_regex(field, value)?;
            f.sql.push_str(&format!("regexp(?, {col})"));
            f.params.push(Value::from(value.to_owned()));
        }
        Cmp::Greater | Cmp::Less => {
            // 文字列で大小比較できるのは date だけ（YYYY-MM-DD の辞書順）
            if field != "date" {
                return Err(field_err(field, cmp_name(cmp)));
            }
            let op = if cmp == Cmp::Greater { ">" } else { "<" };
            f.sql.push_str(&format!("{col} {op} ?"));
            f.params.push(Value::from(value.to_owned()));
        }
    }
    Ok(())
}

fn compare(field: &str, cmp: Cmp, value: &str, f: &mut Fragment) -> Result<(), CompileError> {
    match resolve(field) {
        Kind::Text(col) => text_cmp(field, col, cmp, value, f),
        Kind::Int(col) => match cmp {
            Cmp::Is | Cmp::Greater | Cmp::Less => {
                let n = parse_int(field, value)?;
                let op = match cmp {
                    Cmp::Is => "=",
                    Cmp::Greater => ">",
                    _ => "<",
                };
                f.sql.push_str(&format!("{col} {op} ?"));
                f.params.push(Value::from(n));
                Ok(())
            }
            Cmp::Has | Cmp::Matches => {
                text_cmp(field, &format!("CAST({col} AS TEXT)"), cmp, value, f)
            }
        },
        Kind::Float(col) => match cmp {
            Cmp::Is | Cmp::Greater | Cmp::Less => {
                let v = parse_float(field, value)?;
                let op = match cmp {
                    Cmp::Is => "=",
                    Cmp::Greater => ">",
                    _ => "<",
                };
                f.sql.push_str(&format!("{col} {op} ?"));
                f.params.push(Value::from(v));
                Ok(())
            }
            Cmp::Has | Cmp::Matches => Err(field_err(field, cmp_name(cmp))),
        },
        Kind::Seconds(col) => match cmp {
            Cmp::Is | Cmp::Greater | Cmp::Less => {
                let secs = parse_float(field, value)?;
                let op = match cmp {
                    Cmp::Is => "=",
                    Cmp::Greater => ">",
                    _ => "<",
                };
                // 秒 → ms。IS は丸めた秒で比べる
                if cmp == Cmp::Is {
                    f.sql.push_str(&format!("round({col} / 1000.0) = ?"));
                    f.params.push(Value::from(secs.round()));
                } else {
                    f.sql.push_str(&format!("{col} {op} ?"));
                    f.params.push(Value::from(secs * 1000.0));
                }
                Ok(())
            }
            Cmp::Has | Cmp::Matches => Err(field_err(field, cmp_name(cmp))),
        },
        Kind::Bool(sql) => match cmp {
            Cmp::Is => {
                let b = parse_bool(field, value)?;
                if b {
                    f.sql.push_str(&format!("({sql})"));
                } else {
                    f.sql.push_str(&format!("NOT ({sql})"));
                }
                Ok(())
            }
            _ => Err(field_err(
                field,
                &format!(
                    "{}（真偽のフィールドは IS true / IS false だけ）",
                    cmp_name(cmp)
                ),
            )),
        },
        Kind::Epoch(col) => match cmp {
            Cmp::Is => {
                // 同じ日（UTC）
                let start = parse_epoch(field, value)?;
                let out_of_range = || value_err(field, value, "epoch 秒が範囲外");
                let day = if value.trim().parse::<i64>().is_ok() {
                    start
                        .checked_sub(start.rem_euclid(86_400))
                        .ok_or_else(out_of_range)?
                } else {
                    start
                };
                let end = day.checked_add(86_400).ok_or_else(out_of_range)?;
                f.sql.push_str(&format!("({col} >= ? AND {col} < ?)"));
                f.params.push(Value::from(day));
                f.params.push(Value::from(end));
                Ok(())
            }
            Cmp::Greater | Cmp::Less => {
                let n = parse_epoch(field, value)?;
                // GREATER 2023-01-01 はその日の終わりより後、LESS はその日の始まりより前
                if cmp == Cmp::Greater {
                    let end = if value.trim().parse::<i64>().is_ok() {
                        n
                    } else {
                        n.checked_add(86_400 - 1)
                            .ok_or_else(|| value_err(field, value, "epoch 秒が範囲外"))?
                    };
                    f.sql.push_str(&format!("{col} > ?"));
                    f.params.push(Value::from(end));
                } else {
                    f.sql.push_str(&format!("{col} < ?"));
                    f.params.push(Value::from(n));
                }
                Ok(())
            }
            Cmp::Has | Cmp::Matches => {
                text_cmp(field, &format!("date({col}, 'unixepoch')"), cmp, value, f)
            }
        },
        Kind::Tag(key) => match cmp {
            Cmp::Greater | Cmp::Less => Err(field_err(
                field,
                &format!("{}（数値のフィールドでない）", cmp_name(cmp)),
            )),
            _ => {
                f.sql.push_str(
                    "EXISTS (SELECT 1 FROM track_tags tt WHERE tt.track_id = t.id AND tt.key = ? AND ",
                );
                f.params.push(Value::from(key));
                text_cmp(field, "tt.value", cmp, value, f)?;
                f.sql.push(')');
                Ok(())
            }
        },
    }
}

fn presence(field: &str, present: bool, f: &mut Fragment) {
    let sql = match resolve(field) {
        Kind::Text(col) => format!("({col} IS NOT NULL AND {col} <> '')"),
        Kind::Int(col) | Kind::Float(col) | Kind::Seconds(col) | Kind::Epoch(col) => {
            format!("{col} IS NOT NULL")
        }
        // 真偽のフィールドは常に値を持つ
        Kind::Bool(_) => "1".to_owned(),
        Kind::Tag(key) => {
            f.params.push(Value::from(key));
            "EXISTS (SELECT 1 FROM track_tags tt WHERE tt.track_id = t.id AND tt.key = ?)"
                .to_owned()
        }
    };
    if present {
        f.sql.push_str(&sql);
    } else {
        f.sql.push_str(&format!("NOT {sql}"));
    }
}

/// ORDER BY の式。無ければ id 順。NULL は末尾（昇順でも降順でも）
fn order_clause(order: Option<&Order>) -> Result<Fragment, CompileError> {
    let mut f = Fragment::default();
    let Some(o) = order else {
        f.sql.push_str("t.id");
        return Ok(f);
    };
    let dir = if o.desc { "DESC" } else { "ASC" };
    match &o.field {
        OrderField::Random => f.sql.push_str("RANDOM()"),
        OrderField::Field(field) => {
            // 文字列は大小文字を無視して並べる（IS と同じ NOCASE）
            let (expr, collate) = match resolve(field) {
                Kind::Text(col) => (col.to_owned(), " COLLATE NOCASE"),
                Kind::Int(col) | Kind::Float(col) | Kind::Seconds(col) | Kind::Epoch(col) => {
                    (col.to_owned(), "")
                }
                Kind::Bool(sql) => (format!("({sql})"), ""),
                Kind::Tag(key) => {
                    // 式は 2 回（IS NULL と値）出るのでキーも 2 回バインドする
                    f.params.push(Value::from(key.clone()));
                    f.params.push(Value::from(key));
                    (
                        "(SELECT tt.value FROM track_tags tt WHERE tt.track_id = t.id AND tt.key = ? ORDER BY tt.idx LIMIT 1)"
                            .to_owned(),
                        " COLLATE NOCASE",
                    )
                }
            };
            f.sql.push_str(&format!(
                "({expr}) IS NULL, ({expr}){collate} {dir}, t.id {dir}"
            ));
        }
    }
    Ok(f)
}

/// ルールを評価してトラック id を並び順で返す
pub fn evaluate(conn: &Connection, rule: &Rule) -> Result<Vec<i64>, EvalError> {
    let c = compile(rule)?;
    let sql = format!(
        "SELECT t.id FROM tracks t WHERE {} ORDER BY {}{}",
        c.r#where.sql,
        c.order.sql,
        c.limit.map(|_| " LIMIT ?").unwrap_or("")
    );
    let mut params: Vec<Value> = c.r#where.params;
    params.extend(c.order.params);
    if let Some(n) = c.limit {
        params.push(Value::from(n as i64));
    }
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map(rusqlite::params_from_iter(params), |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<i64>>>()?)
}

#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error(transparent)]
    Compile(#[from] CompileError),
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

// ---------------------------------------------------------------- regexp

thread_local! {
    /// コネクション（スレッド）ごとのコンパイル済み正規表現。同じパターンを全行に当てるので効く
    static REGEX_CACHE: RefCell<HashMap<String, fancy_regex::Regex>> = RefCell::new(HashMap::new());
}

/// キャッシュに置く上限。超えたら空にする（ルール数は高々数十）
const REGEX_CACHE_MAX: usize = 64;

/// `regexp()` が返すエラーメッセージの接頭辞。SQLite はユーザ定義関数のエラーを
/// `SqliteFailure` の文字列としてしか呼び出し側に伝えないので、これでルール由来の失敗を見分ける
/// （[`is_regexp_failure`]）
pub const REGEXP_ERROR_PREFIX: &str = "regexp: ";

/// `regexp()` 由来の実行時エラー（バックトラック上限・不正なパターン）か
pub fn is_regexp_failure(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(_, Some(msg)) if msg.starts_with(REGEXP_ERROR_PREFIX))
}

fn regexp_error(e: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::UserFunctionError(format!("{REGEXP_ERROR_PREFIX}{e}").into())
}

/// `regexp(pattern, text)` を登録する。`text` が NULL なら NULL（比較不成立）。
/// パターンは fancy-regex（後方参照・先読み可）。不正なパターンは SQL エラー
pub fn register_regexp(conn: &Connection) -> rusqlite::Result<()> {
    use rusqlite::functions::FunctionFlags;
    conn.create_scalar_function(
        "regexp",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let pattern: String = ctx.get(0)?;
            let text: Option<String> = ctx.get(1)?;
            let Some(text) = text else {
                return Ok(None);
            };
            let matched = REGEX_CACHE.with(|cache| -> Result<bool, rusqlite::Error> {
                let mut cache = cache.borrow_mut();
                if cache.len() >= REGEX_CACHE_MAX {
                    cache.clear();
                }
                let re = match cache.entry(pattern) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        let re = fancy_regex::Regex::new(e.key()).map_err(regexp_error)?;
                        e.insert(re)
                    }
                };
                re.is_match(&text).map_err(regexp_error)
            })?;
            Ok(Some(matched))
        },
    )
}

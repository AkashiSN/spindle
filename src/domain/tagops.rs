//! 一括編集の操作（SPEC §12.3、D-42）: 固定値代入 / フィールド参照 / 正規表現置換 / 連番 / 削除 /
//! 行ごとの固定値（`set_rows`。API 専用、P4-14）。
//!
//! JSON の配列を [`parse_ops`] で解釈し、[`apply_ops`] で正規化タグ集合へ上から順に適用する。
//! 各操作は前の操作の結果を見る（`set ARTIST` → `ref ALBUMARTIST %artist%` で新しい ARTIST が
//! 入る）。評価は純粋で DB もファイルも触らない。preview と apply が同じ関数を通るので、
//! apply 時に再評価しても preview と同じ結果になる（同じ `tag_version` なら同じタグ集合）。
//!
//! 正規表現は `fancy-regex`（後方参照・先読みが使える）。バックトラックの上限を設けて
//! ユーザ入力のパターンで CPU を食い潰さない

use std::collections::BTreeMap;

use fancy_regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use super::tags::{normalize_tags, TagSet};

/// 正規表現のバックトラック上限（1 値あたり）
const BACKTRACK_LIMIT: usize = 100_000;
/// 操作リストの上限
pub const MAX_OPS: usize = 64;
/// 連番の 0 埋め桁数の上限
pub const MAX_PAD: u8 = 6;
/// `set_rows` の行数の上限（JSON で数 MB。移行の補填で 1 アルバム数百行）
pub const MAX_SET_ROWS: usize = 10_000;
/// 埋め込み画像の擬似キー。操作の対象にできない（P1-3 のアートワークで扱う）
const PICTURE_KEY: &str = "PICTURE";

#[derive(Debug, thiserror::Error)]
pub enum TagOpsError {
    #[error("操作リストは 1 件以上 {MAX_OPS} 件以下の配列")]
    Shape,
    #[error("操作 {index}: {message}")]
    Invalid { index: usize, message: String },
    #[error("操作 {index}: 正規表現が不正: {message}")]
    Regex { index: usize, message: String },
}

/// 1 操作。`key` は大文字正規化済み
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Op {
    /// 固定値代入。空（空文字 / 空配列）は削除と同義
    Set {
        key: String,
        value: Vec<String>,
    },
    /// フィールド参照。`%artist%` を同じトラックの先頭値で展開（無ければ空）
    Ref {
        key: String,
        template: String,
    },
    /// 正規表現置換。各値に対して全一致を置換（`$1` 参照可）
    Replace {
        key: String,
        pattern: String,
        replacement: String,
        #[serde(skip)]
        regex: Option<Regex>,
    },
    /// 連番。`start + index`、`pad` 桁で 0 埋め
    Number {
        key: String,
        start: u32,
        #[serde(default)]
        pad: u8,
    },
    Delete {
        key: String,
    },
    /// 行ごとの固定値代入。`rows` はトラック id → 値（空は削除）。その行の id が無ければ何もしない。
    /// 一括編集の UI には出さず、スクリプトからの補填（`SOURCE_URL` 等）に使う
    #[serde(rename = "set_rows")]
    SetRows {
        key: String,
        #[serde(serialize_with = "serialize_rows")]
        rows: BTreeMap<i64, Vec<String>>,
    },
}

/// JSON のキーは文字列なので、id を文字列にして書く
fn serialize_rows<S: serde::Serializer>(
    rows: &BTreeMap<i64, Vec<String>>,
    s: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap as _;
    let mut m = s.serialize_map(Some(rows.len()))?;
    for (id, v) in rows {
        m.serialize_entry(&id.to_string(), v)?;
    }
    m.end()
}

/// 比較はコンパイル済み正規表現を除く（パターン文字列が同じなら同じ操作）
impl PartialEq for Op {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Op::Set { key: a, value: b }, Op::Set { key: c, value: d }) => a == c && b == d,
            (
                Op::Ref {
                    key: a,
                    template: b,
                },
                Op::Ref {
                    key: c,
                    template: d,
                },
            ) => a == c && b == d,
            (
                Op::Replace {
                    key: a,
                    pattern: b,
                    replacement: c,
                    ..
                },
                Op::Replace {
                    key: d,
                    pattern: e,
                    replacement: f,
                    ..
                },
            ) => a == d && b == e && c == f,
            (
                Op::Number {
                    key: a,
                    start: b,
                    pad: c,
                },
                Op::Number {
                    key: d,
                    start: e,
                    pad: f,
                },
            ) => a == d && b == e && c == f,
            (Op::Delete { key: a }, Op::Delete { key: b }) => a == b,
            (Op::SetRows { key: a, rows: b }, Op::SetRows { key: c, rows: d }) => a == c && b == d,
            _ => false,
        }
    }
}

/// 受け取る JSON の形（`regex` を持たない）
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum OpJson {
    Set {
        key: String,
        #[serde(default)]
        value: SetValue,
    },
    Ref {
        key: String,
        template: String,
    },
    Replace {
        key: String,
        pattern: String,
        #[serde(default)]
        replacement: String,
    },
    Number {
        key: String,
        #[serde(default = "one")]
        start: i64,
        #[serde(default)]
        pad: u8,
    },
    Delete {
        key: String,
    },
    #[serde(rename = "set_rows")]
    SetRows {
        key: String,
        rows: BTreeMap<String, SetValue>,
    },
}

fn one() -> i64 {
    1
}

#[derive(Debug, Deserialize, Default)]
#[serde(untagged)]
enum SetValue {
    #[default]
    Empty,
    One(String),
    Many(Vec<String>),
}

impl SetValue {
    /// NFC に正規化し、空の値を落とす（空だけなら削除と同義）
    fn into_values(self) -> Vec<String> {
        match self {
            SetValue::Empty => Vec::new(),
            SetValue::One(s) => vec![nfc(&s)],
            SetValue::Many(v) => v.iter().map(|s| nfc(s)).collect(),
        }
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect()
    }
}

fn nfc(s: &str) -> String {
    s.nfc().collect()
}

/// キーの検証と正規化: 空でなく、Vorbis Comment で使えない文字（`=`、制御文字）を含まず、
/// `PICTURE` でない
fn normalize_key(index: usize, key: &str) -> Result<String, TagOpsError> {
    let key = key.trim().to_uppercase();
    if key.is_empty() {
        return Err(TagOpsError::Invalid {
            index,
            message: "キーが空".to_owned(),
        });
    }
    if !key.chars().all(|c| (' '..='}').contains(&c) && c != '=') {
        return Err(TagOpsError::Invalid {
            index,
            message: format!("キーに使えない文字がある: {key:?}"),
        });
    }
    if key == PICTURE_KEY {
        return Err(TagOpsError::Invalid {
            index,
            message: "画像はタグ編集の対象外".to_owned(),
        });
    }
    Ok(key)
}

/// JSON 配列を操作リストにする。正規表現はここでコンパイルして不正なら拒否する
pub fn parse_ops(v: &serde_json::Value) -> Result<Vec<Op>, TagOpsError> {
    let arr = v.as_array().ok_or(TagOpsError::Shape)?;
    if arr.is_empty() || arr.len() > MAX_OPS {
        return Err(TagOpsError::Shape);
    }
    let mut out = Vec::with_capacity(arr.len());
    for (index, item) in arr.iter().enumerate() {
        let parsed: OpJson =
            serde_json::from_value(item.clone()).map_err(|e| TagOpsError::Invalid {
                index,
                message: e.to_string(),
            })?;
        let op =
            match parsed {
                OpJson::Set { key, value } => Op::Set {
                    key: normalize_key(index, &key)?,
                    value: value.into_values(),
                },
                OpJson::SetRows { key, rows } => {
                    if rows.is_empty() || rows.len() > MAX_SET_ROWS {
                        return Err(TagOpsError::Invalid {
                            index,
                            message: format!("rows は 1 件以上 {MAX_SET_ROWS} 件以下"),
                        });
                    }
                    let mut parsed = BTreeMap::new();
                    for (id, value) in rows {
                        let id: i64 = id.parse().ok().filter(|n| *n > 0).ok_or_else(|| {
                            TagOpsError::Invalid {
                                index,
                                message: format!("rows のキーはトラック id: {id:?}"),
                            }
                        })?;
                        parsed.insert(id, value.into_values());
                    }
                    Op::SetRows {
                        key: normalize_key(index, &key)?,
                        rows: parsed,
                    }
                }
                OpJson::Ref { key, template } => Op::Ref {
                    key: normalize_key(index, &key)?,
                    template: nfc(&template),
                },
                OpJson::Replace {
                    key,
                    pattern,
                    replacement,
                } => {
                    let regex = RegexBuilder::new(&pattern)
                        .backtrack_limit(BACKTRACK_LIMIT)
                        .build()
                        .map_err(|e| TagOpsError::Regex {
                            index,
                            message: e.to_string(),
                        })?;
                    Op::Replace {
                        key: normalize_key(index, &key)?,
                        pattern,
                        replacement: nfc(&replacement),
                        regex: Some(regex),
                    }
                }
                OpJson::Number { key, start, pad } => {
                    let start = u32::try_from(start).map_err(|_| TagOpsError::Invalid {
                        index,
                        message: format!("start は 0 以上の整数: {start}"),
                    })?;
                    if pad > MAX_PAD {
                        return Err(TagOpsError::Invalid {
                            index,
                            message: format!("pad は 0 以上 {MAX_PAD} 以下: {pad}"),
                        });
                    }
                    Op::Number {
                        key: normalize_key(index, &key)?,
                        start,
                        pad,
                    }
                }
                OpJson::Delete { key } => Op::Delete {
                    key: normalize_key(index, &key)?,
                },
            };
        out.push(op);
    }
    Ok(out)
}

/// `%key%` を `set` の先頭値で展開する（大小文字を問わない。無ければ空）
fn expand_template(template: &str, set: &TagSet) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) if end > 0 => {
                let key = after[..end].to_uppercase();
                if let Some(v) = set.first(&key) {
                    out.push_str(v);
                }
                rest = &after[end + 1..];
            }
            Some(_) => {
                // `%%` は `%` そのもの
                out.push('%');
                rest = &after[1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn set_key(items: &mut Vec<(String, String)>, key: &str, values: Vec<String>) {
    items.retain(|(k, _)| k != key);
    items.extend(values.into_iter().map(|v| (key.to_owned(), v)));
}

/// 操作リストを上から順に適用した新しいタグ集合。`index` は連番の 0 始まりの位置
/// （選択集合を現在のソート順に並べたときの位置）、`track_id` は `set_rows` が行を引く id
pub fn apply_ops(
    ops: &[Op],
    tags: &TagSet,
    index: usize,
    track_id: i64,
) -> Result<TagSet, TagOpsError> {
    let mut items: Vec<(String, String)> = tags.items().to_vec();
    for (i, op) in ops.iter().enumerate() {
        match op {
            Op::Set { key, value } => set_key(&mut items, key, value.clone()),
            Op::Ref { key, template } => {
                let current = normalize_tags(items.clone());
                let v = expand_template(template, &current);
                set_key(
                    &mut items,
                    key,
                    if v.is_empty() { Vec::new() } else { vec![v] },
                );
            }
            Op::Replace {
                key,
                pattern,
                replacement,
                regex,
            } => {
                let regex = match regex {
                    Some(r) => r,
                    None => &RegexBuilder::new(pattern)
                        .backtrack_limit(BACKTRACK_LIMIT)
                        .build()
                        .map_err(|e| TagOpsError::Regex {
                            index: i,
                            message: e.to_string(),
                        })?,
                };
                let mut replaced = Vec::new();
                for (k, v) in &items {
                    if k == key {
                        // `replace_all` は内部で unwrap するので、上限超過を拾える try 版を使う
                        let new = regex
                            .try_replacen(v, 0, replacement.as_str())
                            .map_err(|e| TagOpsError::Regex {
                                index: i,
                                message: e.to_string(),
                            })?
                            .into_owned();
                        replaced.push(new);
                    }
                }
                if !items.iter().any(|(k, _)| k == key) {
                    continue;
                }
                let values: Vec<String> = replaced.into_iter().filter(|s| !s.is_empty()).collect();
                set_key(&mut items, key, values);
            }
            Op::Number { key, start, pad } => {
                let n = u64::from(*start) + index as u64;
                let s = format!("{n:0width$}", width = usize::from(*pad));
                set_key(&mut items, key, vec![s]);
            }
            Op::Delete { key } => set_key(&mut items, key, Vec::new()),
            Op::SetRows { key, rows } => {
                if let Some(values) = rows.get(&track_id) {
                    set_key(&mut items, key, values.clone());
                }
            }
        }
    }
    Ok(normalize_tags(items))
}

/// 操作リストを canonical な JSON にする（snapshot との照合用。`regex` は含まない）
pub fn ops_to_json(ops: &[Op]) -> serde_json::Value {
    serde_json::to_value(ops).unwrap_or_else(|_| serde_json::Value::Array(Vec::new()))
}

//! `GET /api/tracks` のフィルタ・ソート・カーソル（SPEC §9、D-39）。
//!
//! フィルタは URL エンコードした JSON 1 文字列で、キーはホワイトリスト（未知キーは拒否）。
//! `selection.filter` も同じ文字列を受ける。SQL の生成は `db::tracks` にあり、ここは
//! 構文と値域の検証だけを持つ（列名・演算子はこの型を経由してしか SQL に出ない）。
//!
//! カーソルは発行時のソート・ソートキーの値・`id` を JSON オブジェクト `{s, k, id}` にして
//! base64url にしたもの。クライアントは不透明文字列として扱う。別のソートで発行された
//! カーソルは `Query::from_params` が 400 にする（フィルタの違いは検証しない。次ページが
//! 空になるだけで壊れはしない）。

use base64::Engine;
use serde::{Deserialize, Serialize};

const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// 1 ページの既定件数と上限
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 1000;

/// trigram FTS で引ける最短の文字数。これ未満は LIKE にフォールバックする
pub const FTS_MIN_CHARS: usize = 3;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    /// ツリー: カテゴリ名（`categories.name`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// ツリー: `tracks.albumartist` の完全一致
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub albumartist: Option<String>,
    /// ツリー: `tracks.album_id`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_id: Option<i64>,
    /// プレイリスト所属（`playlist_items`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist_id: Option<i64>,
    /// 固定フィルタ（SPEC §12.1 サイドバー）。複数は AND
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<Flag>,
    /// 検索語。`FTS_MIN_CHARS` 以上なら FTS5 trigram、未満なら LIKE
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum FilterError {
    #[error("filter が JSON として不正: {0}")]
    Json(#[from] serde_json::Error),
    #[error("sort が不正: {0}")]
    Sort(String),
    #[error("cursor が不正")]
    Cursor,
}

impl Filter {
    /// JSON 文字列から。空文字は空フィルタ
    pub fn parse(s: &str) -> Result<Filter, FilterError> {
        if s.trim().is_empty() {
            return Ok(Filter::default());
        }
        let mut f: Filter = serde_json::from_str(s)?;
        // 空の検索語は無いのと同じ
        if f.q.as_deref().is_some_and(|q| q.trim().is_empty()) {
            f.q = None;
        }
        // 同じ flag の重複は無害だが SQL が伸びるので落とす
        f.flags.sort();
        f.flags.dedup();
        Ok(f)
    }

    pub fn is_empty(&self) -> bool {
        *self == Filter::default()
    }
}

/// 固定フィルタ（SPEC §12.1 / §12.2 のバッジ条件と対応）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flag {
    /// `verification = 'not_attempted'`（`unverifiable` は含めない。SPEC §6）
    Unverified,
    /// `duplicate_groups` に属する
    Duplicate,
    /// `missing_since IS NOT NULL`
    Missing,
    /// `rg_scanned_at IS NULL`
    NoRg,
    /// 解析済みだが書き込み未反映（`rg_written_at IS NULL OR rg_written_at < rg_scanned_at`）
    RgUnwritten,
    /// pending の op がある
    Pending,
    /// 最新の op が `skipped_conflict`
    Conflict,
    /// `nlink > 1`
    Hardlink,
}

impl Flag {
    pub fn as_str(self) -> &'static str {
        match self {
            Flag::Unverified => "unverified",
            Flag::Duplicate => "duplicate",
            Flag::Missing => "missing",
            Flag::NoRg => "no_rg",
            Flag::RgUnwritten => "rg_unwritten",
            Flag::Pending => "pending",
            Flag::Conflict => "conflict",
            Flag::Hardlink => "hardlink",
        }
    }
}

/// ソートキー。`db::tracks` がそれぞれを索引付きの式に写す
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SortKey {
    /// アルバム順（albumartist, album_id, disc_no, track_no）。既定
    Album,
    Title,
    Artist,
    /// アルバム名（`tracks.album`）
    AlbumTitle,
    AlbumArtist,
    Date,
    Duration,
    Codec,
    RelPath,
    Id,
}

impl SortKey {
    pub fn parse(s: &str) -> Option<SortKey> {
        Some(match s {
            "album" => SortKey::Album,
            "title" => SortKey::Title,
            "artist" => SortKey::Artist,
            "album_title" => SortKey::AlbumTitle,
            "albumartist" => SortKey::AlbumArtist,
            "date" => SortKey::Date,
            "duration" => SortKey::Duration,
            "codec" => SortKey::Codec,
            "rel_path" => SortKey::RelPath,
            "id" => SortKey::Id,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SortKey::Album => "album",
            SortKey::Title => "title",
            SortKey::Artist => "artist",
            SortKey::AlbumTitle => "album_title",
            SortKey::AlbumArtist => "albumartist",
            SortKey::Date => "date",
            SortKey::Duration => "duration",
            SortKey::Codec => "codec",
            SortKey::RelPath => "rel_path",
            SortKey::Id => "id",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sort {
    pub key: SortKey,
    pub desc: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Sort {
            key: SortKey::Album,
            desc: false,
        }
    }
}

impl Sort {
    /// `"title"` / `"-title"`（`-` で降順）。空は既定
    pub fn parse(s: &str) -> Result<Sort, FilterError> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(Sort::default());
        }
        let (desc, name) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let key = SortKey::parse(name).ok_or_else(|| FilterError::Sort(s.to_owned()))?;
        Ok(Sort { key, desc })
    }

    pub fn to_param(self) -> String {
        if self.desc {
            format!("-{}", self.key.as_str())
        } else {
            self.key.as_str().to_owned()
        }
    }
}

/// カーソルの 1 要素。ソート式の値は TEXT か INTEGER（coalesce 済みなので NULL は無い）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CursorValue {
    Int(i64),
    Text(String),
}

impl From<i64> for CursorValue {
    fn from(v: i64) -> Self {
        CursorValue::Int(v)
    }
}

impl From<String> for CursorValue {
    fn from(v: String) -> Self {
        CursorValue::Text(v)
    }
}

/// ソートキー値の型（カーソルの値がソート式の型と合っているかを見る）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    Int,
    Text,
}

impl SortKey {
    /// ソート式の型の並び（`db::tracks::sort_exprs` と同じ順）
    pub fn key_types(self) -> &'static [KeyType] {
        match self {
            SortKey::Album => &[KeyType::Text, KeyType::Int, KeyType::Int, KeyType::Int],
            SortKey::Title
            | SortKey::Artist
            | SortKey::AlbumTitle
            | SortKey::AlbumArtist
            | SortKey::Date
            | SortKey::Codec
            | SortKey::RelPath => &[KeyType::Text],
            SortKey::Duration => &[KeyType::Int],
            SortKey::Id => &[],
        }
    }
}

impl CursorValue {
    pub fn key_type(&self) -> KeyType {
        match self {
            CursorValue::Int(_) => KeyType::Int,
            CursorValue::Text(_) => KeyType::Text,
        }
    }
}

/// 最後に返した行のソートキー値と id。発行時のソートも持ち、別のソートに渡されたカーソルを
/// 見分ける（型が違う値をバインドすると SQLite の storage class 順序で比較が常に真になり、
/// 先頭ページが再掲される）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub sort: Sort,
    pub keys: Vec<CursorValue>,
    pub id: i64,
}

#[derive(Serialize, Deserialize)]
struct CursorWire {
    s: String,
    k: Vec<CursorValue>,
    id: i64,
}

impl Cursor {
    /// ソートの型並びと合っているか（`db::tracks` が SQL を組む前に確認する）
    pub fn matches(&self, sort: Sort) -> bool {
        self.sort == sort
            && self.keys.len() == sort.key.key_types().len()
            && self
                .keys
                .iter()
                .zip(sort.key.key_types())
                .all(|(v, t)| v.key_type() == *t)
    }

    pub fn encode(&self) -> String {
        let wire = CursorWire {
            s: self.sort.to_param(),
            k: self.keys.clone(),
            id: self.id,
        };
        let json = serde_json::to_string(&wire).unwrap_or_else(|_| "{}".to_owned());
        BASE64.encode(json)
    }

    pub fn decode(s: &str) -> Result<Cursor, FilterError> {
        let bytes = BASE64.decode(s).map_err(|_| FilterError::Cursor)?;
        let wire: CursorWire = serde_json::from_slice(&bytes).map_err(|_| FilterError::Cursor)?;
        let sort = Sort::parse(&wire.s).map_err(|_| FilterError::Cursor)?;
        let cursor = Cursor {
            sort,
            keys: wire.k,
            id: wire.id,
        };
        // 発行時のソートと値の型が合わないものは壊れたカーソル
        if !cursor.matches(sort) {
            return Err(FilterError::Cursor);
        }
        Ok(cursor)
    }
}

/// 一覧クエリ 1 回分
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub filter: Filter,
    pub sort: Sort,
    pub cursor: Option<Cursor>,
    pub limit: usize,
}

impl Default for Query {
    fn default() -> Self {
        Query {
            filter: Filter::default(),
            sort: Sort::default(),
            cursor: None,
            limit: DEFAULT_LIMIT,
        }
    }
}

impl Query {
    /// クエリ文字列の各値から組み立てる。`limit` は 1..=MAX_LIMIT に丸める
    pub fn from_params(
        filter: Option<&str>,
        sort: Option<&str>,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Query, FilterError> {
        let filter = Filter::parse(filter.unwrap_or(""))?;
        let sort = Sort::parse(sort.unwrap_or(""))?;
        let cursor = match cursor.map(str::trim).filter(|c| !c.is_empty()) {
            Some(c) => {
                let cursor = Cursor::decode(c)?;
                // 別のソート（向き違いを含む）で発行されたカーソルは 400
                if !cursor.matches(sort) {
                    return Err(FilterError::Cursor);
                }
                Some(cursor)
            }
            None => None,
        };
        let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        Ok(Query {
            filter,
            sort,
            cursor,
            limit,
        })
    }
}

/// FTS5 の MATCH 式にする。ユーザ入力を丸ごと 1 つのフレーズとして二重引用符で囲む
/// （`"` は `""` に）。trigram は部分一致なので語の分割はしない
pub fn fts_phrase(q: &str) -> String {
    format!("\"{}\"", q.replace('"', "\"\""))
}

/// LIKE の中置パターン。`%` `_` `\` は `\` でエスケープする（`ESCAPE '\'` と組で使う）
pub fn like_pattern(q: &str) -> String {
    let mut out = String::with_capacity(q.len() + 2);
    out.push('%');
    for ch in q.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('%');
    out
}

/// 検索語が trigram で引けるか（文字数で判定。バイト数ではない）
pub fn uses_fts(q: &str) -> bool {
    q.chars().count() >= FTS_MIN_CHARS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_whitelisted_keys_and_rejects_unknown() {
        let f = Filter::parse(
            r#"{"category":"J-Pop","album_id":3,"flags":["missing","pending","missing"],"q":"情緒"}"#,
        )
        .unwrap();
        assert_eq!(f.category.as_deref(), Some("J-Pop"));
        assert_eq!(f.album_id, Some(3));
        assert_eq!(f.flags, vec![Flag::Missing, Flag::Pending]);
        assert_eq!(f.q.as_deref(), Some("情緒"));
        assert!(Filter::parse(r#"{"title":"x"}"#).is_err(), "未知キーは拒否");
        assert!(Filter::parse(r#"{"flags":["bogus"]}"#).is_err());
        assert!(Filter::parse("").unwrap().is_empty());
        assert!(Filter::parse(r#"{"q":"  "}"#).unwrap().is_empty());
    }

    #[test]
    fn sort_parses_direction_prefix() {
        assert_eq!(Sort::parse("").unwrap(), Sort::default());
        assert_eq!(
            Sort::parse("-title").unwrap(),
            Sort {
                key: SortKey::Title,
                desc: true
            }
        );
        assert!(Sort::parse("rowid").is_err());
        assert_eq!(Sort::parse("-date").unwrap().to_param(), "-date");
    }

    #[test]
    fn cursor_roundtrips_and_rejects_shape_mismatch() {
        let c = Cursor {
            sort: Sort::parse("-album").unwrap(),
            keys: vec![
                "ヰ世界情緒".to_owned().into(),
                12.into(),
                0.into(),
                3.into(),
            ],
            id: 99,
        };
        let s = c.encode();
        assert!(!s.contains('='), "base64url no-pad");
        assert_eq!(Cursor::decode(&s).unwrap(), c);
        assert!(c.matches(Sort::parse("-album").unwrap()));
        assert!(!c.matches(Sort::parse("album").unwrap()), "向きが違う");
        assert!(!c.matches(Sort::parse("title").unwrap()));
        assert!(Cursor::decode("!!!").is_err());
        assert!(Cursor::decode(&BASE64.encode("[]")).is_err());
        let bad = |json: &str| Cursor::decode(&BASE64.encode(json)).is_err();
        assert!(bad(r#"{"s":"title","k":["a"]}"#), "id が無い");
        assert!(bad(r#"{"s":"title","k":[1],"id":1}"#), "title に整数キー");
        assert!(
            bad(r#"{"s":"duration","k":["1"],"id":1}"#),
            "duration に文字列キー"
        );
        assert!(bad(r#"{"s":"album","k":["a",1,1],"id":1}"#), "キー数不足");
        assert!(bad(r#"{"s":"rowid","k":[],"id":1}"#), "未知のソート");
        assert!(!bad(r#"{"s":"id","k":[],"id":7}"#));
    }

    #[test]
    fn query_clamps_limit() {
        let q = Query::from_params(None, None, None, Some(0)).unwrap();
        assert_eq!(q.limit, 1);
        let q = Query::from_params(None, None, None, Some(10_000)).unwrap();
        assert_eq!(q.limit, MAX_LIMIT);
        let q = Query::from_params(None, None, Some("  "), None).unwrap();
        assert_eq!(q.cursor, None);
        assert_eq!(q.limit, DEFAULT_LIMIT);
    }

    #[test]
    fn query_rejects_cursor_issued_for_another_sort() {
        let c = Cursor {
            sort: Sort::parse("duration").unwrap(),
            keys: vec![5.into()],
            id: 1,
        }
        .encode();
        assert!(Query::from_params(None, Some("duration"), Some(&c), None).is_ok());
        assert!(matches!(
            Query::from_params(None, Some("title"), Some(&c), None),
            Err(FilterError::Cursor)
        ));
        assert!(matches!(
            Query::from_params(None, Some("-duration"), Some(&c), None),
            Err(FilterError::Cursor)
        ));
    }

    #[test]
    fn search_helpers() {
        assert_eq!(fts_phrase(r#"a"b"#), r#""a""b""#);
        assert_eq!(like_pattern("50%_\\"), "%50\\%\\_\\\\%");
        assert!(uses_fts("情緒あ"));
        assert!(!uses_fts("情緒"), "2 文字は LIKE");
        assert!(uses_fts("abc"));
    }
}

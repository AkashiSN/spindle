//! root 相対パスの検証と canonical key（SPEC §5「パスの表現と境界」、D-31）。
//!
//! DB・API・テンプレート展開・プレイリスト出力で扱うパスはすべてこの型を通す。
//! 比較・一意性判定は [`RelPath::key`]（`NFD → full casefold → NFD`）で行い、
//! `rel_path` の文字列比較は使わない（ZFS の insensitive + formD と一致しない）。

use std::fmt;

use unicode_normalization::UnicodeNormalization;

/// ZFS / SMB の要素長上限（バイト）。パス全体ではなく各要素の上限
pub const MAX_COMPONENT_BYTES: usize = 255;

/// SMB / exFAT で使えない文字（`/` は区切り、`\` と NUL は別エラーで先に弾く）
const FORBIDDEN_CHARS: [char; 7] = ['<', '>', ':', '"', '|', '?', '*'];

/// Windows の予約デバイス名（拡張子が付いていても予約扱い）
const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RelPathError {
    #[error("パスが空")]
    Empty,
    #[error("絶対パスは受け付けない")]
    Absolute,
    #[error("空の要素を含む")]
    EmptyComponent,
    #[error("`.` / `..` の要素は受け付けない")]
    DotComponent,
    #[error("NUL 文字を含む")]
    Nul,
    #[error("`\\` を含む（区切りは `/`）")]
    Backslash,
    #[error("SMB / exFAT で使えない文字 {0:?} を含む")]
    ForbiddenChar(char),
    #[error("要素の末尾にドットまたはスペースがある")]
    TrailingDotOrSpace,
    #[error("Windows の予約名 {0}")]
    ReservedName(String),
    #[error("要素が {MAX_COMPONENT_BYTES} バイトを超える")]
    ComponentTooLong,
}

/// 検証済みの root 相対パス。`/` 区切り、先頭 `/` なし、`.` / `..` なし
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelPath(String);

impl RelPath {
    pub fn parse(s: &str) -> Result<Self, RelPathError> {
        if s.is_empty() {
            return Err(RelPathError::Empty);
        }
        if s.contains('\0') {
            return Err(RelPathError::Nul);
        }
        if s.contains('\\') {
            return Err(RelPathError::Backslash);
        }
        if s.starts_with('/') {
            return Err(RelPathError::Absolute);
        }
        for component in s.split('/') {
            validate_component(component)?;
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// 最後の要素
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// 親ディレクトリ。root 直下なら `None`
    pub fn parent(&self) -> Option<RelPath> {
        self.0
            .rsplit_once('/')
            .map(|(dir, _)| RelPath(dir.to_owned()))
    }

    /// 要素を 1 つ足す。`name` は区切りを含まない単一要素
    pub fn join(&self, name: &str) -> Result<RelPath, RelPathError> {
        if name.contains('/') {
            return Err(RelPathError::ForbiddenChar('/'));
        }
        if name.contains('\\') {
            return Err(RelPathError::Backslash);
        }
        if name.contains('\0') {
            return Err(RelPathError::Nul);
        }
        validate_component(name)?;
        Ok(RelPath(format!("{}/{name}", self.0)))
    }

    /// 比較・一意性判定用の canonical key（[`canonical_key`]）
    pub fn key(&self) -> String {
        canonical_key(&self.0)
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RelPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// パスの比較に使う canonical key: `NFD → full casefold → NFD`。
///
/// ZFS の `insensitive` + `formD` に合わせた spindle 側の保守的な同値規則。casefold の結果は
/// NFD とは限らない（U+1F88 → U+1F00 U+03B9 など）ので末尾で NFD を再適用し、
/// `key(key(x)) == key(x)` を保つ。NFKC はしない（全角 `Ｂ` と `B` は別）。
/// OpenZFS の `u8_textprep` と同一の保証はなく、最終判定はファイルシステムに任せる（D-31）
pub fn canonical_key(s: &str) -> String {
    let nfd: String = s.nfd().collect();
    let folded = caseless::default_case_fold_str(&nfd);
    folded.nfd().collect()
}

fn validate_component(component: &str) -> Result<(), RelPathError> {
    if component.is_empty() {
        return Err(RelPathError::EmptyComponent);
    }
    if component == "." || component == ".." {
        return Err(RelPathError::DotComponent);
    }
    if let Some(c) = component
        .chars()
        .find(|c| FORBIDDEN_CHARS.contains(c) || c.is_control())
    {
        return Err(RelPathError::ForbiddenChar(c));
    }
    if component.ends_with('.') || component.ends_with(' ') {
        return Err(RelPathError::TrailingDotOrSpace);
    }
    if component.len() > MAX_COMPONENT_BYTES {
        return Err(RelPathError::ComponentTooLong);
    }
    let stem = component.split('.').next().unwrap_or(component);
    if let Some(reserved) = RESERVED_NAMES.iter().find(|r| r.eq_ignore_ascii_case(stem)) {
        return Err(RelPathError::ReservedName((*reserved).to_owned()));
    }
    Ok(())
}

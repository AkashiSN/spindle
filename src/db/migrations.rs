//! `db/migrations/*.sql` のバイナリ埋め込みと列挙。
//!
//! ファイル名は `NNNN_name.sql`（4 桁ゼロ埋め、1 始まり）。既存ファイルは書き換えず、
//! スキーマ変更は必ず新しい連番ファイルを追加する（CLAUDE.md 禁止事項）。
//! 適用（`schema_version` 管理・1 ファイル = 1 トランザクション）は P0-2

use rust_embed::Embed;

#[derive(Embed)]
#[folder = "db/migrations/"]
#[include = "*.sql"]
struct Assets;

/// 埋め込まれたマイグレーション 1 件
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    pub version: u32,
    pub name: String,
    pub file_name: String,
    pub sql: String,
}

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("マイグレーションのファイル名が規則 NNNN_name.sql に従っていない: {0}")]
    BadFileName(String),
    #[error("マイグレーションが UTF-8 でない: {0}")]
    NotUtf8(String),
    #[error("マイグレーションの連番が飛んでいる: {expected:04} を期待したが {found} だった")]
    Gap { expected: u32, found: String },
    #[error("マイグレーションが 1 件もない")]
    Empty,
}

/// `NNNN_name.sql` を `(version, name)` に分解する。規則に合わなければ `None`
pub fn parse_file_name(file_name: &str) -> Option<(u32, String)> {
    let stem = file_name.strip_suffix(".sql")?;
    let (digits, name) = stem.split_once('_')?;
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let version: u32 = digits.parse().ok()?;
    if version == 0 || name.is_empty() {
        return None;
    }
    Some((version, name.to_string()))
}

/// 埋め込まれたマイグレーションを版順に返す。連番が飛んでいればエラー
pub fn embedded() -> Result<Vec<Migration>, MigrationError> {
    let mut list = Vec::new();
    for file_name in Assets::iter() {
        let file_name = file_name.to_string();
        let (version, name) = parse_file_name(&file_name)
            .ok_or_else(|| MigrationError::BadFileName(file_name.clone()))?;
        // iter() が返した名前なので get() は必ず成功するが、念のためエラーに倒す
        let file = Assets::get(&file_name)
            .ok_or_else(|| MigrationError::BadFileName(file_name.clone()))?;
        let sql = String::from_utf8(file.data.into_owned())
            .map_err(|_| MigrationError::NotUtf8(file_name.clone()))?;
        list.push(Migration {
            version,
            name,
            file_name,
            sql,
        });
    }
    list.sort_by_key(|m| m.version);
    if list.is_empty() {
        return Err(MigrationError::Empty);
    }
    for (i, m) in list.iter().enumerate() {
        let expected = i as u32 + 1;
        if m.version != expected {
            return Err(MigrationError::Gap {
                expected,
                found: m.file_name.clone(),
            });
        }
    }
    Ok(list)
}

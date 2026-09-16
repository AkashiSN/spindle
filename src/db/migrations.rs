//! `db/migrations/*.sql` のバイナリ埋め込みと列挙。
//!
//! ファイル名は `NNNN_name.sql`（4 桁ゼロ埋め、1 始まり）。既存ファイルは書き換えず、
//! スキーマ変更は必ず新しい連番ファイルを追加する（CLAUDE.md 禁止事項）。
//! 適用は [`apply`]: `schema_version` の最大値より新しいファイルだけを、
//! **1 ファイル = 1 トランザクション**で流す。PRAGMA は SQL に書かない（コネクション初期化で
//! トランザクション外から設定する。`super::init_connection`）

use rusqlite::{Connection, OptionalExtension};
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
    #[error(
        "DB のスキーマ版 {db} がバイナリの最新 {latest} より新しい（古いバイナリで開いている）"
    )]
    Newer { db: u32, latest: u32 },
    #[error("マイグレーション {version:04} の適用に失敗: {source}")]
    Sql {
        version: u32,
        #[source]
        source: rusqlite::Error,
    },
    #[error("schema_version の読み取りに失敗: {0}")]
    Version(#[source] rusqlite::Error),
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

/// 適用済みの最大版。`schema_version` 表が無ければ `None`（空 DB）
pub fn current_version(conn: &Connection) -> Result<Option<u32>, MigrationError> {
    let has_table: bool = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_version'",
            [],
            |r| r.get::<_, i64>(0).map(|n| n > 0),
        )
        .map_err(MigrationError::Version)?;
    if !has_table {
        return Ok(None);
    }
    conn.query_row("SELECT max(version) FROM schema_version", [], |r| {
        r.get::<_, Option<u32>>(0)
    })
    .optional()
    .map(|v| v.flatten())
    .map_err(MigrationError::Version)
}

/// 埋め込みマイグレーションのうち未適用のものを適用し、適用した版の一覧を返す
pub fn apply(conn: &mut Connection) -> Result<Vec<u32>, MigrationError> {
    let list = embedded()?;
    apply_list(conn, &list)
}

/// `list`（版順）のうち `current_version` より新しいものを 1 ファイル = 1 トランザクションで適用する。
/// DB の版がリストの最新より新しければ何もせずエラー（古いバイナリで新しい DB を触らない）
pub fn apply_list(conn: &mut Connection, list: &[Migration]) -> Result<Vec<u32>, MigrationError> {
    let current = current_version(conn)?.unwrap_or(0);
    let latest = list.last().map(|m| m.version).unwrap_or(0);
    if current > latest {
        return Err(MigrationError::Newer {
            db: current,
            latest,
        });
    }
    let mut applied = Vec::new();
    for m in list.iter().filter(|m| m.version > current) {
        let tx = conn.transaction().map_err(|source| MigrationError::Sql {
            version: m.version,
            source,
        })?;
        tx.execute_batch(&m.sql)
            .and_then(|_| {
                tx.execute(
                    "INSERT INTO schema_version (version, applied_at) VALUES (?1, ?2)",
                    (m.version, super::now_epoch()),
                )
                .map(|_| ())
            })
            .map_err(|source| MigrationError::Sql {
                version: m.version,
                source,
            })?;
        tx.commit().map_err(|source| MigrationError::Sql {
            version: m.version,
            source,
        })?;
        applied.push(m.version);
    }
    Ok(applied)
}

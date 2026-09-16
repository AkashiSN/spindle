//! DB 層。書き込みは単一コネクション、読み取りはプール。どちらも `spawn_blocking` で回す。
//!
//! PRAGMA（`journal_mode=WAL` / `synchronous=NORMAL` / `foreign_keys=ON`）はマイグレーション
//! SQL には書かず、各コネクションの初期化時にトランザクション外で設定する
//! （`journal_mode` / `synchronous` はトランザクション内で変更できず、`foreign_keys` は
//! トランザクション中は無視されるため）。

pub mod history;
pub mod jobs;
pub mod migrations;
pub mod scans;
pub mod tracks;

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use tokio::sync::Semaphore;

pub use migrations::MigrationError;

/// 読み取りプールの上限。単一ユーザ + ジョブの並列読みで十分な数
const MAX_READ_POOL: usize = 8;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Migration(#[from] MigrationError),
    #[error("DB {path} を開けない: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("journal_mode を WAL にできない（{path}）: 実際の値は {actual}")]
    NotWal { path: PathBuf, actual: String },
    #[error("DB スレッドが異常終了した: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("{0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, DbError>;

/// 現在時刻（UNIX epoch 秒）。DB の時刻列はすべてこれに揃える
pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 全コネクション共通の初期化。トランザクション外で呼ぶこと
fn init_connection(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // 書き込み競合時に即エラーにせず待つ。読みプールと書き手が同時に動くため
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

/// `:memory:` にマイグレーションを流した単一コネクション。テストと診断用
pub fn open_memory_connection() -> Result<Connection> {
    let mut conn = Connection::open_in_memory()?;
    init_connection(&conn)?;
    migrations::apply(&mut conn)?;
    Ok(conn)
}

/// 書き込み単一コネクション + 読み取りプール
pub struct Db {
    writer: Arc<Mutex<Connection>>,
    readers: Arc<Mutex<Vec<Connection>>>,
    read_permits: Arc<Semaphore>,
    read_pool_size: usize,
}

impl Db {
    /// ファイル DB を開き、PRAGMA を設定し、未適用のマイグレーションを流す。
    /// ブロッキングするので起動時か `spawn_blocking` の中で呼ぶ
    pub fn open(path: &Path) -> Result<Db> {
        let read_pool_size = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
            .clamp(2, MAX_READ_POOL);
        Self::open_with_pool(path, read_pool_size)
    }

    fn open_with_pool(path: &Path, read_pool_size: usize) -> Result<Db> {
        let open_err = |source| DbError::Open {
            path: path.to_path_buf(),
            source,
        };

        let mut writer = Connection::open(path).map_err(open_err)?;
        init_connection(&writer).map_err(open_err)?;
        let actual: String = writer
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .map_err(open_err)?;
        if !actual.eq_ignore_ascii_case("wal") {
            return Err(DbError::NotWal {
                path: path.to_path_buf(),
                actual,
            });
        }
        let applied = migrations::apply(&mut writer)?;
        if !applied.is_empty() {
            tracing::info!(?applied, "マイグレーションを適用した");
        }

        let mut readers = Vec::with_capacity(read_pool_size);
        for _ in 0..read_pool_size {
            let conn = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(open_err)?;
            init_connection(&conn).map_err(open_err)?;
            readers.push(conn);
        }

        Ok(Db {
            writer: Arc::new(Mutex::new(writer)),
            readers: Arc::new(Mutex::new(readers)),
            read_permits: Arc::new(Semaphore::new(read_pool_size)),
            read_pool_size,
        })
    }

    pub fn read_pool_size(&self) -> usize {
        self.read_pool_size
    }

    /// 書き込みコネクションで `f` を実行する。呼び出しは直列化される
    pub async fn write<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let writer = Arc::clone(&self.writer);
        tokio::task::spawn_blocking(move || {
            // 前の閉包が panic していてもコネクション自体は使える（未コミットは自動ロールバック）
            let mut conn = writer.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut conn)
        })
        .await?
    }

    /// 読み取りプールのコネクションで `f` を実行する。プールサイズまで並列に走る
    pub async fn read<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        // Semaphore は close しないので acquire は失敗しない。失敗したら permit 無しで進めず、
        // プールが空なら下でエラーにする
        let permit = self.read_permits.clone().acquire_owned().await.ok();
        let readers = Arc::clone(&self.readers);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let conn = readers.lock().unwrap_or_else(|e| e.into_inner()).pop();
            let Some(conn) = conn else {
                return Err(DbError::Sqlite(rusqlite::Error::InvalidQuery));
            };
            // `f` が panic してもコネクションをプールへ戻してから unwind を続ける。
            // 戻さないと permit 数と実コネクション数がずれ、以後の read が失敗する
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| f(&conn)));
            readers.lock().unwrap_or_else(|e| e.into_inner()).push(conn);
            match result {
                Ok(result) => result,
                Err(payload) => std::panic::resume_unwind(payload),
            }
        })
        .await?
    }
}

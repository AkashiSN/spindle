//! DB 層: マイグレーション適用、PRAGMA、書き込み単一 / 読み取りプール、FTS5 の挙動。
//! 仕様: docs/SPEC.md §6「全文検索」、docs/TASKS.md P0-2

use rusqlite::{params, Connection};

use spindle::db::{migrations, open_memory_connection, Db, DbError};

/// tracks に 1 行入れる最小ヘルパ（NOT NULL 列だけ埋める）
fn insert_track(conn: &Connection, id: i64, album_id: Option<i64>, title: &str, album: &str) {
    conn.execute(
        "INSERT INTO tracks (id, album_id, rel_path, rel_path_key, size, mtime_ns, ctime_ns,
                             codec, lossless, title, artist_display, album, albumartist, seen_at)
         VALUES (?1, ?2, ?3, ?3, 0, 0, 0, 'flac', 1, ?4, 'アーティスト', ?5, 'アルバムアーティスト', 0)",
        params![id, album_id, format!("A/B/{id}.flac"), title, album],
    )
    .unwrap();
}

fn fts_ids(conn: &Connection, query: &str) -> Vec<i64> {
    let mut stmt = conn
        .prepare("SELECT rowid FROM tracks_fts WHERE tracks_fts MATCH ?1 ORDER BY rowid")
        .unwrap();
    stmt.query_map([query], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

// ---------------------------------------------------------------- マイグレーション

#[test]
fn memory_connection_has_schema_applied() {
    let conn = open_memory_connection().unwrap();
    let version = migrations::current_version(&conn).unwrap();
    assert_eq!(version, Some(migrations::embedded().unwrap().len() as u32));
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='tracks'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn apply_is_idempotent_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spindle.db");

    let db = Db::open(&path).unwrap();
    drop(db);
    // 2 回目の open で二重適用されない（CREATE TABLE が再実行されればここで失敗する）
    let db = Db::open(&path).unwrap();
    drop(db);

    let conn = Connection::open(&path).unwrap();
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, migrations::embedded().unwrap().len() as i64);
    let (version, applied_at): (u32, i64) = conn
        .query_row(
            "SELECT version, applied_at FROM schema_version ORDER BY version LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(version, 1);
    assert!(
        applied_at > 1_700_000_000,
        "applied_at は UNIX epoch 秒: {applied_at}"
    );
}

#[test]
fn apply_runs_each_file_in_one_transaction() {
    // 途中で失敗する SQL を流すと、そのファイルの変更が丸ごと巻き戻る
    let mut conn = Connection::open_in_memory().unwrap();
    let bad = migrations::Migration {
        version: 1,
        name: "broken".into(),
        file_name: "0001_broken.sql".into(),
        sql: "CREATE TABLE schema_version (version INTEGER NOT NULL, applied_at INTEGER NOT NULL);
              CREATE TABLE t_ok (x INTEGER);
              CREATE TABLE t_bad (x INTEGER) STRICT SYNTAX ERROR;"
            .into(),
    };
    let err = migrations::apply_list(&mut conn, &[bad]).unwrap_err();
    assert!(
        matches!(err, migrations::MigrationError::Sql { version: 1, .. }),
        "{err}"
    );
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name IN ('t_ok','schema_version')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "失敗したファイルの変更が残っている");
}

#[test]
fn apply_refuses_database_newer_than_binary() {
    let mut conn = open_memory_connection().unwrap();
    conn.execute(
        "INSERT INTO schema_version (version, applied_at) VALUES (999, 0)",
        [],
    )
    .unwrap();
    let err = migrations::apply(&mut conn).unwrap_err();
    assert!(
        matches!(err, migrations::MigrationError::Newer { db: 999, .. }),
        "{err}"
    );
}

#[test]
fn apply_only_runs_pending_versions() {
    let mut conn = Connection::open_in_memory().unwrap();
    let v1 = migrations::Migration {
        version: 1,
        name: "init".into(),
        file_name: "0001_init.sql".into(),
        sql: "CREATE TABLE schema_version (version INTEGER NOT NULL, applied_at INTEGER NOT NULL);
              CREATE TABLE a (x INTEGER);"
            .into(),
    };
    let v2 = migrations::Migration {
        version: 2,
        name: "b".into(),
        file_name: "0002_b.sql".into(),
        sql: "CREATE TABLE b (x INTEGER);".into(),
    };
    assert_eq!(
        migrations::apply_list(&mut conn, std::slice::from_ref(&v1)).unwrap(),
        vec![1]
    );
    assert_eq!(
        migrations::apply_list(&mut conn, &[v1.clone(), v2.clone()]).unwrap(),
        vec![2]
    );
    assert_eq!(
        migrations::apply_list(&mut conn, &[v1, v2]).unwrap(),
        Vec::<u32>::new()
    );
    assert_eq!(migrations::current_version(&conn).unwrap(), Some(2));
}

// ---------------------------------------------------------------- PRAGMA / コネクション

#[tokio::test]
async fn file_database_uses_wal_and_foreign_keys_on_every_connection() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("spindle.db")).unwrap();

    let (journal, fk, sync): (String, i64, i64) = db
        .write(|conn| {
            Ok((
                conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?,
                conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?,
                conn.query_row("PRAGMA synchronous", [], |r| r.get(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(journal, "wal");
    assert_eq!(fk, 1);
    assert_eq!(sync, 1, "synchronous=NORMAL は 1");

    let (journal, fk): (String, i64) = db
        .read(|conn| {
            Ok((
                conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?,
                conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(journal, "wal");
    assert_eq!(fk, 1);
}

#[tokio::test]
async fn read_connections_reject_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("spindle.db")).unwrap();
    let err = db
        .read(|conn| {
            conn.execute("INSERT INTO categories (name) VALUES ('x')", [])?;
            Ok(())
        })
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::Sqlite(_)), "{err}");
}

#[tokio::test]
async fn writes_are_serialized_and_visible_to_readers() {
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());

    let mut handles = Vec::new();
    for i in 0..20 {
        let db = db.clone();
        handles.push(tokio::spawn(async move {
            db.write(move |conn| {
                conn.execute(
                    "INSERT INTO categories (name) VALUES (?1)",
                    [format!("c{i}")],
                )?;
                Ok(())
            })
            .await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }
    let n: i64 = db
        .read(|conn| Ok(conn.query_row("SELECT count(*) FROM categories", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(n, 20);
}

#[tokio::test]
async fn reads_run_concurrently_up_to_pool_size() {
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
    assert!(db.read_pool_size() >= 2);
    assert!(all_reads_run_concurrently(&db).await);
}

#[tokio::test]
async fn read_connection_returns_to_pool_even_if_closure_panics() {
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());

    // 閉包の panic は Join エラーとして呼び出し側に返り、コネクションはプールへ戻る
    let err = db
        .read(|_conn| -> Result<(), DbError> { panic!("閉包の中で panic") })
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::Join(_)), "{err}");

    // プールサイズ分の同時 read が全件成功する（コネクションが失われていない）
    assert!(
        all_reads_run_concurrently(&db).await,
        "panic でコネクションが失われている"
    );
}

#[tokio::test]
async fn write_transaction_rolls_back_on_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("spindle.db")).unwrap();
    let err = db
        .write(|conn| {
            let tx = conn.transaction()?;
            tx.execute("INSERT INTO categories (name) VALUES ('keep?')", [])?;
            tx.execute("INSERT INTO scan_runs (kind, state, started_at) VALUES ('incremental', 'bogus', 0)", [])?;
            tx.commit()?;
            Ok(())
        })
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::Sqlite(_)), "{err}");
    let n: i64 = db
        .read(|conn| Ok(conn.query_row("SELECT count(*) FROM categories", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(n, 0);
}

/// タイムアウト付きの barrier。`n` 本の read が同時に走っていることを確かめるのに使う。
/// std の Barrier は揃わないと永久に待ち、テストが固まるので使わない
struct TimedBarrier {
    n: usize,
    state: std::sync::Mutex<usize>,
    cv: std::sync::Condvar,
}

impl TimedBarrier {
    fn new(n: usize) -> Self {
        Self {
            n,
            state: std::sync::Mutex::new(0),
            cv: std::sync::Condvar::new(),
        }
    }

    /// 全員が揃えば true。`timeout` 内に揃わなければ false
    fn wait(&self, timeout: std::time::Duration) -> bool {
        let mut count = self.state.lock().unwrap();
        *count += 1;
        self.cv.notify_all();
        let deadline = std::time::Instant::now() + timeout;
        while *count < self.n {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            count = self.cv.wait_timeout(count, remaining).unwrap().0;
        }
        true
    }
}

/// プールサイズ分の read を同時に発行し、全部が同時に走った（barrier に揃った）かを返す
async fn all_reads_run_concurrently(db: &std::sync::Arc<Db>) -> bool {
    let pool = db.read_pool_size();
    let barrier = std::sync::Arc::new(TimedBarrier::new(pool));
    let mut handles = Vec::new();
    for _ in 0..pool {
        let db = db.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            db.read(move |conn| {
                let reached = barrier.wait(std::time::Duration::from_secs(5));
                let _: i64 = conn.query_row("SELECT 1", [], |r| r.get(0))?;
                Ok(reached)
            })
            .await
        }));
    }
    let mut all = true;
    for h in handles {
        all &= h.await.unwrap().unwrap();
    }
    all
}

// ---------------------------------------------------------------- CHECK 制約

#[test]
fn check_constraints_reject_bogus_enum_values() {
    let conn = open_memory_connection().unwrap();
    let err = conn
        .execute(
            "INSERT INTO scan_runs (kind, state, started_at) VALUES ('incremental', 'bogus', 0)",
            [],
        )
        .unwrap_err();
    assert!(err.to_string().contains("CHECK"), "{err}");
    let err = conn
        .execute(
            "INSERT INTO jobs (type, payload, created_at) VALUES ('scan', 'not json', 0)",
            [],
        )
        .unwrap_err();
    assert!(err.to_string().contains("CHECK"), "{err}");
    let err = conn
        .execute("INSERT INTO categories (id, name) VALUES ('abc', 'x')", [])
        .unwrap_err();
    assert!(
        err.to_string().contains("datatype") || err.to_string().contains("STRICT"),
        "{err}"
    );
}

#[test]
fn foreign_keys_are_enforced_on_memory_connection() {
    let conn = open_memory_connection().unwrap();
    let err = conn
        .execute(
            "INSERT INTO genre_category_map (genre, category_id) VALUES ('Rock', 12345)",
            [],
        )
        .unwrap_err();
    assert!(err.to_string().contains("FOREIGN KEY"), "{err}");
}

// ---------------------------------------------------------------- FTS5

#[test]
fn fts_insert_update_delete_follow_tracks() {
    let conn = open_memory_connection().unwrap();
    insert_track(&conn, 1, None, "夜明けの歌", "最初のアルバム");
    assert_eq!(fts_ids(&conn, "夜明け"), vec![1]);
    assert_eq!(fts_ids(&conn, "最初の"), vec![1]);

    conn.execute("UPDATE tracks SET title = '真昼の歌' WHERE id = 1", [])
        .unwrap();
    assert_eq!(
        fts_ids(&conn, "夜明け"),
        Vec::<i64>::new(),
        "旧語で引けてはいけない"
    );
    assert_eq!(fts_ids(&conn, "真昼の"), vec![1]);

    conn.execute("DELETE FROM tracks WHERE id = 1", []).unwrap();
    assert_eq!(fts_ids(&conn, "真昼の"), Vec::<i64>::new());
}

#[test]
fn fts_returns_column_values_from_content_table() {
    // external content: 索引列がすべて tracks の実列なので列値取得が通る
    let conn = open_memory_connection().unwrap();
    insert_track(&conn, 1, None, "夜明けの歌", "最初のアルバム");
    let (title, album): (String, String) = conn
        .query_row(
            "SELECT title, album FROM tracks_fts WHERE tracks_fts MATCH ?1",
            ["夜明け"],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(title, "夜明けの歌");
    assert_eq!(album, "最初のアルバム");
}

#[test]
fn fts_follows_album_rename_through_tracks_album_cache() {
    let conn = open_memory_connection().unwrap();
    conn.execute(
        "INSERT INTO albums (id, rel_dir, rel_dir_key, album) VALUES (10, 'A/B', 'a/b', '旧アルバム名')",
        [],
    )
    .unwrap();
    insert_track(&conn, 1, Some(10), "曲", "旧アルバム名");
    insert_track(&conn, 2, Some(10), "曲", "旧アルバム名");
    assert_eq!(fts_ids(&conn, "旧アルバム"), vec![1, 2]);

    conn.execute("UPDATE albums SET album = '新アルバム名' WHERE id = 10", [])
        .unwrap();

    let cached: Vec<String> = conn
        .prepare("SELECT album FROM tracks WHERE album_id = 10 ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(cached, vec!["新アルバム名", "新アルバム名"]);
    assert_eq!(fts_ids(&conn, "旧アルバム"), Vec::<i64>::new());
    assert_eq!(fts_ids(&conn, "新アルバム"), vec![1, 2]);
}

#[test]
fn fts_seen_at_update_does_not_touch_index() {
    let conn = open_memory_connection().unwrap();
    insert_track(&conn, 1, None, "曲名", "アルバム");

    let before = conn.total_changes();
    conn.execute("UPDATE tracks SET seen_at = 42 WHERE id = 1", [])
        .unwrap();
    let seen_delta = conn.total_changes() - before;

    let before = conn.total_changes();
    conn.execute("UPDATE tracks SET title = '別の曲名' WHERE id = 1", [])
        .unwrap();
    let title_delta = conn.total_changes() - before;

    assert_eq!(seen_delta, 1, "seen_at 更新で FTS トリガが走っている");
    assert!(
        title_delta > 1,
        "索引列の更新では FTS の再索引が走るはず: {title_delta}"
    );
}

#[test]
fn fts_rebuild_and_integrity_check_succeed() {
    let conn = open_memory_connection().unwrap();
    insert_track(&conn, 1, None, "夜明けの歌", "アルバム");
    insert_track(&conn, 2, None, "真昼の歌", "アルバム");
    conn.execute("INSERT INTO tracks_fts(tracks_fts) VALUES ('rebuild')", [])
        .unwrap();
    assert_eq!(fts_ids(&conn, "アルバ"), vec![1, 2]);
    // trigram は 3 文字未満を引けない（LIKE フォールバックは P0-7 の検索 API 側）
    assert_eq!(fts_ids(&conn, "の歌"), Vec::<i64>::new());
    conn.execute(
        "INSERT INTO tracks_fts(tracks_fts) VALUES ('integrity-check')",
        [],
    )
    .unwrap();
}

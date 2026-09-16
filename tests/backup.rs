//! `backup` ジョブと復元ドリル（SPEC §8 / §14「バックアップ」、docs/TASKS.md P0-13）。
//! `VACUUM INTO` → tmp fsync → rename → 親 fsync → 世代 GC、容量不足の中断、
//! スケジューラの due 判定、バックアップから復元した DB での再スキャン一致

#![cfg(target_os = "linux")]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use spindle::db::history::BatchState;
use spindle::db::{self, now_epoch, Db};
use spindle::edit::{Editor, NewTagOp, TagChange};
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, Scanner};
use spindle::jobs::handlers::backup::{
    self, backup_file_name, enqueue_backup, format_utc, is_due, spawn_scheduler_with, BackupHandler,
};
use spindle::jobs::handlers::tagwrite::TagwriteHandler;
use spindle::jobs::{self as jobs, EnqueueResult, JobState, JobType, Jobs, Registry};

// ---------------------------------------------------------------- ハーネス

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    db: Arc<Db>,
    jobs: Arc<Jobs>,
    scanner: Scanner,
    editor: Arc<Editor>,
    shutdown: CancellationToken,
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Library")).unwrap();
        let db_path = dir.path().join("spindle.db");
        Self::open(dir, db_path)
    }

    fn open(dir: tempfile::TempDir, db_path: PathBuf) -> Self {
        let lib = dir.path().join("Library");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let root = Arc::new(RootDir::open(&lib).unwrap());
        let jobs = Jobs::new(db.clone());
        let scanner = Scanner::new(db.clone(), root.clone(), 2);
        let editor = Arc::new(Editor::new(db.clone(), root, jobs.clone()));
        Self {
            dir,
            db_path,
            db,
            jobs,
            scanner,
            editor,
            shutdown: CancellationToken::new(),
            worker: None,
        }
    }

    fn backup_dir(&self) -> PathBuf {
        self.dir.path().join("backup")
    }

    fn handler(&self, retention: u32) -> BackupHandler {
        BackupHandler::new(self.backup_dir(), retention)
    }

    fn start_with(&mut self, handler: BackupHandler) {
        let mut reg = Registry::new();
        reg.register(JobType::Backup, Arc::new(handler));
        reg.register(
            JobType::Tagwrite,
            Arc::new(TagwriteHandler::new(self.editor.clone())),
        );
        self.worker = Some(self.jobs.start(reg, self.shutdown.clone()));
    }

    fn start(&mut self, retention: u32) {
        let h = self.handler(retention);
        self.start_with(h);
    }

    /// ワーカーを止めて DB のコネクションを全部閉じる（コンテナ停止の模擬）。
    /// 一時ディレクトリだけを返す
    async fn stop(mut self) -> (tempfile::TempDir, PathBuf) {
        self.shutdown.cancel();
        if let Some(w) = self.worker.take() {
            let _ = w.await;
        }
        let dir = std::mem::replace(&mut self.dir, tempfile::tempdir().unwrap());
        let db_path = self.db_path.clone();
        drop(self);
        (dir, db_path)
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.db_path).unwrap()
    }

    fn add(&self, rel: &str, seed: u32, title: &str) -> Option<PathBuf> {
        let p = self.dir.path().join("Library").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let seed_file = self.dir.path().join(format!(".seed{seed}.flac"));
        if !seed_file.exists() {
            common::make_audio(self.dir.path(), &format!(".seed{seed}.flac"), "flac", seed)?;
        }
        std::fs::copy(&seed_file, &p).unwrap();
        common::set_basic_tags(&p, title, "Artist", "Album", "AlbumArtist", 1, 1);
        Some(p)
    }

    async fn scan(&self) {
        self.scanner
            .run(
                ScanKind::Incremental,
                Arc::new(|_, _| {}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }

    fn track_ids(&self) -> Vec<i64> {
        self.conn()
            .prepare("SELECT id FROM tracks ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    async fn wait_terminal(&self, id: i64) -> JobState {
        for _ in 0..1000 {
            let job = self.jobs.get(id).await.unwrap().unwrap();
            if job.state.is_terminal() {
                return job.state;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job {id} が終端にならない");
    }

    async fn wait_batch_terminal(&self, id: i64) -> BatchState {
        for _ in 0..1000 {
            let st = db::history::get_batch(&self.conn(), id)
                .unwrap()
                .unwrap()
                .state;
            if !matches!(st, BatchState::Prepared | BatchState::Applying) {
                return st;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("batch {id} が終端にならない");
    }

    /// タグ編集バッチを 1 つ作って反映まで待つ
    async fn edit_titles(&self, ids: &[i64], prefix: &str) -> i64 {
        let ops = ids
            .iter()
            .enumerate()
            .map(|(i, id)| NewTagOp {
                track_id: *id,
                changes: vec![TagChange {
                    key: "TITLE".to_owned(),
                    values: Some(vec![format!("{prefix}{i}")]),
                }],
            })
            .collect();
        let p = self.editor.prepare_tags(None, ops).await.unwrap();
        assert_eq!(
            self.wait_batch_terminal(p.batch_id).await,
            BatchState::Applied
        );
        p.batch_id
    }

    /// バックアップを 1 回走らせて終端状態を返す
    async fn run_backup(&self) -> (i64, JobState) {
        let EnqueueResult::Inserted(id) = enqueue_backup(&self.jobs).await.unwrap() else {
            panic!("backup が重複扱いになった");
        };
        let st = self.wait_terminal(id).await;
        (id, st)
    }
}

impl Drop for Lib {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// `backup/` 直下の確定済みバックアップ（名前昇順）
fn backups_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| backup::is_backup_file_name(n))
            .collect(),
        Err(_) => Vec::new(),
    };
    v.sort();
    v
}

/// ディレクトリ直下の全エントリ名（tmp の取り残しを見る）
fn all_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

// ---------------------------------------------------------------- 純粋関数

#[test]
fn format_utc_is_fixed_width_and_sortable() {
    assert_eq!(format_utc(0), "19700101T000000Z");
    // 2024-02-29 12:34:56 UTC（閏日）
    assert_eq!(format_utc(1_709_210_096), "20240229T123456Z");
    // 2026-09-16 00:00:00 UTC
    assert_eq!(format_utc(1_789_516_800), "20260916T000000Z");
    assert!(format_utc(1_709_210_096) < format_utc(1_789_516_800));
    assert_eq!(
        backup_file_name(1_709_210_096),
        "spindle-20240229T123456Z.db"
    );
    assert!(backup::is_backup_file_name("spindle-20240229T123456Z.db"));
    assert!(!backup::is_backup_file_name(
        ".spindle-20240229T123456Z.db.tmp"
    ));
    assert!(!backup::is_backup_file_name("spindle.db"));
    assert!(!backup::is_backup_file_name(
        "spindle-20240229T123456Z.db-wal"
    ));
}

#[test]
fn is_due_follows_interval_since_last_terminal_backup() {
    let hour = 3600;
    // 一度も無ければ即
    assert!(is_due(None, 1_000_000, 24 * hour));
    // 間隔未満なら待つ
    assert!(!is_due(Some(1_000_000), 1_000_000 + 23 * hour, 24 * hour));
    // 間隔ちょうどで due
    assert!(is_due(Some(1_000_000), 1_000_000 + 24 * hour, 24 * hour));
    // 時計が戻っても（last が未来）due にはしない
    assert!(!is_due(Some(2_000_000), 1_000_000, 24 * hour));
}

// ---------------------------------------------------------------- ジョブ本体

#[tokio::test]
async fn backup_job_writes_a_consistent_copy_without_leaving_tmp() {
    let mut lib = Lib::new();
    lib.conn()
        .execute(
            "INSERT INTO playlists (name, created_at, updated_at) VALUES ('p', 1, 1)",
            [],
        )
        .unwrap();
    lib.start(14);
    let before = now_epoch();
    let (id, st) = lib.run_backup().await;
    assert_eq!(st, JobState::Done);

    let files = backups_in(&lib.backup_dir());
    assert_eq!(files.len(), 1, "{files:?}");
    let name = &files[0];
    assert!(backup::is_backup_file_name(name));
    // ファイル名の時刻はジョブ実行時刻
    let ts = backup::parse_backup_file_name(name).unwrap();
    assert!(ts >= before && ts <= now_epoch(), "{ts} vs {before}");
    // tmp が残っていない
    assert_eq!(all_in(&lib.backup_dir()), files);

    // 中身はスナップショットとして読める（WAL / SHM 無しの単独ファイル）
    let copy = Connection::open_with_flags(
        lib.backup_dir().join(name),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(count(&copy, "playlists"), 1);
    let ok: String = copy
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ok, "ok");
    let job = lib.jobs.get(id).await.unwrap().unwrap();
    assert_eq!(job.done, job.total);
    let _ = &lib.db;
}

#[tokio::test]
async fn backup_job_keeps_only_retention_generations_newest_first() {
    let mut lib = Lib::new();
    let dir = lib.backup_dir();
    std::fs::create_dir_all(&dir).unwrap();
    // 古い世代を 4 つ + 無関係なファイル + 前回クラッシュの tmp
    for ts in [1_000_000, 2_000_000, 3_000_000, 4_000_000] {
        std::fs::write(dir.join(backup_file_name(ts)), b"old").unwrap();
    }
    std::fs::write(dir.join("notes.txt"), b"keep").unwrap();
    std::fs::write(dir.join(".spindle-19700101T000000Z.db.tmp"), b"junk").unwrap();
    lib.start(3);
    let (_, st) = lib.run_backup().await;
    assert_eq!(st, JobState::Done);

    let files = backups_in(&dir);
    assert_eq!(files.len(), 3, "{files:?}");
    // 残るのは新しい順に 3 つ: 3_000_000, 4_000_000, 今回
    assert_eq!(files[0], backup_file_name(3_000_000));
    assert_eq!(files[1], backup_file_name(4_000_000));
    assert!(backup::parse_backup_file_name(&files[2]).unwrap() > 4_000_000);
    let all = all_in(&dir);
    assert!(all.contains(&"notes.txt".to_owned()));
    assert!(!all.iter().any(|n| n.ends_with(".tmp")), "{all:?}");
}

#[tokio::test]
async fn backup_job_fails_without_writing_when_space_is_short() {
    let mut lib = Lib::new();
    let h = lib.handler(14).with_min_free_bytes(u64::MAX);
    lib.start_with(h);
    // 失敗はバックオフ再試行に回るので、1 回で failed にして観察する
    let EnqueueResult::Inserted(id) = lib
        .jobs
        .enqueue(backup::new_backup_job().max_attempts(1))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(lib.wait_terminal(id).await, JobState::Failed);
    let job = lib.jobs.get(id).await.unwrap().unwrap();
    assert!(
        job.last_error.as_deref().unwrap_or("").contains("容量"),
        "{:?}",
        job.last_error
    );
    assert!(backups_in(&lib.backup_dir()).is_empty());
    assert!(!lib
        .backup_dir()
        .exists()
        .then(|| all_in(&lib.backup_dir()))
        .is_some_and(|v| v.iter().any(|n| n.ends_with(".tmp"))));
}

#[tokio::test]
async fn backup_job_dedups_while_queued() {
    let lib = Lib::new();
    // ワーカー未起動なので queued のまま
    let a = enqueue_backup(&lib.jobs).await.unwrap();
    let b = enqueue_backup(&lib.jobs).await.unwrap();
    assert!(matches!(a, EnqueueResult::Inserted(_)));
    assert_eq!(b, EnqueueResult::Duplicate(a.id()));
}

// ---------------------------------------------------------------- スケジューラ

fn backup_jobs(conn: &Connection) -> Vec<(i64, String)> {
    conn.prepare("SELECT id, state FROM jobs WHERE type = 'backup' ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// 起動直後に 1 件だけ投入し、queued のまま tick を何度回しても増えず、shutdown で止まる
#[tokio::test]
async fn scheduler_enqueues_once_and_stops_on_shutdown() {
    let lib = Lib::new();
    let shutdown = CancellationToken::new();
    // ワーカー未起動なので queued のまま。tick を短くして何度も見直させる
    let h = spawn_scheduler_with(
        lib.jobs.clone(),
        3600,
        Duration::from_millis(20),
        shutdown.clone(),
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(backup_jobs(&lib.conn()), [(1, "queued".to_owned())]);
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), h)
        .await
        .expect("shutdown で止まらない")
        .unwrap();
    assert_eq!(backup_jobs(&lib.conn()).len(), 1);
}

/// 最後の終端 backup が間隔内なら投入しない。間隔を過ぎれば（ここでは 0 秒）次を投入する
#[tokio::test]
async fn scheduler_waits_for_the_interval_after_a_terminal_backup() {
    let mut lib = Lib::new();
    lib.start(14);
    let (first, st) = lib.run_backup().await;
    assert_eq!(st, JobState::Done);

    // 間隔 1 時間: 直後は due でない
    let shutdown = CancellationToken::new();
    let h = spawn_scheduler_with(
        lib.jobs.clone(),
        3600,
        Duration::from_millis(20),
        shutdown.clone(),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(backup_jobs(&lib.conn()), [(first, "done".to_owned())]);
    shutdown.cancel();
    let _ = h.await;

    // 間隔 0 秒: 終端の直後に次が投入され、それも done になる。
    // 同じ秒だとファイル名が衝突して（上書きせず）10 秒のバックオフに入るので秒を跨ぐ
    let t0 = now_epoch();
    while now_epoch() == t0 {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let shutdown = CancellationToken::new();
    let h = spawn_scheduler_with(
        lib.jobs.clone(),
        0,
        Duration::from_millis(20),
        shutdown.clone(),
    );
    let mut second = None;
    for _ in 0..200 {
        if let Some(j) = backup_jobs(&lib.conn()).iter().find(|j| j.0 != first) {
            second = Some(j.0);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let second = second.expect("間隔を過ぎても投入されない");
    shutdown.cancel();
    let _ = h.await;
    assert_eq!(lib.wait_terminal(second).await, JobState::Done);
}

// ---------------------------------------------------------------- 復元ドリル

/// 受け入れ: バックアップを取って DB を消し、復元 → スキャンで元の状態に戻る
/// （編集履歴 3 バッチとプレイリスト 2 本。履歴とプレイリストが復元後も一致）
#[tokio::test]
async fn restore_drill_rescans_to_the_same_state() {
    let mut lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.add("A/02.flac", 1, "b").unwrap();
    lib.add("B/01.flac", 2, "c").unwrap();
    lib.scan().await;
    lib.start(14);
    let ids = lib.track_ids();
    assert_eq!(ids.len(), 3);

    // 編集履歴 3 バッチ
    let b1 = lib.edit_titles(&ids[..2], "x").await;
    let b2 = lib.edit_titles(&ids[1..], "y").await;
    let b3 = lib.edit_titles(&ids, "z").await;
    // プレイリスト 2 本（P1-6 まで API は無いので直接入れる）
    {
        let c = lib.conn();
        c.execute(
            "INSERT INTO playlists (id, name, created_at, updated_at) VALUES (1, 'one', 1, 1), (2, 'two', 2, 2)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO playlist_items (playlist_id, position, track_id) VALUES (1, 0, ?1), (1, 1, ?2), (2, 0, ?3)",
            rusqlite::params![ids[0], ids[1], ids[2]],
        )
        .unwrap();
    }
    /// 復元前後で一致すべきもの: トラック数、op / edit 件数、プレイリスト項目、バッチの id と状態
    #[derive(Debug, PartialEq)]
    struct Snapshot {
        tracks: i64,
        ops: i64,
        edits: i64,
        playlists: Vec<(i64, String)>,
        items: Vec<(i64, i64, i64)>,
        batches: Vec<(i64, String)>,
        /// ジョブ履歴（バックアップ自身の行は除く。それは復元後にリカバリで状態が変わる）
        jobs: Vec<(i64, String, String)>,
    }
    let snapshot = |c: &Connection| -> Snapshot {
        let playlists = c
            .prepare("SELECT id, name FROM playlists ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let jobs = c
            .prepare("SELECT id, type, state FROM jobs WHERE type <> 'backup' ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let items = c
            .prepare("SELECT playlist_id, position, track_id FROM playlist_items ORDER BY 1, 2")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let batches = c
            .prepare("SELECT id, state FROM edit_batches ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        Snapshot {
            tracks: count(c, "tracks"),
            ops: count(c, "edit_ops"),
            edits: count(c, "edits"),
            playlists,
            items,
            batches,
            jobs,
        }
    };
    let before = snapshot(&lib.conn());
    assert_eq!(before.tracks, 3);
    assert_eq!(before.playlists.len(), 2);
    assert_eq!(
        before.batches.iter().map(|b| b.0).collect::<Vec<_>>(),
        [b1, b2, b3]
    );
    // tagwrite ジョブの履歴（3 バッチ分）が保護対象に含まれる
    assert_eq!(before.jobs.len(), 2 + 2 + 3);
    assert!(before
        .jobs
        .iter()
        .all(|j| j.1 == "tagwrite" && j.2 == "done"));

    let (backup_job, st) = lib.run_backup().await;
    assert_eq!(st, JobState::Done);
    let backups = backups_in(&lib.backup_dir());
    assert_eq!(backups.len(), 1);
    let backup_path = lib.backup_dir().join(&backups[0]);

    // 停止 → DB（と WAL / SHM）を消す → バックアップで差し替え → 起動 → 起動時スキャン
    let (dir, db_path) = lib.stop().await;
    for suffix in ["", "-wal", "-shm"] {
        let p = PathBuf::from(format!("{}{suffix}", db_path.display()));
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("{}: {e}", p.display()),
        }
    }
    std::fs::copy(&backup_path, &db_path).unwrap();
    let lib = Lib::open(dir, db_path);
    // バックアップの中では作成元の backup ジョブ自身が running。main と同じく起動時
    // リカバリを通すと queued に戻る（不変条件 6）
    assert_eq!(
        backup_jobs(&lib.conn()),
        [(backup_job, "running".to_owned())]
    );
    let recovered = jobs::recovery::run(&lib.db).await.unwrap();
    assert_eq!(recovered.requeued, 1);
    assert_eq!(
        backup_jobs(&lib.conn()),
        [(backup_job, "queued".to_owned())]
    );
    lib.scan().await;

    let after = snapshot(&lib.conn());
    assert_eq!(after, before);
    // ファイル側のタグも最後のバッチの値のまま（DB を戻してもファイルは触らない）
    let titles: Vec<String> = lib
        .conn()
        .prepare("SELECT value FROM track_tags WHERE key = 'TITLE' ORDER BY track_id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(titles, ["z0", "z1", "z2"]);
}

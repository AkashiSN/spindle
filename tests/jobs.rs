//! ジョブシステム（P0-4）: dedup、起動時リカバリ、バックオフ、協調キャンセル、進捗 SSE、
//! `track_locks` による直列化、版付きジョブの stale 判定、`/api/jobs`。
//! 仕様: docs/SPEC.md §8 / §9、docs/DECISIONS.md D-23

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use rusqlite::{params, Connection};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::{now_epoch, Db};
use spindle::jobs::{
    self, backoff_secs, CancelOutcome, EnqueueResult, Event, JobError, JobState, JobType, Jobs,
    NewJob, Outcome, Registry, RetryOutcome, ORPHAN_REQUEUED_ERROR,
};

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

/// 一時ディレクトリ上の DB とジョブハンドル
struct Harness {
    db: Arc<Db>,
    jobs: Arc<Jobs>,
    shutdown: CancellationToken,
    dir: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let jobs = Jobs::new(db.clone());
        Self {
            db,
            jobs,
            shutdown: CancellationToken::new(),
            dir,
        }
    }

    /// コア数を固定して作る（並列度のテスト用）
    fn with_cpus(cpus: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let jobs = Jobs::with_cpus(db.clone(), cpus);
        Self {
            db,
            jobs,
            shutdown: CancellationToken::new(),
            dir,
        }
    }

    /// 同じ DB ファイルを開き直す（プロセス再起動の模擬）
    fn reopen(mut self) -> Self {
        self.shutdown.cancel();
        let dir = std::mem::replace(&mut self.dir, tempfile::tempdir().unwrap());
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let jobs = Jobs::new(db.clone());
        Self {
            db,
            jobs,
            shutdown: CancellationToken::new(),
            dir,
        }
    }

    fn start(&self, registry: Registry) {
        self.jobs.start(registry, self.shutdown.clone());
    }

    fn raw(&self) -> Connection {
        Connection::open(self.dir.path().join("spindle.db")).unwrap()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn scan_job() -> NewJob {
    NewJob::new(JobType::Scan, serde_json::json!({})).dedup_key("scan")
}

fn state_of(conn: &Connection, id: i64) -> JobState {
    let s: String = conn
        .query_row("SELECT state FROM jobs WHERE id = ?1", [id], |r| r.get(0))
        .unwrap();
    s.parse().unwrap()
}

/// 状態が `want` になるまで待つ（最大 5 秒）
async fn wait_state(h: &Harness, id: i64, want: JobState) {
    let conn = h.raw();
    // CI のランナーは I/O が遅く回数ベースでは足りないので、経過時間で待つ
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        if state_of(&conn, id) == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "job {id} が {want:?} にならない（現在 {:?}）",
        state_of(&conn, id)
    );
}

fn insert_track(conn: &Connection, id: i64, tag_version: i64, audio_version: i64) {
    conn.execute(
        "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                             title, artist_display, album, albumartist, seen_at,
                             tag_version, audio_version)
         VALUES (?1, ?2, ?2, 0, 0, 0, 'flac', 1, 't', 'a', 'al', 'aa', 0, ?3, ?4)",
        params![id, format!("A/{id}.flac"), tag_version, audio_version],
    )
    .unwrap();
}

// ---------------------------------------------------------------- dedup

#[tokio::test]
async fn dedup_key_is_unique_only_while_active() {
    let h = Harness::new();
    let first = h.jobs.enqueue(scan_job()).await.unwrap();
    let EnqueueResult::Inserted(id) = first else {
        panic!("1 件目は挿入されるはず: {first:?}");
    };
    // queued の間は同キーを弾く
    assert_eq!(
        h.jobs.enqueue(scan_job()).await.unwrap(),
        EnqueueResult::Duplicate(id)
    );

    let mut reg = Registry::new();
    reg.register_fn(JobType::Scan, |_ctx| async { Ok(Outcome::Done) });
    h.start(reg);
    wait_state(&h, id, JobState::Done).await;

    // done 後は同キーで再投入できる
    let again = h.jobs.enqueue(scan_job()).await.unwrap();
    assert!(
        matches!(again, EnqueueResult::Inserted(new) if new != id),
        "{again:?}"
    );
}

#[tokio::test]
async fn only_one_of_duplicate_submissions_runs() {
    let h = Harness::new();
    let runs = Arc::new(AtomicUsize::new(0));
    let mut reg = Registry::new();
    {
        let runs = runs.clone();
        reg.register_fn(JobType::Scan, move |_ctx| {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok(Outcome::Done)
            }
        });
    }
    let mut ids = Vec::new();
    for _ in 0..5 {
        if let EnqueueResult::Inserted(id) = h.jobs.enqueue(scan_job()).await.unwrap() {
            ids.push(id);
        }
    }
    assert_eq!(ids.len(), 1);
    h.start(reg);
    wait_state(&h, ids[0], JobState::Done).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------- 起動時リカバリ

#[tokio::test]
async fn recovery_requeues_running_jobs_and_clears_locks() {
    let h = Harness::new();
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    {
        let conn = h.raw();
        insert_track(&conn, 1, 1, 1);
        conn.execute(
            "UPDATE jobs SET state = 'running', started_at = 1 WHERE id = ?1",
            [id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO track_locks (track_id, job_id, acquired_at) VALUES (1, ?1, 1)",
            [id],
        )
        .unwrap();
    }

    // プロセスが kill されたことにして開き直す
    let h = h.reopen();
    let report = jobs::recovery::run(&h.db).await.unwrap();
    assert_eq!(report.requeued, 1);
    assert_eq!(report.locks_cleared, 1);

    let conn = h.raw();
    assert_eq!(state_of(&conn, id), JobState::Queued);
    let started: Option<i64> = conn
        .query_row("SELECT started_at FROM jobs WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(started, None);
    let locks: i64 = conn
        .query_row("SELECT count(*) FROM track_locks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(locks, 0);

    // 再開したワーカーが中断ジョブを拾って完走する
    let runs = Arc::new(AtomicUsize::new(0));
    let mut reg = Registry::new();
    {
        let runs = runs.clone();
        reg.register_fn(JobType::Scan, move |_ctx| {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(Outcome::Done)
            }
        });
    }
    h.start(reg);
    wait_state(&h, id, JobState::Done).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------- バックオフ

#[test]
fn backoff_doubles_and_is_capped() {
    assert_eq!(backoff_secs(1), 10);
    assert_eq!(backoff_secs(2), 20);
    assert_eq!(backoff_secs(3), 40);
    assert_eq!(backoff_secs(9), 2560);
    assert_eq!(backoff_secs(10), 3600);
    assert_eq!(backoff_secs(60), 3600);
}

#[tokio::test]
async fn failure_persists_run_after_and_becomes_failed_after_max_attempts() {
    let h = Harness::new();
    let job = NewJob::new(JobType::Scan, serde_json::json!({}))
        .dedup_key("scan")
        .max_attempts(2);
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(job).await.unwrap() else {
        panic!()
    };
    let mut reg = Registry::new();
    reg.register_fn(JobType::Scan, |_ctx| async {
        Err(JobError::from(anyhow::anyhow!("ディスクが読めない")))
    });
    h.start(reg);

    // 1 回目の失敗: queued に戻り、run_after が未来
    let before = now_epoch();
    let conn = h.raw();
    let mut waited = 0;
    loop {
        let (state, attempts): (String, i64) = conn
            .query_row(
                "SELECT state, attempts FROM jobs WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        if state == "queued" && attempts == 1 {
            break;
        }
        waited += 1;
        assert!(
            waited < 500,
            "1 回目の失敗が記録されない: {state} {attempts}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (run_after, last_error): (i64, String) = conn
        .query_row(
            "SELECT run_after, last_error FROM jobs WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(
        run_after >= before + backoff_secs(1),
        "{run_after} vs {before}"
    );
    assert!(last_error.contains("ディスクが読めない"));

    // 再起動を跨いでも run_after は保たれる（DB 列にあるので当然だが、リカバリが消さないこと）
    let h = h.reopen();
    jobs::recovery::run(&h.db).await.unwrap();
    let conn = h.raw();
    let kept: i64 = conn
        .query_row("SELECT run_after FROM jobs WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(kept, run_after);

    // run_after を過去に倒すと 2 回目が走り、max_attempts に達して failed
    conn.execute("UPDATE jobs SET run_after = 0 WHERE id = ?1", [id])
        .unwrap();
    let mut reg = Registry::new();
    reg.register_fn(JobType::Scan, |_ctx| async {
        Err(JobError::from(anyhow::anyhow!("まだ読めない")))
    });
    h.start(reg);
    wait_state(&h, id, JobState::Failed).await;
    let (attempts, finished): (i64, Option<i64>) = conn
        .query_row(
            "SELECT attempts, finished_at FROM jobs WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(attempts, 2);
    assert!(finished.is_some());
}

#[tokio::test]
async fn fatal_failure_is_failed_immediately_without_backoff() {
    // 再試行しても変わらない失敗（取り込み済み等。D-70）は max_attempts に関わらず即 failed
    let h = Harness::new();
    let job = NewJob::new(JobType::Ytdl, serde_json::json!({ "url": "u" }))
        .dedup_key("ytdl:u")
        .max_attempts(5);
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(job).await.unwrap() else {
        panic!()
    };
    let mut reg = Registry::new();
    reg.register_fn(JobType::Ytdl, |_ctx| async {
        Err(JobError::Fatal(anyhow::anyhow!("取り込み済み: A/1.opus")))
    });
    h.start(reg);
    wait_state(&h, id, JobState::Failed).await;
    let conn = h.raw();
    let (attempts, run_after, last_error, finished): (i64, Option<i64>, String, Option<i64>) = conn
        .query_row(
            "SELECT attempts, run_after, last_error, finished_at FROM jobs WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(attempts, 1);
    assert!(run_after.is_none(), "バックオフを予約しない: {run_after:?}");
    assert!(last_error.contains("取り込み済み"));
    assert!(finished.is_some());
}

#[test]
fn ytdl_job_type_is_serial_and_round_trips() {
    assert_eq!(JobType::Ytdl.as_str(), "ytdl");
    assert_eq!("ytdl".parse::<JobType>().unwrap(), JobType::Ytdl);
    assert_eq!(JobType::Ytdl.concurrency(8), 1);
    assert!(JobType::ALL.contains(&JobType::Ytdl));
}

#[test]
fn hirescheck_job_type_runs_at_half_the_cores_and_is_versioned() {
    assert_eq!(JobType::Hirescheck.as_str(), "hirescheck");
    assert_eq!(
        "hirescheck".parse::<JobType>().unwrap(),
        JobType::Hirescheck
    );
    assert_eq!(JobType::Hirescheck.concurrency(8), 4);
    assert_eq!(JobType::Hirescheck.concurrency(1), 1);
    assert!(JobType::ALL.contains(&JobType::Hirescheck));
    assert_eq!(
        JobType::Hirescheck.version_field(),
        Some(("audio_version", "audio_version"))
    );
}

/// P4-16: 再生リストの同期は並列 1 で版を持たず、対象は購読 id
#[test]
fn playlist_sync_job_type_is_serial_and_names_the_subscription() {
    assert_eq!(JobType::PlaylistSync.as_str(), "playlist_sync");
    assert_eq!(
        "playlist_sync".parse::<JobType>().unwrap(),
        JobType::PlaylistSync
    );
    assert_eq!(JobType::PlaylistSync.concurrency(8), 1);
    assert!(!JobType::PlaylistSync.cpu_bound());
    assert!(JobType::ALL.contains(&JobType::PlaylistSync));
    assert_eq!(JobType::PlaylistSync.version_field(), None);
    assert_eq!(
        spindle::db::jobs::subject_of(
            JobType::PlaylistSync,
            &serde_json::json!({ "subscription_id": 3 }),
            None,
            None,
            None
        ),
        Some("subscription #3".to_owned())
    );
}

#[tokio::test]
async fn queued_job_with_future_run_after_is_not_claimed() {
    let h = Harness::new();
    let job = scan_job().run_after(now_epoch() + 3600);
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(job).await.unwrap() else {
        panic!()
    };
    let mut reg = Registry::new();
    reg.register_fn(JobType::Scan, |_ctx| async { Ok(Outcome::Done) });
    h.start(reg);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(state_of(&h.raw(), id), JobState::Queued);
}

// ---------------------------------------------------------------- キャンセル

#[tokio::test]
async fn cancel_of_running_job_is_cooperative() {
    let h = Harness::new();
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    let mut reg = Registry::new();
    reg.register_fn(JobType::Scan, |ctx| async move {
        for i in 0..1000 {
            ctx.progress(i, 1000).await?;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok(Outcome::Done)
    });
    h.start(reg);
    wait_state(&h, id, JobState::Running).await;

    assert_eq!(h.jobs.cancel(id).await.unwrap(), CancelOutcome::Requested);
    wait_state(&h, id, JobState::Cancelled).await;
    let conn = h.raw();
    let (req, finished): (Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT cancel_requested_at, finished_at FROM jobs WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(req.is_some());
    assert!(finished.is_some());
}

#[tokio::test]
async fn cancel_of_queued_job_is_immediate_and_terminal_is_rejected() {
    let h = Harness::new();
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    assert_eq!(h.jobs.cancel(id).await.unwrap(), CancelOutcome::Cancelled);
    assert_eq!(state_of(&h.raw(), id), JobState::Cancelled);
    assert_eq!(
        h.jobs.cancel(id).await.unwrap(),
        CancelOutcome::NotCancellable
    );
    assert_eq!(h.jobs.cancel(9999).await.unwrap(), CancelOutcome::NotFound);
}

#[tokio::test]
async fn cancel_kills_child_process_group_and_removes_temp_files() {
    let h = Harness::new();
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    let tmp_path = h.dir.path().join("work.spindle-tmp");
    let pid_slot: Arc<std::sync::Mutex<Option<u32>>> = Arc::default();
    let mut reg = Registry::new();
    {
        let tmp_path = tmp_path.clone();
        let pid_slot = pid_slot.clone();
        reg.register_fn(JobType::Scan, move |ctx| {
            let tmp_path = tmp_path.clone();
            let pid_slot = pid_slot.clone();
            async move {
                let _tmp = ctx.temp_file(&tmp_path);
                std::fs::write(&tmp_path, b"partial").unwrap();
                let mut cmd = tokio::process::Command::new("sleep");
                cmd.arg("60");
                let child = jobs::process::ChildGroup::spawn(cmd).unwrap();
                *pid_slot.lock().unwrap() = Some(child.pid());
                // キャンセルされると子プロセスグループが kill され Cancelled が返る
                child.wait(ctx.cancel_token()).await?;
                Ok(Outcome::Done)
            }
        });
    }
    h.start(reg);
    wait_state(&h, id, JobState::Running).await;
    // CI のランナーは I/O が遅く回数ベースでは足りないので、経過時間で待つ
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        if pid_slot.lock().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pid = pid_slot
        .lock()
        .unwrap()
        .expect("子プロセスが起動していること");
    assert!(tmp_path.exists());

    h.jobs.cancel(id).await.unwrap();
    wait_state(&h, id, JobState::Cancelled).await;
    assert!(!tmp_path.exists(), "tmp が掃除されていること");
    // 子プロセスが消えている（kill 0 で存在確認）
    let alive =
        rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid as i32).unwrap())
            .is_ok();
    assert!(!alive, "子プロセス {pid} が生き残っている");
}

#[tokio::test]
async fn recovery_cancels_running_job_that_had_cancel_requested() {
    let h = Harness::new();
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    h.raw()
        .execute(
            "UPDATE jobs SET state = 'running', started_at = 1, cancel_requested_at = 2 WHERE id = ?1",
            [id],
        )
        .unwrap();
    let h = h.reopen();
    let report = jobs::recovery::run(&h.db).await.unwrap();
    assert_eq!(report.requeued, 0);
    assert_eq!(report.cancelled, 1);
    let conn = h.raw();
    assert_eq!(state_of(&conn, id), JobState::Cancelled);
    let finished: Option<i64> = conn
        .query_row("SELECT finished_at FROM jobs WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(finished.is_some());
}

// ---------------------------------------------------------------- 終端書き込みの再試行と稼働中の回収（D-76）

/// `id` の (attempts, last_error, started_at)
fn attempts_error_started(conn: &Connection, id: i64) -> (i64, Option<String>, Option<i64>) {
    conn.query_row(
        "SELECT attempts, last_error, started_at FROM jobs WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

fn lock_rows(conn: &Connection) -> (i64, i64, i64) {
    let n = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    (
        n("SELECT count(*) FROM track_locks"),
        n("SELECT count(*) FROM derived_path_locks"),
        n("SELECT count(*) FROM job_mutexes"),
    )
}

/// 終端の書き込みが失敗しても（ディスク満杯の模擬に writer を読み取り専用にする）、書けるように
/// なれば再試行で完走し、失敗には数えない
#[tokio::test]
async fn terminal_write_is_retried_until_the_db_is_writable() {
    let h = Harness::new();
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    let runs = Arc::new(AtomicUsize::new(0));
    let mut reg = Registry::new();
    {
        let runs = runs.clone();
        reg.register_fn(JobType::Scan, move |ctx| {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                // 仕事は済んだが、終端を書く直前に DB が書けなくなった
                let db = Arc::clone(ctx.db());
                db.write(|c| Ok(c.pragma_update(None, "query_only", true)?))
                    .await?;
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    db.write(|c| Ok(c.pragma_update(None, "query_only", false)?))
                        .await
                        .unwrap();
                });
                Ok(Outcome::Done)
            }
        });
    }
    h.start(reg);
    wait_state(&h, id, JobState::Done).await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "再実行ではなく再試行で完走する"
    );
    let (attempts, last_error, _) = attempts_error_started(&h.raw(), id);
    assert_eq!(attempts, 0);
    assert_eq!(last_error, None);
}

/// 終端を書けないまま running に残った行（実行中表に無い）は、ワーカーが稼働中に queued へ戻して
/// ロックを消し、その後普通に実行される
#[tokio::test]
async fn orphaned_running_job_is_requeued_while_the_worker_runs() {
    let h = Harness::new();
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    {
        let conn = h.raw();
        insert_track(&conn, 1, 1, 1);
        conn.execute(
            "UPDATE jobs SET state = 'running', started_at = 1 WHERE id = ?1",
            [id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO track_locks (track_id, job_id, acquired_at) VALUES (1, ?1, 1)",
            [id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO derived_path_locks (rel_path_key, track_id, job_id, acquired_at)
             VALUES ('opus/a.opus', NULL, ?1, 1)",
            [id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO job_mutexes (name, job_id, acquired_at) VALUES ('library', ?1, 1)",
            [id],
        )
        .unwrap();
    }
    let runs = Arc::new(AtomicUsize::new(0));
    let mut reg = Registry::new();
    {
        let runs = runs.clone();
        reg.register_fn(JobType::Scan, move |_ctx| {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(Outcome::Done)
            }
        });
    }
    let mut rx = h.jobs.subscribe();
    h.start(reg);
    wait_state(&h, id, JobState::Done).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    // 回収（queued）→ 実行（running）→ 完了（done）の順に SSE が流れる
    let mut states = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::Job(j) = ev {
            if j.id == id {
                states.push(j.state);
            }
        }
    }
    assert_eq!(
        states,
        [JobState::Queued, JobState::Running, JobState::Done],
        "{states:?}"
    );
    let conn = h.raw();
    let (attempts, last_error, _) = attempts_error_started(&conn, id);
    assert_eq!(attempts, 0, "回収は失敗に数えない");
    assert_eq!(last_error.as_deref(), Some(ORPHAN_REQUEUED_ERROR));
    assert_eq!(lock_rows(&conn), (0, 0, 0));
}

/// cancel 要求が立ったまま取り残された running は cancelled（起動時リカバリと同じ）
#[tokio::test]
async fn orphaned_running_job_with_cancel_requested_is_cancelled() {
    let h = Harness::new();
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    h.raw()
        .execute(
            "UPDATE jobs SET state = 'running', started_at = 1, cancel_requested_at = 2 WHERE id = ?1",
            [id],
        )
        .unwrap();
    let runs = Arc::new(AtomicUsize::new(0));
    let mut reg = Registry::new();
    {
        let runs = runs.clone();
        reg.register_fn(JobType::Scan, move |_ctx| {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(Outcome::Done)
            }
        });
    }
    h.start(reg);
    wait_state(&h, id, JobState::Cancelled).await;
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    let (_, _, started) = attempts_error_started(&h.raw(), id);
    assert_eq!(started, None);
}

/// 本物の running（実行中表にある）は、ワーカーが何周回っても戻さない
#[tokio::test]
async fn running_job_that_is_tracked_is_not_swept() {
    let h = Harness::new();
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    let runs = Arc::new(AtomicUsize::new(0));
    let mut reg = Registry::new();
    {
        let runs = runs.clone();
        reg.register_fn(JobType::Scan, move |_ctx| {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                // ワーカーの周回（最長 1 秒）を何度も跨ぐ
                tokio::time::sleep(Duration::from_secs(3)).await;
                Ok(Outcome::Done)
            }
        });
    }
    h.start(reg);
    wait_state(&h, id, JobState::Done).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1, "戻されて再実行されていない");
    let (attempts, last_error, _) = attempts_error_started(&h.raw(), id);
    assert_eq!(attempts, 0);
    assert_eq!(last_error, None);
}

#[tokio::test]
async fn queued_job_with_cancel_requested_is_cancelled_without_running() {
    let h = Harness::new();
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    // バックオフ中に cancel が来て、その直後にクラッシュしたような行
    h.raw()
        .execute(
            "UPDATE jobs SET cancel_requested_at = 2 WHERE id = ?1",
            [id],
        )
        .unwrap();
    let runs = Arc::new(AtomicUsize::new(0));
    let mut reg = Registry::new();
    {
        let runs = runs.clone();
        reg.register_fn(JobType::Scan, move |_ctx| {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(Outcome::Done)
            }
        });
    }
    h.start(reg);
    wait_state(&h, id, JobState::Cancelled).await;
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

/// ハンドラが token を見ずに失敗 / 再キューを返しても、cancel 要求があれば cancelled になる
/// （キャンセルしたジョブをバックオフで再実行しない）
#[tokio::test]
async fn cancel_wins_over_failure_and_requeue() {
    for outcome in ["failed", "requeue"] {
        let h = Harness::new();
        let EnqueueResult::Inserted(id) = h.jobs.enqueue(scan_job()).await.unwrap() else {
            panic!()
        };
        let mut reg = Registry::new();
        reg.register_fn(JobType::Scan, move |_ctx| async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if outcome == "failed" {
                Err(JobError::from(anyhow::anyhow!("token を見ないハンドラ")))
            } else {
                Ok(Outcome::Requeue)
            }
        });
        h.start(reg);
        wait_state(&h, id, JobState::Running).await;
        assert_eq!(h.jobs.cancel(id).await.unwrap(), CancelOutcome::Requested);
        wait_state(&h, id, JobState::Cancelled).await;
        let attempts: i64 = h
            .raw()
            .query_row("SELECT attempts FROM jobs WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(attempts, 0, "{outcome}: cancelled なので試行回数は数えない");
    }
}

/// 完了直前に cancel が来た場合は完了が勝つ（仕事は済んでいる。D-36）
#[tokio::test]
async fn done_wins_over_late_cancel() {
    let h = Harness::new();
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    let mut reg = Registry::new();
    reg.register_fn(JobType::Scan, |_ctx| async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(Outcome::Done)
    });
    h.start(reg);
    wait_state(&h, id, JobState::Running).await;
    assert_eq!(h.jobs.cancel(id).await.unwrap(), CancelOutcome::Requested);
    wait_state(&h, id, JobState::Done).await;
}

#[tokio::test]
async fn cancel_kills_grandchild_that_ignores_sigterm() {
    let h = Harness::new();
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    let pid_slot: Arc<std::sync::Mutex<Option<u32>>> = Arc::default();
    let mut reg = Registry::new();
    {
        let pid_slot = pid_slot.clone();
        reg.register_fn(JobType::Scan, move |ctx| {
            let pid_slot = pid_slot.clone();
            async move {
                // leader は TERM で死ぬが、孫は TERM を無視する
                let mut cmd = tokio::process::Command::new("sh");
                cmd.arg("-c")
                    .arg("(trap '' TERM; exec sleep 60) & exec sleep 60");
                let child = jobs::process::ChildGroup::spawn(cmd).unwrap();
                *pid_slot.lock().unwrap() = Some(child.pid());
                child.wait(ctx.cancel_token()).await?;
                Ok(Outcome::Done)
            }
        });
    }
    h.start(reg);
    wait_state(&h, id, JobState::Running).await;
    // CI のランナーは I/O が遅く回数ベースでは足りないので、経過時間で待つ
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        if pid_slot.lock().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pid = pid_slot
        .lock()
        .unwrap()
        .expect("子プロセスが起動していること");
    // 孫が起動するまで少し待つ
    tokio::time::sleep(Duration::from_millis(200)).await;
    let pgid = rustix::process::Pid::from_raw(pid as i32).unwrap();
    assert!(rustix::process::test_kill_process_group(pgid).is_ok());

    h.jobs.cancel(id).await.unwrap();
    wait_state(&h, id, JobState::Cancelled).await;
    // グループ全体（TERM を無視した孫を含む）が消えている
    let mut alive = true;
    // CI のランナーは I/O が遅く回数ベースでは足りないので、経過時間で待つ
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        alive = rustix::process::test_kill_process_group(pgid).is_ok();
        if !alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!alive, "プロセスグループ {pid} に生き残りがいる");
}

// ---------------------------------------------------------------- 進捗

#[tokio::test]
async fn progress_is_broadcast_and_persisted() {
    let h = Harness::new();
    let mut rx = h.jobs.subscribe();
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    let mut reg = Registry::new();
    reg.register_fn(JobType::Scan, |ctx| async move {
        ctx.progress(1, 4).await?;
        ctx.progress(2, 4).await?;
        Ok(Outcome::Done)
    });
    h.start(reg);
    wait_state(&h, id, JobState::Done).await;

    let mut seen = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
        if let Event::Job(j) = ev {
            if j.id == id {
                seen.push((j.state, j.done, j.total));
            }
        }
    }
    assert!(
        seen.contains(&(JobState::Running, Some(2), Some(4))),
        "{seen:?}"
    );
    assert_eq!(seen.last().map(|s| s.0), Some(JobState::Done));

    let conn = h.raw();
    let (progress, done, total): (f64, i64, i64) = conn
        .query_row(
            "SELECT progress, done, total FROM jobs WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((done, total), (4, 4));
    assert!((progress - 1.0).abs() < f64::EPSILON);
}

// ---------------------------------------------------------------- track_locks

#[tokio::test]
async fn jobs_on_the_same_track_are_serialized() {
    let h = Harness::new();
    insert_track(&h.raw(), 1, 1, 1);
    let mut ids = Vec::new();
    // 版を持たない種別（並列 4）でハンドラ自身がロックを取る経路を検証する。
    // 版付き（tagwrite / transcode）は基盤がハンドラ起動前にロックを取る
    for i in 0..3 {
        let job = NewJob::new(JobType::Thumbnail, serde_json::json!({"track_id": 1}))
            .dedup_key(format!("thumbnail:1:{i}"));
        let EnqueueResult::Inserted(id) = h.jobs.enqueue(job).await.unwrap() else {
            panic!()
        };
        ids.push(id);
    }
    let concurrent = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let requeued = Arc::new(AtomicUsize::new(0));
    let mut reg = Registry::new();
    {
        let (concurrent, max_seen, requeued) =
            (concurrent.clone(), max_seen.clone(), requeued.clone());
        reg.register_fn(JobType::Thumbnail, move |ctx| {
            let (concurrent, max_seen, requeued) =
                (concurrent.clone(), max_seen.clone(), requeued.clone());
            async move {
                if !ctx.lock_tracks(&[1]).await? {
                    requeued.fetch_add(1, Ordering::SeqCst);
                    return Ok(Outcome::Requeue);
                }
                let n = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(n, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
                concurrent.fetch_sub(1, Ordering::SeqCst);
                Ok(Outcome::Done)
            }
        });
    }
    h.start(reg);
    for id in &ids {
        wait_state(&h, *id, JobState::Done).await;
    }
    assert_eq!(max_seen.load(Ordering::SeqCst), 1);
    assert!(
        requeued.load(Ordering::SeqCst) >= 1,
        "並列 4 なので取り合いが起きるはず"
    );
    let locks: i64 = h
        .raw()
        .query_row("SELECT count(*) FROM track_locks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(locks, 0, "完了後にロックが残らない");
}

#[tokio::test]
async fn multi_track_lock_is_all_or_nothing() {
    let h = Harness::new();
    let conn = h.raw();
    for t in 1..=3 {
        insert_track(&conn, t, 1, 1);
    }
    // 他のジョブがトラック 2 を握っている
    conn.execute(
        "INSERT INTO jobs (id, type, payload, state, created_at) VALUES (99, 'rg', '{}', 'running', 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO track_locks (track_id, job_id, acquired_at) VALUES (2, 99, 0)",
        [],
    )
    .unwrap();

    let EnqueueResult::Inserted(me) = h.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    let got =
        h.db.write(move |c| spindle::db::jobs::acquire_track_locks(c, me, &[3, 1, 2], 0))
            .await
            .unwrap();
    assert!(!got);
    let held: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT track_id FROM track_locks ORDER BY track_id")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    assert_eq!(held, vec![2], "取れなかったら全解放される");
}

// ---------------------------------------------------------------- stale

#[tokio::test]
async fn stale_versioned_job_is_done_without_running() {
    let h = Harness::new();
    insert_track(&h.raw(), 1, 5, 2);
    let stale = NewJob::new(
        JobType::Tagwrite,
        serde_json::json!({"track_id": 1, "tag_version": 4}),
    )
    .dedup_key("tagwrite:1:4");
    let fresh = NewJob::new(
        JobType::Tagwrite,
        serde_json::json!({"track_id": 1, "tag_version": 5}),
    )
    .dedup_key("tagwrite:1:5");
    let stale_audio = NewJob::new(
        JobType::Transcode,
        serde_json::json!({"track_id": 1, "audio_version": 1}),
    )
    .dedup_key("transcode:1:1");
    let EnqueueResult::Inserted(stale_id) = h.jobs.enqueue(stale).await.unwrap() else {
        panic!()
    };
    let EnqueueResult::Inserted(fresh_id) = h.jobs.enqueue(fresh).await.unwrap() else {
        panic!()
    };
    let EnqueueResult::Inserted(stale_audio_id) = h.jobs.enqueue(stale_audio).await.unwrap() else {
        panic!()
    };
    let ran = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut reg = Registry::new();
    for ty in [JobType::Tagwrite, JobType::Transcode] {
        let ran = ran.clone();
        reg.register_fn(ty, move |ctx| {
            let ran = ran.clone();
            async move {
                ran.lock().unwrap().push(ctx.job.id);
                Ok(Outcome::Done)
            }
        });
    }
    h.start(reg);
    for id in [stale_id, fresh_id, stale_audio_id] {
        wait_state(&h, id, JobState::Done).await;
    }
    assert_eq!(*ran.lock().unwrap(), vec![fresh_id]);
}

/// stale 判定はロック取得の後に行う。ロック待ちの間に版が進めば実行しない
#[tokio::test]
async fn stale_check_happens_after_track_lock() {
    let h = Harness::new();
    let conn = h.raw();
    insert_track(&conn, 1, 5, 1);
    // 別の本物の running ジョブ（rg）がトラック 1 を握り続ける（行を直接 running にすると
    // 稼働中の回収で queued に戻される。D-76）
    let release = CancellationToken::new();
    let runs = Arc::new(AtomicUsize::new(0));
    let mut reg = Registry::new();
    {
        let runs = runs.clone();
        reg.register_fn(JobType::Tagwrite, move |_ctx| {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(Outcome::Done)
            }
        });
        let release = release.clone();
        reg.register_fn(JobType::Rg, move |ctx| {
            let release = release.clone();
            async move {
                if !ctx.lock_tracks(&[1]).await? {
                    return Ok(Outcome::Requeue);
                }
                release.cancelled().await;
                Ok(Outcome::Done)
            }
        });
    }
    let EnqueueResult::Inserted(holder) = h
        .jobs
        .enqueue(NewJob::new(JobType::Rg, serde_json::json!({})))
        .await
        .unwrap()
    else {
        panic!()
    };
    h.start(reg);
    wait_state(&h, holder, JobState::Running).await;
    let locks: i64 = conn
        .query_row("SELECT count(*) FROM track_locks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(locks, 1, "rg がトラック 1 を握っている");
    let job = NewJob::new(
        JobType::Tagwrite,
        serde_json::json!({"track_id": 1, "tag_version": 5}),
    )
    .dedup_key("tagwrite:1:5");
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(job).await.unwrap() else {
        panic!()
    };
    // ロックが取れないので走らずに queued へ戻る（run_after が立つ）
    let mut requeued = false;
    // CI のランナーは I/O が遅く回数ベースでは足りないので、経過時間で待つ
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        let (state, run_after): (String, Option<i64>) = conn
            .query_row(
                "SELECT state, run_after FROM jobs WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        if state == "queued" && run_after.is_some() {
            requeued = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(requeued, "ロック待ちで再キューされること");
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    // ロック保持中に版が進んでからロックが外れる（rg の終端で解放される）
    conn.execute("UPDATE tracks SET tag_version = 6 WHERE id = 1", [])
        .unwrap();
    release.cancel();
    wait_state(&h, holder, JobState::Done).await;
    wait_state(&h, id, JobState::Done).await;
    assert_eq!(runs.load(Ordering::SeqCst), 0, "stale なので実行されない");
    let locks: i64 = conn
        .query_row("SELECT count(*) FROM track_locks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(locks, 0);
}

/// 版付きジョブは基盤がロックを取ってからハンドラに渡す（ハンドラ側で再取得しても取れる）
#[tokio::test]
async fn versioned_job_holds_track_lock_when_handler_runs() {
    let h = Harness::new();
    insert_track(&h.raw(), 1, 1, 1);
    let job = NewJob::new(
        JobType::Transcode,
        serde_json::json!({"track_id": 1, "audio_version": 1}),
    )
    .dedup_key("transcode:1:1");
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(job).await.unwrap() else {
        panic!()
    };
    let held = Arc::new(AtomicUsize::new(0));
    let mut reg = Registry::new();
    {
        let held = held.clone();
        reg.register_fn(JobType::Transcode, move |ctx| {
            let held = held.clone();
            async move {
                let job_id = ctx.job.id;
                let n: i64 = ctx
                    .db()
                    .read(move |c| {
                        Ok(c.query_row(
                            "SELECT count(*) FROM track_locks WHERE track_id = 1 AND job_id = ?1",
                            [job_id],
                            |r| r.get(0),
                        )?)
                    })
                    .await?;
                held.store(n as usize, Ordering::SeqCst);
                assert!(ctx.lock_tracks(&[1]).await?, "自分のロックは再取得できる");
                assert!(!ctx.is_stale().await?);
                Ok(Outcome::Done)
            }
        });
    }
    h.start(reg);
    wait_state(&h, id, JobState::Done).await;
    assert_eq!(held.load(Ordering::SeqCst), 1);
}

/// 対象トラックが消えている版付きジョブは、FK で落ちずに stale として done になる
#[tokio::test]
async fn versioned_job_for_missing_track_is_done_without_running() {
    let h = Harness::new();
    // track 1 は存在しない
    let job = NewJob::new(
        JobType::Tagwrite,
        serde_json::json!({"track_id": 1, "tag_version": 1}),
    )
    .dedup_key("tagwrite:1:1");
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(job).await.unwrap() else {
        panic!()
    };
    let runs = Arc::new(AtomicUsize::new(0));
    let mut reg = Registry::new();
    {
        let runs = runs.clone();
        reg.register_fn(JobType::Tagwrite, move |_ctx| {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(Outcome::Done)
            }
        });
    }
    h.start(reg);
    wait_state(&h, id, JobState::Done).await;
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    let locks: i64 = h
        .raw()
        .query_row("SELECT count(*) FROM track_locks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(locks, 0);
}

/// 版付き種別なのに track_id / 版が無い payload は投入時に拒否する
#[tokio::test]
async fn versioned_job_without_track_or_version_is_rejected_at_enqueue() {
    let h = Harness::new();
    for payload in [
        serde_json::json!({}),
        serde_json::json!({"track_id": 1}),
        serde_json::json!({"tag_version": 1}),
        serde_json::json!({"track_id": "x", "tag_version": 1}),
    ] {
        let job = NewJob::new(JobType::Tagwrite, payload.clone());
        assert!(
            h.jobs.enqueue(job).await.is_err(),
            "{payload} は拒否されるはず"
        );
    }
    let n: i64 = h
        .raw()
        .query_row("SELECT count(*) FROM jobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}

/// DB に直接入った不正 payload（版付きなのに track_id が無い）は、ゲートを迂回して
/// 実行されず、再試行もせずに failed になる
#[tokio::test]
async fn versioned_job_with_invalid_payload_in_db_fails_permanently() {
    let h = Harness::new();
    h.raw()
        .execute(
            "INSERT INTO jobs (id, type, payload, state, created_at) VALUES (7, 'transcode', '{}', 'queued', 0)",
            [],
        )
        .unwrap();
    let runs = Arc::new(AtomicUsize::new(0));
    let mut reg = Registry::new();
    {
        let runs = runs.clone();
        reg.register_fn(JobType::Transcode, move |_ctx| {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(Outcome::Done)
            }
        });
    }
    h.start(reg);
    wait_state(&h, 7, JobState::Failed).await;
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    let err: String = h
        .raw()
        .query_row("SELECT last_error FROM jobs WHERE id = 7", [], |r| r.get(0))
        .unwrap();
    assert!(err.contains("payload"), "{err}");
}

// ---------------------------------------------------------------- 停止

/// 停止済みの token でワーカーを始めても claim しない（停止後に副作用を始めない）
#[tokio::test]
async fn worker_does_not_claim_when_shutdown_is_already_cancelled() {
    let h = Harness::new();
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    let runs = Arc::new(AtomicUsize::new(0));
    let mut reg = Registry::new();
    {
        let runs = runs.clone();
        reg.register_fn(JobType::Scan, move |_ctx| {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(Outcome::Done)
            }
        });
    }
    h.shutdown.cancel();
    let worker = h.jobs.start(reg, h.shutdown.clone());
    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .expect("ワーカーがすぐ終わること")
        .unwrap();
    assert_eq!(state_of(&h.raw(), id), JobState::Queued);
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

/// 停止したワーカーは、claim 済みで未起動のジョブがあれば queued に戻して終わる
#[tokio::test]
async fn worker_shutdown_leaves_no_job_running_that_never_started() {
    let h = Harness::new();
    // 多数投入して、停止と claim ループが重なる状況を作る
    let mut ids = Vec::new();
    for i in 0..20 {
        let job = NewJob::new(JobType::Thumbnail, serde_json::json!({}))
            .dedup_key(format!("thumbnail:{i}"));
        let EnqueueResult::Inserted(id) = h.jobs.enqueue(job).await.unwrap() else {
            panic!()
        };
        ids.push(id);
    }
    let mut reg = Registry::new();
    reg.register_fn(JobType::Thumbnail, |_ctx| async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        Ok(Outcome::Done)
    });
    let worker = h.jobs.start(reg, h.shutdown.clone());
    tokio::time::sleep(Duration::from_millis(50)).await;
    h.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(3), worker)
        .await
        .expect("ワーカーが止まること")
        .unwrap();
    // 実行中のまま破棄されたものは running（リカバリ対象）、それ以外は queued。
    // running の数は並列度（thumbnail = 4）を超えない
    let conn = h.raw();
    let running: i64 = conn
        .query_row(
            "SELECT count(*) FROM jobs WHERE state = 'running'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let queued: i64 = conn
        .query_row(
            "SELECT count(*) FROM jobs WHERE state = 'queued'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(running <= 4, "running = {running}");
    assert_eq!(running + queued, 20);
}

// ---------------------------------------------------------------- CPU 系の共通予算（P4-1、D-73）

/// rg / transcode / flaccheck / hirescheck は種別ごとの上限に加えて共通の予算（= コア数）を取る。
/// コア数 2 で rg（上限 2）と flaccheck（上限 2）を同時に投入しても、実行中の合計は 2 を超えない
#[tokio::test]
async fn cpu_bound_types_share_a_budget_of_cpus() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let h = Harness::with_cpus(2);
    assert_eq!(h.jobs.cpus(), 2);
    assert!(JobType::Rg.cpu_bound() && JobType::Flaccheck.cpu_bound());
    assert!(JobType::Transcode.cpu_bound() && JobType::Hirescheck.cpu_bound());
    assert!(!JobType::Thumbnail.cpu_bound() && !JobType::Scan.cpu_bound());
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut reg = Registry::new();
    for ty in [JobType::Rg, JobType::Flaccheck] {
        let running = running.clone();
        let peak = peak.clone();
        reg.register_fn(ty, move |_ctx| {
            let running = running.clone();
            let peak = peak.clone();
            async move {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(300)).await;
                running.fetch_sub(1, Ordering::SeqCst);
                Ok(Outcome::Done)
            }
        });
    }
    // flaccheck は版付き（track_id + audio_version）なのでトラックを置く
    {
        let conn = h.raw();
        for i in 0..3 {
            insert_track(&conn, 100 + i, 1, 1);
        }
    }
    let mut ids = Vec::new();
    for i in 0..3 {
        let rg = NewJob::new(JobType::Rg, serde_json::json!({})).dedup_key(format!("rg:{i}"));
        let fc = NewJob::new(
            JobType::Flaccheck,
            serde_json::json!({ "track_id": 100 + i, "audio_version": 1 }),
        )
        .dedup_key(format!("flaccheck:{i}"));
        for job in [rg, fc] {
            let EnqueueResult::Inserted(id) = h.jobs.enqueue(job).await.unwrap() else {
                panic!()
            };
            ids.push(id);
        }
    }
    h.start(reg);
    for id in &ids {
        wait_state(&h, *id, JobState::Done).await;
    }
    assert_eq!(peak.load(Ordering::SeqCst), 2, "予算 = コア数を超えない");
}

/// 予算は種別間で公平に配る。4 種を 3 本ずつ（先頭の種別名 flaccheck から順に）投入しても、先頭の
/// キューが尽きる前に残りの種別が始まる（ラウンドロビン。D-73 追記）。合計はコア数を超えない
#[tokio::test]
async fn cpu_budget_is_shared_round_robin_across_types() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    let h = Harness::with_cpus(2);
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let started: Arc<Mutex<Vec<JobType>>> = Arc::new(Mutex::new(Vec::new()));
    let mut reg = Registry::new();
    let types = [
        JobType::Flaccheck,
        JobType::Hirescheck,
        JobType::Rg,
        JobType::Transcode,
    ];
    for ty in types {
        let running = running.clone();
        let peak = peak.clone();
        let started = started.clone();
        reg.register_fn(ty, move |_ctx| {
            let running = running.clone();
            let peak = peak.clone();
            let started = started.clone();
            async move {
                started.lock().unwrap().push(ty);
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(200)).await;
                running.fetch_sub(1, Ordering::SeqCst);
                Ok(Outcome::Done)
            }
        });
    }
    {
        let conn = h.raw();
        for i in 0..12 {
            insert_track(&conn, 200 + i, 1, 1);
        }
    }
    // 種別名順に固めて投入する（flaccheck ×3 → hirescheck ×3 → rg ×3 → transcode ×3）
    let mut ids = Vec::new();
    let mut n = 0;
    for ty in types {
        for _ in 0..3 {
            let payload = if ty.version_field().is_some() {
                serde_json::json!({ "track_id": 200 + n, "audio_version": 1 })
            } else {
                serde_json::json!({})
            };
            let job = NewJob::new(ty, payload).dedup_key(format!("{ty}:{n}"));
            let EnqueueResult::Inserted(id) = h.jobs.enqueue(job).await.unwrap() else {
                panic!()
            };
            ids.push(id);
            n += 1;
        }
    }
    h.start(reg);
    for id in &ids {
        wait_state(&h, *id, JobState::Done).await;
    }
    assert_eq!(peak.load(Ordering::SeqCst), 2);
    let order = started.lock().unwrap().clone();
    assert_eq!(order.len(), 12);
    // 先頭 6 本の開始で 4 種すべてが出ている（固定順なら flaccheck ×3 → hirescheck ×3 が先に並ぶ）
    let first: std::collections::HashSet<JobType> = order.iter().take(6).copied().collect();
    assert_eq!(first.len(), 4, "開始順 {order:?}");
}

/// 予算に入らない種別は今までどおり種別の上限だけ（thumbnail = 4 は CPU 予算 2 に縛られない）
#[tokio::test]
async fn non_cpu_types_are_not_limited_by_the_budget() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let h = Harness::with_cpus(2);
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut reg = Registry::new();
    {
        let running = running.clone();
        let peak = peak.clone();
        reg.register_fn(JobType::Thumbnail, move |_ctx| {
            let running = running.clone();
            let peak = peak.clone();
            async move {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(300)).await;
                running.fetch_sub(1, Ordering::SeqCst);
                Ok(Outcome::Done)
            }
        });
    }
    let mut ids = Vec::new();
    for i in 0..4 {
        let job = NewJob::new(JobType::Thumbnail, serde_json::json!({}))
            .dedup_key(format!("thumbnail:{i}"));
        let EnqueueResult::Inserted(id) = h.jobs.enqueue(job).await.unwrap() else {
            panic!()
        };
        ids.push(id);
    }
    h.start(reg);
    for id in &ids {
        wait_state(&h, *id, JobState::Done).await;
    }
    assert_eq!(peak.load(Ordering::SeqCst), 4);
}

// ---------------------------------------------------------------- HTTP API

struct TestApp {
    router: axum::Router,
    jobs: Arc<Jobs>,
    shutdown: CancellationToken,
    _dir: tempfile::TempDir,
}

async fn app() -> TestApp {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
    let config = Arc::new(Config::parse(EXAMPLE).unwrap());
    let mode = auth::bootstrap(&db, Some("correct horse".into()))
        .await
        .unwrap();
    let state = AppState::new(config, db, mode);
    TestApp {
        jobs: state.jobs.clone(),
        shutdown: state.shutdown.clone(),
        router: api::router(state),
        _dir: dir,
    }
}

fn req(method: Method, uri: &str) -> axum::http::request::Builder {
    let peer: std::net::SocketAddr = LAN.parse().unwrap();
    let mut b = Request::builder().method(method).uri(uri);
    b.extensions_mut().unwrap().insert(ConnectInfo(peer));
    b
}

async fn send(app: &TestApp, r: Request<Body>) -> axum::response::Response {
    app.router.clone().oneshot(r).await.unwrap()
}

async fn json(res: axum::response::Response) -> serde_json::Value {
    let body = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
}

async fn cookie(app: &TestApp) -> String {
    let r = req(Method::POST, "/api/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .header("sec-fetch-site", "same-origin")
        .body(Body::from(r#"{"password":"correct horse"}"#))
        .unwrap();
    let res = send(app, r).await;
    assert_eq!(res.status(), StatusCode::OK);
    let set = res
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    set.split(';').next().unwrap().to_string()
}

#[tokio::test]
async fn jobs_api_requires_session() {
    let app = app().await;
    for (m, uri) in [
        (Method::GET, "/api/jobs"),
        (Method::POST, "/api/jobs/1/cancel"),
        (Method::POST, "/api/jobs/1/retry"),
        (Method::GET, "/api/events"),
    ] {
        let res = send(
            &app,
            req(m.clone(), uri)
                .header("sec-fetch-site", "same-origin")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{m} {uri}");
    }
}

#[tokio::test]
async fn jobs_list_returns_items_and_summary() {
    let app = app().await;
    let c = cookie(&app).await;
    let EnqueueResult::Inserted(id) = app.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    let failed = NewJob::new(JobType::Gc, serde_json::json!({})).dedup_key("gc");
    let EnqueueResult::Inserted(failed_id) = app.jobs.enqueue(failed).await.unwrap() else {
        panic!()
    };
    app.jobs
        .db()
        .write(move |c| {
            c.execute(
                "UPDATE jobs SET state = 'failed', last_error = 'x', finished_at = 1 WHERE id = ?1",
                [failed_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let res = send(
        &app,
        req(Method::GET, "/api/jobs")
            .header(header::COOKIE, &c)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = json(res).await;
    assert_eq!(body["summary"]["queued"], 1);
    assert_eq!(body["summary"]["running"], 0);
    assert_eq!(body["summary"]["failed"], 1);
    assert_eq!(body["summary"]["done"], 0);
    assert_eq!(body["summary"]["cancelled"], 0);
    assert_eq!(body["summary"]["pending_ops"], 0);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let item = items.iter().find(|i| i["id"] == id).unwrap();
    assert_eq!(item["type"], "scan");
    assert_eq!(item["state"], "queued");
    assert_eq!(item["attempts"], 0);
    assert!(item.get("run_after").is_some());
    assert!(item.get("edit_batch_id").is_some());
    assert!(item.get("created_at").is_some());
    assert!(item.get("started_at").is_some());
    // 種別ごとの並列度（SPEC §12.5 のジョブ画面が出す）。scan は常に 1、rg はコア数
    let conc = body["concurrency"].as_object().unwrap();
    assert_eq!(
        body["cpu_budget"], conc["rg"],
        "CPU 系の共通予算 = コア数（D-73）"
    );
    assert_eq!(conc["scan"], 1);
    assert_eq!(conc["gc"], 1);
    assert!(conc["rg"].as_u64().unwrap() >= 1);
    assert!(conc.contains_key("transcode"));
    assert!(item.get("last_error").is_some());
    assert!(item.get("progress").is_some());
    assert!(item.get("done").is_some());
    assert!(item.get("total").is_some());
    // 種別ごとの件数はサーバが全件を集計する（items は上限付きなので web で数えると欠ける）
    let by_type = body["by_type"].as_object().unwrap();
    assert_eq!(by_type["scan"]["queued"], 1);
    assert_eq!(by_type["scan"]["running"], 0);
    assert_eq!(by_type["scan"]["failed"], 0);
    assert_eq!(by_type["scan"]["done"], 0);
    assert_eq!(by_type["gc"]["failed"], 1);
    assert_eq!(by_type["gc"]["queued"], 0);
    assert!(by_type.get("rg").is_none(), "件数 0 の種別は返さない");
    // 終端（done / cancelled）も数える（ジョブ画面の「完了」タブと種別表の完了列）
    app.jobs
        .db()
        .write(move |c| {
            c.execute(
                "UPDATE jobs SET state = 'done', finished_at = 2 WHERE id = ?1",
                [id],
            )?;
            c.execute(
                "UPDATE jobs SET state = 'cancelled', finished_at = 3 WHERE id = ?1",
                [failed_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let res = send(
        &app,
        req(Method::GET, "/api/jobs")
            .header(header::COOKIE, &c)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let body = json(res).await;
    assert_eq!(body["summary"]["done"], 1);
    assert_eq!(body["summary"]["cancelled"], 1);
    assert_eq!(body["summary"]["queued"], 0);
    assert_eq!(body["by_type"]["scan"]["done"], 1);
    assert_eq!(body["by_type"]["gc"]["cancelled"], 1);
    assert_eq!(body["by_type"]["gc"]["failed"], 0);
}

/// 一覧は状態ごとの切り出し（実行中・待ち ≤ 1,000、完了 ≤ 300、失敗・取り消し ≤ 300）。待ちが上限を超えて
/// いても完了・失敗が届く（1 本の並びで切ると待ちだけで埋まり、完了タブが空になる）
#[tokio::test]
async fn jobs_list_returns_recent_terminal_jobs_even_when_the_queue_is_long() {
    let app = app().await;
    let c = cookie(&app).await;
    app.jobs
        .db()
        .write(|c| {
            let tx = c.transaction()?;
            {
                let mut st = tx.prepare(
                    "INSERT INTO jobs (type, payload, state, created_at, finished_at, max_attempts)
                     VALUES ('gc', '{}', ?1, ?2, ?3, 1)",
                )?;
                for i in 0..1_100i64 {
                    st.execute(rusqlite::params!["queued", 100 + i, Option::<i64>::None])?;
                }
                for i in 0..310i64 {
                    st.execute(rusqlite::params!["done", 50, Some(200 + i)])?;
                }
                for i in 0..5i64 {
                    st.execute(rusqlite::params!["failed", 50, Some(900 + i)])?;
                }
                st.execute(rusqlite::params!["cancelled", 50, Some(950)])?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .unwrap();
    let res = send(
        &app,
        req(Method::GET, "/api/jobs")
            .header(header::COOKIE, &c)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let body = json(res).await;
    let items = body["items"].as_array().unwrap();
    let count = |st: &str| items.iter().filter(|i| i["state"] == st).count();
    assert_eq!(count("queued"), 1_000, "待ちは上限まで");
    assert_eq!(count("done"), 300, "完了は新しい順に上限まで");
    assert_eq!(count("failed"), 5);
    assert_eq!(count("cancelled"), 1);
    // 完了は finished_at の新しい順（一番古い 10 本が落ちる）
    let oldest_done = items
        .iter()
        .filter(|i| i["state"] == "done")
        .map(|i| i["finished_at"].as_i64().unwrap())
        .min()
        .unwrap();
    assert_eq!(oldest_done, 210);
    assert_eq!(body["limits"]["active"], 1_000);
    assert_eq!(body["limits"]["done"], 300);
    assert_eq!(body["limits"]["failed"], 300);
    assert_eq!(body["summary"]["queued"], 1_100);
    assert_eq!(body["summary"]["done"], 310);
}

/// 一覧の `subject`（ジョブの対象。payload の track_id / album_id / batch_id を解決した表示用の文字列）
#[tokio::test]
async fn jobs_list_resolves_the_subject_of_each_job() {
    let app = app().await;
    let c = cookie(&app).await;
    app.jobs
        .db()
        .write(|c| {
            c.execute_batch(
                "INSERT INTO albums (id, rel_dir, rel_dir_key, albumartist, album)
                   VALUES (7, 'Anime/AA/Album', 'anime/aa/album', 'AA', 'Album');
                 INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                                     title, artist_display, album, albumartist, seen_at, album_id)
                   VALUES (5, 'Anime/AA/Album/1-01 曲.flac', 'anime/aa/album/1-01 曲.flac', 0, 0, 0, 'flac', 1,
                           '曲', 'a', 'Album', 'AA', 0, 7);
                 INSERT INTO edit_batches (id, description, created_at)
                   VALUES (3, 'テンプレート変更', 0);",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let jobs = [
        NewJob::new(
            JobType::Transcode,
            serde_json::json!({ "track_id": 5, "variant": "aac", "audio_version": 1, "tag_version": 1 }),
        ),
        NewJob::new(JobType::Rg, serde_json::json!({ "album_id": 7 })),
        NewJob::new(JobType::Rg, serde_json::json!({ "track_id": 5 })),
        NewJob::new(JobType::Rename, serde_json::json!({ "batch_id": 3 })),
        NewJob::new(JobType::Scan, serde_json::json!({ "kind": "deep" })),
        NewJob::new(
            JobType::Ytdl,
            serde_json::json!({ "url": "https://music.youtube.com/watch?v=x" }),
        ),
        NewJob::new(JobType::Thumbnail, serde_json::json!({ "artwork_id": 9 })),
        NewJob::new(
            JobType::Flaccheck,
            serde_json::json!({ "track_id": 404, "audio_version": 1 }),
        ),
        NewJob::new(JobType::Gc, serde_json::json!({})),
    ];
    let mut ids = Vec::new();
    for j in jobs {
        let EnqueueResult::Inserted(id) = app.jobs.enqueue(j).await.unwrap() else {
            panic!()
        };
        ids.push(id);
    }
    let res = send(
        &app,
        req(Method::GET, "/api/jobs")
            .header(header::COOKIE, &c)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let body = json(res).await;
    let subject = |id: i64| -> serde_json::Value {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == id)
            .unwrap()["subject"]
            .clone()
    };
    assert_eq!(subject(ids[0]), "Anime/AA/Album/1-01 曲.flac [aac]");
    assert_eq!(subject(ids[1]), "Anime/AA/Album/");
    assert_eq!(subject(ids[2]), "Anime/AA/Album/1-01 曲.flac");
    assert_eq!(subject(ids[3]), "テンプレート変更");
    assert_eq!(subject(ids[4]), "deep");
    assert_eq!(subject(ids[5]), "https://music.youtube.com/watch?v=x");
    assert_eq!(subject(ids[6]), "artwork #9");
    assert_eq!(subject(ids[7]), "track #404", "行が無いトラックは id で");
    assert_eq!(subject(ids[8]), serde_json::Value::Null);
}

/// 一覧は実行中が先頭、待ちはキューから取られる順（priority → created_at → id 昇順）。同じ秒に
/// 数千件を一括投入すると id 降順では実行中（古い id）が上限の外に落ちる
#[tokio::test]
async fn jobs_list_puts_running_first_and_queued_in_dequeue_order() {
    let app = app().await;
    let c = cookie(&app).await;
    let mut ids = Vec::new();
    for i in 0..4 {
        let j =
            NewJob::new(JobType::Gc, serde_json::json!({ "i": i })).dedup_key(format!("gc:{i}"));
        let EnqueueResult::Inserted(id) = app.jobs.enqueue(j).await.unwrap() else {
            panic!()
        };
        ids.push(id);
    }
    let (first, third) = (ids[0], ids[2]);
    app.jobs
        .db()
        .write(move |c| {
            // 全部同じ秒に作られ、先頭が実行中、3 つ目は高優先
            c.execute("UPDATE jobs SET created_at = 100", [])?;
            c.execute(
                "UPDATE jobs SET state = 'running', started_at = 101 WHERE id = ?1",
                [first],
            )?;
            c.execute("UPDATE jobs SET priority = 5 WHERE id = ?1", [third])?;
            Ok(())
        })
        .await
        .unwrap();
    let res = send(
        &app,
        req(Method::GET, "/api/jobs")
            .header(header::COOKIE, &c)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let body = json(res).await;
    let order: Vec<i64> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_i64().unwrap())
        .collect();
    assert_eq!(order, vec![ids[0], ids[2], ids[1], ids[3]]);
    assert_eq!(body["by_type"]["gc"]["running"], 1);
    assert_eq!(body["by_type"]["gc"]["queued"], 3);
}

#[tokio::test]
async fn jobs_cancel_and_retry_endpoints() {
    let app = app().await;
    let c = cookie(&app).await;
    let EnqueueResult::Inserted(id) = app.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    let post = |uri: String| {
        req(Method::POST, &uri)
            .header(header::COOKIE, &c)
            .header("sec-fetch-site", "same-origin")
            .body(Body::empty())
            .unwrap()
    };

    // queued → cancel は 202
    let res = send(&app, post(format!("/api/jobs/{id}/cancel"))).await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    // 終端を再度 cancel は 409
    let res = send(&app, post(format!("/api/jobs/{id}/cancel"))).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(json(res).await["error"], "not_cancellable");
    // 存在しない id は 404
    let res = send(&app, post("/api/jobs/424242/cancel".into())).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // cancelled → retry は 202 で queued に戻る
    let res = send(&app, post(format!("/api/jobs/{id}/retry"))).await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let job = app.jobs.get(id).await.unwrap().unwrap();
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(job.attempts, 0);
    assert!(job.cancel_requested_at.is_none());
    assert!(job.finished_at.is_none());
    // queued を retry は 409
    let res = send(&app, post(format!("/api/jobs/{id}/retry"))).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(json(res).await["error"], "not_retryable");
    let res = send(&app, post("/api/jobs/424242/retry".into())).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn retry_is_rejected_when_same_key_is_active() {
    let h = Harness::new();
    let EnqueueResult::Inserted(a) = h.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    assert_eq!(h.jobs.cancel(a).await.unwrap(), CancelOutcome::Cancelled);
    let EnqueueResult::Inserted(_b) = h.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    assert_eq!(h.jobs.retry(a).await.unwrap(), RetryOutcome::Duplicate);
    assert_eq!(h.jobs.retry(9999).await.unwrap(), RetryOutcome::NotFound);
}

#[tokio::test]
async fn events_endpoint_streams_job_events() {
    let app = app().await;
    let c = cookie(&app).await;
    let res = send(
        &app,
        req(Method::GET, "/api/events")
            .header(header::COOKIE, &c)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let ct = res
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.starts_with("text/event-stream"), "{ct}");

    let EnqueueResult::Inserted(id) = app.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    app.jobs.cancel(id).await.unwrap();

    let mut body = res.into_body();
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !buf.contains("\"state\":\"cancelled\"") {
        let frame = tokio::time::timeout_at(deadline, body.frame())
            .await
            .expect("イベントが届くこと")
            .expect("ストリームが閉じないこと")
            .unwrap();
        if let Some(data) = frame.data_ref() {
            buf.push_str(std::str::from_utf8(data).unwrap());
        }
    }
    assert!(buf.contains("event: job"), "{buf}");
    assert!(buf.contains(&format!("\"id\":{id}")), "{buf}");
}

#[tokio::test]
async fn events_stream_ends_on_shutdown() {
    let app = app().await;
    let c = cookie(&app).await;
    let res = send(
        &app,
        req(Method::GET, "/api/events")
            .header(header::COOKIE, &c)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let mut body = res.into_body();
    app.shutdown.cancel();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let frame = tokio::time::timeout_at(deadline, body.frame())
            .await
            .expect("停止でストリームが閉じること");
        if frame.is_none() {
            break;
        }
    }
}

#[tokio::test]
async fn events_stream_signals_resync_when_lagged() {
    let app = app().await;
    let c = cookie(&app).await;
    let res = send(
        &app,
        req(Method::GET, "/api/events")
            .header(header::COOKIE, &c)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    // 購読者が読まないまま容量を超えて流す
    for i in 0..(jobs::EVENT_CAPACITY as i64 + 100) {
        app.jobs.publish(Event::Job(jobs::JobEvent {
            id: i,
            state: JobState::Queued,
            progress: None,
            done: None,
            total: None,
        }));
    }
    let mut body = res.into_body();
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !buf.contains("event: resync") {
        let frame = tokio::time::timeout_at(deadline, body.frame())
            .await
            .expect("resync が届くこと")
            .expect("ストリームが閉じないこと")
            .unwrap();
        if let Some(data) = frame.data_ref() {
            buf.push_str(std::str::from_utf8(data).unwrap());
        }
    }
    // resync は取りこぼした後続イベントより前に出る
    let resync_at = buf.find("event: resync").unwrap();
    let first_job = buf.find("event: job").unwrap_or(usize::MAX);
    assert!(resync_at < first_job, "{buf}");
}

/// P4-13: `GET /api/jobs?type=<種別>` はその種別だけの一覧（上限も種別内で数える。YouTube 画面が
/// 他種別の完了 300 件に押し出されない）。summary / by_type は全件のまま。不明な種別は 400
#[tokio::test]
async fn jobs_list_can_be_filtered_by_type() {
    let app = app().await;
    let c = cookie(&app).await;
    let EnqueueResult::Inserted(scan_id) = app.jobs.enqueue(scan_job()).await.unwrap() else {
        panic!()
    };
    let ytdl = NewJob::new(
        JobType::Ytdl,
        serde_json::json!({"url": "https://youtu.be/x"}),
    )
    .dedup_key("ytdl:x");
    let EnqueueResult::Inserted(ytdl_id) = app.jobs.enqueue(ytdl).await.unwrap() else {
        panic!()
    };
    app.jobs
        .db()
        .write(move |c| {
            c.execute(
                "UPDATE jobs SET state = 'done', finished_at = 1 WHERE id = ?1",
                [ytdl_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let res = send(
        &app,
        req(Method::GET, "/api/jobs?type=ytdl")
            .header(header::COOKIE, &c)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = json(res).await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], ytdl_id);
    assert_eq!(items[0]["type"], "ytdl");
    assert_eq!(body["summary"]["queued"], 1, "summary は全件");
    assert_eq!(body["by_type"]["scan"]["queued"], 1);
    let _ = scan_id;

    let res = send(
        &app,
        req(Method::GET, "/api/jobs?type=nope")
            .header(header::COOKIE, &c)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

/// P4-13: ハンドラが `Outcome::DoneWith(note)` を返すと、完了時の結果 1 行が `jobs.note` に残り
/// `GET /api/jobs` の `note` で読める（ytdl の「Inbox に置いた: …」「プラグインが skip: …」等）
#[tokio::test]
async fn done_with_note_persists_the_note() {
    let h = Harness::new();
    let job = NewJob::new(JobType::Ytdl, serde_json::json!({ "url": "u" })).dedup_key("ytdl:u");
    let EnqueueResult::Inserted(id) = h.jobs.enqueue(job).await.unwrap() else {
        panic!()
    };
    let mut reg = Registry::new();
    reg.register_fn(JobType::Ytdl, |_ctx| async {
        Ok(Outcome::DoneWith("Inbox に置いた: youtube/x/y.opus".into()))
    });
    h.start(reg);
    wait_state(&h, id, JobState::Done).await;
    let note: Option<String> = h
        .raw()
        .query_row("SELECT note FROM jobs WHERE id = ?1", [id], |r| r.get(0))
        .unwrap();
    assert_eq!(note.as_deref(), Some("Inbox に置いた: youtube/x/y.opus"));
    let got = h.jobs.get(id).await.unwrap().unwrap();
    assert_eq!(
        got.note.as_deref(),
        Some("Inbox に置いた: youtube/x/y.opus")
    );
}

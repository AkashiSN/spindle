//! `flaccheck` ジョブ（SPEC §7.9 / §8、docs/TASKS.md P1-5、D-57）。`flac -t` と STREAMINFO の MD5 から
//! ok / md5_missing / decode_error を版付きで記録する。ファイルは書かない。
//! 合成ファイルは ffmpeg で作り、検査は flac で行う（無ければ skip）

#![cfg(target_os = "linux")]

mod common;

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{params, Connection};
use tokio_util::sync::CancellationToken;

use spindle::db::{flaccheck as dbfc, Db};
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, Scanner};
use spindle::jobs::handlers::flaccheck::{new_flaccheck_job, FlaccheckHandler};
use spindle::jobs::{EnqueueResult, JobState, JobType, Jobs, Registry};

/// STREAMINFO の MD5 のファイル内オフセット（"fLaC" 4 + ブロックヘッダ 4 + 18）
const MD5_OFFSET: usize = 26;

fn flac_available() -> bool {
    Command::new("flac")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    db: Arc<Db>,
    jobs: Arc<Jobs>,
    scanner: Scanner,
    root: Arc<RootDir>,
    shutdown: CancellationToken,
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Library")).unwrap();
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let root = Arc::new(RootDir::open(&dir.path().join("Library")).unwrap());
        let jobs = Jobs::new(db.clone());
        let scanner = Scanner::new(db.clone(), root.clone(), 2);
        Self {
            dir,
            db_path,
            db,
            jobs,
            scanner,
            root,
            shutdown: CancellationToken::new(),
        }
    }

    fn start(&self) {
        self.start_with("flac");
    }

    fn start_with(&self, flac: &str) {
        let mut reg = Registry::new();
        reg.register(
            JobType::Flaccheck,
            Arc::new(FlaccheckHandler::new(self.root.clone(), flac)),
        );
        self.jobs.start(reg, self.shutdown.clone());
    }

    fn lib(&self) -> PathBuf {
        self.dir.path().join("Library")
    }

    fn add(&self, rel: &str, seed: u32) -> PathBuf {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        let ext = rel.rsplit_once('.').unwrap().1;
        let made = common::make_audio(p.parent().unwrap(), &name, ext, seed).unwrap();
        common::set_basic_tags(&made, "t", "Artist", "Album", "Artist", 1, 1);
        made
    }

    async fn scan(&self) {
        self.scanner
            .run(
                ScanKind::Incremental,
                Arc::new(|_, _, _| {}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.db_path).unwrap()
    }

    fn track(&self, rel: &str) -> (i64, i64) {
        self.conn()
            .query_row(
                "SELECT id, audio_version FROM tracks WHERE rel_path = ?1",
                [rel],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    fn result(&self, rel: &str) -> Check {
        self.conn()
            .query_row(
                "SELECT flac_check, flac_checked_at, flac_check_version, flac_check_error, audio_md5 IS NULL
                 FROM tracks WHERE rel_path = ?1",
                [rel],
                |r| {
                    Ok(Check {
                        status: r.get(0)?,
                        checked_at: r.get(1)?,
                        version: r.get(2)?,
                        error: r.get(3)?,
                        md5_null: r.get(4)?,
                    })
                },
            )
            .unwrap()
    }

    async fn run(&self, rel: &str) -> JobState {
        let (id, ver) = self.track(rel);
        let job = match self.jobs.enqueue(new_flaccheck_job(id, ver)).await.unwrap() {
            EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
        };
        self.wait_job(job).await
    }

    async fn wait_job(&self, id: i64) -> JobState {
        for _ in 0..3000 {
            let s: String = self
                .conn()
                .query_row("SELECT state FROM jobs WHERE id = ?1", [id], |r| r.get(0))
                .unwrap();
            let st: JobState = s.parse().unwrap();
            if st.is_terminal() {
                return st;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let row: (String, Option<i64>, i64, Option<String>) = self
            .conn()
            .query_row(
                "SELECT state, run_after, attempts, last_error FROM jobs WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        panic!("job {id} が終端にならない: {row:?}");
    }
}

impl Drop for Lib {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[derive(Debug)]
struct Check {
    status: Option<String>,
    checked_at: Option<i64>,
    version: Option<i64>,
    error: Option<String>,
    md5_null: bool,
}

/// STREAMINFO の MD5 を全ゼロにする（古いエンコーダの出力を模す）。mtime は据え置く
fn zero_md5(p: &std::path::Path) {
    let meta = std::fs::metadata(p).unwrap();
    let mut bytes = std::fs::read(p).unwrap();
    assert_eq!(&bytes[..4], b"fLaC");
    bytes[MD5_OFFSET..MD5_OFFSET + 16].fill(0);
    std::fs::write(p, bytes).unwrap();
    std::fs::File::open(p)
        .unwrap()
        .set_modified(meta.modified().unwrap())
        .unwrap();
}

/// フレームの途中を壊す（デコードエラー）
fn corrupt(p: &std::path::Path) {
    let mut bytes = std::fs::read(p).unwrap();
    let mid = bytes.len() / 2;
    for b in &mut bytes[mid..mid + 64] {
        *b ^= 0xff;
    }
    std::fs::write(p, bytes).unwrap();
}

#[tokio::test]
async fn healthy_flac_is_ok_and_result_carries_the_audio_version() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    if !flac_available() {
        eprintln!("flac が無いので skip");
        return;
    }
    let lib = Lib::new();
    lib.add("A/ok.flac", 1);
    lib.scan().await;
    lib.start();
    assert_eq!(lib.run("A/ok.flac").await, JobState::Done);
    let r = lib.result("A/ok.flac");
    assert_eq!(r.status.as_deref(), Some("ok"), "{r:?}");
    assert!(r.checked_at.is_some());
    assert_eq!(r.version, Some(1));
    assert_eq!(r.error, None);
    assert!(!r.md5_null);
    // 同じ版の再投入は dedup、終わった後は再投入できて結果は同じ
    assert_eq!(lib.run("A/ok.flac").await, JobState::Done);
    assert_eq!(lib.result("A/ok.flac").status.as_deref(), Some("ok"));
}

#[tokio::test]
async fn zeroed_md5_is_reported_as_md5_missing_without_touching_the_file() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    if !flac_available() {
        return;
    }
    let lib = Lib::new();
    let p = lib.add("A/nomd5.flac", 2);
    zero_md5(&p);
    lib.scan().await;
    let before = std::fs::metadata(&p).unwrap();
    lib.start();
    assert_eq!(lib.run("A/nomd5.flac").await, JobState::Done);
    let r = lib.result("A/nomd5.flac");
    assert_eq!(r.status.as_deref(), Some("md5_missing"), "{r:?}");
    assert!(r.md5_null, "スキャナも MD5 無しとして扱っている");
    let after = std::fs::metadata(&p).unwrap();
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    assert_eq!(before.len(), after.len());
}

#[tokio::test]
async fn corrupted_flac_is_reported_as_decode_error_with_flac_stderr() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    if !flac_available() {
        return;
    }
    let lib = Lib::new();
    let p = lib.add("A/bad.flac", 3);
    lib.scan().await;
    corrupt(&p);
    // 壊した後の実体を DB に追随させる（壊す前の inode/mtime のままだと照合で弾かれる）
    lib.scan().await;
    lib.start();
    assert_eq!(lib.run("A/bad.flac").await, JobState::Done);
    let r = lib.result("A/bad.flac");
    assert_eq!(r.status.as_deref(), Some("decode_error"), "{r:?}");
    assert!(r.error.as_deref().is_some_and(|e| !e.is_empty()), "{r:?}");
}

#[tokio::test]
async fn non_flac_and_missing_tracks_are_no_ops() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    if !flac_available() {
        return;
    }
    let lib = Lib::new();
    lib.add("A/x.opus", 4);
    let gone = lib.add("A/gone.flac", 5);
    lib.scan().await;
    std::fs::remove_file(&gone).unwrap();
    lib.scan().await;
    lib.start();
    assert_eq!(lib.run("A/x.opus").await, JobState::Done);
    assert_eq!(lib.result("A/x.opus").status, None);
    assert_eq!(lib.run("A/gone.flac").await, JobState::Done);
    assert_eq!(lib.result("A/gone.flac").status, None);
}

/// 行の dev だけが古い（ホスト再起動で振り直された）ときは同じ実体として結果を書く（D-62）
#[tokio::test]
async fn row_with_stale_dev_only_is_still_checked() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    if !flac_available() {
        return;
    }
    let lib = Lib::new();
    lib.add("A/d.flac", 6);
    lib.scan().await;
    let (id, _) = lib.track("A/d.flac");
    lib.conn()
        .execute("UPDATE tracks SET dev = dev + 1 WHERE id = ?1", [id])
        .unwrap();
    lib.start();
    assert_eq!(lib.run("A/d.flac").await, JobState::Done);
    assert_eq!(lib.result("A/d.flac").status.as_deref(), Some("ok"));
}

#[tokio::test]
async fn stale_version_and_replaced_file_do_not_write_a_result() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    if !flac_available() {
        return;
    }
    let lib = Lib::new();
    let p = lib.add("A/v.flac", 6);
    lib.scan().await;
    let (id, ver) = lib.track("A/v.flac");
    // 版が進んだ後に古い版のジョブが走る → stale ゲートで no-op
    lib.conn()
        .execute(
            "UPDATE tracks SET audio_version = audio_version + 1 WHERE id = ?1",
            [id],
        )
        .unwrap();
    lib.start();
    let job = match lib.jobs.enqueue(new_flaccheck_job(id, ver)).await.unwrap() {
        EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
    };
    assert_eq!(lib.wait_job(job).await, JobState::Done);
    assert_eq!(lib.result("A/v.flac").status, None);
    // 同名で差し替えられた（inode が DB と違う）→ 書かずに done
    let bytes = std::fs::read(&p).unwrap();
    std::fs::remove_file(&p).unwrap();
    std::fs::write(&p, bytes).unwrap();
    assert_eq!(lib.run("A/v.flac").await, JobState::Done);
    assert_eq!(lib.result("A/v.flac").status, None);
    // FLAC ですらないバイト列に差し替えられても、照合が先なので STREAMINFO を読まずに done
    std::fs::remove_file(&p).unwrap();
    std::fs::write(&p, b"not a flac").unwrap();
    lib.conn()
        .execute(
            "UPDATE tracks SET audio_version = audio_version + 1 WHERE id = ?1",
            [id],
        )
        .unwrap();
    assert_eq!(lib.run("A/v.flac").await, JobState::Done);
    assert_eq!(lib.result("A/v.flac").status, None);
    let failed: i64 = lib
        .conn()
        .query_row(
            "SELECT count(*) FROM jobs WHERE type = 'flaccheck' AND (state = 'failed' OR last_error IS NOT NULL)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(failed, 0);
}

#[tokio::test]
async fn missing_flac_binary_fails_the_job() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new();
    lib.add("A/ok.flac", 7);
    lib.scan().await;
    lib.start_with("/nonexistent/flac");
    let (id, ver) = lib.track("A/ok.flac");
    let job = match lib.jobs.enqueue(new_flaccheck_job(id, ver)).await.unwrap() {
        EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
    };
    for _ in 0..3000 {
        let e: Option<String> = lib
            .conn()
            .query_row("SELECT last_error FROM jobs WHERE id = ?1", [job], |r| {
                r.get(0)
            })
            .unwrap();
        if e.is_some() {
            assert_eq!(lib.result("A/ok.flac").status, None);
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("失敗が記録されない");
}

#[tokio::test]
async fn enqueue_all_unchecked_targets_flac_without_a_current_result() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    if !flac_available() {
        return;
    }
    let lib = Lib::new();
    lib.add("A/a.flac", 8);
    lib.add("A/b.flac", 9);
    lib.add("A/c.opus", 10);
    lib.scan().await;
    lib.start();
    assert_eq!(lib.run("A/a.flac").await, JobState::Done);
    // a は現在の版の結果がある、b は無い、c は FLAC でない
    let ids = lib
        .db
        .write(|c| dbfc::enqueue_all_unchecked(c, spindle::db::now_epoch()))
        .await
        .unwrap();
    assert_eq!(ids.len(), 1);
    let (b_id, _) = lib.track("A/b.flac");
    let target: i64 = lib
        .conn()
        .query_row(
            "SELECT json_extract(payload, '$.track_id') FROM jobs WHERE id = ?1",
            [ids[0]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(target, b_id);
    assert_eq!(lib.wait_job(ids[0]).await, JobState::Done);
    assert_eq!(lib.result("A/b.flac").status.as_deref(), Some("ok"));
    // 版が進めば a も対象に戻る
    lib.conn()
        .execute(
            "UPDATE tracks SET audio_version = audio_version + 1 WHERE rel_path = 'A/a.flac'",
            [],
        )
        .unwrap();
    let ids = lib
        .db
        .write(|c| dbfc::enqueue_all_unchecked(c, spindle::db::now_epoch()))
        .await
        .unwrap();
    assert_eq!(ids.len(), 1);
    let _ = params![0];
}

#[tokio::test]
async fn scan_job_enqueues_flaccheck_for_new_flac_when_enabled() {
    use spindle::jobs::handlers::scan::{new_scan_job, ScanHandler};
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    if !flac_available() {
        return;
    }
    let lib = Lib::new();
    lib.add("A/a.flac", 11);
    lib.add("A/b.opus", 12);
    let scanner = Arc::new(Scanner::new(lib.db.clone(), lib.root.clone(), 2));
    let mut reg = Registry::new();
    reg.register(
        JobType::Scan,
        Arc::new(ScanHandler::new(scanner, 30).with_flac_verify(true)),
    );
    reg.register(
        JobType::Flaccheck,
        Arc::new(FlaccheckHandler::new(lib.root.clone(), "flac")),
    );
    lib.jobs.start(reg, lib.shutdown.clone());
    let scan = match lib
        .jobs
        .enqueue(new_scan_job(ScanKind::Incremental))
        .await
        .unwrap()
    {
        EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
    };
    assert_eq!(lib.wait_job(scan).await, JobState::Done);
    // FLAC 1 本分の flaccheck が投入され、完走して ok になる
    let ids: Vec<i64> = {
        let c = lib.conn();
        let mut st = c
            .prepare("SELECT id FROM jobs WHERE type = 'flaccheck' ORDER BY id")
            .unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    assert_eq!(ids.len(), 1);
    assert_eq!(lib.wait_job(ids[0]).await, JobState::Done);
    assert_eq!(lib.result("A/a.flac").status.as_deref(), Some("ok"));
    // 2 回目のスキャンは結果が現在の版なので投入しない
    let scan = match lib
        .jobs
        .enqueue(new_scan_job(ScanKind::Incremental))
        .await
        .unwrap()
    {
        EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
    };
    assert_eq!(lib.wait_job(scan).await, JobState::Done);
    let n: i64 = lib
        .conn()
        .query_row(
            "SELECT count(*) FROM jobs WHERE type = 'flaccheck'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}

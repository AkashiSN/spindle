//! `thumbnail` ジョブ（SPEC §8、docs/TASKS.md P1-3、D-49）。原画像から WebP のサムネイルを
//! ffmpeg で作る。ffmpeg が無ければ skip

#![cfg(target_os = "linux")]

mod common;

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use spindle::db::{artwork as dbart, Db};
use spindle::jobs::handlers::thumbnail::{new_thumbnail_job, ThumbnailHandler};
use spindle::jobs::{EnqueueResult, JobState, JobType, Jobs, Registry};
use spindle::media::artwork::{sniff, ArtworkStore, THUMB_SIZES};

struct Env {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    db: Arc<Db>,
    jobs: Arc<Jobs>,
    store: Arc<ArtworkStore>,
    shutdown: CancellationToken,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let jobs = Jobs::new(db.clone());
        let store = Arc::new(ArtworkStore::new(dir.path().join("thumbs")));
        Self {
            dir,
            db_path,
            db,
            jobs,
            store,
            shutdown: CancellationToken::new(),
        }
    }

    fn start(&self) {
        let mut reg = Registry::new();
        reg.register(
            JobType::Thumbnail,
            Arc::new(ThumbnailHandler::new(self.store.clone(), "ffmpeg")),
        );
        self.jobs.start(reg, self.shutdown.clone());
    }

    /// ffmpeg で `w`x`h` の実画像（PNG）を作る
    fn make_image(&self, w: u32, h: u32) -> Vec<u8> {
        let p = self.dir.path().join(format!("src-{w}x{h}.png"));
        let st = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg(format!("color=c=red:s={w}x{h}"))
            .args(["-frames:v", "1"])
            .arg(&p)
            .status()
            .unwrap();
        assert!(st.success());
        std::fs::read(p).unwrap()
    }

    /// 原画像を置いて artwork 行を作る
    async fn register(&self, bytes: &[u8]) -> (i64, [u8; 32]) {
        let info = sniff(bytes).unwrap();
        let hash = ArtworkStore::hash_of(bytes);
        self.store.put_original(&hash, info.mime, bytes).unwrap();
        let n = bytes.len();
        let id = self
            .db
            .write(move |c| {
                dbart::upsert(
                    c,
                    &hash,
                    info.mime,
                    Some(info.width),
                    Some(info.height),
                    n,
                    "file",
                )
            })
            .await
            .unwrap();
        (id, hash)
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.db_path).unwrap()
    }

    async fn wait_job(&self, id: i64) -> JobState {
        // CI のランナーは I/O が遅く回数ベースでは足りないので、経過時間で待つ
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while std::time::Instant::now() < deadline {
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
        panic!("job {id} が終端にならない");
    }

    /// 最初の試行が失敗して `last_error` が入るまで待つ（再試行はバックオフで先）
    async fn wait_error(&self, id: i64) -> String {
        // CI のランナーは I/O が遅く回数ベースでは足りないので、経過時間で待つ
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while std::time::Instant::now() < deadline {
            if let Some(e) = self.last_error(id) {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job {id} が失敗しない");
    }

    fn last_error(&self, id: i64) -> Option<String> {
        self.conn()
            .query_row("SELECT last_error FROM jobs WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn job_id(r: EnqueueResult) -> i64 {
    match r {
        EnqueueResult::Inserted(id) | EnqueueResult::Duplicate(id) => id,
    }
}

#[tokio::test]
async fn makes_webp_for_each_size_with_aspect_kept_and_no_upscale() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let env = Env::new();
    let big = env.make_image(1200, 600);
    let (id, hash) = env.register(&big).await;
    env.start();
    let job = job_id(env.jobs.enqueue(new_thumbnail_job(id)).await.unwrap());
    assert_eq!(
        env.wait_job(job).await,
        JobState::Done,
        "{:?}",
        env.last_error(job)
    );
    for size in THUMB_SIZES {
        let p = env.store.thumb_path(&hash, size);
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WEBP");
        let info = sniff(&bytes).unwrap();
        assert_eq!(info.mime, "image/webp");
        assert_eq!((info.width, info.height), (size, size / 2), "{size}");
    }
    assert!(env.store.missing_thumbs(&hash).is_empty());
    // tmp は残らない
    let names: Vec<String> = std::fs::read_dir(env.store.thumb_path(&hash, 256).parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|n| !n.contains(".tmp")), "{names:?}");

    // 小さい画像は拡大しない
    let small = env.make_image(100, 80);
    let (id2, hash2) = env.register(&small).await;
    let job = job_id(env.jobs.enqueue(new_thumbnail_job(id2)).await.unwrap());
    assert_eq!(env.wait_job(job).await, JobState::Done);
    for size in THUMB_SIZES {
        let info = sniff(&std::fs::read(env.store.thumb_path(&hash2, size)).unwrap()).unwrap();
        assert_eq!((info.width, info.height), (100, 80));
    }
}

#[tokio::test]
async fn rerun_skips_existing_sizes_and_missing_original_fails() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let env = Env::new();
    let img = env.make_image(300, 300);
    let (id, hash) = env.register(&img).await;
    env.start();
    let job = job_id(env.jobs.enqueue(new_thumbnail_job(id)).await.unwrap());
    assert_eq!(env.wait_job(job).await, JobState::Done);
    let before = std::fs::metadata(env.store.thumb_path(&hash, 256))
        .unwrap()
        .modified()
        .unwrap();
    // 1 つ消して再投入 → 消した方だけ作る
    std::fs::remove_file(env.store.thumb_path(&hash, 768)).unwrap();
    let job = job_id(env.jobs.enqueue(new_thumbnail_job(id)).await.unwrap());
    assert_eq!(env.wait_job(job).await, JobState::Done);
    assert!(env.store.thumb_path(&hash, 768).is_file());
    let after = std::fs::metadata(env.store.thumb_path(&hash, 256))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(before, after, "既にあるサイズは触らない");

    // 原画像が無ければ失敗し、参照している album の再解決を予約する
    let (id2, hash2) = env.register(&env.make_image(50, 50)).await;
    env.conn()
        .execute(
            "INSERT INTO albums (rel_dir, rel_dir_key, artwork_id, artwork_resolved_at)
             VALUES ('A', 'a', ?1, 100)",
            [id2],
        )
        .unwrap();
    std::fs::remove_file(env.store.original_path(&hash2, "image/png")).unwrap();
    let job = job_id(env.jobs.enqueue(new_thumbnail_job(id2)).await.unwrap());
    assert!(env.wait_error(job).await.contains("原画像が無い"));
    let resolved: Option<i64> = env
        .conn()
        .query_row(
            "SELECT artwork_resolved_at FROM albums WHERE rel_dir = 'A'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(resolved, None);

    // artwork 行が無ければ何もせず done
    let job = job_id(env.jobs.enqueue(new_thumbnail_job(9999)).await.unwrap());
    assert_eq!(env.wait_job(job).await, JobState::Done);
}

#[tokio::test]
async fn failed_placement_leaves_no_tmp() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let env = Env::new();
    let (id, hash) = env.register(&env.make_image(120, 120)).await;
    // 宛先をディレクトリで塞ぐと rename が失敗する
    std::fs::create_dir_all(env.store.thumb_path(&hash, 768)).unwrap();
    env.start();
    let job = job_id(env.jobs.enqueue(new_thumbnail_job(id)).await.unwrap());
    assert!(env.wait_error(job).await.contains("サムネイルを置けない"));
    let dir = env
        .store
        .thumb_path(&hash, 256)
        .parent()
        .unwrap()
        .to_path_buf();
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|n| !n.contains(".tmp")), "{names:?}");
    // 256 は作れている
    assert!(env.store.thumb_path(&hash, 256).is_file());
}

//! `gc` ジョブ（SPEC §6「論理削除」/ §8、docs/TASKS.md P1-11、D-56）。物理削除を行う唯一の経路。
//! 5 区分（missing トラック / missing アルバム / Archive の退避 / Derived の孤児 / アートワークの
//! 孤児）の判定と実行、dry-run が何も消さないこと、scan 実行中は走らないことを固定する

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rusqlite::{params, Connection};
use tokio_util::sync::CancellationToken;

use spindle::db::{now_epoch, Db};
use spindle::fsroot::RootDir;
use spindle::gc::{execute_all, execute_rows, plan, GcRoots, ORPHAN_GRACE_SECS};
use spindle::jobs::handlers::gc::{new_gc_job, GcHandler};
use spindle::jobs::{EnqueueResult, JobState, JobType, Jobs, NewJob, Registry};
use spindle::media::artwork::ArtworkStore;

const DAY: i64 = 86_400;
const RETENTION: i64 = 30 * DAY;
/// テスト用の GC ジョブ行（ロックの持ち主として要る）
const GC_JOB: i64 = 101;

type HoldFut = std::pin::Pin<
    Box<dyn std::future::Future<Output = spindle::jobs::HandlerResult> + Send + 'static>,
>;

struct Env {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    db: Arc<Db>,
    jobs: Arc<Jobs>,
    roots: Arc<GcRoots>,
    shutdown: CancellationToken,
    now: i64,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["Library", "Archive", "Derived", "thumbs"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        Connection::open(&db_path)
            .unwrap()
            .execute(
                "INSERT INTO jobs (id, type, payload, state, created_at) VALUES (?1, 'gc', '{}', 'running', 0)",
                [GC_JOB],
            )
            .unwrap();
        let jobs = Jobs::new(db.clone());
        let roots = Arc::new(GcRoots {
            library: Arc::new(RootDir::open(&dir.path().join("Library")).unwrap()),
            archive: Arc::new(RootDir::open(&dir.path().join("Archive")).unwrap()),
            derived: Arc::new(RootDir::open(&dir.path().join("Derived")).unwrap()),
            artwork: Arc::new(ArtworkStore::new(dir.path().join("thumbs"))),
        });
        Self {
            dir,
            db_path,
            db,
            jobs,
            roots,
            shutdown: CancellationToken::new(),
            now: now_epoch(),
        }
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.db_path).unwrap()
    }

    /// `GC_JOB`（直接実行用に入れてある `running` 行）を終端にする。ワーカーを起動するテストで
    /// 先に呼ぶ（実行中表に無い `running` は稼働中の回収で queued に戻され、走ってしまう。D-76）
    fn retire_fake_gc_job(&self) {
        self.conn()
            .execute(
                "UPDATE jobs SET state = 'done', finished_at = 1 WHERE id = ?1",
                [GC_JOB],
            )
            .unwrap();
    }

    /// `LIBRARY_MUTEX` を取って `release` が倒れるまで持ち続けるハンドラ（本物の running が
    /// mutex を持つ状況の模擬）。取れなければ `Requeue`
    fn mutex_holder(release: CancellationToken) -> impl Fn(spindle::jobs::JobContext) -> HoldFut {
        move |ctx| {
            let release = release.clone();
            Box::pin(async move {
                if !ctx.lock_mutex(spindle::jobs::LIBRARY_MUTEX).await? {
                    return Ok(spindle::jobs::Outcome::Requeue);
                }
                release.cancelled().await;
                Ok(spindle::jobs::Outcome::Done)
            })
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// `rel` にファイルを置く（親ディレクトリも作る）。mtime は `age_secs` 前
    fn put(&self, rel: &str, bytes: &[u8], age_secs: i64) -> PathBuf {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        set_age(&p, age_secs);
        p
    }

    /// トラック行。`missing_age` は missing_since を何秒前にするか（None は active）
    fn track(&self, id: i64, rel: &str, album_id: Option<i64>, missing_age: Option<i64>) {
        let missing = missing_age.map(|a| self.now - a);
        self.conn()
            .execute(
                "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                                     album_id, audio_version, tag_version, seen_at, missing_since)
                 VALUES (?1, ?2, lower(?2), 1, 0, 0, 'flac', 1, ?3, 1, 1, 0, ?4)",
                params![id, rel, album_id, missing],
            )
            .unwrap();
    }

    fn album(&self, id: i64, rel_dir: &str, artwork_id: Option<i64>, missing_age: Option<i64>) {
        let missing = missing_age.map(|a| self.now - a);
        self.conn()
            .execute(
                "INSERT INTO albums (id, rel_dir, rel_dir_key, album, artwork_id, missing_since)
                 VALUES (?1, ?2, lower(?2), ?2, ?3, ?4)",
                params![id, rel_dir, artwork_id, missing],
            )
            .unwrap();
    }

    fn artwork(&self, id: i64, hash_byte: u8) -> String {
        let hash = [hash_byte; 32];
        self.conn()
            .execute(
                "INSERT INTO artwork (id, sha256, mime, bytes, origin)
                 VALUES (?1, ?2, 'image/png', 1, 'file')",
                params![id, &hash[..]],
            )
            .unwrap();
        ArtworkStore::hex(&hash)
    }

    fn derived_row(&self, track_id: i64, rel: &str) {
        self.conn()
            .execute(
                "INSERT INTO derived_files (track_id, variant, rel_path, rel_path_key, codec, src_audio_version,
                                            src_tag_version, generated_at, audio_profile, tag_profile)
                 VALUES (?1, 'opus', ?2, lower(?2), 'opus', 1, 1, 0, 'opus:256:v1', 'opus:v1')",
                params![track_id, rel],
            )
            .unwrap();
    }

    fn archived(&self, id: i64, rel: &str, state: &str, eligible_in: i64) {
        self.conn()
            .execute(
                "INSERT INTO archived_files (id, track_id, rel_path, rel_path_key, source_rel_path, reason,
                                             archived_at, eligible_after, state)
                 VALUES (?1, NULL, ?2, lower(?2), 'x', 'normalize', ?3, ?4, ?5)",
                params![id, rel, self.now - DAY, self.now + eligible_in, state],
            )
            .unwrap();
    }

    fn count(&self, sql: &str) -> i64 {
        self.conn().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn job_state(&self, id: i64) -> JobState {
        let s: String = self
            .conn()
            .query_row("SELECT state FROM jobs WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();
        s.parse().unwrap()
    }

    async fn wait_job(&self, id: i64) -> JobState {
        // CI のランナーは I/O が遅く回数ベースでは足りないので、経過時間で待つ
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while std::time::Instant::now() < deadline {
            let st = self.job_state(id);
            if st.is_terminal() {
                return st;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job {id} が終端にならない");
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn set_age(p: &Path, age_secs: i64) {
    let t = SystemTime::now() - Duration::from_secs(age_secs.max(0) as u64);
    // 所有者なら読み取りで開いた fd でも futimens できる（ディレクトリにも使う）
    std::fs::File::open(p).unwrap().set_modified(t).unwrap();
}

async fn run(env: &Env) -> spindle::gc::Summary {
    let plan = plan(&env.db, &env.roots, RETENTION, env.now).await.unwrap();
    execute_all(
        &env.db,
        &env.roots,
        &plan,
        &CancellationToken::new(),
        Some(GC_JOB),
    )
    .await
    .unwrap()
}

// ---------------------------------------------------------------- A / B

#[tokio::test]
async fn missing_tracks_past_retention_are_deleted_unless_the_file_is_back() {
    let env = Env::new();
    env.track(1, "A/gone.flac", None, Some(31 * DAY)); // 期限超、実体無し → 消える
    env.track(2, "A/back.flac", None, Some(31 * DAY)); // 期限超だが実体がある → 残る
    env.put("Library/A/back.flac", b"x", 0);
    env.track(3, "A/recent.flac", None, Some(10 * DAY)); // 期限前 → 残る
    env.track(4, "A/active.flac", None, None);
    let c = env.conn();
    c.execute(
        "INSERT INTO playlists (id, name, name_key, kind, created_at, updated_at) VALUES (1, 'p', 'p', 'manual', 0, 0)",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO playlist_items (playlist_id, position, track_id) VALUES (1, 1, 1), (1, 2, 4)",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO edit_batches (id, created_at, state) VALUES (1, 0, 'applied')",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO edit_ops (id, batch_id, ordinal, track_id, kind, result) VALUES (1, 1, 1, 1, 'tags', 'applied')",
        [],
    )
    .unwrap();

    let p = plan(&env.db, &env.roots, RETENTION, env.now).await.unwrap();
    let ids: Vec<i64> = p.tracks.iter().map(|t| t.id).collect();
    assert_eq!(ids, vec![1]);
    assert_eq!(p.tracks[0].rel_path, "A/gone.flac");
    // plan は何も消さない
    assert_eq!(env.count("SELECT count(*) FROM tracks"), 4);

    let s = run(&env).await;
    assert_eq!(s.tracks.deleted, 1);
    let remaining: Vec<i64> = {
        let mut st = c.prepare("SELECT id FROM tracks ORDER BY id").unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    assert_eq!(remaining, vec![2, 3, 4]);
    // プレイリスト項目は CASCADE、編集履歴は残る
    assert_eq!(env.count("SELECT count(*) FROM playlist_items"), 1);
    assert_eq!(
        env.count("SELECT count(*) FROM edit_ops WHERE track_id = 1"),
        1
    );
    // 2 回目は何もしない
    let s = run(&env).await;
    assert_eq!(s.tracks.deleted, 0);
}

#[tokio::test]
async fn missing_albums_are_deleted_only_when_no_track_remains() {
    let env = Env::new();
    env.album(1, "A", None, Some(31 * DAY)); // 構成が消えるトラックだけ → 消える
    env.track(1, "A/gone.flac", Some(1), Some(31 * DAY));
    env.album(2, "B", None, Some(31 * DAY)); // active なトラックが残る → 残る
    env.track(2, "B/live.flac", Some(2), None);
    env.album(3, "C", None, Some(10 * DAY)); // 期限前 → 残る
    env.album(4, "D", None, None); // active
    let p = plan(&env.db, &env.roots, RETENTION, env.now).await.unwrap();
    let ids: Vec<i64> = p.albums.iter().map(|a| a.id).collect();
    assert_eq!(ids, vec![1]);
    let s = run(&env).await;
    assert_eq!((s.tracks.deleted, s.albums.deleted), (1, 1));
    assert_eq!(env.count("SELECT count(*) FROM albums"), 3);
    assert_eq!(env.count("SELECT count(*) FROM albums WHERE id = 1"), 0);
}

// ---------------------------------------------------------------- C

#[tokio::test]
async fn archived_files_past_eligibility_are_unlinked_and_marked_deleted() {
    let env = Env::new();
    env.archived(1, "A/1.m4a", "held", -DAY); // 期限超 → unlink して deleted
    env.put("Archive/A/1.m4a", b"12345", 40 * DAY);
    env.archived(2, "A/2.m4a", "held", DAY); // 期限前 → 残る
    env.put("Archive/A/2.m4a", b"1", 40 * DAY);
    env.archived(3, "A/3.m4a", "restored", -DAY); // restored は触らない
    env.put("Archive/A/3.m4a", b"1", 40 * DAY);
    env.archived(4, "A/4.m4a", "held", -DAY); // 期限超で実体が無い → deleted に進む
    env.archived(5, "A/5dir", "held", -DAY); // unlink が失敗（ディレクトリ）→ held のまま
    std::fs::create_dir_all(env.path("Archive/A/5dir")).unwrap();

    let p = plan(&env.db, &env.roots, RETENTION, env.now).await.unwrap();
    let ids: Vec<i64> = p.archived.iter().map(|a| a.id).collect();
    assert_eq!(ids, vec![1, 4, 5]);
    assert_eq!(p.archived[0].bytes, 5);

    let s = run(&env).await;
    assert_eq!(s.archived.deleted, 2, "{s:?}");
    assert_eq!(s.archived.bytes, 5);
    assert_eq!(s.archived.failed, 1);
    assert!(!env.path("Archive/A/1.m4a").exists());
    assert!(env.path("Archive/A/2.m4a").exists());
    assert!(env.path("Archive/A/3.m4a").exists());
    assert!(env.path("Archive/A/5dir").is_dir());
    let state = |id: i64| -> String {
        env.conn()
            .query_row(
                "SELECT state FROM archived_files WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(state(1), "deleted");
    assert_eq!(state(2), "held");
    assert_eq!(state(3), "restored");
    assert_eq!(state(4), "deleted");
    assert_eq!(state(5), "held");
    assert!(env.count("SELECT state_at FROM archived_files WHERE id = 1") >= env.now);
}

// ---------------------------------------------------------------- D

#[tokio::test]
async fn derived_orphans_are_removed_but_referenced_tmp_and_recent_files_stay() {
    let env = Env::new();
    env.track(1, "A/live.flac", None, None);
    env.derived_row(1, "A/live.opus");
    env.put("Derived/A/live.opus", b"keep", 5 * DAY);
    env.put("Derived/B/orphan.opus", b"orphan!", 5 * DAY); // 行が無い → 消える。B/ も空になるので消える
    env.put("Derived/A/.spindle-tmp-abc", b"tmp", 5 * DAY); // in-flight の作業ファイル → 残す
    env.put("Derived/C/fresh.opus", b"fresh", ORPHAN_GRACE_SECS - 60); // 24h 以内 → 残す
                                                                       // missing 期限超のトラックの Derived は行が CASCADE で消えるので同じ実行で拾う
    env.track(2, "A/gone.flac", None, Some(31 * DAY));
    env.derived_row(2, "A/gone.opus");
    env.put("Derived/A/gone.opus", b"gone", 5 * DAY);

    let p = plan(&env.db, &env.roots, RETENTION, env.now).await.unwrap();
    let mut paths: Vec<&str> = p.derived.iter().map(|f| f.rel_path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(paths, vec!["A/gone.opus", "B/orphan.opus"]);

    let s = run(&env).await;
    assert_eq!(s.derived.deleted, 2, "{s:?}");
    assert_eq!(s.derived.bytes, 7 + 4);
    assert!(env.path("Derived/A/live.opus").exists());
    assert!(env.path("Derived/A/.spindle-tmp-abc").exists());
    assert!(env.path("Derived/C/fresh.opus").exists());
    assert!(!env.path("Derived/B/orphan.opus").exists());
    assert!(
        !env.path("Derived/B").exists(),
        "空になったディレクトリは消す"
    );
    assert!(env.path("Derived/A").is_dir());
    assert!(env.path("Derived").is_dir(), "root は残す");
}

#[tokio::test]
async fn derived_orphan_replaced_after_planning_is_left_alone() {
    let env = Env::new();
    let p_orphan = env.put("Derived/A/x.opus", b"old", 5 * DAY);
    let p = plan(&env.db, &env.roots, RETENTION, env.now).await.unwrap();
    assert_eq!(p.derived.len(), 1);
    // 一覧の後に transcode が同じパスへ置き換えた（inode と mtime が変わる）
    std::fs::remove_file(&p_orphan).unwrap();
    std::fs::write(&p_orphan, b"new content").unwrap();
    let s = execute_all(
        &env.db,
        &env.roots,
        &p,
        &CancellationToken::new(),
        Some(GC_JOB),
    )
    .await
    .unwrap();
    assert_eq!(s.derived.deleted, 0, "{s:?}");
    assert_eq!(s.derived.skipped, 1);
    assert_eq!(std::fs::read(&p_orphan).unwrap(), b"new content");
}

// ---------------------------------------------------------------- E

#[tokio::test]
async fn artwork_rows_without_album_reference_and_dirs_without_rows_are_removed() {
    let env = Env::new();
    let used = env.artwork(1, 0x11);
    env.album(1, "A", Some(1), None);
    let unused = env.artwork(2, 0x22);
    env.put(&format!("thumbs/{used}/orig.png"), b"u", 5 * DAY);
    env.put(&format!("thumbs/{used}/256.webp"), b"u", 5 * DAY);
    env.put(&format!("thumbs/{unused}/orig.png"), b"x", 5 * DAY);
    let stray = "3".repeat(64);
    env.put(&format!("thumbs/{stray}/orig.png"), b"s", 5 * DAY);
    env.put("thumbs/not-a-hash/x", b"n", 5 * DAY); // hex でない名前は触らない
    for d in [&used, &unused, &stray] {
        set_age(&env.path(&format!("thumbs/{d}")), 5 * DAY);
    }
    // Derived が unused を参照していても行は消え（SET NULL）、次の retag が追随する
    env.track(1, "A/t.flac", Some(1), None);
    env.derived_row(1, "A/t.opus");
    env.conn()
        .execute(
            "UPDATE derived_files SET src_artwork_id = 2 WHERE track_id = 1",
            [],
        )
        .unwrap();

    let p = plan(&env.db, &env.roots, RETENTION, env.now).await.unwrap();
    assert_eq!(
        p.artwork_rows.iter().map(|a| a.id).collect::<Vec<_>>(),
        vec![2]
    );
    let mut dirs: Vec<&str> = p.artwork_dirs.iter().map(String::as_str).collect();
    dirs.sort_unstable();
    assert_eq!(dirs, vec![unused.as_str(), stray.as_str()]);

    let s = run(&env).await;
    assert_eq!(
        (s.artwork_rows.deleted, s.artwork_dirs.deleted),
        (1, 2),
        "{s:?}"
    );
    assert!(env.path(&format!("thumbs/{used}/256.webp")).exists());
    assert!(!env.path(&format!("thumbs/{unused}")).exists());
    assert!(!env.path(&format!("thumbs/{stray}")).exists());
    assert!(env.path("thumbs/not-a-hash/x").exists());
    assert_eq!(env.count("SELECT count(*) FROM artwork"), 1);
    assert_eq!(
        env.count("SELECT src_artwork_id IS NULL FROM derived_files WHERE track_id = 1"),
        1
    );
}

// ---------------------------------------------------------------- ジョブ

#[tokio::test]
async fn gc_job_waits_while_a_scan_holds_the_library_mutex() {
    let env = Env::new();
    env.retire_fake_gc_job();
    env.track(1, "A/gone.flac", None, Some(31 * DAY));
    // 本物の running な scan が library mutex を持ち続ける
    let release = CancellationToken::new();
    let mut reg = Registry::new();
    reg.register_fn(JobType::Scan, Env::mutex_holder(release.clone()));
    reg.register(
        JobType::Gc,
        Arc::new(GcHandler::new(env.db.clone(), env.roots.clone(), RETENTION)),
    );
    let scan_id = match env
        .jobs
        .enqueue(NewJob::new(JobType::Scan, serde_json::json!({})))
        .await
        .unwrap()
    {
        EnqueueResult::Inserted(id) | EnqueueResult::Duplicate(id) => id,
    };
    env.jobs.start(reg, env.shutdown.clone());
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while env.job_state(scan_id) != JobState::Running && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(env.job_state(scan_id), JobState::Running);
    let gc_id = match env.jobs.enqueue(new_gc_job()).await.unwrap() {
        EnqueueResult::Inserted(id) | EnqueueResult::Duplicate(id) => id,
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.job_state(gc_id), JobState::Queued);
    assert_eq!(env.count("SELECT count(*) FROM tracks"), 1);
    // scan が終端になれば（mutex が解放されれば）走る
    release.cancel();
    assert_eq!(env.wait_job(scan_id).await, JobState::Done);
    assert_eq!(env.wait_job(gc_id).await, JobState::Done);
    assert_eq!(env.count("SELECT count(*) FROM tracks"), 0);
    assert_eq!(
        env.count("SELECT count(*) FROM job_mutexes"),
        0,
        "終端で解放される"
    );
    // 同じキーは未完了の間だけ dedup。終わった後は再投入できる
    assert!(matches!(
        env.jobs.enqueue(new_gc_job()).await.unwrap(),
        EnqueueResult::Inserted(_)
    ));
}

#[tokio::test]
async fn scan_and_gc_enqueued_together_both_finish() {
    use spindle::import::scanner::Scanner;
    use spindle::jobs::handlers::scan::{new_scan_job, ScanHandler};
    let env = Env::new();
    env.retire_fake_gc_job();
    env.track(1, "A/gone.flac", None, Some(31 * DAY));
    let scanner = Arc::new(Scanner::new(env.db.clone(), env.roots.library.clone(), 2));
    let mut reg = Registry::new();
    reg.register(JobType::Scan, Arc::new(ScanHandler::new(scanner, 30)));
    reg.register(
        JobType::Gc,
        Arc::new(GcHandler::new(env.db.clone(), env.roots.clone(), RETENTION)),
    );
    for round in 0..5 {
        let scan_id = match env
            .jobs
            .enqueue(new_scan_job(spindle::db::scans::ScanKind::Incremental))
            .await
            .unwrap()
        {
            EnqueueResult::Inserted(id) | EnqueueResult::Duplicate(id) => id,
        };
        let gc_id = match env.jobs.enqueue(new_gc_job()).await.unwrap() {
            EnqueueResult::Inserted(id) | EnqueueResult::Duplicate(id) => id,
        };
        if round == 0 {
            env.jobs.start(reg, env.shutdown.clone());
            // 以降の round は同じワーカーが拾う
            reg = Registry::new();
        }
        assert_eq!(env.wait_job(scan_id).await, JobState::Done, "round {round}");
        assert_eq!(env.wait_job(gc_id).await, JobState::Done, "round {round}");
    }
    assert_eq!(env.count("SELECT count(*) FROM job_mutexes"), 0);
}

// ---------------------------------------------------------------- 計画と実行の間の状態変化

#[tokio::test]
async fn archived_item_restored_or_extended_after_planning_is_not_deleted() {
    let env = Env::new();
    env.archived(1, "A/1.m4a", "held", -DAY);
    env.put("Archive/A/1.m4a", b"1", 40 * DAY);
    env.archived(2, "A/2.m4a", "held", -DAY);
    env.put("Archive/A/2.m4a", b"2", 40 * DAY);
    env.archived(3, "A/3.m4a", "held", -DAY);
    env.put("Archive/A/3.m4a", b"3", 40 * DAY);
    let p = plan(&env.db, &env.roots, RETENTION, env.now).await.unwrap();
    assert_eq!(p.archived.len(), 3);
    // 1 は巻き戻しで restored、2 は redo で期限が延びた、3 はそのまま
    let c = env.conn();
    c.execute(
        "UPDATE archived_files SET state = 'restored' WHERE id = 1",
        [],
    )
    .unwrap();
    c.execute(
        "UPDATE archived_files SET eligible_after = ?1 WHERE id = 2",
        [env.now + 10 * DAY],
    )
    .unwrap();
    let s = execute_all(
        &env.db,
        &env.roots,
        &p,
        &CancellationToken::new(),
        Some(GC_JOB),
    )
    .await
    .unwrap();
    assert_eq!((s.archived.deleted, s.archived.skipped), (1, 2), "{s:?}");
    assert!(env.path("Archive/A/1.m4a").exists());
    assert!(env.path("Archive/A/2.m4a").exists());
    assert!(!env.path("Archive/A/3.m4a").exists());
    let state = |id: i64| -> String {
        env.conn()
            .query_row(
                "SELECT state FROM archived_files WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(
        (state(1), state(2), state(3)),
        ("restored".into(), "held".into(), "deleted".into())
    );
}

#[tokio::test]
async fn archived_item_whose_track_is_locked_by_another_job_is_skipped() {
    use spindle::gc::execute_archive;
    let env = Env::new();
    env.track(1, "A/t.flac", None, None);
    env.conn()
        .execute("UPDATE archived_files SET track_id = 1 WHERE id = 1", [])
        .ok();
    env.archived(1, "A/1.m4a", "held", -DAY);
    env.conn()
        .execute("UPDATE archived_files SET track_id = 1 WHERE id = 1", [])
        .unwrap();
    env.put("Archive/A/1.m4a", b"1", 40 * DAY);
    // 別のジョブ（normalize の巻き戻し相当）がトラックのロックを持っている
    let c = env.conn();
    c.execute(
        "INSERT INTO jobs (id, type, payload, state, created_at) VALUES (100, 'normalize', '{}', 'running', 0)",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO track_locks (track_id, job_id, acquired_at) VALUES (1, 100, 0)",
        [],
    )
    .unwrap();
    let p = plan(&env.db, &env.roots, RETENTION, env.now).await.unwrap();
    assert_eq!(p.archived.len(), 1);
    let counts = execute_archive(
        &env.db,
        &env.roots,
        &p,
        &CancellationToken::new(),
        Some(101),
    )
    .await
    .unwrap();
    assert_eq!((counts.deleted, counts.skipped), (0, 1), "{counts:?}");
    assert!(env.path("Archive/A/1.m4a").exists());
    assert_eq!(
        env.count("SELECT count(*) FROM archived_files WHERE state = 'held'"),
        1
    );
    // ロックが外れれば消え、GC 自身のロックは残らない
    c.execute("DELETE FROM track_locks WHERE job_id = 100", [])
        .unwrap();
    let counts = execute_archive(
        &env.db,
        &env.roots,
        &p,
        &CancellationToken::new(),
        Some(101),
    )
    .await
    .unwrap();
    assert_eq!(counts.deleted, 1, "{counts:?}");
    assert!(!env.path("Archive/A/1.m4a").exists());
    assert_eq!(env.count("SELECT count(*) FROM track_locks"), 0);
    // トラック行が既に無い台帳（track_id が FK でないので残る）はロック無しで消える
    env.archived(2, "A/2.m4a", "held", -DAY);
    c.execute("UPDATE archived_files SET track_id = 999 WHERE id = 2", [])
        .unwrap();
    env.put("Archive/A/2.m4a", b"2", 40 * DAY);
    let p = plan(&env.db, &env.roots, RETENTION, env.now).await.unwrap();
    let counts = execute_archive(
        &env.db,
        &env.roots,
        &p,
        &CancellationToken::new(),
        Some(101),
    )
    .await
    .unwrap();
    assert_eq!(counts.deleted, 1, "{counts:?}");
    assert!(!env.path("Archive/A/2.m4a").exists());
}

#[tokio::test]
async fn derived_of_a_track_revived_after_planning_is_kept() {
    let env = Env::new();
    env.track(2, "A/gone.flac", None, Some(31 * DAY));
    env.derived_row(2, "A/gone.opus");
    env.put("Derived/A/gone.opus", b"gone", 5 * DAY);
    let p = plan(&env.db, &env.roots, RETENTION, env.now).await.unwrap();
    assert_eq!(p.tracks.len(), 1);
    assert_eq!(p.derived.len(), 1);
    // スキャンが復活させた（行が残るので Derived の行も残る）
    env.conn()
        .execute("UPDATE tracks SET missing_since = NULL WHERE id = 2", [])
        .unwrap();
    let s = execute_all(
        &env.db,
        &env.roots,
        &p,
        &CancellationToken::new(),
        Some(GC_JOB),
    )
    .await
    .unwrap();
    assert_eq!(
        (s.tracks.deleted, s.derived.deleted, s.derived.skipped),
        (0, 0, 1),
        "{s:?}"
    );
    assert!(env.path("Derived/A/gone.opus").exists());
    assert_eq!(env.count("SELECT count(*) FROM derived_files"), 1);
    // transcode が期待パスを予約しているときも消さない
    env.put("Derived/B/claimed.opus", b"c", 5 * DAY);
    env.conn()
        .execute(
            "INSERT INTO jobs (id, type, payload, state, created_at) VALUES (7, 'transcode', '{}', 'running', 0)",
            [],
        )
        .unwrap();
    let p = plan(&env.db, &env.roots, RETENTION, env.now).await.unwrap();
    assert_eq!(p.derived.len(), 1);
    env.conn()
        .execute(
            "INSERT INTO derived_path_locks (rel_path_key, track_id, job_id, acquired_at) VALUES ('b/claimed.opus', 2, 7, 0)",
            [],
        )
        .unwrap();
    let s = execute_all(
        &env.db,
        &env.roots,
        &p,
        &CancellationToken::new(),
        Some(GC_JOB),
    )
    .await
    .unwrap();
    assert_eq!(s.derived.deleted, 0, "{s:?}");
    assert!(env.path("Derived/B/claimed.opus").exists());
}

#[tokio::test]
async fn artwork_dir_referenced_or_touched_after_planning_is_kept() {
    let env = Env::new();
    let unused = env.artwork(2, 0x22);
    env.put(&format!("thumbs/{unused}/orig.png"), b"x", 5 * DAY);
    set_age(&env.path(&format!("thumbs/{unused}")), 5 * DAY);
    let stray = "3".repeat(64);
    env.put(&format!("thumbs/{stray}/orig.png"), b"s", 5 * DAY);
    set_age(&env.path(&format!("thumbs/{stray}")), 5 * DAY);
    let p = plan(&env.db, &env.roots, RETENTION, env.now).await.unwrap();
    assert_eq!(p.artwork_rows.len(), 1);
    assert_eq!(p.artwork_dirs.len(), 2);
    // 計画の後にアルバムが参照した / スキャンが dir を触った
    env.album(1, "A", Some(2), None);
    env.put(&format!("thumbs/{stray}/256.webp"), b"n", 0);
    let s = execute_all(
        &env.db,
        &env.roots,
        &p,
        &CancellationToken::new(),
        Some(GC_JOB),
    )
    .await
    .unwrap();
    assert_eq!(
        (
            s.artwork_rows.deleted,
            s.artwork_dirs.deleted,
            s.artwork_dirs.skipped
        ),
        (0, 0, 2),
        "{s:?}"
    );
    assert!(env.path(&format!("thumbs/{unused}/orig.png")).exists());
    assert!(env.path(&format!("thumbs/{stray}/orig.png")).exists());
    assert_eq!(env.count("SELECT count(*) FROM artwork"), 1);
}

// ---------------------------------------------------------------- 排他

#[tokio::test]
async fn gc_takes_the_derived_path_lock_so_transcode_cannot_claim_during_deletion() {
    use spindle::db::{derived as dbderived, gc as dbgc};
    let env = Env::new();
    env.track(2, "A/t.flac", None, None);
    let c = env.conn();
    c.execute(
        "INSERT INTO jobs (id, type, payload, state, created_at) VALUES (7, 'transcode', '{}', 'running', 0),
                                                                        (8, 'transcode', '{}', 'done', 0)",
        [],
    )
    .unwrap();
    // 孤児なら GC が予約を取れ、その間は transcode の lock_path が取れない
    assert!(dbgc::lock_derived_for_gc(&c, "a/x.opus", GC_JOB, 1).unwrap());
    assert!(!dbderived::lock_path(&c, "a/x.opus", Some(2), 7, 1).unwrap());
    assert_eq!(
        env.count("SELECT count(*) FROM derived_path_locks WHERE job_id = 101"),
        1
    );
    dbderived::unlock_paths(&c, GC_JOB).unwrap();
    // running なジョブの予約があれば取れない。終わったジョブの残骸なら奪える
    assert!(dbderived::lock_path(&c, "a/y.opus", Some(2), 7, 1).unwrap());
    assert!(!dbgc::lock_derived_for_gc(&c, "a/y.opus", GC_JOB, 1).unwrap());
    c.execute(
        "INSERT INTO derived_path_locks (rel_path_key, track_id, job_id, acquired_at) VALUES ('a/z.opus', 2, 8, 0)",
        [],
    )
    .unwrap();
    assert!(dbgc::lock_derived_for_gc(&c, "a/z.opus", GC_JOB, 1).unwrap());
    // derived_files の行があれば予約しない
    env.derived_row(2, "A/t.opus");
    assert!(!dbgc::lock_derived_for_gc(&c, "a/t.opus", GC_JOB, 1).unwrap());
    assert_eq!(
        env.count("SELECT count(*) FROM derived_path_locks WHERE rel_path_key = 'a/t.opus'"),
        0
    );
}

#[tokio::test]
async fn stale_path_lock_of_a_finished_job_does_not_protect_an_orphan_and_gc_releases_its_locks() {
    let env = Env::new();
    env.track(2, "A/t.flac", None, None);
    env.put("Derived/A/stale.opus", b"s", 5 * DAY);
    env.conn()
        .execute(
            "INSERT INTO jobs (id, type, payload, state, created_at) VALUES (8, 'transcode', '{}', 'done', 0)",
            [],
        )
        .unwrap();
    env.conn()
        .execute(
            "INSERT INTO derived_path_locks (rel_path_key, track_id, job_id, acquired_at) VALUES ('a/stale.opus', 2, 8, 0)",
            [],
        )
        .unwrap();
    let s = run(&env).await;
    assert_eq!(s.derived.deleted, 1, "{s:?}");
    assert!(!env.path("Derived/A/stale.opus").exists());
    assert_eq!(env.count("SELECT count(*) FROM derived_path_locks"), 0);
}

#[tokio::test]
async fn scan_waits_while_gc_holds_the_library_mutex() {
    use spindle::import::scanner::Scanner;
    use spindle::jobs::handlers::scan::{new_scan_job, ScanHandler};
    let env = Env::new();
    env.retire_fake_gc_job();
    // 本物の running な gc が library mutex を持ち続ける
    let release = CancellationToken::new();
    let scanner = Arc::new(Scanner::new(env.db.clone(), env.roots.library.clone(), 2));
    let mut reg = Registry::new();
    reg.register(JobType::Scan, Arc::new(ScanHandler::new(scanner, 30)));
    reg.register_fn(JobType::Gc, Env::mutex_holder(release.clone()));
    let gc_id = match env.jobs.enqueue(new_gc_job()).await.unwrap() {
        EnqueueResult::Inserted(id) | EnqueueResult::Duplicate(id) => id,
    };
    env.jobs.start(reg, env.shutdown.clone());
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while env.job_state(gc_id) != JobState::Running && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(env.job_state(gc_id), JobState::Running);
    let scan_id = match env
        .jobs
        .enqueue(new_scan_job(spindle::db::scans::ScanKind::Incremental))
        .await
        .unwrap()
    {
        EnqueueResult::Inserted(id) | EnqueueResult::Duplicate(id) => id,
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.job_state(scan_id), JobState::Queued);
    release.cancel();
    assert_eq!(env.wait_job(gc_id).await, JobState::Done);
    assert_eq!(env.wait_job(scan_id).await, JobState::Done);
}

// ---------------------------------------------------------------- 失敗の局所化と猶予

#[tokio::test]
async fn unreadable_derived_subdirectory_is_skipped_and_other_categories_proceed() {
    use std::os::unix::fs::PermissionsExt;
    if nix_is_root() {
        eprintln!("root では権限で読めない状態を作れないので skip");
        return;
    }
    let env = Env::new();
    env.put("Derived/A/orphan.opus", b"a", 5 * DAY);
    env.put("Derived/Locked/hidden.opus", b"h", 5 * DAY);
    let locked = env.path("Derived/Locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    env.archived(1, "A/1.m4a", "held", -DAY);
    env.put("Archive/A/1.m4a", b"12345", 40 * DAY);

    let p = plan(&env.db, &env.roots, RETENTION, env.now).await.unwrap();
    let paths: Vec<&str> = p.derived.iter().map(|f| f.rel_path.as_str()).collect();
    assert_eq!(
        paths,
        vec!["A/orphan.opus"],
        "読めない側は飛ばし、読める側は拾う"
    );
    assert_eq!(p.archived.len(), 1);
    let s = execute_all(
        &env.db,
        &env.roots,
        &p,
        &CancellationToken::new(),
        Some(GC_JOB),
    )
    .await
    .unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!((s.derived.deleted, s.archived.deleted), (1, 1), "{s:?}");
    assert!(env.path("Derived/Locked/hidden.opus").exists());
    assert!(
        env.path("Derived/Locked").is_dir(),
        "読めなかったディレクトリは消さない"
    );
}

fn nix_is_root() -> bool {
    std::fs::metadata("/proc/self").is_ok_and(|m| {
        use std::os::unix::fs::MetadataExt;
        m.uid() == 0
    })
}

#[tokio::test]
async fn recently_touched_thumbs_dir_without_row_is_kept() {
    let env = Env::new();
    let fresh = "4".repeat(64);
    env.put(&format!("thumbs/{fresh}/orig.png"), b"f", 0);
    let old = "5".repeat(64);
    env.put(&format!("thumbs/{old}/orig.png"), b"o", 5 * DAY);
    set_age(&env.path(&format!("thumbs/{old}")), 5 * DAY);
    let p = plan(&env.db, &env.roots, RETENTION, env.now).await.unwrap();
    assert_eq!(
        p.artwork_dirs,
        vec![old.clone()],
        "スキャンが置いたばかりの dir は次回に回す"
    );
}

// ---------------------------------------------------------------- E: 編集履歴が参照する画像（D-60）

/// `edits` の `PICTURE` 値（旧 / 新）に現れる画像は、行も dir も回収しない（巻き戻しに要る）
#[tokio::test]
async fn artwork_referenced_by_picture_edits_is_kept() {
    let env = Env::new();
    let old = env.artwork(1, 0x11);
    let new = env.artwork(2, 0x22);
    let unrelated = env.artwork(3, 0x33);
    for d in [&old, &new, &unrelated] {
        env.put(&format!("thumbs/{d}/orig.png"), b"x", 5 * DAY);
        set_age(&env.path(&format!("thumbs/{d}")), 5 * DAY);
    }
    env.track(1, "A/t.flac", None, None);
    let c = env.conn();
    c.execute(
        "INSERT INTO edit_batches (id, created_at, state) VALUES (1, 0, 'applied')",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO edit_ops (id, batch_id, ordinal, track_id, kind, result) VALUES (1, 1, 1, 1, 'tags', 'applied')",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO edits (op_id, key, old_value, new_value) VALUES (1, 'PICTURE', ?1, ?2)",
        params![
            format!("[\"image/png:{old}\"]"),
            format!("[\"image/png:{new}\"]")
        ],
    )
    .unwrap();
    // 画像なし → 画像ありの op（旧値 null）も壊れない
    c.execute(
        "INSERT INTO edit_ops (id, batch_id, ordinal, track_id, kind, result) VALUES (2, 1, 2, 1, 'tags', 'applied')",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO edits (op_id, key, old_value, new_value) VALUES (2, 'PICTURE', 'null', ?1)",
        [format!("[\"image/png:{new}\"]")],
    )
    .unwrap();
    drop(c);

    let p = plan(&env.db, &env.roots, RETENTION, env.now).await.unwrap();
    assert_eq!(
        p.artwork_rows.iter().map(|a| a.id).collect::<Vec<_>>(),
        vec![3]
    );
    assert_eq!(p.artwork_dirs, vec![unrelated.clone()]);
    let s = run(&env).await;
    assert_eq!((s.artwork_rows.deleted, s.artwork_dirs.deleted), (1, 1));
    assert!(env.path(&format!("thumbs/{old}/orig.png")).exists());
    assert!(env.path(&format!("thumbs/{new}/orig.png")).exists());
    assert_eq!(env.count("SELECT count(*) FROM artwork"), 2);
}

/// アップロード直後（参照前）の行は dir と同じ 24 時間の猶予で残す
#[tokio::test]
async fn recently_uploaded_artwork_row_without_reference_is_kept() {
    let env = Env::new();
    let fresh = env.artwork(1, 0x11);
    env.put(&format!("thumbs/{fresh}/orig.png"), b"f", 0);
    let stale = env.artwork(2, 0x22);
    env.put(&format!("thumbs/{stale}/orig.png"), b"s", 5 * DAY);
    set_age(&env.path(&format!("thumbs/{stale}")), 5 * DAY);
    // dir の無い行は猶予に関係なく消える（実体が無いので使えない）
    env.artwork(3, 0x33);

    let p = plan(&env.db, &env.roots, RETENTION, env.now).await.unwrap();
    let mut rows: Vec<i64> = p.artwork_rows.iter().map(|a| a.id).collect();
    rows.sort_unstable();
    assert_eq!(rows, vec![2, 3]);
    assert_eq!(p.artwork_dirs, vec![stale.clone()]);
    let s = run(&env).await;
    assert_eq!(s.artwork_rows.deleted, 2, "{s:?}");
    assert_eq!(env.count("SELECT count(*) FROM artwork WHERE id = 1"), 1);
}

/// 計画の後に同じ画像が再アップロードされた（touch + upsert）行は、E(行) の削除直前の猶予の
/// 再確認で残る（codex の指摘: 計画済み id を参照の有無だけで消すと、アップロード直後に行だけが
/// 消えて GET / embed が artwork_not_found になる）
#[tokio::test]
async fn artwork_row_re_uploaded_after_planning_is_kept() {
    let env = Env::new();
    let stale = env.artwork(2, 0x22);
    env.put(&format!("thumbs/{stale}/orig.png"), b"s", 5 * DAY);
    set_age(&env.path(&format!("thumbs/{stale}")), 5 * DAY);
    let gone = env.artwork(3, 0x33);
    env.put(&format!("thumbs/{gone}/orig.png"), b"g", 5 * DAY);
    set_age(&env.path(&format!("thumbs/{gone}")), 5 * DAY);

    let p = plan(&env.db, &env.roots, RETENTION, env.now).await.unwrap();
    let mut rows: Vec<i64> = p.artwork_rows.iter().map(|a| a.id).collect();
    rows.sort_unstable();
    assert_eq!(rows, vec![2, 3]);

    // 計画の後にアップロード: put_original は既存と一致すれば dir を touch するだけなので、その
    // 経路（touch）と同じ writer での upsert を模す
    let hash = [0x22u8; 32];
    env.roots.artwork.touch(&hash).unwrap();
    env.db
        .write(move |c| spindle::db::artwork::upsert(c, &hash, "image/png", None, None, 1, "file"))
        .await
        .unwrap();

    let s = execute_rows(&env.db, &env.roots, &p).await.unwrap();
    assert_eq!(
        (s.artwork_rows.deleted, s.artwork_rows.skipped),
        (1, 1),
        "{s:?}"
    );
    assert_eq!(env.count("SELECT count(*) FROM artwork WHERE id = 2"), 1);
    assert_eq!(env.count("SELECT count(*) FROM artwork WHERE id = 3"), 0);
}

/// `tracks.artwork_id` が参照する画像は回収しない（D-61。FK SET NULL で消すと増分では復旧しない）
#[tokio::test]
async fn artwork_referenced_by_a_track_is_kept() {
    let env = Env::new();
    let own = env.artwork(1, 0x11);
    let orphan = env.artwork(2, 0x22);
    for d in [&own, &orphan] {
        env.put(&format!("thumbs/{d}/orig.png"), b"x", 5 * DAY);
        set_age(&env.path(&format!("thumbs/{d}")), 5 * DAY);
    }
    env.track(1, "A/t.flac", None, None);
    env.conn()
        .execute("UPDATE tracks SET artwork_id = 1 WHERE id = 1", [])
        .unwrap();
    let p = plan(&env.db, &env.roots, RETENTION, env.now).await.unwrap();
    assert_eq!(
        p.artwork_rows.iter().map(|a| a.id).collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(p.artwork_dirs, vec![orphan.clone()]);
    let s = run(&env).await;
    assert_eq!((s.artwork_rows.deleted, s.artwork_dirs.deleted), (1, 1));
    assert!(env.path(&format!("thumbs/{own}/orig.png")).exists());
    assert_eq!(env.count("SELECT artwork_id FROM tracks WHERE id = 1"), 1);
}

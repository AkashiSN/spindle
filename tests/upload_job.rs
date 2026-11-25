//! ジョブ種別 `upload`（展開 / 公開）と照合（`import::upload::reconcile`。SPEC §7.12「zip の展開」「Inbox へ置く（公開）」「照合とジョブ」）。
//! 一時ディレクトリの Inbox に本物の送信でファイルを置き、本物のワーカーでハンドラを走らせる。
//! jobs 行を直接 running にはしない（D-76）: 実行中の状態は本物のハンドラを故障注入・フックで
//! 止めて作る

#![cfg(target_os = "linux")]

mod common;

use std::io::{Cursor, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use spindle::config::{LayoutConfig, UploadConfig};
use spindle::db::inbox as dbinbox;
use spindle::db::jobs::{self as dbjobs, JobState, JobType};
use spindle::db::{open_memory_connection, Db};
use spindle::domain::relpath::canonical_key;
use spindle::fsroot::RootDir;
use spindle::import::inbox::PlaceItemEnv;
use spindle::import::upload::{
    begin_origin, create_upload, freeze, publish, read_manifest, receive_chunk, reconcile,
    with_manifest, Manifest, PublishStep, Stage, UploadId, UploadLocks, ZipError,
};
use spindle::jobs::handlers::inbox::{new_inbox_job, InboxHandler};
use spindle::jobs::handlers::upload::{
    ensure_inbox_scan, new_publish_job, new_unpack_job, upload_busy, ScanSecured, UploadHandler,
};
use spindle::jobs::{EnqueueResult, Jobs, Registry};

const NOW: i64 = 1_700_000_000;

fn layout() -> LayoutConfig {
    LayoutConfig {
        multi_disc: "{category}/{albumartist}/{album}/{disc}-{track:02} {title}".into(),
        single_disc: "{category}/{albumartist}/{album}/{track:02} {title}".into(),
        unsorted: "_Unsorted/{albumartist}/{album}/{track:02} {title}".into(),
    }
}

/// 公開の故障注入（`with_publish_fault` に渡す形）
type Fault = Arc<dyn Fn(PublishStep) -> bool + Send + Sync>;

struct Env {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    db: Arc<Db>,
    jobs: Arc<Jobs>,
    inbox: Arc<RootDir>,
    library: Arc<RootDir>,
    locks: UploadLocks,
    id: UploadId,
    cfg: UploadConfig,
    shutdown: CancellationToken,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["Inbox", "Library", "src"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let inbox = Arc::new(RootDir::open(&dir.path().join("Inbox")).unwrap());
        let library = Arc::new(RootDir::open(&dir.path().join("Library")).unwrap());
        let m = create_upload(&inbox, NOW).unwrap();
        let id = UploadId::parse(&m.id).unwrap();
        Self {
            dir,
            db_path,
            jobs: Jobs::new(db.clone()),
            db,
            inbox,
            library,
            locks: UploadLocks::default(),
            id,
            cfg: UploadConfig::default(),
            shutdown: CancellationToken::new(),
        }
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.db_path).unwrap()
    }

    fn handler(&self) -> UploadHandler {
        UploadHandler::new(self.inbox.clone(), self.locks.clone(), self.cfg.clone())
    }

    /// 本物の inbox ハンドラ（`after_scan` があれば走査の後でそれを待つ）
    fn inbox_handler(&self, after_scan: Option<Arc<Gate>>) -> InboxHandler {
        let h = InboxHandler::new(PlaceItemEnv {
            db: self.db.clone(),
            library: self.library.clone(),
            inbox: self.inbox.clone(),
            jobs: self.jobs.clone(),
            layout: layout(),
            editor: None,
            wav_to_flac: false,
            before_place: None,
            artwork: None,
            before_artwork: None,
            coverart: None,
        });
        match after_scan {
            Some(gate) => h.with_after_scan(Arc::new(move || {
                let gate = gate.clone();
                Box::pin(async move { gate.pass().await })
            })),
            None => h,
        }
    }

    /// ワーカーを起動する（`[inbox].poll_interval_secs = 0` 相当: 周期の監視は回さない）
    fn start(&self, upload: Option<UploadHandler>, inbox: Option<InboxHandler>) {
        let mut reg = Registry::new();
        if let Some(h) = upload {
            reg.register(JobType::Upload, Arc::new(h));
        }
        if let Some(h) = inbox {
            reg.register(JobType::Inbox, Arc::new(h));
        }
        self.jobs.start(reg, self.shutdown.clone());
    }

    fn wav(&self, name: &str, seed: u32, album: Option<&str>, track: u32) -> Vec<u8> {
        let p = self.dir.path().join("src").join(format!("{seed}-{name}"));
        common::write_wav(&p, &common::pcm_samples(seed), 16);
        if let Some(album) = album {
            common::set_basic_tags(&p, name, "Artist", album, "Artist", track, 1);
        }
        std::fs::read(&p).unwrap()
    }

    fn send(&self, client_path: &str, bytes: &[u8]) -> String {
        let o = begin_origin(
            &self.inbox,
            &self.locks,
            &self.id,
            client_path,
            bytes.len() as u64,
            &self.cfg,
            NOW,
        )
        .unwrap();
        let r = receive_chunk(
            &self.inbox,
            &self.locks,
            &self.id,
            &o.id,
            0,
            bytes,
            NOW,
            &|_| false,
        )
        .unwrap();
        assert!(r.complete);
        o.id
    }

    /// 2 件: タグ付きの `X`（2 曲）と、ばらの未タグ 1 曲
    fn two_groups(&self) {
        let x1 = self.wav("01.wav", 1, Some("X"), 1);
        let x2 = self.wav("02.wav", 2, Some("X"), 2);
        let loose = self.wav("loose.wav", 3, None, 1);
        self.send("X/01.wav", &x1);
        self.send("X/02.wav", &x2);
        self.send("loose.wav", &loose);
    }

    /// タグ付きの 2 曲を入れた zip を送る。返り値は origin
    fn send_zip(&self) -> String {
        let x1 = self.wav("01.wav", 11, Some("Z"), 1);
        let x2 = self.wav("02.wav", 12, Some("Z"), 2);
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in [("Z/01.wav", &x1), ("Z/02.wav", &x2)] {
            w.start_file(name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        let bytes = w.finish().unwrap().into_inner();
        self.send("z.zip", &bytes)
    }

    fn freeze(&self) {
        let conn = open_memory_connection().unwrap();
        freeze(
            &self.inbox,
            &self.locks,
            &self.id,
            &conn,
            &layout(),
            &self.cfg,
            NOW,
        )
        .unwrap();
    }

    fn manifest(&self) -> Manifest {
        read_manifest(&self.inbox, &self.id).unwrap()
    }

    fn edit(&self, f: impl FnOnce(&mut Manifest)) {
        with_manifest(&self.inbox, &self.locks, &self.id, |m| {
            f(m);
            Ok(())
        })
        .unwrap();
    }

    async fn reconcile(&self) -> usize {
        reconcile(&self.db, self.inbox.clone(), &self.locks)
            .await
            .unwrap()
    }

    fn enqueue(&self, job: dbjobs::NewJob) -> i64 {
        dbjobs::enqueue(&self.conn(), &job, NOW).unwrap().id()
    }

    fn job(&self, id: i64) -> dbjobs::Job {
        dbjobs::get(&self.conn(), id).unwrap().unwrap()
    }

    /// 種別のジョブ行（id 順）
    fn jobs_of(&self, ty: JobType) -> Vec<dbjobs::Job> {
        let c = self.conn();
        let mut st = c
            .prepare("SELECT id FROM jobs WHERE type = ?1 ORDER BY id")
            .unwrap();
        let ids: Vec<i64> = st
            .query_map([ty.as_str()], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        ids.into_iter().map(|id| self.job(id)).collect()
    }

    async fn wait_until(&self, what: &str, mut f: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("待ちきれない: {what}");
    }

    async fn wait_terminal(&self, id: i64) -> dbjobs::Job {
        self.wait_until("ジョブの終端", || self.job(id).state.is_terminal())
            .await;
        self.job(id)
    }

    fn inbox_path(&self) -> PathBuf {
        self.dir.path().join("Inbox")
    }

    /// `upload/` の下の件のディレクトリ（Inbox 相対。名前順）
    fn published_dirs(&self) -> Vec<String> {
        fn walk(base: &Path, rel: &str, out: &mut Vec<String>) {
            let Ok(rd) = std::fs::read_dir(base.join(rel)) else {
                return;
            };
            let mut has_file = false;
            for e in rd {
                let e = e.unwrap();
                let name = e.file_name().to_str().unwrap().to_string();
                let child = format!("{rel}/{name}");
                if e.file_type().unwrap().is_dir() {
                    walk(base, &child, out);
                } else {
                    has_file = true;
                }
            }
            if has_file {
                out.push(rel.to_string());
            }
        }
        let mut out = Vec::new();
        walk(&self.inbox_path(), "upload", &mut out);
        out.sort();
        out
    }

    fn inbox_item(&self, rel_dir: &str) -> Option<dbinbox::Item> {
        dbinbox::find_by_dir_key(&self.conn(), &canonical_key(rel_dir)).unwrap()
    }
}

/// inbox ハンドラを走査の後で止める門。最初の 1 回だけ止まり、`open` で通す
struct Gate {
    calls: AtomicUsize,
    entered: Notify,
    released: Notify,
}

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            entered: Notify::new(),
            released: Notify::new(),
        })
    }

    async fn pass(&self) {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.notify_one();
            self.released.notified().await;
        }
    }

    async fn wait_entered(&self) {
        tokio::time::timeout(Duration::from_secs(60), self.entered.notified())
            .await
            .expect("inbox ハンドラが走査を終えない");
    }

    fn open(&self) {
        self.released.notify_one();
    }
}

// ---------------------------------------------------------------- 種別とスキーマ

#[test]
fn job_type_roundtrip_includes_upload() {
    assert!(JobType::ALL.contains(&JobType::Upload));
    assert_eq!(JobType::ALL.len(), 23);
    assert_eq!(JobType::Upload.as_str(), "upload");
    assert_eq!("upload".parse::<JobType>().unwrap(), JobType::Upload);
    assert_eq!(JobType::Upload.concurrency(8), 1);
    let id = UploadId::parse("0123456789abcdef").unwrap();
    let job = new_unpack_job(&id, "o2", true);
    assert_eq!(job.job_type, JobType::Upload);
    assert_eq!(
        job.payload,
        serde_json::json!({ "upload_id": "0123456789abcdef", "op": "unpack", "origin": "o2", "retry": true })
    );
    assert_eq!(
        job.dedup_key.as_deref(),
        Some("upload:0123456789abcdef:unpack:o2")
    );
    let job = new_publish_job(&id);
    assert_eq!(
        job.payload,
        serde_json::json!({ "upload_id": "0123456789abcdef", "op": "publish" })
    );
    assert_eq!(
        job.dedup_key.as_deref(),
        Some("upload:0123456789abcdef:publish")
    );
    assert_eq!(
        dbjobs::subject_of(JobType::Upload, &job.payload, None, None, None).as_deref(),
        Some("0123456789abcdef")
    );
}

#[test]
fn migration_accepts_upload_type() {
    let conn = open_memory_connection().unwrap();
    let id = UploadId::parse("0123456789abcdef").unwrap();
    let job_id = dbjobs::enqueue(&conn, &new_publish_job(&id), NOW)
        .unwrap()
        .id();
    assert_eq!(
        dbjobs::get(&conn, job_id).unwrap().unwrap().job_type,
        JobType::Upload
    );
    // 投入済みの間は dedup でまとまり、upload_busy が立つ（別のアップロードは立たない）
    assert!(matches!(
        dbjobs::enqueue(&conn, &new_publish_job(&id), NOW).unwrap(),
        EnqueueResult::Duplicate(j) if j == job_id
    ));
    assert!(upload_busy(&conn, &id).unwrap());
    let other = UploadId::parse("0123456789abcdee").unwrap();
    assert!(!upload_busy(&conn, &other).unwrap());
    // 実行中も立つ（claim で running にする）。終端になれば外れる
    let claimed = dbjobs::claim_next(&conn, JobType::Upload, NOW)
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, job_id);
    assert!(upload_busy(&conn, &id).unwrap());
    assert!(dbjobs::mark_done(&conn, job_id, NOW, None).unwrap());
    assert!(!upload_busy(&conn, &id).unwrap());
    // 既存の索引も作り直されている
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name IN \
             ('idx_jobs_dedup_active','idx_jobs_queue','idx_jobs_finished','idx_jobs_batch')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 4);
}

#[test]
fn ensure_inbox_scan_secures_new_or_queued() {
    let conn = open_memory_connection().unwrap();
    assert_eq!(ensure_inbox_scan(&conn).unwrap(), ScanSecured::Secured);
    // 待ちの inbox ジョブと重なっても、それは公開の後に走るので確保できている
    assert_eq!(ensure_inbox_scan(&conn).unwrap(), ScanSecured::Secured);
    let n: i64 = conn
        .query_row("SELECT count(*) FROM jobs WHERE type = 'inbox'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 1);
}

// ---------------------------------------------------------------- ハンドラ

#[tokio::test]
async fn unpack_job_marks_generation() {
    let env = Env::new();
    let origin = env.send_zip();
    env.start(Some(env.handler()), None);
    let job = env.enqueue(new_unpack_job(&env.id, &origin, false));
    let done = env.wait_terminal(job).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.last_error);
    assert_eq!(
        done.note.as_deref(),
        Some("展開: 2 ファイル（取り込めなかった 0）")
    );
    let m = env.manifest();
    let z = m.origins[0].zip.as_ref().unwrap();
    assert_eq!(z.generation, 1);
    assert!(z.error.is_none());
}

#[tokio::test]
async fn publish_job_enqueues_inbox_scan() {
    let env = Env::new();
    env.two_groups();
    env.freeze();
    env.start(Some(env.handler()), None);
    let job = env.enqueue(new_publish_job(&env.id));
    let done = env.wait_terminal(job).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.last_error);
    let note = done.note.unwrap();
    assert!(note.starts_with("Inbox へ置いた: 2 件\n"), "{note}");
    assert!(note.contains("upload/Artist/X"), "{note}");
    let m = env.manifest();
    assert!(m.inbox_enqueued_at.is_some());
    assert!(m.discarded_at.is_some());
    let inbox = env.jobs_of(JobType::Inbox);
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].state, JobState::Queued);
    assert_eq!(env.published_dirs().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbox_duplicate_running_requeues_publish() {
    let env = Env::new();
    env.two_groups();
    env.freeze();
    let gate = Gate::new();
    env.start(
        Some(env.handler()),
        Some(env.inbox_handler(Some(gate.clone()))),
    );
    // inbox ジョブを走らせ、走査を終えたところで止める（公開の前の走査なので新しい件は見ない）
    let first_inbox = env.enqueue(new_inbox_job());
    gate.wait_entered().await;
    assert_eq!(env.job(first_inbox).state, JobState::Running);

    let job = env.enqueue(new_publish_job(&env.id));
    // 公開は終わるが、走っている inbox と重なったので確保できず、後でやり直す
    env.wait_until("upload ジョブの再キュー", || {
        let j = env.job(job);
        j.state == JobState::Queued && j.run_after.is_some()
    })
    .await;
    assert_eq!(env.published_dirs().len(), 2);
    let m = env.manifest();
    assert!(m.inbox_enqueued_at.is_none());
    assert!(m.discarded_at.is_none());
    assert_eq!(env.jobs_of(JobType::Inbox).len(), 1);

    gate.open();
    let first = env.wait_terminal(first_inbox).await;
    assert_eq!(first.state, JobState::Done, "{:?}", first.last_error);
    let done = env.wait_terminal(job).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.last_error);
    let m = env.manifest();
    assert!(m.inbox_enqueued_at.is_some());
    assert!(m.discarded_at.is_some());
    // やり直しで新しい inbox ジョブが入り、走査が件を見つける
    let inbox = env.jobs_of(JobType::Inbox);
    assert_eq!(inbox.len(), 2);
    let second = env.wait_terminal(inbox[1].id).await;
    assert_eq!(second.state, JobState::Done, "{:?}", second.last_error);
    let item = env
        .inbox_item("upload/Artist/X")
        .expect("件が承認一覧に出る");
    assert_eq!(item.state, dbinbox::ItemState::Pending);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_recovery_reruns_publish_idempotently() {
    let env = Env::new();
    env.two_groups();
    env.freeze();
    // 1 件目を宛先へ rename した後（マニフェストは Publishing のまま）で止め、その間にプロセスを止める
    let (reached_tx, reached_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let reached_tx = Mutex::new(Some(reached_tx));
    let release_rx = Mutex::new(release_rx);
    let fault: Fault = Arc::new(move |step| {
        if step != PublishStep::AfterRename {
            return false;
        }
        if let Some(tx) = reached_tx.lock().unwrap().take() {
            tx.send(()).unwrap();
            let _ = release_rx.lock().unwrap().recv();
        }
        // 止めたプロセスの続きは何も変えない
        true
    });
    let mut reg = Registry::new();
    reg.register(
        JobType::Upload,
        Arc::new(env.handler().with_publish_fault(fault)),
    );
    let worker = env.jobs.start(reg, env.shutdown.clone());
    let job = env.enqueue(new_publish_job(&env.id));
    tokio::task::spawn_blocking(move || reached_rx.recv_timeout(Duration::from_secs(60)))
        .await
        .unwrap()
        .expect("公開が rename まで進まない");
    env.shutdown.cancel();
    worker.await.unwrap();
    release_tx.send(()).unwrap();
    assert_eq!(env.job(job).state, JobState::Running);
    let m = env.manifest();
    let stages: Vec<_> = m
        .publish
        .as_ref()
        .unwrap()
        .groups
        .iter()
        .map(|g| g.stage.clone())
        .collect();
    assert!(matches!(stages[0], Stage::Publishing { .. }), "{stages:?}");

    // 再起動: リカバリで queued に戻し、新しいワーカーで続きから
    let report = spindle::jobs::recovery::run(&env.db).await.unwrap();
    assert_eq!(report.requeued, 1);
    assert_eq!(env.job(job).state, JobState::Queued);
    let jobs2 = Jobs::new(env.db.clone());
    let mut reg = Registry::new();
    reg.register(JobType::Upload, Arc::new(env.handler()));
    let shutdown2 = CancellationToken::new();
    jobs2.start(reg, shutdown2.clone());
    let done = env.wait_terminal(job).await;
    shutdown2.cancel();
    assert_eq!(done.state, JobState::Done, "{:?}", done.last_error);
    // 件は二重にならない（` (2)` ができない）
    let dirs = env.published_dirs();
    assert_eq!(dirs.len(), 2, "{dirs:?}");
    assert!(dirs.iter().all(|d| !d.contains(" (2)")), "{dirs:?}");
    assert!(env.manifest().inbox_enqueued_at.is_some());
    assert_eq!(env.jobs_of(JobType::Inbox).len(), 1);
}

// ---------------------------------------------------------------- 照合

#[tokio::test]
async fn reconcile_requeues_unpack_after_crash_before_enqueue() {
    let env = Env::new();
    let origin = env.send_zip();
    // zip は受け取り終えたが、ジョブを投入する前に落ちた
    assert!(env.jobs_of(JobType::Upload).is_empty());
    assert_eq!(env.reconcile().await, 1);
    let jobs = env.jobs_of(JobType::Upload);
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].payload,
        new_unpack_job(&env.id, &origin, false).payload
    );
    assert_eq!(
        jobs[0].dedup_key.as_deref(),
        Some(format!("upload:{}:unpack:{origin}", env.id.as_str()).as_str())
    );
    // 照合し直しても重ならない
    assert_eq!(env.reconcile().await, 0);
    assert_eq!(env.jobs_of(JobType::Upload).len(), 1);
}

#[tokio::test]
async fn reconcile_requeues_publish_after_db_loss() {
    let env = Env::new();
    env.two_groups();
    env.freeze();
    // DB が消えた（jobs は空）: マニフェストから公開を投入し直す
    assert!(env.jobs_of(JobType::Upload).is_empty());
    assert_eq!(env.reconcile().await, 1);
    let jobs = env.jobs_of(JobType::Upload);
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].payload, new_publish_job(&env.id).payload);
}

#[tokio::test]
async fn reconcile_does_not_duplicate_active_job() {
    let env = Env::new();
    let origin = env.send_zip();
    let job = env.enqueue(new_unpack_job(&env.id, &origin, false));
    assert_eq!(env.reconcile().await, 0);
    let jobs = env.jobs_of(JobType::Upload);
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, job);
}

#[tokio::test]
async fn reconcile_skips_failed_zip() {
    let env = Env::new();
    env.send_zip();
    env.edit(|m| {
        m.origins[0].zip.get_or_insert_with(Default::default).error = Some(ZipError {
            reason: "壊れた".into(),
            retryable: false,
        });
    });
    assert_eq!(env.reconcile().await, 0);
    // やり直せる失敗も、人が「展開し直す」を押すまで投入しない
    env.edit(|m| {
        m.origins[0].zip.as_mut().unwrap().error = Some(ZipError {
            reason: "空き容量が足りない".into(),
            retryable: true,
        });
    });
    assert_eq!(env.reconcile().await, 0);
    // 捨てたアップロードも投入しない
    env.edit(|m| {
        m.origins[0].zip.as_mut().unwrap().error = None;
        m.discarded_at = Some(NOW);
    });
    assert_eq!(env.reconcile().await, 0);
    assert!(env.jobs_of(JobType::Upload).is_empty());
}

#[tokio::test]
async fn reconcile_enqueues_inbox_after_crash_post_publish() {
    let env = Env::new();
    env.two_groups();
    env.freeze();
    // 最後の件を Published にした後・inbox を投入する前に落ちた
    let published = AtomicUsize::new(0);
    let r = publish(
        &env.inbox,
        &env.locks,
        &env.id,
        NOW,
        &CancellationToken::new(),
        &|step| {
            step == PublishStep::AfterPublishedWrite
                && published.fetch_add(1, Ordering::SeqCst) == 1
        },
    );
    assert!(r.is_err());
    let m = env.manifest();
    assert!(m
        .publish
        .as_ref()
        .unwrap()
        .groups
        .iter()
        .all(|g| matches!(g.stage, Stage::Published { .. })));
    assert!(m.inbox_enqueued_at.is_none());

    assert_eq!(env.reconcile().await, 1);
    env.start(Some(env.handler()), None);
    let job = env.jobs_of(JobType::Upload)[0].id;
    let done = env.wait_terminal(job).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.last_error);
    let inbox = env.jobs_of(JobType::Inbox);
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].state, JobState::Queued);
    let m = env.manifest();
    assert!(m.inbox_enqueued_at.is_some());
    assert!(m.discarded_at.is_some());
    assert_eq!(env.published_dirs().len(), 2);
    // 確保した後の照合は何も投入しない
    assert_eq!(env.reconcile().await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconcile_hands_off_to_publish_job_when_inbox_running() {
    let env = Env::new();
    env.two_groups();
    env.freeze();
    // 全件公開済み・publish ジョブ行なし
    let r = publish(
        &env.inbox,
        &env.locks,
        &env.id,
        NOW,
        &CancellationToken::new(),
        &|_| false,
    )
    .unwrap();
    assert!(r.finished);
    assert!(env.jobs_of(JobType::Upload).is_empty());

    let gate = Gate::new();
    env.start(
        Some(env.handler()),
        Some(env.inbox_handler(Some(gate.clone()))),
    );
    let first_inbox = env.enqueue(new_inbox_job());
    gate.wait_entered().await;

    // 起動時の照合を 1 回だけ（以後 GET による照合は無い）
    assert_eq!(env.reconcile().await, 1);
    let job = env.jobs_of(JobType::Upload)[0].id;
    env.wait_until("upload ジョブの再キュー", || {
        let j = env.job(job);
        j.state == JobState::Queued && j.run_after.is_some()
    })
    .await;
    assert!(env.manifest().inbox_enqueued_at.is_none());

    gate.open();
    env.wait_terminal(first_inbox).await;
    let done = env.wait_terminal(job).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.last_error);
    let inbox = env.jobs_of(JobType::Inbox);
    assert_eq!(inbox.len(), 2);
    let second = env.wait_terminal(inbox[1].id).await;
    assert_eq!(second.state, JobState::Done, "{:?}", second.last_error);
    assert!(env.inbox_item("upload/Artist/X").is_some(), "件が出る");
    assert!(env.manifest().inbox_enqueued_at.is_some());
}

#[tokio::test]
async fn publish_conflict_records_error_and_reconcile_skips() {
    let env = Env::new();
    env.two_groups();
    env.freeze();
    // 凍結した計画のメンバーが外から消えた: 公開はやり直しても変わらない失敗（Conflict）で止まる
    let from = env.manifest().publish.unwrap().groups[0].members[0]
        .from
        .clone();
    std::fs::remove_file(
        env.inbox_path()
            .join(env.id.staging_dir().as_str())
            .join(&from),
    )
    .unwrap();
    env.start(Some(env.handler()), None);
    let job = env.enqueue(new_publish_job(&env.id));
    let done = env.wait_terminal(job).await;
    assert_eq!(done.state, JobState::Failed, "{:?}", done.note);
    let m = env.manifest();
    let err = m.publish_error.expect("公開の失敗がマニフェストに残る");
    assert!(err.contains("公開するファイルが無い"), "{err}");
    assert!(m.inbox_enqueued_at.is_none());
    // 照合は失敗で止まった公開を投入し直さない（一覧の取得のたびに失敗するジョブを作らない）
    assert_eq!(env.reconcile().await, 0);
    assert_eq!(env.jobs_of(JobType::Upload).len(), 1);
}

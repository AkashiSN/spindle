//! 端末ジョブ（P5-3b Task 5）。偽の adb が手元の sh で端末側スクリプトを実行し、
//! `<tmp>/storage/emulated/0/Music/spindle` を端末の保存先として使う

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{params, OptionalExtension as _};
use spindle::config::{DerivedConfig, OpusVariantConfig};
use spindle::db::devices::{self, Confirm, Device, NewDevice, PlanEnd, Selection};
use spindle::db::{derived, now_epoch, Db};
use spindle::device::adb::AdbConfig;
use spindle::device::recover::STALE_TOKEN;
use spindle::device::runtime::AdbRuntime;
use spindle::device::store;
use spindle::device::sync::SourceError;
use spindle::device::track::TrackedDevice;
use spindle::domain::derived::Variant;
use spindle::domain::device::{semantic_master, sha256_hex, SourceHash, SourceKind, Transport};
use spindle::domain::relpath::RelPath;
use spindle::fsroot::{self, RootDir};
use spindle::jobs::handlers::device::{
    on_connected, scan_job, sync_job, verify_job, DeviceEnv, ScanHandler, SyncHandler,
    VerifyHandler,
};
use spindle::jobs::{Job, JobState, JobType, Jobs, Registry};
use tokio_util::sync::CancellationToken;

/// 偽の adb。書き込み中の fd を別スレッドの fork が引き継ぐと ETXTBSY になるので、1 度だけ書く
fn fake_adb() -> PathBuf {
    static ONCE: std::sync::OnceLock<(tempfile::TempDir, PathBuf)> = std::sync::OnceLock::new();
    ONCE.get_or_init(write_fake_adb).1.clone()
}

/// `tests/device_adb.rs` と同じ本体（SER1 だけ・`tcp:adb:5037`・HOME 必須・`shell` と `get-state`）
fn write_fake_adb() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("adb");
    std::fs::write(
        &path,
        r#"#!/bin/sh
[ "$ADB_SERVER_SOCKET" = "tcp:adb:5037" ] || { echo "* cannot connect to daemon at $ADB_SERVER_SOCKET" >&2; exit 1; }
[ -n "$HOME" ] && [ -d "$HOME" ] && [ -w "$HOME" ] || { echo "adb_utils.cpp:315 Cannot mkdir '$HOME/.android': Permission denied" >&2; exit 134; }
[ "$1" = "-s" ] || { echo "-s が無い" >&2; exit 2; }
[ "$2" = "SER1" ] || { echo "adb: device '$2' not found" >&2; exit 1; }
if [ "$3" = "get-state" ]; then
  [ "$#" -eq 3 ] || { echo "引数の数が違う: $#" >&2; exit 2; }
  if [ -f "$HOME/offline" ]; then echo "adb: device '$2' not found" >&2; exit 1; fi
  if [ -f "$HOME/unauthorized" ]; then echo "unauthorized"; exit 0; fi
  echo device; exit 0
fi
[ "$3" = "shell" ] || { echo "未対応のサブコマンド $3" >&2; exit 2; }
[ "$#" -eq 4 ] || { echo "引数の数が違う: $#" >&2; exit 2; }
if [ -f "$HOME/drop" ]; then exit 255; fi
exec sh -c "$4"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (dir, path)
}

const TRACK: &str = "YT/a.opus";

struct Fx {
    _dir: tempfile::TempDir,
    db: Arc<Db>,
    jobs: Arc<Jobs>,
    rt: Arc<AdbRuntime>,
    shutdown: CancellationToken,
    library: PathBuf,
    library_root: Arc<RootDir>,
    derived_root: Arc<RootDir>,
    device_root: PathBuf,
    device: Device,
}

impl Drop for Fx {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

impl Fx {
    /// Opus 系統が有効、原本が Opus の曲 1 つ（`YT/a.opus` = "AAAA"）とハッシュ、adb の端末（SER1、emulated）
    async fn new() -> Fx {
        let _ = fake_adb();
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let library = base.join("library");
        std::fs::create_dir_all(library.join("YT")).unwrap();
        std::fs::write(library.join(TRACK), b"AAAA").unwrap();
        let derived_dir = base.join("derived");
        std::fs::create_dir_all(&derived_dir).unwrap();
        let home = base.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let storage = base.join("storage");
        let device_root = storage.join("emulated/0/Music/spindle");

        let library_root = Arc::new(RootDir::open(&library).unwrap());
        let derived_root = Arc::new(RootDir::open(&derived_dir).unwrap());
        let st = fsroot::fstat(
            &library_root
                .open_file(&RelPath::parse(TRACK).unwrap())
                .unwrap(),
        )
        .unwrap();

        let db = Arc::new(Db::open(&base.join("spindle.db")).unwrap());
        let device = db
            .write(move |c| {
                let cfg = DerivedConfig {
                    opus: OpusVariantConfig {
                        enabled: true,
                        bitrate: 256,
                    },
                    aac: Default::default(),
                };
                derived::sync_variants(c, &cfg, false, 0)?;
                c.execute(
                    "INSERT INTO tracks (id, rel_path, rel_path_key, inode, size, mtime_ns, ctime_ns,
                                         codec, lossless, channels, audio_version, tag_version, seen_at)
                     VALUES (1, ?1, lower(?1), ?2, ?3, ?4, ?5, 'opus', 0, 2, 1, 1, 0)",
                    params![TRACK, st.inode as i64, st.size as i64, st.mtime_ns, st.ctime_ns],
                )?;
                let h = SourceHash {
                    semantic: semantic_master(1, 1),
                    inode: st.inode,
                    size: st.size,
                    mtime_ns: st.mtime_ns,
                    ctime_ns: st.ctime_ns,
                    sha256: sha256_hex(b"AAAA"),
                };
                devices::put_source_hash(c, 1, SourceKind::Master, &h, 0)?;
                devices::create(
                    c,
                    &NewDevice {
                        name: "Xperia",
                        transport: Transport::Adb,
                        variant: Variant::Opus,
                        selection: Selection::All,
                        adb: Some(("SER1", "emulated", "Music/spindle")),
                    },
                    0,
                )
            })
            .await
            .unwrap();

        let shutdown = CancellationToken::new();
        let rt = AdbRuntime::with_storage_base(
            AdbConfig {
                program: fake_adb(),
                server: "tcp:adb:5037".into(),
                home,
                timeout: Duration::from_secs(30),
                transfer_timeout: Duration::from_secs(60),
            },
            shutdown.clone(),
            storage.to_str().unwrap().to_owned(),
        );
        let jobs = Jobs::new(db.clone());
        Fx {
            _dir: dir,
            db,
            jobs,
            rt,
            shutdown,
            library,
            library_root,
            derived_root,
            device_root,
            device,
        }
    }

    fn start(&self) {
        let env = Arc::new(DeviceEnv {
            db: self.db.clone(),
            jobs: self.jobs.clone(),
            rt: self.rt.clone(),
            library: self.library_root.clone(),
            derived: self.derived_root.clone(),
        });
        let mut reg = Registry::new();
        reg.register(JobType::DeviceScan, Arc::new(ScanHandler::new(env.clone())))
            .register(JobType::DeviceSync, Arc::new(SyncHandler::new(env.clone())))
            .register(JobType::DeviceVerify, Arc::new(VerifyHandler::new(env)));
        self.jobs.start(reg, self.shutdown.clone());
    }

    fn connect(&self) {
        self.rt.apply(vec![TrackedDevice {
            serial: "SER1".into(),
            state: "device".into(),
            model: None,
        }]);
    }

    async fn initialize_as(&self, uuid: &str) {
        let fs = self.rt.fs_for("SER1", "emulated", "Music/spindle").unwrap();
        store::initialize(&fs, uuid, "emulated").await.unwrap();
    }

    async fn initialize(&self) {
        let uuid = self.device.uuid.clone();
        self.initialize_as(&uuid).await;
    }

    async fn confirm(&self) -> i64 {
        let id = self.device.id;
        self.db
            .write(move |c| {
                let comp = devices::compute(c, id)?.unwrap();
                match devices::confirm_plan(c, id, &comp.plan_token, now_epoch())? {
                    Confirm::Created(p) => Ok(p.id),
                    other => panic!("計画を確定できない: {other:?}"),
                }
            })
            .await
            .unwrap()
    }

    async fn wait_until(&self, id: i64, what: &str, pred: impl Fn(&Job) -> bool) -> Job {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let job = self.jobs.get(id).await.unwrap().unwrap();
            if pred(&job) {
                return job;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "job {id} が {what} にならない（現在 {:?}、error {:?}）",
                job.state,
                job.last_error
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait(&self, id: i64, want: JobState) -> Job {
        self.wait_until(id, &format!("{want:?}"), |j| j.state == want)
            .await
    }

    /// ジョブ `job` が job_mutexes の `device:<id>` を持つまで待つ
    async fn wait_device_mutex(&self, job: i64) {
        let (db, dev) = (self.db.clone(), self.device.id);
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let holder: Option<i64> = db
                .read(move |c| {
                    Ok(c.query_row(
                        "SELECT job_id FROM job_mutexes WHERE name = ?1",
                        [spindle::jobs::device_mutex(dev)],
                        |r| r.get(0),
                    )
                    .optional()?)
                })
                .await
                .unwrap();
            if holder == Some(job) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "ジョブ {job} が device:{dev} を取らない（{holder:?}）"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// 計画を確定して同期し、完了を待つ
    async fn sync_once(&self) -> Job {
        let plan_id = self.confirm().await;
        let id = self
            .jobs
            .enqueue(sync_job(self.device.id, plan_id))
            .await
            .unwrap()
            .id();
        self.wait(id, JobState::Done).await
    }

    async fn items(&self) -> Vec<spindle::domain::device::DeviceItem> {
        let id = self.device.id;
        self.db.read(move |c| devices::items(c, id)).await.unwrap()
    }

    async fn scan_jobs(&self) -> i64 {
        self.db
            .read(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM jobs WHERE type = 'device_scan'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap()
    }
}

fn device_file(root: &Path) -> PathBuf {
    root.join(TRACK)
}

#[tokio::test]
async fn sync_sends_the_plan_and_completes_it() {
    let fx = Fx::new().await;
    fx.connect();
    fx.initialize().await;
    fx.start();
    let plan_id = fx.confirm().await;
    let id = fx
        .jobs
        .enqueue(sync_job(fx.device.id, plan_id))
        .await
        .unwrap()
        .id();
    let job = fx.wait(id, JobState::Done).await;
    let note = job.note.unwrap();
    assert!(note.contains("送った 1 件・保留 0 件"), "{note}");
    assert_eq!(
        std::fs::read(device_file(&fx.device_root)).unwrap(),
        b"AAAA"
    );
    let dev = fx.device.id;
    let (items, state, synced) = fx
        .db
        .read(move |c| {
            let items = devices::items(c, dev)?;
            let state: String = c.query_row(
                "SELECT state FROM device_sync_plans WHERE id = ?1",
                [plan_id],
                |r| r.get(0),
            )?;
            let synced = devices::get(c, dev)?.unwrap().last_synced_at;
            Ok((items, state, synced))
        })
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].track_id, 1);
    assert_eq!(items[0].sha256, sha256_hex(b"AAAA"));
    assert_eq!(state, "completed");
    assert!(synced.is_some());
    assert!(fx.rt.free(fx.device.id).is_some());
}

#[tokio::test]
async fn sync_waits_without_spending_attempts_until_connected() {
    let fx = Fx::new().await;
    fx.initialize().await; // fs_for は接続状態を見ないので初期化はできる
    fx.start();
    let plan_id = fx.confirm().await;
    let before = now_epoch();
    let id = fx
        .jobs
        .enqueue(sync_job(fx.device.id, plan_id))
        .await
        .unwrap()
        .id();
    // 一度走って、未接続なので待ちへ戻る
    let job = fx
        .wait_until(id, "待ちへ戻る", |j| {
            j.state == JobState::Queued && j.run_after.is_some_and(|t| t >= before + 290)
        })
        .await;
    assert_eq!(job.attempts, 0);
    assert!(!device_file(&fx.device_root).exists());
    fx.connect();
    assert_eq!(fx.jobs.wake_device(fx.device.id).await.unwrap(), vec![id]);
    fx.wait(id, JobState::Done).await;
    assert_eq!(
        std::fs::read(device_file(&fx.device_root)).unwrap(),
        b"AAAA"
    );
}

#[tokio::test]
async fn sync_of_a_closed_plan_is_a_no_op() {
    let fx = Fx::new().await;
    fx.connect();
    fx.initialize().await;
    fx.start();
    let plan_id = fx.confirm().await;
    fx.db
        .write(move |c| devices::close_plan(c, plan_id, PlanEnd::Abandoned, None, now_epoch()))
        .await
        .unwrap();
    let id = fx
        .jobs
        .enqueue(sync_job(fx.device.id, plan_id))
        .await
        .unwrap()
        .id();
    let job = fx.wait(id, JobState::Done).await;
    assert!(job.note.unwrap().contains("既に終わっている"));
    assert!(!device_file(&fx.device_root).exists());
}

#[tokio::test]
async fn uuid_mismatch_fails_without_retry() {
    let fx = Fx::new().await;
    fx.connect();
    fx.initialize_as("0123456789abcdef0123456789abcdef").await;
    fx.start();
    let id = fx.jobs.enqueue(scan_job(fx.device.id)).await.unwrap().id();
    let job = fx.wait(id, JobState::Failed).await;
    assert_eq!(job.attempts, 1);
    let err = job.last_error.unwrap();
    assert!(err.contains("別の端末"), "{err}");
}

#[tokio::test]
async fn scan_recovers_and_replaces_the_cache_and_measures_free() {
    let fx = Fx::new().await;
    fx.connect();
    fx.initialize().await;
    fx.start();
    fx.sync_once().await;
    let dev = fx.device.id;
    fx.db
        .write(move |c| devices::replace_items(c, dev, &[], now_epoch()))
        .await
        .unwrap();
    assert!(fx.items().await.is_empty());
    fx.rt.set_free(dev, 0);
    let id = fx.jobs.enqueue(scan_job(dev)).await.unwrap().id();
    let job = fx.wait(id, JobState::Done).await;
    assert_eq!(job.note.as_deref(), Some("管理外のファイル 0 件"));
    let items = fx.items().await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].track_id, 1);
    assert!(fx.rt.free(dev).is_some_and(|b| b > 0));
}

#[tokio::test]
async fn scan_of_a_disconnected_device_finishes_with_a_note() {
    let fx = Fx::new().await;
    fx.initialize().await;
    fx.start();
    let id = fx.jobs.enqueue(scan_job(fx.device.id)).await.unwrap().id();
    let job = fx.wait(id, JobState::Done).await;
    assert_eq!(job.note.as_deref(), Some("未接続"));
}

#[tokio::test]
async fn verify_marks_a_corrupted_copy_stale() {
    let fx = Fx::new().await;
    fx.connect();
    fx.initialize().await;
    fx.start();
    fx.sync_once().await;
    std::fs::write(device_file(&fx.device_root), b"BBBB").unwrap();
    let id = fx
        .jobs
        .enqueue(verify_job(fx.device.id))
        .await
        .unwrap()
        .id();
    let job = fx.wait(id, JobState::Done).await;
    let note = job.note.unwrap();
    assert!(note.contains("不一致 1"), "{note}");
    let items = fx.items().await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].token, STALE_TOKEN);
}

#[tokio::test]
async fn changed_source_forgets_the_hash() {
    let fx = Fx::new().await;
    fx.connect();
    fx.initialize().await;
    fx.start();
    let plan_id = fx.confirm().await;
    // 同じサイズで書き換える（inode は同じでも mtime / ctime が変わる）
    tokio::time::sleep(Duration::from_millis(20)).await;
    std::fs::write(fx.library.join(TRACK), b"CCCC").unwrap();
    let id = fx
        .jobs
        .enqueue(sync_job(fx.device.id, plan_id))
        .await
        .unwrap()
        .id();
    let job = fx.wait(id, JobState::Done).await;
    let note = job.note.unwrap();
    assert!(note.contains("保留 1"), "{note}");
    let dev = fx.device.id;
    let (hash, errors) = fx
        .db
        .read(move |c| {
            Ok((
                devices::source_hash(c, 1, SourceKind::Master)?,
                devices::device_errors(c, dev)?,
            ))
        })
        .await
        .unwrap();
    assert!(hash.is_none(), "ハッシュを取り直させる");
    assert_eq!(
        errors,
        vec![("track".to_owned(), 1, SourceError::Changed.reason())]
    );
    assert!(!device_file(&fx.device_root).exists());
}

#[tokio::test]
async fn busy_device_lock_delays_the_job() {
    let fx = Fx::new().await;
    fx.connect();
    fx.initialize().await;
    fx.start();
    let guard = fx.rt.device_lock(fx.device.id).lock_owned().await;
    let id = fx.jobs.enqueue(scan_job(fx.device.id)).await.unwrap().id();
    fx.wait(id, JobState::Running).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        fx.jobs.get(id).await.unwrap().unwrap().state,
        JobState::Running
    );
    drop(guard);
    fx.wait(id, JobState::Done).await;
}

#[tokio::test]
async fn on_connected_enqueues_one_scan_for_a_registered_serial() {
    let fx = Fx::new().await;
    on_connected(&fx.db, &fx.jobs, "SER1".into()).await;
    assert_eq!(fx.scan_jobs().await, 1);
    on_connected(&fx.db, &fx.jobs, "SER1".into()).await;
    assert_eq!(fx.scan_jobs().await, 1, "未完了の間は dedup");
}

#[tokio::test]
async fn on_connected_ignores_an_unknown_serial() {
    let fx = Fx::new().await;
    on_connected(&fx.db, &fx.jobs, "OTHER".into()).await;
    assert_eq!(fx.scan_jobs().await, 0);
}

#[tokio::test]
async fn sync_of_a_plan_abandoned_while_waiting_for_the_lock_is_a_no_op() {
    let fx = Fx::new().await;
    fx.connect();
    fx.initialize().await;
    fx.start();
    let plan_id = fx.confirm().await;
    let guard = fx.rt.device_lock(fx.device.id).lock_owned().await;
    let id = fx
        .jobs
        .enqueue(sync_job(fx.device.id, plan_id))
        .await
        .unwrap()
        .id();
    fx.wait(id, JobState::Running).await;
    fx.db
        .write(move |c| devices::close_plan(c, plan_id, PlanEnd::Abandoned, None, now_epoch()))
        .await
        .unwrap();
    drop(guard);
    let job = fx.wait(id, JobState::Done).await;
    let note = job.note.unwrap();
    assert!(note.contains("既に終わっている"), "{note}");
    assert!(!device_file(&fx.device_root).exists());
    let dev = fx.device.id;
    let synced = fx
        .db
        .read(move |c| Ok(devices::get(c, dev)?.unwrap().last_synced_at))
        .await
        .unwrap();
    assert_eq!(synced, None);
}

#[tokio::test]
async fn sync_of_a_deleted_device_says_so() {
    let fx = Fx::new().await;
    fx.start();
    let plan_id = fx.confirm().await;
    let dev = fx.device.id;
    fx.db.write(move |c| devices::delete(c, dev)).await.unwrap();
    let id = fx.jobs.enqueue(sync_job(dev, plan_id)).await.unwrap().id();
    let job = fx.wait(id, JobState::Done).await;
    assert_eq!(job.note.as_deref(), Some("端末が削除された"));
}

#[tokio::test]
async fn verify_waits_without_spending_attempts_until_connected() {
    let fx = Fx::new().await;
    fx.initialize().await;
    fx.start();
    let before = now_epoch();
    let id = fx
        .jobs
        .enqueue(verify_job(fx.device.id))
        .await
        .unwrap()
        .id();
    let job = fx
        .wait_until(id, "待ちへ戻る", |j| {
            j.state == JobState::Queued && j.run_after.is_some_and(|t| t >= before + 290)
        })
        .await;
    assert_eq!(job.attempts, 0);
}

#[tokio::test]
async fn job_waiting_for_the_device_lock_ends_quietly_if_the_device_was_deleted() {
    let fx = Fx::new().await;
    fx.connect();
    fx.initialize().await;
    fx.start();
    // 端末の削除（API）がロックを持っている間に、走査が行を読んでロックを待つ
    let guard = fx.rt.device_lock(fx.device.id).lock_owned().await;
    let scan = fx.jobs.enqueue(scan_job(fx.device.id)).await.unwrap().id();
    fx.wait(scan, JobState::Running).await;
    fx.wait_device_mutex(scan).await;
    let dev = fx.device.id;
    fx.db
        .write(move |c| {
            devices::delete(c, dev)?;
            Ok(())
        })
        .await
        .unwrap();
    drop(guard);
    // ロックを取った後に行を読み直し、消えていれば端末に触らずに終わる
    let job = fx.wait(scan, JobState::Done).await;
    assert_eq!(job.note.as_deref(), Some("端末が削除された"));
}

#[tokio::test]
async fn busy_job_mutex_requeues_after_a_while() {
    let fx = Fx::new().await;
    fx.connect();
    fx.initialize().await;
    fx.start();
    // 本物の同期のハンドラに job_mutexes の `device:<id>` を握らせる（プロセス内のロックで待たせる）
    let guard = fx.rt.device_lock(fx.device.id).lock_owned().await;
    let plan_id = fx.confirm().await;
    let sync = fx
        .jobs
        .enqueue(sync_job(fx.device.id, plan_id))
        .await
        .unwrap()
        .id();
    fx.wait(sync, JobState::Running).await;
    // 同期が job_mutexes の行を持つまで待つ（ロックは lock_mutex_named → device_lock の順に取る）
    fx.wait_device_mutex(sync).await;
    let before = now_epoch();
    let scan = fx.jobs.enqueue(scan_job(fx.device.id)).await.unwrap().id();
    // 1 秒ごとの空回りではなく、しばらく置いてから再び対象にする
    let job = fx
        .wait_until(scan, "戻された queued", |j| {
            j.state == JobState::Queued && j.run_after.is_some_and(|t| t > before)
        })
        .await;
    assert!(
        job.run_after.is_some_and(|t| t >= before + 14),
        "run_after {:?} before {before}",
        job.run_after
    );
    assert_eq!(job.attempts, 0);
    drop(guard);
    fx.wait(sync, JobState::Done).await;
}

#[tokio::test]
async fn invalid_stored_volume_fails_without_retry() {
    let fx = Fx::new().await;
    fx.connect();
    fx.initialize().await;
    let dev = fx.device.id;
    fx.db
        .write(move |c| {
            c.execute(
                "UPDATE devices SET adb_volume = 'bad/volume' WHERE id = ?1",
                [dev],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    fx.start();
    let id = fx.jobs.enqueue(verify_job(dev)).await.unwrap().id();
    let job = fx.wait(id, JobState::Failed).await;
    assert_eq!(job.attempts, 1, "{:?}", job.last_error);
    let err = job.last_error.unwrap();
    assert!(err.contains("保存先が不正"), "{err}");
}

//! GC のアップロード区分（設計 docs/development/2026-11-25-upload-design.md §5.8）。
//! 捨てた・壊れたステージングの削除、`garbage` の古い展開世代の削除、放置の破棄扱い、
//! 計画の後に根拠が変わったら何もしないことを固定する

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio_util::sync::CancellationToken;

use spindle::config::UploadConfig;
use spindle::db::{now_epoch, Db};
use spindle::fsroot::RootDir;
use spindle::gc::{
    execute_all, execute_uploads, plan, GcRoots, GcUploads, Plan, Preview, UploadGcReason,
};
use spindle::import::upload::{
    create_upload, mark_broken_discarded, read_manifest, with_manifest, FileClass, Garbage,
    Manifest, Origin, UploadId, UploadLocks, ZipState,
};
use spindle::media::artwork::ArtworkStore;

const DAY: i64 = 86_400;
const RETENTION: i64 = 7 * DAY;

struct Env {
    dir: tempfile::TempDir,
    db: Arc<Db>,
    roots: Arc<GcRoots>,
    inbox: Arc<RootDir>,
    locks: UploadLocks,
    cfg: UploadConfig,
    now: i64,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in [
            "Library", "Archive", "Derived", "Inbox", "thumbs", "outside",
        ] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let inbox = Arc::new(RootDir::open(&dir.path().join("Inbox")).unwrap());
        let locks = UploadLocks::default();
        let cfg = UploadConfig {
            stale_days: 30,
            ..UploadConfig::default()
        };
        let roots = Arc::new(GcRoots {
            library: Arc::new(RootDir::open(&dir.path().join("Library")).unwrap()),
            archive: Arc::new(RootDir::open(&dir.path().join("Archive")).unwrap()),
            derived: Arc::new(RootDir::open(&dir.path().join("Derived")).unwrap()),
            artwork: Arc::new(ArtworkStore::new(dir.path().join("thumbs"))),
            inbox: Some(Arc::clone(&inbox)),
            uploads: Some(GcUploads {
                locks: locks.clone(),
                cfg: cfg.clone(),
            }),
        });
        Self {
            dir,
            db,
            roots,
            inbox,
            locks,
            cfg,
            now: now_epoch(),
        }
    }

    /// `created` 秒（epoch）に作ったアップロード
    fn upload(&self, created: i64) -> UploadId {
        let m = create_upload(&self.inbox, created).unwrap();
        UploadId::parse(&m.id).unwrap()
    }

    fn update(&self, id: &UploadId, f: impl FnOnce(&mut Manifest)) {
        with_manifest(&self.inbox, &self.locks, id, |m| {
            f(m);
            Ok(())
        })
        .unwrap();
    }

    fn manifest(&self, id: &UploadId) -> Manifest {
        read_manifest(&self.inbox, id).unwrap()
    }

    fn staging(&self, id: &UploadId) -> PathBuf {
        self.dir
            .path()
            .join("Inbox")
            .join(id.staging_dir().as_str())
    }

    async fn plan(&self) -> Plan {
        plan(&self.db, &self.roots, RETENTION, self.now)
            .await
            .unwrap()
    }

    async fn execute(&self, plan: &Plan) -> spindle::gc::Counts {
        execute_uploads(
            &self.db,
            &self.roots,
            &self.locks,
            &self.cfg,
            plan,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
    }
}

/// ファイルを置く（親も作る）
fn put(p: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

/// mtime を `age_secs` 前にする（ディレクトリにも使う）
fn set_age(p: &Path, age_secs: i64) {
    let t = SystemTime::now() - Duration::from_secs(age_secs.max(0) as u64);
    std::fs::File::open(p).unwrap().set_modified(t).unwrap();
}

fn reasons(plan: &Plan) -> Vec<(String, UploadGcReason)> {
    plan.uploads
        .iter()
        .map(|u| (u.dir.clone(), u.reason.clone()))
        .collect()
}

#[tokio::test]
async fn gc_removes_discarded_after_retention() {
    let env = Env::new();
    let id = env.upload(env.now - 20 * DAY);
    let staging = env.staging(&id);
    put(&staging.join("files/o1"), b"12345");
    put(&staging.join("x/o2/g1/a.flac"), b"abc");
    put(&staging.join("files/.spindle-tmp-zz"), b"tmp");
    // ステージングの外を指す symlink は辿らない（リンクだけ消し、指す先は残す）
    let outside = env.dir.path().join("outside/keep.txt");
    put(&outside, b"keep");
    std::os::unix::fs::symlink(&outside, staging.join("x/o2/g1/link")).unwrap();
    std::os::unix::fs::symlink(env.dir.path().join("outside"), staging.join("x/dirlink")).unwrap();
    env.update(&id, |m| m.discarded_at = Some(env.now - 8 * DAY));

    let plan = env.plan().await;
    assert_eq!(
        reasons(&plan),
        vec![(
            id.staging_dir().as_str().to_owned(),
            UploadGcReason::Discarded
        )]
    );
    let manifest_len = std::fs::metadata(staging.join("upload.json"))
        .unwrap()
        .len();
    assert_eq!(plan.uploads[0].bytes, 5 + 3 + 3 + manifest_len);

    let s = execute_all(&env.db, &env.roots, &plan, &CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(s.uploads.planned, 1);
    assert_eq!(s.uploads.deleted, 1, "{:?}", s.uploads);
    assert_eq!(s.uploads.bytes, 5 + 3 + 3 + manifest_len);
    assert!(!staging.exists(), "ステージングごと消える");
    assert!(outside.exists(), "symlink の指す先は消さない");
    assert!(env.dir.path().join("outside").is_dir());
    assert!(s.total_deleted() >= 1);
}

#[tokio::test]
async fn gc_keeps_recent_discarded() {
    let env = Env::new();
    let id = env.upload(env.now - 20 * DAY);
    put(&env.staging(&id).join("files/o1"), b"12345");
    env.update(&id, |m| m.discarded_at = Some(env.now - DAY));

    let plan = env.plan().await;
    assert!(plan.uploads.is_empty(), "{:?}", plan.uploads);
    let c = env.execute(&plan).await;
    assert_eq!(c.deleted, 0);
    assert!(env.staging(&id).join("files/o1").exists());
}

#[tokio::test]
async fn gc_marks_stale_upload_discarded() {
    let env = Env::new();
    let stale = env.upload(env.now - 31 * DAY);
    let fresh = env.upload(env.now - 29 * DAY);

    let plan = env.plan().await;
    assert_eq!(
        reasons(&plan),
        vec![(
            stale.staging_dir().as_str().to_owned(),
            UploadGcReason::MarkStale
        )]
    );
    assert_eq!(plan.uploads[0].bytes, 0, "印を付けるだけで消さない");
    let c = env.execute(&plan).await;
    assert_eq!(c.marked, 1, "{c:?}");
    assert_eq!(c.deleted, 0);
    let at = env.manifest(&stale).discarded_at.expect("破棄扱いになる");
    assert!(at >= env.now && at <= now_epoch(), "{at}");
    // updated_at は進めない（利用者の操作ではない）
    assert_eq!(env.manifest(&stale).updated_at, env.now - 31 * DAY);
    assert!(env.staging(&stale).exists(), "消すのは後日の (a)");
    assert_eq!(env.manifest(&fresh).discarded_at, None);
}

#[tokio::test]
async fn gc_removes_old_broken_staging() {
    let env = Env::new();
    let inbox = env.dir.path().join("Inbox");
    // マニフェストの無いステージング（古い / 新しい）
    let old = inbox.join(".spindle-upload-00000000000000aa");
    put(&old.join("files/o1"), b"1234");
    set_age(&old, 8 * DAY);
    let young = inbox.join(".spindle-upload-00000000000000bb");
    put(&young.join("files/o1"), b"1234");
    // 読めないマニフェストに捨てる印を付けたもの（印を付けた後に古くなった）
    let marked = inbox.join(".spindle-upload-00000000000000cc");
    put(&marked.join("upload.json"), b"{broken");
    mark_broken_discarded(
        &env.inbox,
        &env.locks,
        &UploadId::parse("00000000000000cc").unwrap(),
    )
    .unwrap();
    set_age(&marked, 8 * DAY);
    // 前置きの違うものは触らない
    let other = inbox.join(".spindle-upload-xyz");
    put(&other.join("a"), b"1");
    set_age(&other, 8 * DAY);

    let plan = env.plan().await;
    let mut got = reasons(&plan);
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        got,
        vec![
            (
                ".spindle-upload-00000000000000aa".to_owned(),
                UploadGcReason::Broken
            ),
            (
                ".spindle-upload-00000000000000cc".to_owned(),
                UploadGcReason::Broken
            ),
        ]
    );
    let c = env.execute(&plan).await;
    assert_eq!(c.deleted, 2, "{c:?}");
    assert!(!old.exists());
    assert!(!marked.exists());
    assert!(young.join("files/o1").exists());
    assert!(other.join("a").exists());
}

#[tokio::test]
async fn gc_removes_old_garbage_generation_of_live_upload() {
    let env = Env::new();
    let id = env.upload(env.now - 3 * DAY);
    let staging = env.staging(&id);
    // g1: 古い世代（garbage・古い）、g2: 成功した世代、g3: 展開中（garbage・古いが触らない）、
    // g4: garbage だが新しい、g5: garbage に載っているが実体が無い（項だけ外す）
    put(&staging.join("x/o1/g1/sub/a.flac"), b"aaaa");
    put(&staging.join("x/o1/g1/b.flac"), b"bb");
    set_age(&staging.join("x/o1/g1/sub"), 9 * DAY);
    set_age(&staging.join("x/o1/g1"), 9 * DAY);
    put(&staging.join("x/o1/g2/a.flac"), b"live");
    put(&staging.join("x/o1/g3/a.flac"), b"wip");
    set_age(&staging.join("x/o1/g3"), 9 * DAY);
    put(&staging.join("x/o1/g4/a.flac"), b"new");
    env.update(&id, |m| {
        m.origins.push(Origin {
            id: "o1".into(),
            client_path: "a.zip".into(),
            class: FileClass::Zip,
            total: 10,
            received: 10,
            complete: true,
            removed: false,
            damaged: false,
            zip: Some(ZipState {
                generation: 2,
                next_generation: 4,
                unpacking: Some(3),
                ..ZipState::default()
            }),
        });
        m.next_origin = 1;
        for g in ["g1", "g3", "g4", "g5"] {
            m.garbage.push(Garbage {
                path: format!("x/o1/{g}"),
                bytes: 6,
                added_at: env.now - 9 * DAY,
            });
        }
    });

    let plan = env.plan().await;
    let dir = id.staging_dir().as_str().to_owned();
    assert_eq!(
        reasons(&plan),
        vec![
            (
                dir.clone(),
                UploadGcReason::Garbage {
                    path: "x/o1/g1".into()
                }
            ),
            (
                dir.clone(),
                UploadGcReason::Garbage {
                    path: "x/o1/g5".into()
                }
            ),
        ]
    );
    assert_eq!(plan.uploads[0].bytes, 6);
    let c = env.execute(&plan).await;
    assert_eq!(c.deleted, 2, "{c:?}");
    assert_eq!(c.bytes, 6);
    assert!(!staging.join("x/o1/g1").exists());
    assert!(staging.join("x/o1/g2/a.flac").exists());
    assert!(staging.join("x/o1/g3/a.flac").exists());
    assert!(staging.join("x/o1/g4/a.flac").exists());
    let m = env.manifest(&id);
    let left: Vec<&str> = m.garbage.iter().map(|g| g.path.as_str()).collect();
    assert_eq!(left, vec!["x/o1/g3", "x/o1/g4"]);
    assert_eq!(m.discarded_at, None);
    assert_eq!(m.updated_at, env.now - 3 * DAY);
}

#[tokio::test]
async fn gc_skips_when_basis_changed() {
    let env = Env::new();
    let id = env.upload(env.now - 31 * DAY);
    let staging = env.staging(&id);
    put(&staging.join("x/o1/g1/a.flac"), b"old");
    set_age(&staging.join("x/o1/g1"), 9 * DAY);
    env.update(&id, |m| {
        m.garbage.push(Garbage {
            path: "x/o1/g1".into(),
            bytes: 3,
            added_at: env.now - 9 * DAY,
        });
    });
    // 捨てたもの（計画の後にマニフェストが書き直される）
    let gone = env.upload(env.now - 20 * DAY);
    env.update(&gone, |m| m.discarded_at = Some(env.now - 8 * DAY));
    set_age(&env.staging(&gone), DAY);
    // 壊れたもの（計画の後に捨てる印が付く）
    let broken = env
        .dir
        .path()
        .join("Inbox/.spindle-upload-00000000000000aa");
    put(&broken.join("files/o1"), b"1");
    set_age(&broken, 8 * DAY);

    let plan = env.plan().await;
    assert_eq!(plan.uploads.len(), 4, "{:?}", plan.uploads);

    // 計画の後に PATCH 相当（updated_at が進む）・マニフェストの書き直し・捨てる印
    env.update(&id, |m| m.updated_at = now_epoch() + 1);
    env.update(&gone, |m| m.publish_error = Some("x".into()));
    mark_broken_discarded(
        &env.inbox,
        &env.locks,
        &UploadId::parse("00000000000000aa").unwrap(),
    )
    .unwrap();

    let c = env.execute(&plan).await;
    assert_eq!(c.deleted, 0, "{c:?}");
    assert_eq!(c.marked, 0);
    assert_eq!(c.skipped, 4);
    let m = env.manifest(&id);
    assert_eq!(m.discarded_at, None, "印も付けない");
    assert_eq!(m.garbage.len(), 1);
    assert!(staging.join("x/o1/g1/a.flac").exists());
    assert!(env.staging(&gone).exists());
    assert!(broken.join("files/o1").exists());
}

#[tokio::test]
async fn gc_keeps_publish_requested() {
    let env = Env::new();
    let id = env.upload(env.now - 40 * DAY);
    env.update(&id, |m| m.publish_requested_at = Some(env.now - 35 * DAY));

    let plan = env.plan().await;
    assert!(plan.uploads.is_empty(), "{:?}", plan.uploads);
    env.execute(&plan).await;
    assert_eq!(env.manifest(&id).discarded_at, None);
    assert!(env.staging(&id).exists());
}

#[tokio::test]
async fn gc_preview_includes_uploads() {
    let env = Env::new();
    let id = env.upload(env.now - 20 * DAY);
    put(&env.staging(&id).join("files/o1"), b"12345");
    env.update(&id, |m| m.discarded_at = Some(env.now - 8 * DAY));
    let stale = env.upload(env.now - 31 * DAY);

    let plan = env.plan().await;
    let v = serde_json::to_value(Preview::of(&plan)).unwrap();
    assert_eq!(v["uploads"]["count"], 2, "{v}");
    let manifest_len = std::fs::metadata(env.staging(&id).join("upload.json"))
        .unwrap()
        .len();
    assert_eq!(v["uploads"]["bytes"], 5 + manifest_len);
    let sample: Vec<String> = v["uploads"]["sample"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_owned())
        .collect();
    assert!(
        sample.iter().any(|s| s == id.staging_dir().as_str()),
        "{sample:?}"
    );
    assert!(
        sample
            .iter()
            .any(|s| s.starts_with(stale.staging_dir().as_str())),
        "{sample:?}"
    );
    // dry-run は何も変えない
    assert!(env.staging(&id).join("files/o1").exists());
    assert_eq!(env.manifest(&stale).discarded_at, None);
}

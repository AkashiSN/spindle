//! Derived の追随ジョブが投入される契機（docs/TASKS.md P1-10、D-51）: scan 完了、tagwrite applied、
//! rename applied、RG 解析の保存。transcode ハンドラを登録せずに queued の transcode を数える。
//! 最後に scan → transcode の自動連鎖で `delivery` ビューが版に追随することを通す。
//! ffmpeg / opusenc が無ければ skip

#![cfg(target_os = "linux")]

mod common;

use std::fs::File;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use lofty::tag::Accessor as _;
use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use spindle::db::history::{self, BatchState};
use spindle::db::{derived, Db};
use spindle::domain::tags::read_audio_file;
use spindle::edit::{Editor, NewTagOp, RenameTarget, TagChange};
use spindle::fsroot::RootDir;
use spindle::import::scanner::Scanner;
use spindle::jobs::handlers::rename::RenameHandler;
use spindle::jobs::handlers::rg::{new_album_job, RgHandler};
use spindle::jobs::handlers::scan::{enqueue_scan, ScanHandler};
use spindle::jobs::handlers::tagwrite::TagwriteHandler;
use spindle::jobs::handlers::transcode::TranscodeHandler;
use spindle::jobs::{EnqueueResult, JobState, JobType, Jobs, Registry};
use spindle::media::artwork::ArtworkStore;
use spindle::media::decode::Decoder;
use spindle::media::encode::OpusEncoder;

const REFERENCE: f64 = -18.0;

fn opusenc_bin() -> Option<PathBuf> {
    let p = Command::new("opusenc").arg("--version").output().ok()?;
    p.status.success().then(|| PathBuf::from("opusenc"))
}

macro_rules! require_tools {
    () => {
        if common::ffmpeg().is_none() || opusenc_bin().is_none() {
            eprintln!("ffmpeg / opusenc が無いので skip");
            return;
        }
    };
}

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    jobs: Arc<Jobs>,
    scanner: Arc<Scanner>,
    editor: Arc<Editor>,
    library: Arc<RootDir>,
    derived: Arc<RootDir>,
    store: Arc<ArtworkStore>,
    shutdown: CancellationToken,
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["Library", "Derived", "thumbs", "tmp"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let library = Arc::new(RootDir::open(&dir.path().join("Library")).unwrap());
        let derived = Arc::new(RootDir::open(&dir.path().join("Derived")).unwrap());
        let store = Arc::new(ArtworkStore::new(dir.path().join("thumbs")));
        let jobs = Jobs::new(db.clone());
        let scanner =
            Arc::new(Scanner::new(db.clone(), library.clone(), 2).with_artwork(store.clone()));
        let editor = Arc::new(
            Editor::new(db, library.clone(), jobs.clone()).with_replaygain_reference(REFERENCE),
        );
        Self {
            dir,
            db_path,
            jobs,
            scanner,
            editor,
            library,
            derived,
            store,
            shutdown: CancellationToken::new(),
        }
    }

    /// scan / tagwrite / rename / rg を登録する。`with_transcode` なら transcode も
    fn start(&self, with_transcode: bool) {
        let mut reg = Registry::new();
        reg.register(
            JobType::Scan,
            Arc::new(ScanHandler::new(self.scanner.clone(), 0)),
        );
        reg.register(
            JobType::Tagwrite,
            Arc::new(TagwriteHandler::new(self.editor.clone())),
        );
        reg.register(
            JobType::Rename,
            Arc::new(RenameHandler::new(self.editor.clone())),
        );
        reg.register(
            JobType::Rg,
            Arc::new(RgHandler::new(
                self.library.clone(),
                Decoder::new("ffmpeg"),
                REFERENCE,
            )),
        );
        if with_transcode {
            reg.register(
                JobType::Transcode,
                Arc::new(TranscodeHandler::new(
                    self.library.clone(),
                    self.derived.clone(),
                    OpusEncoder::new("ffmpeg", "opusenc", 128, self.dir.path().join("tmp")),
                    self.store.clone(),
                    "ffmpeg",
                    REFERENCE,
                )),
            );
        }
        self.jobs.start(reg, self.shutdown.clone());
    }

    fn lib(&self) -> PathBuf {
        self.dir.path().join("Library")
    }

    fn derived_dir(&self) -> PathBuf {
        self.dir.path().join("Derived")
    }

    fn add(&self, rel: &str, seed: u32, title: &str) -> PathBuf {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let ext = rel.rsplit_once('.').unwrap().1;
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        let made = common::make_audio(p.parent().unwrap(), &name, ext, seed).unwrap();
        common::set_basic_tags(&made, title, "Artist", "Album", "AlbumArtist", 1, 1);
        made
    }

    fn add_multichannel(&self, rel: &str) {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let wav = p.with_extension("src.wav");
        common::write_wav(&wav, &common::pcm_samples(9), 16);
        common::encode(&wav, &p, &["-ac", "6", "-c:a", "flac"]).unwrap();
        std::fs::remove_file(&wav).unwrap();
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.db_path).unwrap()
    }

    fn track_id(&self, rel: &str) -> i64 {
        self.conn()
            .query_row("SELECT id FROM tracks WHERE rel_path = ?1", [rel], |r| {
                r.get(0)
            })
            .unwrap()
    }

    fn versions(&self, id: i64) -> (i64, i64) {
        self.conn()
            .query_row(
                "SELECT audio_version, tag_version FROM tracks WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    /// Derived が揃っている状態を DB に直接作る（transcode を走らせずに追随だけを見る）
    fn fake_derived(&self, id: i64, rel_opus: &str) {
        let (av, tv) = self.versions(id);
        derived::upsert(
            &self.conn(),
            id,
            rel_opus,
            "opus",
            None,
            av,
            derived::TagState {
                src_tag_version: tv,
                src_artwork_id: None,
                src_rg_scanned_at: None,
            },
            0,
        )
        .unwrap();
    }

    /// queued の transcode（track_id, audio_version）を id 順で
    fn queued_transcodes(&self) -> Vec<(i64, i64)> {
        let c = self.conn();
        let mut st = c
            .prepare(
                "SELECT json_extract(payload, '$.track_id'), json_extract(payload, '$.audio_version')
                 FROM jobs WHERE type = 'transcode' AND state = 'queued' ORDER BY id",
            )
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn delivery(&self, id: i64) -> (String, i64) {
        self.conn()
            .query_row(
                "SELECT path, stale_tags FROM delivery WHERE track_id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    async fn scan_job(&self) -> JobState {
        let id = match enqueue_scan(&self.jobs, "incremental").await.unwrap() {
            EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
        };
        self.wait_job(id).await
    }

    async fn wait_job(&self, id: i64) -> JobState {
        for _ in 0..6000 {
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

    /// 未完了の transcode が無くなるまで待つ
    async fn wait_transcodes(&self) {
        for _ in 0..6000 {
            let n: i64 = self
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM jobs WHERE type = 'transcode' AND state IN ('queued', 'running')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            if n == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("transcode が終わらない");
    }

    async fn wait_batch(&self, id: i64) -> BatchState {
        for _ in 0..3000 {
            let st = history::get_batch(&self.conn(), id).unwrap().unwrap().state;
            if !matches!(st, BatchState::Prepared | BatchState::Applying) {
                return st;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("batch {id} が終端にならない");
    }
}

impl Drop for Lib {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[tokio::test]
async fn scan_job_enqueues_transcode_for_every_lossless_track_needing_it() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/01.flac", 1, "a");
    lib.add("A/02.opus", 2, "b");
    lib.add_multichannel("A/03.flac");
    lib.start(false);
    assert_eq!(lib.scan_job().await, JobState::Done);
    let a = lib.track_id("A/01.flac");
    assert_eq!(lib.queued_transcodes(), vec![(a, 1)]);
    // もう一度 scan → dedup で増えない
    assert_eq!(lib.scan_job().await, JobState::Done);
    assert_eq!(lib.queued_transcodes(), vec![(a, 1)]);
}

#[tokio::test]
async fn tagwrite_applied_enqueues_transcode() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/01.flac", 1, "a");
    lib.start(false);
    assert_eq!(lib.scan_job().await, JobState::Done);
    let a = lib.track_id("A/01.flac");
    // scan が投入した分を消し、Derived が揃っている状態にする
    lib.conn()
        .execute("DELETE FROM jobs WHERE type = 'transcode'", [])
        .unwrap();
    lib.fake_derived(a, "A/01.opus");
    let prepared = lib
        .editor
        .prepare_tags(
            None,
            vec![NewTagOp {
                track_id: a,
                changes: vec![TagChange {
                    key: "TITLE".into(),
                    values: Some(vec!["b".into()]),
                }],
            }],
        )
        .await
        .unwrap();
    assert_eq!(lib.wait_batch(prepared.batch_id).await, BatchState::Applied);
    assert_eq!(lib.versions(a), (1, 2));
    assert_eq!(lib.queued_transcodes(), vec![(a, 1)]);
}

#[tokio::test]
async fn rename_applied_enqueues_transcode() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/01.flac", 1, "a");
    lib.start(false);
    assert_eq!(lib.scan_job().await, JobState::Done);
    let a = lib.track_id("A/01.flac");
    lib.conn()
        .execute("DELETE FROM jobs WHERE type = 'transcode'", [])
        .unwrap();
    lib.fake_derived(a, "A/01.opus");
    let prepared = lib
        .editor
        .prepare_rename(
            None,
            vec![RenameTarget {
                track_id: a,
                new_rel_path: "B/01 a.flac".into(),
                expected: None,
                planned_conflict: None,
            }],
        )
        .await
        .unwrap();
    assert_eq!(lib.wait_batch(prepared.batch_id).await, BatchState::Applied);
    assert_eq!(lib.track_id("B/01 a.flac"), a);
    assert_eq!(lib.queued_transcodes(), vec![(a, 1)]);
}

#[tokio::test]
async fn rg_store_enqueues_transcode() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/01.flac", 1, "a");
    lib.start(false);
    assert_eq!(lib.scan_job().await, JobState::Done);
    let a = lib.track_id("A/01.flac");
    lib.conn()
        .execute("DELETE FROM jobs WHERE type = 'transcode'", [])
        .unwrap();
    lib.fake_derived(a, "A/01.opus");
    let album: i64 = lib
        .conn()
        .query_row("SELECT album_id FROM tracks WHERE id = ?1", [a], |r| {
            r.get(0)
        })
        .unwrap();
    let job = match lib.jobs.enqueue(new_album_job(album)).await.unwrap() {
        EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
    };
    assert_eq!(lib.wait_job(job).await, JobState::Done);
    // RG の解析は版に乗らないが、Derived のタグには要る。src_rg_scanned_at の不一致で投入される
    assert_eq!(lib.versions(a), (1, 1));
    assert_eq!(lib.queued_transcodes(), vec![(a, 1)]);
}

#[tokio::test]
async fn delivery_view_follows_versions_end_to_end() {
    require_tools!();
    let lib = Lib::new();
    let p = lib.add("A/01.flac", 1, "a");
    lib.add("A/02.opus", 2, "b");
    lib.start(true);
    // 初回: scan → transcode の連鎖で Derived が出来る
    assert_eq!(lib.scan_job().await, JobState::Done);
    lib.wait_transcodes().await;
    let a = lib.track_id("A/01.flac");
    let b = lib.track_id("A/02.opus");
    assert_eq!(lib.delivery(a), ("Derived/A/01.opus".into(), 0));
    assert_eq!(lib.delivery(b), ("Library/A/02.opus".into(), 0));
    assert!(lib.derived_dir().join("A/01.opus").is_file());
    assert!(!lib.derived_dir().join("A/02.opus").exists());

    // 外部でタグを変えて scan → stale_tags → transcode が追随
    common::retag(&p, |t| t.set_title("x".to_owned()));
    lib.scanner
        .run(
            spindle::import::scanner::ScanKind::Incremental,
            Arc::new(|_, _, _| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(lib.delivery(a), ("Derived/A/01.opus".into(), 1));
    assert_eq!(lib.scan_job().await, JobState::Done);
    lib.wait_transcodes().await;
    assert_eq!(lib.delivery(a), ("Derived/A/01.opus".into(), 0));
    let af = read_audio_file(
        File::open(lib.derived_dir().join("A/01.opus")).unwrap(),
        Some("opus"),
    )
    .unwrap();
    assert_eq!(af.tags.first("TITLE"), Some("x"));

    // 音声を差し替えて scan → 原本へフォールバック → 再エンコードで Derived に戻る
    std::fs::remove_file(&p).unwrap();
    lib.add("A/01.flac", 3, "x");
    lib.scanner
        .run(
            spindle::import::scanner::ScanKind::Incremental,
            Arc::new(|_, _, _| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(lib.versions(a).0, 2);
    assert_eq!(lib.delivery(a), ("Library/A/01.flac".into(), 0));
    assert_eq!(lib.scan_job().await, JobState::Done);
    lib.wait_transcodes().await;
    assert_eq!(lib.delivery(a), ("Derived/A/01.opus".into(), 0));
}

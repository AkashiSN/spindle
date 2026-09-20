//! `transcode` ジョブ（SPEC §7.6 / §8、docs/TASKS.md P1-10、D-51）。Library の可逆から Derived の
//! Opus を作り、音声版・タグ版・パス・アートワークの変化に追随する。`delivery` ビューの版一致
//! フォールバックもここで見る。ffmpeg / opusenc が無ければ skip

#![cfg(target_os = "linux")]

mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use lofty::tag::Accessor as _;
use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use spindle::db::{derived, Db};
use spindle::domain::derived::Variant;
use spindle::domain::tags::read_audio_file;
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, Scanner};
use spindle::jobs::handlers::transcode::{sweep_tmp, SweepReport, TestHook, TranscodeHandler};
use spindle::jobs::{EnqueueResult, JobState, JobType, Jobs, Registry};
use spindle::media::artwork::ArtworkStore;
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
    handler: Arc<TranscodeHandler>,
    library: Arc<RootDir>,
    derived_root: Arc<RootDir>,
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
        // 起動時の sync_variants と同じ（opus 系統 128k、on。エンコーダの設定と揃える）
        common::enable_opus_variant(&db_path, 128);
        let library = Arc::new(RootDir::open(&dir.path().join("Library")).unwrap());
        let derived = Arc::new(RootDir::open(&dir.path().join("Derived")).unwrap());
        let store = Arc::new(ArtworkStore::new(dir.path().join("thumbs")));
        let jobs = Jobs::new(db.clone());
        let scanner =
            Arc::new(Scanner::new(db.clone(), library.clone(), 2).with_artwork(store.clone()));
        let encoder = OpusEncoder::new("ffmpeg", "opusenc", 128, dir.path().join("tmp"));
        let handler = Arc::new(TranscodeHandler::new(
            library.clone(),
            derived.clone(),
            encoder,
            store.clone(),
            "ffmpeg",
            REFERENCE,
        ));
        Self {
            dir,
            db_path,
            jobs,
            scanner,
            handler,
            library,
            derived_root: derived,
            store,
            shutdown: CancellationToken::new(),
        }
    }

    fn start(&self) {
        let mut reg = Registry::new();
        reg.register(JobType::Transcode, self.handler.clone());
        self.jobs.start(reg, self.shutdown.clone());
    }

    /// テストフック付きのハンドラで起動する
    fn start_with_hook(&self, hook: TestHook) {
        let handler = TranscodeHandler::new(
            self.library.clone(),
            self.derived_root.clone(),
            OpusEncoder::new("ffmpeg", "opusenc", 128, self.dir.path().join("tmp")),
            self.store.clone(),
            "ffmpeg",
            REFERENCE,
        )
        .with_test_hook(hook);
        let mut reg = Registry::new();
        reg.register(JobType::Transcode, Arc::new(handler));
        self.jobs.start(reg, self.shutdown.clone());
    }

    fn lib(&self) -> PathBuf {
        self.dir.path().join("Library")
    }

    fn derived(&self) -> PathBuf {
        self.dir.path().join("Derived")
    }

    fn tmp_count(&self) -> usize {
        std::fs::read_dir(self.dir.path().join("tmp"))
            .unwrap()
            .count()
    }

    /// `rel`（flac / opus / m4a など `common::encode_args` の形式）に合成音源を置き、基本タグを付ける
    fn add(&self, rel: &str, seed: u32, title: &str) -> PathBuf {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let ext = rel.rsplit_once('.').unwrap().1;
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        let made = common::make_audio(p.parent().unwrap(), &name, ext, seed).unwrap();
        common::set_basic_tags(&made, title, "Artist", "Album", "AlbumArtist", 1, 1);
        made
    }

    /// 6ch の FLAC
    fn add_multichannel(&self, rel: &str) -> PathBuf {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let wav = p.with_extension("src.wav");
        common::write_wav(&wav, &common::pcm_samples(9), 16);
        common::encode(&wav, &p, &["-ac", "6", "-c:a", "flac"]).unwrap();
        std::fs::remove_file(&wav).unwrap();
        p
    }

    /// album ディレクトリに cover.png（ffmpeg で生成）を置く
    fn add_cover(&self, dir: &str, color: &str) {
        let p = self.lib().join(dir).join("cover.png");
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
            .arg(format!("color=c={color}:s=1200x1200"))
            .args(["-frames:v", "1"])
            .arg(&p)
            .status()
            .unwrap();
        assert!(st.success());
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

    /// opus 系統の設定を写し直す（aac は節省略の既定 = off のまま）
    fn sync_opus(&self, enabled: bool, bitrate: u32, now: i64) {
        derived::sync_variants(
            &self.conn(),
            &spindle::config::DerivedConfig {
                opus: spindle::config::OpusVariantConfig { enabled, bitrate },
                aac: Default::default(),
            },
            now,
        )
        .unwrap();
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

    fn set_rg(&self, id: i64) {
        self.conn()
            .execute(
                "UPDATE tracks SET rg_track_gain = -6.0, rg_track_peak = 0.5, rg_album_gain = -4.0,
                        rg_album_peak = 0.7, rg_scanned_at = 1 WHERE id = ?1",
                [id],
            )
            .unwrap();
    }

    fn derived_row(&self, id: i64) -> Option<(String, i64, i64, Option<i64>)> {
        self.conn()
            .query_row(
                "SELECT rel_path, src_audio_version, src_tag_version, src_artwork_id
                 FROM derived_files WHERE track_id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok()
    }

    fn delivery(&self, id: i64) -> (String, String, i64) {
        self.conn()
            .query_row(
                "SELECT path, codec, stale_tags FROM delivery WHERE track_id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }

    fn thumb_exists(&self, artwork_id: i64, size: u32) -> bool {
        let hash: Vec<u8> = self
            .conn()
            .query_row(
                "SELECT sha256 FROM artwork WHERE id = ?1",
                [artwork_id],
                |r| r.get(0),
            )
            .unwrap();
        self.dir
            .path()
            .join("thumbs")
            .join(ArtworkStore::hex(&hash))
            .join(format!("{size}.webp"))
            .is_file()
    }

    async fn enqueue(&self, id: i64, av: i64, tv: i64) -> i64 {
        match self
            .jobs
            .enqueue(derived::new_job(id, Variant::Opus, av, tv))
            .await
            .unwrap()
        {
            EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
        }
    }

    /// 現在の版で、再試行なしで投入する（失敗を待つテスト用）
    async fn enqueue_once(&self, id: i64) -> i64 {
        let (av, tv) = self.versions(id);
        match self
            .jobs
            .enqueue(derived::new_job(id, Variant::Opus, av, tv).max_attempts(1))
            .await
            .unwrap()
        {
            EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
        }
    }

    /// 現在の版で投入して終端まで待つ
    async fn run(&self, id: i64) -> JobState {
        let (av, tv) = self.versions(id);
        let job = self.enqueue(id, av, tv).await;
        self.wait_job(job).await
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

    fn last_error(&self, id: i64) -> Option<String> {
        self.conn()
            .query_row("SELECT last_error FROM jobs WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
    }
}

impl Drop for Lib {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn sha256(p: &Path) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(std::fs::read(p).unwrap()).into()
}

/// ffmpeg でデコードした PCM の MD5（タグを除いた音声の同一性）
fn audio_md5(p: &Path) -> String {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(p)
        .args(["-map", "0:a:0", "-f", "md5", "-"])
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

fn picture_of(p: &Path) -> Option<String> {
    let af = read_audio_file(File::open(p).unwrap(), Some("opus")).unwrap();
    af.tags.first("PICTURE").map(str::to_owned)
}

fn has_tmp(dir: &Path) -> bool {
    std::fs::read_dir(dir).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".spindle-tmp-")
    })
}

#[tokio::test]
async fn first_run_encodes_with_tags_rg_and_cover() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/B/01.flac", 1, "曲");
    lib.add_cover("A/B", "red");
    lib.scan().await;
    lib.start();
    let id = lib.track_id("A/B/01.flac");
    lib.set_rg(id);
    assert_eq!(lib.run(id).await, JobState::Done);
    let (rel, av, tv, art) = lib.derived_row(id).unwrap();
    assert_eq!(rel, "opus/A/B/01.opus");
    assert_eq!((av, tv), (1, 1));
    let art = art.expect("album のアートワークを埋めた");
    let opus = lib.derived().join("opus/A/B/01.opus");
    let af = read_audio_file(File::open(&opus).unwrap(), Some("opus")).unwrap();
    assert_eq!(af.codec.as_str(), "opus");
    assert_eq!(af.tags.first("TITLE"), Some("曲"));
    assert_eq!(af.tags.first("ALBUM"), Some("Album"));
    assert_eq!(af.tags.first("R128_TRACK_GAIN"), Some("-2816"));
    assert_eq!(af.tags.first("R128_ALBUM_GAIN"), Some("-2304"));
    assert!(af.tags.first("REPLAYGAIN_TRACK_GAIN").is_none());
    assert_eq!(
        af.tags
            .first("PICTURE")
            .map(|s| s.starts_with("image/webp:")),
        Some(true)
    );
    assert_eq!(
        lib.delivery(id),
        ("Derived/opus/A/B/01.opus".into(), "opus".into(), 0)
    );
    // 768 のサムネイルをハンドラ自身が作った（thumbnail ジョブは登録していない）
    assert!(lib.thumb_exists(art, 768));
    // tmp が残っていない
    assert_eq!(lib.tmp_count(), 0);
    assert!(!has_tmp(&lib.derived().join("opus/A/B")));
}

#[tokio::test]
async fn without_rg_and_cover_tags_only_transfer() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/01.flac", 1, "a");
    lib.scan().await;
    lib.start();
    let id = lib.track_id("A/01.flac");
    assert_eq!(lib.run(id).await, JobState::Done);
    assert_eq!(lib.derived_row(id).unwrap().3, None);
    let af = read_audio_file(
        File::open(lib.derived().join("opus/A/01.opus")).unwrap(),
        Some("opus"),
    )
    .unwrap();
    assert_eq!(af.tags.first("TITLE"), Some("a"));
    assert!(af.tags.first("R128_TRACK_GAIN").is_none());
    assert!(af.tags.first("PICTURE").is_none());
}

#[tokio::test]
async fn up_to_date_and_ineligible_are_noops() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/01.flac", 1, "a");
    lib.add("A/02.opus", 2, "b");
    lib.add_multichannel("A/03.flac");
    lib.scan().await;
    lib.start();
    let a = lib.track_id("A/01.flac");
    assert_eq!(lib.run(a).await, JobState::Done);
    let before = sha256(&lib.derived().join("opus/A/01.opus"));
    // 二度目は何もしない（ファイルは同一）
    assert_eq!(lib.run(a).await, JobState::Done);
    assert_eq!(sha256(&lib.derived().join("opus/A/01.opus")), before);
    for rel in ["A/02.opus", "A/03.flac"] {
        let id = lib.track_id(rel);
        assert_eq!(lib.run(id).await, JobState::Done);
        assert!(lib.derived_row(id).is_none(), "{rel}");
    }
    assert!(!lib.derived().join("opus/A/02.opus").exists());
    assert!(!lib.derived().join("opus/A/03.opus").exists());
    // 非可逆の delivery は原本
    let b = lib.track_id("A/02.opus");
    assert_eq!(lib.delivery(b).0, "Library/A/02.opus");
}

// ---------------------------------------------------------------- 系統と設定の世代（P4-7、D-75）

/// 0018 が移した旧ルート直下の行（audio_profile が 128k）は、設定が 256k なら作り直され opus/ 配下に
/// 置かれて旧ファイルが消える。設定が 128k のままなら profile 一致でパスの差分だけなので Move
#[tokio::test]
async fn legacy_root_row_is_reencoded_or_moved_under_the_variant_dir() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/01.flac", 1, "a");
    lib.add("A/02.flac", 2, "b");
    lib.scan().await;
    lib.start();
    let a = lib.track_id("A/01.flac");
    let b = lib.track_id("A/02.flac");
    assert_eq!(lib.run(a).await, JobState::Done);
    assert_eq!(lib.run(b).await, JobState::Done);
    let a_sha = sha256(&lib.derived().join("opus/A/01.opus"));
    let b_sha = sha256(&lib.derived().join("opus/A/02.opus"));
    // 0018 直後の状態を作る: ファイルをルート直下へ戻し、行もそこを指す（a は 128k の世代 = 設定と
    // 同じ、b は 96k の世代 = 設定と違う）
    std::fs::create_dir_all(lib.derived().join("A")).unwrap();
    for (id, name, profile) in [
        (a, "A/01.opus", "opus:128:v1"),
        (b, "A/02.opus", "opus:96:v1"),
    ] {
        std::fs::rename(
            lib.derived().join("opus").join(name),
            lib.derived().join(name),
        )
        .unwrap();
        lib.conn()
            .execute(
                "UPDATE derived_files SET rel_path = ?2, rel_path_key = ?3, audio_profile = ?4
                  WHERE track_id = ?1",
                rusqlite::params![id, name, name.to_lowercase(), profile],
            )
            .unwrap();
    }
    // a: profile 一致 → Move（再エンコードなし）
    assert_eq!(lib.run(a).await, JobState::Done);
    assert_eq!(lib.derived_row(a).unwrap().0, "opus/A/01.opus");
    assert_eq!(sha256(&lib.derived().join("opus/A/01.opus")), a_sha);
    assert!(!lib.derived().join("A/01.opus").exists());
    // b: profile 差分 → Encode（作り直し。旧ファイルは消える）
    assert_eq!(lib.run(b).await, JobState::Done);
    assert_eq!(lib.derived_row(b).unwrap().0, "opus/A/02.opus");
    assert!(lib.derived().join("opus/A/02.opus").is_file());
    assert!(!lib.derived().join("A/02.opus").exists());
    let profile: String = lib
        .conn()
        .query_row(
            "SELECT audio_profile FROM derived_files WHERE track_id = ?1",
            [b],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(profile, "opus:128:v1", "行の世代は設定に揃う");
    let _ = b_sha;
}

/// 系統が凍結（enabled=false）なら何もしない: 行もファイルも据え置き、タグ版が進んでも触らない。
/// 設定に無い系統（aac は P4-8）のジョブも何もしない
#[tokio::test]
async fn frozen_variant_and_unknown_variant_are_noops() {
    require_tools!();
    let lib = Lib::new();
    let p = lib.add("A/01.flac", 1, "a");
    lib.scan().await;
    lib.start();
    let a = lib.track_id("A/01.flac");
    assert_eq!(lib.run(a).await, JobState::Done);
    let before = sha256(&lib.derived().join("opus/A/01.opus"));
    // 凍結
    lib.sync_opus(false, 128, 1);
    common::retag(&p, |t| t.set_title("a2".to_owned()));
    lib.scan().await;
    let (av, tv) = lib.versions(a);
    let job = lib.enqueue(a, av, tv).await;
    assert_eq!(lib.wait_job(job).await, JobState::Done);
    assert_eq!(sha256(&lib.derived().join("opus/A/01.opus")), before);
    assert_eq!(lib.derived_row(a).unwrap().2, 1, "タグ版も据え置き");
    // 凍結中の系統（aac は節省略の既定 = off）
    let job = lib
        .jobs
        .enqueue(derived::new_job(a, Variant::Aac, av, tv))
        .await
        .unwrap()
        .id();
    assert_eq!(lib.wait_job(job).await, JobState::Done);
    assert!(!lib.derived().join("aac").exists());
    // 戻せば追随する
    lib.sync_opus(true, 128, 2);
    assert_eq!(lib.run(a).await, JobState::Done);
    assert_eq!(lib.derived_row(a).unwrap().2, 2);
}

#[tokio::test]
async fn tag_version_bump_retags_without_reencoding() {
    require_tools!();
    let lib = Lib::new();
    let p = lib.add("A/01.flac", 1, "a");
    lib.scan().await;
    lib.start();
    let id = lib.track_id("A/01.flac");
    assert_eq!(lib.run(id).await, JobState::Done);
    let opus = lib.derived().join("opus/A/01.opus");
    let audio_before = audio_md5(&opus);
    // 外部でタグを書き換えて再スキャン → tag_version++
    common::retag(&p, |t| t.set_title("b".to_owned()));
    lib.scan().await;
    assert_eq!(lib.versions(id), (1, 2));
    assert_eq!(lib.delivery(id).2, 1, "stale_tags");
    assert_eq!(lib.run(id).await, JobState::Done);
    assert_eq!(lib.derived_row(id).unwrap().2, 2);
    assert_eq!(
        lib.delivery(id),
        ("Derived/opus/A/01.opus".into(), "opus".into(), 0)
    );
    let af = read_audio_file(File::open(&opus).unwrap(), Some("opus")).unwrap();
    assert_eq!(af.tags.first("TITLE"), Some("b"));
    assert_eq!(audio_md5(&opus), audio_before, "音声は再エンコードしない");
    assert!(!has_tmp(&lib.derived().join("opus/A")));
}

#[tokio::test]
async fn audio_version_bump_reencodes_and_delivery_falls_back_meanwhile() {
    require_tools!();
    let lib = Lib::new();
    let p = lib.add("A/01.flac", 1, "a");
    lib.scan().await;
    lib.start();
    let id = lib.track_id("A/01.flac");
    assert_eq!(lib.run(id).await, JobState::Done);
    let before = audio_md5(&lib.derived().join("opus/A/01.opus"));
    // 別の音声で差し替え → audio_version++
    std::fs::remove_file(&p).unwrap();
    lib.add("A/01.flac", 2, "a");
    lib.scan().await;
    assert_eq!(lib.versions(id).0, 2);
    assert_eq!(
        lib.delivery(id),
        ("Library/A/01.flac".into(), "flac".into(), 0),
        "再エンコード待ちは原本へフォールバック"
    );
    assert_eq!(lib.run(id).await, JobState::Done);
    assert_eq!(lib.derived_row(id).unwrap().1, 2);
    assert_ne!(audio_md5(&lib.derived().join("opus/A/01.opus")), before);
    assert_eq!(
        lib.delivery(id),
        ("Derived/opus/A/01.opus".into(), "opus".into(), 0)
    );
}

#[tokio::test]
async fn stale_job_is_noop() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/01.flac", 1, "a");
    lib.scan().await;
    lib.start();
    let id = lib.track_id("A/01.flac");
    lib.conn()
        .execute("UPDATE tracks SET audio_version = 2 WHERE id = ?1", [id])
        .unwrap();
    let job = lib.enqueue(id, 1, 1).await;
    assert_eq!(lib.wait_job(job).await, JobState::Done);
    assert!(lib.derived_row(id).is_none());
    assert!(!lib.derived().join("opus/A/01.opus").exists());
}

#[tokio::test]
async fn external_move_renames_derived() {
    require_tools!();
    let lib = Lib::new();
    let p = lib.add("A/01.flac", 1, "a");
    lib.scan().await;
    lib.start();
    let id = lib.track_id("A/01.flac");
    assert_eq!(lib.run(id).await, JobState::Done);
    let before = sha256(&lib.derived().join("opus/A/01.opus"));
    std::fs::create_dir_all(lib.lib().join("B")).unwrap();
    std::fs::rename(&p, lib.lib().join("B/01 new.flac")).unwrap();
    lib.scan().await;
    assert_eq!(lib.track_id("B/01 new.flac"), id);
    assert_eq!(lib.run(id).await, JobState::Done);
    assert_eq!(lib.derived_row(id).unwrap().0, "opus/B/01 new.opus");
    assert!(!lib.derived().join("opus/A/01.opus").exists());
    assert_eq!(sha256(&lib.derived().join("opus/B/01 new.opus")), before);
    assert_eq!(lib.delivery(id).0, "Derived/opus/B/01 new.opus");
}

#[tokio::test]
async fn move_with_lost_derived_reencodes() {
    require_tools!();
    let lib = Lib::new();
    let p = lib.add("A/01.flac", 1, "a");
    lib.scan().await;
    lib.start();
    let id = lib.track_id("A/01.flac");
    assert_eq!(lib.run(id).await, JobState::Done);
    std::fs::remove_file(lib.derived().join("opus/A/01.opus")).unwrap();
    std::fs::rename(&p, lib.lib().join("A/02.flac")).unwrap();
    lib.scan().await;
    assert_eq!(lib.run(id).await, JobState::Done);
    assert!(lib.derived().join("opus/A/02.opus").is_file());
    assert_eq!(lib.derived_row(id).unwrap().0, "opus/A/02.opus");
}

#[tokio::test]
async fn retag_with_lost_derived_reencodes() {
    require_tools!();
    let lib = Lib::new();
    let p = lib.add("A/01.flac", 1, "a");
    lib.scan().await;
    lib.start();
    let id = lib.track_id("A/01.flac");
    assert_eq!(lib.run(id).await, JobState::Done);
    std::fs::remove_file(lib.derived().join("opus/A/01.opus")).unwrap();
    common::retag(&p, |t| t.set_title("b".to_owned()));
    lib.scan().await;
    assert_eq!(lib.run(id).await, JobState::Done);
    assert!(lib.derived().join("opus/A/01.opus").is_file());
    assert_eq!(lib.derived_row(id).unwrap().2, 2);
}

#[tokio::test]
async fn cover_replacement_retags_with_new_picture() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/B/01.flac", 1, "a");
    lib.add_cover("A/B", "red");
    lib.scan().await;
    lib.start();
    let id = lib.track_id("A/B/01.flac");
    assert_eq!(lib.run(id).await, JobState::Done);
    let opus = lib.derived().join("opus/A/B/01.opus");
    let first_art = lib.derived_row(id).unwrap().3.unwrap();
    let pic_before = picture_of(&opus).unwrap();
    let audio_before = audio_md5(&opus);
    std::thread::sleep(Duration::from_millis(20));
    lib.add_cover("A/B", "blue");
    lib.scan().await;
    assert_eq!(lib.versions(id), (1, 1), "カバー差し替えは版に乗らない");
    assert_eq!(lib.run(id).await, JobState::Done);
    let second_art = lib.derived_row(id).unwrap().3.unwrap();
    assert_ne!(first_art, second_art);
    assert_ne!(picture_of(&opus).unwrap(), pic_before);
    assert_eq!(audio_md5(&opus), audio_before);
    // 画像を消せば埋め込みも消える
    std::fs::remove_file(lib.lib().join("A/B/cover.png")).unwrap();
    lib.scan().await;
    assert_eq!(lib.run(id).await, JobState::Done);
    assert_eq!(lib.derived_row(id).unwrap().3, None);
    assert!(picture_of(&opus).is_none());
}

#[tokio::test]
async fn library_changed_under_us_fails_without_writing() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/01.flac", 1, "a");
    lib.scan().await;
    lib.start();
    let id = lib.track_id("A/01.flac");
    // DB の stat をずらして「開いた FD が行と違う」状態にする
    lib.conn()
        .execute(
            "UPDATE tracks SET mtime_ns = mtime_ns + 1 WHERE id = ?1",
            [id],
        )
        .unwrap();
    let job = lib.enqueue_once(id).await;
    assert_eq!(lib.wait_job(job).await, JobState::Failed);
    assert!(lib.last_error(job).unwrap().contains("再スキャン"));
    assert!(lib.derived_row(id).is_none());
    assert!(!lib.derived().join("opus/A/01.opus").exists());
    assert_eq!(lib.tmp_count(), 0);
}

#[tokio::test]
async fn occupied_path_of_missing_track_is_taken_over() {
    require_tools!();
    let lib = Lib::new();
    let pa = lib.add("A/01.flac", 1, "a");
    let pb = lib.add("A/02.flac", 2, "b");
    lib.scan().await;
    lib.start();
    let a = lib.track_id("A/01.flac");
    let b = lib.track_id("A/02.flac");
    assert_eq!(lib.run(a).await, JobState::Done);
    assert_eq!(lib.run(b).await, JobState::Done);
    let b_audio = audio_md5(&lib.derived().join("opus/A/02.opus"));
    // a を消して b を a のパスへ移す → b は inode で追随し、a は key を明け渡して missing。
    // b の Derived の期待パスは a の Derived が占有している
    std::fs::remove_file(&pa).unwrap();
    std::fs::rename(&pb, &pa).unwrap();
    lib.scan().await;
    assert_eq!(lib.track_id("A/01.flac"), b);
    let a_missing: Option<i64> = lib
        .conn()
        .query_row("SELECT missing_since FROM tracks WHERE id = ?1", [a], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(a_missing.is_some());
    assert_eq!(lib.run(b).await, JobState::Done);
    assert_eq!(lib.derived_row(b).unwrap().0, "opus/A/01.opus");
    assert!(
        lib.derived_row(a).is_none(),
        "missing 行の Derived は明け渡す"
    );
    assert!(!lib.derived().join("opus/A/02.opus").exists());
    assert_eq!(audio_md5(&lib.derived().join("opus/A/01.opus")), b_audio);
    assert_eq!(lib.delivery(b).0, "Derived/opus/A/01.opus");
}

#[tokio::test]
async fn occupied_path_of_live_stale_track_fails_until_it_moves() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/01.flac", 1, "a");
    lib.add("A/02.flac", 2, "b");
    lib.scan().await;
    lib.start();
    let a = lib.track_id("A/01.flac");
    let b = lib.track_id("A/02.flac");
    // b の Derived が a の期待パスを持っている（DB を直接壊す）
    derived::upsert(
        &lib.conn(),
        b,
        Variant::Opus,
        "opus/A/01.opus",
        None,
        1,
        derived::TagState {
            src_tag_version: 1,
            src_artwork_id: None,
            src_rg_scanned_at: None,
        },
        &derived::Profiles {
            audio_profile: "opus:128:v1".into(),
            tag_profile: "opus:v1".into(),
        },
        0,
    )
    .unwrap();
    let job = lib.enqueue_once(a).await;
    assert_eq!(lib.wait_job(job).await, JobState::Failed);
    // b 自身の期待パスは A/02.opus なので「追随待ち」。b の transcode は無いので失敗（次の scan で
    // 両方が投入される）
    assert!(lib.last_error(job).unwrap().contains("追随待ち"));
    assert!(!lib.derived().join("opus/A/01.opus").exists());
    // b が追随（行の指す実体は無いので作り直し）すれば a は通る
    assert_eq!(lib.run(b).await, JobState::Done);
    assert_eq!(lib.derived_row(b).unwrap().0, "opus/A/02.opus");
    assert_eq!(lib.run(a).await, JobState::Done);
    assert_eq!(lib.derived_row(a).unwrap().0, "opus/A/01.opus");
}

#[tokio::test]
async fn cancel_leaves_nothing_behind() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/01.flac", 1, "a");
    lib.scan().await;
    lib.start();
    let id = lib.track_id("A/01.flac");
    let (av, tv) = lib.versions(id);
    let job = lib.enqueue(id, av, tv).await;
    let _ = lib.jobs.cancel(job).await;
    let st = lib.wait_job(job).await;
    // cancel が間に合えば何も出来ない。間に合わなければ完成している（どちらも整合）
    match st {
        JobState::Cancelled => {
            assert!(lib.derived_row(id).is_none());
            assert!(!lib.derived().join("opus/A/01.opus").exists());
        }
        JobState::Done => assert!(lib.derived_row(id).is_some()),
        other => panic!("{other:?}: {:?}", lib.last_error(job)),
    }
    assert_eq!(lib.tmp_count(), 0);
}

#[tokio::test]
async fn rg_scanned_after_derived_retags_with_r128() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/01.flac", 1, "a");
    lib.scan().await;
    lib.start();
    let id = lib.track_id("A/01.flac");
    assert_eq!(lib.run(id).await, JobState::Done);
    let opus = lib.derived().join("opus/A/01.opus");
    assert!(picture_of(&opus).is_none());
    let audio_before = audio_md5(&opus);
    // 解析値が後から入る（版は動かない）→ タグだけ書き直す
    lib.set_rg(id);
    assert_eq!(lib.versions(id), (1, 1));
    assert_eq!(lib.run(id).await, JobState::Done);
    let af = read_audio_file(File::open(&opus).unwrap(), Some("opus")).unwrap();
    assert_eq!(af.tags.first("R128_TRACK_GAIN"), Some("-2816"));
    assert_eq!(audio_md5(&opus), audio_before);
    // もう一度は何もしない
    let before = sha256(&opus);
    assert_eq!(lib.run(id).await, JobState::Done);
    assert_eq!(sha256(&opus), before);
}

/// 占有の確認と物理的な書き込みの間に、占有側（missing）が別のパスで復活する競合。
/// claim は DB で先に確定するので、復活した側は行を失って自分の期待パスに作り直すだけで、
/// こちらが置いたファイルを動かさない
#[tokio::test]
async fn holder_reviving_after_claim_does_not_steal_the_placed_file() {
    require_tools!();
    let lib = Lib::new();
    let pa = lib.add("A/01.flac", 1, "a");
    let pb = lib.add("A/02.flac", 2, "b");
    let a_copy = lib.dir.path().join("a-copy.flac");
    std::fs::copy(&pa, &a_copy).unwrap();
    lib.scan().await;
    // フック: 武装している間だけ、b が占有を確定した直後に a を A/03.flac として復活させる
    // （scanner は audio_md5 で同じ行に追随する）
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let scanner = lib.scanner.clone();
    let lib_dir = lib.lib();
    let hook: TestHook = {
        let armed = armed.clone();
        Arc::new(move |point: &'static str| {
            let scanner = scanner.clone();
            let lib_dir = lib_dir.clone();
            let a_copy = a_copy.clone();
            let armed = armed.clone();
            Box::pin(async move {
                if point != "claimed" || !armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                std::fs::copy(&a_copy, lib_dir.join("A/03.flac")).unwrap();
                scanner
                    .run(
                        ScanKind::Incremental,
                        Arc::new(|_, _, _| {}),
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap();
            })
        })
    };
    lib.start_with_hook(hook);
    let a = lib.track_id("A/01.flac");
    let b = lib.track_id("A/02.flac");
    assert_eq!(lib.run(a).await, JobState::Done);
    assert_eq!(lib.run(b).await, JobState::Done);
    let b_audio = audio_md5(&lib.derived().join("opus/A/02.opus"));
    let a_audio = audio_md5(&lib.derived().join("opus/A/01.opus"));
    // a を消して b を a のパスへ → a は missing、b の期待パスは a の Derived が占有
    std::fs::remove_file(&pa).unwrap();
    std::fs::rename(&pb, &pa).unwrap();
    lib.scan().await;
    assert_eq!(lib.track_id("A/01.flac"), b);

    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(lib.run(b).await, JobState::Done);
    assert!(
        !armed.load(std::sync::atomic::Ordering::SeqCst),
        "フックが発火した"
    );
    // a は復活したが Derived の行は失っている。b の Derived は b の音声
    assert_eq!(lib.track_id("A/03.flac"), a);
    let a_missing: Option<i64> = lib
        .conn()
        .query_row("SELECT missing_since FROM tracks WHERE id = ?1", [a], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(a_missing.is_none(), "a は復活している");
    assert!(lib.derived_row(a).is_none());
    assert_eq!(lib.derived_row(b).unwrap().0, "opus/A/01.opus");
    assert_eq!(audio_md5(&lib.derived().join("opus/A/01.opus")), b_audio);
    // a の transcode は自分の期待パスに作り直すだけで、b のファイルを動かさない
    assert_eq!(lib.run(a).await, JobState::Done);
    assert_eq!(lib.derived_row(a).unwrap().0, "opus/A/03.opus");
    assert_eq!(audio_md5(&lib.derived().join("opus/A/03.opus")), a_audio);
    assert_eq!(audio_md5(&lib.derived().join("opus/A/01.opus")), b_audio);
    assert_eq!(lib.derived_row(b).unwrap().0, "opus/A/01.opus");
}

/// 原画像がキャッシュに無いときは画像なしで作り、`src_artwork_id` には None を記録する。
/// 次のスキャンが原画像を復旧すれば不一致になり、画像付きで書き直される
#[tokio::test]
async fn missing_artwork_cache_is_recorded_as_no_picture_and_recovers() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/B/01.flac", 1, "a");
    lib.add_cover("A/B", "red");
    lib.scan().await;
    lib.start();
    let id = lib.track_id("A/B/01.flac");
    // キャッシュを丸ごと消す
    std::fs::remove_dir_all(lib.dir.path().join("thumbs")).unwrap();
    std::fs::create_dir(lib.dir.path().join("thumbs")).unwrap();
    assert_eq!(lib.run(id).await, JobState::Done);
    let opus = lib.derived().join("opus/A/B/01.opus");
    assert!(picture_of(&opus).is_none());
    assert_eq!(
        lib.derived_row(id).unwrap().3,
        None,
        "埋めていないので None"
    );
    // 揃っているとは判定されない → もう一度走らせても原画像が無い間は画像なしのまま
    assert_eq!(lib.run(id).await, JobState::Done);
    assert!(picture_of(&opus).is_none());
    // スキャンが原画像を復旧する（同じ SHA-256 なので同じ artwork id）
    lib.scan().await;
    assert_eq!(lib.run(id).await, JobState::Done);
    let art = lib.derived_row(id).unwrap().3;
    assert!(art.is_some());
    assert!(picture_of(&opus).is_some());
    assert!(lib.thumb_exists(art.unwrap(), 768));
}

#[test]
fn sweep_tmp_removes_leftovers_and_keeps_everything_else() {
    let dir = tempfile::tempdir().unwrap();
    let data_tmp = dir.path().join("tmp");
    let derived = dir.path().join("Derived");
    std::fs::create_dir_all(derived.join("opus/A/B")).unwrap();
    std::fs::create_dir_all(&data_tmp).unwrap();
    for name in [
        "spindle-transcode-0123.wav",
        "spindle-transcode-0123.opus",
        "spindle-normalize-9.flac",
        "other.txt",
    ] {
        std::fs::write(data_tmp.join(name), b"x").unwrap();
    }
    std::fs::write(derived.join("opus/A/B/01.opus"), b"x").unwrap();
    std::fs::create_dir_all(derived.join("A/B")).unwrap();
    std::fs::write(derived.join("A/B/.spindle-tmp-abcd"), b"x").unwrap();
    std::fs::write(derived.join(".spindle-tmp-root"), b"x").unwrap();
    std::fs::write(derived.join("opus/A/keep.opus"), b"x").unwrap();
    let root = RootDir::open(&derived).unwrap();
    let report = sweep_tmp(&data_tmp, &root);
    assert_eq!(
        report,
        SweepReport {
            data_tmp: 3,
            derived_tmp: 2
        }
    );
    assert!(data_tmp.join("other.txt").is_file());
    assert!(!data_tmp.join("spindle-transcode-0123.wav").exists());
    assert!(derived.join("opus/A/B/01.opus").is_file());
    assert!(derived.join("opus/A/keep.opus").is_file());
    assert!(!derived.join("A/B/.spindle-tmp-abcd").exists());
    assert!(!derived.join(".spindle-tmp-root").exists());
    // 二度目は 0
    assert_eq!(sweep_tmp(&data_tmp, &root), SweepReport::default());
    // 無いディレクトリでも落ちない
    assert_eq!(
        sweep_tmp(&dir.path().join("nope"), &root),
        SweepReport::default()
    );
}

/// `x.flac` と `x.wav` は Library では別のパスだが期待パスは同じ `x.opus`。同時に初回生成しても
/// 予約で直列化され、勝った方のファイルと行だけが残る。負けた方は「生きているトラックが持って
/// いる」で失敗し、勝者のファイルを上書きしない
#[tokio::test]
async fn same_expected_path_from_flac_and_wav_only_one_wins() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/x.flac", 1, "flac");
    let wav = lib.lib().join("A/x.wav");
    common::write_wav(&wav, &common::pcm_samples(2), 16);
    common::set_basic_tags(&wav, "wav", "Artist", "Album", "AlbumArtist", 2, 1);
    lib.scan().await;
    lib.start();
    let f = lib.track_id("A/x.flac");
    let w = lib.track_id("A/x.wav");
    let jf = lib.enqueue_once(f).await;
    let jw = lib.enqueue_once(w).await;
    let (sf, sw) = (lib.wait_job(jf).await, lib.wait_job(jw).await);
    let mut states = [sf, sw];
    states.sort_by_key(|s| s.as_str());
    assert_eq!(
        states,
        [JobState::Done, JobState::Failed],
        "{sf:?} / {sw:?}"
    );
    let (winner, loser_job) = if sf == JobState::Done {
        (f, jw)
    } else {
        (w, jf)
    };
    assert!(lib
        .last_error(loser_job)
        .unwrap()
        .contains("生きているトラック"));
    // 行は勝者のものだけ。ファイルのタグは勝者のタイトル
    let rows: i64 = lib
        .conn()
        .query_row("SELECT COUNT(*) FROM derived_files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1);
    assert_eq!(lib.derived_row(winner).unwrap().0, "opus/A/x.opus");
    let af = read_audio_file(
        File::open(lib.derived().join("opus/A/x.opus")).unwrap(),
        Some("opus"),
    )
    .unwrap();
    let want = if winner == f { "flac" } else { "wav" };
    assert_eq!(af.tags.first("TITLE"), Some(want));
    // 予約は残っていない
    let locks: i64 = lib
        .conn()
        .query_row("SELECT COUNT(*) FROM derived_path_locks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(locks, 0);
}

/// 先に走っている旧 holder の retag（Library を読み終えて Derived を書く直前）は予約を持って
/// いるので、その間に占有側が missing になって新しいトラックが同じ期待パスを claim しようとしても
/// 再キューされる。retag が終わってから claim → 生成の順になり、最終的なファイルは新しい方の音声
#[tokio::test]
async fn running_retag_blocks_a_new_claimant_until_it_finishes() {
    require_tools!();
    let lib = Lib::new();
    let pa = lib.add("A/01.flac", 1, "a");
    let pb = lib.add("A/02.flac", 2, "b");
    lib.scan().await;
    let gate = Arc::new(tokio::sync::Notify::new());
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook: TestHook = {
        let gate = gate.clone();
        let armed = armed.clone();
        Arc::new(move |point: &'static str| {
            let gate = gate.clone();
            let armed = armed.clone();
            Box::pin(async move {
                if point == "retag_before_write"
                    && armed.swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    gate.notified().await;
                }
            })
        })
    };
    lib.start_with_hook(hook);
    let a = lib.track_id("A/01.flac");
    let b = lib.track_id("A/02.flac");
    assert_eq!(lib.run(a).await, JobState::Done);
    assert_eq!(lib.run(b).await, JobState::Done);
    let a_audio = audio_md5(&lib.derived().join("opus/A/01.opus"));
    let b_audio = audio_md5(&lib.derived().join("opus/A/02.opus"));
    assert_ne!(a_audio, b_audio);
    // a に retag が要る状態にして、書く直前で止める
    common::retag(&pa, |t| t.set_title("a2".to_owned()));
    lib.scan().await;
    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let (av, tv) = lib.versions(a);
    let ja = lib.enqueue(a, av, tv).await;
    // CI のランナーは I/O が遅く回数ベースでは足りないので、経過時間で待つ
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        if !armed.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !armed.load(std::sync::atomic::Ordering::SeqCst),
        "retag が書く直前で止まった"
    );
    // その間に a を消して b を a のパスへ → a は missing、b の期待パスは a の Derived
    std::fs::remove_file(&pa).unwrap();
    std::fs::rename(&pb, &pa).unwrap();
    lib.scan().await;
    assert_eq!(lib.track_id("A/01.flac"), b);
    let (bv, btv) = lib.versions(b);
    let jb = lib.enqueue(b, bv, btv).await;
    // b は予約が取れず再キューを繰り返す。a のファイルは a のまま
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let sb: String = lib
        .conn()
        .query_row("SELECT state FROM jobs WHERE id = ?1", [jb], |r| r.get(0))
        .unwrap();
    assert_eq!(sb, "queued", "予約待ちで再キューされている");
    assert_eq!(audio_md5(&lib.derived().join("opus/A/01.opus")), a_audio);
    assert_eq!(lib.derived_row(a).unwrap().0, "opus/A/01.opus");
    // a の retag を進める → 終わったら b が claim して作り直す
    gate.notify_one();
    assert_eq!(lib.wait_job(ja).await, JobState::Done);
    assert_eq!(lib.wait_job(jb).await, JobState::Done);
    assert!(lib.derived_row(a).is_none());
    assert_eq!(lib.derived_row(b).unwrap().0, "opus/A/01.opus");
    assert_eq!(audio_md5(&lib.derived().join("opus/A/01.opus")), b_audio);
    assert!(!lib.derived().join("opus/A/02.opus").exists());
}

/// retag が Library を読んでから Derived を書く間に album gain を off にされた（track lock を取らない
/// 経路。D-74）とき、書いた内容は古い世代なので同じジョブが再キューされ、揃え直した Derived には
/// R128_ALBUM_GAIN が残らず、行の世代も現在値に揃う
#[tokio::test]
async fn album_gain_turned_off_during_retag_is_caught_up_by_requeue() {
    use spindle::db::replaygain as dbrg;
    require_tools!();
    let lib = Lib::new();
    lib.add("A/01.flac", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let album: i64 = lib
        .conn()
        .query_row("SELECT album_id FROM tracks WHERE id = ?1", [a], |r| {
            r.get(0)
        })
        .unwrap();
    lib.conn()
        .execute("UPDATE albums SET album_gain = 1 WHERE id = ?1", [album])
        .unwrap();
    lib.set_rg(a);
    let gate = Arc::new(tokio::sync::Notify::new());
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook: TestHook = {
        let gate = gate.clone();
        let armed = armed.clone();
        Arc::new(move |point: &'static str| {
            let gate = gate.clone();
            let armed = armed.clone();
            Box::pin(async move {
                if point == "retag_before_write"
                    && armed.swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    gate.notified().await;
                }
            })
        })
    };
    lib.start_with_hook(hook);
    assert_eq!(lib.run(a).await, JobState::Done);
    let opus = lib.derived().join("opus/A/01.opus");
    let af = read_audio_file(File::open(&opus).unwrap(), Some("opus")).unwrap();
    assert_eq!(af.tags.first("R128_ALBUM_GAIN"), Some("-2304"));

    // RG の世代だけ進めて retag を要る状態にし、書く直前で止める
    lib.conn()
        .execute("UPDATE tracks SET rg_scanned_at = 2 WHERE id = ?1", [a])
        .unwrap();
    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let (av, tv) = lib.versions(a);
    let job = lib.enqueue(a, av, tv).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while std::time::Instant::now() < deadline && armed.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !armed.load(std::sync::atomic::Ordering::SeqCst),
        "retag が書く直前で止まった"
    );
    // その間に album gain を off（album 値が消え、rg_scanned_at が進む）。running の間の投入は dedup
    let ch = dbrg::set_album_gain(&lib.conn(), album, false, 1000)
        .unwrap()
        .unwrap();
    assert_eq!(ch.cleared, vec![a]);
    gate.notify_one();
    assert_eq!(lib.wait_job(job).await, JobState::Done);

    let scanned: i64 = lib
        .conn()
        .query_row("SELECT rg_scanned_at FROM tracks WHERE id = ?1", [a], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(scanned, 1000);
    let src: Option<i64> = lib
        .conn()
        .query_row(
            "SELECT src_rg_scanned_at FROM derived_files WHERE track_id = ?1",
            [a],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(src, Some(1000), "再キューで現在の世代に揃う");
    let af = read_audio_file(File::open(&opus).unwrap(), Some("opus")).unwrap();
    assert!(
        af.tags.first("R128_ALBUM_GAIN").is_none(),
        "古い album gain が Derived に残らない"
    );
    assert_eq!(af.tags.first("R128_TRACK_GAIN"), Some("-2816"));
}

/// Library の 2 件 swap。双方の期待パスを相手が持っているが、一方が自分の Derived を一時名へ退避して
/// 相手に譲り、次の試行で自分も移る（rename バッチの 2 段階と同じ）。再エンコードは走らない
#[tokio::test]
async fn library_swap_moves_both_derived_without_reencoding() {
    require_tools!();
    let lib = Lib::new();
    let p1 = lib.add("A/01.flac", 1, "one");
    let p2 = lib.add("A/02.flac", 2, "two");
    lib.scan().await;
    lib.start();
    let a = lib.track_id("A/01.flac");
    let b = lib.track_id("A/02.flac");
    assert_eq!(lib.run(a).await, JobState::Done);
    assert_eq!(lib.run(b).await, JobState::Done);
    let a_audio = audio_md5(&lib.derived().join("opus/A/01.opus"));
    let b_audio = audio_md5(&lib.derived().join("opus/A/02.opus"));
    let a_sha = sha256(&lib.derived().join("opus/A/01.opus"));
    let b_sha = sha256(&lib.derived().join("opus/A/02.opus"));
    // swap
    let tmp = lib.lib().join("A/swap.tmp");
    std::fs::rename(&p1, &tmp).unwrap();
    std::fs::rename(&p2, &p1).unwrap();
    std::fs::rename(&tmp, &p2).unwrap();
    lib.scan().await;
    assert_eq!(lib.track_id("A/01.flac"), b);
    assert_eq!(lib.track_id("A/02.flac"), a);
    // 両方を投入（scan 完了時の一括投入と同じ）。どちらが先でも収束する
    let (av, atv) = lib.versions(a);
    let (bv, btv) = lib.versions(b);
    let ja = lib.enqueue(a, av, atv).await;
    let jb = lib.enqueue(b, bv, btv).await;
    assert_eq!(lib.wait_job(ja).await, JobState::Done);
    assert_eq!(lib.wait_job(jb).await, JobState::Done);
    assert_eq!(lib.derived_row(a).unwrap().0, "opus/A/02.opus");
    assert_eq!(lib.derived_row(b).unwrap().0, "opus/A/01.opus");
    assert_eq!(audio_md5(&lib.derived().join("opus/A/02.opus")), a_audio);
    assert_eq!(audio_md5(&lib.derived().join("opus/A/01.opus")), b_audio);
    // ファイルは rename されただけ（再エンコードしていない）
    assert_eq!(sha256(&lib.derived().join("opus/A/02.opus")), a_sha);
    assert_eq!(sha256(&lib.derived().join("opus/A/01.opus")), b_sha);
    // 退避名も予約も残らない
    let leftovers: Vec<String> = std::fs::read_dir(lib.derived().join("opus/A"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".moving-") || n.starts_with(".spindle-tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    let locks: i64 = lib
        .conn()
        .query_row("SELECT COUNT(*) FROM derived_path_locks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(locks, 0);
    assert_eq!(lib.delivery(a).0, "Derived/opus/A/02.opus");
    assert_eq!(lib.delivery(b).0, "Derived/opus/A/01.opus");
}

/// 3 件の循環 rename（01 → 02 → 03 → 01）も収束する
#[tokio::test]
async fn library_three_cycle_converges() {
    require_tools!();
    let lib = Lib::new();
    let p1 = lib.add("A/01.flac", 1, "one");
    let p2 = lib.add("A/02.flac", 2, "two");
    let p3 = lib.add("A/03.flac", 3, "three");
    lib.scan().await;
    lib.start();
    let ids: Vec<i64> = ["A/01.flac", "A/02.flac", "A/03.flac"]
        .iter()
        .map(|r| lib.track_id(r))
        .collect();
    for &id in &ids {
        assert_eq!(lib.run(id).await, JobState::Done);
    }
    let audio: Vec<String> = ["opus/A/01.opus", "opus/A/02.opus", "opus/A/03.opus"]
        .iter()
        .map(|r| audio_md5(&lib.derived().join(r)))
        .collect();
    // 01 → 02 → 03 → 01
    let tmp = lib.lib().join("A/cycle.tmp");
    std::fs::rename(&p3, &tmp).unwrap();
    std::fs::rename(&p2, &p3).unwrap();
    std::fs::rename(&p1, &p2).unwrap();
    std::fs::rename(&tmp, &p1).unwrap();
    lib.scan().await;
    assert_eq!(lib.track_id("A/02.flac"), ids[0]);
    assert_eq!(lib.track_id("A/03.flac"), ids[1]);
    assert_eq!(lib.track_id("A/01.flac"), ids[2]);
    let mut jobs = Vec::new();
    for &id in &ids {
        let (av, tv) = lib.versions(id);
        jobs.push(lib.enqueue(id, av, tv).await);
    }
    for j in jobs {
        assert_eq!(lib.wait_job(j).await, JobState::Done);
    }
    assert_eq!(lib.derived_row(ids[0]).unwrap().0, "opus/A/02.opus");
    assert_eq!(lib.derived_row(ids[1]).unwrap().0, "opus/A/03.opus");
    assert_eq!(lib.derived_row(ids[2]).unwrap().0, "opus/A/01.opus");
    assert_eq!(audio_md5(&lib.derived().join("opus/A/02.opus")), audio[0]);
    assert_eq!(audio_md5(&lib.derived().join("opus/A/03.opus")), audio[1]);
    assert_eq!(audio_md5(&lib.derived().join("opus/A/01.opus")), audio[2]);
    let leftovers = std::fs::read_dir(lib.derived().join("opus/A"))
        .unwrap()
        .filter(|e| {
            let n = e
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .into_owned();
            n.contains(".moving-") || n.starts_with(".spindle-tmp-")
        })
        .count();
    assert_eq!(leftovers, 0);
}

/// do_move がエラーで早期 return しても予約は解放される
#[tokio::test]
async fn move_failure_releases_the_path_lock() {
    require_tools!();
    let lib = Lib::new();
    let p = lib.add("A/01.flac", 1, "a");
    lib.scan().await;
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let derived_dir = lib.derived();
    let hook: TestHook = {
        let armed = armed.clone();
        Arc::new(move |point: &'static str| {
            let armed = armed.clone();
            let derived_dir = derived_dir.clone();
            Box::pin(async move {
                if point == "claimed" && armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    // 宛先ディレクトリ B の場所にファイルを置いて create_dir_all を失敗させる
                    std::fs::create_dir_all(derived_dir.join("opus")).unwrap();
                    std::fs::write(derived_dir.join("opus/B"), b"not a dir").unwrap();
                }
            })
        })
    };
    lib.start_with_hook(hook);
    let id = lib.track_id("A/01.flac");
    assert_eq!(lib.run(id).await, JobState::Done);
    std::fs::create_dir_all(lib.lib().join("B")).unwrap();
    std::fs::rename(&p, lib.lib().join("B/01.flac")).unwrap();
    lib.scan().await;
    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let job = lib.enqueue_once(id).await;
    assert_eq!(lib.wait_job(job).await, JobState::Failed);
    assert!(lib.last_error(job).unwrap().contains("ディレクトリ"));
    let locks: i64 = lib
        .conn()
        .query_row("SELECT COUNT(*) FROM derived_path_locks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(locks, 0);
    // 元の Derived は無傷
    assert!(lib.derived().join("opus/A/01.opus").is_file());
    assert_eq!(lib.derived_row(id).unwrap().0, "opus/A/01.opus");
}

/// swap と同時に両方の音声が差し替わった（両方 Plan::Encode で、互いの行が相手の期待パスを持つ）。
/// 音声版が古くても退避して key を空けるので収束する
#[tokio::test]
async fn swap_with_both_audio_replaced_converges() {
    require_tools!();
    let lib = Lib::new();
    let p1 = lib.add("A/01.flac", 1, "one");
    let p2 = lib.add("A/02.flac", 2, "two");
    lib.scan().await;
    lib.start();
    let a = lib.track_id("A/01.flac");
    let b = lib.track_id("A/02.flac");
    assert_eq!(lib.run(a).await, JobState::Done);
    assert_eq!(lib.run(b).await, JobState::Done);
    // swap → scan（inode で追随）→ 両方の中身を別の音声に差し替え → scan（rel_path で追随、音声版++）
    let tmp = lib.lib().join("A/swap.tmp");
    std::fs::rename(&p1, &tmp).unwrap();
    std::fs::rename(&p2, &p1).unwrap();
    std::fs::rename(&tmp, &p2).unwrap();
    lib.scan().await;
    assert_eq!(lib.track_id("A/01.flac"), b);
    assert_eq!(lib.track_id("A/02.flac"), a);
    std::fs::remove_file(&p1).unwrap();
    std::fs::remove_file(&p2).unwrap();
    lib.add("A/01.flac", 3, "one-new");
    lib.add("A/02.flac", 4, "two-new");
    lib.scan().await;
    assert_eq!(lib.track_id("A/01.flac"), b);
    assert_eq!(lib.track_id("A/02.flac"), a);
    assert_eq!(lib.versions(a).0, 2);
    assert_eq!(lib.versions(b).0, 2);
    assert_eq!(lib.derived_row(a).unwrap().0, "opus/A/01.opus");
    assert_eq!(lib.derived_row(b).unwrap().0, "opus/A/02.opus");
    let (av, atv) = lib.versions(a);
    let (bv, btv) = lib.versions(b);
    let ja = lib.enqueue(a, av, atv).await;
    let jb = lib.enqueue(b, bv, btv).await;
    assert_eq!(lib.wait_job(ja).await, JobState::Done);
    assert_eq!(lib.wait_job(jb).await, JobState::Done);
    let ra = lib.derived_row(a).unwrap();
    let rb = lib.derived_row(b).unwrap();
    assert_eq!((ra.0.as_str(), ra.1), ("opus/A/02.opus", 2));
    assert_eq!((rb.0.as_str(), rb.1), ("opus/A/01.opus", 2));
    let one = read_audio_file(
        File::open(lib.derived().join("opus/A/01.opus")).unwrap(),
        Some("opus"),
    )
    .unwrap();
    assert_eq!(one.tags.first("TITLE"), Some("one-new"));
    let two = read_audio_file(
        File::open(lib.derived().join("opus/A/02.opus")).unwrap(),
        Some("opus"),
    )
    .unwrap();
    assert_eq!(two.tags.first("TITLE"), Some("two-new"));
    let leftovers = std::fs::read_dir(lib.derived().join("opus/A"))
        .unwrap()
        .filter(|e| {
            let n = e
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .into_owned();
            n.contains(".moving-") || n.starts_with(".spindle-tmp-")
        })
        .count();
    assert_eq!(leftovers, 0);
    let locks: i64 = lib
        .conn()
        .query_row("SELECT COUNT(*) FROM derived_path_locks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(locks, 0);
}

/// 行だけ残って実体が無い状態での循環（電源断・外部削除）。実体が無い側は行を消して key を明け渡し、
/// 次の試行で作り直す
#[tokio::test]
async fn cycle_with_missing_physical_derived_converges() {
    require_tools!();
    let lib = Lib::new();
    let p1 = lib.add("A/01.flac", 1, "one");
    let p2 = lib.add("A/02.flac", 2, "two");
    let p3 = lib.add("A/03.flac", 3, "three");
    lib.scan().await;
    lib.start();
    let ids: Vec<i64> = ["A/01.flac", "A/02.flac", "A/03.flac"]
        .iter()
        .map(|r| lib.track_id(r))
        .collect();
    for &id in &ids {
        assert_eq!(lib.run(id).await, JobState::Done);
    }
    // 01 → 02 → 03 → 01 の循環にして、Derived の実体を全部消す
    let tmp = lib.lib().join("A/cycle.tmp");
    std::fs::rename(&p3, &tmp).unwrap();
    std::fs::rename(&p2, &p3).unwrap();
    std::fs::rename(&p1, &p2).unwrap();
    std::fs::rename(&tmp, &p1).unwrap();
    lib.scan().await;
    for rel in ["opus/A/01.opus", "opus/A/02.opus", "opus/A/03.opus"] {
        std::fs::remove_file(lib.derived().join(rel)).unwrap();
    }
    let mut jobs = Vec::new();
    for &id in &ids {
        let (av, tv) = lib.versions(id);
        jobs.push(lib.enqueue(id, av, tv).await);
    }
    for j in jobs {
        assert_eq!(lib.wait_job(j).await, JobState::Done);
    }
    assert_eq!(lib.derived_row(ids[0]).unwrap().0, "opus/A/02.opus");
    assert_eq!(lib.derived_row(ids[1]).unwrap().0, "opus/A/03.opus");
    assert_eq!(lib.derived_row(ids[2]).unwrap().0, "opus/A/01.opus");
    for (rel, title) in [
        ("opus/A/02.opus", "one"),
        ("opus/A/03.opus", "two"),
        ("opus/A/01.opus", "three"),
    ] {
        let af =
            read_audio_file(File::open(lib.derived().join(rel)).unwrap(), Some("opus")).unwrap();
        assert_eq!(af.tags.first("TITLE"), Some(title), "{rel}");
    }
    let locks: i64 = lib
        .conn()
        .query_row("SELECT COUNT(*) FROM derived_path_locks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(locks, 0);
}

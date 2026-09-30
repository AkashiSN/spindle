//! `source_hash` ジョブ（仕様 ③「送る元のハッシュ」）。実ファイルを tempdir に置いて確かめる。
//! 下半分（[`Lib`] 以降）はジョブシステムを実際に通す結合テスト（レビュー指摘: ハンドラの
//! `run` 自体を試す統合テストが無かった）

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{params, Connection};
use tokio_util::sync::CancellationToken;

use spindle::config::{DerivedConfig, OpusVariantConfig};
use spindle::db::{devices, open_memory_connection, Db};
use spindle::domain::derived::{Current, Variant};
use spindle::domain::device::{
    semantic_derived, semantic_master, sha256_hex, SourceHash, SourceKind,
};
use spindle::domain::relpath::RelPath;
use spindle::fsroot::RootDir;
use spindle::jobs::handlers::source_hash::{
    hash_source, hash_source_with_hook, save_if_current, SourceHashHandler,
};
use spindle::jobs::{EnqueueResult, JobState, JobType, Jobs, NewJob, Registry};

#[test]
fn hash_matches_content_and_records_identity() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("A")).unwrap();
    std::fs::write(dir.path().join("A/x.opus"), b"hello").unwrap();
    let root = RootDir::open(dir.path()).unwrap();
    let rel = RelPath::parse("A/x.opus").unwrap();
    let h = hash_source(&root, &rel, &semantic_master(1, 1))
        .unwrap()
        .unwrap();
    assert_eq!(h.sha256, sha256_hex(b"hello"));
    assert_eq!(h.size, 5);
    assert_eq!(h.semantic, semantic_master(1, 1));
    assert!(h.inode > 0);
}

#[test]
fn hash_is_discarded_when_file_changes_during_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.opus");
    std::fs::write(&path, b"hello").unwrap();
    let root = RootDir::open(dir.path()).unwrap();
    let rel = RelPath::parse("x.opus").unwrap();
    // 読んでいる途中で追記する（size / mtime が変わる）
    let mut hook = || {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"!").unwrap();
    };
    let got = hash_source_with_hook(&root, &rel, "s", &mut hook).unwrap();
    assert!(got.is_none(), "途中で変わったハッシュは保存しない");
}

#[test]
fn symlinks_are_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("real.opus"), b"x").unwrap();
    std::os::unix::fs::symlink(dir.path().join("real.opus"), dir.path().join("link.opus")).unwrap();
    let root = RootDir::open(dir.path()).unwrap();
    let rel = RelPath::parse("link.opus").unwrap();
    assert!(hash_source(&root, &rel, "s").is_err());
}

// ------------------------------------------------------------ ハンドラをジョブ経由で試す結合テスト

/// Library / Derived root と実 DB を持つ最小の環境。`SourceHashHandler` を本物のジョブとして走らせる
struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    jobs: Arc<Jobs>,
    library: Arc<RootDir>,
    derived: Arc<RootDir>,
    shutdown: CancellationToken,
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Library")).unwrap();
        std::fs::create_dir(dir.path().join("Derived")).unwrap();
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let library = Arc::new(RootDir::open(&dir.path().join("Library")).unwrap());
        let derived = Arc::new(RootDir::open(&dir.path().join("Derived")).unwrap());
        let jobs = Jobs::new(db);
        Self {
            dir,
            db_path,
            jobs,
            library,
            derived,
            shutdown: CancellationToken::new(),
        }
    }

    fn start(&self) {
        let mut reg = Registry::new();
        reg.register(
            JobType::SourceHash,
            Arc::new(SourceHashHandler::new(
                self.library.clone(),
                self.derived.clone(),
            )),
        );
        self.jobs.start(reg, self.shutdown.clone());
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.db_path).unwrap()
    }

    /// Library 直下に `rel` を書き、実体の物理同一性（inode/size/mtime_ns/ctime_ns）で tracks 行を作る
    fn insert_master_track(
        &self,
        id: i64,
        rel: &str,
        content: &[u8],
        audio_version: i64,
        tag_version: i64,
    ) {
        let path = self.dir.path().join("Library").join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        let rp = RelPath::parse(rel).unwrap();
        let st = self.library.stat(&rp).unwrap();
        self.conn()
            .execute(
                "INSERT INTO tracks (id, rel_path, rel_path_key, inode, size, mtime_ns, ctime_ns, codec,
                                     lossless, channels, audio_version, tag_version, seen_at)
                 VALUES (?1, ?2, lower(?2), ?3, ?4, ?5, ?6, 'opus', 0, 2, ?7, ?8, 0)",
                params![
                    id,
                    rel,
                    st.inode as i64,
                    st.size as i64,
                    st.mtime_ns,
                    st.ctime_ns,
                    audio_version,
                    tag_version
                ],
            )
            .unwrap();
    }

    /// 可逆トラックと、opus 系統を on にした設定（`enqueue_if_stale` が投入対象と見るため。Derived は
    /// RG が揃ってから作るので、RG は解析・書き込み済みで入れる。D-97）
    fn insert_lossless_track_with_opus_on(&self, id: i64, rel: &str) {
        let c = self.conn();
        c.execute(
            "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                                 channels, audio_version, tag_version, seen_at,
                                 rg_track_gain, rg_track_peak, rg_scanned_at, rg_written_at)
             VALUES (?1, ?2, lower(?2), 1, 0, 0, 'flac', 1, 2, 1, 1, 0, 0.0, 1.0, 0, 0)",
            params![id, rel],
        )
        .unwrap();
        let cfg = DerivedConfig {
            opus: OpusVariantConfig {
                enabled: true,
                bitrate: 256,
            },
            aac: Default::default(),
        };
        spindle::db::derived::sync_variants(&c, &cfg, false, 0).unwrap();
    }

    /// 実ファイルを置かずに Derived の行だけを作る
    fn insert_derived_row(&self, track_id: i64, rel: &str) {
        self.conn()
            .execute(
                "INSERT INTO derived_files (track_id, variant, rel_path, rel_path_key, codec, bitrate,
                                            src_audio_version, src_tag_version, generated_at,
                                            audio_profile, tag_profile)
                 VALUES (?1, 'opus', ?2, lower(?2), 'opus', 256, 1, 1, 0, 'ap1', 'tp1')",
                params![track_id, rel],
            )
            .unwrap();
    }

    fn derived_row_exists(&self, track_id: i64) -> bool {
        spindle::db::derived::get(&self.conn(), track_id, Variant::Opus)
            .unwrap()
            .is_some()
    }

    fn transcode_count(&self, track_id: i64) -> i64 {
        self.conn()
            .query_row(
                "SELECT count(*) FROM jobs WHERE type = 'transcode' AND state = 'queued'
                   AND json_extract(payload, '$.track_id') = ?1",
                [track_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    async fn run(&self, track_id: i64, kind: SourceKind) -> JobState {
        let job = match self
            .jobs
            .enqueue(
                NewJob::new(
                    JobType::SourceHash,
                    serde_json::json!({ "track_id": track_id, "source": kind.as_str() }),
                )
                .dedup_key(devices::source_hash_dedup_key(track_id, kind)),
            )
            .await
            .unwrap()
        {
            EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
        };
        self.wait_job(job).await
    }

    async fn wait_job(&self, id: i64) -> JobState {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
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
}

impl Drop for Lib {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// identity が tracks の物理同一性と一致していれば、意味トークンと sha256 を保存する
#[tokio::test]
async fn master_hash_is_saved_when_identity_matches() {
    let lib = Lib::new();
    lib.insert_master_track(1, "A/a.opus", b"hello world", 3, 5);
    lib.start();
    assert_eq!(lib.run(1, SourceKind::Master).await, JobState::Done);

    let row = devices::source_hash(&lib.conn(), 1, SourceKind::Master)
        .unwrap()
        .unwrap();
    assert_eq!(row.sha256, sha256_hex(b"hello world"));
    assert_eq!(row.semantic, semantic_master(3, 5));

    let (inode, size, mtime_ns, ctime_ns): (i64, i64, i64, i64) = lib
        .conn()
        .query_row(
            "SELECT inode, size, mtime_ns, ctime_ns FROM tracks WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(row.inode as i64, inode);
    assert_eq!(row.size as i64, size);
    assert_eq!(row.mtime_ns, mtime_ns);
    assert_eq!(row.ctime_ns, ctime_ns);
}

/// `tracks` の物理同一性が実体とずれている（走査がまだ反映していない）間はハッシュを取らず、
/// 失敗ではなく `Done`（走査待ち）で終わる。`source_hashes` には何も残らない
#[tokio::test]
async fn stale_master_identity_ends_done_without_saving_a_hash() {
    let lib = Lib::new();
    lib.insert_master_track(2, "A/b.opus", b"stale content", 1, 1);
    lib.conn()
        .execute("UPDATE tracks SET mtime_ns = mtime_ns + 1 WHERE id = 2", [])
        .unwrap();
    lib.start();

    assert_eq!(lib.run(2, SourceKind::Master).await, JobState::Done);
    assert!(devices::source_hash(&lib.conn(), 2, SourceKind::Master)
        .unwrap()
        .is_none());
}

/// 走査待ちで終わったら増分スキャンを投入し、`tracks` の物理同一性が変わるまで同じ送る元を再投入しない
/// （差分の計算のたびに source_hash が回り続けるループの防止）。物理同一性が変われば再び投入する
#[tokio::test]
async fn scan_pending_enqueues_scan_and_suppresses_reenqueue_until_identity_changes() {
    let lib = Lib::new();
    lib.insert_master_track(2, "A/b.opus", b"stale content", 1, 1);
    lib.conn()
        .execute("UPDATE tracks SET mtime_ns = mtime_ns + 1 WHERE id = 2", [])
        .unwrap();
    lib.start();
    let needs = [(2, SourceKind::Master)];

    let ids = devices::enqueue_source_hashes(&lib.conn(), &needs, 10).unwrap();
    assert_eq!(ids.len(), 1);
    lib.jobs.notify_enqueued(&ids).await;
    assert_eq!(lib.wait_job(ids[0]).await, JobState::Done);
    let note: Option<String> = lib
        .conn()
        .query_row("SELECT note FROM jobs WHERE id = ?1", [ids[0]], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(note.as_deref(), Some(devices::SCAN_PENDING_NOTE));
    let scans: i64 = lib
        .conn()
        .query_row(
            "SELECT count(*) FROM jobs WHERE type = 'scan' AND state = 'queued'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(scans, 1, "走査で tracks を直してもらう");

    assert!(
        devices::enqueue_source_hashes(&lib.conn(), &needs, 11)
            .unwrap()
            .is_empty(),
        "tracks の物理同一性が変わらない間は再投入しない"
    );
    assert!(devices::hashes_to_enqueue(&lib.conn(), &needs)
        .unwrap()
        .is_empty());

    // 走査が tracks を直した体（物理同一性が変わる）
    lib.conn()
        .execute("UPDATE tracks SET mtime_ns = mtime_ns - 1 WHERE id = 2", [])
        .unwrap();
    let again = devices::enqueue_source_hashes(&lib.conn(), &needs, 12).unwrap();
    assert_eq!(again.len(), 1, "物理同一性が変われば再び投入する");
    lib.jobs.notify_enqueued(&again).await;
    assert_eq!(lib.wait_job(again[0]).await, JobState::Done);
    assert!(devices::source_hash(&lib.conn(), 2, SourceKind::Master)
        .unwrap()
        .is_some());
}

/// Derived（opus）を送る元にしたときは `derived_files` の版から意味トークンを作り、
/// `source` 列に系統名（`opus`）で保存する
#[tokio::test]
async fn derived_source_is_saved_with_the_variant_semantic() {
    let lib = Lib::new();
    lib.conn()
        .execute(
            "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                                 channels, audio_version, tag_version, seen_at)
             VALUES (3, 'A/c.flac', 'a/c.flac', 1, 0, 0, 'flac', 1, 2, 1, 1, 0)",
            [],
        )
        .unwrap();
    let rel = "opus/A/c.opus";
    let path = lib.dir.path().join("Derived").join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"derived bytes").unwrap();
    lib.conn()
        .execute(
            "INSERT INTO derived_files (track_id, variant, rel_path, rel_path_key, codec, bitrate,
                                        src_audio_version, src_tag_version, generated_at, audio_profile,
                                        tag_profile)
             VALUES (3, 'opus', ?1, lower(?1), 'opus', 256, 1, 1, 0, 'ap1', 'tp1')",
            [rel],
        )
        .unwrap();
    lib.start();

    assert_eq!(
        lib.run(3, SourceKind::Derived(Variant::Opus)).await,
        JobState::Done
    );
    let row = devices::source_hash(&lib.conn(), 3, SourceKind::Derived(Variant::Opus))
        .unwrap()
        .unwrap();
    assert_eq!(row.sha256, sha256_hex(b"derived bytes"));
    let current = Current {
        rel_path: rel.to_owned(),
        src_audio_version: 1,
        src_tag_version: 1,
        src_artwork_id: None,
        src_rg_scanned_at: None,
        audio_profile: "ap1".into(),
        tag_profile: "tp1".into(),
    };
    assert_eq!(row.semantic, semantic_derived(Variant::Opus, &current));
}

/// Derived の実ファイルが無ければ失敗にせず、行を消して transcode を投入する（D-51 の drift）
#[tokio::test]
async fn missing_derived_row_is_deleted_and_transcode_enqueued() {
    let lib = Lib::new();
    lib.insert_lossless_track_with_opus_on(1, "A/a.flac");
    lib.insert_derived_row(1, "opus/A/a.opus");
    lib.start();
    assert_eq!(
        lib.run(1, SourceKind::Derived(Variant::Opus)).await,
        JobState::Done
    );
    assert!(!lib.derived_row_exists(1), "行を消す");
    assert_eq!(lib.transcode_count(1), 1, "作り直しを投入する");
}

/// transcode が動いている間（rename の途中かもしれない）は行を消さず、失敗（再試行）にする
#[tokio::test]
async fn missing_derived_is_kept_while_transcode_active() {
    let lib = Lib::new();
    lib.insert_lossless_track_with_opus_on(1, "A/a.flac");
    lib.insert_derived_row(1, "opus/A/a.opus");
    {
        let c = lib.conn();
        spindle::db::jobs::enqueue(
            &c,
            &spindle::db::derived::new_job(1, Variant::Opus, 1, 1),
            0,
        )
        .unwrap();
    }
    lib.start();
    // 失敗は再試行（バックオフ待ちの queued）になるので終端は待たず、失敗の記録が付くまで待つ
    let job = match lib
        .jobs
        .enqueue(
            NewJob::new(
                JobType::SourceHash,
                serde_json::json!({ "track_id": 1, "source": "opus" }),
            )
            .dedup_key(devices::source_hash_dedup_key(
                1,
                SourceKind::Derived(Variant::Opus),
            )),
        )
        .await
        .unwrap()
    {
        EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let last_error = loop {
        let e: Option<String> = lib
            .conn()
            .query_row("SELECT last_error FROM jobs WHERE id = ?1", [job], |r| {
                r.get(0)
            })
            .unwrap();
        if let Some(e) = e {
            break e;
        }
        assert!(std::time::Instant::now() < deadline, "失敗が記録されない");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(
        last_error.contains("transcode が動いている"),
        "{last_error}"
    );
    assert!(lib.derived_row_exists(1));
}

/// 行の `rel_path` が読んだ時から変わっていたら（transcode が rename を終えた）消さない
#[test]
fn delete_if_path_keeps_a_row_whose_path_moved() {
    let c = open_memory_connection().unwrap();
    c.execute(
        "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                             channels, audio_version, tag_version, seen_at)
         VALUES (1, 'A/a.flac', 'a/a.flac', 1, 0, 0, 'flac', 1, 2, 1, 1, 0)",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO derived_files (track_id, variant, rel_path, rel_path_key, codec, bitrate,
                                    src_audio_version, src_tag_version, generated_at, audio_profile,
                                    tag_profile)
         VALUES (1, 'opus', 'opus/A/old.opus', 'opus/a/old.opus', 'opus', 256, 1, 1, 0, 'ap1', 'tp1')",
        [],
    )
    .unwrap();
    let del = spindle::db::derived::delete_if_path;
    assert!(!del(&c, 1, Variant::Opus, "opus/A/other.opus").unwrap());
    assert!(spindle::db::derived::get(&c, 1, Variant::Opus)
        .unwrap()
        .is_some());
    assert!(del(&c, 1, Variant::Opus, "opus/A/old.opus").unwrap());
}

// ------------------------------------------------- save_if_current（読んでいる間の版の変化に対する再確認）

/// `save_if_current` は `resolve` した現在の意味トークンと `hash.semantic` が一致するときだけ保存する。
/// ジョブシステム越しにハッシュ計算とタグ書き込みの競合を確定的に再現するのは難しいので、
/// ハンドラの保存ステップを切り出したこの関数を直接叩いて確かめる（レビュー指摘のフォールバック）
#[test]
fn save_if_current_saves_when_matching_and_skips_when_semantic_is_stale() {
    let c = open_memory_connection().unwrap();
    c.execute(
        "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                             channels, audio_version, tag_version, seen_at)
         VALUES (1, 'a.opus', 'a.opus', 1, 0, 0, 'opus', 0, 2, 1, 1, 0)",
        [],
    )
    .unwrap();
    let hash = SourceHash {
        semantic: semantic_master(1, 1),
        inode: 1,
        size: 1,
        mtime_ns: 0,
        ctime_ns: 0,
        sha256: "s1".into(),
    };
    assert!(
        save_if_current(&c, 1, SourceKind::Master, &hash, 10).unwrap(),
        "現在の意味トークンと一致するので保存する"
    );
    assert_eq!(
        devices::source_hash(&c, 1, SourceKind::Master)
            .unwrap()
            .unwrap()
            .sha256,
        "s1"
    );

    // 保存前にタグが進んでいた（意味トークンが変わった）体で、古いハッシュを保存しようとする
    c.execute(
        "UPDATE tracks SET tag_version = tag_version + 1 WHERE id = 1",
        [],
    )
    .unwrap();
    let stale = SourceHash {
        sha256: "s2".into(),
        ..hash
    };
    assert!(
        !save_if_current(&c, 1, SourceKind::Master, &stale, 20).unwrap(),
        "意味トークンが変わっていれば保存しない"
    );
    assert_eq!(
        devices::source_hash(&c, 1, SourceKind::Master)
            .unwrap()
            .unwrap()
            .sha256,
        "s1",
        "古い行のまま残る"
    );
}

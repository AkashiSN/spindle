//! ロスレス → FLAC 正規化の coordinator（SPEC §7.4、D-10 / D-45 / D-46、docs/TASKS.md P1-4）。
//! 記録 → normalize ジョブ → MD5 照合 → Archive 退避 → 台帳 → 巻き戻し（restore / redo）→
//! 事前条件 → クラッシュ復旧 → スキャンとの並走 → キャンセル。
//! 合成ファイルは ffmpeg で作り、エンコードには flac を使う（どちらかが無ければ skip）

#![cfg(target_os = "linux")]

mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension as _};
use tokio_util::sync::CancellationToken;

use spindle::db::archive::{self, ArchiveReason, ArchiveState};
use spindle::db::history::{self, BatchState, OpKind, OpResult};
use spindle::db::Db;
use spindle::domain::relpath::RelPath;
use spindle::domain::tags::read_audio_file;
use spindle::edit::{
    flac_rel_path, normalize_temp_rel_path, CancelOutcome, EditError, Editor, NormalizeEnv,
    NormalizePlan, NormalizeStep, NormalizeTarget, SOURCE_HASH_KEY,
};
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, ScanReport, Scanner};
use spindle::jobs::handlers::normalize::NormalizeHandler;
use spindle::jobs::{JobState, JobType, Jobs, Registry};
use spindle::media::encode::FlacEncoder;
use spindle::media::fingerprint::{decoded_pcm_md5, flac_streaminfo_md5};

// ---------------------------------------------------------------- ハーネス

fn flac_bin() -> Option<PathBuf> {
    let p = Command::new("flac").arg("--version").output().ok()?;
    p.status.success().then(|| PathBuf::from("flac"))
}

macro_rules! require_tools {
    () => {
        if common::ffmpeg().is_none() || flac_bin().is_none() {
            eprintln!("ffmpeg / flac が無いので skip");
            return;
        }
    };
}

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    jobs: Arc<Jobs>,
    scanner: Scanner,
    editor: Arc<Editor>,
    shutdown: CancellationToken,
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["Library", "Archive", "tmp"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let db_path = dir.path().join("spindle.db");
        Self::open(dir, db_path, "ffmpeg")
    }

    fn open(dir: tempfile::TempDir, db_path: PathBuf, ffmpeg: impl AsRef<Path>) -> Self {
        let lib = dir.path().join("Library");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let root = Arc::new(RootDir::open(&lib).unwrap());
        let archive = Arc::new(RootDir::open(&dir.path().join("Archive")).unwrap());
        let jobs = Jobs::new(db.clone());
        let scanner = Scanner::new(db.clone(), root.clone(), 2);
        let editor = Arc::new(Editor::new(db.clone(), root, jobs.clone()).with_normalize(
            NormalizeEnv {
                archive,
                encoder: FlacEncoder::new(ffmpeg.as_ref(), "flac", 8, dir.path().join("tmp")),
                retention_days: 30,
            },
        ));
        Self {
            dir,
            db_path,
            jobs,
            scanner,
            editor,
            shutdown: CancellationToken::new(),
        }
    }

    /// 同じ DB とライブラリを開き直す（プロセス再起動の模擬）
    fn reopen(mut self) -> Self {
        self.shutdown.cancel();
        let dir = std::mem::replace(&mut self.dir, tempfile::tempdir().unwrap());
        let db_path = self.db_path.clone();
        Self::open(dir, db_path, "ffmpeg")
    }

    /// ffmpeg を差し替えて開き直す（MD5 不一致の模擬）
    fn reopen_with_ffmpeg(mut self, ffmpeg: &Path) -> Self {
        self.shutdown.cancel();
        let dir = std::mem::replace(&mut self.dir, tempfile::tempdir().unwrap());
        let db_path = self.db_path.clone();
        Self::open(dir, db_path, ffmpeg)
    }

    fn start(&self) {
        let mut reg = Registry::new();
        reg.register(
            JobType::Normalize,
            Arc::new(NormalizeHandler::new(self.editor.clone())),
        );
        self.jobs.start(reg, self.shutdown.clone());
    }

    fn lib(&self) -> PathBuf {
        self.dir.path().join("Library")
    }

    fn archive_dir(&self) -> PathBuf {
        self.dir.path().join("Archive")
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.lib().join(rel)
    }

    fn archived(&self, rel: &str) -> PathBuf {
        self.archive_dir().join(rel)
    }

    /// WAV（Rust で書く）か ALAC（ffmpeg）を Library に置き、基本タグを付ける
    fn add(&self, rel: &str, seed: u32, title: &str) -> PathBuf {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let ext = rel.rsplit_once('.').unwrap().1;
        match ext {
            "wav" => common::write_wav(&p, &common::pcm_samples(seed), 16),
            "m4a" => {
                let name = p.file_name().unwrap().to_str().unwrap().to_owned();
                common::make_audio(p.parent().unwrap(), &name, "alac.m4a", seed).unwrap();
            }
            "flac" => {
                let name = p.file_name().unwrap().to_str().unwrap().to_owned();
                common::make_audio(p.parent().unwrap(), &name, "flac", seed).unwrap();
            }
            "aiff" => {
                let name = p.file_name().unwrap().to_str().unwrap().to_owned();
                common::make_audio(p.parent().unwrap(), &name, "aiff", seed).unwrap();
            }
            other => panic!("unsupported {other}"),
        }
        common::set_basic_tags(&p, title, "Artist", "Album", "AlbumArtist", 1, 1);
        p
    }

    /// クラッシュ後の状態を作るとき、ジョブが unlink の前に記録する SHA-256 を代わりに書く
    fn record_source_hash(&self, batch_id: i64, bytes: &[u8]) {
        use sha2::Digest as _;
        let hex: String = sha2::Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let op = &self.ops(batch_id)[0];
        history::insert_edit(
            &self.conn(),
            op.id,
            SOURCE_HASH_KEY,
            &serde_json::Value::Null,
            &serde_json::json!(hex),
        )
        .unwrap();
    }

    /// pending の archive op の一時名（Library 相対）
    fn temp_path(&self, batch_id: i64, source_rel: &str) -> PathBuf {
        let op = &self.ops(batch_id)[0];
        let rel = normalize_temp_rel_path(op.id, &RelPath::parse(source_rel).unwrap()).unwrap();
        self.path(rel.as_str())
    }

    async fn scan(&self) -> ScanReport {
        self.scanner
            .run(
                ScanKind::Incremental,
                Arc::new(|_, _| {}),
                CancellationToken::new(),
            )
            .await
            .unwrap()
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

    fn row(&self, id: i64) -> Row {
        self.conn()
            .query_row(
                "SELECT rel_path, codec, lossless, audio_md5, audio_version, tag_version, inode,
                        original_codec, normalized_at, missing_since, title
                 FROM tracks WHERE id = ?1",
                [id],
                |r| {
                    Ok(Row {
                        rel_path: r.get(0)?,
                        codec: r.get(1)?,
                        lossless: r.get::<_, i64>(2)? == 1,
                        audio_md5: r.get(3)?,
                        audio_version: r.get(4)?,
                        tag_version: r.get(5)?,
                        inode: r.get(6)?,
                        original_codec: r.get(7)?,
                        normalized_at: r.get(8)?,
                        missing_since: r.get(9)?,
                        title: r.get(10)?,
                    })
                },
            )
            .unwrap()
    }

    fn track_count(&self) -> i64 {
        self.conn()
            .query_row("SELECT count(*) FROM tracks", [], |r| r.get(0))
            .unwrap()
    }

    fn batch_state(&self, id: i64) -> BatchState {
        history::get_batch(&self.conn(), id).unwrap().unwrap().state
    }

    fn ops(&self, batch_id: i64) -> Vec<history::Op> {
        history::list_ops(&self.conn(), batch_id).unwrap()
    }

    fn archived_row(&self, rel: &str) -> Option<archive::ArchivedFile> {
        archive::get_by_rel_path(&self.conn(), rel).unwrap()
    }

    async fn wait_batch_terminal(&self, id: i64) -> BatchState {
        for _ in 0..6000 {
            let st = self.batch_state(id);
            if !matches!(st, BatchState::Prepared | BatchState::Applying) {
                return st;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("batch {id} が終端にならない: {:?}", self.batch_state(id));
    }

    async fn wait_job_terminal(&self, id: i64) -> JobState {
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

    /// 対象を計画どおりに記録する（API の apply 相当）
    async fn normalize(&self, ids: &[i64]) -> Result<spindle::edit::Prepared, EditError> {
        let planned = self.editor.plan_normalize(ids).await?;
        let targets = planned
            .into_iter()
            .filter_map(|p| match p.planned {
                NormalizePlan::Path(new) => Some(NormalizeTarget {
                    track_id: p.track_id,
                    new_rel_path: new,
                    new_codec: "flac".to_owned(),
                    expected: None,
                    planned_conflict: None,
                }),
                NormalizePlan::Unchanged => None,
                NormalizePlan::Conflict(reason) => Some(NormalizeTarget {
                    track_id: p.track_id,
                    new_rel_path: p.current_rel_path,
                    new_codec: "flac".to_owned(),
                    expected: None,
                    planned_conflict: Some(reason),
                }),
            })
            .collect();
        self.editor
            .prepare_normalize(Some("FLAC 化"), targets)
            .await
    }

    fn tmp_leftovers(&self) -> Vec<String> {
        let mut out = Vec::new();
        for d in [self.dir.path().join("tmp"), self.lib(), self.archive_dir()] {
            walk_names(&d, &mut out);
        }
        out.retain(|n| n.contains("spindle-"));
        out
    }
}

impl Drop for Lib {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[derive(Debug)]
struct Row {
    rel_path: String,
    codec: String,
    lossless: bool,
    audio_md5: Option<Vec<u8>>,
    audio_version: i64,
    tag_version: i64,
    inode: i64,
    original_codec: Option<String>,
    normalized_at: Option<i64>,
    missing_since: Option<i64>,
    title: Option<String>,
}

fn walk_names(dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd {
        let e = e.unwrap();
        let p = e.path();
        if p.is_dir() {
            walk_names(&p, out);
        } else {
            out.push(p.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
}

fn inode_of(path: &Path) -> i64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).unwrap().ino() as i64
}

fn md5_of(path: &Path) -> Vec<u8> {
    let ext = path.extension().and_then(|e| e.to_str());
    if ext == Some("flac") {
        flac_streaminfo_md5(File::open(path).unwrap())
            .unwrap()
            .unwrap()
            .to_vec()
    } else {
        decoded_pcm_md5(File::open(path).unwrap(), ext)
            .unwrap()
            .to_vec()
    }
}

// ---------------------------------------------------------------- 計画と記録

#[tokio::test]
async fn plan_targets_only_lossless_sources() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/01.wav", 1, "a");
    lib.add("A/02.m4a", 2, "b");
    lib.add("A/03.flac", 3, "c");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let b = lib.track_id("A/02.m4a");
    let c = lib.track_id("A/03.flac");
    let planned = lib.editor.plan_normalize(&[a, b, c]).await.unwrap();
    assert_eq!(
        planned[0].planned,
        NormalizePlan::Path("A/01.flac".to_owned())
    );
    assert_eq!(planned[0].codec, "wav");
    assert_eq!(
        planned[1].planned,
        NormalizePlan::Path("A/02.flac".to_owned())
    );
    assert_eq!(planned[1].codec, "alac");
    assert_eq!(planned[2].planned, NormalizePlan::Unchanged);
    assert_eq!(flac_rel_path("x/y.WAV"), "x/y.flac");
    assert_eq!(flac_rel_path("noext"), "noext.flac");
}

#[tokio::test]
async fn plan_conflicts_when_destination_is_taken() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/01.wav", 1, "a");
    lib.add("A/01.flac", 2, "other");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let planned = lib.editor.plan_normalize(&[a]).await.unwrap();
    assert!(
        matches!(&planned[0].planned, NormalizePlan::Conflict(r) if r.contains("占有")),
        "{:?}",
        planned[0].planned
    );
    // 記録すると skipped_conflict の op として残る（ファイルは触らない）
    let prepared = lib.normalize(&[a]).await.unwrap();
    assert_eq!(prepared.conflict, 1);
    assert_eq!(prepared.job_ids.len(), 0);
    // applied が 0 件なので集計は failed（SPEC §7.5 の集計規則）
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Failed);
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(ops[0].result, OpResult::SkippedConflict);
    assert!(lib.path("A/01.wav").exists());
}

#[tokio::test]
async fn prepare_records_archive_ops_without_overlay() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let before = lib.row(a);
    let prepared = lib.normalize(&[a]).await.unwrap();
    assert_eq!(prepared.affected, 1);
    assert_eq!(prepared.job_ids.len(), 1);
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Prepared);
    // DB は先行更新しない（ファイルが正。変換が終わるまで元の実体）
    let after = lib.row(a);
    assert_eq!(after.rel_path, "A/01.wav");
    assert_eq!(after.codec, "wav");
    assert_eq!(after.inode, before.inode);
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].kind, OpKind::Archive);
    assert_eq!(ops[0].result, OpResult::Pending);
    assert_eq!(ops[0].expected.rel_path.as_deref(), Some("A/01.wav"));
    assert_eq!(ops[0].expected.inode, Some(before.inode));
    let edits = history::list_edits(&lib.conn(), ops[0].id).unwrap();
    let rel = edits.iter().find(|e| e.key == "rel_path").unwrap();
    assert_eq!(rel.old_value, serde_json::json!("A/01.wav"));
    assert_eq!(rel.new_value, serde_json::json!("A/01.flac"));
    let codec = edits.iter().find(|e| e.key == "codec").unwrap();
    assert_eq!(codec.old_value, serde_json::json!("wav"));
    assert_eq!(codec.new_value, serde_json::json!("flac"));
    let job: (String, Option<i64>, Option<String>) = lib
        .conn()
        .query_row(
            "SELECT type, edit_batch_id, dedup_key FROM jobs WHERE id = ?1",
            [prepared.job_ids[0]],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(job.0, "normalize");
    assert_eq!(job.1, Some(prepared.batch_id));
    assert_eq!(
        job.2.as_deref(),
        Some(format!("normalize:{a}:{}", ops[0].id).as_str())
    );
    // pending 中の再編集は拒否される
    let err = lib.normalize(&[a]).await.unwrap_err();
    assert!(matches!(err, EditError::Pending { .. }));
}

// ---------------------------------------------------------------- 反映

#[tokio::test]
async fn wav_becomes_flac_and_original_is_archived() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "曲名");
    let original_bytes = std::fs::read(&wav).unwrap();
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let before = lib.row(a);
    assert_eq!(before.audio_md5.as_deref(), Some(md5_of(&wav).as_slice()));

    lib.start();
    let prepared = lib.normalize(&[a]).await.unwrap();
    let state = lib.wait_batch_terminal(prepared.batch_id).await;
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(ops[0].result, OpResult::Applied, "{:?}", ops[0].error);
    assert_eq!(state, BatchState::Applied);

    // ファイル: Library には FLAC だけ、元 WAV は Archive の同じ相対パスに無傷で
    assert!(!lib.path("A/01.wav").exists());
    assert!(lib.path("A/01.flac").exists());
    assert_eq!(
        std::fs::read(lib.archived("A/01.wav")).unwrap(),
        original_bytes
    );
    assert_eq!(
        md5_of(&lib.path("A/01.flac")),
        md5_of(&lib.archived("A/01.wav"))
    );
    assert!(lib.tmp_leftovers().is_empty(), "{:?}", lib.tmp_leftovers());

    // DB: パス・コーデック・inode を追随、audio_md5 と audio_version は据え置き、出自を記録
    let after = lib.row(a);
    assert_eq!(after.rel_path, "A/01.flac");
    assert_eq!(after.codec, "flac");
    assert!(after.lossless);
    assert_eq!(after.audio_md5, before.audio_md5);
    assert_eq!(after.audio_version, before.audio_version);
    assert_eq!(after.inode, inode_of(&lib.path("A/01.flac")));
    assert_eq!(after.original_codec.as_deref(), Some("wav"));
    assert!(after.normalized_at.is_some());
    assert_eq!(after.title.as_deref(), Some("曲名"));
    // タグは FLAC に写っている
    let af = read_audio_file(File::open(lib.path("A/01.flac")).unwrap(), Some("flac")).unwrap();
    assert_eq!(af.tags.first("TITLE"), Some("曲名"));
    assert_eq!(af.tags.first("ALBUMARTIST"), Some("AlbumArtist"));
    assert_eq!(af.tags.first("TRACKNUMBER"), Some("1"));

    // 台帳: held、reason=normalize、期限は 30 日後
    let row = lib.archived_row("A/01.wav").unwrap();
    assert_eq!(row.state, ArchiveState::Held);
    assert_eq!(row.reason, ArchiveReason::Normalize);
    assert_eq!(row.track_id, Some(a));
    assert_eq!(row.op_id, Some(ops[0].id));
    assert_eq!(row.source_rel_path, "A/01.wav");
    assert_eq!(row.eligible_after - row.archived_at, 30 * 86_400);

    // 次のスキャンで差分が出ない（DB がファイルと一致している）
    let report = lib.scan().await;
    assert_eq!(report.new, 0);
    assert_eq!(report.moved, 0);
    assert_eq!(report.missing_marked, 0);
    assert_eq!(lib.track_count(), 1);
    let again = lib.row(a);
    assert_eq!(again.tag_version, after.tag_version);
    assert_eq!(again.audio_version, after.audio_version);
}

#[tokio::test]
async fn alac_tags_and_picture_are_transferred() {
    require_tools!();
    let lib = Lib::new();
    let m4a = lib.add("A/01.m4a", 5, "ALAC 曲");
    common::retag(&m4a, |tag| {
        use lofty::tag::{Accessor, ItemKey};
        tag.set_genre("J-Pop".to_owned());
        tag.insert_text(ItemKey::Comment, "メモ".to_owned());
        let pic = lofty::picture::Picture::unchecked(b"\xff\xd8\xff\xe0fakejpeg".to_vec())
            .pic_type(lofty::picture::PictureType::CoverFront)
            .mime_type(lofty::picture::MimeType::Jpeg)
            .build();
        tag.push_picture(pic);
    });
    lib.scan().await;
    let a = lib.track_id("A/01.m4a");
    let before = lib.row(a);
    assert_eq!(before.codec, "alac");

    lib.start();
    let prepared = lib.normalize(&[a]).await.unwrap();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );
    let after = lib.row(a);
    assert_eq!(after.rel_path, "A/01.flac");
    assert_eq!(after.codec, "flac");
    assert_eq!(after.audio_md5, before.audio_md5);
    assert_eq!(after.audio_version, before.audio_version);
    assert_eq!(after.original_codec.as_deref(), Some("alac"));

    let af = read_audio_file(File::open(lib.path("A/01.flac")).unwrap(), Some("flac")).unwrap();
    assert_eq!(af.tags.first("TITLE"), Some("ALAC 曲"));
    assert_eq!(af.tags.first("GENRE"), Some("J-Pop"));
    assert_eq!(af.tags.first("COMMENT"), Some("メモ"));
    assert!(af.tags.first("PICTURE").is_some(), "画像が写っていない");
    // 元と同じタグ集合なら tag_version は動かない
    let src = read_audio_file(File::open(lib.archived("A/01.m4a")).unwrap(), Some("m4a")).unwrap();
    if src.tags == af.tags {
        assert_eq!(after.tag_version, before.tag_version);
    } else {
        assert_eq!(after.tag_version, before.tag_version + 1);
    }
    assert_eq!(
        lib.archived_row("A/01.m4a").unwrap().reason,
        ArchiveReason::Normalize
    );
}

#[tokio::test]
async fn md5_mismatch_aborts_and_keeps_original() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    let original_bytes = std::fs::read(&wav).unwrap();
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    // 音量を変えるデコーダ（ffmpeg のラッパ）。出力の PCM が元と一致しなくなる
    let fake = lib.dir.path().join("fake-ffmpeg.py");
    std::fs::write(
        &fake,
        "#!/usr/bin/env python3\nimport os, sys\nargs = sys.argv[1:]\nargs[-1:-1] = ['-af', 'volume=0.5']\nos.execvp('ffmpeg', ['ffmpeg'] + args)\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let lib = lib.reopen_with_ffmpeg(&fake);
    lib.start();
    let prepared = lib.normalize(&[a]).await.unwrap();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Failed
    );
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(ops[0].result, OpResult::Failed);
    assert!(
        ops[0].error.as_deref().unwrap_or("").contains("MD5"),
        "{:?}",
        ops[0].error
    );
    // 元ファイルは無傷、FLAC は残らない、Archive にも台帳にも何も無い
    assert_eq!(std::fs::read(&wav).unwrap(), original_bytes);
    assert!(!lib.path("A/01.flac").exists());
    assert!(!lib.archived("A/01.wav").exists());
    assert!(lib.archived_row("A/01.wav").is_none());
    assert!(lib.tmp_leftovers().is_empty(), "{:?}", lib.tmp_leftovers());
    let row = lib.row(a);
    assert_eq!(row.rel_path, "A/01.wav");
    assert_eq!(row.codec, "wav");
    // 失敗した op は終端なので、同じトラックを再度記録できる
    assert!(lib.normalize(&[a]).await.is_ok());
}

#[tokio::test]
async fn precondition_mismatch_is_conflict_and_touches_nothing() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    // 記録の後・反映の前に外部がタグを書き換えた
    common::retag(&wav, |tag| {
        use lofty::tag::Accessor;
        tag.set_title("外部で変更".to_owned());
    });
    let bytes = std::fs::read(&wav).unwrap();
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Failed
    );
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(ops[0].result, OpResult::SkippedConflict);
    assert!(ops[0].error.as_deref().unwrap().contains("事前条件不一致"));
    assert_eq!(std::fs::read(&wav).unwrap(), bytes);
    assert!(!lib.path("A/01.flac").exists());
    assert!(!lib.archived("A/01.wav").exists());
    // DB はファイルの現在値に揃う
    let row = lib.row(a);
    assert_eq!(row.rel_path, "A/01.wav");
    assert_eq!(row.title.as_deref(), Some("外部で変更"));
}

#[tokio::test]
async fn cancel_before_start_closes_ops() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    let outcome = lib.editor.cancel_batch(prepared.batch_id).await.unwrap();
    assert!(matches!(
        outcome,
        CancelOutcome::Cancelled {
            ops_cancelled: 1,
            running: 0
        }
    ));
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Cancelled);
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(ops[0].result, OpResult::Failed);
    assert_eq!(ops[0].error.as_deref(), Some("cancelled"));
    assert!(wav.exists());
    // タグに rel_path / codec のような偽キーが混ざっていない
    let tags = history::load_track_tags(&lib.conn(), a).unwrap();
    assert!(tags.first("REL_PATH").is_none());
    assert!(tags.first("CODEC").is_none());
    let row = lib.row(a);
    assert_eq!(row.rel_path, "A/01.wav");
    lib.start();
    assert_eq!(
        lib.wait_job_terminal(prepared.job_ids[0]).await,
        JobState::Cancelled
    );
}

// ---------------------------------------------------------------- 巻き戻し

#[tokio::test]
async fn revert_restores_original_and_archives_flac() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    let original_bytes = std::fs::read(&wav).unwrap();
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let before = lib.row(a);
    lib.start();
    let prepared = lib.normalize(&[a]).await.unwrap();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );
    let flac_bytes = std::fs::read(lib.path("A/01.flac")).unwrap();

    // 巻き戻し: 逆バッチ（kind=archive、rel_path 新→旧）が記録され、ジョブが戻す
    let reverse = lib
        .editor
        .revert_batch(prepared.batch_id, Some("戻す"))
        .await
        .unwrap();
    assert_eq!(reverse.affected, 1);
    assert_eq!(reverse.conflict, 0);
    assert_eq!(
        lib.wait_batch_terminal(reverse.batch_id).await,
        BatchState::Applied
    );
    let ops = lib.ops(reverse.batch_id);
    assert_eq!(ops[0].kind, OpKind::Archive);
    assert_eq!(ops[0].result, OpResult::Applied, "{:?}", ops[0].error);

    // Library: 元 WAV（Archive からのコピー）だけ。FLAC は Archive へ
    assert_eq!(std::fs::read(&wav).unwrap(), original_bytes);
    assert!(!lib.path("A/01.flac").exists());
    assert_eq!(
        std::fs::read(lib.archived("A/01.flac")).unwrap(),
        flac_bytes
    );
    // Archive は追記のみ: 退避した WAV は消えない（台帳は restored）
    assert_eq!(
        std::fs::read(lib.archived("A/01.wav")).unwrap(),
        original_bytes
    );
    let wav_row = lib.archived_row("A/01.wav").unwrap();
    assert_eq!(wav_row.state, ArchiveState::Restored);
    let flac_row = lib.archived_row("A/01.flac").unwrap();
    assert_eq!(flac_row.state, ArchiveState::Held);
    assert_eq!(flac_row.reason, ArchiveReason::Restore);
    assert_eq!(flac_row.op_id, Some(ops[0].id));

    // DB: 元の実体へ
    let row = lib.row(a);
    assert_eq!(row.rel_path, "A/01.wav");
    assert_eq!(row.codec, "wav");
    assert_eq!(row.audio_md5, before.audio_md5);
    assert_eq!(row.audio_version, before.audio_version);
    assert_eq!(row.inode, inode_of(&wav));
    assert_eq!(row.original_codec, None);
    assert_eq!(row.normalized_at, None);
    let orig = history::get_batch(&lib.conn(), prepared.batch_id)
        .unwrap()
        .unwrap();
    assert!(orig.reverted_at.is_some());
    // 二重 revert は拒否
    assert!(matches!(
        lib.editor.revert_batch(prepared.batch_id, None).await,
        Err(spindle::edit::RevertError::AlreadyReverted)
    ));

    // やり直し（逆バッチの revert）: FLAC が Library へ戻り、WAV は再び held
    let redo = lib
        .editor
        .revert_batch(reverse.batch_id, None)
        .await
        .unwrap();
    assert_eq!(
        lib.wait_batch_terminal(redo.batch_id).await,
        BatchState::Applied
    );
    assert!(!lib.path("A/01.wav").exists());
    assert_eq!(std::fs::read(lib.path("A/01.flac")).unwrap(), flac_bytes);
    assert_eq!(
        lib.archived_row("A/01.wav").unwrap().state,
        ArchiveState::Held
    );
    assert_eq!(
        lib.archived_row("A/01.flac").unwrap().state,
        ArchiveState::Restored
    );
    let row = lib.row(a);
    assert_eq!(row.rel_path, "A/01.flac");
    assert_eq!(row.codec, "flac");
    assert_eq!(row.original_codec.as_deref(), Some("wav"));
    assert!(lib.tmp_leftovers().is_empty(), "{:?}", lib.tmp_leftovers());
    let report = lib.scan().await;
    assert_eq!((report.new, report.moved, report.missing_marked), (0, 0, 0));
}

#[tokio::test]
async fn revert_conflicts_when_archived_file_is_gone() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    lib.start();
    let prepared = lib.normalize(&[a]).await.unwrap();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );
    // GC が回収した（台帳 deleted）状態
    let row = lib.archived_row("A/01.wav").unwrap();
    archive::set_state(&lib.conn(), row.id, ArchiveState::Deleted, 1).unwrap();
    let reverse = lib
        .editor
        .revert_batch(prepared.batch_id, None)
        .await
        .unwrap();
    assert_eq!(reverse.conflict, 1);
    assert_eq!(lib.batch_state(reverse.batch_id), BatchState::Failed);
    assert!(lib.path("A/01.flac").exists());
    assert_eq!(lib.row(a).rel_path, "A/01.flac");
}

// ---------------------------------------------------------------- クラッシュ復旧

#[tokio::test]
async fn resumes_after_crash_between_placement_and_archive() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    // 配置まで済んだ直後に落ちた状態を作る: 正しい FLAC が Library に既にある
    let enc = FlacEncoder::new("ffmpeg", "flac", 8, lib.dir.path().join("tmp"));
    let out = enc
        .encode(File::open(&wav).unwrap(), 16, &CancellationToken::new())
        .await
        .unwrap();
    std::fs::copy(out.guard.path(), lib.path("A/01.flac")).unwrap();
    drop(out);

    let lib = lib.reopen();
    lib.editor.recover().await.unwrap();
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );
    assert!(!lib.path("A/01.wav").exists());
    assert!(lib.path("A/01.flac").exists());
    assert!(lib.archived("A/01.wav").exists());
    let row = lib.row(a);
    assert_eq!(row.rel_path, "A/01.flac");
    assert_eq!(row.inode, inode_of(&lib.path("A/01.flac")));
}

#[tokio::test]
async fn resumes_after_crash_before_db_commit() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    // 配置・退避・unlink まで済み、DB 確定の前に落ちた状態
    let enc = FlacEncoder::new("ffmpeg", "flac", 8, lib.dir.path().join("tmp"));
    let out = enc
        .encode(File::open(&wav).unwrap(), 16, &CancellationToken::new())
        .await
        .unwrap();
    std::fs::copy(out.guard.path(), lib.path("A/01.flac")).unwrap();
    drop(out);
    std::fs::create_dir_all(lib.archived("A")).unwrap();
    lib.record_source_hash(prepared.batch_id, &std::fs::read(&wav).unwrap());
    std::fs::rename(&wav, lib.archived("A/01.wav")).unwrap();

    let lib = lib.reopen();
    lib.editor.recover().await.unwrap();
    lib.start();
    let state = lib.wait_batch_terminal(prepared.batch_id).await;
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(ops[0].result, OpResult::Applied, "{:?}", ops[0].error);
    assert_eq!(state, BatchState::Applied);
    let row = lib.row(a);
    assert_eq!(row.rel_path, "A/01.flac");
    assert_eq!(row.codec, "flac");
    assert_eq!(row.original_codec.as_deref(), Some("wav"));
    assert_eq!(
        lib.archived_row("A/01.wav").unwrap().state,
        ArchiveState::Held
    );
}

#[tokio::test]
async fn foreign_file_at_destination_is_conflict() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    // 記録の後に外部が別の FLAC を宛先に置いた（スキャン前）
    common::make_audio(&lib.path("A"), "01.flac", "flac", 9).unwrap();
    let foreign = std::fs::read(lib.path("A/01.flac")).unwrap();
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Failed
    );
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(
        ops[0].result,
        OpResult::SkippedConflict,
        "{:?}",
        ops[0].error
    );
    assert!(wav.exists());
    assert_eq!(std::fs::read(lib.path("A/01.flac")).unwrap(), foreign);
    assert!(!lib.archived("A/01.wav").exists());
}

// ---------------------------------------------------------------- スキャンとの並走

#[tokio::test]
async fn scan_during_pending_op_does_not_register_destination() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    // ジョブが配置まで済んだ瞬間（元と宛先が両方ある）にスキャンが走った
    let enc = FlacEncoder::new("ffmpeg", "flac", 8, lib.dir.path().join("tmp"));
    let out = enc
        .encode(File::open(&wav).unwrap(), 16, &CancellationToken::new())
        .await
        .unwrap();
    std::fs::copy(out.guard.path(), lib.path("A/01.flac")).unwrap();
    drop(out);
    let report = lib.scan().await;
    assert_eq!(report.new, 0, "宛先を新規トラックにしてはいけない");
    assert_eq!(lib.track_count(), 1);
    assert_eq!(lib.row(a).missing_since, None);
    // unlink まで済んだ瞬間（宛先だけ）でも同じ。track は missing にならない
    std::fs::create_dir_all(lib.archived("A")).unwrap();
    lib.record_source_hash(prepared.batch_id, &std::fs::read(&wav).unwrap());
    std::fs::rename(&wav, lib.archived("A/01.wav")).unwrap();
    let report = lib.scan().await;
    assert_eq!(report.new, 0);
    assert_eq!(report.moved, 0);
    assert_eq!(report.missing_marked, 0);
    assert_eq!(lib.row(a).rel_path, "A/01.wav");
    assert_eq!(lib.row(a).missing_since, None);
    // ジョブが続きを行い、確定する
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(lib.row(a).rel_path, "A/01.flac");
    let report = lib.scan().await;
    assert_eq!((report.new, report.moved, report.missing_marked), (0, 0, 0));
    assert_eq!(lib.track_count(), 1);
}

#[tokio::test]
async fn external_move_during_pending_op_is_conflict() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    std::fs::create_dir_all(lib.path("B")).unwrap();
    std::fs::rename(&wav, lib.path("B/01.wav")).unwrap();
    let report = lib.scan().await;
    assert_eq!(report.moved, 1);
    assert_eq!(lib.row(a).rel_path, "B/01.wav");
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Failed
    );
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(
        ops[0].result,
        OpResult::SkippedConflict,
        "{:?}",
        ops[0].error
    );
    assert!(lib.path("B/01.wav").exists());
    assert!(!lib.path("B/01.flac").exists());
    // 終端になったので改めて（新しいパスで）正規化できる
    let again = lib.normalize(&[a]).await.unwrap();
    assert_eq!(
        lib.wait_batch_terminal(again.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(lib.row(a).rel_path, "B/01.flac");
}

#[tokio::test]
async fn pending_count_is_visible_in_tracks_row() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    let pending: Option<i64> = lib
        .conn()
        .query_row(
            "SELECT batch_id FROM edit_ops WHERE track_id = ?1 AND result = 'pending'",
            [a],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    assert_eq!(pending, Some(prepared.batch_id));
}

// ---------------------------------------------------------------- 破壊フェーズの保全

/// 元ファイルの一時名（退避後）に in-place で書き込む。同じ inode なので FD からも見える
fn corrupt_in_place(path: &Path) {
    use std::io::{Seek as _, SeekFrom, Write as _};
    let mut f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.seek(SeekFrom::Start(100)).unwrap();
    f.write_all(b"XXXXXXXX").unwrap();
    f.sync_all().unwrap();
}

#[tokio::test]
async fn in_place_update_during_archive_copy_is_conflict_and_keeps_data() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    // 退避（一時名へ rename）の直後、Archive コピーの前に同じ inode を外部が更新する
    let tmp = lib.temp_path(prepared.batch_id, "A/01.wav");
    let hook_tmp = tmp.clone();
    lib.editor.set_normalize_hook(Arc::new(move |step| {
        if let NormalizeStep::BeforeUnlink(_) = step {
            corrupt_in_place(&hook_tmp);
        }
        Ok(())
    }));
    lib.start();
    let state = lib.wait_batch_terminal(prepared.batch_id).await;
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(
        ops[0].result,
        OpResult::SkippedConflict,
        "{:?}",
        ops[0].error
    );
    assert!(ops[0].error.as_deref().unwrap().contains("退避の間"));
    assert_eq!(state, BatchState::Failed);
    // 更新後の内容が元パスに戻っている。自分の生成物（FLAC / Archive のコピー）は無い
    let now = std::fs::read(&wav).unwrap();
    assert_eq!(&now[100..108], b"XXXXXXXX");
    assert!(!tmp.exists());
    assert!(!lib.path("A/01.flac").exists());
    assert!(!lib.archived("A/01.wav").exists());
    assert!(lib.archived_row("A/01.wav").is_none());
    assert_eq!(lib.row(a).rel_path, "A/01.wav");
    assert!(lib.tmp_leftovers().is_empty(), "{:?}", lib.tmp_leftovers());
}

#[tokio::test]
async fn in_place_update_before_archive_copy_is_conflict() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    let original = std::fs::read(&wav).unwrap();
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    let tmp = lib.temp_path(prepared.batch_id, "A/01.wav");
    let hook_tmp = tmp.clone();
    lib.editor.set_normalize_hook(Arc::new(move |step| {
        if let NormalizeStep::Staged(_) = step {
            corrupt_in_place(&hook_tmp);
        }
        Ok(())
    }));
    lib.start();
    lib.wait_batch_terminal(prepared.batch_id).await;
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(
        ops[0].result,
        OpResult::SkippedConflict,
        "{:?}",
        ops[0].error
    );
    let now = std::fs::read(&wav).unwrap();
    assert_ne!(now, original);
    assert_eq!(&now[100..108], b"XXXXXXXX");
    assert!(!lib.path("A/01.flac").exists());
    assert!(!lib.archived("A/01.wav").exists());
}

#[tokio::test]
async fn path_replaced_during_archive_copy_keeps_both_files() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    let original = std::fs::read(&wav).unwrap();
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    // 退避の後に外部が元パスへ別のファイルを置く（tmp + rename の模擬）
    let foreign_path = wav.clone();
    lib.editor.set_normalize_hook(Arc::new(move |step| {
        if let NormalizeStep::Staged(_) = step {
            common::write_wav(&foreign_path, &common::pcm_samples(77), 16);
        }
        Ok(())
    }));
    lib.start();
    let state = lib.wait_batch_terminal(prepared.batch_id).await;
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(ops[0].result, OpResult::Applied, "{:?}", ops[0].error);
    assert_eq!(state, BatchState::Applied);
    // 自分の元は Archive へ、FLAC は Library へ。外部が置いたファイルは触らない
    assert_eq!(std::fs::read(lib.archived("A/01.wav")).unwrap(), original);
    assert!(lib.path("A/01.flac").exists());
    assert!(wav.exists());
    assert_ne!(std::fs::read(&wav).unwrap(), original);
    assert_eq!(lib.row(a).rel_path, "A/01.flac");
    // 次のスキャンで外部のファイルは新規トラックになる
    let report = lib.scan().await;
    assert_eq!(report.new, 1);
    assert_eq!(lib.track_count(), 2);
}

#[tokio::test]
async fn archive_name_clash_fails_before_touching_library() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    let original = std::fs::read(&wav).unwrap();
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    // Archive の同じパスに別の内容がある
    std::fs::create_dir_all(lib.archived("A")).unwrap();
    common::write_wav(&lib.archived("A/01.wav"), &common::pcm_samples(50), 16);
    let clash = std::fs::read(lib.archived("A/01.wav")).unwrap();
    lib.start();
    let prepared = lib.normalize(&[a]).await.unwrap();
    lib.wait_batch_terminal(prepared.batch_id).await;
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(ops[0].result, OpResult::Failed, "{:?}", ops[0].error);
    assert!(ops[0].error.as_deref().unwrap().contains("Archive"));
    assert_eq!(std::fs::read(&wav).unwrap(), original);
    assert!(!lib.path("A/01.flac").exists());
    assert_eq!(std::fs::read(lib.archived("A/01.wav")).unwrap(), clash);
    assert!(lib.archived_row("A/01.wav").is_none());
    assert_eq!(lib.row(a).rel_path, "A/01.wav");
}

#[tokio::test]
async fn io_failure_after_placement_restores_and_retries() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    let original = std::fs::read(&wav).unwrap();
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    let op_id = lib.ops(prepared.batch_id)[0].id;
    // Archive へのコピーで I/O 失敗（フックの Err で模擬）
    lib.editor.set_normalize_hook(Arc::new(|step| match step {
        NormalizeStep::Staged(_) => Err("Archive を書けない".to_owned()),
        _ => Ok(()),
    }));
    let err = lib
        .editor
        .apply_archive_op(op_id, None, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Archive を書けない"), "{err}");
    // 再試行できる状態: op は pending のまま、元は一時名に無傷（戻す rename は ctime を進めるので
    // 元パスへは戻さない）、生成物は無い
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(ops[0].result, OpResult::Pending);
    let tmp = lib.temp_path(prepared.batch_id, "A/01.wav");
    assert_eq!(std::fs::read(&tmp).unwrap(), original);
    assert!(!wav.exists());
    assert!(!lib.path("A/01.flac").exists());
    assert!(!lib.archived("A/01.wav").exists());
    // スキャンは一時名を作業中として扱う
    let report = lib.scan().await;
    assert_eq!((report.new, report.missing_marked), (0, 0));
    // 障害が直れば同じ op が完了する
    lib.editor.set_normalize_hook(Arc::new(|_| Ok(())));
    let outcome = lib
        .editor
        .apply_archive_op(op_id, None, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(outcome, spindle::edit::OpOutcome::Applied);
    assert!(!wav.exists());
    assert!(lib.path("A/01.flac").exists());
    assert_eq!(std::fs::read(lib.archived("A/01.wav")).unwrap(), original);
}

#[tokio::test]
async fn already_done_with_corrupted_archive_is_conflict() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    // unlink の後・DB 確定の前に落ち、さらに Archive のコピーが壊れている
    let enc = FlacEncoder::new("ffmpeg", "flac", 8, lib.dir.path().join("tmp"));
    let out = enc
        .encode(File::open(&wav).unwrap(), 16, &CancellationToken::new())
        .await
        .unwrap();
    std::fs::copy(out.guard.path(), lib.path("A/01.flac")).unwrap();
    drop(out);
    std::fs::create_dir_all(lib.archived("A")).unwrap();
    lib.record_source_hash(prepared.batch_id, &std::fs::read(&wav).unwrap());
    std::fs::rename(&wav, lib.archived("A/01.wav")).unwrap();
    corrupt_in_place(&lib.archived("A/01.wav"));
    let corrupted = std::fs::read(lib.archived("A/01.wav")).unwrap();

    let lib = lib.reopen();
    lib.editor.recover().await.unwrap();
    lib.start();
    lib.wait_batch_terminal(prepared.batch_id).await;
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(
        ops[0].result,
        OpResult::SkippedConflict,
        "{:?}",
        ops[0].error
    );
    assert!(ops[0].error.as_deref().unwrap().contains("照合できない"));
    // 何も消さない: 宛先 FLAC も Archive の実体もそのまま。台帳にも載せない
    assert!(lib.path("A/01.flac").exists());
    assert_eq!(std::fs::read(lib.archived("A/01.wav")).unwrap(), corrupted);
    assert!(lib.archived_row("A/01.wav").is_none());
    assert_eq!(lib.row(a).rel_path, "A/01.wav");
}

#[tokio::test]
async fn already_done_without_recorded_hash_is_conflict() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    let enc = FlacEncoder::new("ffmpeg", "flac", 8, lib.dir.path().join("tmp"));
    let out = enc
        .encode(File::open(&wav).unwrap(), 16, &CancellationToken::new())
        .await
        .unwrap();
    std::fs::copy(out.guard.path(), lib.path("A/01.flac")).unwrap();
    drop(out);
    std::fs::create_dir_all(lib.archived("A")).unwrap();
    std::fs::rename(&wav, lib.archived("A/01.wav")).unwrap();
    lib.start();
    lib.wait_batch_terminal(prepared.batch_id).await;
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(
        ops[0].result,
        OpResult::SkippedConflict,
        "{:?}",
        ops[0].error
    );
    assert!(lib.path("A/01.flac").exists());
    assert!(lib.archived("A/01.wav").exists());
}

#[tokio::test]
async fn resumes_from_staged_temp_name() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    let original = std::fs::read(&wav).unwrap();
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    // 宛先を置き、元を一時名へ退避した直後に落ちた状態
    let enc = FlacEncoder::new("ffmpeg", "flac", 8, lib.dir.path().join("tmp"));
    let out = enc
        .encode(File::open(&wav).unwrap(), 16, &CancellationToken::new())
        .await
        .unwrap();
    std::fs::copy(out.guard.path(), lib.path("A/01.flac")).unwrap();
    drop(out);
    let tmp = lib.temp_path(prepared.batch_id, "A/01.wav");
    std::fs::rename(&wav, &tmp).unwrap();
    // スキャンは一時名を作業中として扱う（新規登録も missing もしない）
    let report = lib.scan().await;
    assert_eq!((report.new, report.missing_marked), (0, 0));
    assert_eq!(lib.track_count(), 1);

    let lib = lib.reopen();
    lib.editor.recover().await.unwrap();
    lib.start();
    let state = lib.wait_batch_terminal(prepared.batch_id).await;
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(ops[0].result, OpResult::Applied, "{:?}", ops[0].error);
    assert_eq!(state, BatchState::Applied);
    assert!(!tmp.exists());
    assert!(!wav.exists());
    assert_eq!(std::fs::read(lib.archived("A/01.wav")).unwrap(), original);
    assert_eq!(lib.row(a).rel_path, "A/01.flac");
}

// ---------------------------------------------------------------- スキャンに追い越されない

#[tokio::test]
async fn scan_overtaken_by_normalize_commit_does_not_abort() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    let op_id = lib.ops(prepared.batch_id)[0].id;
    // ジョブが配置まで済んだ瞬間に inventory を取り、Phase 3 に入る前にジョブが DB を確定する
    let enc = FlacEncoder::new("ffmpeg", "flac", 8, lib.dir.path().join("tmp"));
    let out = enc
        .encode(File::open(&wav).unwrap(), 16, &CancellationToken::new())
        .await
        .unwrap();
    std::fs::copy(out.guard.path(), lib.path("A/01.flac")).unwrap();
    drop(out);
    let editor = lib.editor.clone();
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fired2 = fired.clone();
    let progress: spindle::import::scanner::Progress = Arc::new(move |done, _| {
        if done == 0 && !fired2.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let editor = editor.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async move {
                    editor
                        .apply_archive_op(op_id, None, &CancellationToken::new())
                        .await
                        .unwrap()
                })
            })
            .join()
            .unwrap();
        }
    });
    let report = lib
        .scanner
        .run(ScanKind::Incremental, progress, CancellationToken::new())
        .await
        .unwrap();
    assert!(fired.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(report.new, 0, "追い越された宛先を新規登録してはいけない");
    assert_eq!(report.missing_marked, 0);
    assert_eq!(lib.track_count(), 1);
    let row = lib.row(a);
    assert_eq!(row.rel_path, "A/01.flac");
    assert_eq!(row.missing_since, None);
    assert_eq!(lib.ops(prepared.batch_id)[0].result, OpResult::Applied);
    // 次のスキャンで差分が出ない
    let report = lib.scan().await;
    assert_eq!((report.new, report.moved, report.missing_marked), (0, 0, 0));
}

// ---------------------------------------------------------------- AIFF

#[tokio::test]
async fn aiff_becomes_flac_and_reverts() {
    require_tools!();
    let lib = Lib::new();
    let aiff = lib.add("A/01.aiff", 4, "AIFF 曲");
    let original = std::fs::read(&aiff).unwrap();
    lib.scan().await;
    let a = lib.track_id("A/01.aiff");
    let before = lib.row(a);
    assert_eq!(before.codec, "aiff");
    assert_eq!(before.audio_md5.as_deref(), Some(md5_of(&aiff).as_slice()));

    lib.start();
    let prepared = lib.normalize(&[a]).await.unwrap();
    let state = lib.wait_batch_terminal(prepared.batch_id).await;
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(ops[0].result, OpResult::Applied, "{:?}", ops[0].error);
    assert_eq!(state, BatchState::Applied);
    let after = lib.row(a);
    assert_eq!(after.rel_path, "A/01.flac");
    assert_eq!(after.codec, "flac");
    assert_eq!(after.audio_md5, before.audio_md5);
    assert_eq!(after.audio_version, before.audio_version);
    assert_eq!(after.original_codec.as_deref(), Some("aiff"));
    assert_eq!(
        md5_of(&lib.path("A/01.flac")),
        md5_of(&lib.archived("A/01.aiff"))
    );
    let af = read_audio_file(File::open(lib.path("A/01.flac")).unwrap(), Some("flac")).unwrap();
    assert_eq!(af.tags.first("TITLE"), Some("AIFF 曲"));
    assert_eq!(af.tags.first("ALBUMARTIST"), Some("AlbumArtist"));
    assert_eq!(std::fs::read(lib.archived("A/01.aiff")).unwrap(), original);

    let reverse = lib
        .editor
        .revert_batch(prepared.batch_id, None)
        .await
        .unwrap();
    assert_eq!(
        lib.wait_batch_terminal(reverse.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(std::fs::read(&aiff).unwrap(), original);
    assert!(!lib.path("A/01.flac").exists());
    assert!(lib.archived("A/01.flac").exists());
    let row = lib.row(a);
    assert_eq!(row.rel_path, "A/01.aiff");
    assert_eq!(row.codec, "aiff");
    assert_eq!(row.original_codec, None);
}

#[tokio::test]
async fn closing_failed_op_restores_staged_source() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    let original = std::fs::read(&wav).unwrap();
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    let op_id = lib.ops(prepared.batch_id)[0].id;
    lib.editor.set_normalize_hook(Arc::new(|step| match step {
        NormalizeStep::Staged(_) => Err("Archive を書けない".to_owned()),
        _ => Ok(()),
    }));
    lib.editor
        .apply_archive_op(op_id, None, &CancellationToken::new())
        .await
        .unwrap_err();
    let tmp = lib.temp_path(prepared.batch_id, "A/01.wav");
    assert!(tmp.exists());
    // 最終試行の失敗（またはキャンセル）で op を閉じると、元パスへ戻る
    assert!(lib.editor.close_op(op_id, "最終失敗", None).await.unwrap());
    assert!(!tmp.exists());
    assert_eq!(std::fs::read(&wav).unwrap(), original);
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Failed);
    // 閉じた後のスキャンで差分が出ない（ctime だけ進んでいるので物理属性の更新のみ）
    let report = lib.scan().await;
    assert_eq!((report.new, report.moved, report.missing_marked), (0, 0, 0));
    assert_eq!(lib.row(a).rel_path, "A/01.wav");
}

// ---------------------------------------------------------------- 確定点の後と、外部の差し替え

#[tokio::test]
async fn failure_after_unlink_keeps_flac_and_archive_and_resumes() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    let original = std::fs::read(&wav).unwrap();
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    let op_id = lib.ops(prepared.batch_id)[0].id;
    // 確定点（unlink）の直後に失敗（fsync 失敗の模擬）
    lib.editor.set_normalize_hook(Arc::new(|step| match step {
        NormalizeStep::AfterUnlink(_) => Err("fsync に失敗".to_owned()),
        _ => Ok(()),
    }));
    let err = lib
        .editor
        .apply_archive_op(op_id, None, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("fsync"), "{err}");
    // 元は消えているが、宛先 FLAC と Archive のコピーは残っている（undo しない）
    assert!(!wav.exists());
    assert!(!lib.temp_path(prepared.batch_id, "A/01.wav").exists());
    assert!(lib.path("A/01.flac").exists());
    assert_eq!(std::fs::read(lib.archived("A/01.wav")).unwrap(), original);
    assert_eq!(lib.ops(prepared.batch_id)[0].result, OpResult::Pending);
    // 再試行は反映済みとして確定する
    lib.editor.set_normalize_hook(Arc::new(|_| Ok(())));
    let outcome = lib
        .editor
        .apply_archive_op(op_id, None, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(outcome, spindle::edit::OpOutcome::Applied);
    let row = lib.row(a);
    assert_eq!(row.rel_path, "A/01.flac");
    assert_eq!(row.codec, "flac");
    assert_eq!(
        lib.archived_row("A/01.wav").unwrap().state,
        ArchiveState::Held
    );
    assert!(lib.tmp_leftovers().is_empty(), "{:?}", lib.tmp_leftovers());
}

/// `path` を別の WAV で置き換える（tmp + rename。外部ツールの保存の模擬）
fn replace_with_foreign(path: &Path, seed: u32) -> Vec<u8> {
    let tmp = path.with_extension("foreign.tmp");
    common::write_wav(&tmp, &common::pcm_samples(seed), 16);
    std::fs::rename(&tmp, path).unwrap();
    std::fs::read(path).unwrap()
}

#[tokio::test]
async fn undo_does_not_delete_replaced_destination() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    let tmp = lib.temp_path(prepared.batch_id, "A/01.wav");
    let dest = lib.path("A/01.flac");
    let foreign = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (hook_tmp, hook_dest, hook_foreign) = (tmp.clone(), dest.clone(), foreign.clone());
    lib.editor.set_normalize_hook(Arc::new(move |step| {
        if let NormalizeStep::BeforeUnlink(_) = step {
            // 外部が宛先を差し替え、さらに元を更新した（conflict になる）
            *hook_foreign.lock().unwrap() = replace_with_foreign(&hook_dest, 91);
            corrupt_in_place(&hook_tmp);
        }
        Ok(())
    }));
    lib.start();
    lib.wait_batch_terminal(prepared.batch_id).await;
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(
        ops[0].result,
        OpResult::SkippedConflict,
        "{:?}",
        ops[0].error
    );
    // 外部が置いた宛先は消さない。元は元パスへ戻る。Archive の自分のコピーは消す
    assert_eq!(std::fs::read(&dest).unwrap(), *foreign.lock().unwrap());
    assert!(wav.exists());
    assert!(!tmp.exists());
    assert!(!lib.archived("A/01.wav").exists());
}

#[tokio::test]
async fn undo_does_not_delete_replaced_archive_copy() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    let tmp = lib.temp_path(prepared.batch_id, "A/01.wav");
    let archived = lib.archived("A/01.wav");
    let foreign = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (hook_tmp, hook_arch, hook_foreign) = (tmp.clone(), archived.clone(), foreign.clone());
    lib.editor.set_normalize_hook(Arc::new(move |step| {
        if let NormalizeStep::BeforeUnlink(_) = step {
            *hook_foreign.lock().unwrap() = replace_with_foreign(&hook_arch, 92);
            corrupt_in_place(&hook_tmp);
        }
        Ok(())
    }));
    lib.start();
    lib.wait_batch_terminal(prepared.batch_id).await;
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(
        ops[0].result,
        OpResult::SkippedConflict,
        "{:?}",
        ops[0].error
    );
    assert_eq!(std::fs::read(&archived).unwrap(), *foreign.lock().unwrap());
    assert!(wav.exists());
    assert!(!lib.path("A/01.flac").exists());
    assert!(lib.archived_row("A/01.wav").is_none());
}

#[tokio::test]
async fn replaced_staged_temp_is_conflict_and_source_is_written_back() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    let original = std::fs::read(&wav).unwrap();
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    let tmp = lib.temp_path(prepared.batch_id, "A/01.wav");
    let foreign = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (hook_tmp, hook_foreign) = (tmp.clone(), foreign.clone());
    lib.editor.set_normalize_hook(Arc::new(move |step| {
        if let NormalizeStep::BeforeUnlink(_) = step {
            // 外部が一時名の上に別のファイルを rename した（自分の inode はパスを失う）
            *hook_foreign.lock().unwrap() = replace_with_foreign(&hook_tmp, 93);
        }
        Ok(())
    }));
    lib.start();
    lib.wait_batch_terminal(prepared.batch_id).await;
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(
        ops[0].result,
        OpResult::SkippedConflict,
        "{:?}",
        ops[0].error
    );
    // 自分の inode がパスを失う（nlink 0）と ctime が進むので、stat の照合が先に検出する
    assert!(ops[0].error.as_deref().unwrap().contains("退避の間"));
    // 外部のファイルは一時名のまま残し（消さない）、自分の元の内容は元パスへ書き戻される
    assert_eq!(std::fs::read(&tmp).unwrap(), *foreign.lock().unwrap());
    assert_eq!(std::fs::read(&wav).unwrap(), original);
    assert!(!lib.path("A/01.flac").exists());
    assert!(!lib.archived("A/01.wav").exists());
    assert_eq!(lib.row(a).rel_path, "A/01.wav");
}

/// `dir` 直下の回収ファイル（`spindle-recovery-*`）
fn recovery_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("spindle-recovery-"))
        })
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn staged_temp_and_old_path_both_taken_keeps_original_in_recovery_path() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    let original = std::fs::read(&wav).unwrap();
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    let tmp = lib.temp_path(prepared.batch_id, "A/01.wav");
    let foreign_tmp = Arc::new(std::sync::Mutex::new(Vec::new()));
    let foreign_old = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (h_tmp, h_old, h_ft, h_fo) = (
        tmp.clone(),
        wav.clone(),
        foreign_tmp.clone(),
        foreign_old.clone(),
    );
    lib.editor.set_normalize_hook(Arc::new(move |step| {
        if let NormalizeStep::BeforeUnlink(_) = step {
            // 外部が一時名と元パスの両方に別のファイルを置いた
            *h_ft.lock().unwrap() = replace_with_foreign(&h_tmp, 94);
            *h_fo.lock().unwrap() = replace_with_foreign(&h_old, 95);
        }
        Ok(())
    }));
    lib.start();
    lib.wait_batch_terminal(prepared.batch_id).await;
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(
        ops[0].result,
        OpResult::SkippedConflict,
        "{:?}",
        ops[0].error
    );
    assert!(ops[0]
        .error
        .as_deref()
        .unwrap()
        .contains("spindle-recovery-"));
    // 外部の 2 ファイルは無傷、自分の元の内容は回収パスに残る
    assert_eq!(std::fs::read(&tmp).unwrap(), *foreign_tmp.lock().unwrap());
    assert_eq!(std::fs::read(&wav).unwrap(), *foreign_old.lock().unwrap());
    let recovered = recovery_files(&lib.path("A"));
    assert_eq!(recovered.len(), 1, "{recovered:?}");
    assert_eq!(std::fs::read(&recovered[0]).unwrap(), original);
    // 元が durable に残ったので自分の生成物は消す
    assert!(!lib.path("A/01.flac").exists());
    assert!(!lib.archived("A/01.wav").exists());
    // 回収ファイルは次のスキャンで普通のトラックとして見える（消えない）
    let report = lib.scan().await;
    assert!(report.new >= 1);
    assert!(recovered[0].exists());
}

#[tokio::test]
async fn replaced_temp_between_verify_and_unlink_is_conflict() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    let original = std::fs::read(&wav).unwrap();
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    let tmp = lib.temp_path(prepared.batch_id, "A/01.wav");
    let foreign = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (h_tmp, h_f) = (tmp.clone(), foreign.clone());
    lib.editor.set_normalize_hook(Arc::new(move |step| {
        if let NormalizeStep::Verified(_) = step {
            // 最終照合の後・unlink の前に一時名を差し替える
            *h_f.lock().unwrap() = replace_with_foreign(&h_tmp, 96);
        }
        Ok(())
    }));
    lib.start();
    lib.wait_batch_terminal(prepared.batch_id).await;
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(
        ops[0].result,
        OpResult::SkippedConflict,
        "{:?}",
        ops[0].error
    );
    assert!(ops[0].error.as_deref().unwrap().contains("確定の直前"));
    assert_eq!(std::fs::read(&tmp).unwrap(), *foreign.lock().unwrap());
    assert_eq!(std::fs::read(&wav).unwrap(), original);
    assert!(!lib.path("A/01.flac").exists());
    assert!(!lib.archived("A/01.wav").exists());
}

#[tokio::test]
async fn undo_keeps_generated_files_modified_in_place() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    let tmp = lib.temp_path(prepared.batch_id, "A/01.wav");
    let dest = lib.path("A/01.flac");
    let archived = lib.archived("A/01.wav");
    let (h_tmp, h_dest, h_arch) = (tmp.clone(), dest.clone(), archived.clone());
    lib.editor.set_normalize_hook(Arc::new(move |step| {
        if let NormalizeStep::BeforeUnlink(_) = step {
            // 外部が自分の生成物（同じ inode）に in-place で書き、元も更新した（conflict になる）
            corrupt_in_place(&h_dest);
            corrupt_in_place(&h_arch);
            corrupt_in_place(&h_tmp);
        }
        Ok(())
    }));
    lib.start();
    lib.wait_batch_terminal(prepared.batch_id).await;
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(
        ops[0].result,
        OpResult::SkippedConflict,
        "{:?}",
        ops[0].error
    );
    // 置いてから変わった生成物は自分のものと言えないので消さない
    assert!(dest.exists());
    assert!(archived.exists());
    assert!(wav.exists());
    assert!(!tmp.exists());
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[tokio::test]
async fn nothing_is_deleted_when_original_cannot_be_kept_before_archive_copy() {
    require_tools!();
    if rustix::process::geteuid().is_root() {
        eprintln!("root では書き込み禁止ディレクトリを作れないので skip");
        return;
    }
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    let op_id = lib.ops(prepared.batch_id)[0].id;
    let tmp = lib.temp_path(prepared.batch_id, "A/01.wav");
    let dir_a = lib.path("A");
    let foreign_tmp = Arc::new(std::sync::Mutex::new(Vec::new()));
    let foreign_old = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (h_tmp, h_old, h_dir, h_ft, h_fo) = (
        tmp.clone(),
        wav.clone(),
        dir_a.clone(),
        foreign_tmp.clone(),
        foreign_old.clone(),
    );
    // 退避の直後: 外部が一時名と元パスの両方を差し替え、ディレクトリに書けなくなり、
    // さらに Archive へのコピーが（作る前に）失敗する
    lib.editor
        .set_normalize_hook(Arc::new(move |step| match step {
            NormalizeStep::Staged(_) => {
                *h_ft.lock().unwrap() = replace_with_foreign(&h_tmp, 97);
                *h_fo.lock().unwrap() = replace_with_foreign(&h_old, 98);
                set_mode(&h_dir, 0o555);
                Err("Archive に書けない（ENOSPC）".to_owned())
            }
            _ => Ok(()),
        }));
    let err = lib
        .editor
        .apply_archive_op(op_id, None, &CancellationToken::new())
        .await
        .unwrap_err();
    set_mode(&dir_a, 0o755);
    assert!(err.to_string().contains("ENOSPC"), "{err}");
    // 元を残せなかったので、同じ音声を持つ宛先の FLAC を消さない。外部の 2 ファイルも無傷
    assert!(
        lib.path("A/01.flac").exists(),
        "唯一の複製である FLAC が消えた"
    );
    assert_eq!(
        md5_of(&lib.path("A/01.flac")),
        lib.row(a).audio_md5.unwrap()
    );
    assert_eq!(std::fs::read(&tmp).unwrap(), *foreign_tmp.lock().unwrap());
    assert_eq!(std::fs::read(&wav).unwrap(), *foreign_old.lock().unwrap());
    assert!(recovery_files(&dir_a).is_empty());
}

#[tokio::test]
async fn conflict_note_lists_only_copies_that_exist() {
    require_tools!();
    if rustix::process::geteuid().is_root() {
        eprintln!("root では書き込み禁止ディレクトリを作れないので skip");
        return;
    }
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    let original = std::fs::read(&wav).unwrap();
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    let tmp = lib.temp_path(prepared.batch_id, "A/01.wav");
    let dir_a = lib.path("A");
    let (h_tmp, h_old, h_dir) = (tmp.clone(), wav.clone(), dir_a.clone());
    lib.editor.set_normalize_hook(Arc::new(move |step| {
        if let NormalizeStep::BeforeUnlink(_) = step {
            replace_with_foreign(&h_tmp, 97);
            replace_with_foreign(&h_old, 98);
            set_mode(&h_dir, 0o555);
        }
        Ok(())
    }));
    lib.start();
    lib.wait_batch_terminal(prepared.batch_id).await;
    set_mode(&dir_a, 0o755);
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(
        ops[0].result,
        OpResult::SkippedConflict,
        "{:?}",
        ops[0].error
    );
    let error = ops[0].error.clone().unwrap();
    // Archive のコピーは作られているので、FLAC と Archive の両方を複製として残し、そう述べる
    assert!(error.contains("宛先の FLAC"), "{error}");
    assert!(error.contains("Archive のコピー"), "{error}");
    assert!(lib.path("A/01.flac").exists());
    assert_eq!(std::fs::read(lib.archived("A/01.wav")).unwrap(), original);
    assert!(lib.archived_row("A/01.wav").is_none());
}

// ---------------------------------------------------------------- dedup とジョブの相乗り

#[tokio::test]
async fn revert_gets_its_own_job_even_while_previous_job_is_still_running() {
    require_tools!();
    let lib = Lib::new();
    lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    lib.start();
    let prepared = lib.normalize(&[a]).await.unwrap();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );
    let old_job = prepared.job_ids[0];
    lib.wait_job_terminal(old_job).await;
    // 前のジョブがまだ running（ハンドラは返ったが終端のトランザクションが済んでいない）状態を
    // 作る。dedup key がトラック単位だと、この間の revert が前のジョブに相乗りして永遠に動かない
    lib.conn()
        .execute("UPDATE jobs SET state = 'running' WHERE id = ?1", [old_job])
        .unwrap();
    let reverse = lib
        .editor
        .revert_batch(prepared.batch_id, None)
        .await
        .unwrap();
    assert_eq!(reverse.job_ids.len(), 1);
    assert_ne!(
        reverse.job_ids[0], old_job,
        "前のジョブに相乗りしてはいけない"
    );
    let op = &lib.ops(reverse.batch_id)[0];
    assert_eq!(op.job_id, Some(reverse.job_ids[0]));
    lib.conn()
        .execute("UPDATE jobs SET state = 'done' WHERE id = ?1", [old_job])
        .unwrap();
    assert_eq!(
        lib.wait_batch_terminal(reverse.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(lib.row(a).rel_path, "A/01.wav");
}

#[tokio::test]
async fn conflict_note_excludes_copies_replaced_or_corrupted() {
    require_tools!();
    if rustix::process::geteuid().is_root() {
        eprintln!("root では書き込み禁止ディレクトリを作れないので skip");
        return;
    }
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    let tmp = lib.temp_path(prepared.batch_id, "A/01.wav");
    let dir_a = lib.path("A");
    let dest = lib.path("A/01.flac");
    let archived = lib.archived("A/01.wav");
    let foreign_dest = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (h_tmp, h_old, h_dir, h_dest, h_arch, h_fd) = (
        tmp.clone(),
        wav.clone(),
        dir_a.clone(),
        dest.clone(),
        archived.clone(),
        foreign_dest.clone(),
    );
    lib.editor.set_normalize_hook(Arc::new(move |step| {
        if let NormalizeStep::BeforeUnlink(_) = step {
            // 元を残せない状況に加えて、宛先は差し替え、Archive のコピーは同じ inode のまま破損
            replace_with_foreign(&h_tmp, 97);
            replace_with_foreign(&h_old, 98);
            *h_fd.lock().unwrap() = replace_with_foreign(&h_dest, 99);
            corrupt_in_place(&h_arch);
            set_mode(&h_dir, 0o555);
        }
        Ok(())
    }));
    lib.start();
    lib.wait_batch_terminal(prepared.batch_id).await;
    set_mode(&dir_a, 0o755);
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(
        ops[0].result,
        OpResult::SkippedConflict,
        "{:?}",
        ops[0].error
    );
    let error = ops[0].error.clone().unwrap();
    // 実体は消さないが、自分の生成物と言えないものは「複製」として報告しない
    assert!(error.contains("複製も残せなかった"), "{error}");
    assert!(!error.contains("宛先の FLAC"), "{error}");
    assert!(!error.contains("Archive のコピー"), "{error}");
    assert_eq!(std::fs::read(&dest).unwrap(), *foreign_dest.lock().unwrap());
    assert!(archived.exists());
}

#[tokio::test]
async fn undo_never_deletes_file_placed_at_path_after_verification() {
    require_tools!();
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    let original = std::fs::read(&wav).unwrap();
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    let tmp = lib.temp_path(prepared.batch_id, "A/01.wav");
    let dest = lib.path("A/01.flac");
    let archived = lib.archived("A/01.wav");
    let foreign_dest = Arc::new(std::sync::Mutex::new(Vec::new()));
    let foreign_arch = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (h_tmp, h_dest, h_arch, h_fd, h_fa) = (
        tmp.clone(),
        dest.clone(),
        archived.clone(),
        foreign_dest.clone(),
        foreign_arch.clone(),
    );
    lib.editor.set_normalize_hook(Arc::new(move |step| {
        match step {
            // 元を更新して conflict にする
            NormalizeStep::BeforeUnlink(_) => corrupt_in_place(&h_tmp),
            // undo が生成物を照合し終えた直後・消す直前に、外部が同じパスへ別ファイルを置く
            NormalizeStep::UndoVerified(_) => {
                if !h_dest.exists() {
                    *h_fd.lock().unwrap() = replace_with_foreign(&h_dest, 61);
                }
                if !h_arch.exists() {
                    *h_fa.lock().unwrap() = replace_with_foreign(&h_arch, 62);
                }
            }
            _ => {}
        }
        Ok(())
    }));
    lib.start();
    lib.wait_batch_terminal(prepared.batch_id).await;
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(
        ops[0].result,
        OpResult::SkippedConflict,
        "{:?}",
        ops[0].error
    );
    // 照合後に来た外部ファイルは消えていない（自分の生成物だけが消えた）
    assert_eq!(std::fs::read(&dest).unwrap(), *foreign_dest.lock().unwrap());
    assert_eq!(
        std::fs::read(&archived).unwrap(),
        *foreign_arch.lock().unwrap()
    );
    // 元は元パスに戻り、私有名の取り残しも無い
    assert_ne!(std::fs::read(&wav).unwrap(), original);
    assert!(lib.tmp_leftovers().is_empty(), "{:?}", lib.tmp_leftovers());
    let hidden: Vec<_> = std::fs::read_dir(lib.path("A"))
        .unwrap()
        .chain(std::fs::read_dir(lib.archived("A")).unwrap())
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".spindle-tmp-"))
        .collect();
    assert!(hidden.is_empty(), "{hidden:?}");
}

#[tokio::test]
async fn quarantined_foreign_file_is_never_reclaimed_by_scan() {
    require_tools!();
    if rustix::process::geteuid().is_root() {
        eprintln!("root では書き込み禁止ディレクトリを作れないので skip");
        return;
    }
    let lib = Lib::new();
    let wav = lib.add("A/01.wav", 1, "a");
    lib.scan().await;
    let a = lib.track_id("A/01.wav");
    let prepared = lib.normalize(&[a]).await.unwrap();
    let tmp = lib.temp_path(prepared.batch_id, "A/01.wav");
    let dest = lib.path("A/01.flac");
    let dir_a = lib.path("A");
    let foreign = Arc::new(std::sync::Mutex::new(Vec::new()));
    // 宛先（1 件目の remove_if_unchanged）のときだけ仕掛ける。Archive のコピーの分では何もしない
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (h_tmp, h_dest, h_dir, h_f, h_fired) = (
        tmp.clone(),
        dest.clone(),
        dir_a.clone(),
        foreign.clone(),
        fired.clone(),
    );
    lib.editor.set_normalize_hook(Arc::new(move |step| {
        match step {
            NormalizeStep::BeforeUnlink(_) => corrupt_in_place(&h_tmp),
            // stat 確認と隔離の間に外部が宛先を差し替える → 隔離したのは外部ファイル
            NormalizeStep::UndoChecked(_) if !h_fired.load(std::sync::atomic::Ordering::SeqCst) => {
                *h_f.lock().unwrap() = replace_with_foreign(&h_dest, 71);
            }
            // 隔離の直後: 元の場所を別のファイルで塞ぎ、ディレクトリを書けなくして、戻す rename と
            // 回収パスへの rename の両方を失敗させる
            NormalizeStep::UndoQuarantined(_)
                if !h_fired.swap(true, std::sync::atomic::Ordering::SeqCst) =>
            {
                replace_with_foreign(&h_dest, 72);
                set_mode(&h_dir, 0o555);
            }
            _ => {}
        }
        Ok(())
    }));
    lib.start();
    lib.wait_batch_terminal(prepared.batch_id).await;
    set_mode(&dir_a, 0o755);
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(
        ops[0].result,
        OpResult::SkippedConflict,
        "{:?}",
        ops[0].error
    );
    let attempts: i64 = lib
        .conn()
        .query_row(
            "SELECT attempts FROM jobs WHERE id = ?1",
            [prepared.job_ids[0]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(attempts, 0, "再試行なしで conflict に落ちるはず");
    // 隔離名に残った外部ファイルを見つける
    let quarantined: Vec<PathBuf> = std::fs::read_dir(&dir_a)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(spindle::edit::UNDO_QUARANTINE_PREFIX))
        })
        .collect();
    assert_eq!(quarantined.len(), 1, "{quarantined:?}");
    assert_eq!(
        std::fs::read(&quarantined[0]).unwrap(),
        *foreign.lock().unwrap()
    );
    assert!(wav.exists());
    // 1 時間以上前の mtime にしてスキャンしても、隔離名の実体はスキャナの回収対象にならない
    let old = std::time::SystemTime::now() - Duration::from_secs(3 * 3600);
    std::fs::File::options()
        .write(true)
        .open(&quarantined[0])
        .unwrap()
        .set_modified(old)
        .unwrap();
    let report = lib.scan().await;
    assert_eq!(report.tmp_removed, 0);
    assert!(
        quarantined[0].exists(),
        "隔離した外部ファイルがスキャンで消えた"
    );
    assert_eq!(
        std::fs::read(&quarantined[0]).unwrap(),
        *foreign.lock().unwrap()
    );
}

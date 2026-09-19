//! 一括リネームの coordinator（SPEC §7.5「リネーム」、§7.1、docs/TASKS.md P0-11）。
//! 記録 → DB 先行更新（2 段階）→ rename ジョブの 2 phase → 事前条件 → 衝突 → キャンセル →
//! phase 境界でのクラッシュ復旧 → album の追随。合成ファイルは ffmpeg で作る（無ければ skip）

#![cfg(target_os = "linux")]

mod common;

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use spindle::config::LayoutConfig;
use spindle::db::history::{self, BatchState, OpKind, OpResult};
use spindle::db::Db;
use spindle::domain::pathgen::Planned;
use spindle::domain::relpath::RelPath;
use spindle::edit::{CancelOutcome, EditError, Editor, RenameStep, RenameTarget};
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, ScanReport, Scanner};
use spindle::jobs::handlers::rename::RenameHandler;
use spindle::jobs::{JobState, JobType, Jobs, Registry};

// ---------------------------------------------------------------- ハーネス

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    db: Arc<Db>,
    jobs: Arc<Jobs>,
    scanner: Scanner,
    editor: Arc<Editor>,
    shutdown: CancellationToken,
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Library");
        std::fs::create_dir(&lib).unwrap();
        let db_path = dir.path().join("spindle.db");
        Self::open(dir, db_path)
    }

    fn open(dir: tempfile::TempDir, db_path: PathBuf) -> Self {
        let lib = dir.path().join("Library");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let root = Arc::new(RootDir::open(&lib).unwrap());
        let jobs = Jobs::new(db.clone());
        let scanner = Scanner::new(db.clone(), root.clone(), 2);
        let editor = Arc::new(Editor::new(db.clone(), root, jobs.clone()));
        Self {
            dir,
            db_path,
            db,
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
        Self::open(dir, db_path)
    }

    fn start(&self) {
        let mut reg = Registry::new();
        reg.register(
            JobType::Rename,
            Arc::new(RenameHandler::new(self.editor.clone())),
        );
        self.jobs.start(reg, self.shutdown.clone());
    }

    fn lib(&self) -> PathBuf {
        self.dir.path().join("Library")
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.lib().join(rel)
    }

    fn add(&self, rel: &str, seed: u32, title: &str) -> Option<PathBuf> {
        self.add_tagged(rel, seed, title, "Album", "AlbumArtist", 1, 1)
    }

    #[allow(clippy::too_many_arguments)]
    fn add_tagged(
        &self,
        rel: &str,
        seed: u32,
        title: &str,
        album: &str,
        albumartist: &str,
        track: u32,
        disc: u32,
    ) -> Option<PathBuf> {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let ext = rel.rsplit_once('.').unwrap().1;
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        let made = common::make_audio(p.parent().unwrap(), &name, ext, seed)?;
        common::set_basic_tags(&made, title, "Artist", album, albumartist, track, disc);
        Some(made)
    }

    async fn scan(&self) -> ScanReport {
        self.scanner
            .run(
                ScanKind::Incremental,
                Arc::new(|_, _, _| {}),
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

    fn rel_path(&self, id: i64) -> String {
        self.conn()
            .query_row("SELECT rel_path FROM tracks WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
    }

    fn rel_path_key(&self, id: i64) -> String {
        self.conn()
            .query_row("SELECT rel_path_key FROM tracks WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
    }

    fn phys(&self, id: i64) -> (i64, i64, i64, i64) {
        self.conn()
            .query_row(
                "SELECT dev, inode, mtime_ns, ctime_ns FROM tracks WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
    }

    fn album_of(&self, id: i64) -> Option<(i64, String, Option<i64>)> {
        self.conn()
            .query_row(
                "SELECT a.id, a.rel_dir, a.missing_since FROM tracks t JOIN albums a ON a.id = t.album_id
                 WHERE t.id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok()
    }

    fn missing_since(&self, id: i64) -> Option<i64> {
        self.conn()
            .query_row(
                "SELECT missing_since FROM tracks WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn batch_state(&self, id: i64) -> BatchState {
        history::get_batch(&self.conn(), id).unwrap().unwrap().state
    }

    fn ops(&self, batch_id: i64) -> Vec<history::Op> {
        history::list_ops(&self.conn(), batch_id).unwrap()
    }

    fn job_state(&self, id: i64) -> JobState {
        let s: String = self
            .conn()
            .query_row("SELECT state FROM jobs WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();
        s.parse().unwrap()
    }

    async fn wait_batch_terminal(&self, id: i64) -> BatchState {
        // CI のランナーは I/O が遅く回数ベースでは足りないので、経過時間で待つ
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while std::time::Instant::now() < deadline {
            let st = self.batch_state(id);
            if !matches!(st, BatchState::Prepared | BatchState::Applying) {
                return st;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("batch {id} が終端にならない: {:?}", self.batch_state(id));
    }

    async fn wait_job_terminal(&self, id: i64) -> JobState {
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

impl Drop for Lib {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn inode_of(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).unwrap().ino()
}

fn target(track_id: i64, new_rel_path: &str) -> RenameTarget {
    RenameTarget {
        track_id,
        new_rel_path: new_rel_path.to_owned(),
        expected: None,
        planned_conflict: None,
    }
}

fn layout() -> LayoutConfig {
    LayoutConfig {
        multi_disc: "{category}/{albumartist}/{album}/{disc}-{track:02} {title}".to_owned(),
        single_disc: "{category}/{albumartist}/{album}/{track:02} {title}".to_owned(),
        unsorted: "_Unsorted/{albumartist}/{album}/{track:02} {title}".to_owned(),
    }
}

/// ライブラリ内の音声ファイル一覧（相対パス、ソート済み）
fn files_in(lib: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            if p.is_dir() {
                walk(root, &p, out);
            } else if p.extension().is_some_and(|x| x == "flac") {
                out.push(
                    p.strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut out = Vec::new();
    walk(lib, lib, &mut out);
    out.sort();
    out
}

// ---------------------------------------------------------------- 記録と DB 先行更新

#[tokio::test]
async fn prepare_records_rename_ops_and_overlays_db_paths() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.add("A/02.flac", 2, "b");
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let b = lib.track_id("A/02.flac");
    let before = lib.phys(a);

    let prepared = lib
        .editor
        .prepare_rename(
            Some("移動"),
            vec![target(a, "B/01 a.flac"), target(b, "B/02 b.flac")],
        )
        .await
        .unwrap();
    assert_eq!(prepared.affected, 2);
    assert_eq!(prepared.conflict, 0);
    // バッチ 1 つに rename ジョブ 1 つ
    assert_eq!(prepared.job_ids.len(), 1);
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Prepared);
    let job: (String, Option<i64>, Option<String>) = lib
        .conn()
        .query_row(
            "SELECT type, edit_batch_id, dedup_key FROM jobs WHERE id = ?1",
            [prepared.job_ids[0]],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(job.0, "rename");
    assert_eq!(job.1, Some(prepared.batch_id));
    assert_eq!(
        job.2.as_deref(),
        Some(format!("rename:batch:{}", prepared.batch_id).as_str())
    );

    // op: kind=rename、事前条件は記録時点の実体、expected_rel_path は旧パス
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(ops.len(), 2);
    assert!(ops.iter().all(|o| o.kind == OpKind::Rename));
    assert!(ops.iter().all(|o| o.result == OpResult::Pending));
    assert!(ops.iter().all(|o| o.job_id == Some(prepared.job_ids[0])));
    let oa = ops.iter().find(|o| o.track_id == a).unwrap();
    assert_eq!(oa.expected.rel_path.as_deref(), Some("A/01.flac"));
    assert_eq!(oa.expected.inode, Some(before.1));
    let edits = history::list_edits(&lib.conn(), oa.id).unwrap();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].key, "rel_path");
    assert_eq!(edits[0].old_value, serde_json::json!("A/01.flac"));
    assert_eq!(edits[0].new_value, serde_json::json!("B/01 a.flac"));

    // overlay: DB は新パス。ファイルは旧パスのまま
    assert_eq!(lib.rel_path(a), "B/01 a.flac");
    assert_eq!(lib.rel_path_key(b), "b/02 b.flac");
    assert!(lib.path("A/01.flac").exists());
    assert!(!lib.path("B/01 a.flac").exists());
}

#[tokio::test]
async fn prepare_rejects_pending_tracks_and_reports_unchanged() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.add("A/02.flac", 2, "b");
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let b = lib.track_id("A/02.flac");
    let first = lib
        .editor
        .prepare_rename(None, vec![target(a, "B/01.flac")])
        .await
        .unwrap();
    let err = lib
        .editor
        .prepare_rename(None, vec![target(a, "C/01.flac"), target(b, "C/02.flac")])
        .await
        .unwrap_err();
    assert!(matches!(err, EditError::Pending { ref track_ids } if track_ids == &[a]));
    // 変更なしだけなら NoChanges
    let err = lib
        .editor
        .prepare_rename(None, vec![target(b, "A/02.flac")])
        .await
        .unwrap_err();
    assert!(matches!(err, EditError::NoChanges));
    assert_eq!(lib.batch_state(first.batch_id), BatchState::Prepared);
}

#[tokio::test]
async fn prepare_records_destination_collision_as_conflict_without_touching_db() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.add("A/02.flac", 2, "b");
    lib.add("A/03.flac", 3, "c");
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let b = lib.track_id("A/02.flac");
    // a → 選択外の c が占有するパス、b → 正常
    let prepared = lib
        .editor
        .prepare_rename(None, vec![target(a, "A/03.flac"), target(b, "B/02.flac")])
        .await
        .unwrap();
    assert_eq!(prepared.affected, 2);
    assert_eq!(prepared.conflict, 1);
    let oa = lib
        .ops(prepared.batch_id)
        .into_iter()
        .find(|o| o.track_id == a)
        .unwrap();
    assert_eq!(oa.result, OpResult::SkippedConflict);
    assert_eq!(lib.rel_path(a), "A/01.flac");
    assert_eq!(lib.rel_path(b), "B/02.flac");
}

#[tokio::test]
async fn case_only_difference_between_two_targets_is_a_conflict() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.add("A/02.flac", 2, "b");
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let b = lib.track_id("A/02.flac");
    let prepared = lib
        .editor
        .prepare_rename(
            None,
            vec![target(a, "B/Song.flac"), target(b, "B/song.flac")],
        )
        .await
        .unwrap();
    assert_eq!(prepared.conflict, 2);
    assert!(lib
        .ops(prepared.batch_id)
        .iter()
        .all(|o| o.result == OpResult::SkippedConflict));
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Failed);
    assert_eq!(lib.rel_path(a), "A/01.flac");
    assert_eq!(lib.rel_path(b), "A/02.flac");
}

// ---------------------------------------------------------------- 反映

#[tokio::test]
async fn rename_job_moves_files_creates_directories_and_follows_inode() {
    let lib = Lib::new();
    let pa = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let ino = inode_of(&pa);
    let (dev0, ino0, mtime0, _) = lib.phys(a);

    let prepared = lib
        .editor
        .prepare_rename(None, vec![target(a, "J-Pop/Artist/Album/01 a.flac")])
        .await
        .unwrap();
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(
        lib.wait_job_terminal(prepared.job_ids[0]).await,
        JobState::Done
    );
    let dst = lib.path("J-Pop/Artist/Album/01 a.flac");
    assert!(dst.exists());
    assert!(!pa.exists());
    assert_eq!(inode_of(&dst), ino);
    // DB: パス・物理属性（rename は ctime を進める）
    assert_eq!(lib.rel_path(a), "J-Pop/Artist/Album/01 a.flac");
    let (dev1, ino1, mtime1, ctime1) = lib.phys(a);
    assert_eq!((dev1, ino1, mtime1), (dev0, ino0, mtime0));
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(&dst).unwrap();
    assert_eq!(ctime1, md.ctime() * 1_000_000_000 + md.ctime_nsec());
    let op = &lib.ops(prepared.batch_id)[0];
    assert_eq!(op.result, OpResult::Applied);
    assert_eq!(op.job_id, Some(prepared.job_ids[0]));
    // 一時名は残らない
    assert_eq!(files_in(&lib.lib()), ["J-Pop/Artist/Album/01 a.flac"]);
    // 再スキャンで重複も移動も出ない
    let report = lib.scan().await;
    assert_eq!((report.new, report.moved, report.missing_marked), (0, 0, 0));
}

#[tokio::test]
async fn swap_of_two_paths_completes() {
    let lib = Lib::new();
    let pa = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    let pb = lib.add("A/02.flac", 2, "b").unwrap();
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let b = lib.track_id("A/02.flac");
    let (ia, ib) = (inode_of(&pa), inode_of(&pb));

    let prepared = lib
        .editor
        .prepare_rename(None, vec![target(a, "A/02.flac"), target(b, "A/01.flac")])
        .await
        .unwrap();
    assert_eq!(lib.rel_path(a), "A/02.flac");
    assert_eq!(lib.rel_path(b), "A/01.flac");
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(inode_of(&lib.path("A/02.flac")), ia);
    assert_eq!(inode_of(&lib.path("A/01.flac")), ib);
    assert_eq!(files_in(&lib.lib()), ["A/01.flac", "A/02.flac"]);
    let report = lib.scan().await;
    assert_eq!((report.new, report.moved, report.missing_marked), (0, 0, 0));
}

#[tokio::test]
async fn cycle_of_three_paths_completes() {
    let lib = Lib::new();
    let p1 = require_ffmpeg!(lib.add("A/1.flac", 1, "a"));
    let p2 = lib.add("A/2.flac", 2, "b").unwrap();
    let p3 = lib.add("A/3.flac", 3, "c").unwrap();
    lib.scan().await;
    let (t1, t2, t3) = (
        lib.track_id("A/1.flac"),
        lib.track_id("A/2.flac"),
        lib.track_id("A/3.flac"),
    );
    let (i1, i2, i3) = (inode_of(&p1), inode_of(&p2), inode_of(&p3));
    let prepared = lib
        .editor
        .prepare_rename(
            None,
            vec![
                target(t1, "A/2.flac"),
                target(t2, "A/3.flac"),
                target(t3, "A/1.flac"),
            ],
        )
        .await
        .unwrap();
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(inode_of(&lib.path("A/2.flac")), i1);
    assert_eq!(inode_of(&lib.path("A/3.flac")), i2);
    assert_eq!(inode_of(&lib.path("A/1.flac")), i3);
    assert_eq!(files_in(&lib.lib()), ["A/1.flac", "A/2.flac", "A/3.flac"]);
}

#[tokio::test]
async fn applying_is_idempotent() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let prepared = lib
        .editor
        .prepare_rename(None, vec![target(a, "B/01.flac")])
        .await
        .unwrap();
    let first = lib
        .editor
        .apply_rename_batch(prepared.batch_id, None, None)
        .await
        .unwrap();
    assert_eq!(first.applied, 1);
    let ctime = lib.phys(a).3;
    let second = lib
        .editor
        .apply_rename_batch(prepared.batch_id, None, None)
        .await
        .unwrap();
    assert_eq!(second.applied, 0);
    assert_eq!(lib.phys(a).3, ctime);
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Applied);
    assert_eq!(files_in(&lib.lib()), ["B/01.flac"]);
}

// ---------------------------------------------------------------- 事前条件と衝突

#[tokio::test]
async fn externally_moved_source_conflicts_and_restores_db_path() {
    let lib = Lib::new();
    let pa = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let prepared = lib
        .editor
        .prepare_rename(None, vec![target(a, "B/01.flac")])
        .await
        .unwrap();
    std::fs::rename(&pa, lib.path("A/moved.flac")).unwrap();
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Failed
    );
    let op = &lib.ops(prepared.batch_id)[0];
    assert_eq!(op.result, OpResult::SkippedConflict);
    // ファイルは触らず、DB は記録時点の旧パスへ戻す（次回スキャンが inode で追随する）
    assert_eq!(files_in(&lib.lib()), ["A/moved.flac"]);
    assert_eq!(lib.rel_path(a), "A/01.flac");
    let report = lib.scan().await;
    assert_eq!(report.moved, 1);
    assert_eq!(lib.rel_path(a), "A/moved.flac");
}

/// 記録の後にホストを再起動して dev 番号が変わっても、inode 以下が同じなら同じ実体（D-62）
#[tokio::test]
async fn expected_dev_mismatch_alone_does_not_block_rename() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let prepared = lib
        .editor
        .prepare_rename(None, vec![target(a, "B/01.flac")])
        .await
        .unwrap();
    lib.conn()
        .execute(
            "UPDATE edit_ops SET expected_dev = expected_dev + 1 WHERE batch_id = ?1",
            [prepared.batch_id],
        )
        .unwrap();
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(files_in(&lib.lib()), ["B/01.flac"]);
    assert_eq!(lib.rel_path(a), "B/01.flac");
}

#[tokio::test]
async fn externally_modified_source_conflicts_and_is_not_moved() {
    let lib = Lib::new();
    let pa = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let prepared = lib
        .editor
        .prepare_rename(None, vec![target(a, "B/01.flac")])
        .await
        .unwrap();
    // in-place のタグ書き換え（inode 不変、ctime が進む）
    common::retag(&pa, |t| {
        use lofty::tag::Accessor as _;
        t.set_title("外部変更".to_owned())
    });
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Failed
    );
    assert_eq!(
        lib.ops(prepared.batch_id)[0].result,
        OpResult::SkippedConflict
    );
    assert!(pa.exists());
    assert_eq!(lib.rel_path(a), "A/01.flac");
    // overlay 解消でファイルの現在値（物理属性）に揃う
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(&pa).unwrap();
    assert_eq!(lib.phys(a).3, md.ctime() * 1_000_000_000 + md.ctime_nsec());
}

#[tokio::test]
async fn destination_taken_externally_conflicts_and_rolls_back() {
    let lib = Lib::new();
    let pa = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.add("A/02.flac", 2, "b");
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let b = lib.track_id("A/02.flac");
    let prepared = lib
        .editor
        .prepare_rename(None, vec![target(a, "B/01.flac"), target(b, "B/02.flac")])
        .await
        .unwrap();
    // 宛先を外部が先に作る（DB の事前判定では見えず、RENAME_NOREPLACE が最終判定になる）
    std::fs::create_dir_all(lib.path("B")).unwrap();
    std::fs::write(lib.path("B/01.flac"), b"intruder").unwrap();
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Partial
    );
    let ops = lib.ops(prepared.batch_id);
    let oa = ops.iter().find(|o| o.track_id == a).unwrap();
    let ob = ops.iter().find(|o| o.track_id == b).unwrap();
    assert_eq!(oa.result, OpResult::SkippedConflict);
    assert_eq!(ob.result, OpResult::Applied);
    // a は元の場所へ戻り、DB も旧パス。侵入者は無傷
    assert!(pa.exists());
    assert_eq!(std::fs::read(lib.path("B/01.flac")).unwrap(), b"intruder");
    assert_eq!(lib.rel_path(a), "A/01.flac");
    assert_eq!(lib.rel_path(b), "B/02.flac");
    assert_eq!(
        files_in(&lib.lib()),
        ["A/01.flac", "B/01.flac", "B/02.flac"]
    );
}

#[tokio::test]
async fn lost_temp_file_in_a_swap_does_not_break_the_unique_path_of_the_partner() {
    // A: x→y, B: y→x。phase 1 の後に A の一時名を外部が消す。B は x へ置けるが、A の所在は不明。
    // A を記録時点の x へ戻すと B と UNIQUE で衝突するので、A は予約 key へ退避する
    let lib = Lib::new();
    let px = require_ffmpeg!(lib.add("A/x.flac", 1, "a"));
    let py = lib.add("A/y.flac", 2, "b").unwrap();
    lib.scan().await;
    let (a, b) = (lib.track_id("A/x.flac"), lib.track_id("A/y.flac"));
    let ib = inode_of(&py);
    let prepared = lib
        .editor
        .prepare_rename(None, vec![target(a, "A/y.flac"), target(b, "A/x.flac")])
        .await
        .unwrap();
    let op_a = lib.ops(prepared.batch_id)[0].id;
    let gate = gate_at(&lib.editor, RenameStep::Staged);
    lib.start();
    gate.wait_reached().await;
    std::fs::remove_file(lib.path(&format!("A/spindle-rename-{op_a}.flac"))).unwrap();
    gate.resume();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Partial
    );
    assert_eq!(
        lib.wait_job_terminal(prepared.job_ids[0]).await,
        JobState::Done
    );
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(ops[0].result, OpResult::SkippedConflict);
    assert_eq!(ops[1].result, OpResult::Applied);
    assert_eq!(inode_of(&lib.path("A/x.flac")), ib);
    assert_eq!(lib.rel_path(b), "A/x.flac");
    assert!(lib.rel_path(a).starts_with('\0'), "{:?}", lib.rel_path(a));
    let _ = px;
    // 次のスキャンで A は missing になり、B は動かない
    let report = lib.scan().await;
    assert_eq!(report.missing_marked, 1);
    assert_eq!(report.moved, 0);
    assert!(lib.missing_since(a).is_some());
}

#[tokio::test]
async fn scan_that_detects_external_move_follows_the_file_and_the_job_leaves_it() {
    let lib = Lib::new();
    let pa = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let prepared = lib
        .editor
        .prepare_rename(None, vec![target(a, "B/01.flac")])
        .await
        .unwrap();
    std::fs::rename(&pa, lib.path("A/moved.flac")).unwrap();
    // 外部 rename をスキャンが先に検出: op は conflict、rel_path は実在パスへ
    let report = lib.scan().await;
    assert_eq!(report.moved, 1);
    assert_eq!(
        lib.ops(prepared.batch_id)[0].result,
        OpResult::SkippedConflict
    );
    assert_eq!(lib.rel_path(a), "A/moved.flac");
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Failed
    );
    assert_eq!(
        lib.wait_job_terminal(prepared.job_ids[0]).await,
        JobState::Done
    );
    assert_eq!(lib.rel_path(a), "A/moved.flac");
    assert_eq!(files_in(&lib.lib()), ["A/moved.flac"]);
}

// ---------------------------------------------------------------- キャンセル

#[tokio::test]
async fn cancel_before_start_restores_swapped_paths() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.add("A/02.flac", 2, "b");
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let b = lib.track_id("A/02.flac");
    let prepared = lib
        .editor
        .prepare_rename(None, vec![target(a, "A/02.flac"), target(b, "A/01.flac")])
        .await
        .unwrap();
    let outcome = lib.editor.cancel_batch(prepared.batch_id).await.unwrap();
    assert!(matches!(
        outcome,
        CancelOutcome::Cancelled {
            ops_cancelled: 2,
            running: 0
        }
    ));
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Cancelled);
    assert_eq!(lib.rel_path(a), "A/01.flac");
    assert_eq!(lib.rel_path(b), "A/02.flac");
    assert!(lib
        .ops(prepared.batch_id)
        .iter()
        .all(|o| o.result == OpResult::Failed && o.error.as_deref() == Some("cancelled")));
    assert_eq!(lib.job_state(prepared.job_ids[0]), JobState::Cancelled);
}

/// phase 1 完了時点でハンドラを止め、テスト側が操作してから続行させるためのゲート
struct Gate {
    reached: Arc<Mutex<mpsc::Receiver<()>>>,
    resume: mpsc::Sender<()>,
}

fn gate_at(editor: &Editor, at: RenameStep) -> Gate {
    let (reached_tx, reached_rx) = mpsc::channel::<()>();
    let (resume_tx, resume_rx) = mpsc::channel::<()>();
    let resume_rx = Mutex::new(resume_rx);
    editor.set_rename_hook(Arc::new(move |step| {
        if step == at {
            reached_tx.send(()).unwrap();
            resume_rx.lock().unwrap().recv().unwrap();
        }
        Ok(())
    }));
    Gate {
        reached: Arc::new(Mutex::new(reached_rx)),
        resume: resume_tx,
    }
}

impl Gate {
    async fn wait_reached(&self) {
        let rx = Arc::clone(&self.reached);
        tokio::task::spawn_blocking(move || {
            rx.lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        })
        .await
        .unwrap();
    }

    fn resume(&self) {
        self.resume.send(()).unwrap();
    }
}

#[tokio::test]
async fn cancel_before_start_with_a_lost_source_in_a_swap_restores_the_partner() {
    // A: x→y, B: y→x。開始前に x（A の source）が外部に消える。cancel で B は y へ戻り、
    // A は所在不明。A の overlay（y）が B の戻り先と重なるので、A を先に退避しないと UNIQUE を踏む
    let lib = Lib::new();
    let px = require_ffmpeg!(lib.add("A/x.flac", 1, "a"));
    lib.add("A/y.flac", 2, "b");
    lib.scan().await;
    let (a, b) = (lib.track_id("A/x.flac"), lib.track_id("A/y.flac"));
    let prepared = lib
        .editor
        .prepare_rename(None, vec![target(a, "A/y.flac"), target(b, "A/x.flac")])
        .await
        .unwrap();
    std::fs::remove_file(&px).unwrap();
    let outcome = lib.editor.cancel_batch(prepared.batch_id).await.unwrap();
    assert!(
        matches!(outcome, CancelOutcome::Cancelled { .. }),
        "{outcome:?}"
    );
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Cancelled);
    assert_eq!(lib.rel_path(b), "A/y.flac");
    // A の記録時点のパス x は空いているので戻す（ファイルは無い。次のスキャンで missing）
    assert_eq!(lib.rel_path(a), "A/x.flac");
    assert_eq!(files_in(&lib.lib()), ["A/y.flac"]);
    let report = lib.scan().await;
    assert_eq!((report.moved, report.missing_marked), (0, 1));
}

#[tokio::test]
async fn cancel_during_phase1_rolls_back_staged_files() {
    let lib = Lib::new();
    let pa = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    let pb = lib.add("A/02.flac", 2, "b").unwrap();
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let b = lib.track_id("A/02.flac");
    let prepared = lib
        .editor
        .prepare_rename(None, vec![target(a, "B/01.flac"), target(b, "B/02.flac")])
        .await
        .unwrap();
    let gate = gate_at(&lib.editor, RenameStep::Staged);
    lib.start();
    gate.wait_reached().await;
    // 一時名に退避済み
    assert!(!pa.exists());
    assert!(!pb.exists());
    let outcome = lib.editor.cancel_batch(prepared.batch_id).await.unwrap();
    assert!(matches!(
        outcome,
        CancelOutcome::Cancelled {
            ops_cancelled: 0,
            running: 2
        }
    ));
    gate.resume();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Cancelled
    );
    assert_eq!(
        lib.wait_job_terminal(prepared.job_ids[0]).await,
        JobState::Cancelled
    );
    // ファイルは元の場所へ、DB も旧パスへ
    assert_eq!(files_in(&lib.lib()), ["A/01.flac", "A/02.flac"]);
    assert_eq!(lib.rel_path(a), "A/01.flac");
    assert_eq!(lib.rel_path(b), "A/02.flac");
}

// ---------------------------------------------------------------- クラッシュ復旧

#[tokio::test]
async fn kill_after_phase1_completes_on_restart() {
    let lib = Lib::new();
    let pa = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    let pb = lib.add("A/02.flac", 2, "b").unwrap();
    let pc = lib.add("A/03.flac", 3, "c").unwrap();
    lib.scan().await;
    let (a, b, c) = (
        lib.track_id("A/01.flac"),
        lib.track_id("A/02.flac"),
        lib.track_id("A/03.flac"),
    );
    let (ia, ib, ic) = (inode_of(&pa), inode_of(&pb), inode_of(&pc));
    // 3 件の循環
    let prepared = lib
        .editor
        .prepare_rename(
            None,
            vec![
                target(a, "A/02.flac"),
                target(b, "A/03.flac"),
                target(c, "A/01.flac"),
            ],
        )
        .await
        .unwrap();
    let job_id = prepared.job_ids[0];
    // phase 1 の直後に kill: hook がエラーを返し、ジョブ行は running のまま残す
    lib.editor.set_rename_hook(Arc::new(|step| {
        if step == RenameStep::Staged {
            Err("killed".to_owned())
        } else {
            Ok(())
        }
    }));
    lib.conn()
        .execute("UPDATE jobs SET state = 'running' WHERE id = ?1", [job_id])
        .unwrap();
    let err = lib
        .editor
        .apply_rename_batch(prepared.batch_id, Some(job_id), None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("killed"), "{err}");
    // 元のパスには無く、一時名にある
    assert!(!pa.exists() && !pb.exists() && !pc.exists());
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Applying);

    let lib = lib.reopen();
    spindle::jobs::recovery::run(&lib.db).await.unwrap();
    assert_eq!(lib.job_state(job_id), JobState::Queued);
    let report = lib.editor.recover().await.unwrap();
    assert_eq!(report.requeued, 0); // queued のままなので再投入は不要
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(inode_of(&lib.path("A/02.flac")), ia);
    assert_eq!(inode_of(&lib.path("A/03.flac")), ib);
    assert_eq!(inode_of(&lib.path("A/01.flac")), ic);
    assert_eq!(
        files_in(&lib.lib()),
        ["A/01.flac", "A/02.flac", "A/03.flac"]
    );
    let report = lib.scan().await;
    assert_eq!((report.new, report.moved, report.missing_marked), (0, 0, 0));
}

#[tokio::test]
async fn kill_during_phase2_completes_on_restart() {
    let lib = Lib::new();
    let pa = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    let pb = lib.add("A/02.flac", 2, "b").unwrap();
    lib.scan().await;
    let (a, b) = (lib.track_id("A/01.flac"), lib.track_id("A/02.flac"));
    let (ia, ib) = (inode_of(&pa), inode_of(&pb));
    let prepared = lib
        .editor
        .prepare_rename(None, vec![target(a, "A/02.flac"), target(b, "A/01.flac")])
        .await
        .unwrap();
    let job_id = prepared.job_ids[0];
    let second_op = lib.ops(prepared.batch_id)[1].id;
    // 1 件目は最終名に置いた後、2 件目の直前で kill
    lib.editor.set_rename_hook(Arc::new(move |step| {
        if step == RenameStep::BeforeFinal(second_op) {
            Err("killed".to_owned())
        } else {
            Ok(())
        }
    }));
    lib.conn()
        .execute("UPDATE jobs SET state = 'running' WHERE id = ?1", [job_id])
        .unwrap();
    lib.editor
        .apply_rename_batch(prepared.batch_id, Some(job_id), None)
        .await
        .unwrap_err();
    assert_eq!(inode_of(&lib.path("A/02.flac")), ia);
    assert!(!lib.path("A/01.flac").exists());

    let lib = lib.reopen();
    spindle::jobs::recovery::run(&lib.db).await.unwrap();
    lib.editor.recover().await.unwrap();
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(inode_of(&lib.path("A/02.flac")), ia);
    assert_eq!(inode_of(&lib.path("A/01.flac")), ib);
    assert_eq!(files_in(&lib.lib()), ["A/01.flac", "A/02.flac"]);
}

#[tokio::test]
async fn recovery_requeues_rename_batch_job_when_job_row_is_terminal() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let prepared = lib
        .editor
        .prepare_rename(None, vec![target(a, "B/01.flac")])
        .await
        .unwrap();
    lib.conn()
        .execute(
            "UPDATE jobs SET state = 'failed' WHERE id = ?1",
            [prepared.job_ids[0]],
        )
        .unwrap();
    let lib = lib.reopen();
    spindle::jobs::recovery::run(&lib.db).await.unwrap();
    let report = lib.editor.recover().await.unwrap();
    assert_eq!(report.requeued, 1);
    let queued: i64 = lib
        .conn()
        .query_row(
            "SELECT count(*) FROM jobs WHERE type = 'rename' AND state = 'queued'
               AND edit_batch_id = ?1",
            [prepared.batch_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(queued, 1);
    assert_eq!(lib.editor.recover().await.unwrap().requeued, 0);
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(files_in(&lib.lib()), ["B/01.flac"]);
}

// ---------------------------------------------------------------- スキャンとの並走

#[tokio::test]
async fn scan_between_phases_keeps_ops_pending_and_tracks_present() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.add("A/02.flac", 2, "b");
    lib.scan().await;
    let (a, b) = (lib.track_id("A/01.flac"), lib.track_id("A/02.flac"));
    let prepared = lib
        .editor
        .prepare_rename(None, vec![target(a, "B/01.flac"), target(b, "B/02.flac")])
        .await
        .unwrap();
    let gate = gate_at(&lib.editor, RenameStep::Staged);
    lib.start();
    gate.wait_reached().await;
    // ファイルは一時名。スキャンは op を衝突にせず、トラックを missing にもしない
    let report = lib.scan().await;
    assert_eq!(report.missing_marked, 0);
    assert_eq!(report.new, 0);
    assert!(lib
        .ops(prepared.batch_id)
        .iter()
        .all(|o| o.result == OpResult::Pending));
    assert_eq!(lib.missing_since(a), None);
    assert_eq!(lib.rel_path(a), "B/01.flac");
    gate.resume();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(files_in(&lib.lib()), ["B/01.flac", "B/02.flac"]);
    let report = lib.scan().await;
    assert_eq!((report.new, report.moved, report.missing_marked), (0, 0, 0));
}

// ---------------------------------------------------------------- album の追随

#[tokio::test]
async fn whole_album_move_keeps_album_id_and_updates_rel_dir() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.add("A/02.flac", 2, "b");
    lib.scan().await;
    let (a, b) = (lib.track_id("A/01.flac"), lib.track_id("A/02.flac"));
    let (album_id, _, _) = lib.album_of(a).unwrap();
    let prepared = lib
        .editor
        .prepare_rename(
            None,
            vec![target(a, "X/Y/01.flac"), target(b, "X/Y/02.flac")],
        )
        .await
        .unwrap();
    // overlay の時点で album も追随する
    assert_eq!(lib.album_of(a), Some((album_id, "X/Y".to_owned(), None)));
    assert_eq!(lib.album_of(b), Some((album_id, "X/Y".to_owned(), None)));
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(lib.album_of(a), Some((album_id, "X/Y".to_owned(), None)));
    // 再スキャンでも album は増減しない
    lib.scan().await;
    let n: i64 = lib
        .conn()
        .query_row("SELECT count(*) FROM albums", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(lib.album_of(a), Some((album_id, "X/Y".to_owned(), None)));
}

#[tokio::test]
async fn partial_move_creates_new_album_and_keeps_old_one() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.add("A/02.flac", 2, "b");
    lib.scan().await;
    let (a, b) = (lib.track_id("A/01.flac"), lib.track_id("A/02.flac"));
    let (album_id, _, _) = lib.album_of(a).unwrap();
    let prepared = lib
        .editor
        .prepare_rename(None, vec![target(a, "B/01.flac")])
        .await
        .unwrap();
    let (new_album, dir, missing) = lib.album_of(a).unwrap();
    assert_ne!(new_album, album_id);
    assert_eq!(dir, "B");
    assert_eq!(missing, None);
    assert_eq!(lib.album_of(b), Some((album_id, "A".to_owned(), None)));
    lib.start();
    lib.wait_batch_terminal(prepared.batch_id).await;
    // 再スキャンと一致する
    lib.scan().await;
    assert_eq!(lib.album_of(a).map(|x| x.0), Some(new_album));
    assert_eq!(lib.album_of(b).map(|x| x.0), Some(album_id));
}

#[tokio::test]
async fn conflict_restores_album_membership() {
    let lib = Lib::new();
    let pa = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.add("A/02.flac", 2, "b");
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let (album_id, _, _) = lib.album_of(a).unwrap();
    let prepared = lib
        .editor
        .prepare_rename(None, vec![target(a, "B/01.flac")])
        .await
        .unwrap();
    assert_ne!(lib.album_of(a).unwrap().0, album_id);
    common::retag(&pa, |t| {
        use lofty::tag::Accessor as _;
        t.set_title("外部変更".to_owned())
    });
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Failed
    );
    assert_eq!(lib.album_of(a), Some((album_id, "A".to_owned(), None)));
}

// ---------------------------------------------------------------- テンプレートからの計画

#[tokio::test]
async fn plan_rename_uses_layout_and_album_metadata() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add_tagged("old/x.flac", 1, "曲: 一", "魔法", "花譜", 1, 1));
    lib.add_tagged("old/y.flac", 2, "曲二", "魔法", "花譜", 2, 1);
    // 別ディレクトリの同名 album = 別リリース（同名 ≠ 同一リリース）
    lib.add_tagged("other/z.flac", 3, "曲三", "魔法", "花譜", 1, 1);
    lib.scan().await;
    lib.conn()
        .execute("INSERT INTO categories (name) VALUES ('J-Pop')", [])
        .unwrap();
    lib.conn()
        .execute(
            "UPDATE albums SET category_id = (SELECT id FROM categories WHERE name = 'J-Pop'),
                    date = CASE rel_dir WHEN 'old' THEN '2020-01-01' ELSE '2023' END",
            [],
        )
        .unwrap();
    let (x, y, z) = (
        lib.track_id("old/x.flac"),
        lib.track_id("old/y.flac"),
        lib.track_id("other/z.flac"),
    );
    let plan = lib.editor.plan_rename(&[x, y, z], &layout()).await.unwrap();
    let by_id = |id: i64| plan.iter().find(|p| p.track_id == id).unwrap();
    assert_eq!(by_id(x).current_rel_path, "old/x.flac");
    assert_eq!(
        by_id(x).planned,
        Planned::Path(RelPath::parse("J-Pop/花譜/魔法 (2020)/01 曲： 一.flac").unwrap())
    );
    assert_eq!(
        by_id(y).planned,
        Planned::Path(RelPath::parse("J-Pop/花譜/魔法 (2020)/02 曲二.flac").unwrap())
    );
    assert_eq!(
        by_id(z).planned,
        Planned::Path(RelPath::parse("J-Pop/花譜/魔法 (2023)/01 曲三.flac").unwrap())
    );
}

#[tokio::test]
async fn plan_rename_without_category_uses_unsorted_template() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add_tagged("old/x.flac", 1, "t", "Al", "Ar", 3, 1));
    lib.scan().await;
    let x = lib.track_id("old/x.flac");
    let plan = lib.editor.plan_rename(&[x], &layout()).await.unwrap();
    assert_eq!(
        plan[0].planned,
        Planned::Path(RelPath::parse("_Unsorted/Ar/Al/03 t.flac").unwrap())
    );
}

#[tokio::test]
async fn plan_rename_uses_multi_disc_template_when_album_has_multiple_discs() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add_tagged("J-Pop/x.flac", 1, "t1", "Al", "Ar", 1, 1));
    lib.add_tagged("J-Pop/y.flac", 2, "t2", "Al", "Ar", 1, 2);
    lib.conn()
        .execute("INSERT INTO categories (name) VALUES ('J-Pop')", [])
        .unwrap();
    lib.scan().await;
    let (x, y) = (lib.track_id("J-Pop/x.flac"), lib.track_id("J-Pop/y.flac"));
    let plan = lib.editor.plan_rename(&[x, y], &layout()).await.unwrap();
    assert_eq!(
        plan[0].planned,
        Planned::Path(RelPath::parse("J-Pop/Ar/Al/1-01 t1.flac").unwrap())
    );
    assert_eq!(
        plan[1].planned,
        Planned::Path(RelPath::parse("J-Pop/Ar/Al/2-01 t2.flac").unwrap())
    );
}

#[tokio::test]
async fn plan_rename_keeps_incumbent_and_reports_conflict() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add_tagged("_Unsorted/Ar/Al/01 t.flac", 1, "t", "Al", "Ar", 1, 1));
    lib.add_tagged("old/y.flac", 2, "u", "Al", "Ar", 2, 1);
    lib.scan().await;
    let x = lib.track_id("_Unsorted/Ar/Al/01 t.flac");
    let y = lib.track_id("old/y.flac");
    let plan = lib.editor.plan_rename(&[x, y], &layout()).await.unwrap();
    assert_eq!(plan[0].planned, Planned::Unchanged);
    // y は old/ の別 album（同名）なので降格するが年が無い → conflict
    assert!(
        matches!(plan[1].planned, Planned::Conflict(_)),
        "{:?}",
        plan[1].planned
    );
}

// ---------------------------------------------------------------- ジョブ側の防御

#[tokio::test]
async fn rename_job_for_terminal_batch_is_a_noop() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let prepared = lib
        .editor
        .prepare_rename(None, vec![target(a, "B/01.flac")])
        .await
        .unwrap();
    lib.editor.cancel_batch(prepared.batch_id).await.unwrap();
    // cancel でジョブは cancelled。手で queued に戻して走らせても何もしない
    lib.conn()
        .execute(
            "UPDATE jobs SET state = 'queued', cancel_requested_at = NULL WHERE id = ?1",
            [prepared.job_ids[0]],
        )
        .unwrap();
    lib.start();
    assert_eq!(
        lib.wait_job_terminal(prepared.job_ids[0]).await,
        JobState::Done
    );
    assert_eq!(files_in(&lib.lib()), ["A/01.flac"]);
    assert_eq!(lib.rel_path(a), "A/01.flac");
}

// ---------------------------------------------------------------- 同梱ファイルの追随（D-43 / D-67）

#[tokio::test]
async fn whole_album_move_carries_companion_files_and_leaves_unknown_files() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.add("A/02.flac", 2, "b");
    std::fs::write(lib.path("A/cover.jpg"), b"jpg").unwrap();
    std::fs::write(lib.path("A/disc.cue"), b"cue").unwrap();
    std::fs::write(lib.path("A/disc.toc"), b"toc").unwrap();
    std::fs::write(lib.path("A/rip.log"), b"spindle rip log v1\n").unwrap();
    // 未知の名前は動かさない
    std::fs::write(lib.path("A/notes.txt"), b"keep").unwrap();
    lib.scan().await;
    let (a, b) = (lib.track_id("A/01.flac"), lib.track_id("A/02.flac"));
    let prepared = lib
        .editor
        .prepare_rename(
            None,
            vec![target(a, "X/Y/01.flac"), target(b, "X/Y/02.flac")],
        )
        .await
        .unwrap();
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );
    for n in ["cover.jpg", "disc.cue", "disc.toc", "rip.log"] {
        assert!(
            lib.path(&format!("X/Y/{n}")).exists(),
            "{n} が追随していない"
        );
        assert!(
            !lib.path(&format!("A/{n}")).exists(),
            "{n} が旧ディレクトリに残っている"
        );
    }
    // 未知のファイルが残るので旧ディレクトリは消さない
    assert!(lib.path("A/notes.txt").exists());
    assert!(lib.path("A").is_dir());
}

#[tokio::test]
async fn partial_move_leaves_companions_and_last_track_move_carries_them_and_removes_dir() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.add("A/02.flac", 2, "b");
    std::fs::write(lib.path("A/cover.jpg"), b"jpg").unwrap();
    lib.scan().await;
    let (a, b) = (lib.track_id("A/01.flac"), lib.track_id("A/02.flac"));
    lib.start();
    // 一部だけ動かす → 同梱ファイルは残る
    let p1 = lib
        .editor
        .prepare_rename(None, vec![target(a, "B/01.flac")])
        .await
        .unwrap();
    assert_eq!(
        lib.wait_batch_terminal(p1.batch_id).await,
        BatchState::Applied
    );
    assert!(lib.path("A/cover.jpg").exists());
    assert!(!lib.path("B/cover.jpg").exists());
    // 残りも同じ宛先へ → 旧ディレクトリから active な行が消えるので追随し、空になったので消える
    let p2 = lib
        .editor
        .prepare_rename(None, vec![target(b, "B/02.flac")])
        .await
        .unwrap();
    assert_eq!(
        lib.wait_batch_terminal(p2.batch_id).await,
        BatchState::Applied
    );
    assert!(lib.path("B/cover.jpg").exists());
    assert!(!lib.path("A").exists());
}

#[tokio::test]
async fn companion_conflict_is_left_in_place_and_revert_moves_back() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    std::fs::write(lib.path("A/cover.jpg"), b"new").unwrap();
    std::fs::write(lib.path("A/rip.log"), b"spindle rip log v1\n").unwrap();
    std::fs::create_dir_all(lib.path("B")).unwrap();
    std::fs::write(lib.path("B/cover.jpg"), b"old").unwrap();
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    lib.start();
    let p = lib
        .editor
        .prepare_rename(None, vec![target(a, "B/01.flac")])
        .await
        .unwrap();
    assert_eq!(
        lib.wait_batch_terminal(p.batch_id).await,
        BatchState::Applied
    );
    // 宛先に同名があれば動かさず（警告）、旧ディレクトリも残る。他の同梱ファイルは動く
    assert_eq!(std::fs::read(lib.path("A/cover.jpg")).unwrap(), b"new");
    assert_eq!(std::fs::read(lib.path("B/cover.jpg")).unwrap(), b"old");
    assert!(lib.path("B/rip.log").exists());
    assert!(lib.path("A").is_dir());
    // 巻き戻し（逆向きの album 全体の移動）: rip.log は戻り、B の cover は A に同名があるので残る
    let r = lib.editor.revert_batch(p.batch_id, None).await.unwrap();
    assert_eq!(
        lib.wait_batch_terminal(r.batch_id).await,
        BatchState::Applied
    );
    assert!(lib.path("A/01.flac").exists());
    assert!(lib.path("A/rip.log").exists());
    assert_eq!(std::fs::read(lib.path("A/cover.jpg")).unwrap(), b"new");
    assert_eq!(std::fs::read(lib.path("B/cover.jpg")).unwrap(), b"old");
    assert!(lib.path("B").is_dir());
}

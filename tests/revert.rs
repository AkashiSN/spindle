//! 巻き戻し（SPEC §7.5「巻き戻し」、docs/TASKS.md P0-12）。tags / rename / delete の逆バッチ、
//! 対象集合（applied − 逆バッチで applied 済み）、conflict、reverted_at、redo。
//! 合成ファイルは ffmpeg で 1 本作り、コピーして増やす

#![cfg(target_os = "linux")]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use spindle::db::history::{self, BatchState, OpKind, OpResult};
use spindle::db::Db;
use spindle::domain::tags::read_audio_file;
use spindle::edit::{EditError, Editor, NewTagOp, RenameTarget, RevertError, TagChange};
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, ScanReport, Scanner};
use spindle::jobs::handlers::rename::RenameHandler;
use spindle::jobs::handlers::tagwrite::TagwriteHandler;
use spindle::jobs::{JobType, Jobs, Registry};

// ---------------------------------------------------------------- ハーネス

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
            jobs,
            scanner,
            editor,
            shutdown: CancellationToken::new(),
        }
    }

    fn start(&self) {
        let mut reg = Registry::new();
        reg.register(
            JobType::Rename,
            Arc::new(RenameHandler::new(self.editor.clone())),
        );
        reg.register(
            JobType::Tagwrite,
            Arc::new(TagwriteHandler::new(self.editor.clone())),
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
        let seed_file = self.dir.path().join(format!(".seed{seed}.flac"));
        if !seed_file.exists() {
            common::make_audio(self.dir.path(), &format!(".seed{seed}.flac"), "flac", seed)?;
        }
        std::fs::copy(&seed_file, &p).unwrap();
        common::set_basic_tags(&p, title, "Artist", album, albumartist, track, disc);
        Some(p)
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

    fn rel_path(&self, id: i64) -> String {
        self.conn()
            .query_row("SELECT rel_path FROM tracks WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
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

    async fn wait_batch_terminal(&self, id: i64) -> BatchState {
        for _ in 0..1000 {
            let st = self.batch_state(id);
            if !matches!(st, BatchState::Prepared | BatchState::Applying) {
                return st;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("batch {id} が終端にならない: {:?}", self.batch_state(id));
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

fn set(track_id: i64, key: &str, value: &str) -> NewTagOp {
    NewTagOp {
        track_id,
        changes: vec![TagChange {
            key: key.to_owned(),
            values: Some(vec![value.to_owned()]),
        }],
    }
}

fn file_tags(path: &Path, key: &str) -> Vec<String> {
    let ext = path.extension().and_then(|e| e.to_str());
    let af = read_audio_file(std::fs::File::open(path).unwrap(), ext).unwrap();
    af.tags.values(key).map(str::to_owned).collect()
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

impl Lib {
    fn tag_values(&self, track_id: i64, key: &str) -> Vec<String> {
        self.conn()
            .prepare("SELECT value FROM track_tags WHERE track_id = ?1 AND key = ?2 ORDER BY idx")
            .unwrap()
            .query_map(rusqlite::params![track_id, key], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn batch(&self, id: i64) -> history::Batch {
        history::get_batch(&self.conn(), id).unwrap().unwrap()
    }

    /// タグ編集バッチを作って反映まで待つ
    async fn edit_titles(&self, ids: &[i64], prefix: &str) -> i64 {
        let ops = ids
            .iter()
            .enumerate()
            .map(|(i, id)| set(*id, "TITLE", &format!("{prefix}{i}")))
            .collect();
        let p = self.editor.prepare_tags(None, ops).await.unwrap();
        assert_eq!(
            self.wait_batch_terminal(p.batch_id).await,
            BatchState::Applied
        );
        p.batch_id
    }
}

// ---------------------------------------------------------------- tags

#[tokio::test]
async fn revert_of_tags_batch_restores_files_and_marks_original_reverted() {
    let lib = Lib::new();
    let pa = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    let pb = lib.add("A/02.flac", 1, "b").unwrap();
    lib.scan().await;
    lib.start();
    let (a, b) = (lib.track_id("A/01.flac"), lib.track_id("A/02.flac"));
    let orig = lib.edit_titles(&[a, b], "new").await;
    assert_eq!(file_tags(&pa, "TITLE"), ["new0"]);

    let r = lib.editor.revert_batch(orig, Some("戻す")).await.unwrap();
    assert_eq!(r.affected, 2);
    assert_eq!(r.conflict, 0);
    let rb = lib.batch(r.batch_id);
    assert_eq!(rb.reverts_batch_id, Some(orig));
    assert_eq!(rb.description.as_deref(), Some("戻す"));
    // 逆バッチの op は edits の old / new が入れ替わる
    let ops = lib.ops(r.batch_id);
    assert_eq!(ops.len(), 2);
    let oa = ops.iter().find(|o| o.track_id == a).unwrap();
    let edits = history::list_edits(&lib.conn(), oa.id).unwrap();
    assert_eq!(edits[0].key, "TITLE");
    assert_eq!(edits[0].old_value, serde_json::json!(["new0"]));
    assert_eq!(edits[0].new_value, serde_json::json!(["a"]));
    // DB 先行更新
    assert_eq!(lib.tag_values(a, "TITLE"), ["a"]);

    assert_eq!(
        lib.wait_batch_terminal(r.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(file_tags(&pa, "TITLE"), ["a"]);
    assert_eq!(file_tags(&pb, "TITLE"), ["b"]);
    // 元バッチの reverted_at は逆バッチが終端になり全件 applied のときだけ立つ
    assert!(lib.batch(orig).reverted_at.is_some());
    assert!(lib.batch(r.batch_id).reverted_at.is_none());
    // 版は進む（Derived の追随のため）、再スキャンで差分は出ない
    let report = lib.scan().await;
    assert_eq!((report.new, report.updated, report.moved), (0, 0, 0));
}

#[tokio::test]
async fn revert_requires_terminal_batch_and_rejects_fully_reverted_batch() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    // prepared のまま（ワーカー未起動）
    let p = lib
        .editor
        .prepare_tags(None, vec![set(a, "TITLE", "x")])
        .await
        .unwrap();
    let err = lib.editor.revert_batch(p.batch_id, None).await.unwrap_err();
    assert!(matches!(err, RevertError::NotTerminal), "{err}");
    assert!(matches!(
        lib.editor.revert_batch(9999, None).await.unwrap_err(),
        RevertError::NotFound
    ));
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(p.batch_id).await,
        BatchState::Applied
    );
    let r = lib.editor.revert_batch(p.batch_id, None).await.unwrap();
    assert_eq!(
        lib.wait_batch_terminal(r.batch_id).await,
        BatchState::Applied
    );
    // 全件戻し済みの再 revert は拒否
    let err = lib.editor.revert_batch(p.batch_id, None).await.unwrap_err();
    assert!(matches!(err, RevertError::AlreadyReverted), "{err}");
}

#[tokio::test]
async fn redo_is_revert_of_the_reverse_batch() {
    let lib = Lib::new();
    let pa = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.scan().await;
    lib.start();
    let a = lib.track_id("A/01.flac");
    let orig = lib.edit_titles(&[a], "new").await;
    let r1 = lib.editor.revert_batch(orig, None).await.unwrap();
    assert_eq!(
        lib.wait_batch_terminal(r1.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(file_tags(&pa, "TITLE"), ["a"]);
    // やり直し = 逆バッチの revert
    let r2 = lib.editor.revert_batch(r1.batch_id, None).await.unwrap();
    assert_eq!(lib.batch(r2.batch_id).reverts_batch_id, Some(r1.batch_id));
    assert_eq!(
        lib.wait_batch_terminal(r2.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(file_tags(&pa, "TITLE"), ["new0"]);
    assert!(lib.batch(r1.batch_id).reverted_at.is_some());
    // 元バッチは戻し済みのまま（r1 が全件 applied だった事実は変わらない）
    assert!(lib.batch(orig).reverted_at.is_some());
}

#[tokio::test]
async fn externally_changed_track_conflicts_and_the_rest_are_reverted() {
    let lib = Lib::new();
    let pa = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    let pb = lib.add("A/02.flac", 1, "b").unwrap();
    let pc = lib.add("A/03.flac", 1, "c").unwrap();
    lib.scan().await;
    lib.start();
    let (a, b, c) = (
        lib.track_id("A/01.flac"),
        lib.track_id("A/02.flac"),
        lib.track_id("A/03.flac"),
    );
    let orig = lib.edit_titles(&[a, b, c], "new").await;
    // b を外部で書き換えてスキャン済み（DB の現在値が元バッチの新値と違う）
    common::retag(&pb, |t| {
        use lofty::tag::Accessor as _;
        t.set_title("外部".to_owned())
    });
    lib.scan().await;
    let r = lib.editor.revert_batch(orig, None).await.unwrap();
    assert_eq!(r.affected, 3);
    assert_eq!(r.conflict, 1);
    assert_eq!(
        lib.wait_batch_terminal(r.batch_id).await,
        BatchState::Partial
    );
    let ops = lib.ops(r.batch_id);
    let ob = ops.iter().find(|o| o.track_id == b).unwrap();
    assert_eq!(ob.result, OpResult::SkippedConflict);
    // 戻そうとした変更は履歴に残る（現在値 → 元の値）
    let edits = history::list_edits(&lib.conn(), ob.id).unwrap();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].old_value, serde_json::json!(["外部"]));
    assert_eq!(edits[0].new_value, serde_json::json!(["b"]));
    assert_eq!(file_tags(&pa, "TITLE"), ["a"]);
    assert_eq!(file_tags(&pb, "TITLE"), ["外部"]);
    assert_eq!(file_tags(&pc, "TITLE"), ["c"]);
    // 1 件残っているので元バッチの reverted_at は立たない
    assert!(lib.batch(orig).reverted_at.is_none());
    // 外部変更を直して（元バッチの新値に戻して）スキャン → 再 revert は残り 1 件だけが対象
    common::retag(&pb, |t| {
        use lofty::tag::Accessor as _;
        t.set_title("new1".to_owned())
    });
    lib.scan().await;
    let r2 = lib.editor.revert_batch(orig, None).await.unwrap();
    assert_eq!(r2.affected, 1);
    assert_eq!(lib.ops(r2.batch_id)[0].track_id, b);
    assert_eq!(
        lib.wait_batch_terminal(r2.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(file_tags(&pb, "TITLE"), ["b"]);
    assert!(lib.batch(orig).reverted_at.is_some());
}

#[tokio::test]
async fn unscanned_external_change_is_caught_by_the_tagwrite_precondition() {
    let lib = Lib::new();
    let pa = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.scan().await;
    lib.start();
    let a = lib.track_id("A/01.flac");
    let orig = lib.edit_titles(&[a], "new").await;
    common::retag(&pa, |t| {
        use lofty::tag::Accessor as _;
        t.set_title("外部".to_owned())
    });
    // スキャンしていないので DB は新値のまま → revert は記録されるが tagwrite が conflict にする
    let r = lib.editor.revert_batch(orig, None).await.unwrap();
    assert_eq!(r.conflict, 0);
    assert_eq!(
        lib.wait_batch_terminal(r.batch_id).await,
        BatchState::Failed
    );
    assert_eq!(file_tags(&pa, "TITLE"), ["外部"]);
    assert_eq!(lib.tag_values(a, "TITLE"), ["外部"]);
    assert!(lib.batch(orig).reverted_at.is_none());
}

#[tokio::test]
async fn revert_with_pending_tracks_is_rejected() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.scan().await;
    lib.start();
    let a = lib.track_id("A/01.flac");
    let orig = lib.edit_titles(&[a], "new").await;
    lib.shutdown.cancel();
    // 別バッチが pending
    lib.editor
        .prepare_tags(None, vec![set(a, "TITLE", "other")])
        .await
        .unwrap();
    let err = lib.editor.revert_batch(orig, None).await.unwrap_err();
    assert!(
        matches!(err, RevertError::Edit(EditError::Pending { ref track_ids }) if track_ids == &[a]),
        "{err}"
    );
}

#[tokio::test]
async fn revert_of_partial_batch_targets_only_applied_ops() {
    let lib = Lib::new();
    let pa = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    let pb = lib.add("A/02.flac", 1, "b").unwrap();
    lib.scan().await;
    let (a, b) = (lib.track_id("A/01.flac"), lib.track_id("A/02.flac"));
    let p = lib
        .editor
        .prepare_tags(None, vec![set(a, "TITLE", "x"), set(b, "TITLE", "y")])
        .await
        .unwrap();
    // b は反映前に外部で変わる → conflict
    common::retag(&pb, |t| {
        use lofty::tag::Accessor as _;
        t.set_title("外部".to_owned())
    });
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(p.batch_id).await,
        BatchState::Partial
    );
    let r = lib.editor.revert_batch(p.batch_id, None).await.unwrap();
    assert_eq!(r.affected, 1);
    assert_eq!(lib.ops(r.batch_id)[0].track_id, a);
    assert_eq!(
        lib.wait_batch_terminal(r.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(file_tags(&pa, "TITLE"), ["a"]);
    assert!(lib.batch(p.batch_id).reverted_at.is_some());
}

#[tokio::test]
async fn revert_of_1000_track_edit_restores_every_title() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/0000.flac", 1, "t0"));
    for i in 1..1000 {
        lib.add(
            &format!("{}/{i:04}.flac", ["A", "B", "C", "D"][i % 4]),
            1,
            &format!("t{i}"),
        )
        .unwrap();
    }
    lib.scan().await;
    lib.start();
    let ids: Vec<i64> = lib
        .conn()
        .prepare("SELECT id FROM tracks ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let orig = lib.edit_titles(&ids, "Track ").await;
    let r = lib.editor.revert_batch(orig, None).await.unwrap();
    assert_eq!(r.affected, 1000);
    assert_eq!(
        lib.wait_batch_terminal(r.batch_id).await,
        BatchState::Applied
    );
    for i in [0usize, 1, 500, 999] {
        let rel = format!("{}/{i:04}.flac", ["A", "B", "C", "D"][i % 4]);
        assert_eq!(file_tags(&lib.path(&rel), "TITLE"), [format!("t{i}")]);
    }
    let report = lib.scan().await;
    assert_eq!((report.new, report.updated), (0, 0));
    assert!(lib.batch(orig).reverted_at.is_some());
}

// ---------------------------------------------------------------- rename

#[tokio::test]
async fn revert_of_rename_batch_moves_files_back() {
    let lib = Lib::new();
    let pa = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.add("A/02.flac", 1, "b");
    lib.scan().await;
    lib.start();
    let (a, b) = (lib.track_id("A/01.flac"), lib.track_id("A/02.flac"));
    let ia = inode_of(&pa);
    let p = lib
        .editor
        .prepare_rename(None, vec![target(a, "B/x.flac"), target(b, "B/y.flac")])
        .await
        .unwrap();
    assert_eq!(
        lib.wait_batch_terminal(p.batch_id).await,
        BatchState::Applied
    );
    let r = lib.editor.revert_batch(p.batch_id, None).await.unwrap();
    assert_eq!(r.affected, 2);
    assert_eq!(lib.batch(r.batch_id).reverts_batch_id, Some(p.batch_id));
    assert!(lib.ops(r.batch_id).iter().all(|o| o.kind == OpKind::Rename));
    assert_eq!(lib.rel_path(a), "A/01.flac");
    assert_eq!(
        lib.wait_batch_terminal(r.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(files_in(&lib.lib()), ["A/01.flac", "A/02.flac"]);
    assert_eq!(inode_of(&lib.path("A/01.flac")), ia);
    assert!(lib.batch(p.batch_id).reverted_at.is_some());
    let report = lib.scan().await;
    assert_eq!((report.new, report.moved, report.missing_marked), (0, 0, 0));
}

#[tokio::test]
async fn revert_of_rename_conflicts_when_file_was_moved_again() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.add("A/02.flac", 1, "b");
    lib.scan().await;
    lib.start();
    let (a, b) = (lib.track_id("A/01.flac"), lib.track_id("A/02.flac"));
    let p = lib
        .editor
        .prepare_rename(None, vec![target(a, "B/x.flac"), target(b, "B/y.flac")])
        .await
        .unwrap();
    assert_eq!(
        lib.wait_batch_terminal(p.batch_id).await,
        BatchState::Applied
    );
    // a はその後別のバッチで動いた（現在値が元バッチの新値と違う）
    let p2 = lib
        .editor
        .prepare_rename(None, vec![target(a, "C/z.flac")])
        .await
        .unwrap();
    assert_eq!(
        lib.wait_batch_terminal(p2.batch_id).await,
        BatchState::Applied
    );
    let r = lib.editor.revert_batch(p.batch_id, None).await.unwrap();
    assert_eq!((r.affected, r.conflict), (2, 1));
    assert_eq!(
        lib.wait_batch_terminal(r.batch_id).await,
        BatchState::Partial
    );
    assert_eq!(files_in(&lib.lib()), ["A/02.flac", "C/z.flac"]);
    assert!(lib.batch(p.batch_id).reverted_at.is_none());
}

#[tokio::test]
async fn revert_of_1000_track_rename_restores_every_path() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/0000.flac", 1, "t0"));
    for i in 1..1000 {
        lib.add(&format!("A/{i:04}.flac"), 1, &format!("t{i}"))
            .unwrap();
    }
    lib.scan().await;
    lib.start();
    let ids: Vec<(i64, String)> = lib
        .conn()
        .prepare("SELECT id, rel_path FROM tracks ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let before = files_in(&lib.lib());
    let targets = ids
        .iter()
        .map(|(id, rel)| target(*id, &rel.replace("A/", "B/C/")))
        .collect();
    let p = lib.editor.prepare_rename(None, targets).await.unwrap();
    assert_eq!(
        lib.wait_batch_terminal(p.batch_id).await,
        BatchState::Applied
    );
    assert!(files_in(&lib.lib()).iter().all(|f| f.starts_with("B/C/")));
    let r = lib.editor.revert_batch(p.batch_id, None).await.unwrap();
    assert_eq!(r.affected, 1000);
    assert_eq!(
        lib.wait_batch_terminal(r.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(files_in(&lib.lib()), before);
    let report = lib.scan().await;
    assert_eq!((report.new, report.moved, report.missing_marked), (0, 0, 0));
}

// ---------------------------------------------------------------- delete

#[tokio::test]
async fn revert_of_delete_batch_clears_missing_since() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    // 論理削除バッチ（生成側は後続タスク）を手で作る
    let batch = {
        let c = lib.conn();
        let batch = history::insert_batch(&c, Some("削除"), 1, None, 1).unwrap();
        let pre = history::precondition_of_track(&c, a).unwrap().unwrap();
        let op = history::insert_op(&c, batch, 0, a, OpKind::Delete, &pre).unwrap();
        history::insert_edit(
            &c,
            op,
            "missing_since",
            &serde_json::Value::Null,
            &serde_json::json!(123),
        )
        .unwrap();
        c.execute("UPDATE tracks SET missing_since = 123 WHERE id = ?1", [a])
            .unwrap();
        history::finish_op(&c, op, OpResult::Applied, None, None, 1).unwrap();
        history::aggregate_batch(&c, batch, 1).unwrap();
        batch
    };
    assert_eq!(lib.missing_since(a), Some(123));
    let r = lib.editor.revert_batch(batch, None).await.unwrap();
    assert_eq!(r.affected, 1);
    // DB だけの操作なので即終端
    assert_eq!(lib.batch_state(r.batch_id), BatchState::Applied);
    assert_eq!(lib.missing_since(a), None);
    assert!(lib.batch(batch).reverted_at.is_some());
    let ops = lib.ops(r.batch_id);
    assert_eq!(ops[0].kind, OpKind::Delete);
    assert_eq!(ops[0].result, OpResult::Applied);
}

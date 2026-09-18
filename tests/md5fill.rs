//! FLAC の MD5 補填（SPEC §7.9、docs/TASKS.md P1-5b、D-57 / D-59）。
//! STREAMINFO の MD5 が全ゼロの FLAC に、デコードした PCM MD5 を書く編集バッチ（`md5` op）。
//! 音声もタグも変わらないので `audio_version` / `tag_version` は据え置き、inode / mtime は追随、
//! 旧値（全ゼロ）を `edits` に残して巻き戻せる。合成ファイルは ffmpeg で作る（無ければ skip）

#![cfg(target_os = "linux")]

mod common;

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension as _};
use tokio_util::sync::CancellationToken;

use spindle::db::history::{self, BatchState, OpKind, OpResult};
use spindle::db::Db;
use spindle::edit::{EditError, Editor};
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, Scanner};
use spindle::jobs::handlers::tagwrite::TagwriteHandler;
use spindle::jobs::{JobType, Jobs, Registry};
use spindle::media::fingerprint::{flac_streaminfo_md5, flac_streaminfo_md5_at};

const ZERO: &str = "00000000000000000000000000000000";

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    jobs: Arc<Jobs>,
    scanner: Scanner,
    editor: Arc<Editor>,
    shutdown: CancellationToken,
}

#[derive(Debug, Clone, PartialEq)]
struct Row {
    audio_md5: Option<Vec<u8>>,
    flac_check: Option<String>,
    audio_version: i64,
    tag_version: i64,
    inode: i64,
    mtime_ns: i64,
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Library");
        std::fs::create_dir(&lib).unwrap();
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let root = Arc::new(RootDir::open(&lib).unwrap());
        let jobs = Jobs::new(db.clone());
        let scanner = Scanner::new(db.clone(), root.clone(), 2);
        let editor = Arc::new(Editor::new(db, root, jobs.clone()));
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
            JobType::Tagwrite,
            Arc::new(TagwriteHandler::new(self.editor.clone())),
        );
        self.jobs.start(reg, self.shutdown.clone());
    }

    fn lib(&self) -> PathBuf {
        self.dir.path().join("Library")
    }

    fn add(&self, rel: &str, seed: u32) -> Option<PathBuf> {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let ext = rel.rsplit_once('.').unwrap().1;
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        let made = common::make_audio(p.parent().unwrap(), &name, ext, seed)?;
        common::set_basic_tags(&made, "T", "Artist", "Album", "AlbumArtist", 1, 1);
        Some(made)
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

    fn track_id(&self, rel: &str) -> i64 {
        self.conn()
            .query_row("SELECT id FROM tracks WHERE rel_path = ?1", [rel], |r| {
                r.get(0)
            })
            .unwrap()
    }

    /// flaccheck ジョブの結果の模擬
    fn set_check(&self, id: i64, status: &str) {
        self.conn()
            .execute(
                "UPDATE tracks SET flac_check = ?2, flac_checked_at = 1, flac_check_version = audio_version
                 WHERE id = ?1",
                params![id, status],
            )
            .unwrap();
    }

    fn row(&self, id: i64) -> Row {
        self.conn()
            .query_row(
                "SELECT audio_md5, flac_check, audio_version, tag_version, inode, mtime_ns FROM tracks WHERE id = ?1",
                [id],
                |r| {
                    Ok(Row {
                        audio_md5: r.get(0)?,
                        flac_check: r.get(1)?,
                        audio_version: r.get(2)?,
                        tag_version: r.get(3)?,
                        inode: r.get(4)?,
                        mtime_ns: r.get(5)?,
                    })
                },
            )
            .unwrap()
    }

    fn batch_state(&self, id: i64) -> BatchState {
        history::get_batch(&self.conn(), id).unwrap().unwrap().state
    }

    fn ops(&self, batch_id: i64) -> Vec<history::Op> {
        history::list_ops(&self.conn(), batch_id).unwrap()
    }

    fn edits(&self, op_id: i64) -> Vec<history::Edit> {
        history::list_edits(&self.conn(), op_id).unwrap()
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

/// STREAMINFO の MD5 を `md5` に書き換える（外部ツールの模擬。inode は変えない）
fn overwrite_md5(path: &std::path::Path, md5: [u8; 16]) {
    let (offset, _) = flac_streaminfo_md5_at(File::open(path).unwrap()).unwrap();
    let mut f = OpenOptions::new().write(true).open(path).unwrap();
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.write_all(&md5).unwrap();
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ---------------------------------------------------------------- 補填

#[tokio::test]
async fn fill_writes_decoded_md5_into_streaminfo_without_touching_versions() {
    let lib = Lib::new();
    let Some(path) = lib.add("A/a.flac", 1) else {
        eprintln!("ffmpeg が無いので skip");
        return;
    };
    // ffmpeg の FLAC は MD5 付き。それを控えてから全ゼロに潰す（MD5 無しの FLAC の模擬）
    let expected = flac_streaminfo_md5(File::open(&path).unwrap())
        .unwrap()
        .unwrap();
    overwrite_md5(&path, [0u8; 16]);
    lib.scan().await;
    let id = lib.track_id("A/a.flac");
    let before = lib.row(id);
    assert_eq!(before.audio_md5, None, "全ゼロはスキャナが未設定として扱う");
    lib.set_check(id, "md5_missing");
    lib.start();

    let p = lib
        .editor
        .prepare_md5_fill(Some("補填"), vec![id])
        .await
        .unwrap();
    let batch_id = p.batch_id.expect("op を記録する");
    assert_eq!((p.affected, p.skipped), (1, 0));
    assert_eq!(p.job_ids.len(), 1);
    let ops = lib.ops(batch_id);
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].kind, OpKind::Md5);
    assert_eq!(ops[0].expected.inode, Some(before.inode));
    let e = lib.edits(ops[0].id);
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].key, "audio_md5");
    assert_eq!(e[0].old_value, serde_json::json!(ZERO));
    assert!(e[0].new_value.is_null(), "計算前は null");

    assert_eq!(lib.wait_batch_terminal(batch_id).await, BatchState::Applied);
    // ファイル: STREAMINFO に正しい MD5、音声はそのまま
    assert_eq!(
        flac_streaminfo_md5(File::open(&path).unwrap()).unwrap(),
        Some(expected)
    );
    // DB: audio_md5 が入り、検査結果は ok、版は据え置き、物理属性は新しい実体に追随
    let after = lib.row(id);
    assert_eq!(after.audio_md5.as_deref(), Some(&expected[..]));
    assert_eq!(after.flac_check.as_deref(), Some("ok"));
    assert_eq!(after.audio_version, before.audio_version);
    assert_eq!(after.tag_version, before.tag_version);
    assert_ne!(after.inode, before.inode, "tmp + rename で実体が変わる");
    let e = lib.edits(ops[0].id);
    assert_eq!(e[0].new_value, serde_json::json!(hex(&expected)));
    assert_eq!(lib.ops(batch_id)[0].result, OpResult::Applied);

    // 再スキャンしても「変更なし」（物理属性が揃っている）
    lib.scan().await;
    assert_eq!(lib.row(id), after);

    // 巻き戻し: 全ゼロに戻り、audio_md5 は NULL、検査結果は md5_missing
    let r = lib
        .editor
        .revert_batch(batch_id, Some("戻す"))
        .await
        .unwrap();
    assert_eq!(
        lib.wait_batch_terminal(r.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(
        flac_streaminfo_md5(File::open(&path).unwrap()).unwrap(),
        None
    );
    let reverted = lib.row(id);
    assert_eq!(reverted.audio_md5, None);
    assert_eq!(reverted.flac_check.as_deref(), Some("md5_missing"));
    assert_eq!(reverted.audio_version, before.audio_version);
    let rops = lib.ops(r.batch_id);
    assert_eq!(rops[0].kind, OpKind::Md5);
    let re = lib.edits(rops[0].id);
    assert_eq!(re[0].old_value, serde_json::json!(hex(&expected)));
    assert_eq!(re[0].new_value, serde_json::json!(ZERO));
}

#[tokio::test]
async fn fill_skips_non_targets_and_refuses_pending() {
    let lib = Lib::new();
    let Some(flac) = lib.add("A/a.flac", 1) else {
        eprintln!("ffmpeg が無いので skip");
        return;
    };
    overwrite_md5(&flac, [0u8; 16]);
    lib.add("A/ok.flac", 2).unwrap();
    lib.add("A/b.m4a", 3).unwrap();
    lib.scan().await;
    let a = lib.track_id("A/a.flac");
    let ok = lib.track_id("A/ok.flac");
    let m4a = lib.track_id("A/b.m4a");
    lib.set_check(a, "md5_missing");
    lib.set_check(ok, "ok");
    // missing の行は対象外
    lib.conn()
        .execute(
            "INSERT INTO tracks (rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless, seen_at,
                                 missing_since, flac_check)
             VALUES ('gone.flac', 'gone.flac', 1, 0, 0, 'flac', 1, 0, 1, 'md5_missing')",
            [],
        )
        .unwrap();
    let gone = lib.track_id("gone.flac");

    // 対象が無ければ NoChanges（バッチも記録しない）
    let err = lib
        .editor
        .prepare_md5_fill(None, vec![ok, m4a, gone])
        .await
        .unwrap_err();
    assert!(matches!(err, EditError::NoChanges), "{err:?}");

    let p = lib
        .editor
        .prepare_md5_fill(None, vec![a, ok, m4a, gone])
        .await
        .unwrap();
    assert_eq!((p.affected, p.skipped), (1, 3));
    let batch_id = p.batch_id.unwrap();
    assert_eq!(lib.ops(batch_id).len(), 1);

    // 反映待ちがあれば Pending
    let err = lib
        .editor
        .prepare_md5_fill(None, vec![a])
        .await
        .unwrap_err();
    assert!(
        matches!(err, EditError::Pending { ref track_ids } if track_ids == &vec![a]),
        "{err:?}"
    );
    assert!(matches!(
        lib.editor
            .prepare_md5_fill(None, vec![9999])
            .await
            .unwrap_err(),
        EditError::TrackNotFound(9999)
    ));
}

#[tokio::test]
async fn fill_conflicts_when_file_already_has_md5_or_changed() {
    let lib = Lib::new();
    let Some(path) = lib.add("A/a.flac", 1) else {
        eprintln!("ffmpeg が無いので skip");
        return;
    };
    let real = flac_streaminfo_md5(File::open(&path).unwrap())
        .unwrap()
        .unwrap();
    overwrite_md5(&path, [0u8; 16]);
    lib.scan().await;
    let id = lib.track_id("A/a.flac");
    lib.set_check(id, "md5_missing");

    // 記録の後、反映の前に外部ツールが補填した（inode は同じ、内容だけ違う）
    let p = lib.editor.prepare_md5_fill(None, vec![id]).await.unwrap();
    let batch_id = p.batch_id.unwrap();
    let op_id = lib.ops(batch_id)[0].id;
    overwrite_md5(&path, real);
    let outcome = lib.editor.apply_op(op_id, None).await.unwrap();
    assert!(
        matches!(outcome, spindle::edit::OpOutcome::Conflict(_)),
        "{outcome:?}"
    );
    assert_eq!(lib.ops(batch_id)[0].result, OpResult::SkippedConflict);
    assert_eq!(
        lib.wait_batch_terminal(batch_id).await,
        BatchState::Failed,
        "全件 conflict は failed"
    );
    // ファイルは触らず、DB はファイルの現在値（補填済み）に揃う
    assert_eq!(
        flac_streaminfo_md5(File::open(&path).unwrap()).unwrap(),
        Some(real)
    );
    let row = lib.row(id);
    assert_eq!(row.audio_md5.as_deref(), Some(&real[..]));
    assert_eq!(row.audio_version, 1, "同じ音声なので版は進まない");
}

// ---------------------------------------------------------------- 競合とクラッシュ

#[tokio::test]
async fn revert_enqueues_its_own_job_even_while_the_original_job_is_still_running() {
    // md5 op は tag_version を進めないので、tags と同じ dedup key（tagwrite:<id>:<ver>）だと、
    // 元ジョブが running のうちに巻き戻すと逆 op のジョブが Duplicate になって二度と走らない
    let lib = Lib::new();
    let Some(path) = lib.add("A/a.flac", 1) else {
        eprintln!("ffmpeg が無いので skip");
        return;
    };
    overwrite_md5(&path, [0u8; 16]);
    lib.scan().await;
    let id = lib.track_id("A/a.flac");
    lib.set_check(id, "md5_missing");
    // ワーカー無し。元ジョブを running に見せかけ、op は直接反映する
    let p = lib.editor.prepare_md5_fill(None, vec![id]).await.unwrap();
    let batch_id = p.batch_id.unwrap();
    let orig_job = p.job_ids[0];
    lib.conn()
        .execute(
            "UPDATE jobs SET state = 'running', started_at = 1 WHERE id = ?1",
            [orig_job],
        )
        .unwrap();
    let op_id = lib.ops(batch_id)[0].id;
    assert_eq!(
        lib.editor.apply_op(op_id, Some(orig_job)).await.unwrap(),
        spindle::edit::OpOutcome::Applied
    );
    assert_eq!(lib.batch_state(batch_id), BatchState::Applied);

    let r = lib.editor.revert_batch(batch_id, None).await.unwrap();
    assert_eq!(r.job_ids.len(), 1, "逆 op に自分のジョブが付く");
    assert_ne!(r.job_ids[0], orig_job, "running の元ジョブに相乗りしない");
    let rop = &lib.ops(r.batch_id)[0];
    assert_eq!(rop.job_id, Some(r.job_ids[0]));
    let key: String = lib
        .conn()
        .query_row(
            "SELECT dedup_key FROM jobs WHERE id = ?1",
            [r.job_ids[0]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(key, format!("tagwrite:{id}:md5:{}", rop.id));
}

#[tokio::test]
async fn crash_after_rename_before_db_commit_is_recovered_as_applied() {
    // rename 済み・DB 未確定でプロセスが落ちた状態を作る: new_value は rename の前に耐久化されている
    // 前提で、ファイルは新 inode かつ補填済み、op は pending のまま。再実行で applied に確定し、
    // 巻き戻しもできること（「ジョブは冪等」「破壊的操作は巻き戻せる」）
    let lib = Lib::new();
    let Some(path) = lib.add("A/a.flac", 1) else {
        eprintln!("ffmpeg が無いので skip");
        return;
    };
    let real = flac_streaminfo_md5(File::open(&path).unwrap())
        .unwrap()
        .unwrap();
    overwrite_md5(&path, [0u8; 16]);
    lib.scan().await;
    let id = lib.track_id("A/a.flac");
    lib.set_check(id, "md5_missing");
    let p = lib.editor.prepare_md5_fill(None, vec![id]).await.unwrap();
    let batch_id = p.batch_id.unwrap();
    let op_id = lib.ops(batch_id)[0].id;

    // rename の前に new_value が耐久化されることを、rename 直前のフックで DB を読んで確かめる
    let seen = Arc::new(AtomicBool::new(false));
    let seen2 = Arc::clone(&seen);
    let db_path = lib.db_path.clone();
    lib.editor
        .set_before_rename_hook(Arc::new(move |_rel: &str| {
            let v: Option<String> = Connection::open(&db_path)
                .unwrap()
                .query_row(
                    "SELECT new_value FROM edits WHERE op_id = ?1 AND key = 'audio_md5'",
                    [op_id],
                    |r| r.get(0),
                )
                .optional()
                .unwrap();
            if v.as_deref() == Some(&serde_json::json!(hex(&real)).to_string()) {
                seen2.store(true, Ordering::SeqCst);
            }
        }));

    // クラッシュの模擬: 反映を途中まで手で再現する。計算値を耐久化し（phase B）、ファイルを
    // 新 inode + 補填済みにし（rename 済み）、DB の op / tracks はそのまま
    lib.conn()
        .execute(
            "UPDATE edits SET new_value = ?2 WHERE op_id = ?1 AND key = 'audio_md5'",
            params![op_id, serde_json::json!(hex(&real)).to_string()],
        )
        .unwrap();
    let tmp = path.with_extension("flac.crash");
    std::fs::copy(&path, &tmp).unwrap();
    overwrite_md5(&tmp, real);
    std::fs::rename(&tmp, &path).unwrap();

    // 再実行（recover が再投入したジョブの本体）: 事前条件は外れているがファイルは新値 → applied
    assert_eq!(
        lib.editor.apply_op(op_id, None).await.unwrap(),
        spindle::edit::OpOutcome::Applied
    );
    assert_eq!(lib.batch_state(batch_id), BatchState::Applied);
    let row = lib.row(id);
    assert_eq!(row.audio_md5.as_deref(), Some(&real[..]));
    assert_eq!(row.flac_check.as_deref(), Some("ok"));
    assert_eq!(row.audio_version, 1);
    assert!(
        !seen.load(Ordering::SeqCst),
        "書かずに確定したので rename は起きない"
    );

    // 巻き戻せる（ワーカーで反映。rename 直前に new_value が耐久化されているのをフックで見る）
    lib.start();
    let r = lib.editor.revert_batch(batch_id, None).await.unwrap();
    assert_eq!(
        lib.wait_batch_terminal(r.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(
        flac_streaminfo_md5(File::open(&path).unwrap()).unwrap(),
        None
    );
    assert_eq!(lib.row(id).audio_md5, None);
}

#[tokio::test]
async fn new_value_is_durable_before_rename() {
    let lib = Lib::new();
    let Some(path) = lib.add("A/a.flac", 1) else {
        eprintln!("ffmpeg が無いので skip");
        return;
    };
    let real = flac_streaminfo_md5(File::open(&path).unwrap())
        .unwrap()
        .unwrap();
    overwrite_md5(&path, [0u8; 16]);
    lib.scan().await;
    let id = lib.track_id("A/a.flac");
    lib.set_check(id, "md5_missing");
    let p = lib.editor.prepare_md5_fill(None, vec![id]).await.unwrap();
    let op_id = lib.ops(p.batch_id.unwrap())[0].id;
    let seen = Arc::new(AtomicBool::new(false));
    let seen2 = Arc::clone(&seen);
    let db_path = lib.db_path.clone();
    lib.editor
        .set_before_rename_hook(Arc::new(move |_rel: &str| {
            let v: Option<String> = Connection::open(&db_path)
                .unwrap()
                .query_row(
                    "SELECT new_value FROM edits WHERE op_id = ?1 AND key = 'audio_md5'",
                    [op_id],
                    |r| r.get(0),
                )
                .optional()
                .unwrap();
            seen2.store(
                v.as_deref() == Some(&serde_json::json!(hex(&real)).to_string()),
                Ordering::SeqCst,
            );
        }));
    assert_eq!(
        lib.editor.apply_op(op_id, None).await.unwrap(),
        spindle::edit::OpOutcome::Applied
    );
    assert!(
        seen.load(Ordering::SeqCst),
        "rename の前に計算値が edits に耐久化されている"
    );
}

#[tokio::test]
async fn md5_job_is_not_stale_gated_by_tag_version() {
    // md5 op は tags overlay を持たず tag_version も進めない。補填が queued の間に外部ツールが
    // タグを書き換えて scan が tag_version を進めても、ジョブは stale として捨てられず反映される
    let lib = Lib::new();
    let Some(path) = lib.add("A/a.flac", 1) else {
        eprintln!("ffmpeg が無いので skip");
        return;
    };
    let expected = flac_streaminfo_md5(File::open(&path).unwrap())
        .unwrap()
        .unwrap();
    overwrite_md5(&path, [0u8; 16]);
    lib.scan().await;
    let id = lib.track_id("A/a.flac");
    lib.set_check(id, "md5_missing");
    let p = lib.editor.prepare_md5_fill(None, vec![id]).await.unwrap();
    let batch_id = p.batch_id.unwrap();
    // 外部のタグ変更をスキャンが拾った状態（tag_version が進む。物理属性は補填の事前条件に
    // 影響しないよう DB 側だけ進める）
    lib.conn()
        .execute(
            "UPDATE tracks SET tag_version = tag_version + 1 WHERE id = ?1",
            [id],
        )
        .unwrap();
    lib.start();
    assert_eq!(lib.wait_batch_terminal(batch_id).await, BatchState::Applied);
    assert_eq!(
        flac_streaminfo_md5(File::open(&path).unwrap()).unwrap(),
        Some(expected)
    );
    let job_state: String = lib
        .conn()
        .query_row(
            "SELECT state FROM jobs WHERE id = ?1",
            [p.job_ids[0]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(job_state, "done");
}

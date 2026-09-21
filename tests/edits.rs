//! 編集履歴の記録機構（SPEC §7.5、docs/TASKS.md P0-9、D-24）。
//! 記録 → DB 先行更新 → track 単位の tagwrite → 事前条件 → overlay の解消 → 集計 → キャンセル →
//! 起動時リカバリ。合成ファイルは ffmpeg で作る（無ければ skip）。

#![cfg(target_os = "linux")]

mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lofty::tag::Accessor;
use rusqlite::{params, Connection, OptionalExtension};
use tokio_util::sync::CancellationToken;

use spindle::db::history::{self, BatchState, OpResult};
use spindle::db::Db;
use spindle::domain::tags::read_audio_file;
use spindle::edit::{CancelOutcome, EditError, Editor, NewTagOp, OpOutcome, TagChange};
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, ScanReport, Scanner};
use spindle::jobs::handlers::tagwrite::TagwriteHandler;
use spindle::jobs::{Event, JobState, JobType, Jobs, Registry};

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

#[derive(Debug, Clone, PartialEq)]
struct Phys {
    dev: i64,
    inode: i64,
    size: i64,
    mtime_ns: i64,
    ctime_ns: i64,
    tag_hash: Vec<u8>,
    tag_version: i64,
    rel_path: String,
    title: Option<String>,
    artist_display: Option<String>,
    albumartist: Option<String>,
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
        let lib = dir.path().join("Library");
        let db_path = dir.path().join("spindle.db");
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

    fn path(&self, rel: &str) -> PathBuf {
        self.lib().join(rel)
    }

    fn add(&self, rel: &str, seed: u32, title: &str) -> Option<PathBuf> {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let ext = rel.rsplit_once('.').unwrap().1;
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        let made = common::make_audio(p.parent().unwrap(), &name, ext, seed)?;
        common::set_basic_tags(&made, title, "Artist", "Album", "AlbumArtist", 1, 1);
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

    fn phys(&self, id: i64) -> Phys {
        self.conn()
            .query_row(
                "SELECT dev, inode, size, mtime_ns, ctime_ns, tag_hash, tag_version, rel_path,
                        title, artist_display, albumartist
                 FROM tracks WHERE id = ?1",
                [id],
                |r| {
                    Ok(Phys {
                        dev: r.get(0)?,
                        inode: r.get(1)?,
                        size: r.get(2)?,
                        mtime_ns: r.get(3)?,
                        ctime_ns: r.get(4)?,
                        tag_hash: r.get(5)?,
                        tag_version: r.get(6)?,
                        rel_path: r.get(7)?,
                        title: r.get(8)?,
                        artist_display: r.get(9)?,
                        albumartist: r.get(10)?,
                    })
                },
            )
            .unwrap()
    }

    fn tag_values(&self, track_id: i64, key: &str) -> Vec<String> {
        self.conn()
            .prepare("SELECT value FROM track_tags WHERE track_id = ?1 AND key = ?2 ORDER BY idx")
            .unwrap()
            .query_map(params![track_id, key], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
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

fn set(key: &str, values: &[&str]) -> TagChange {
    TagChange {
        key: key.to_owned(),
        values: Some(values.iter().map(|s| (*s).to_owned()).collect()),
    }
}

fn del(key: &str) -> TagChange {
    TagChange {
        key: key.to_owned(),
        values: None,
    }
}

fn op(track_id: i64, changes: Vec<TagChange>) -> NewTagOp {
    NewTagOp { track_id, changes }
}

/// ファイルのタグ（大文字キー、多値は順序どおり）
fn file_tags(path: &Path, key: &str) -> Vec<String> {
    let ext = path.extension().and_then(|e| e.to_str());
    let af = read_audio_file(File::open(path).unwrap(), ext).unwrap();
    af.tags.values(key).map(str::to_owned).collect()
}

fn file_tag_hash(path: &Path) -> Vec<u8> {
    let ext = path.extension().and_then(|e| e.to_str());
    let af = read_audio_file(File::open(path).unwrap(), ext).unwrap();
    spindle::domain::tags::tag_hash(&af.tags).to_vec()
}

fn inode_of(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).unwrap().ino()
}

fn three_field_edit(track_id: i64) -> NewTagOp {
    op(
        track_id,
        vec![
            set("TITLE", &["新しい題"]),
            set("ARTIST", &["A", "B"]),
            del("ALBUMARTIST"),
        ],
    )
}

// ---------------------------------------------------------------- 記録と DB 先行更新

#[tokio::test]
async fn prepare_records_batch_ops_edits_and_overlays_db() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/01.flac", 1, "旧い題"));
    lib.scan().await;
    let id = lib.track_id("A/01.flac");
    let before = lib.phys(id);

    let prepared = lib
        .editor
        .prepare_tags(Some("3 フィールド"), vec![three_field_edit(id)])
        .await
        .unwrap();
    assert_eq!(prepared.affected, 1);
    assert_eq!(prepared.job_ids.len(), 1);

    // バッチと op
    let batch = history::get_batch(&lib.conn(), prepared.batch_id)
        .unwrap()
        .unwrap();
    assert_eq!(batch.state, BatchState::Prepared);
    assert_eq!(batch.affected, Some(1));
    assert_eq!(batch.description.as_deref(), Some("3 フィールド"));
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(ops.len(), 1);
    let o = &ops[0];
    assert_eq!(o.track_id, id);
    assert_eq!(o.result, OpResult::Pending);
    assert_eq!(o.kind, history::OpKind::Tags);
    // 事前条件は記録時点の行の実体
    assert_eq!(o.expected.dev, Some(before.dev));
    assert_eq!(o.expected.inode, Some(before.inode));
    assert_eq!(o.expected.size, Some(before.size));
    assert_eq!(o.expected.mtime_ns, Some(before.mtime_ns));
    assert_eq!(o.expected.ctime_ns, Some(before.ctime_ns));
    assert_eq!(
        o.expected.tag_hash.as_deref(),
        Some(before.tag_hash.as_slice())
    );
    assert_eq!(o.expected.rel_path.as_deref(), Some("A/01.flac"));
    assert_eq!(o.job_id, Some(prepared.job_ids[0]));

    // edits は JSON。タグ不存在は null
    let edits = history::list_edits(&lib.conn(), o.id).unwrap();
    let find = |k: &str| edits.iter().find(|e| e.key == k).unwrap();
    assert_eq!(find("TITLE").old_value, serde_json::json!(["旧い題"]));
    assert_eq!(find("TITLE").new_value, serde_json::json!(["新しい題"]));
    assert_eq!(find("ARTIST").old_value, serde_json::json!(["Artist"]));
    assert_eq!(find("ARTIST").new_value, serde_json::json!(["A", "B"]));
    assert_eq!(
        find("ALBUMARTIST").old_value,
        serde_json::json!(["AlbumArtist"])
    );
    assert_eq!(find("ALBUMARTIST").new_value, serde_json::Value::Null);

    // DB は先行更新（overlay）。tag_version はトラックごとに 1 回だけ
    let after = lib.phys(id);
    assert_eq!(after.tag_version, before.tag_version + 1);
    assert_eq!(after.title.as_deref(), Some("新しい題"));
    assert_eq!(after.artist_display.as_deref(), Some("A, B"));
    // ALBUMARTIST を消したので ARTIST の先頭に落ちる
    assert_eq!(after.albumartist.as_deref(), Some("A"));
    assert_eq!(lib.tag_values(id, "ARTIST"), ["A", "B"]);
    assert!(lib.tag_values(id, "ALBUMARTIST").is_empty());
    assert_ne!(after.tag_hash, before.tag_hash);
    // 物理属性は据え置き（ファイルはまだ触っていない）
    assert_eq!(after.inode, before.inode);
    assert_eq!(after.mtime_ns, before.mtime_ns);
    assert_eq!(file_tags(&p, "TITLE"), ["旧い題"]);

    // tagwrite ジョブが batch に紐づいて queued
    let (ty, key, batch_id, payload): (String, String, i64, String) = lib
        .conn()
        .query_row(
            "SELECT type, dedup_key, edit_batch_id, payload FROM jobs WHERE id = ?1",
            [prepared.job_ids[0]],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(ty, "tagwrite");
    assert_eq!(key, format!("tagwrite:{id}:{}", after.tag_version));
    assert_eq!(batch_id, prepared.batch_id);
    let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(payload["track_id"], id);
    assert_eq!(payload["tag_version"], after.tag_version);
    assert_eq!(payload["op_id"], o.id);
}

#[tokio::test]
async fn prepare_skips_ops_without_actual_change() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "同じ"));
    require_ffmpeg!(lib.add("A/02.flac", 2, "違う"));
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let b = lib.track_id("A/02.flac");

    let prepared = lib
        .editor
        .prepare_tags(
            None,
            vec![
                op(a, vec![set("TITLE", &["同じ"])]),
                op(b, vec![set("TITLE", &["変える"])]),
            ],
        )
        .await
        .unwrap();
    assert_eq!(prepared.affected, 1);
    assert_eq!(prepared.unchanged, 1);
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].track_id, b);
    assert_eq!(lib.phys(a).tag_version, 1);

    // 全件無変更ならバッチを作らない
    let err = lib
        .editor
        .prepare_tags(None, vec![op(a, vec![set("TITLE", &["同じ"])])])
        .await
        .unwrap_err();
    assert!(matches!(err, EditError::NoChanges), "{err:?}");
    let n: i64 = lib
        .conn()
        .query_row("SELECT count(*) FROM edit_batches", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn new_op_on_pending_track_is_rejected() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "t"));
    require_ffmpeg!(lib.add("A/02.flac", 2, "t"));
    lib.scan().await;
    let a = lib.track_id("A/01.flac");
    let b = lib.track_id("A/02.flac");

    lib.editor
        .prepare_tags(None, vec![op(a, vec![set("TITLE", &["x"])])])
        .await
        .unwrap();
    let err = lib
        .editor
        .prepare_tags(
            None,
            vec![
                op(a, vec![set("TITLE", &["y"])]),
                op(b, vec![set("TITLE", &["y"])]),
            ],
        )
        .await
        .unwrap_err();
    match err {
        EditError::Pending { track_ids } => assert_eq!(track_ids, vec![a]),
        other => panic!("{other:?}"),
    }
    // 拒否されたバッチは何も残さない（b の版も動かない）
    let n: i64 = lib
        .conn()
        .query_row("SELECT count(*) FROM edit_batches", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(lib.phys(b).tag_version, 1);

    // DB 側でも partial UNIQUE で保証される
    let conn = lib.conn();
    let r = conn.execute(
        "INSERT INTO edit_ops (batch_id, ordinal, track_id, kind) VALUES (1, 99, ?1, 'tags')",
        [a],
    );
    assert!(r.is_err(), "pending の重複が DB で通ってしまった");
}

// ---------------------------------------------------------------- 反映

#[tokio::test]
async fn three_fields_are_written_in_one_rename_and_op_is_applied() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/01.flac", 1, "旧い題"));
    lib.scan().await;
    let id = lib.track_id("A/01.flac");
    let before = lib.phys(id);
    let inode_before = inode_of(&p);

    let mut rx = lib.jobs.subscribe();
    let prepared = lib
        .editor
        .prepare_tags(None, vec![three_field_edit(id)])
        .await
        .unwrap();
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );

    // ファイルに 3 フィールドとも反映され、他のタグは残る
    assert_eq!(file_tags(&p, "TITLE"), ["新しい題"]);
    assert_eq!(file_tags(&p, "ARTIST"), ["A", "B"]);
    assert!(file_tags(&p, "ALBUMARTIST").is_empty());
    assert_eq!(file_tags(&p, "ALBUM"), ["Album"]);
    // tmp + rename なので inode は変わる（1 回だけ）
    let inode_after = inode_of(&p);
    assert_ne!(inode_after, inode_before);

    let ops = lib.ops(prepared.batch_id);
    assert_eq!(ops[0].result, OpResult::Applied);
    assert!(ops[0].applied_at.is_some());
    assert_eq!(ops[0].job_id, Some(prepared.job_ids[0]));
    assert_eq!(lib.job_state(prepared.job_ids[0]), JobState::Done);

    // DB は新 inode / 属性 / tag_hash に追随し、tag_version は prepare 時の 1 回のまま
    let after = lib.phys(id);
    assert_eq!(after.inode as u64, inode_after);
    assert_eq!(after.tag_version, before.tag_version + 1);
    assert_eq!(after.tag_hash, file_tag_hash(&p));
    assert_eq!(after.size, std::fs::metadata(&p).unwrap().len() as i64);

    // 次のスキャンは「変更なし」と見る（新規登録も更新も起きない）
    let report = lib.scan().await;
    assert_eq!(report.new, 0);
    assert_eq!(report.updated, 0);
    assert_eq!(report.moved, 0);
    assert_eq!(lib.phys(id).tag_version, before.tag_version + 1);

    // 取り残しの tmp が無い
    let leftovers: Vec<_> = std::fs::read_dir(lib.path("A"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".spindle-tmp-")
        })
        .collect();
    assert!(leftovers.is_empty());

    // batch イベントが流れる（applying → applied）
    let mut states = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::Batch(b) = ev {
            assert_eq!(b.id, prepared.batch_id);
            states.push(b.state);
        }
    }
    assert_eq!(states, ["applying", "applied"]);
}

#[tokio::test]
async fn applying_the_same_op_repeatedly_changes_nothing() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/01.opus", 1, "旧い題"));
    lib.scan().await;
    let id = lib.track_id("A/01.opus");

    let prepared = lib
        .editor
        .prepare_tags(None, vec![three_field_edit(id)])
        .await
        .unwrap();
    let op_id = lib.ops(prepared.batch_id)[0].id;
    let first = lib.editor.apply_op(op_id, None).await.unwrap();
    assert_eq!(first, OpOutcome::Applied);
    let snapshot = lib.phys(id);
    let inode = inode_of(&p);
    let hash = file_tag_hash(&p);

    for _ in 0..3 {
        let again = lib.editor.apply_op(op_id, None).await.unwrap();
        assert_eq!(again, OpOutcome::AlreadyTerminal(OpResult::Applied));
        assert_eq!(lib.phys(id), snapshot);
        assert_eq!(inode_of(&p), inode);
        assert_eq!(file_tag_hash(&p), hash);
    }
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Applied);
}

#[tokio::test]
async fn op_whose_file_already_has_new_values_is_confirmed_applied() {
    // クラッシュ前に rename まで済んでいた op の模擬: ファイルは新値、DB の事前条件は旧 inode
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/01.flac", 1, "旧い題"));
    lib.scan().await;
    let id = lib.track_id("A/01.flac");

    let prepared = lib
        .editor
        .prepare_tags(None, vec![three_field_edit(id)])
        .await
        .unwrap();
    // 外部で（= 中断前の自分自身が）新値どおりに書き換えた。lofty は in-place だが、
    // 別ファイルへコピー → rename して inode も変える
    let tmp = lib.path("A/.copy.flac");
    std::fs::copy(&p, &tmp).unwrap();
    common::retag(&tmp, |t| {
        t.set_title("新しい題".to_owned());
        t.remove_artist();
        t.push(lofty::tag::TagItem::new(
            lofty::tag::ItemKey::TrackArtist,
            lofty::tag::ItemValue::Text("A".to_owned()),
        ));
        t.push(lofty::tag::TagItem::new(
            lofty::tag::ItemKey::TrackArtist,
            lofty::tag::ItemValue::Text("B".to_owned()),
        ));
        t.remove_key(lofty::tag::ItemKey::AlbumArtist);
    });
    std::fs::rename(&tmp, &p).unwrap();
    let inode = inode_of(&p);

    let op_id = lib.ops(prepared.batch_id)[0].id;
    assert_eq!(
        lib.editor.apply_op(op_id, None).await.unwrap(),
        OpOutcome::Applied
    );
    // ファイルは書き直していない
    assert_eq!(inode_of(&p), inode);
    let after = lib.phys(id);
    assert_eq!(after.inode as u64, inode);
    assert_eq!(after.tag_hash, file_tag_hash(&p));
    assert_eq!(after.tag_version, 2);
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Applied);
}

// ---------------------------------------------------------------- 事前条件と overlay の解消

#[tokio::test]
async fn precondition_mismatch_skips_op_without_touching_file_and_resolves_overlay() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/01.flac", 1, "旧い題"));
    lib.scan().await;
    let id = lib.track_id("A/01.flac");

    let prepared = lib
        .editor
        .prepare_tags(None, vec![three_field_edit(id)])
        .await
        .unwrap();
    assert_eq!(lib.phys(id).title.as_deref(), Some("新しい題"));

    // 外部ツールが別の値を書いた
    common::retag(&p, |t| t.set_title("外部の題".to_owned()));
    let meta = std::fs::metadata(&p).unwrap();
    let inode = inode_of(&p);

    let op_id = lib.ops(prepared.batch_id)[0].id;
    let outcome = lib.editor.apply_op(op_id, None).await.unwrap();
    assert!(matches!(outcome, OpOutcome::Conflict(_)), "{outcome:?}");

    // ファイルは触らない
    assert_eq!(inode_of(&p), inode);
    assert_eq!(
        std::fs::metadata(&p).unwrap().modified().unwrap(),
        meta.modified().unwrap()
    );
    assert_eq!(file_tags(&p, "TITLE"), ["外部の題"]);

    // op は skipped_conflict、DB はファイルの現在値へ戻る（overlay の解消）。版は据え置き
    let o = &lib.ops(prepared.batch_id)[0];
    assert_eq!(o.result, OpResult::SkippedConflict);
    assert!(o.error.is_some());
    let after = lib.phys(id);
    assert_eq!(after.title.as_deref(), Some("外部の題"));
    assert_eq!(after.artist_display.as_deref(), Some("Artist"));
    assert_eq!(after.albumartist.as_deref(), Some("AlbumArtist"));
    assert_eq!(lib.tag_values(id, "ARTIST"), ["Artist"]);
    assert_eq!(after.tag_hash, file_tag_hash(&p));
    assert_eq!(after.inode as u64, inode);
    assert_eq!(after.tag_version, 2);
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Failed);

    // 解消後はスキャンも同じ値を見る
    let report = lib.scan().await;
    assert_eq!(report.updated, 0);
    assert_eq!(lib.phys(id).title.as_deref(), Some("外部の題"));
}

/// バッチ準備の後にホストを再起動すると dev 番号が振り直される。`expected_dev` の不一致だけでは
/// conflict にしない（inode / size / mtime / ctime / tag_hash で同じ実体と分かる。D-62）
#[tokio::test]
async fn expected_dev_mismatch_alone_is_not_a_conflict() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/01.flac", 1, "旧い題"));
    lib.scan().await;
    let id = lib.track_id("A/01.flac");

    let prepared = lib
        .editor
        .prepare_tags(None, vec![three_field_edit(id)])
        .await
        .unwrap();
    lib.conn()
        .execute(
            "UPDATE edit_ops SET expected_dev = expected_dev + 1 WHERE batch_id = ?1",
            [prepared.batch_id],
        )
        .unwrap();

    let op_id = lib.ops(prepared.batch_id)[0].id;
    let outcome = lib.editor.apply_op(op_id, None).await.unwrap();
    assert!(matches!(outcome, OpOutcome::Applied), "{outcome:?}");
    assert_eq!(file_tags(&p, "TITLE"), ["新しい題"]);
    assert_eq!(lib.ops(prepared.batch_id)[0].result, OpResult::Applied);
}

#[tokio::test]
async fn in_place_update_preserving_mtime_is_detected_by_ctime() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/01.flac", 1, "abcdef"));
    lib.scan().await;
    let id = lib.track_id("A/01.flac");
    let before = std::fs::metadata(&p).unwrap();

    let prepared = lib
        .editor
        .prepare_tags(None, vec![op(id, vec![set("TITLE", &["ghijkl"])])])
        .await
        .unwrap();

    // 同じ長さの値を in-place で書き、mtime を戻す（`touch -r` 相当）。inode / size / mtime は同じ
    let mtime = before.modified().unwrap();
    common::retag(&p, |t| t.set_title("ABCDEF".to_owned()));
    File::options()
        .write(true)
        .open(&p)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    let after = std::fs::metadata(&p).unwrap();
    assert_eq!(inode_of(&p), before.ino_compat());
    assert_eq!(
        after.len(),
        before.len(),
        "size が変わるとテストが ctime を試していない"
    );
    assert_eq!(after.modified().unwrap(), mtime);

    let op_id = lib.ops(prepared.batch_id)[0].id;
    let outcome = lib.editor.apply_op(op_id, None).await.unwrap();
    match outcome {
        OpOutcome::Conflict(reason) => assert!(reason.contains("ctime"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(file_tags(&p, "TITLE"), ["ABCDEF"]);
    assert_eq!(lib.phys(id).title.as_deref(), Some("ABCDEF"));
}

trait InoCompat {
    fn ino_compat(&self) -> u64;
}

impl InoCompat for std::fs::Metadata {
    fn ino_compat(&self) -> u64 {
        use std::os::unix::fs::MetadataExt;
        self.ino()
    }
}

#[tokio::test]
async fn missing_file_is_conflict_and_db_reverts_to_recorded_old_values() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/01.flac", 1, "旧い題"));
    lib.scan().await;
    let id = lib.track_id("A/01.flac");
    let before = lib.phys(id);

    let prepared = lib
        .editor
        .prepare_tags(None, vec![three_field_edit(id)])
        .await
        .unwrap();
    std::fs::remove_file(&p).unwrap();

    let op_id = lib.ops(prepared.batch_id)[0].id;
    let outcome = lib.editor.apply_op(op_id, None).await.unwrap();
    assert!(matches!(outcome, OpOutcome::Conflict(_)), "{outcome:?}");
    let after = lib.phys(id);
    assert_eq!(after.title, before.title);
    assert_eq!(after.artist_display, before.artist_display);
    assert_eq!(after.albumartist, before.albumartist);
    assert_eq!(after.tag_hash, before.tag_hash);
    assert_eq!(lib.tag_values(id, "ALBUMARTIST"), ["AlbumArtist"]);
    assert_eq!(after.tag_version, 2);
}

#[tokio::test]
async fn tags_op_follows_external_rename_tracked_by_scan() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/01.flac", 1, "旧い題"));
    lib.scan().await;
    let id = lib.track_id("A/01.flac");

    let prepared = lib
        .editor
        .prepare_tags(None, vec![three_field_edit(id)])
        .await
        .unwrap();
    // 外部で rename → スキャンが (dev, inode) で追随（pending の tags は据え置き）
    let q = lib.path("A/01 renamed.flac");
    std::fs::rename(&p, &q).unwrap();
    let report = lib.scan().await;
    assert_eq!(report.moved, 1);
    assert_eq!(lib.phys(id).rel_path, "A/01 renamed.flac");
    assert_eq!(lib.phys(id).title.as_deref(), Some("新しい題"));

    let op_id = lib.ops(prepared.batch_id)[0].id;
    assert_eq!(
        lib.editor.apply_op(op_id, None).await.unwrap(),
        OpOutcome::Applied
    );
    assert_eq!(file_tags(&q, "TITLE"), ["新しい題"]);
    assert_eq!(lib.phys(id).rel_path, "A/01 renamed.flac");
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Applied);
}

#[tokio::test]
async fn batch_with_mixed_results_is_partial_and_worker_applies_all() {
    let lib = Lib::new();
    let a = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    let b = require_ffmpeg!(lib.add("A/02.flac", 2, "b"));
    let c = require_ffmpeg!(lib.add("B/01.opus", 3, "c"));
    lib.scan().await;
    let ids: Vec<i64> = ["A/01.flac", "A/02.flac", "B/01.opus"]
        .iter()
        .map(|r| lib.track_id(r))
        .collect();

    let prepared = lib
        .editor
        .prepare_tags(
            Some("mixed"),
            ids.iter()
                .map(|id| op(*id, vec![set("TITLE", &["共通"])]))
                .collect(),
        )
        .await
        .unwrap();
    assert_eq!(prepared.affected, 3);
    // 2 件目だけ外部で書き換える
    common::retag(&b, |t| t.set_title("外部".to_owned()));

    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Partial
    );
    let ops = lib.ops(prepared.batch_id);
    let by_track = |id: i64| ops.iter().find(|o| o.track_id == id).unwrap();
    assert_eq!(by_track(ids[0]).result, OpResult::Applied);
    assert_eq!(by_track(ids[1]).result, OpResult::SkippedConflict);
    assert_eq!(by_track(ids[2]).result, OpResult::Applied);
    assert_eq!(file_tags(&a, "TITLE"), ["共通"]);
    assert_eq!(file_tags(&b, "TITLE"), ["外部"]);
    assert_eq!(file_tags(&c, "TITLE"), ["共通"]);
    assert_eq!(lib.phys(ids[1]).title.as_deref(), Some("外部"));
    let counts = history::batch_counts(&lib.conn(), prepared.batch_id).unwrap();
    assert_eq!(
        (
            counts.applied,
            counts.conflict,
            counts.failed,
            counts.pending
        ),
        (2, 1, 0, 0)
    );
    for j in &prepared.job_ids {
        assert_eq!(lib.wait_job_terminal(*j).await, JobState::Done);
    }
}

// ---------------------------------------------------------------- 反映の窓での外部更新

#[tokio::test]
async fn external_update_between_check_and_rename_is_conflict_and_tmp_is_discarded() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/01.flac", 1, "旧い題"));
    lib.scan().await;
    let id = lib.track_id("A/01.flac");
    let prepared = lib
        .editor
        .prepare_tags(None, vec![three_field_edit(id)])
        .await
        .unwrap();

    // 事前条件の確認後・rename 直前に外部ツールが in-place で書き換える
    let hooked = p.clone();
    lib.editor
        .set_before_rename_hook(Arc::new(move |_rel: &str| {
            common::retag(&hooked, |t| t.set_title("窓の中の外部更新".to_owned()));
        }));
    let inode = inode_of(&p);
    let op_id = lib.ops(prepared.batch_id)[0].id;
    let outcome = lib.editor.apply_op(op_id, None).await.unwrap();
    assert!(matches!(outcome, OpOutcome::Conflict(_)), "{outcome:?}");
    // 外部の変更が勝ち、spindle の tmp は残らない
    assert_eq!(file_tags(&p, "TITLE"), ["窓の中の外部更新"]);
    assert_eq!(inode_of(&p), inode);
    assert!(std::fs::read_dir(lib.path("A")).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".spindle-tmp-")));
    assert_eq!(lib.phys(id).title.as_deref(), Some("窓の中の外部更新"));
    assert_eq!(lib.phys(id).tag_hash, file_tag_hash(&p));
    assert_eq!(
        lib.ops(prepared.batch_id)[0].result,
        OpResult::SkippedConflict
    );
}

// ---------------------------------------------------------------- 外部差し替えの音声属性

fn audio_state(lib: &Lib, id: i64) -> (i64, Option<Vec<u8>>, Option<String>) {
    lib.conn()
        .query_row(
            "SELECT audio_version, audio_md5, codec FROM tracks WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

fn flac_md5(path: &Path) -> Vec<u8> {
    spindle::media::fingerprint::flac_streaminfo_md5(File::open(path).unwrap())
        .unwrap()
        .unwrap()
        .to_vec()
}

#[tokio::test]
async fn replaced_audio_with_matching_tags_is_applied_and_audio_version_advances() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/01.flac", 1, "旧い題"));
    lib.scan().await;
    let id = lib.track_id("A/01.flac");
    let (v0, md5_0, _) = audio_state(&lib, id);
    let prepared = lib
        .editor
        .prepare_tags(None, vec![op(id, vec![set("TITLE", &["新しい題"])])])
        .await
        .unwrap();
    // 別音源（新値のタイトル付き）へ差し替え
    let other = require_ffmpeg!(common::make_audio(lib.dir.path(), "other.flac", "flac", 7));
    common::set_basic_tags(&other, "新しい題", "Artist", "Album", "AlbumArtist", 1, 1);
    std::fs::rename(&other, &p).unwrap();

    let op_id = lib.ops(prepared.batch_id)[0].id;
    assert_eq!(
        lib.editor.apply_op(op_id, None).await.unwrap(),
        OpOutcome::Applied
    );
    let (v1, md5_1, _) = audio_state(&lib, id);
    assert_eq!(v1, v0 + 1);
    assert_ne!(md5_1, md5_0);
    assert_eq!(md5_1.unwrap(), flac_md5(&p));
    // 次回スキャンは差分を見ない（DB は既にファイルと一致）
    let report = lib.scan().await;
    assert_eq!(report.updated, 0);
    assert_eq!(audio_state(&lib, id).0, v1);
}

#[tokio::test]
async fn replaced_audio_with_other_tags_is_conflict_and_audio_version_advances() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/01.flac", 1, "旧い題"));
    lib.scan().await;
    let id = lib.track_id("A/01.flac");
    let (v0, _, _) = audio_state(&lib, id);
    let prepared = lib
        .editor
        .prepare_tags(None, vec![op(id, vec![set("TITLE", &["新しい題"])])])
        .await
        .unwrap();
    let other = require_ffmpeg!(common::make_audio(lib.dir.path(), "other.flac", "flac", 7));
    common::set_basic_tags(&other, "別の題", "Artist", "Album", "AlbumArtist", 1, 1);
    std::fs::rename(&other, &p).unwrap();

    let op_id = lib.ops(prepared.batch_id)[0].id;
    assert!(matches!(
        lib.editor.apply_op(op_id, None).await.unwrap(),
        OpOutcome::Conflict(_)
    ));
    let (v1, md5_1, _) = audio_state(&lib, id);
    assert_eq!(v1, v0 + 1);
    assert_eq!(md5_1.unwrap(), flac_md5(&p));
    assert_eq!(lib.phys(id).title.as_deref(), Some("別の題"));
    assert_eq!(lib.scan().await.updated, 0);
}

#[tokio::test]
async fn own_tagwrite_does_not_advance_audio_version_or_recompute_fingerprint() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "旧い題"));
    lib.scan().await;
    let id = lib.track_id("A/01.flac");
    let before = audio_state(&lib, id);
    let prepared = lib
        .editor
        .prepare_tags(None, vec![three_field_edit(id)])
        .await
        .unwrap();
    let op_id = lib.ops(prepared.batch_id)[0].id;
    assert_eq!(
        lib.editor.apply_op(op_id, None).await.unwrap(),
        OpOutcome::Applied
    );
    assert_eq!(audio_state(&lib, id), before);
}

// ---------------------------------------------------------------- mode / xattr の保持

#[tokio::test]
async fn tagwrite_preserves_file_mode_and_user_xattrs() {
    use std::os::unix::fs::PermissionsExt;
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/01.flac", 1, "旧い題"));
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o640)).unwrap();
    let xattr_supported = {
        let f = File::open(&p).unwrap();
        rustix::fs::fsetxattr(
            &f,
            "user.spindle_test",
            b"1",
            rustix::fs::XattrFlags::empty(),
        )
        .is_ok()
    };
    lib.scan().await;
    let id = lib.track_id("A/01.flac");
    let prepared = lib
        .editor
        .prepare_tags(None, vec![three_field_edit(id)])
        .await
        .unwrap();
    let op_id = lib.ops(prepared.batch_id)[0].id;
    assert_eq!(
        lib.editor.apply_op(op_id, None).await.unwrap(),
        OpOutcome::Applied
    );
    assert_eq!(file_tags(&p, "TITLE"), ["新しい題"]);
    let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o640);
    if xattr_supported {
        let f = File::open(&p).unwrap();
        let mut buf = [0u8; 8];
        let n = rustix::fs::fgetxattr(&f, "user.spindle_test", &mut buf[..]).unwrap();
        assert_eq!(&buf[..n], b"1");
    } else {
        eprintln!("xattr 非対応の FS なので xattr の確認は skip");
    }
}

// ---------------------------------------------------------------- キャンセル

#[tokio::test]
async fn cancel_marks_unstarted_ops_failed_and_reverts_overlay_from_file() {
    let lib = Lib::new();
    let a = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    let b = require_ffmpeg!(lib.add("A/02.flac", 2, "b"));
    lib.scan().await;
    let ia = lib.track_id("A/01.flac");
    let ib = lib.track_id("A/02.flac");

    let prepared = lib
        .editor
        .prepare_tags(
            None,
            vec![
                op(ia, vec![set("TITLE", &["x"])]),
                op(ib, vec![set("TITLE", &["y"]), set("ARTIST", &["Z"])]),
            ],
        )
        .await
        .unwrap();
    assert_eq!(lib.phys(ia).title.as_deref(), Some("x"));
    // b はキャンセル前に外部で変わっている → 戻し先はファイルの現在値
    common::retag(&b, |t| t.set_title("外部b".to_owned()));

    // ワーカーは動いていない（未着手）
    let outcome = lib.editor.cancel_batch(prepared.batch_id).await.unwrap();
    match outcome {
        CancelOutcome::Cancelled { ops_cancelled, .. } => assert_eq!(ops_cancelled, 2),
        other => panic!("{other:?}"),
    }
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Cancelled);
    for o in lib.ops(prepared.batch_id) {
        assert_eq!(o.result, OpResult::Failed);
        assert_eq!(o.error.as_deref(), Some("cancelled"));
    }
    for j in &prepared.job_ids {
        assert_eq!(lib.job_state(*j), JobState::Cancelled);
    }
    // その直後にファイルの値で読める
    assert_eq!(lib.phys(ia).title.as_deref(), Some("a"));
    assert_eq!(lib.phys(ib).title.as_deref(), Some("外部b"));
    assert_eq!(lib.tag_values(ib, "ARTIST"), ["Artist"]);
    assert_eq!(lib.phys(ib).tag_hash, file_tag_hash(&b));
    assert_eq!(lib.phys(ia).tag_hash, file_tag_hash(&a));
    // 版は据え置き、pending は無いので再編集できる
    assert_eq!(lib.phys(ia).tag_version, 2);
    lib.editor
        .prepare_tags(None, vec![op(ia, vec![set("TITLE", &["x"])])])
        .await
        .unwrap();

    // 終端バッチの cancel は拒否
    assert_eq!(
        lib.editor.cancel_batch(prepared.batch_id).await.unwrap(),
        CancelOutcome::NotCancellable
    );
    assert_eq!(
        lib.editor.cancel_batch(9999).await.unwrap(),
        CancelOutcome::NotFound
    );
}

#[tokio::test]
async fn cancel_keeps_applied_ops_and_ends_as_cancelled() {
    let lib = Lib::new();
    let a = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    let b = require_ffmpeg!(lib.add("A/02.flac", 2, "b"));
    lib.scan().await;
    let ia = lib.track_id("A/01.flac");
    let ib = lib.track_id("A/02.flac");

    let prepared = lib
        .editor
        .prepare_tags(
            None,
            vec![
                op(ia, vec![set("TITLE", &["x"])]),
                op(ib, vec![set("TITLE", &["y"])]),
            ],
        )
        .await
        .unwrap();
    let ops = lib.ops(prepared.batch_id);
    let op_a = ops.iter().find(|o| o.track_id == ia).unwrap().id;
    // 1 件だけ反映済みにしてからキャンセル
    assert_eq!(
        lib.editor.apply_op(op_a, None).await.unwrap(),
        OpOutcome::Applied
    );
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Applying);
    lib.editor.cancel_batch(prepared.batch_id).await.unwrap();
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Cancelled);
    let ops = lib.ops(prepared.batch_id);
    assert_eq!(
        ops.iter().find(|o| o.track_id == ia).unwrap().result,
        OpResult::Applied
    );
    assert_eq!(
        ops.iter().find(|o| o.track_id == ib).unwrap().result,
        OpResult::Failed
    );
    assert_eq!(file_tags(&a, "TITLE"), ["x"]);
    assert_eq!(file_tags(&b, "TITLE"), ["b"]);
    assert_eq!(lib.phys(ib).title.as_deref(), Some("b"));
}

// ---------------------------------------------------------------- 起動時リカバリ

#[tokio::test]
async fn recovery_requeues_track_jobs_for_pending_ops_of_open_batches() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.scan().await;
    let id = lib.track_id("A/01.flac");
    let prepared = lib
        .editor
        .prepare_tags(None, vec![op(id, vec![set("TITLE", &["x"])])])
        .await
        .unwrap();
    // ジョブ行が失われた / 上限失敗で failed になった、の模擬
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
            "SELECT count(*) FROM jobs WHERE type = 'tagwrite' AND state = 'queued'
               AND edit_batch_id = ?1",
            [prepared.batch_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(queued, 1);
    // 二度目は dedup で増えない
    let report = lib.editor.recover().await.unwrap();
    assert_eq!(report.requeued, 0);

    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(file_tags(&p, "TITLE"), ["x"]);
}

#[tokio::test]
async fn recovery_finishes_ops_whose_jobs_were_cancelled_before_restart() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.scan().await;
    let id = lib.track_id("A/01.flac");
    let prepared = lib
        .editor
        .prepare_tags(None, vec![op(id, vec![set("TITLE", &["x"])])])
        .await
        .unwrap();
    // 実行中に cancel 要求が立ったまま落ちた
    lib.conn()
        .execute(
            "UPDATE jobs SET state = 'running', cancel_requested_at = 1 WHERE id = ?1",
            [prepared.job_ids[0]],
        )
        .unwrap();

    let lib = lib.reopen();
    spindle::jobs::recovery::run(&lib.db).await.unwrap();
    assert_eq!(lib.job_state(prepared.job_ids[0]), JobState::Cancelled);
    let report = lib.editor.recover().await.unwrap();
    assert_eq!(report.cancelled_ops, 1);
    assert_eq!(report.requeued, 0);
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Cancelled);
    assert_eq!(lib.phys(id).title.as_deref(), Some("a"));
    assert_eq!(file_tags(&p, "TITLE"), ["a"]);
}

#[tokio::test]
async fn cancel_requested_before_start_makes_handler_finish_op_as_cancelled() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.scan().await;
    let id = lib.track_id("A/01.flac");
    let prepared = lib
        .editor
        .prepare_tags(None, vec![op(id, vec![set("TITLE", &["x"])])])
        .await
        .unwrap();
    let job = prepared.job_ids[0];
    // claim されたが手を付ける前に cancel が来た、の模擬（running + cancel_requested_at）
    lib.conn()
        .execute(
            "UPDATE jobs SET state = 'queued', cancel_requested_at = NULL WHERE id = ?1",
            [job],
        )
        .unwrap();
    lib.jobs.cancel(job).await.unwrap();
    // queued なら即 cancelled。op は編集機構側が閉じる
    lib.editor.recover().await.unwrap();
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Cancelled);
    assert_eq!(lib.phys(id).title.as_deref(), Some("a"));
    assert_eq!(file_tags(&p, "TITLE"), ["a"]);
}

// ---------------------------------------------------------------- ジョブ側の防御

#[tokio::test]
async fn handler_fails_op_on_last_attempt_and_resolves_overlay() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.scan().await;
    let id = lib.track_id("A/01.flac");
    let prepared = lib
        .editor
        .prepare_tags(None, vec![op(id, vec![set("TITLE", &["x"])])])
        .await
        .unwrap();
    let op_id = lib.ops(prepared.batch_id)[0].id;
    // 反映を確実に失敗させる: 親ディレクトリを書けなくする
    let dir = lib.path("A");
    let mut perm = std::fs::metadata(&dir).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perm.set_mode(0o555);
    std::fs::set_permissions(&dir, perm.clone()).unwrap();
    if unsafe_is_root() {
        perm.set_mode(0o755);
        std::fs::set_permissions(&dir, perm).unwrap();
        eprintln!("root では書き込み拒否を作れないので skip");
        return;
    }
    // 最終試行にする
    lib.conn()
        .execute(
            "UPDATE jobs SET attempts = max_attempts - 1 WHERE id = ?1",
            [prepared.job_ids[0]],
        )
        .unwrap();
    lib.start();
    assert_eq!(
        lib.wait_job_terminal(prepared.job_ids[0]).await,
        JobState::Failed
    );
    perm.set_mode(0o755);
    std::fs::set_permissions(&dir, perm).unwrap();
    let o = lib
        .ops(prepared.batch_id)
        .into_iter()
        .find(|o| o.id == op_id)
        .unwrap();
    assert_eq!(o.result, OpResult::Failed);
    assert!(o.error.is_some());
    assert_eq!(lib.batch_state(prepared.batch_id), BatchState::Failed);
    // overlay は解消済み
    assert_eq!(lib.phys(id).title.as_deref(), Some("a"));
    assert_eq!(file_tags(&p, "TITLE"), ["a"]);
}

fn unsafe_is_root() -> bool {
    rustix::process::geteuid().is_root()
}

#[tokio::test]
async fn history_helpers_expose_batch_and_ops() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a"));
    lib.scan().await;
    let id = lib.track_id("A/01.flac");
    let prepared = lib
        .editor
        .prepare_tags(Some("d"), vec![op(id, vec![set("TITLE", &["x"])])])
        .await
        .unwrap();
    let conn = lib.conn();
    let counts = history::batch_counts(&conn, prepared.batch_id).unwrap();
    assert_eq!(counts.pending, 1);
    assert_eq!(counts.total(), 1);
    let none: Option<history::Batch> = history::get_batch(&conn, 42).unwrap();
    assert!(none.is_none());
    let ops = history::list_ops(&conn, prepared.batch_id).unwrap();
    assert_eq!(ops[0].ordinal, 0);
    let (pending_track,): (i64,) = conn
        .query_row(
            "SELECT track_id FROM edit_ops WHERE result = 'pending'",
            [],
            |r| Ok((r.get(0)?,)),
        )
        .optional()
        .unwrap()
        .unwrap();
    assert_eq!(pending_track, id);
}

/// P4-11: ALAC（m4a）に写像表に無いキーを `set` → `delete` しても applied になる（フリーフォーム
/// atom で書き、同じキーで読み戻せる）
#[tokio::test]
async fn arbitrary_key_set_and_delete_on_alac_are_applied() {
    let lib = Lib::new();
    let dir = lib.path("A");
    std::fs::create_dir_all(&dir).unwrap();
    let p = require_ffmpeg!(common::make_audio(&dir, "01.m4a", "alac.m4a", 1));
    common::set_basic_tags(&p, "題", "Artist", "Album", "AlbumArtist", 1, 1);
    lib.scan().await;
    let id = lib.track_id("A/01.m4a");

    let prepared = lib
        .editor
        .prepare_tags(None, vec![op(id, vec![set("SPINDLETEST", &["a", "b"])])])
        .await
        .unwrap();
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(lib.ops(prepared.batch_id)[0].result, OpResult::Applied);
    assert_eq!(file_tags(&p, "SPINDLETEST"), ["a", "b"]);
    assert_eq!(lib.tag_values(id, "SPINDLETEST"), ["a", "b"]);
    assert_eq!(file_tags(&p, "TITLE"), ["題"]);

    let prepared = lib
        .editor
        .prepare_tags(None, vec![op(id, vec![del("SPINDLETEST")])])
        .await
        .unwrap();
    assert_eq!(
        lib.wait_batch_terminal(prepared.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(lib.ops(prepared.batch_id)[0].result, OpResult::Applied);
    assert!(file_tags(&p, "SPINDLETEST").is_empty());
    assert!(lib.tag_values(id, "SPINDLETEST").is_empty());
}

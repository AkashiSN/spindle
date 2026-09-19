//! ReplayGain のタグ書き込み（SPEC §6「ReplayGain の内部表現」、docs/TASKS.md P1-2）。
//! 解析値（DB の `rg_*`）を形式ごとのタグへ変換し、通常の編集バッチ（tags op）として書く。
//! `rg_written_at` は「ファイルの RG タグが解析値と一致している」ことを確認した時刻で、
//! 書き込みの applied だけでなく、既に一致していた行・外れた行の追随でも動く。
//! 合成ファイルは ffmpeg で作る（無ければ skip）

#![cfg(target_os = "linux")]

mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{params, Connection};
use tokio_util::sync::CancellationToken;

use spindle::db::history::{self, BatchState, OpResult};
use spindle::db::{replaygain as dbrg, Db};
use spindle::domain::replaygain::{opus_r128, Values};
use spindle::domain::tags::read_audio_file;
use spindle::edit::{EditError, Editor, NewTagOp, TagChange};
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, Scanner};
use spindle::jobs::handlers::tagwrite::TagwriteHandler;
use spindle::jobs::{JobType, Jobs, Registry};

const REFERENCE: f64 = -18.0;

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
    scanned_at: Option<i64>,
    written_at: Option<i64>,
    tag_version: i64,
    audio_version: i64,
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
        let editor =
            Arc::new(Editor::new(db, root, jobs.clone()).with_replaygain_reference(REFERENCE));
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

    fn add(&self, rel: &str, seed: u32, title: &str) -> Option<PathBuf> {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let ext = rel.rsplit_once('.').unwrap().1;
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        let made = common::make_audio(p.parent().unwrap(), &name, ext, seed)?;
        common::set_basic_tags(&made, title, "Artist", "Album", "AlbumArtist", 1, 1);
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

    /// 解析済みの状態を作る（rg ジョブの結果の模擬）
    fn set_rg(&self, id: i64, v: Values, scanned_at: i64) {
        self.conn()
            .execute(
                "UPDATE tracks SET rg_track_gain = ?2, rg_track_peak = ?3, rg_album_gain = ?4,
                        rg_album_peak = ?5, rg_scanned_at = ?6, rg_written_at = NULL
                  WHERE id = ?1",
                params![
                    id,
                    v.track_gain,
                    v.track_peak,
                    v.album_gain,
                    v.album_peak,
                    scanned_at
                ],
            )
            .unwrap();
    }

    fn row(&self, id: i64) -> Row {
        self.conn()
            .query_row(
                "SELECT rg_scanned_at, rg_written_at, tag_version, audio_version FROM tracks WHERE id = ?1",
                [id],
                |r| {
                    Ok(Row {
                        scanned_at: r.get(0)?,
                        written_at: r.get(1)?,
                        tag_version: r.get(2)?,
                        audio_version: r.get(3)?,
                    })
                },
            )
            .unwrap()
    }

    fn db_tag(&self, track_id: i64, key: &str) -> Vec<String> {
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
}

impl Drop for Lib {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn file_tags(path: &Path, key: &str) -> Vec<String> {
    let ext = path.extension().and_then(|e| e.to_str());
    let af = read_audio_file(File::open(path).unwrap(), ext).unwrap();
    af.tags.values(key).map(str::to_owned).collect()
}

fn values(track_gain: f64, album_gain: Option<f64>) -> Values {
    Values {
        track_gain,
        track_peak: 0.5,
        album_gain,
        album_peak: album_gain.map(|_| 0.75),
    }
}

fn set(key: &str, values: &[&str]) -> TagChange {
    TagChange {
        key: key.to_owned(),
        values: Some(values.iter().map(|s| (*s).to_owned()).collect()),
    }
}

// ---------------------------------------------------------------- 書き込み

#[tokio::test]
async fn writes_format_specific_tags_and_marks_written() {
    let lib = Lib::new();
    let flac = require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    let opus = lib.add("A/2.opus", 2, "two").unwrap();
    let m4a = lib.add("A/3.m4a", 3, "three").unwrap();
    // Opus に古い REPLAYGAIN_* が残っている（外部ツール）→ 消える
    common::retag(&opus, |t| {
        t.insert_text(
            lofty::tag::ItemKey::ReplayGainTrackGain,
            "-3.00 dB".to_owned(),
        );
    });
    lib.scan().await;
    let (f, o, m) = (
        lib.track_id("A/1.flac"),
        lib.track_id("A/2.opus"),
        lib.track_id("A/3.m4a"),
    );
    lib.set_rg(f, values(-2.5, Some(-1.0)), 1000);
    lib.set_rg(o, values(0.0, Some(5.0)), 1000);
    lib.set_rg(m, values(1.25, None), 1000);
    let before = [lib.row(f), lib.row(o), lib.row(m)];

    let rep = lib
        .editor
        .prepare_rg_write(Some("RG"), vec![f, o, m])
        .await
        .unwrap();
    let batch_id = rep.batch_id.unwrap();
    assert_eq!((rep.affected, rep.unchanged, rep.unscanned), (3, 0, 0));
    assert_eq!(rep.job_ids.len(), 3);
    // overlay: DB は新値、rg_written_at はまだ立たない
    assert_eq!(lib.db_tag(f, "REPLAYGAIN_TRACK_GAIN"), ["-2.50 dB"]);
    assert_eq!(lib.row(f).written_at, None);

    lib.start();
    assert_eq!(lib.wait_batch_terminal(batch_id).await, BatchState::Applied);

    assert_eq!(file_tags(&flac, "REPLAYGAIN_TRACK_GAIN"), ["-2.50 dB"]);
    assert_eq!(file_tags(&flac, "REPLAYGAIN_TRACK_PEAK"), ["0.500000"]);
    assert_eq!(file_tags(&flac, "REPLAYGAIN_ALBUM_GAIN"), ["-1.00 dB"]);
    assert_eq!(file_tags(&flac, "REPLAYGAIN_ALBUM_PEAK"), ["0.750000"]);
    assert_eq!(file_tags(&flac, "TITLE"), ["one"]);
    assert_eq!(
        file_tags(&opus, "R128_TRACK_GAIN"),
        [opus_r128(0.0, REFERENCE).to_string()]
    );
    assert_eq!(file_tags(&opus, "R128_ALBUM_GAIN"), ["0"]);
    assert!(file_tags(&opus, "REPLAYGAIN_TRACK_GAIN").is_empty());
    // MP4 は lofty の写像（----:com.apple.iTunes:replaygain_track_gain）で往復する
    assert_eq!(file_tags(&m4a, "REPLAYGAIN_TRACK_GAIN"), ["+1.25 dB"]);
    assert!(file_tags(&m4a, "REPLAYGAIN_ALBUM_GAIN").is_empty());

    for (id, b) in [f, o, m].into_iter().zip(before) {
        let r = lib.row(id);
        assert!(r.written_at.is_some_and(|w| w >= 1000), "{r:?}");
        assert_eq!(r.tag_version, b.tag_version + 1);
        assert_eq!(r.audio_version, b.audio_version);
    }
    // 履歴: 旧値 null → 新値
    let ops = lib.ops(batch_id);
    assert!(ops.iter().all(|o| o.result == OpResult::Applied));
    let flac_op = ops.iter().find(|o| o.track_id == f).unwrap();
    let edits = history::list_edits(&lib.conn(), flac_op.id).unwrap();
    let gain = edits
        .iter()
        .find(|e| e.key == "REPLAYGAIN_TRACK_GAIN")
        .unwrap();
    assert_eq!(gain.old_value, serde_json::Value::Null);
    assert_eq!(gain.new_value, serde_json::json!(["-2.50 dB"]));
}

#[tokio::test]
async fn matching_rows_are_marked_written_without_a_batch() {
    let lib = Lib::new();
    let flac = require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    lib.scan().await;
    let f = lib.track_id("A/1.flac");
    lib.set_rg(f, values(-2.5, None), 1000);
    lib.start();
    let rep = lib.editor.prepare_rg_write(None, vec![f]).await.unwrap();
    lib.wait_batch_terminal(rep.batch_id.unwrap()).await;
    let first = lib.row(f);
    assert!(first.written_at.is_some());

    // 再解析で同じ値になった → ファイルは既に一致。バッチは作らず rg_written_at だけ進む
    lib.set_rg(f, values(-2.5, None), 2000);
    assert_eq!(lib.row(f).written_at, None);
    let rep = lib.editor.prepare_rg_write(None, vec![f]).await.unwrap();
    assert_eq!(rep.batch_id, None);
    assert_eq!((rep.affected, rep.unchanged, rep.unscanned), (0, 1, 0));
    let r = lib.row(f);
    assert!(r.written_at.is_some_and(|w| w >= 2000), "{r:?}");
    assert_eq!(r.tag_version, first.tag_version);
    assert_eq!(file_tags(&flac, "REPLAYGAIN_TRACK_GAIN"), ["-2.50 dB"]);
}

#[tokio::test]
async fn unscanned_rows_are_skipped() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    lib.add("A/2.flac", 2, "two").unwrap();
    lib.scan().await;
    let (a, b) = (lib.track_id("A/1.flac"), lib.track_id("A/2.flac"));
    lib.set_rg(a, values(1.0, None), 1000);

    let rep = lib.editor.prepare_rg_write(None, vec![a, b]).await.unwrap();
    assert_eq!((rep.affected, rep.unchanged, rep.unscanned), (1, 0, 1));
    assert_eq!(lib.ops(rep.batch_id.unwrap()).len(), 1);
    assert_eq!(lib.row(b).written_at, None);

    // 全部未解析なら何も記録しない
    let rep = lib.editor.prepare_rg_write(None, vec![b]).await.unwrap();
    assert_eq!(rep.batch_id, None);
    assert_eq!(rep.unscanned, 1);

    // missing は解析済みでも書けない
    lib.set_rg(b, values(1.0, None), 1000);
    std::fs::remove_file(lib.lib().join("A/2.flac")).unwrap();
    lib.scan().await;
    let rep = lib.editor.prepare_rg_write(None, vec![b]).await.unwrap();
    assert_eq!(rep.batch_id, None);
    assert_eq!((rep.unscanned, rep.missing), (0, 1));
    assert_eq!(lib.row(b).written_at, None);
}

#[tokio::test]
async fn pending_rows_are_rejected() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    lib.scan().await;
    let a = lib.track_id("A/1.flac");
    lib.set_rg(a, values(1.0, None), 1000);
    lib.editor
        .prepare_tags(
            None,
            vec![NewTagOp {
                track_id: a,
                changes: vec![set("TITLE", &["x"])],
            }],
        )
        .await
        .unwrap();
    let err = lib
        .editor
        .prepare_rg_write(None, vec![a])
        .await
        .unwrap_err();
    assert!(
        matches!(err, EditError::Pending { ref track_ids } if track_ids == &[a]),
        "{err:?}"
    );
    assert!(matches!(
        lib.editor.prepare_rg_write(None, vec![a, a]).await,
        Err(EditError::DuplicateTrack(_))
    ));
}

// ---------------------------------------------------------------- rg_written_at の追随

#[tokio::test]
async fn revert_restores_old_tags_and_clears_written_at() {
    let lib = Lib::new();
    let opus = require_ffmpeg!(lib.add("A/1.opus", 1, "one"));
    lib.scan().await;
    let a = lib.track_id("A/1.opus");
    lib.set_rg(a, values(2.0, Some(2.0)), 1000);
    lib.start();
    let rep = lib.editor.prepare_rg_write(None, vec![a]).await.unwrap();
    let batch_id = rep.batch_id.unwrap();
    lib.wait_batch_terminal(batch_id).await;
    assert!(lib.row(a).written_at.is_some());

    let rev = lib.editor.revert_batch(batch_id, None).await.unwrap();
    assert_eq!(
        lib.wait_batch_terminal(rev.batch_id).await,
        BatchState::Applied
    );
    assert!(file_tags(&opus, "R128_TRACK_GAIN").is_empty());
    assert!(file_tags(&opus, "R128_ALBUM_GAIN").is_empty());
    // ファイルは解析値を持たなくなった
    assert_eq!(lib.row(a).written_at, None);
}

#[tokio::test]
async fn manual_edit_of_rg_key_clears_written_at() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    lib.scan().await;
    let a = lib.track_id("A/1.flac");
    lib.set_rg(a, values(2.0, None), 1000);
    lib.start();
    let rep = lib.editor.prepare_rg_write(None, vec![a]).await.unwrap();
    lib.wait_batch_terminal(rep.batch_id.unwrap()).await;
    assert!(lib.row(a).written_at.is_some());

    // 無関係なキーの編集では消えない
    let p = lib
        .editor
        .prepare_tags(
            None,
            vec![NewTagOp {
                track_id: a,
                changes: vec![set("TITLE", &["改題"])],
            }],
        )
        .await
        .unwrap();
    lib.wait_batch_terminal(p.batch_id).await;
    assert!(lib.row(a).written_at.is_some());

    // RG のキーを解析値と違う値にすると消える
    let p = lib
        .editor
        .prepare_tags(
            None,
            vec![NewTagOp {
                track_id: a,
                changes: vec![set("REPLAYGAIN_TRACK_GAIN", &["+9.99 dB"])],
            }],
        )
        .await
        .unwrap();
    lib.wait_batch_terminal(p.batch_id).await;
    assert_eq!(lib.row(a).written_at, None);
}

#[tokio::test]
async fn conflict_leaves_file_untouched_and_written_at_null() {
    let lib = Lib::new();
    let flac = require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    lib.scan().await;
    let a = lib.track_id("A/1.flac");
    lib.set_rg(a, values(2.0, None), 1000);
    let rep = lib.editor.prepare_rg_write(None, vec![a]).await.unwrap();
    let batch_id = rep.batch_id.unwrap();
    // ワーカーが動く前に外部が書き換える → 事前条件不一致
    common::retag(&flac, |t| {
        use lofty::tag::Accessor as _;
        t.set_title("外部".to_owned());
    });
    lib.start();
    assert_eq!(lib.wait_batch_terminal(batch_id).await, BatchState::Failed);
    assert_eq!(lib.ops(batch_id)[0].result, OpResult::SkippedConflict);
    assert!(file_tags(&flac, "REPLAYGAIN_TRACK_GAIN").is_empty());
    // overlay の解消でファイルの現在値に戻り、rg_written_at は立たない
    assert!(lib.db_tag(a, "REPLAYGAIN_TRACK_GAIN").is_empty());
    assert_eq!(lib.db_tag(a, "TITLE"), ["外部"]);
    assert_eq!(lib.row(a).written_at, None);
}

#[tokio::test]
async fn rerun_after_rescan_with_new_values_rewrites() {
    let lib = Lib::new();
    let flac = require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    lib.scan().await;
    let a = lib.track_id("A/1.flac");
    lib.set_rg(a, values(2.0, None), 1000);
    lib.start();
    let rep = lib.editor.prepare_rg_write(None, vec![a]).await.unwrap();
    lib.wait_batch_terminal(rep.batch_id.unwrap()).await;
    let v1 = lib.row(a).tag_version;

    lib.set_rg(a, values(3.0, Some(3.0)), 2000);
    let rep = lib.editor.prepare_rg_write(None, vec![a]).await.unwrap();
    let batch_id = rep.batch_id.unwrap();
    assert_eq!(lib.wait_batch_terminal(batch_id).await, BatchState::Applied);
    assert_eq!(file_tags(&flac, "REPLAYGAIN_TRACK_GAIN"), ["+3.00 dB"]);
    assert_eq!(file_tags(&flac, "REPLAYGAIN_ALBUM_GAIN"), ["+3.00 dB"]);
    let r = lib.row(a);
    assert!(r.written_at.is_some_and(|w| w >= 2000));
    assert_eq!(r.tag_version, v1 + 1);
    // 履歴には前の値が旧値として残る
    let edits = history::list_edits(&lib.conn(), lib.ops(batch_id)[0].id).unwrap();
    let gain = edits
        .iter()
        .find(|e| e.key == "REPLAYGAIN_TRACK_GAIN")
        .unwrap();
    assert_eq!(gain.old_value, serde_json::json!(["+2.00 dB"]));
}

// ---------------------------------------------------------------- 再解析と rg_written_at

/// 時刻は秒単位なので、確認と再解析が同じ秒に起きると `rg_written_at < rg_scanned_at` では
/// 検出できない。`store` は値が変わった行の `rg_written_at` を NULL にする
#[tokio::test]
async fn store_with_changed_values_clears_written_at_even_in_the_same_second() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    lib.scan().await;
    let a = lib.track_id("A/1.flac");
    lib.set_rg(a, values(2.0, None), 1000);
    lib.start();
    let rep = lib.editor.prepare_rg_write(None, vec![a]).await.unwrap();
    lib.wait_batch_terminal(rep.batch_id.unwrap()).await;
    let written = lib.row(a).written_at.unwrap();

    // 同じ秒に別の値で再解析 → 未書き込み
    dbrg::store(&lib.conn(), &[(a, values(3.0, None))], written).unwrap();
    let r = lib.row(a);
    assert_eq!(r.scanned_at, Some(written));
    assert_eq!(r.written_at, None, "{r:?}");
    let rep = lib.editor.prepare_rg_write(None, vec![a]).await.unwrap();
    assert!(rep.batch_id.is_some());
    lib.wait_batch_terminal(rep.batch_id.unwrap()).await;
    let written = lib.row(a).written_at.unwrap();

    // 同じ値で再解析 → 確認は成り立ったままなので進む（rg_unwritten に落ちない）
    dbrg::store(&lib.conn(), &[(a, values(3.0, None))], written + 10).unwrap();
    let r = lib.row(a);
    assert_eq!(r.scanned_at, Some(written + 10));
    assert_eq!(r.written_at, Some(written + 10), "{r:?}");

    // album の値だけ変わっても NULL
    dbrg::store(&lib.conn(), &[(a, values(3.0, Some(1.0)))], written + 10).unwrap();
    assert_eq!(lib.row(a).written_at, None);

    // 確認が無い（NULL）行は同じ値でも立てない
    dbrg::store(&lib.conn(), &[(a, values(3.0, Some(1.0)))], written + 20).unwrap();
    assert_eq!(lib.row(a).written_at, None);
}

// ---------------------------------------------------------------- スキャナの追随（D-47 / D-48 の未決）

/// 外部で音声が差し替わって `audio_version` が進んだら解析値は古い: `rg_scanned_at` / `rg_written_at`
/// を NULL に戻す（次の解析までは未解析扱い。古い値を Derived や再生に使わない）
#[tokio::test]
async fn external_audio_replacement_resets_rg_analysis_on_scan() {
    let lib = Lib::new();
    let flac = require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    lib.scan().await;
    let id = lib.track_id("A/1.flac");
    lib.set_rg(id, values(-2.5, Some(-1.0)), 1000);
    lib.conn()
        .execute("UPDATE tracks SET rg_written_at = 1001 WHERE id = ?1", [id])
        .unwrap();
    let before = lib.row(id);

    // 同じパスに別の音声（seed 違い）を置く
    std::fs::remove_file(&flac).unwrap();
    lib.add("A/1.flac", 9, "one");
    lib.scan().await;
    let after = lib.row(id);
    assert_eq!(after.audio_version, before.audio_version + 1);
    assert_eq!(after.scanned_at, None);
    assert_eq!(after.written_at, None);

    // 音声が変わらないタグだけの変更では据え置き
    lib.set_rg(id, values(-2.5, Some(-1.0)), 2000);
    common::retag(&flac, |t| {
        t.insert_text(lofty::tag::ItemKey::TrackTitle, "renamed".to_owned());
    });
    lib.scan().await;
    let row = lib.row(id);
    assert_eq!(row.audio_version, after.audio_version);
    assert_eq!(row.scanned_at, Some(2000));
}

/// 外部ツールが RG タグを消したり書き換えたりしたら `rg_written_at` を判定し直す
#[tokio::test]
async fn external_tag_change_resyncs_rg_written_at_on_scan() {
    let lib = Lib::new();
    let flac = require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    lib.scan().await;
    let id = lib.track_id("A/1.flac");
    lib.set_rg(id, values(-2.5, Some(-1.0)), 1000);
    lib.start();
    let rep = lib.editor.prepare_rg_write(None, vec![id]).await.unwrap();
    assert_eq!(
        lib.wait_batch_terminal(rep.batch_id.unwrap()).await,
        BatchState::Applied
    );
    assert!(lib.row(id).written_at.is_some());

    // 外部ツールが RG タグを消す → 未書込に戻る
    common::retag(&flac, |t| {
        t.remove_key(lofty::tag::ItemKey::ReplayGainTrackGain);
        t.remove_key(lofty::tag::ItemKey::ReplayGainTrackPeak);
        t.remove_key(lofty::tag::ItemKey::ReplayGainAlbumGain);
        t.remove_key(lofty::tag::ItemKey::ReplayGainAlbumPeak);
    });
    lib.scan().await;
    assert_eq!(lib.row(id).written_at, None);
    assert_eq!(lib.row(id).scanned_at, Some(1000), "解析値は残る");

    // 外部ツールが解析値と一致する RG タグを書く → 書込済みに戻る
    common::retag(&flac, |t| {
        t.insert_text(
            lofty::tag::ItemKey::ReplayGainTrackGain,
            "-2.50 dB".to_owned(),
        );
        t.insert_text(
            lofty::tag::ItemKey::ReplayGainTrackPeak,
            "0.500000".to_owned(),
        );
        t.insert_text(
            lofty::tag::ItemKey::ReplayGainAlbumGain,
            "-1.00 dB".to_owned(),
        );
        t.insert_text(
            lofty::tag::ItemKey::ReplayGainAlbumPeak,
            "0.750000".to_owned(),
        );
    });
    lib.scan().await;
    assert!(lib.row(id).written_at.is_some());
}

/// tagwrite が事前条件不一致で外部の実体を読んだとき（overlay の解消）も、音声が差し替わっていれば
/// 解析値を捨てる（スキャナと同じ規則）
#[tokio::test]
async fn audio_replacement_seen_by_tagwrite_conflict_resets_rg_analysis() {
    let lib = Lib::new();
    let flac = require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    lib.scan().await;
    let id = lib.track_id("A/1.flac");
    lib.set_rg(id, values(-2.5, Some(-1.0)), 1000);
    let p = lib
        .editor
        .prepare_tags(
            None,
            vec![NewTagOp {
                track_id: id,
                changes: vec![set("TITLE", &["x"])],
            }],
        )
        .await
        .unwrap();
    std::fs::remove_file(&flac).unwrap();
    lib.add("A/1.flac", 9, "one");
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(p.batch_id).await,
        BatchState::Failed
    );
    let row = lib.row(id);
    assert_eq!(row.audio_version, 2);
    assert_eq!(row.scanned_at, None);
}

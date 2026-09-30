//! ReplayGain の自動書き込みと、Derived を RG の書き込み後に作る順序（D-97）。
//! スキャンで新規トラックの rg を積み、rg の保存で rgwrite を積み、rgwrite が編集バッチを作り、
//! tagwrite の applied で `rg_written_at` が立ってから transcode が積まれる。
//! 合成ファイルは ffmpeg で作る（無ければ skip）

#![cfg(target_os = "linux")]

mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{params, Connection};
use tokio_util::sync::CancellationToken;

use spindle::config::{DerivedConfig, OpusVariantConfig};
use spindle::db::history::{self, BatchState};
use spindle::db::{derived, replaygain as dbrg, Db};
use spindle::domain::tags::read_audio_file;
use spindle::edit::{Editor, NewTagOp, TagChange};
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, Scanner};
use spindle::jobs::handlers::rg::RgHandler;
use spindle::jobs::handlers::rgwrite::{RgwriteHandler, DESCRIPTION};
use spindle::jobs::handlers::tagwrite::TagwriteHandler;
use spindle::jobs::{JobType, Jobs, Registry};
use spindle::media::decode::Decoder;

const REFERENCE: f64 = -18.0;

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    root: Arc<RootDir>,
    jobs: Arc<Jobs>,
    scanner: Scanner,
    editor: Arc<Editor>,
    shutdown: CancellationToken,
}

impl Lib {
    /// opus 系統を on、`rg_write_required = write_tags` で写した Library
    fn new(write_tags: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Library");
        std::fs::create_dir(&lib).unwrap();
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let root = Arc::new(RootDir::open(&lib).unwrap());
        let jobs = Jobs::new(db.clone());
        let scanner = Scanner::new(db.clone(), root.clone(), 2).with_analyze_new_tracks(true);
        let editor = Arc::new(
            Editor::new(db, root.clone(), jobs.clone()).with_replaygain_reference(REFERENCE),
        );
        let this = Self {
            dir,
            db_path,
            root,
            jobs,
            scanner,
            editor,
            shutdown: CancellationToken::new(),
        };
        let cfg = DerivedConfig {
            opus: OpusVariantConfig {
                enabled: true,
                bitrate: 128,
            },
            aac: Default::default(),
        };
        derived::sync_variants(&this.conn(), &cfg, write_tags, 0).unwrap();
        this
    }

    /// rg / rgwrite / tagwrite を動かす（transcode は登録しないので積まれたまま残る）
    fn start(&self, write_tags: bool) {
        let mut reg = Registry::new();
        reg.register(
            JobType::Rg,
            Arc::new(
                RgHandler::new(self.root.clone(), Decoder::new("ffmpeg"), REFERENCE)
                    .with_write_tags(write_tags),
            ),
        );
        reg.register(
            JobType::Rgwrite,
            Arc::new(RgwriteHandler::new(self.editor.clone(), write_tags)),
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

    fn album_id(&self, id: i64) -> i64 {
        self.conn()
            .query_row("SELECT album_id FROM tracks WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
    }

    /// 種別ごとの dedup key（id 順。状態は問わない）
    fn job_keys(&self, ty: &str) -> Vec<String> {
        let conn = self.conn();
        let mut st = conn
            .prepare("SELECT dedup_key FROM jobs WHERE type = ?1 ORDER BY id")
            .unwrap();
        st.query_map([ty], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// `track_id` の `ty` ジョブの id（id 順）
    fn job_ids_of_track(&self, ty: &str, track_id: i64) -> Vec<i64> {
        let conn = self.conn();
        let mut st = conn
            .prepare(
                "SELECT id FROM jobs WHERE type = ?1 AND json_extract(payload, '$.track_id') = ?2
                  ORDER BY id",
            )
            .unwrap();
        st.query_map(params![ty, track_id], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// 種別のジョブ（dedup キーの有無は問わない。rgwrite は実行時にキーを外す）
    fn job_keys_all(&self, ty: &str) -> Vec<i64> {
        let conn = self.conn();
        let mut st = conn
            .prepare("SELECT id FROM jobs WHERE type = ?1 ORDER BY id")
            .unwrap();
        st.query_map([ty], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn due(&self, id: i64) -> i64 {
        self.conn()
            .query_row("SELECT rg_write_due FROM tracks WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
    }

    fn enqueue_write(&self) -> i64 {
        dbrg::enqueue_write(&self.conn(), 0).unwrap()
    }

    fn job_state(&self, id: i64) -> String {
        self.conn()
            .query_row("SELECT state FROM jobs WHERE id = ?1", [id], |r| r.get(0))
            .unwrap()
    }

    fn written_at(&self, id: i64) -> Option<i64> {
        self.conn()
            .query_row(
                "SELECT rg_written_at FROM tracks WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// 自動書き込みのバッチ（id 順）
    fn auto_batches(&self) -> Vec<i64> {
        let conn = self.conn();
        let mut st = conn
            .prepare("SELECT id FROM edit_batches WHERE description = ?1 ORDER BY id")
            .unwrap();
        st.query_map([DESCRIPTION], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn batch_state(&self, id: i64) -> BatchState {
        history::get_batch(&self.conn(), id).unwrap().unwrap().state
    }

    /// 解析済みの状態を作る（rg ジョブの保存の模擬。書き込みは未確認で、書き込み待ちの印を立てる）
    fn set_rg(&self, id: i64, track_gain: f64, scanned_at: i64) {
        self.conn()
            .execute(
                "UPDATE tracks SET rg_track_gain = ?2, rg_track_peak = 0.5, rg_scanned_at = ?3,
                        rg_written_at = NULL, rg_write_due = 1
                  WHERE id = ?1",
                params![id, track_gain, scanned_at],
            )
            .unwrap();
    }

    /// 条件が成り立つまで待つ（CI のランナーは I/O が遅いので経過時間で待つ）
    async fn wait_until(&self, what: &str, mut f: impl FnMut(&Self) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while std::time::Instant::now() < deadline {
            if f(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{what} にならない");
    }

    /// 積まれたジョブが（登録していない transcode を除いて）すべて終端になるまで待つ
    async fn wait_idle(&self) {
        self.wait_until("ジョブが空", |l| {
            l.conn()
                .query_row(
                    "SELECT count(*) FROM jobs
                      WHERE state IN ('queued', 'running') AND type != 'transcode'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
                == 0
        })
        .await;
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

// ---------------------------------------------------------------- スキャン → rg の投入

#[tokio::test]
async fn scan_enqueues_analysis_only_for_new_tracks() {
    let lib = Lib::new(true);
    require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    lib.add("A/2.flac", 2, "two").unwrap();
    lib.scan().await;
    let a = lib.track_id("A/1.flac");
    let b = lib.track_id("A/2.flac");
    // album gain が off（既定）の album は track 単位
    let mut keys = lib.job_keys("rg");
    keys.sort();
    let mut want = vec![format!("rg:track:{a}"), format!("rg:track:{b}")];
    want.sort();
    assert_eq!(keys, want);
    // 既存の行は（未解析のままでも）拾わない。新しく置いた曲だけ
    lib.conn().execute("DELETE FROM jobs", []).unwrap();
    lib.scan().await;
    assert!(lib.job_keys("rg").is_empty());
    lib.add("A/3.flac", 3, "three").unwrap();
    lib.scan().await;
    let c = lib.track_id("A/3.flac");
    assert_eq!(lib.job_keys("rg"), vec![format!("rg:track:{c}")]);
    // 解析が終わるまで Derived は積まれない
    assert!(lib.job_keys("transcode").is_empty());
}

#[tokio::test]
async fn new_track_joining_an_album_gain_album_is_analyzed_per_album() {
    let lib = Lib::new(true);
    require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    lib.scan().await;
    let a = lib.track_id("A/1.flac");
    let album = lib.album_id(a);
    lib.conn()
        .execute("UPDATE albums SET album_gain = 1 WHERE id = ?1", [album])
        .unwrap();
    lib.conn().execute("DELETE FROM jobs", []).unwrap();
    lib.add("A/2.flac", 2, "two").unwrap();
    lib.scan().await;
    assert_eq!(lib.job_keys("rg"), vec![format!("rg:album:{album}")]);
}

// ---------------------------------------------------------------- 解析 → 書き込み → Derived

#[tokio::test]
async fn analysis_writes_tags_then_derived_follows() {
    let lib = Lib::new(true);
    let one = require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    let two = lib.add("A/2.opus", 2, "two").unwrap();
    lib.scan().await;
    let a = lib.track_id("A/1.flac");
    let b = lib.track_id("A/2.opus");
    lib.start(true);
    lib.wait_until("両方書き込み済み", |l| {
        l.written_at(a).is_some() && l.written_at(b).is_some()
    })
    .await;
    lib.wait_idle().await;

    assert_eq!(file_tags(&one, "REPLAYGAIN_TRACK_GAIN").len(), 1);
    assert_eq!(file_tags(&two, "R128_TRACK_GAIN").len(), 1);
    // 同じ album の track 単位の解析は、出揃ってから 1 つのバッチにまとめて書く
    assert!(!lib.job_keys_all("rgwrite").is_empty());
    assert_eq!(lib.due(a), 0);
    assert_eq!(lib.due(b), 0);
    let batches = lib.auto_batches();
    assert_eq!(batches.len(), 1, "{batches:?}");
    assert_eq!(lib.batch_state(batches[0]), BatchState::Applied);
    let ops = history::list_ops(&lib.conn(), batches[0]).unwrap();
    let mut op_tracks: Vec<i64> = ops.iter().map(|o| o.track_id).collect();
    op_tracks.sort_unstable();
    let mut want = vec![a, b];
    want.sort_unstable();
    assert_eq!(op_tracks, want);

    // Derived（opus）は可逆の 1 だけ。書き込みの tagwrite の後に積まれる（解析の保存では積まれない）
    let transcodes = lib.job_ids_of_track("transcode", a);
    assert_eq!(transcodes.len(), 1, "{transcodes:?}");
    let tagwrite = lib.job_ids_of_track("tagwrite", a);
    assert_eq!(tagwrite.len(), 1);
    assert!(
        transcodes[0] > tagwrite[0],
        "transcode が書き込みより先に積まれた"
    );
    assert!(lib.job_ids_of_track("transcode", b).is_empty());
}

#[tokio::test]
async fn write_tags_off_leaves_tags_alone_and_derived_follows_analysis() {
    let lib = Lib::new(false);
    let one = require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    lib.scan().await;
    let a = lib.track_id("A/1.flac");
    lib.start(false);
    lib.wait_idle().await;
    // rgwrite は積まれない。印は残る（有効にした後の起動時の回収で書く）
    assert!(lib.job_keys_all("rgwrite").is_empty());
    assert_eq!(lib.due(a), 1);
    assert!(lib.auto_batches().is_empty());
    assert!(file_tags(&one, "REPLAYGAIN_TRACK_GAIN").is_empty());
    assert_eq!(lib.written_at(a), None);
    // タグに書かない運用では解析済みだけを待つ
    assert_eq!(lib.job_ids_of_track("transcode", a).len(), 1);
}

// ---------------------------------------------------------------- rgwrite の待ちと対象

#[tokio::test]
async fn rgwrite_waits_for_pending_edits_instead_of_skipping() {
    let lib = Lib::new(true);
    let one = require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    lib.scan().await;
    lib.conn().execute("DELETE FROM jobs", []).unwrap();
    let a = lib.track_id("A/1.flac");
    lib.set_rg(a, 1.0, 1000);
    // 反映待ちの編集がある状態で rgwrite を積む
    lib.editor
        .prepare_tags(
            None,
            vec![NewTagOp {
                track_id: a,
                changes: vec![TagChange {
                    key: "TITLE".into(),
                    values: Some(vec!["x".into()]),
                }],
            }],
        )
        .await
        .unwrap();
    let job = lib.enqueue_write();
    lib.start(true);
    lib.wait_until("書き込み済み", |l| l.written_at(a).is_some())
        .await;
    lib.wait_idle().await;
    assert_eq!(lib.job_state(job), "done");
    assert_eq!(file_tags(&one, "TITLE"), vec!["x"]);
    assert_eq!(file_tags(&one, "REPLAYGAIN_TRACK_GAIN"), vec!["+1.00 dB"]);
    assert_eq!(lib.due(a), 0);
}

/// 印の付いた行だけを書く。巻き戻しは印を立てないので、同じ解析時刻で同じ album の別の行が解析されても
/// 巻き戻した行は書き直さない（時刻の境界に依らない）
#[tokio::test]
async fn undo_is_not_rewritten_even_when_the_album_is_analyzed_in_the_same_second() {
    let lib = Lib::new(true);
    let one = require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    let two = lib.add("A/2.flac", 2, "two").unwrap();
    lib.scan().await;
    lib.conn().execute("DELETE FROM jobs", []).unwrap();
    let a = lib.track_id("A/1.flac");
    let b = lib.track_id("A/2.flac");
    lib.set_rg(a, 1.0, 1000);
    lib.enqueue_write();
    lib.start(true);
    lib.wait_until("a が書き込み済み", |l| l.written_at(a).is_some())
        .await;
    lib.wait_idle().await;
    let batch = lib.auto_batches()[0];
    let rev = lib.editor.revert_batch(batch, None).await.unwrap();
    lib.wait_until("巻き戻し", |l| {
        l.batch_state(rev.batch_id) == BatchState::Applied
    })
    .await;
    lib.wait_idle().await;
    assert!(file_tags(&one, "REPLAYGAIN_TRACK_GAIN").is_empty());
    assert_eq!((lib.written_at(a), lib.due(a)), (None, 0));

    // 同じ album の b を a と同じ解析時刻で解析した
    lib.set_rg(b, 2.0, 1000);
    let job = lib.enqueue_write();
    lib.jobs.notify_enqueued(&[job]).await;
    lib.wait_until("b が書き込み済み", |l| l.written_at(b).is_some())
        .await;
    lib.wait_idle().await;
    assert_eq!(file_tags(&two, "REPLAYGAIN_TRACK_GAIN"), vec!["+2.00 dB"]);
    assert!(file_tags(&one, "REPLAYGAIN_TRACK_GAIN").is_empty());
    assert_eq!(lib.written_at(a), None);
}

/// missing の間に回ってきた rgwrite は行を飛ばすが印は残り、戻った走査の完了で積み直して書く。
/// 別の album へ移った行も、印で拾うので書き漏れない
#[tokio::test]
async fn missing_and_moved_rows_keep_their_mark_until_written() {
    let lib = Lib::new(true);
    let one = require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    lib.scan().await;
    lib.conn().execute("DELETE FROM jobs", []).unwrap();
    let a = lib.track_id("A/1.flac");
    lib.set_rg(a, 1.0, 1000);
    // 一時的に Library から外す
    let away = lib.dir.path().join("away.flac");
    std::fs::rename(&one, &away).unwrap();
    lib.scan().await;
    lib.conn().execute("DELETE FROM jobs", []).unwrap();
    let job = lib.enqueue_write();
    lib.start(true);
    lib.wait_until("rgwrite が終端", |l| l.job_state(job) == "done")
        .await;
    assert_eq!(lib.due(a), 1, "missing の行は飛ばして印を残す");
    assert!(lib.auto_batches().is_empty());

    // 別の album（ディレクトリ）へ戻す → 同じ行として追随し、走査の完了で rgwrite が積まれる
    let moved = lib.lib().join("B/1.flac");
    std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
    std::fs::rename(&away, &moved).unwrap();
    lib.scan().await;
    assert_eq!(lib.track_id("B/1.flac"), a);
    let pending: Vec<i64> = lib.job_keys_all("rgwrite");
    lib.jobs.notify_enqueued(&pending).await;
    lib.wait_until("書き込み済み", |l| l.written_at(a).is_some())
        .await;
    lib.wait_idle().await;
    assert_eq!(file_tags(&moved, "REPLAYGAIN_TRACK_GAIN"), vec!["+1.00 dB"]);
    assert_eq!(lib.due(a), 0);
}

/// 実行中の rgwrite が dedup キーを外した後の投入は新しいジョブになる（実行中のジョブへ合流して、
/// 読み終えた対象に吸われて消えることがない）
#[tokio::test]
async fn enqueue_after_release_creates_a_new_job() {
    let lib = Lib::new(true);
    let c = lib.conn();
    let first = lib.enqueue_write();
    assert_eq!(lib.enqueue_write(), first, "queued の間は合流する");
    c.execute("UPDATE jobs SET state = 'running' WHERE id = ?1", [first])
        .unwrap();
    assert_eq!(
        lib.enqueue_write(),
        first,
        "キーを外す前は running にも合流する"
    );
    assert!(spindle::db::jobs::release_dedup_key(&c, first).unwrap());
    let second = lib.enqueue_write();
    assert_ne!(second, first);
    assert_eq!(lib.enqueue_write(), second);
}

/// album gain を off にすると album の値を消した行に印が立ち、rgwrite がファイルの album のキーを消し、
/// その後に Derived が追随する（write_tags = true。D-74 / D-97）
#[tokio::test]
async fn album_gain_off_rewrites_tags_and_derived_follows() {
    let lib = Lib::new(true);
    let one = require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    lib.scan().await;
    let a = lib.track_id("A/1.flac");
    let album = lib.album_id(a);
    lib.conn()
        .execute("UPDATE albums SET album_gain = 1 WHERE id = ?1", [album])
        .unwrap();
    lib.conn().execute("DELETE FROM jobs", []).unwrap();
    // album 単位で解析し、自動で書き込むまで進める
    spindle::db::jobs::enqueue(&lib.conn(), &dbrg::new_album_job(album), 0).unwrap();
    lib.start(true);
    lib.wait_until("書き込み済み", |l| l.written_at(a).is_some())
        .await;
    lib.wait_idle().await;
    assert_eq!(file_tags(&one, "REPLAYGAIN_ALBUM_GAIN").len(), 1);
    // 最初の Derived の投入（未実行で残る）は消し、off の後の追随だけを数える
    lib.conn()
        .execute("DELETE FROM jobs WHERE type = 'transcode'", [])
        .unwrap();

    // off にする（API と同じ組み立て）
    let job = {
        let c = lib.conn();
        let change = dbrg::set_album_gain(&c, album, false, 0).unwrap().unwrap();
        assert_eq!(change.cleared, vec![a]);
        assert_eq!(lib.due(a), 1);
        dbrg::enqueue_write(&c, 0).unwrap()
    };
    lib.jobs.notify_enqueued(&[job]).await;
    lib.wait_until("書き直し", |l| {
        l.written_at(a).is_some() && l.due(a) == 0
    })
    .await;
    lib.wait_idle().await;
    assert!(file_tags(&one, "REPLAYGAIN_ALBUM_GAIN").is_empty());
    assert_eq!(file_tags(&one, "REPLAYGAIN_TRACK_GAIN").len(), 1);
    assert_eq!(
        lib.job_ids_of_track("transcode", a).len(),
        1,
        "書き込みの後に Derived の追随が積まれる"
    );
}

// ---------------------------------------------------------------- 外部の変更と待機の収束

/// 印が立った後に外部ツールが RG のキーを書き換えたら、その値を自動書き込みで上書きしない（ファイルが正。
/// 印は走査で下りる）。RG 以外のタグだけが外部で変わった行は印を残して書く
#[tokio::test]
async fn external_rg_change_cancels_the_pending_auto_write() {
    let lib = Lib::new(true);
    let one = require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    let two = lib.add("A/2.flac", 2, "two").unwrap();
    lib.scan().await;
    lib.conn().execute("DELETE FROM jobs", []).unwrap();
    let a = lib.track_id("A/1.flac");
    let b = lib.track_id("A/2.flac");
    lib.set_rg(a, 1.0, 1000);
    lib.set_rg(b, 2.0, 1000);
    // a は外部が RG を書いた、b は外部がタイトルだけ変えた
    common::retag(&one, |t| {
        t.insert_text(
            lofty::tag::ItemKey::ReplayGainTrackGain,
            "+9.00 dB".to_owned(),
        );
    });
    common::retag(&two, |t| {
        lofty::tag::Accessor::set_title(t, "two!".to_owned());
    });
    lib.scan().await;
    assert_eq!(lib.due(a), 0, "外部の RG の変更で印を下ろす");
    assert_eq!(lib.due(b), 1, "RG 以外の変更では印を残す");
    let job = lib.enqueue_write();
    lib.start(true);
    lib.wait_until("rgwrite が終端", |l| l.job_state(job) == "done")
        .await;
    lib.wait_until("b が書き込み済み", |l| l.written_at(b).is_some())
        .await;
    lib.wait_idle().await;
    assert_eq!(file_tags(&one, "REPLAYGAIN_TRACK_GAIN"), vec!["+9.00 dB"]);
    assert_eq!(lib.written_at(a), None);
    assert_eq!(file_tags(&two, "REPLAYGAIN_TRACK_GAIN"), vec!["+2.00 dB"]);
    assert_eq!(file_tags(&two, "TITLE"), vec!["two!"]);
}

/// 解析が残っている album を待つ間、rgwrite は自分を Requeue せずキー付きの後継を 1 本だけ積む。
/// 待機中の rgwrite は増えず、解析が終われば書く
#[tokio::test]
async fn waiting_for_analysis_keeps_a_single_rgwrite() {
    let lib = Lib::new(true);
    let one = require_ffmpeg!(lib.add("A/1.flac", 1, "one"));
    lib.scan().await;
    lib.conn().execute("DELETE FROM jobs", []).unwrap();
    let a = lib.track_id("A/1.flac");
    lib.set_rg(a, 1.0, 1000);
    // 同じ track を解析する rg が待っている（遠い未来まで走らない）
    let blocker = spindle::db::jobs::enqueue(
        &lib.conn(),
        &dbrg::new_track_job(a).run_after(i64::MAX / 2),
        0,
    )
    .unwrap()
    .id();
    lib.enqueue_write();
    lib.start(true);
    let active = |l: &Lib| -> i64 {
        l.conn()
            .query_row(
                "SELECT count(*) FROM jobs WHERE type = 'rgwrite' AND state IN ('queued', 'running')",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    // 何度か後継に引き継がれるまで待つ
    lib.wait_until("後継に引き継ぐ", |l| {
        l.job_keys_all("rgwrite").len() >= 3
    })
    .await;
    // rg の完了の模擬（キー付きの投入）が重なっても、待機中の rgwrite は 1 本に収まる
    for _ in 0..5 {
        lib.enqueue_write();
        assert!(active(&lib) <= 2, "待機中の rgwrite が増えている");
    }
    assert_eq!(lib.due(a), 1);
    assert!(file_tags(&one, "REPLAYGAIN_TRACK_GAIN").is_empty());
    // 解析が終わった（rg が消えた）ら書く
    lib.conn()
        .execute("DELETE FROM jobs WHERE id = ?1", [blocker])
        .unwrap();
    lib.wait_until("書き込み済み", |l| l.written_at(a).is_some())
        .await;
    lib.wait_idle().await;
    assert_eq!(file_tags(&one, "REPLAYGAIN_TRACK_GAIN"), vec!["+1.00 dB"]);
    assert_eq!(active(&lib), 0);
}

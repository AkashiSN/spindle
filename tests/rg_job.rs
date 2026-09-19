//! `rg` ジョブ（SPEC §6「ReplayGain の内部表現」/ §8、D-22、docs/TASKS.md P1-1）。
//! album 単位で構成トラックを解析し、track / album の gain と peak を DB に書く。
//! タグには書かない（P1-2）。合成ファイルは ffmpeg で作る（無ければ skip）

#![cfg(target_os = "linux")]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use spindle::db::{now_epoch, Db};
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, Scanner};
use spindle::jobs::handlers::rg::{new_album_job, new_track_job, RgHandler};
use spindle::jobs::{JobState, JobType, Jobs, Registry};
use spindle::media::decode::Decoder;

const REFERENCE: f64 = -18.0;

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    jobs: Arc<Jobs>,
    scanner: Scanner,
    handler: Arc<RgHandler>,
    shutdown: CancellationToken,
}

impl Lib {
    fn new() -> Self {
        Self::with_ffmpeg("ffmpeg")
    }

    fn with_ffmpeg(ffmpeg: impl AsRef<std::path::Path>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Library")).unwrap();
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let root = Arc::new(RootDir::open(&dir.path().join("Library")).unwrap());
        let jobs = Jobs::new(db.clone());
        let scanner = Scanner::new(db.clone(), root.clone(), 2);
        let handler = Arc::new(RgHandler::new(root, Decoder::new(ffmpeg), REFERENCE));
        Self {
            dir,
            db_path,
            jobs,
            scanner,
            handler,
            shutdown: CancellationToken::new(),
        }
    }

    fn start(&self) {
        let mut reg = Registry::new();
        reg.register(JobType::Rg, self.handler.clone());
        self.jobs.start(reg, self.shutdown.clone());
    }

    fn lib(&self) -> PathBuf {
        self.dir.path().join("Library")
    }

    /// `rel` に `amp_db` dBFS の 1 kHz 正弦波（2 秒）を置く。`ext` は flac / opus / flac6（6ch）
    fn add(&self, rel: &str, amp_db: f32, title: &str) -> PathBuf {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let wav = p.with_extension("src.wav");
        common::write_wav(&wav, &sine(amp_db), 16);
        let ext = rel.rsplit_once('.').unwrap().1;
        let args: &[&str] = match ext {
            "flac" => &["-c:a", "flac"],
            "opus" => &["-c:a", "libopus", "-b:a", "128k"],
            other => panic!("unsupported {other}"),
        };
        common::encode(&wav, &p, args).unwrap();
        std::fs::remove_file(&wav).unwrap();
        common::set_basic_tags(&p, title, "Artist", "Album", "AlbumArtist", 1, 1);
        p
    }

    /// 6ch の FLAC（ステレオをアップミックス）
    fn add_multichannel(&self, rel: &str, amp_db: f32, title: &str) -> PathBuf {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let wav = p.with_extension("src.wav");
        common::write_wav(&wav, &sine(amp_db), 16);
        common::encode(&wav, &p, &["-ac", "6", "-c:a", "flac"]).unwrap();
        std::fs::remove_file(&wav).unwrap();
        common::set_basic_tags(&p, title, "Artist", "Album", "AlbumArtist", 1, 1);
        p
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

    fn album_id(&self, rel: &str) -> i64 {
        self.conn()
            .query_row(
                "SELECT album_id FROM tracks WHERE rel_path = ?1",
                [rel],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn rg(&self, rel: &str) -> Rg {
        self.conn()
            .query_row(
                "SELECT rg_track_gain, rg_track_peak, rg_album_gain, rg_album_peak,
                        rg_scanned_at, rg_written_at, audio_version, tag_version, channels
                 FROM tracks WHERE rel_path = ?1",
                [rel],
                |r| {
                    Ok(Rg {
                        track_gain: r.get(0)?,
                        track_peak: r.get(1)?,
                        album_gain: r.get(2)?,
                        album_peak: r.get(3)?,
                        scanned_at: r.get(4)?,
                        written_at: r.get(5)?,
                        audio_version: r.get(6)?,
                        tag_version: r.get(7)?,
                        channels: r.get(8)?,
                    })
                },
            )
            .unwrap()
    }

    async fn wait_job(&self, id: i64) -> JobState {
        for _ in 0..3000 {
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

#[derive(Debug)]
struct Rg {
    track_gain: Option<f64>,
    track_peak: Option<f64>,
    album_gain: Option<f64>,
    album_peak: Option<f64>,
    scanned_at: Option<i64>,
    written_at: Option<i64>,
    audio_version: i64,
    tag_version: i64,
    channels: Option<i64>,
}

/// 2 秒の 1 kHz 正弦波（両チャンネル同じ振幅）。積分ラウドネス ≈ `amp_db` LUFS
fn sine(amp_db: f32) -> Vec<i32> {
    let rate = 44_100usize;
    let amp = 10f32.powf(amp_db / 20.0) * 32767.0;
    let mut out = Vec::with_capacity(rate * 4);
    for i in 0..rate * 2 {
        let t = i as f32 / rate as f32;
        let v = ((t * 1000.0 * std::f32::consts::TAU).sin() * amp) as i32;
        out.push(v);
        out.push(v);
    }
    out
}

fn close(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() < tol
}

#[test]
fn dedup_keys_include_the_scope() {
    assert_eq!(new_album_job(12).dedup_key.as_deref(), Some("rg:album:12"));
    assert_eq!(new_track_job(7).dedup_key.as_deref(), Some("rg:track:7"));
    assert_eq!(new_album_job(12).job_type, JobType::Rg);
}

#[tokio::test]
async fn album_job_writes_track_and_album_values() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new();
    lib.add("A/01.flac", -20.0, "loud");
    lib.add("A/02.flac", -26.0, "quiet");
    lib.add_multichannel("A/03.flac", -20.0, "surround");
    lib.scan().await;
    let album = lib.album_id("A/01.flac");
    assert_eq!(lib.rg("A/03.flac").channels, Some(6));

    lib.start();
    let id = lib.jobs.enqueue(new_album_job(album)).await.unwrap().id();
    assert_eq!(lib.wait_job(id).await, JobState::Done);

    let loud = lib.rg("A/01.flac");
    let quiet = lib.rg("A/02.flac");
    let multi = lib.rg("A/03.flac");
    // track gain = -18 - LUFS
    assert!(close(loud.track_gain.unwrap(), 2.0, 0.3), "{loud:?}");
    assert!(close(quiet.track_gain.unwrap(), 8.0, 0.3), "{quiet:?}");
    assert!(close(loud.track_peak.unwrap(), 0.1, 0.01), "{loud:?}");
    // album は 2ch の 2 曲だけをまとめてゲートした値。両曲で同じで、各 track の値の間にある
    let ag = loud.album_gain.unwrap();
    assert_eq!(quiet.album_gain, Some(ag));
    assert!(ag > 2.0 && ag < 8.0, "album gain = {ag}");
    assert!(close(loud.album_peak.unwrap(), 0.1, 0.01), "{loud:?}");
    // 6ch は track の値だけ。album の集計には入らず album の値も持たない（D-22）
    assert!(
        multi.track_gain.is_some() && multi.track_peak.is_some(),
        "{multi:?}"
    );
    assert_eq!(multi.album_gain, None);
    assert_eq!(multi.album_peak, None);
    for r in [&loud, &quiet, &multi] {
        assert!(r.scanned_at.is_some(), "{r:?}");
        assert_eq!(r.written_at, None, "書き込みは P1-2");
        assert_eq!((r.audio_version, r.tag_version), (1, 1), "版は動かない");
    }
}

/// 行の dev だけが古い（ホスト再起動で振り直された）ときは同じ実体として測る（D-62）
#[tokio::test]
async fn member_with_stale_dev_only_is_still_scanned() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new();
    lib.add("D/01.flac", -20.0, "loud");
    lib.scan().await;
    let album = lib.album_id("D/01.flac");
    let tid = lib.track_id("D/01.flac");
    lib.conn()
        .execute("UPDATE tracks SET dev = dev + 1 WHERE id = ?1", [tid])
        .unwrap();
    lib.start();
    let id = lib.jobs.enqueue(new_album_job(album)).await.unwrap().id();
    assert_eq!(lib.wait_job(id).await, JobState::Done);
    assert!(lib.rg("D/01.flac").scanned_at.is_some());
}

#[tokio::test]
async fn opus_member_is_decoded_via_ffmpeg() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new();
    lib.add("B/01.flac", -20.0, "flac");
    lib.add("B/02.opus", -20.0, "opus");
    lib.scan().await;
    lib.start();
    let id = lib
        .jobs
        .enqueue(new_album_job(lib.album_id("B/01.flac")))
        .await
        .unwrap()
        .id();
    assert_eq!(lib.wait_job(id).await, JobState::Done);
    let f = lib.rg("B/01.flac");
    let o = lib.rg("B/02.opus");
    assert!(close(o.track_gain.unwrap(), 2.0, 0.5), "{o:?}");
    assert!(close(f.track_gain.unwrap(), o.track_gain.unwrap(), 0.5));
    assert_eq!(f.album_gain, o.album_gain);
}

#[tokio::test]
async fn missing_tracks_are_left_alone_and_excluded_from_album() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new();
    lib.add("C/01.flac", -20.0, "a");
    lib.add("C/02.flac", -26.0, "b");
    lib.scan().await;
    let gone = lib.track_id("C/02.flac");
    lib.conn()
        .execute(
            "UPDATE tracks SET missing_since = ?1 WHERE id = ?2",
            (now_epoch(), gone),
        )
        .unwrap();
    lib.start();
    let id = lib
        .jobs
        .enqueue(new_album_job(lib.album_id("C/01.flac")))
        .await
        .unwrap()
        .id();
    assert_eq!(lib.wait_job(id).await, JobState::Done);
    let a = lib.rg("C/01.flac");
    let b = lib.rg("C/02.flac");
    assert!(
        close(a.album_gain.unwrap(), a.track_gain.unwrap(), 0.01),
        "{a:?}"
    );
    assert_eq!(b.scanned_at, None, "{b:?}");
}

#[tokio::test]
async fn track_job_for_a_track_without_album() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new();
    lib.add("D/01.flac", -20.0, "solo");
    lib.scan().await;
    let tid = lib.track_id("D/01.flac");
    lib.conn()
        .execute("UPDATE tracks SET album_id = NULL WHERE id = ?1", [tid])
        .unwrap();
    lib.start();
    let id = lib.jobs.enqueue(new_track_job(tid)).await.unwrap().id();
    assert_eq!(lib.wait_job(id).await, JobState::Done);
    let r = lib.rg("D/01.flac");
    assert!(close(r.track_gain.unwrap(), 2.0, 0.3), "{r:?}");
    assert_eq!(r.album_gain, None);
    assert_eq!(r.album_peak, None);
    assert!(r.scanned_at.is_some());
}

#[tokio::test]
async fn undecodable_member_fails_the_whole_album() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new();
    lib.add("E/01.flac", -20.0, "ok");
    let bad = lib.add("E/02.flac", -20.0, "broken");
    lib.scan().await;
    std::fs::write(&bad, b"fLaCnot really").unwrap();
    lib.start();
    let id = lib
        .jobs
        .enqueue(new_album_job(lib.album_id("E/01.flac")).max_attempts(1))
        .await
        .unwrap()
        .id();
    assert_eq!(lib.wait_job(id).await, JobState::Failed);
    let err = lib.last_error(id).unwrap();
    assert!(err.contains("E/02.flac"), "{err}");
    // 途中まで解析できても album が揃わないので何も書かない
    assert_eq!(lib.rg("E/01.flac").scanned_at, None);
}

#[tokio::test]
async fn album_without_active_tracks_is_a_noop() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new();
    lib.add("F/01.flac", -20.0, "x");
    lib.scan().await;
    let album = lib.album_id("F/01.flac");
    lib.conn()
        .execute("UPDATE tracks SET missing_since = ?1", [now_epoch()])
        .unwrap();
    lib.start();
    let id = lib.jobs.enqueue(new_album_job(album)).await.unwrap().id();
    assert_eq!(lib.wait_job(id).await, JobState::Done);
    assert_eq!(lib.rg("F/01.flac").scanned_at, None);
}

#[tokio::test]
async fn cancel_before_start_is_honoured() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new();
    lib.add("G/01.flac", -20.0, "x");
    lib.scan().await;
    let album = lib.album_id("G/01.flac");
    let id = lib.jobs.enqueue(new_album_job(album)).await.unwrap().id();
    lib.jobs.cancel(id).await.unwrap();
    lib.start();
    assert_eq!(lib.wait_job(id).await, JobState::Cancelled);
    assert_eq!(lib.rg("G/01.flac").scanned_at, None);
}

#[tokio::test]
async fn swapped_file_is_detected_and_nothing_written() {
    // DB を読んでから open するまでに外部が同名で別ファイルに差し替えた（inode が変わる）。
    // パスは識別子ではないので、その track の値として保存してはいけない
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new();
    lib.add("H/01.flac", -20.0, "a");
    let target = lib.add("H/02.flac", -26.0, "b");
    lib.scan().await;
    let other = lib.add("H/other.flac", -10.0, "c");
    std::fs::rename(&other, &target).unwrap();
    lib.start();
    let id = lib
        .jobs
        .enqueue(new_album_job(lib.album_id("H/01.flac")).max_attempts(1))
        .await
        .unwrap()
        .id();
    assert_eq!(lib.wait_job(id).await, JobState::Failed);
    let err = lib.last_error(id).unwrap();
    assert!(err.contains("H/02.flac"), "{err}");
    assert_eq!(lib.rg("H/01.flac").scanned_at, None);
    assert_eq!(lib.rg("H/02.flac").scanned_at, None);
}

#[tokio::test]
async fn member_going_missing_during_analysis_fails_without_writing() {
    // 解析中に構成が変わった（scanner が 1 本を missing にした）。開始時の集合で計算した
    // album gain は現在の構成と合わないので、何も書かずに失敗する（再試行で取り直す）
    let ffmpeg = require_ffmpeg!(common::ffmpeg());
    let dir = tempfile::tempdir().unwrap();
    // Opus は ffmpeg 経路なので、遅い ffmpeg で解析中の窓を作る
    let slow = dir.path().join("slow-ffmpeg");
    std::fs::write(
        &slow,
        format!("#!/bin/sh\nsleep 1\nexec {} \"$@\"\n", ffmpeg.display()),
    )
    .unwrap();
    std::fs::set_permissions(&slow, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let lib = Lib::with_ffmpeg(&slow);
    lib.add("I/01.opus", -20.0, "a");
    lib.add("I/02.flac", -26.0, "b");
    lib.scan().await;
    let album = lib.album_id("I/01.opus");
    let gone = lib.track_id("I/02.flac");
    lib.start();
    let id = lib
        .jobs
        .enqueue(new_album_job(album).max_attempts(1))
        .await
        .unwrap()
        .id();
    tokio::time::sleep(Duration::from_millis(400)).await;
    lib.conn()
        .execute(
            "UPDATE tracks SET missing_since = ?1 WHERE id = ?2",
            (now_epoch(), gone),
        )
        .unwrap();
    assert_eq!(lib.wait_job(id).await, JobState::Failed);
    assert_eq!(lib.rg("I/01.opus").scanned_at, None);
    assert_eq!(lib.rg("I/02.flac").scanned_at, None);
}

#[tokio::test]
async fn row_rebound_to_a_new_file_during_analysis_is_not_written() {
    // 解析中（旧 inode の FD を読んでいる間）に外部が同じパスを別音声へ差し替え、scanner が
    // 同じ track 行を新実体へ更新した。旧 FD の stat は変わらないので解析後の照合は通るが、
    // 行はもう別の音声を指している。id 列だけの比較では見逃すので、行の stat まで比較する
    let ffmpeg = require_ffmpeg!(common::ffmpeg());
    let dir = tempfile::tempdir().unwrap();
    let slow = dir.path().join("slow-ffmpeg");
    std::fs::write(
        &slow,
        format!("#!/bin/sh\nsleep 1\nexec {} \"$@\"\n", ffmpeg.display()),
    )
    .unwrap();
    std::fs::set_permissions(&slow, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let lib = Lib::with_ffmpeg(&slow);
    let target = lib.add("J/01.opus", -20.0, "a");
    lib.scan().await;
    let album = lib.album_id("J/01.opus");
    let before = lib.track_id("J/01.opus");
    lib.start();
    let id = lib
        .jobs
        .enqueue(new_album_job(album).max_attempts(1))
        .await
        .unwrap()
        .id();
    tokio::time::sleep(Duration::from_millis(400)).await;
    // 別音声を同じパスへ rename（新 inode）し、scanner に行を追随させる
    let other = lib.add("J/other.opus", -10.0, "b");
    std::fs::rename(&other, &target).unwrap();
    lib.scan().await;
    assert_eq!(lib.track_id("J/01.opus"), before, "同じ行が新実体を指す");
    assert_eq!(lib.wait_job(id).await, JobState::Failed);
    assert_eq!(lib.rg("J/01.opus").scanned_at, None);
}

#[tokio::test]
async fn row_stat_updated_during_analysis_is_not_written() {
    // 上の差し替えを DB 側だけで模擬する（scanner が同じ行の dev / inode 等を新実体へ更新した
    // 状態。旧 FD の stat は開始時のスナップショットと一致するので、書き込み時の行の比較でしか
    // 検出できない）
    let ffmpeg = require_ffmpeg!(common::ffmpeg());
    let dir = tempfile::tempdir().unwrap();
    let slow = dir.path().join("slow-ffmpeg");
    std::fs::write(
        &slow,
        format!("#!/bin/sh\nsleep 1\nexec {} \"$@\"\n", ffmpeg.display()),
    )
    .unwrap();
    std::fs::set_permissions(&slow, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let lib = Lib::with_ffmpeg(&slow);
    lib.add("K/01.opus", -20.0, "a");
    lib.scan().await;
    let album = lib.album_id("K/01.opus");
    let tid = lib.track_id("K/01.opus");
    lib.start();
    let id = lib
        .jobs
        .enqueue(new_album_job(album).max_attempts(1))
        .await
        .unwrap()
        .id();
    tokio::time::sleep(Duration::from_millis(400)).await;
    lib.conn()
        .execute(
            "UPDATE tracks SET inode = inode + 1, audio_version = audio_version + 1 WHERE id = ?1",
            [tid],
        )
        .unwrap();
    assert_eq!(lib.wait_job(id).await, JobState::Failed);
    assert_eq!(lib.rg("K/01.opus").scanned_at, None);
}

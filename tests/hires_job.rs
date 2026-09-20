//! `hirescheck` ジョブ（SPEC §7.10 / §8、docs/TASKS.md P3-5、D-71）。合成した 24/96 の可逆を
//! デコードして判定と計測値を版付きで記録する。ファイルは書かない。
//! 合成ファイルは ffmpeg で作る（無ければ skip）

#![cfg(target_os = "linux")]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use spindle::db::Db;
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, Scanner};
use spindle::jobs::handlers::hirescheck::{new_hirescheck_job, HirescheckHandler};
use spindle::jobs::handlers::scan::ScanHandler;
use spindle::jobs::{EnqueueResult, JobState, JobType, Jobs, Registry};
use spindle::media::decode::Decoder;
use spindle::media::hires::Thresholds;

const RATE: u32 = 96_000;

struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// 24 bit のステレオ白色雑音（全帯域、全ビットを使う）。`secs` 秒
fn noise24(secs: u32, seed: u64) -> Vec<i32> {
    let mut rng = Xorshift(seed | 1);
    (0..RATE * secs * 2)
        .map(|_| ((rng.next() >> 40) as i32 & 0x00ff_ffff) - 0x0080_0000)
        .collect()
}

/// 下位 8 bit をゼロにする（16 bit を 24 bit に詰めた形）
fn pad16(samples: &[i32]) -> Vec<i32> {
    samples.iter().map(|s| s & !0xff).collect()
}

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    jobs: Arc<Jobs>,
    scanner: Arc<Scanner>,
    root: Arc<RootDir>,
    shutdown: CancellationToken,
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Library")).unwrap();
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let root = Arc::new(RootDir::open(&dir.path().join("Library")).unwrap());
        let jobs = Jobs::new(db.clone());
        let scanner = Arc::new(Scanner::new(db.clone(), root.clone(), 2));
        Self {
            dir,
            db_path,
            jobs,
            scanner,
            root,
            shutdown: CancellationToken::new(),
        }
    }

    fn start(&self, check_on_import: bool) {
        let mut reg = Registry::new();
        reg.register(
            JobType::Hirescheck,
            Arc::new(HirescheckHandler::new(
                self.root.clone(),
                Decoder::new("ffmpeg"),
                Thresholds {
                    cutoff_hz: 25_000,
                    cliff_db: 30.0,
                },
            )),
        );
        reg.register(
            JobType::Scan,
            Arc::new(ScanHandler::new(self.scanner.clone(), 0).with_hires_check(check_on_import)),
        );
        self.jobs.start(reg, self.shutdown.clone());
    }

    fn lib(&self) -> PathBuf {
        self.dir.path().join("Library")
    }

    /// `rel` に `samples`（インターリーブ、`bits`、`rate`）のファイルを作る。拡張子で形式を決める
    fn add(&self, rel: &str, samples: &[i32], bits: u32, rate: u32) -> PathBuf {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let wav = p.with_extension("src.wav");
        common::write_wav_ex(&wav, samples, bits, rate, 2);
        let args: &[&str] = match rel.rsplit_once('.').unwrap().1 {
            "flac" => &["-c:a", "flac"],
            "wv" => &["-c:a", "wavpack"],
            other => panic!("unknown ext {other}"),
        };
        common::encode(&wav, &p, args).unwrap();
        std::fs::remove_file(&wav).unwrap();
        common::set_basic_tags(&p, "t", "Artist", "Album", "Artist", 1, 1);
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

    fn track(&self, rel: &str) -> (i64, i64) {
        self.conn()
            .query_row(
                "SELECT id, audio_version FROM tracks WHERE rel_path = ?1",
                [rel],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    fn props(&self, rel: &str) -> (Option<i64>, Option<i64>) {
        self.conn()
            .query_row(
                "SELECT sample_rate, bit_depth FROM tracks WHERE rel_path = ?1",
                [rel],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    fn result(&self, rel: &str) -> Check {
        self.conn()
            .query_row(
                "SELECT hires_check, hires_checked_at, hires_check_version, hires_check_error,
                        hires_cutoff_hz, hires_cliff_db, hires_effective_bits
                 FROM tracks WHERE rel_path = ?1",
                [rel],
                |r| {
                    Ok(Check {
                        status: r.get(0)?,
                        checked_at: r.get(1)?,
                        version: r.get(2)?,
                        error: r.get(3)?,
                        cutoff_hz: r.get(4)?,
                        cliff_db: r.get(5)?,
                        effective_bits: r.get(6)?,
                    })
                },
            )
            .unwrap()
    }

    async fn run(&self, rel: &str) -> JobState {
        let (id, ver) = self.track(rel);
        let job = match self
            .jobs
            .enqueue(new_hirescheck_job(id, ver))
            .await
            .unwrap()
        {
            EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
        };
        self.wait_job(job).await
    }

    async fn wait_job(&self, id: i64) -> JobState {
        let deadline = std::time::Instant::now() + Duration::from_secs(180);
        while std::time::Instant::now() < deadline {
            let s: String = self
                .conn()
                .query_row("SELECT state FROM jobs WHERE id = ?1", [id], |r| r.get(0))
                .unwrap();
            let st: JobState = s.parse().unwrap();
            if st.is_terminal() {
                return st;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let row: (String, Option<i64>, i64, Option<String>) = self
            .conn()
            .query_row(
                "SELECT state, run_after, attempts, last_error FROM jobs WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        panic!("job {id} が終端にならない: {row:?}");
    }

    fn jobs_of_type(&self, ty: &str) -> i64 {
        self.conn()
            .query_row("SELECT count(*) FROM jobs WHERE type = ?1", [ty], |r| {
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
struct Check {
    status: Option<String>,
    checked_at: Option<i64>,
    version: Option<i64>,
    error: Option<String>,
    cutoff_hz: Option<i64>,
    cliff_db: Option<f64>,
    effective_bits: Option<i64>,
}

fn mtime_of(p: &Path) -> std::time::SystemTime {
    std::fs::metadata(p).unwrap().modified().unwrap()
}

#[tokio::test]
async fn full_band_24bit_noise_is_ok_and_padded_bits_are_reported() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new();
    let ok = lib.add("A/ok.flac", &noise24(1, 1), 24, RATE);
    lib.add("A/padded.flac", &pad16(&noise24(1, 2)), 24, RATE);
    lib.scan().await;
    assert_eq!(lib.props("A/ok.flac"), (Some(96_000), Some(24)));
    let before = mtime_of(&ok);
    lib.start(false);

    assert_eq!(lib.run("A/ok.flac").await, JobState::Done);
    let r = lib.result("A/ok.flac");
    assert_eq!(r.status.as_deref(), Some("ok"), "{r:?}");
    assert_eq!(r.cutoff_hz, Some(48_000), "白色雑音は Nyquist まで");
    assert_eq!(r.cliff_db, None);
    assert_eq!(r.effective_bits, Some(24));
    assert_eq!(r.version, Some(1));
    assert!(r.checked_at.is_some());
    assert_eq!(r.error, None);
    assert_eq!(mtime_of(&ok), before, "読むだけ");

    assert_eq!(lib.run("A/padded.flac").await, JobState::Done);
    let r = lib.result("A/padded.flac");
    assert_eq!(r.status.as_deref(), Some("padded"), "{r:?}");
    assert_eq!(r.effective_bits, Some(16));
    assert_eq!(r.cutoff_hz, Some(48_000));

    // 同じ版の再投入は dedup、終わった後は再投入できて結果は同じ
    assert_eq!(lib.run("A/ok.flac").await, JobState::Done);
    assert_eq!(lib.result("A/ok.flac").status.as_deref(), Some("ok"));
}

#[tokio::test]
async fn bit_depth_only_target_at_44k_skips_the_spectrum() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new();
    // 44.1 kHz の 24 bit（bit_depth だけで対象）。噪音を 44.1k で書く
    let samples: Vec<i32> = pad16(&noise24(1, 3))[..44_100 * 2].to_vec();
    lib.add("A/p44.flac", &samples, 24, 44_100);
    lib.scan().await;
    lib.start(false);
    assert_eq!(lib.run("A/p44.flac").await, JobState::Done);
    let r = lib.result("A/p44.flac");
    assert_eq!(r.status.as_deref(), Some("padded"), "{r:?}");
    assert_eq!((r.cutoff_hz, r.cliff_db), (None, None));
    assert_eq!(r.effective_bits, Some(16));
}

#[tokio::test]
async fn ffmpeg_fallback_keeps_the_integer_scale_of_symphonia() {
    // WavPack は symphonia が demux できず ffmpeg の f32le を通る。ffmpeg の wavpack エンコーダは
    // 32 bit でしか書けない（wavpack CLI も無い）ので、24 bit の値を 8 bit 左に寄せた 32 bit の
    // .wv を作り、24 bit のファイルとして sink に流す。ffmpeg の f32le が symphonia と同じ
    // 2^-(bits-1) のスケールなら、下位 8 bit ゼロ / 非ゼロ・最大正負が同じ実効ビットに戻る
    // （D-71 のビット判定の前提）
    use spindle::media::decode::PcmSink;
    use spindle::media::hires::{HiresSink, Measurement};

    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let dir = tempfile::tempdir().unwrap();
    let decode = |name: &str, samples: &[i32]| {
        let wav = dir.path().join(format!("{name}.wav"));
        let shifted: Vec<i32> = samples.iter().map(|v| v << 8).collect();
        common::write_wav_ex(&wav, &shifted, 32, RATE, 2);
        let wv = dir.path().join(format!("{name}.wv"));
        common::encode(&wav, &wv, &["-c:a", "wavpack"])?;
        Some(wv)
    };
    let mut full = noise24(1, 5);
    full[0] = 0x007f_ffff;
    full[1] = -0x0080_0000;
    let Some(full_wv) = decode("full", &full) else {
        eprintln!("ffmpeg に wavpack が無いので skip");
        return;
    };
    let padded_wv = decode("padded", &pad16(&noise24(1, 6))).unwrap();
    let decoder = Decoder::new("ffmpeg");
    let measure = |path: PathBuf| {
        let decoder = decoder.clone();
        async move {
            let file = std::fs::File::open(&path).unwrap();
            let (info, sink) = decoder
                .decode(
                    file,
                    Some("wv"),
                    HiresSink::new(Some(24)),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            assert_eq!(info.sample_rate, RATE);
            let m: Measurement = sink.finish();
            m
        }
    };
    let m = measure(full_wv).await;
    assert_eq!(m.effective_bits, Some(24), "{m:?}");
    assert_eq!(m.cutoff_hz, Some(48_000), "{m:?}");
    let m = measure(padded_wv).await;
    assert_eq!(m.effective_bits, Some(16), "{m:?}");
    // 同じ経路で start / push が呼ばれていること（sink が空で終わらない）
    let mut probe = HiresSink::new(Some(24));
    probe
        .start(&spindle::media::decode::PcmInfo {
            channels: 2,
            sample_rate: RATE,
        })
        .unwrap();
    assert!(probe.push(&[0.0; 4]).is_ok());
}

#[tokio::test]
async fn non_target_or_replaced_or_advanced_tracks_are_not_recorded() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new();
    // 16/44 は対象外
    let s16: Vec<i32> = noise24(1, 7)[..44_100 * 2].iter().map(|v| v >> 8).collect();
    lib.add("A/cd.flac", &s16, 16, 44_100);
    let hi = lib.add("A/hi.flac", &noise24(1, 8), 24, RATE);
    lib.scan().await;
    lib.start(false);
    assert_eq!(lib.run("A/cd.flac").await, JobState::Done);
    assert_eq!(lib.result("A/cd.flac").status, None, "対象外は記録しない");

    // 差し替え済み（fstat が行と一致しない）は記録せず Done
    let (id, ver) = lib.track("A/hi.flac");
    std::fs::remove_file(&hi).unwrap();
    lib.add("A/hi.flac", &noise24(1, 9), 24, RATE);
    let job = match lib.jobs.enqueue(new_hirescheck_job(id, ver)).await.unwrap() {
        EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
    };
    assert_eq!(lib.wait_job(job).await, JobState::Done);
    assert_eq!(
        lib.result("A/hi.flac").status,
        None,
        "差し替えは次のスキャン待ち"
    );

    // 版が進んだ payload は stale ゲートで no-op
    lib.scan().await;
    let (id, ver) = lib.track("A/hi.flac");
    lib.conn()
        .execute(
            "UPDATE tracks SET audio_version = audio_version + 1 WHERE id = ?1",
            [id],
        )
        .unwrap();
    let job = match lib.jobs.enqueue(new_hirescheck_job(id, ver)).await.unwrap() {
        EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
    };
    assert_eq!(lib.wait_job(job).await, JobState::Done);
    assert_eq!(lib.result("A/hi.flac").status, None);
}

#[tokio::test]
async fn corrupted_file_is_recorded_as_decode_error() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new();
    let p = lib.add("A/bad.flac", &noise24(1, 10), 24, RATE);
    let mut bytes = std::fs::read(&p).unwrap();
    // fLaC マーカーの直後から壊す（STREAMINFO ごと）。lofty が読めなくなると scanner が拾わないので
    // 先にスキャンし、壊した後の実体を行に追随させる
    lib.scan().await;
    let half = bytes.len() / 2;
    for b in &mut bytes[64..half] {
        *b ^= 0xa5;
    }
    std::fs::write(&p, &bytes).unwrap();
    let st = std::fs::metadata(&p).unwrap();
    use std::os::unix::fs::MetadataExt;
    lib.conn()
        .execute(
            "UPDATE tracks SET inode = ?2, size = ?3, mtime_ns = ?4, ctime_ns = ?5 WHERE rel_path = ?1",
            rusqlite::params![
                "A/bad.flac",
                st.ino() as i64,
                st.len() as i64,
                st.mtime() * 1_000_000_000 + st.mtime_nsec(),
                st.ctime() * 1_000_000_000 + st.ctime_nsec()
            ],
        )
        .unwrap();
    lib.start(false);
    assert_eq!(
        lib.run("A/bad.flac").await,
        JobState::Done,
        "検査結果であってジョブの失敗ではない"
    );
    let r = lib.result("A/bad.flac");
    assert_eq!(r.status.as_deref(), Some("decode_error"), "{r:?}");
    assert!(r.error.is_some());
    assert_eq!(
        (r.cutoff_hz, r.cliff_db, r.effective_bits),
        (None, None, None)
    );
}

#[tokio::test]
async fn scan_enqueues_unchecked_targets_when_enabled() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new();
    lib.add("A/hi.flac", &noise24(1, 11), 24, RATE);
    let s16: Vec<i32> = noise24(1, 12)[..44_100 * 2]
        .iter()
        .map(|v| v >> 8)
        .collect();
    lib.add("A/cd.flac", &s16, 16, 44_100);
    lib.start(true);
    let scan = match lib
        .jobs
        .enqueue(spindle::jobs::handlers::scan::new_scan_job(
            ScanKind::Incremental,
        ))
        .await
        .unwrap()
    {
        EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
    };
    assert_eq!(lib.wait_job(scan).await, JobState::Done);
    assert_eq!(lib.jobs_of_type("hirescheck"), 1, "対象の 24/96 だけ");
    let id: i64 = lib
        .conn()
        .query_row("SELECT id FROM jobs WHERE type = 'hirescheck'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(lib.wait_job(id).await, JobState::Done);
    assert_eq!(lib.result("A/hi.flac").status.as_deref(), Some("ok"));
    // 検査済みなら次のスキャンで再投入しない
    let scan = match lib
        .jobs
        .enqueue(spindle::jobs::handlers::scan::new_scan_job(
            ScanKind::Incremental,
        ))
        .await
        .unwrap()
    {
        EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
    };
    assert_eq!(lib.wait_job(scan).await, JobState::Done);
    assert_eq!(lib.jobs_of_type("hirescheck"), 1);
}

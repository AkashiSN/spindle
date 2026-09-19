//! `verify` ジョブ（SPEC §7.3、§8、D-13、P2-9）。既存 FLAC のアルバムを CTDB / AccurateRip に
//! 遡及照合し、`album_verifications` / `track_verifications` / `tracks.verification` と
//! `data/verify/<album_id>.log` に残す。
//!
//! DB 側の応答はローカルの axum サーバで返す。テストが書いた PCM から P2-6 の計算で
//! 「基準の吸い出し」のエントリを作り、AccurateRip の bin / CTDB の XML に整形する。
//! FLAC は ffmpeg で作る（無ければ skip）

#![cfg(target_os = "linux")]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use rusqlite::Connection;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use spindle::cd::accuraterip::{AccurateRipClient, ArCalculator};
use spindle::cd::ctdb::{Crc32Calculator, CtdbClient};
use spindle::cd::toc::Toc;
use spindle::db::Db;
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, Scanner};
use spindle::jobs::handlers::verify::{new_verify_job, VerifyHandler};
use spindle::jobs::{EnqueueResult, JobState, JobType, Jobs, Registry};

const SECTOR: usize = 588;

// ---------------------------------------------------------------- 合成 DB サーバ

/// 返す内容。`ar` は AccurateRip の bin、`ctdb` は XML。None なら 404 / 空
#[derive(Default)]
struct Responses {
    ar: Option<Vec<u8>>,
    ctdb: Option<String>,
    ar_hits: usize,
    ctdb_hits: usize,
    /// エラーを返す
    fail: bool,
    /// CTDB の応答を止める（テストが照会中に手を入れるため）。notify_one で解放
    hold: Option<Arc<tokio::sync::Notify>>,
}

type Shared = Arc<Mutex<Responses>>;

async fn ar_handler(State(s): State<Shared>, AxPath(_p): AxPath<String>) -> (StatusCode, Vec<u8>) {
    let mut s = s.lock().await;
    s.ar_hits += 1;
    if s.fail {
        return (StatusCode::INTERNAL_SERVER_ERROR, Vec::new());
    }
    match &s.ar {
        Some(b) => (StatusCode::OK, b.clone()),
        None => (StatusCode::NOT_FOUND, Vec::new()),
    }
}

async fn ctdb_handler(
    State(s): State<Shared>,
    Query(_q): Query<Vec<(String, String)>>,
) -> (StatusCode, String) {
    let hold = {
        let mut s = s.lock().await;
        s.ctdb_hits += 1;
        s.hold.clone()
    };
    if let Some(h) = hold {
        h.notified().await;
    }
    let s = s.lock().await;
    if s.fail {
        return (StatusCode::INTERNAL_SERVER_ERROR, String::new());
    }
    match &s.ctdb {
        Some(x) => (StatusCode::OK, x.clone()),
        None => (
            StatusCode::OK,
            r#"<ctdb xmlns="http://db.cuetools.net/ns/mmd-1.0#"><metadata /></ctdb>"#.to_owned(),
        ),
    }
}

async fn serve() -> (String, Shared) {
    let shared: Shared = Arc::default();
    let app = Router::new()
        .route("/accuraterip/{*path}", get(ar_handler))
        .route("/lookup2.php", get(ctdb_handler))
        .with_state(Arc::clone(&shared));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (format!("http://{addr}"), shared)
}

/// 基準の吸い出し（トラックごとのインターリーブ i16）から DB の応答を作る
struct Reference {
    toc: Toc,
    tracks: Vec<Vec<i16>>,
}

impl Reference {
    fn pcm(&self) -> Vec<i16> {
        self.tracks.concat()
    }

    fn ar_bin(&self, confidence: u8) -> Vec<u8> {
        let layout = self.toc.track_layout().expect("layout");
        let mut calc = ArCalculator::new(&layout);
        calc.push(&self.pcm()).expect("push");
        let crcs = calc.finish().expect("finish");
        let id = self.toc.accuraterip_id();
        let mut out = vec![id.audio_tracks];
        out.extend_from_slice(&id.id1.to_le_bytes());
        out.extend_from_slice(&id.id2.to_le_bytes());
        out.extend_from_slice(&id.cddb.to_le_bytes());
        for c in &crcs {
            out.push(confidence);
            out.extend_from_slice(&c.v1.to_le_bytes());
            out.extend_from_slice(&c.crc450.unwrap_or(0).to_le_bytes());
        }
        out
    }

    fn ctdb_xml(&self, confidence: u32) -> String {
        let layout = self.toc.track_layout().expect("layout");
        let mut calc = Crc32Calculator::new(&layout);
        calc.push(&self.pcm()).expect("push");
        let crcs = calc.finish().expect("finish");
        let track_crcs: Vec<String> = crcs.tracks.iter().map(|c| format!("{c:08x}")).collect();
        format!(
            r#"<ctdb xmlns="http://db.cuetools.net/ns/mmd-1.0#"><entry confidence="{confidence}" crc32="{:08x}" id="1" npar="8" stride="5880" toc="{}" trackcrcs="{}" /><metadata /></ctdb>"#,
            crcs.disc,
            self.toc.ctdb_toc(),
            track_crcs.join(" ")
        )
    }
}

// ---------------------------------------------------------------- ライブラリ

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    db: Arc<Db>,
    jobs: Arc<Jobs>,
    scanner: Scanner,
    root: Arc<RootDir>,
    shutdown: CancellationToken,
    base: String,
    responses: Shared,
}

impl Lib {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Library")).unwrap();
        std::fs::create_dir(dir.path().join("data")).unwrap();
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let root = Arc::new(RootDir::open(&dir.path().join("Library")).unwrap());
        let jobs = Jobs::new(db.clone());
        let scanner = Scanner::new(db.clone(), root.clone(), 2);
        let (base, responses) = serve().await;
        Self {
            dir,
            db_path,
            db,
            jobs,
            scanner,
            root,
            shutdown: CancellationToken::new(),
            base,
            responses,
        }
    }

    fn start(&self) {
        let ar = AccurateRipClient::new(format!("{}/accuraterip/", self.base), "spindle-test/0.1")
            .unwrap();
        let ctdb =
            CtdbClient::new(format!("{}/lookup2.php", self.base), "spindle-test/0.1").unwrap();
        let mut reg = Registry::new();
        reg.register(
            JobType::Verify,
            Arc::new(VerifyHandler::new(
                self.root.clone(),
                self.dir.path().join("data"),
                ar,
                ctdb,
            )),
        );
        self.jobs.start(reg, self.shutdown.clone());
    }

    fn lib(&self) -> PathBuf {
        self.dir.path().join("Library")
    }

    /// `rel` に `samples`（インターリーブ i16）の FLAC を置き、タグを付ける
    fn add_flac(&self, rel: &str, samples: &[i16], album: &str, track: u32, disc: u32) -> PathBuf {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let wav = p.with_extension("src.wav");
        let as_i32: Vec<i32> = samples.iter().map(|&s| i32::from(s)).collect();
        common::write_wav(&wav, &as_i32, 16);
        common::encode(&wav, &p, common::encode_args("flac")).expect("ffmpeg");
        std::fs::remove_file(&wav).unwrap();
        common::set_basic_tags(
            &p,
            &format!("T{track}"),
            "Artist",
            album,
            "Artist",
            track,
            disc,
        );
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

    fn album_id(&self, album: &str) -> i64 {
        self.conn()
            .query_row("SELECT id FROM albums WHERE album = ?1", [album], |r| {
                r.get(0)
            })
            .unwrap()
    }

    async fn run(&self, album_id: i64) -> JobState {
        let job = match self.jobs.enqueue(new_verify_job(album_id)).await.unwrap() {
            EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
        };
        self.wait_job(job).await
    }

    async fn enqueue(&self, album_id: i64) -> i64 {
        match self.jobs.enqueue(new_verify_job(album_id)).await.unwrap() {
            EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
        }
    }

    /// CTDB の照会が来る（= デコードが終わり hold で止まっている）まで待つ
    async fn wait_ctdb_hit(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            if self.responses.lock().await.ctdb_hits > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("CTDB の照会が来ない");
    }

    async fn wait_job(&self, id: i64) -> JobState {
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
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

    fn verifications(&self, album_id: i64) -> Vec<Verification> {
        let conn = self.conn();
        let mut st = conn
            .prepare(
                "SELECT id, disc_no, method, result, source, detected_offset, confidence, log_path
                 FROM album_verifications WHERE album_id = ?1 ORDER BY disc_no, method",
            )
            .unwrap();
        st.query_map([album_id], |r| {
            Ok(Verification {
                id: r.get(0)?,
                disc_no: r.get(1)?,
                method: r.get(2)?,
                result: r.get(3)?,
                source: r.get(4)?,
                detected_offset: r.get(5)?,
                confidence: r.get(6)?,
                log_path: r.get(7)?,
            })
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    }

    fn track_verifications(&self, verification_id: i64) -> Vec<TrackVerification> {
        let conn = self.conn();
        let mut st = conn
            .prepare(
                "SELECT tv.track_id, tv.crc_v1, tv.crc_v2, tv.ctdb_crc, tv.matched
                 FROM track_verifications tv JOIN tracks t ON t.id = tv.track_id
                 WHERE tv.verification_id = ?1 ORDER BY t.disc_no, t.track_no",
            )
            .unwrap();
        st.query_map([verification_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    }

    fn track_states(&self, album_id: i64) -> Vec<String> {
        let conn = self.conn();
        let mut st = conn
            .prepare(
                "SELECT verification FROM tracks WHERE album_id = ?1 ORDER BY disc_no, track_no",
            )
            .unwrap();
        st.query_map([album_id], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<String>, _>>()
            .unwrap()
    }

    fn log_path(&self, album_id: i64) -> PathBuf {
        self.dir
            .path()
            .join("data")
            .join("verify")
            .join(format!("{album_id}.log"))
    }
}

impl Drop for Lib {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// (track_id, crc_v1, crc_v2, ctdb_crc, matched)
type TrackVerification = (i64, Option<i64>, Option<i64>, Option<i64>, bool);

#[derive(Debug, PartialEq)]
struct Verification {
    id: i64,
    disc_no: Option<i64>,
    method: String,
    result: String,
    source: String,
    detected_offset: Option<i64>,
    confidence: Option<i64>,
    log_path: Option<String>,
}

fn lcg_pcm(seed: u32, frames: usize) -> Vec<i16> {
    let mut x = seed;
    (0..frames * 2)
        .map(|_| {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            (x >> 16) as u16 as i16
        })
        .collect()
}

/// S'[i] = S[i + o]（外は 0）
fn shifted(pcm: &[i16], o: i64) -> Vec<i16> {
    let frames = pcm.len() / 2;
    (0..frames)
        .flat_map(|i| {
            let src = i as i64 + o;
            if src < 0 || src >= frames as i64 {
                [0, 0]
            } else {
                [pcm[src as usize * 2], pcm[src as usize * 2 + 1]]
            }
        })
        .collect()
}

/// セクタ数で指定したトラック列の基準吸い出し
fn reference(seed: u32, sectors: &[usize]) -> Reference {
    let tracks: Vec<Vec<i16>> = sectors
        .iter()
        .enumerate()
        .map(|(i, &n)| lcg_pcm(seed + i as u32, n * SECTOR))
        .collect();
    let toc = Toc::from_audio_sample_counts(sectors.iter().map(|&n| (n * SECTOR) as u64)).unwrap();
    Reference { toc, tracks }
}

fn read_log(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

// ---------------------------------------------------------------- テスト

/// 両 DB が一致: CTDB を主として verified_ctdb。行・CRC・ログが揃う
#[tokio::test]
async fn verified_album_records_both_methods_and_promotes_tracks() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new().await;
    let r = reference(1, &[75, 80]);
    lib.add_flac("A/Alb/01.flac", &r.tracks[0], "Alb", 1, 1);
    lib.add_flac("A/Alb/02.flac", &r.tracks[1], "Alb", 2, 1);
    lib.scan().await;
    {
        let mut s = lib.responses.lock().await;
        s.ar = Some(r.ar_bin(12));
        s.ctdb = Some(r.ctdb_xml(34));
    }
    lib.start();
    let album = lib.album_id("Alb");
    assert_eq!(lib.run(album).await, JobState::Done);

    let v = lib.verifications(album);
    assert_eq!(v.len(), 2, "{v:?}");
    let ar = v.iter().find(|x| x.method == "accuraterip").unwrap();
    let ctdb = v.iter().find(|x| x.method == "ctdb").unwrap();
    assert_eq!(ar.result, "verified");
    assert_eq!(ctdb.result, "verified");
    assert_eq!(ar.source, "retro");
    assert_eq!(ar.detected_offset, Some(0));
    assert_eq!(ctdb.detected_offset, Some(0));
    assert_eq!(ar.confidence, Some(12));
    assert_eq!(ctdb.confidence, Some(34));
    assert_eq!(ar.disc_no, Some(1));
    let log = lib.log_path(album);
    assert_eq!(ctdb.log_path.as_deref(), Some(log.to_str().unwrap()));
    assert!(log.is_file());
    let text = read_log(&log);
    assert!(text.contains(&r.toc.accuraterip_id().to_string()), "{text}");
    assert!(text.contains(&r.toc.musicbrainz_disc_id()), "{text}");

    let tv = lib.track_verifications(ctdb.id);
    assert_eq!(tv.len(), 2);
    assert!(tv.iter().all(|t| t.4 && t.3.is_some()));
    let tv = lib.track_verifications(ar.id);
    assert!(tv.iter().all(|t| t.4 && t.1.is_some() && t.2.is_some()));
    assert_eq!(
        lib.track_states(album),
        vec!["verified_ctdb", "verified_ctdb"]
    );
    let s = lib.responses.lock().await;
    assert_eq!((s.ar_hits, s.ctdb_hits), (1, 1));
}

/// 手元が基準より 3 サンプル遅れていれば offset 3 で一致し、detected_offset に残る
#[tokio::test]
async fn offset_is_detected_and_recorded() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new().await;
    let r = reference(2, &[75, 75]);
    let mine = shifted(&r.pcm(), -3);
    let (a, b) = mine.split_at(75 * SECTOR * 2);
    lib.add_flac("A/Off/01.flac", a, "Off", 1, 1);
    lib.add_flac("A/Off/02.flac", b, "Off", 2, 1);
    lib.scan().await;
    {
        let mut s = lib.responses.lock().await;
        s.ar = Some(r.ar_bin(5));
        s.ctdb = Some(r.ctdb_xml(9));
    }
    lib.start();
    let album = lib.album_id("Off");
    assert_eq!(lib.run(album).await, JobState::Done);
    let v = lib.verifications(album);
    assert!(
        v.iter()
            .all(|x| x.result == "verified" && x.detected_offset == Some(3)),
        "{v:?}"
    );
    assert_eq!(
        lib.track_states(album),
        vec!["verified_ctdb", "verified_ctdb"]
    );
}

/// CTDB に無く AccurateRip だけ一致 → verified_ar。CTDB の行は not_found
#[tokio::test]
async fn accuraterip_only_match_promotes_to_verified_ar() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new().await;
    let r = reference(3, &[75]);
    lib.add_flac("A/Ar/01.flac", &r.tracks[0], "Ar", 1, 1);
    lib.scan().await;
    lib.responses.lock().await.ar = Some(r.ar_bin(2));
    lib.start();
    let album = lib.album_id("Ar");
    assert_eq!(lib.run(album).await, JobState::Done);
    let v = lib.verifications(album);
    let ar = v.iter().find(|x| x.method == "accuraterip").unwrap();
    let ctdb = v.iter().find(|x| x.method == "ctdb").unwrap();
    assert_eq!(ar.result, "verified");
    assert_eq!(ctdb.result, "not_found");
    assert_eq!(lib.track_states(album), vec!["verified_ar"]);
}

/// 壊れたトラックがあれば mismatch。一致したトラックは verified、壊れたものは mismatch
#[tokio::test]
async fn corrupted_track_is_a_mismatch_without_touching_the_others() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new().await;
    let r = reference(4, &[75, 75]);
    let mut bad = r.tracks[1].clone();
    bad[20_000] ^= 0x0101;
    lib.add_flac("A/Bad/01.flac", &r.tracks[0], "Bad", 1, 1);
    lib.add_flac("A/Bad/02.flac", &bad, "Bad", 2, 1);
    lib.scan().await;
    {
        let mut s = lib.responses.lock().await;
        s.ar = Some(r.ar_bin(5));
        s.ctdb = Some(r.ctdb_xml(9));
    }
    lib.start();
    let album = lib.album_id("Bad");
    assert_eq!(lib.run(album).await, JobState::Done);
    let v = lib.verifications(album);
    assert!(v.iter().all(|x| x.result == "mismatch"), "{v:?}");
    assert_eq!(lib.track_states(album), vec!["verified_ctdb", "mismatch"]);
    let ctdb = v.iter().find(|x| x.method == "ctdb").unwrap();
    let tv = lib.track_verifications(ctdb.id);
    assert_eq!(
        tv.iter().map(|t| t.4).collect::<Vec<_>>(),
        vec![true, false]
    );
}

/// どちらの DB にも無い → not_found。トラックは not_attempted のまま。ログは書く
#[tokio::test]
async fn unknown_disc_is_not_found_and_tracks_stay_not_attempted() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new().await;
    let r = reference(5, &[75]);
    lib.add_flac("A/Nf/01.flac", &r.tracks[0], "Nf", 1, 1);
    lib.scan().await;
    lib.start();
    let album = lib.album_id("Nf");
    assert_eq!(lib.run(album).await, JobState::Done);
    let v = lib.verifications(album);
    assert_eq!(v.len(), 2);
    assert!(v.iter().all(|x| x.result == "not_found"));
    assert_eq!(lib.track_states(album), vec!["not_attempted"]);
    assert!(lib.log_path(album).is_file());
}

/// 588 の倍数でないサンプル数（CD 由来でない）は unverifiable
#[tokio::test]
async fn non_sector_aligned_disc_is_unverifiable() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new().await;
    let pcm = lcg_pcm(6, 75 * SECTOR + 1);
    lib.add_flac("A/Hi/01.flac", &pcm, "Hi", 1, 1);
    lib.scan().await;
    lib.start();
    let album = lib.album_id("Hi");
    assert_eq!(lib.run(album).await, JobState::Done);
    let v = lib.verifications(album);
    assert!(!v.is_empty());
    assert!(v.iter().all(|x| x.result == "unverifiable"), "{v:?}");
    assert_eq!(lib.track_states(album), vec!["unverifiable"]);
    let s = lib.responses.lock().await;
    assert_eq!((s.ar_hits, s.ctdb_hits), (0, 0), "照会しない");
}

/// トラック番号が抜けている（不完全なディスク）は TOC を作れないので何もしない
#[tokio::test]
async fn incomplete_disc_is_skipped() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new().await;
    let r = reference(7, &[75, 75]);
    lib.add_flac("A/Inc/01.flac", &r.tracks[0], "Inc", 1, 1);
    lib.add_flac("A/Inc/03.flac", &r.tracks[1], "Inc", 3, 1);
    lib.scan().await;
    lib.start();
    let album = lib.album_id("Inc");
    assert_eq!(lib.run(album).await, JobState::Done);
    assert!(lib.verifications(album).is_empty());
    assert_eq!(
        lib.track_states(album),
        vec!["not_attempted", "not_attempted"]
    );
}

/// 複数ディスクはディスクごとに TOC を作って照合し、disc_no 付きで記録する
#[tokio::test]
async fn multi_disc_album_is_verified_per_disc() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new().await;
    let d1 = reference(8, &[75]);
    let d2 = reference(9, &[80, 76]);
    lib.add_flac("A/Md/1-01.flac", &d1.tracks[0], "Md", 1, 1);
    lib.add_flac("A/Md/2-01.flac", &d2.tracks[0], "Md", 1, 2);
    lib.add_flac("A/Md/2-02.flac", &d2.tracks[1], "Md", 2, 2);
    lib.scan().await;
    // サーバは 1 種類しか返せないので disc 2 だけ一致させる
    lib.responses.lock().await.ctdb = Some(d2.ctdb_xml(3));
    lib.start();
    let album = lib.album_id("Md");
    assert_eq!(lib.run(album).await, JobState::Done);
    let v = lib.verifications(album);
    let disc1: Vec<&Verification> = v.iter().filter(|x| x.disc_no == Some(1)).collect();
    let disc2: Vec<&Verification> = v.iter().filter(|x| x.disc_no == Some(2)).collect();
    assert_eq!(disc1.len(), 2);
    assert_eq!(disc2.len(), 2);
    assert!(disc1.iter().all(|x| x.result == "not_found"));
    let ctdb2 = disc2.iter().find(|x| x.method == "ctdb").unwrap();
    assert_eq!(ctdb2.result, "verified");
    assert_eq!(
        lib.track_states(album),
        vec!["not_attempted", "verified_ctdb", "verified_ctdb"]
    );
    let text = read_log(&lib.log_path(album));
    assert!(text.contains(&d1.toc.musicbrainz_disc_id()), "{text}");
    assert!(text.contains(&d2.toc.musicbrainz_disc_id()), "{text}");
}

/// 照会に失敗したらジョブは失敗（再試行）し、何も記録しない
#[tokio::test]
async fn lookup_failure_fails_the_job_without_recording() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new().await;
    let r = reference(10, &[75]);
    lib.add_flac("A/Err/01.flac", &r.tracks[0], "Err", 1, 1);
    lib.scan().await;
    lib.responses.lock().await.fail = true;
    lib.start();
    let album = lib.album_id("Err");
    let job = match lib
        .jobs
        .enqueue(new_verify_job(album).max_attempts(1))
        .await
        .unwrap()
    {
        EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
    };
    assert_eq!(lib.wait_job(job).await, JobState::Failed);
    assert!(lib.verifications(album).is_empty());
    assert_eq!(lib.track_states(album), vec!["not_attempted"]);
    assert!(!lib.log_path(album).is_file());
}

/// 再照合は履歴として積む（前の行は消えない）。トラックの状態は最新の結果
#[tokio::test]
async fn re_verification_appends_history() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new().await;
    let r = reference(11, &[75]);
    lib.add_flac("A/Re/01.flac", &r.tracks[0], "Re", 1, 1);
    lib.scan().await;
    lib.start();
    let album = lib.album_id("Re");
    assert_eq!(lib.run(album).await, JobState::Done);
    assert_eq!(lib.track_states(album), vec!["not_attempted"]);
    lib.responses.lock().await.ctdb = Some(r.ctdb_xml(1));
    // 同じ dedup key のジョブは done の後なら再投入できる
    assert_eq!(lib.run(album).await, JobState::Done);
    let v = lib.verifications(album);
    assert_eq!(v.len(), 4, "{v:?}");
    assert_eq!(lib.track_states(album), vec!["verified_ctdb"]);
}

/// 照合中（デコード後、照会で止めている間）に音声が差し替わって audio_version が進んだら、
/// 古い結果は一切記録しない（部分記録もしない）
#[tokio::test]
async fn audio_version_change_during_verification_records_nothing() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new().await;
    let r = reference(12, &[75, 75]);
    lib.add_flac("A/Av/01.flac", &r.tracks[0], "Av", 1, 1);
    lib.add_flac("A/Av/02.flac", &r.tracks[1], "Av", 2, 1);
    lib.scan().await;
    let hold = Arc::new(tokio::sync::Notify::new());
    {
        let mut s = lib.responses.lock().await;
        s.ctdb = Some(r.ctdb_xml(9));
        s.hold = Some(Arc::clone(&hold));
    }
    lib.start();
    let album = lib.album_id("Av");
    let job = lib.enqueue(album).await;
    lib.wait_ctdb_hit().await;
    lib.conn()
        .execute(
            "UPDATE tracks SET audio_version = audio_version + 1 WHERE rel_path = 'A/Av/02.flac'",
            [],
        )
        .unwrap();
    hold.notify_one();
    assert_eq!(lib.wait_job(job).await, JobState::Done);
    assert!(lib.verifications(album).is_empty());
    assert_eq!(
        lib.track_states(album),
        vec!["not_attempted", "not_attempted"]
    );
    assert!(!lib.log_path(album).is_file());
    assert!(!lib
        .log_path(album)
        .with_file_name(format!(".{album}.log.tmp"))
        .is_file());
}

/// 照合中にファイルが差し替わった（inode が変わる）ら記録しない。DB の行はまだ古いまま
#[tokio::test]
async fn file_replaced_during_verification_records_nothing() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new().await;
    let r = reference(13, &[75]);
    let p = lib.add_flac("A/Rp/01.flac", &r.tracks[0], "Rp", 1, 1);
    lib.scan().await;
    let hold = Arc::new(tokio::sync::Notify::new());
    {
        let mut s = lib.responses.lock().await;
        s.ctdb = Some(r.ctdb_xml(9));
        s.hold = Some(Arc::clone(&hold));
    }
    lib.start();
    let album = lib.album_id("Rp");
    let job = lib.enqueue(album).await;
    lib.wait_ctdb_hit().await;
    // 別の内容で作り直して rename で差し替える
    let other = lcg_pcm(99, 75 * SECTOR);
    let tmp = p.with_extension("new.flac");
    let wav = p.with_extension("new.wav");
    let as_i32: Vec<i32> = other.iter().map(|&s| i32::from(s)).collect();
    common::write_wav(&wav, &as_i32, 16);
    common::encode(&wav, &tmp, common::encode_args("flac")).unwrap();
    std::fs::rename(&tmp, &p).unwrap();
    hold.notify_one();
    assert_eq!(lib.wait_job(job).await, JobState::Done);
    assert!(lib.verifications(album).is_empty());
    assert_eq!(lib.track_states(album), vec!["not_attempted"]);
    assert!(!lib.log_path(album).is_file());
}

/// 照会中にキャンセルされたら cancelled で終わり、ログも DB も増えない
#[tokio::test]
async fn cancel_during_lookup_records_nothing() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new().await;
    let r = reference(14, &[75]);
    lib.add_flac("A/Cn/01.flac", &r.tracks[0], "Cn", 1, 1);
    lib.scan().await;
    let hold = Arc::new(tokio::sync::Notify::new());
    {
        let mut s = lib.responses.lock().await;
        s.ctdb = Some(r.ctdb_xml(9));
        s.ar = Some(r.ar_bin(3));
        s.hold = Some(Arc::clone(&hold));
    }
    lib.start();
    let album = lib.album_id("Cn");
    let job = lib.enqueue(album).await;
    lib.wait_ctdb_hit().await;
    lib.jobs.cancel(job).await.unwrap();
    hold.notify_one();
    assert_eq!(lib.wait_job(job).await, JobState::Cancelled);
    assert!(lib.verifications(album).is_empty());
    assert_eq!(lib.track_states(album), vec!["not_attempted"]);
    assert!(!lib.log_path(album).is_file());
}

/// 記録は 1 トランザクション。2 枚目のディスクの INSERT が失敗（存在しない track_id で FK 違反）
/// したら 1 枚目の行も残らない
#[tokio::test]
async fn recording_is_atomic_across_discs() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    use spindle::db::verify::{
        record_album, DiscRecord, DiscResult, Method, MethodRecord, TrackRecord, TrackState,
    };
    let lib = Lib::new().await;
    let r = reference(15, &[75]);
    lib.add_flac("A/Tx/01.flac", &r.tracks[0], "Tx", 1, 1);
    lib.scan().await;
    let album = lib.album_id("Tx");
    let (track_id, version): (i64, i64) = lib
        .conn()
        .query_row(
            "SELECT id, audio_version FROM tracks WHERE rel_path = 'A/Tx/01.flac'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let ok_disc = DiscRecord {
        disc_no: 1,
        methods: vec![MethodRecord {
            method: Method::Ctdb,
            result: DiscResult::Verified,
            detected_offset: Some(0),
            confidence: Some(1),
            tracks: vec![TrackRecord {
                track_id,
                crc_v1: None,
                crc_v2: None,
                ctdb_crc: Some(1),
                matched: true,
            }],
        }],
        states: vec![(track_id, TrackState::VerifiedCtdb)],
    };
    let mut bad_disc = ok_disc.clone();
    bad_disc.disc_no = 2;
    bad_disc.methods[0].tracks[0].track_id = 999_999;
    let discs = vec![ok_disc, bad_disc];
    let expected = vec![(track_id, version)];
    let res = lib
        .db
        .transaction(move |c| record_album(c, album, 1, &expected, &discs, None, 1))
        .await;
    assert!(res.is_err(), "FK 違反で失敗する: {res:?}");
    assert!(lib.verifications(album).is_empty());
    assert_eq!(lib.track_states(album), vec!["not_attempted"]);
}

/// verify.log の確定（rename）に失敗したら DB の記録も巻き戻り、ジョブは失敗する
#[tokio::test]
async fn log_rename_failure_rolls_back_the_records() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new().await;
    let r = reference(16, &[75]);
    lib.add_flac("A/Lf/01.flac", &r.tracks[0], "Lf", 1, 1);
    lib.scan().await;
    lib.responses.lock().await.ctdb = Some(r.ctdb_xml(9));
    lib.start();
    let album = lib.album_id("Lf");
    // ログと同名の空でないディレクトリを置いて rename を失敗させる
    let log = lib.log_path(album);
    std::fs::create_dir_all(log.join("blocker")).unwrap();
    let job = match lib
        .jobs
        .enqueue(new_verify_job(album).max_attempts(1))
        .await
        .unwrap()
    {
        EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
    };
    assert_eq!(lib.wait_job(job).await, JobState::Failed);
    assert!(lib.verifications(album).is_empty());
    assert_eq!(lib.track_states(album), vec!["not_attempted"]);
    assert!(!log.with_file_name(format!(".{album}.log.tmp")).is_file());
}

/// commit の後に落ちて同じジョブが再実行されても、履歴もログも初回のまま
/// （ファイルが差し替わり DB の応答も変わっていても、読み直して上書きしない）
#[tokio::test]
async fn rerun_of_the_same_job_after_commit_keeps_history_and_log() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let lib = Lib::new().await;
    let r = reference(17, &[75]);
    let p = lib.add_flac("A/Rr/01.flac", &r.tracks[0], "Rr", 1, 1);
    lib.scan().await;
    lib.responses.lock().await.ctdb = Some(r.ctdb_xml(9));
    lib.start();
    let album = lib.album_id("Rr");
    let job = lib.enqueue(album).await;
    assert_eq!(lib.wait_job(job).await, JobState::Done);
    let first = lib.verifications(album);
    assert_eq!(first.len(), 2);
    let first_log = read_log(&lib.log_path(album));
    let hits_before = lib.responses.lock().await.ctdb_hits;

    // 別の PCM に差し替えてスキャン（audio_version が進む）し、DB の応答も別のディスクに
    let other = reference(77, &[75]);
    let wav = p.with_extension("new.wav");
    let as_i32: Vec<i32> = other.tracks[0].iter().map(|&s| i32::from(s)).collect();
    common::write_wav(&wav, &as_i32, 16);
    let tmp = p.with_extension("new.flac");
    common::encode(&wav, &tmp, common::encode_args("flac")).unwrap();
    std::fs::rename(&tmp, &p).unwrap();
    common::set_basic_tags(&p, "T1", "Artist", "Rr", "Artist", 1, 1);
    lib.scan().await;
    lib.responses.lock().await.ctdb = Some(other.ctdb_xml(1));

    // commit 直後に落ちた状況: ジョブを queued に戻して同じ id で再実行させる
    lib.conn()
        .execute(
            "UPDATE jobs SET state = 'queued', finished_at = NULL WHERE id = ?1",
            [job],
        )
        .unwrap();
    lib.jobs.notify_enqueued(&[job]).await;
    assert_eq!(lib.wait_job(job).await, JobState::Done);
    assert_eq!(lib.verifications(album), first, "履歴は初回のまま");
    assert_eq!(
        read_log(&lib.log_path(album)),
        first_log,
        "ログも初回のまま"
    );
    assert_eq!(
        lib.responses.lock().await.ctdb_hits,
        hits_before,
        "読み直しも照会もしない"
    );
}

/// DB 層: 同じ job_id で 2 回記録しても 2 回目は何も書かない
#[tokio::test]
async fn record_album_is_idempotent_per_job() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    use spindle::db::verify::{
        record_album, DiscRecord, DiscResult, Method, MethodRecord, RecordOutcome, TrackRecord,
        TrackState,
    };
    let lib = Lib::new().await;
    let r = reference(18, &[75]);
    lib.add_flac("A/Id/01.flac", &r.tracks[0], "Id", 1, 1);
    lib.scan().await;
    let album = lib.album_id("Id");
    let (track_id, version): (i64, i64) = lib
        .conn()
        .query_row(
            "SELECT id, audio_version FROM tracks WHERE rel_path = 'A/Id/01.flac'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let disc = DiscRecord {
        disc_no: 1,
        methods: vec![MethodRecord {
            method: Method::AccurateRip,
            result: DiscResult::NotFound,
            detected_offset: Some(0),
            confidence: Some(0),
            tracks: vec![TrackRecord {
                track_id,
                crc_v1: Some(1),
                crc_v2: Some(2),
                ctdb_crc: None,
                matched: false,
            }],
        }],
        states: vec![(track_id, TrackState::Mismatch)],
    };
    // ジョブ行が要る（FK）
    let job = lib.enqueue(album).await;
    for expect_first in [true, false] {
        let (d, e) = (vec![disc.clone()], vec![(track_id, version)]);
        let out = lib
            .db
            .transaction(move |c| record_album(c, album, job, &e, &d, None, 1))
            .await
            .unwrap();
        if expect_first {
            assert!(matches!(out, RecordOutcome::Recorded(ref ids) if ids.len() == 1));
        } else {
            assert_eq!(out, RecordOutcome::AlreadyRecorded);
        }
    }
    assert_eq!(lib.verifications(album).len(), 1);
}

//! 吸い出しの本体（`cd::rip::rip_disc`。SPEC §7.2、D-66 / D-83、P2-5）。ドライブ・読み取り・照会を
//! フェイクにして、オフセットの検出と学習、CTDB の修復、吸い直し、別の盤の拒否を確かめる。
//! Inbox への配置まで通すので `flac` が無ければ skip

#![cfg(target_os = "linux")]

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use spindle::cd::accuraterip::ArDiscEntry;
use spindle::cd::crctable::{CrcSampler, CrcTable};
use spindle::cd::ctdb::CtdbEntry;
use spindle::cd::device::{DiscIds, Drive, DriveError, DriveState};
use spindle::cd::metadata::{DiscMetadata, DiscTrackMetadata, MetadataSource};
use spindle::cd::place::PlaceEnv;
use spindle::cd::repair::{DbSyndromes, SyndromeSampler};
use spindle::cd::rip::{
    rip_disc, DiscReader, RipEnv, RipError, RipJobError, RipLookup, RipPhase, RipProgress,
};
use spindle::cd::riplog::{OffsetSource, TrackRead};
use spindle::cd::toc::Toc;
use spindle::cd::verify::Outcome;
use spindle::cd::LookupError;
use spindle::config::DriveOffset;
use spindle::db::Db;
use spindle::fsroot::RootDir;
use spindle::import::sidecar::Sidecar;
use spindle::jobs::{BoxFuture, Jobs};

const SECTOR: u64 = 588;
const DRIVE: &str = "TEST DRIVE-1";

fn toc() -> Toc {
    Toc::from_audio_sample_counts([750 * SECTOR, 600 * SECTOR, 900 * SECTOR]).unwrap()
}

/// 決定的な擬似乱数のインターリーブ i16（正しいデータ）
fn truth() -> Vec<i16> {
    let frames = toc().track_layout().unwrap().total_samples();
    let mut x: u32 = 0x9e37_79b9;
    (0..frames * 2)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 3) as i16
        })
        .collect()
}

fn table(pcm: &[i16]) -> CrcTable {
    let mut s = CrcSampler::new(&toc().track_layout().unwrap());
    s.push(pcm).unwrap();
    s.finish().unwrap()
}

/// ドライブのずれで `d` サンプル遅れて（負なら早く）読めたデータ
fn delayed(pcm: &[i16], d: i32) -> Vec<i16> {
    let k = d.unsigned_abs() as usize * 2;
    if d >= 0 {
        std::iter::repeat_n(0, k)
            .chain(pcm[..pcm.len() - k].iter().copied())
            .collect()
    } else {
        pcm[k..]
            .iter()
            .copied()
            .chain(std::iter::repeat_n(0, k))
            .collect()
    }
}

/// `sector` 番目のセクタを丸ごと壊す（CTDB の修復は列あたり 1 語で直る）
fn scratched(pcm: &[i16], sector: usize) -> Vec<i16> {
    let mut out = pcm.to_vec();
    for w in &mut out[sector * 1176..(sector + 1) * 1176] {
        *w = w.wrapping_add(0x1357);
    }
    out
}

fn bytes(pcm: &[i16]) -> Vec<u8> {
    pcm.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// 正しいデータの CTDB エントリ（パリティあり）
fn ctdb_entry(pcm: &[i16]) -> CtdbEntry {
    let t = table(pcm);
    CtdbEntry {
        id: 1,
        confidence: 7,
        crc32: t.ctdb_disc(0).unwrap(),
        track_crcs: (0..3).map(|i| t.ctdb_track(i, 0).unwrap()).collect(),
        toc: toc().ctdb_toc(),
        npar: 8,
        stride: 5880,
        has_parity: Some("http://example.invalid/parity".into()),
        syndrome: None,
        parity: None,
    }
}

fn db_syndromes(pcm: &[i16]) -> DbSyndromes {
    let mut s = SyndromeSampler::new(pcm.len() as u64 / 2, 8).unwrap();
    s.push(pcm).unwrap();
    DbSyndromes::from_table(&s.finish().unwrap())
}

// ---------------------------------------------------------------- フェイク

struct FakeDrive {
    toc: Toc,
}

impl Drive for FakeDrive {
    fn status(&self) -> Result<DriveState, DriveError> {
        Ok(DriveState::DiscOk)
    }
    fn read_toc(&self) -> Result<Toc, DriveError> {
        Ok(self.toc.clone())
    }
    fn read_ids(&self, _toc: &Toc) -> Result<DiscIds, DriveError> {
        Ok(DiscIds {
            isrcs: Vec::new(),
            mcn: None,
        })
    }
    fn eject(&self) -> Result<(), DriveError> {
        Ok(())
    }
    fn model(&self) -> Result<Option<String>, DriveError> {
        Ok(Some(DRIVE.into()))
    }
}

/// 呼ばれるたびに `reads` の次の PCM を書く（尽きたら最後のもの）
struct FakeReader {
    reads: Vec<Vec<u8>>,
    calls: AtomicUsize,
}

impl DiscReader for FakeReader {
    fn read<'a>(
        &'a self,
        toc: &'a Toc,
        out: &'a Path,
        progress: &'a (dyn Fn(u64, u64) + Send + Sync),
        _token: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<TrackRead>, RipError>> {
        Box::pin(async move {
            let i = self.calls.fetch_add(1, Ordering::SeqCst);
            let data = &self.reads[i.min(self.reads.len() - 1)];
            std::fs::write(out, data)?;
            let total = toc.track_layout().unwrap().total_samples() / SECTOR;
            progress(total / 2, total);
            progress(total, total);
            Ok(vec![TrackRead::default(); 3])
        })
    }
}

struct FakeLookup {
    ctdb: Option<Vec<CtdbEntry>>,
    ar: Option<Vec<ArDiscEntry>>,
    syndromes: Option<DbSyndromes>,
    /// パリティを渡すときに取り消す（修復の途中で取り消された状況）
    cancel_on_syndromes: Option<CancellationToken>,
    /// AccurateRip のドライブ表の値（無ければ表なし）
    table: Option<i32>,
}

impl RipLookup for FakeLookup {
    fn ctdb<'a>(&'a self, _toc: &'a Toc) -> BoxFuture<'a, Result<Vec<CtdbEntry>, LookupError>> {
        Box::pin(async move { self.ctdb.clone().ok_or(LookupError::Status(503)) })
    }
    fn accuraterip<'a>(
        &'a self,
        _toc: &'a Toc,
    ) -> BoxFuture<'a, Result<Vec<ArDiscEntry>, LookupError>> {
        Box::pin(async move { self.ar.clone().ok_or(LookupError::Status(503)) })
    }
    fn syndromes<'a>(
        &'a self,
        _entry: &'a CtdbEntry,
        _npar: usize,
    ) -> BoxFuture<'a, Result<DbSyndromes, LookupError>> {
        Box::pin(async move {
            if let Some(t) = &self.cancel_on_syndromes {
                t.cancel();
            }
            self.syndromes.clone().ok_or(LookupError::NoParity)
        })
    }
    fn table_offset<'a>(
        &'a self,
        model: &'a str,
    ) -> BoxFuture<'a, Option<spindle::cd::driveoffsets::DriveEntry>> {
        Box::pin(async move {
            self.table
                .map(|offset| spindle::cd::driveoffsets::DriveEntry {
                    name: model.to_owned(),
                    offset,
                    submissions: 1,
                })
        })
    }
}

// ---------------------------------------------------------------- 環境

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    db: Arc<Db>,
    jobs: Arc<Jobs>,
    inbox: Arc<RootDir>,
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["Inbox", "tmp"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let inbox = Arc::new(RootDir::open(&dir.path().join("Inbox")).unwrap());
        let jobs = Jobs::new(db.clone());
        Self {
            dir,
            db_path,
            db,
            jobs,
            inbox,
        }
    }

    fn env(&self, reads: Vec<Vec<u8>>, lookup: FakeLookup, offset: DriveOffset) -> RipEnv {
        RipEnv {
            reader: Arc::new(FakeReader {
                reads,
                calls: AtomicUsize::new(0),
            }),
            lookup: Arc::new(lookup),
            drive: Arc::new(FakeDrive { toc: toc() }),
            db: self.db.clone(),
            place: PlaceEnv {
                inbox: self.inbox.clone(),
                jobs: self.jobs.clone(),
                flac: PathBuf::from("flac"),
                compression: 5,
                tmp_dir: self.dir.path().join("tmp"),
                before_publish: None,
            },
            drive_offset: offset,
            retries: 2,
            device: "/dev/sr0".into(),
        }
    }

    fn learned(&self) -> Option<i32> {
        let c = rusqlite::Connection::open(&self.db_path).unwrap();
        spindle::db::drive_offsets::get(&c, DRIVE)
            .unwrap()
            .map(|l| l.offset)
    }

    fn sidecar(&self, dir: &spindle::domain::relpath::RelPath) -> Sidecar {
        Sidecar::read(&self.inbox, dir).unwrap().unwrap()
    }

    /// tmp に残った PCM（吸い出しの作業ファイルは終わったら消える）
    fn leftover_pcm(&self) -> Vec<String> {
        std::fs::read_dir(self.dir.path().join("tmp"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".pcm"))
            .collect()
    }
}

fn meta() -> DiscMetadata {
    DiscMetadata {
        source: MetadataSource::Manual,
        release_id: None,
        release_group_id: None,
        album: String::new(),
        album_artist: String::new(),
        date: None,
        label: None,
        catalog_number: None,
        barcode: None,
        disc_no: 1,
        disc_count: 1,
        category: None,
        tracks: (1..=3u8)
            .map(|number| DiscTrackMetadata {
                number,
                title: String::new(),
                artist: String::new(),
                mb: None,
            })
            .collect(),
    }
}

/// ドライブが読んだ ISRC / MCN（サイドカーにそのまま残る。P4-21）
fn ids() -> DiscIds {
    DiscIds {
        isrcs: vec![
            Some("JPAA00000001".into()),
            None,
            Some("JPAA00000003".into()),
        ],
        mcn: Some("4988000000000".into()),
    }
}

type Seen = Arc<Mutex<Vec<RipProgress>>>;

async fn run(env: &RipEnv) -> (Result<spindle::cd::place::Placed, RipJobError>, Seen) {
    run_with(env, &CancellationToken::new()).await
}

async fn run_with(
    env: &RipEnv,
    token: &CancellationToken,
) -> (Result<spindle::cd::place::Placed, RipJobError>, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let s = Arc::clone(&seen);
    let r = rip_disc(
        env,
        &toc(),
        &meta(),
        &ids(),
        Arc::new(move |p| s.lock().unwrap().push(p)),
        token,
    )
    .await;
    (r, seen)
}

macro_rules! require_flac {
    () => {
        if std::process::Command::new("flac")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("flac が無いので skip");
            return;
        }
    };
}

// ---------------------------------------------------------------- テスト

/// 学習前のドライブ: 0 で吸い、照合で見つかったずれを PCM に当てて配置し、型番ごとに覚える
#[tokio::test]
async fn unknown_offset_is_detected_applied_and_learned() {
    require_flac!();
    let lib = Lib::new();
    let t = truth();
    let env = lib.env(
        vec![bytes(&delayed(&t, 667))],
        FakeLookup {
            ctdb: Some(vec![ctdb_entry(&t)]),
            ar: Some(Vec::new()),
            syndromes: None,
            cancel_on_syndromes: None,
            table: None,
        },
        DriveOffset::Auto,
    );
    let (placed, seen) = run(&env).await;
    let placed = placed.unwrap();
    let rip = lib.sidecar(&placed.rel_dir).rip.unwrap();
    assert_eq!(
        (rip.isrcs.clone(), rip.mcn.clone()),
        (ids().isrcs, ids().mcn)
    );
    assert_eq!(rip.report.read_offset, 667);
    assert_eq!(rip.report.offset_source, OffsetSource::Detected);
    assert_eq!(rip.report.attempts, 1);
    let ctdb = rip.report.ctdb.unwrap();
    assert_eq!((ctdb.outcome, ctdb.offset), (Outcome::Verified, 0));
    // 当てた後の CRC は正しいデータの CRC（オフセット 0）
    let want = table(&t);
    for (i, c) in rip.report.crcs.iter().enumerate() {
        assert_eq!(c.ctdb, want.ctdb_track(i, 0).unwrap());
    }
    assert_eq!(rip.report.drive.as_deref(), Some(DRIVE));
    assert_eq!(lib.learned(), Some(667));
    // 進捗: 読み取り → 照合 → エンコード（トラックごと）→ 配置
    let phases: Vec<RipPhase> = seen.lock().unwrap().iter().map(|p| p.phase).collect();
    assert_eq!(phases.first(), Some(&RipPhase::Read));
    assert!(phases.contains(&RipPhase::Verify));
    let encoded: Vec<Option<u8>> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p.phase == RipPhase::Encode && p.done > 0)
        .map(|p| p.track_no)
        .collect();
    assert_eq!(encoded, [Some(1), Some(2), Some(3)]);
    assert_eq!(phases.last(), Some(&RipPhase::Place));
    assert!(lib.leftover_pcm().is_empty(), "{:?}", lib.leftover_pcm());
}

/// 覚えた値があればそれで当て、照合でずれが 0 なら `Learned` のまま
#[tokio::test]
async fn learned_offset_is_used() {
    require_flac!();
    let lib = Lib::new();
    {
        let c = rusqlite::Connection::open(&lib.db_path).unwrap();
        spindle::db::drive_offsets::set(
            &c,
            DRIVE,
            667,
            spindle::db::drive_offsets::OffsetMethod::Ctdb,
            3,
            1,
        )
        .unwrap();
    }
    let t = truth();
    let env = lib.env(
        vec![bytes(&delayed(&t, 667))],
        FakeLookup {
            ctdb: Some(vec![ctdb_entry(&t)]),
            ar: Some(Vec::new()),
            syndromes: None,
            cancel_on_syndromes: None,
            table: None,
        },
        DriveOffset::Auto,
    );
    let placed = run(&env).await.0.unwrap();
    let rip = lib.sidecar(&placed.rel_dir).rip.unwrap();
    assert_eq!(
        (rip.report.read_offset, rip.report.offset_source),
        (667, OffsetSource::Learned)
    );
    assert_eq!(rip.report.ctdb.unwrap().outcome, Outcome::Verified);
}

/// 手動指定のオフセットは当てるが覚えない
#[tokio::test]
async fn manual_offset_is_applied_and_not_learned() {
    require_flac!();
    let lib = Lib::new();
    let t = truth();
    let env = lib.env(
        vec![bytes(&delayed(&t, 667))],
        FakeLookup {
            ctdb: Some(vec![ctdb_entry(&t)]),
            ar: Some(Vec::new()),
            syndromes: None,
            cancel_on_syndromes: None,
            table: None,
        },
        DriveOffset::Samples(667),
    );
    let placed = run(&env).await.0.unwrap();
    let rip = lib.sidecar(&placed.rel_dir).rip.unwrap();
    assert_eq!(
        (rip.report.read_offset, rip.report.offset_source),
        (667, OffsetSource::Manual)
    );
    assert_eq!(rip.report.ctdb.unwrap().outcome, Outcome::Verified);
    assert_eq!(lib.learned(), None);
}

/// 1 セクタの傷は CTDB のパリティで直す（吸い直さない）
#[tokio::test]
async fn scratched_sector_is_repaired_with_ctdb_parity() {
    require_flac!();
    let lib = Lib::new();
    let t = truth();
    let env = lib.env(
        vec![bytes(&scratched(&t, 1000))],
        FakeLookup {
            ctdb: Some(vec![ctdb_entry(&t)]),
            ar: Some(Vec::new()),
            syndromes: Some(db_syndromes(&t)),
            cancel_on_syndromes: None,
            table: None,
        },
        DriveOffset::Samples(0),
    );
    let (placed, seen) = run(&env).await;
    let rip = lib.sidecar(&placed.unwrap().rel_dir).rip.unwrap();
    assert_eq!(rip.report.attempts, 1);
    assert_eq!(rip.report.repaired_words, Some(1176));
    assert_eq!(rip.report.ctdb.unwrap().outcome, Outcome::Verified);
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .any(|p| p.phase == RipPhase::Repair));
}

/// パリティで直せなければ吸い直し、通った回を使う
#[tokio::test]
async fn unrepairable_read_is_ripped_again() {
    require_flac!();
    let lib = Lib::new();
    let t = truth();
    let env = lib.env(
        vec![bytes(&scratched(&t, 1000)), bytes(&t)],
        FakeLookup {
            ctdb: Some(vec![ctdb_entry(&t)]),
            ar: Some(Vec::new()),
            syndromes: None, // パリティを取れない
            cancel_on_syndromes: None,
            table: None,
        },
        DriveOffset::Samples(0),
    );
    let (placed, seen) = run(&env).await;
    let rip = lib.sidecar(&placed.unwrap().rel_dir).rip.unwrap();
    assert_eq!(rip.report.attempts, 2);
    assert_eq!(rip.report.repaired_words, None);
    assert_eq!(rip.report.ctdb.unwrap().outcome, Outcome::Verified);
    let attempts: Vec<u32> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p.phase == RipPhase::Read)
        .map(|p| p.attempt)
        .collect();
    assert!(
        attempts.contains(&1) && attempts.contains(&2),
        "{attempts:?}"
    );
}

/// 何回吸っても通らなければ mismatch のまま取り込む（SPEC §7.2。1 + retry_on_mismatch 回）
#[tokio::test]
async fn persistent_mismatch_is_placed_after_all_attempts() {
    require_flac!();
    let lib = Lib::new();
    let t = truth();
    let bad = bytes(&scratched(&t, 1000));
    let env = lib.env(
        vec![bad],
        FakeLookup {
            ctdb: Some(vec![ctdb_entry(&t)]),
            ar: Some(Vec::new()),
            syndromes: None,
            cancel_on_syndromes: None,
            table: None,
        },
        DriveOffset::Samples(0),
    );
    let placed = run(&env).await.0.unwrap();
    let rip = lib.sidecar(&placed.rel_dir).rip.unwrap();
    assert_eq!(rip.report.attempts, 3);
    let ctdb = rip.report.ctdb.unwrap();
    assert_eq!(ctdb.outcome, Outcome::Mismatch);
    // 傷の無いトラックは一致している（1000 セクタ目はトラック 2）
    assert_eq!(
        ctdb.tracks.iter().map(|t| t.matched).collect::<Vec<_>>(),
        [true, false, true]
    );
    assert_eq!(lib.learned(), None);
}

/// 照会できなくても吸い出しは続け、その手法は「照会しなかった」
#[tokio::test]
async fn lookup_failure_still_places_without_verification() {
    require_flac!();
    let lib = Lib::new();
    let t = truth();
    let env = lib.env(
        vec![bytes(&t)],
        FakeLookup {
            ctdb: None,
            ar: None,
            syndromes: None,
            cancel_on_syndromes: None,
            table: None,
        },
        DriveOffset::Auto,
    );
    let placed = run(&env).await.0.unwrap();
    let rip = lib.sidecar(&placed.rel_dir).rip.unwrap();
    assert!(rip.report.ctdb.is_none() && rip.report.accuraterip.is_none());
    assert_eq!(rip.report.offset_source, OffsetSource::Unknown);
    // 照合が無いので吸い直さない
    assert_eq!(rip.report.attempts, 1);
}

/// ドライブの盤が取り込みを始めた盤と違えば吸わない
#[tokio::test]
async fn different_disc_in_the_drive_is_rejected() {
    let lib = Lib::new();
    let mut env = lib.env(
        vec![Vec::new()],
        FakeLookup {
            ctdb: Some(Vec::new()),
            ar: Some(Vec::new()),
            syndromes: None,
            cancel_on_syndromes: None,
            table: None,
        },
        DriveOffset::Auto,
    );
    env.drive = Arc::new(FakeDrive {
        toc: Toc::from_audio_sample_counts([750 * SECTOR]).unwrap(),
    });
    assert!(matches!(
        run(&env).await.0,
        Err(RipJobError::DiscChanged { .. })
    ));
}

/// 3 トラックそれぞれに傷（別の列に 1 語ずつ）がある
fn scratched_everywhere(pcm: &[i16]) -> Vec<i16> {
    scratched(&scratched(&scratched(pcm, 101), 902), 1703)
}

/// 未学習のドライブで、傷のために CRC ではどのオフセットでも一致しない盤: パリティがずれを見つけて
/// 直し、そのずれを当ててからオフセット 0 で照合し、覚える（codex 指摘）
#[tokio::test]
async fn parity_repair_finds_and_applies_the_offset_and_learns_it() {
    require_flac!();
    let lib = Lib::new();
    let t = truth();
    let ours = delayed(&scratched_everywhere(&t), 667);
    let env = lib.env(
        vec![bytes(&ours)],
        FakeLookup {
            ctdb: Some(vec![ctdb_entry(&t)]),
            ar: Some(Vec::new()),
            syndromes: Some(db_syndromes(&t)),
            cancel_on_syndromes: None,
            table: None,
        },
        DriveOffset::Auto,
    );
    let placed = run(&env).await.0.unwrap();
    let rip = lib.sidecar(&placed.rel_dir).rip.unwrap();
    assert_eq!(rip.report.attempts, 1);
    assert_eq!(rip.report.repaired_words, Some(3 * 1176));
    assert_eq!(
        (rip.report.read_offset, rip.report.offset_source),
        (667, OffsetSource::Detected)
    );
    let ctdb = rip.report.ctdb.unwrap();
    assert_eq!((ctdb.outcome, ctdb.offset), (Outcome::Verified, 0));
    let want = table(&t);
    for (i, c) in rip.report.crcs.iter().enumerate() {
        assert_eq!(c.ctdb, want.ctdb_track(i, 0).unwrap(), "track {i}");
    }
    assert_eq!(lib.learned(), Some(667));
}

/// 修復の途中で取り消されたら、そこで止まる（数秒かかる走査を最後まで回さない）
#[tokio::test]
async fn cancel_during_repair_stops() {
    let lib = Lib::new();
    let t = truth();
    let token = CancellationToken::new();
    let env = lib.env(
        vec![bytes(&scratched(&t, 1000))],
        FakeLookup {
            ctdb: Some(vec![ctdb_entry(&t)]),
            ar: Some(Vec::new()),
            syndromes: Some(db_syndromes(&t)),
            cancel_on_syndromes: Some(token.clone()),
            table: None,
        },
        DriveOffset::Samples(0),
    );
    let (r, seen) = run_with(&env, &token).await;
    assert!(matches!(r, Err(RipJobError::Cancelled)), "{r:?}");
    // 修復の走査は進捗を出す前に止まり、エンコード・配置には進まない
    let phases: Vec<RipPhase> = seen.lock().unwrap().iter().map(|p| p.phase).collect();
    assert!(!phases.contains(&RipPhase::Encode), "{phases:?}");
    assert!(lib.leftover_pcm().is_empty(), "{:?}", lib.leftover_pcm());
}

/// CTDB は別のオフセット（+100）で 1 曲だけ一致、AccurateRip は +667 で全曲一致: 全曲一致の方を当て、
/// 通った手法（AccurateRip）の値として覚える（codex 指摘）
#[tokio::test]
async fn full_match_wins_over_partial_match_at_another_offset() {
    use spindle::cd::accuraterip::ArTrackEntry;
    require_flac!();
    let lib = Lib::new();
    let t = truth();
    let ours = delayed(&t, 667);
    // ours を 100 ずらすと 1 曲目が合う CTDB エントリ（2・3 曲目とディスク CRC は別物）
    let mut partial = ctdb_entry(&delayed(&t, 567));
    partial.crc32 ^= 1;
    partial.track_crcs[1] ^= 1;
    partial.track_crcs[2] ^= 1;
    let want = table(&t);
    let ar = ArDiscEntry {
        id: toc().accuraterip_id(),
        tracks: (0..3)
            .map(|i| ArTrackEntry {
                confidence: 5,
                crc: want.ar_v1(i, 0).unwrap(),
                crc450: 0,
            })
            .collect(),
    };
    let env = lib.env(
        vec![bytes(&ours)],
        FakeLookup {
            ctdb: Some(vec![partial]),
            ar: Some(vec![ar]),
            syndromes: None,
            cancel_on_syndromes: None,
            table: None,
        },
        DriveOffset::Auto,
    );
    let placed = run(&env).await.0.unwrap();
    let rip = lib.sidecar(&placed.rel_dir).rip.unwrap();
    assert_eq!(
        (
            rip.report.read_offset,
            rip.report.offset_source,
            rip.report.attempts
        ),
        (667, OffsetSource::Detected, 1)
    );
    let ar = rip.report.accuraterip.unwrap();
    assert_eq!((ar.outcome, ar.offset), (Outcome::Verified, 0));
    assert_eq!(rip.report.ctdb.unwrap().outcome, Outcome::Mismatch);
    let c = rusqlite::Connection::open(&lib.db_path).unwrap();
    let learned = spindle::db::drive_offsets::get(&c, DRIVE).unwrap().unwrap();
    assert_eq!(
        (learned.offset, learned.method.as_str()),
        (667, "accuraterip")
    );
}

/// 学習前でも AccurateRip のドライブ表の値で吸い、照合が通れば覚える（D-83 追記）
#[tokio::test]
async fn table_offset_is_used_before_learning() {
    require_flac!();
    let lib = Lib::new();
    let t = truth();
    let env = lib.env(
        vec![bytes(&delayed(&t, 667))],
        FakeLookup {
            ctdb: Some(vec![ctdb_entry(&t)]),
            ar: Some(Vec::new()),
            syndromes: None,
            cancel_on_syndromes: None,
            table: Some(667),
        },
        DriveOffset::Auto,
    );
    let placed = run(&env).await.0.unwrap();
    let rip = lib.sidecar(&placed.rel_dir).rip.unwrap();
    assert_eq!(
        (rip.report.read_offset, rip.report.offset_source),
        (667, OffsetSource::Table)
    );
    let ctdb = rip.report.ctdb.unwrap();
    assert_eq!((ctdb.outcome, ctdb.offset), (Outcome::Verified, 0));
    assert_eq!(lib.learned(), Some(667));
}

/// 学習済みの値が範囲外（壊れた DB）なら、表の値で吸う（codex 指摘）
#[tokio::test]
async fn out_of_range_learned_offset_falls_back_to_the_table() {
    require_flac!();
    let lib = Lib::new();
    {
        let c = rusqlite::Connection::open(&lib.db_path).unwrap();
        spindle::db::drive_offsets::set(
            &c,
            DRIVE,
            5000,
            spindle::db::drive_offsets::OffsetMethod::Ctdb,
            1,
            1,
        )
        .unwrap();
    }
    let t = truth();
    let env = lib.env(
        vec![bytes(&delayed(&t, 667))],
        FakeLookup {
            ctdb: Some(vec![ctdb_entry(&t)]),
            ar: Some(Vec::new()),
            syndromes: None,
            cancel_on_syndromes: None,
            table: Some(667),
        },
        DriveOffset::Auto,
    );
    let placed = run(&env).await.0.unwrap();
    let rip = lib.sidecar(&placed.rel_dir).rip.unwrap();
    assert_eq!(
        (rip.report.read_offset, rip.report.offset_source),
        (667, OffsetSource::Table)
    );
    // 照合が通ったので正しい値で上書きされる
    assert_eq!(lib.learned(), Some(667));
}

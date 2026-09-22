//! ドライブ制御（SPEC §7.2「ディスク検出」「TOC 取得」、P2-1 / P2-2）。
//! ioctl の生の TOC エントリから `Toc` を組み立てる純粋な部分と、ポーラの遷移（DiscOk になった
//! ときだけ TOC を読み、抜かれたら捨てる）をフェイクのドライブで確かめる。実ドライブは `#[ignore]`

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use spindle::cd::device::{
    toc_from_entries, Drive, DriveError, DriveMonitor, DriveState, LinuxDrive, TocEntry,
    CONTROL_DATA,
};
use spindle::cd::toc::{Toc, TocError};

const NEVERMIND_TOC: &str =
    "0:22593:41700:58133:71920:91198:104468:115188:131988:143758:159678:174415:191880";

// ------------------------------------------------------------ TOC の組み立て

#[test]
fn toc_from_entries_builds_audio_tracks_and_leadout() {
    let entries = [
        TocEntry {
            track: 1,
            control: 0x00,
            lba: 0,
        },
        TocEntry {
            track: 2,
            control: 0x00,
            lba: 20144,
        },
    ];
    let toc = toc_from_entries(&entries, 40290).unwrap();
    assert_eq!(toc.ctdb_toc(), "0:20144:40290");
    assert!(toc.tracks().iter().all(|t| t.is_audio));
}

#[test]
fn toc_from_entries_marks_data_track_by_control_bit() {
    // Enhanced CD: 音声 2 本の後にデータトラック（control の data ビット）
    let entries = [
        TocEntry {
            track: 1,
            control: 0x00,
            lba: 0,
        },
        TocEntry {
            track: 2,
            control: 0x02,
            lba: 20144,
        }, // コピー許可ビットは無関係
        TocEntry {
            track: 3,
            control: CONTROL_DATA,
            lba: 60000,
        },
    ];
    let toc = toc_from_entries(&entries, 90000).unwrap();
    assert_eq!(toc.ctdb_toc(), "0:20144:-60000:90000");
}

#[test]
fn toc_from_entries_rejects_invalid_layout() {
    // 検証は Toc::new と同じ（ここでは開始位置が昇順でない）
    let entries = [
        TocEntry {
            track: 1,
            control: 0,
            lba: 100,
        },
        TocEntry {
            track: 2,
            control: 0,
            lba: 50,
        },
    ];
    assert_eq!(
        toc_from_entries(&entries, 200).unwrap_err(),
        TocError::NotAscending { index: 1 }
    );
}

// ------------------------------------------------------------ ポーラの遷移

/// 台本どおりに応答するドライブ。`status` は呼ぶたびに先頭から消費し、尽きたら最後を繰り返す
/// 失敗は `(what, メッセージ)` で書き、返すときに `DriveError::Io` にする（io::Error は Clone でない）
type Fail = (&'static str, &'static str);

struct FakeDrive {
    states: Mutex<Vec<Result<DriveState, Fail>>>,
    tocs: Mutex<Vec<Result<Toc, Fail>>>,
    toc_reads: AtomicUsize,
    ejects: AtomicUsize,
}

impl FakeDrive {
    fn new(states: Vec<Result<DriveState, Fail>>, tocs: Vec<Result<Toc, Fail>>) -> Self {
        Self {
            states: Mutex::new(states),
            tocs: Mutex::new(tocs),
            toc_reads: AtomicUsize::new(0),
            ejects: AtomicUsize::new(0),
        }
    }
}

fn take<T: Clone>(v: &Mutex<Vec<Result<T, Fail>>>) -> Result<T, DriveError> {
    let mut v = v.lock().unwrap();
    let r = if v.len() > 1 {
        v.remove(0)
    } else {
        v[0].clone()
    };
    r.map_err(|(what, msg)| DriveError::Io {
        what,
        source: std::io::Error::other(msg),
    })
}

impl Drive for FakeDrive {
    fn status(&self) -> Result<DriveState, DriveError> {
        take(&self.states)
    }
    fn read_toc(&self) -> Result<Toc, DriveError> {
        self.toc_reads.fetch_add(1, Ordering::SeqCst);
        take(&self.tocs)
    }
    fn eject(&self) -> Result<(), DriveError> {
        self.ejects.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn nevermind() -> Toc {
    Toc::parse(NEVERMIND_TOC).unwrap()
}

#[test]
fn poll_reads_toc_once_while_disc_stays() {
    let drive = FakeDrive::new(vec![Ok(DriveState::DiscOk)], vec![Ok(nevermind())]);
    let mon = DriveMonitor::default();
    mon.poll(&drive, 1000);
    let s = mon.snapshot();
    assert_eq!(s.state, DriveState::DiscOk);
    assert_eq!(
        s.toc.as_ref().map(Toc::ctdb_toc).as_deref(),
        Some(NEVERMIND_TOC)
    );
    assert_eq!(s.error, None);
    assert_eq!(s.checked_at, 1000);
    mon.poll(&drive, 1002);
    mon.poll(&drive, 1004);
    assert_eq!(
        drive.toc_reads.load(Ordering::SeqCst),
        1,
        "同じディスクの間は読み直さない"
    );
    assert_eq!(mon.snapshot().checked_at, 1004);
}

#[test]
fn poll_drops_toc_when_disc_removed_and_rereads_next_disc() {
    let drive = FakeDrive::new(
        vec![
            Ok(DriveState::DiscOk),
            Ok(DriveState::TrayOpen),
            Ok(DriveState::NoDisc),
            Ok(DriveState::DiscOk),
        ],
        vec![Ok(nevermind())],
    );
    let mon = DriveMonitor::default();
    mon.poll(&drive, 1);
    assert!(mon.snapshot().toc.is_some());
    mon.poll(&drive, 2);
    let s = mon.snapshot();
    assert_eq!(s.state, DriveState::TrayOpen);
    assert!(s.toc.is_none(), "抜かれたら TOC は捨てる");
    mon.poll(&drive, 3);
    assert_eq!(mon.snapshot().state, DriveState::NoDisc);
    mon.poll(&drive, 4);
    assert_eq!(mon.snapshot().state, DriveState::DiscOk);
    assert!(mon.snapshot().toc.is_some());
    assert_eq!(drive.toc_reads.load(Ordering::SeqCst), 2);
}

#[test]
fn poll_keeps_error_and_retries_toc_next_round() {
    // 挿入直後は READ TOC が失敗することがある。DiscOk で TOC が無い間は毎回読み直す
    let drive = FakeDrive::new(
        vec![Ok(DriveState::DiscOk)],
        vec![Err(("READ TOC", "Unit not ready")), Ok(nevermind())],
    );
    let mon = DriveMonitor::default();
    mon.poll(&drive, 1);
    let s = mon.snapshot();
    assert_eq!(s.state, DriveState::DiscOk);
    assert!(s.toc.is_none());
    let err = s.error.expect("読めなかった理由を持つ");
    assert!(
        err.contains("READ TOC") && err.contains("Unit not ready"),
        "{err}"
    );
    mon.poll(&drive, 2);
    let s = mon.snapshot();
    assert!(s.toc.is_some());
    assert_eq!(s.error, None);
    assert_eq!(drive.toc_reads.load(Ordering::SeqCst), 2);
}

#[test]
fn poll_reports_no_drive_when_status_fails() {
    let drive = FakeDrive::new(
        vec![
            Err(("open", "No such file or directory")),
            Ok(DriveState::NoDisc),
        ],
        vec![Ok(nevermind())],
    );
    let mon = DriveMonitor::default();
    mon.poll(&drive, 1);
    let s = mon.snapshot();
    assert_eq!(s.state, DriveState::NoDrive);
    assert!(
        s.error.as_deref().unwrap_or("").contains("open"),
        "{:?}",
        s.error
    );
    // 復帰したら通常の状態に戻る
    mon.poll(&drive, 2);
    let s = mon.snapshot();
    assert_eq!(s.state, DriveState::NoDisc);
    assert_eq!(s.error, None);
}

#[test]
fn monitor_starts_as_unknown_until_first_poll() {
    let mon = DriveMonitor::default();
    let s = mon.snapshot();
    assert_eq!(s.state, DriveState::Unknown);
    assert_eq!(s.checked_at, 0);
}

#[test]
fn drive_state_serializes_snake_case() {
    assert_eq!(
        serde_json::to_string(&DriveState::DiscOk).unwrap(),
        "\"disc_ok\""
    );
    assert_eq!(
        serde_json::to_string(&DriveState::NoDrive).unwrap(),
        "\"no_drive\""
    );
}

// ------------------------------------------------------------ eject と定期 poll の直列化

/// `status()` の 1 回目で止まり、`release` されるまで返さないドライブ。eject すると以後 TrayOpen
struct StallingDrive {
    entered: Arc<(Mutex<bool>, Condvar)>,
    release: Arc<(Mutex<bool>, Condvar)>,
    ejected: Arc<AtomicBool>,
    stalled_once: AtomicBool,
    ejects: AtomicUsize,
}

fn wait_flag(pair: &(Mutex<bool>, Condvar)) {
    let (m, cv) = pair;
    let mut g = m.lock().unwrap();
    while !*g {
        g = cv.wait(g).unwrap();
    }
}

fn set_flag(pair: &(Mutex<bool>, Condvar)) {
    let (m, cv) = pair;
    *m.lock().unwrap() = true;
    cv.notify_all();
}

impl Drive for StallingDrive {
    fn status(&self) -> Result<DriveState, DriveError> {
        if !self.stalled_once.swap(true, Ordering::SeqCst) {
            set_flag(&self.entered);
            wait_flag(&self.release);
        }
        Ok(if self.ejected.load(Ordering::SeqCst) {
            DriveState::TrayOpen
        } else {
            DriveState::DiscOk
        })
    }
    fn read_toc(&self) -> Result<Toc, DriveError> {
        Ok(nevermind())
    }
    fn eject(&self) -> Result<(), DriveError> {
        self.ejects.fetch_add(1, Ordering::SeqCst);
        self.ejected.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// 定期 poll がドライブを見ている最中に eject が来ても、古い DiscOk / TOC が後から書き戻されない。
/// eject + 直後の poll は定期 poll と同じ排他の中で走る（codex 指摘: 後勝ちで抜いたディスクの
/// TOC が復活し、次のディスクでも読み直されない）
#[test]
fn eject_is_serialized_with_periodic_poll() {
    let drive = Arc::new(StallingDrive {
        entered: Arc::new((Mutex::new(false), Condvar::new())),
        release: Arc::new((Mutex::new(false), Condvar::new())),
        ejected: Arc::new(AtomicBool::new(false)),
        stalled_once: AtomicBool::new(false),
        ejects: AtomicUsize::new(0),
    });
    let mon = Arc::new(DriveMonitor::default());
    // 定期 poll: status() の中で止まる
    let periodic = {
        let (d, m) = (Arc::clone(&drive), Arc::clone(&mon));
        std::thread::spawn(move || m.poll(d.as_ref(), 1))
    };
    wait_flag(&drive.entered);
    // その間に eject が来る。排他で待たされ、ドライブにはまだ届かない
    let ejecting = {
        let (d, m) = (Arc::clone(&drive), Arc::clone(&mon));
        std::thread::spawn(move || m.eject_and_poll(d.as_ref(), 2))
    };
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(
        drive.ejects.load(Ordering::SeqCst),
        0,
        "定期 poll の途中で eject が割り込んではいけない"
    );
    set_flag(&drive.release);
    periodic.join().unwrap();
    ejecting.join().unwrap().unwrap();
    let s = mon.snapshot();
    assert_eq!(s.state, DriveState::TrayOpen);
    assert!(
        s.toc.is_none(),
        "抜いたディスクの TOC が残ってはいけない: {s:?}"
    );
    assert_eq!(s.checked_at, 2);
}

// ------------------------------------------------------------ 実ドライブ

/// ディスクを入れた実ドライブで状態と TOC を取る。`SPINDLE_TEST_CD_DEVICE`（既定 /dev/sr0）
#[test]
#[ignore]
fn real_drive_reports_disc_and_toc() {
    let dev = std::env::var("SPINDLE_TEST_CD_DEVICE").unwrap_or_else(|_| "/dev/sr0".into());
    let drive = LinuxDrive::new(dev.into());
    let state = drive.status().unwrap();
    eprintln!("state = {state:?}");
    if state == DriveState::DiscOk {
        let toc = drive.read_toc().unwrap();
        eprintln!("toc = {}", toc.ctdb_toc());
        eprintln!("discid = {}", toc.musicbrainz_disc_id());
        assert!(!toc.tracks().is_empty());
    }
}

/// 実ドライブでトレイを開ける（開いたままにする）。ディスクの有無は問わない
#[test]
#[ignore]
fn real_drive_eject_opens_tray() {
    let dev = std::env::var("SPINDLE_TEST_CD_DEVICE").unwrap_or_else(|_| "/dev/sr0".into());
    let drive = LinuxDrive::new(dev.into());
    drive.eject().unwrap();
    let mut state = drive.status().unwrap();
    for _ in 0..20 {
        if state == DriveState::TrayOpen {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        state = drive.status().unwrap();
    }
    assert_eq!(state, DriveState::TrayOpen);
}

/// デバイスが無いパスは開けない = `Io { what: "open" }`
#[test]
fn missing_device_fails_on_open() {
    let drive = LinuxDrive::new("/nonexistent/sr9".into());
    match drive.status() {
        Err(DriveError::Io { what, .. }) => assert_eq!(what, "open"),
        other => panic!("{other:?}"),
    }
}

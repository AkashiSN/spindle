//! ドライブ制御（SPEC §7.2「ディスク検出」「TOC 取得」、§15 `cd/device.rs`。P2-1 / P2-2）。
//!
//! udev はコンテナに届かないので、`CDROM_DRIVE_STATUS` ioctl を周期的に叩いてディスクの有無を
//! 見る（[`spawn_poller`]）。TOC は kernel の `CDROMREADTOCHDR` / `CDROMREADTOCENTRY` ioctl
//! （中身は READ TOC コマンド。`/dev/sg*` も外部プロセスも要らない）で LBA 形式で読み、
//! [`Toc`] へ組み立てる。DiscOk になってから TOC が取れるまでは毎周回読み直し、取れたら
//! ディスクが抜かれるまで読み直さない。eject は `CDROMEJECT`。
//!
//! ioctl の呼び出しだけが unsafe で、[`LinuxDrive`] に閉じ込める。ポーラの遷移
//! （[`DriveMonitor::poll`]）はドライブを [`Drive`] トレイトで受けるので、フェイクで検証できる

use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use super::toc::{Toc, TocError, TocTrack};

/// `CDROM_DRIVE_STATUS` の応答（linux/cdrom.h の `CDS_*`）
const CDS_NO_INFO: i32 = 0;
const CDS_NO_DISC: i32 = 1;
const CDS_TRAY_OPEN: i32 = 2;
const CDS_DRIVE_NOT_READY: i32 = 3;
const CDS_DISC_OK: i32 = 4;

/// ioctl 番号（linux/cdrom.h）
const CDROMREADTOCHDR: libc::c_ulong = 0x5305;
const CDROMREADTOCENTRY: libc::c_ulong = 0x5306;
const CDROMEJECT: libc::c_ulong = 0x5309;
const CDROM_DRIVE_STATUS: libc::c_ulong = 0x5326;
const CDROM_LOCKDOOR: libc::c_ulong = 0x5329;
/// `CDROM_DRIVE_STATUS` の引数。チェンジャでない普通のドライブは「現在のスロット」
const CDSL_CURRENT: libc::c_int = libc::c_int::MAX;
/// `cdte_format` = LBA
const CDROM_LBA: u8 = 0x01;
/// リードアウトのトラック番号
const CDROM_LEADOUT: u8 = 0xAA;
/// `cdte_ctrl` のデータトラックのビット（Q サブチャネルの control フィールド、bit 2）
pub const CONTROL_DATA: u8 = 0x04;
/// ディスク検出の間隔（SPEC §7.2）
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// `struct cdrom_tochdr`
#[repr(C)]
#[derive(Default)]
struct CdromTocHdr {
    trk0: u8,
    trk1: u8,
}

/// `struct cdrom_tocentry`。`cdte_adr:4` と `cdte_ctrl:4` のビットフィールドは 1 バイトに詰まり、
/// GCC（little endian）では adr が下位 4 bit、ctrl が上位 4 bit。`cdte_addr` は union で、
/// LBA 形式なら `int lba`。末尾の `cdte_datamode` の後は詰め物
#[repr(C)]
#[derive(Default)]
struct CdromTocEntry {
    track: u8,
    adr_ctrl: u8,
    format: u8,
    addr: i32,
    datamode: u8,
}

/// ドライブの状態。`Unknown` は起動直後で 1 度も見ていない、`NoDrive` はデバイスを開けない
/// （無い・権限が無い）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DriveState {
    Unknown,
    NoDrive,
    NoDisc,
    TrayOpen,
    NotReady,
    DiscOk,
}

#[derive(Debug, thiserror::Error)]
pub enum DriveError {
    #[error("{what}: {source}")]
    Io {
        /// どの操作か（`open` / `CDROM_DRIVE_STATUS` / `READ TOC` / `CDROMEJECT`）
        what: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("TOC が不正: {0}")]
    Toc(#[from] TocError),
    #[error("ドライブの状態が不明な値: {0}")]
    UnknownStatus(i32),
}

/// ドライブへの操作。実装は [`LinuxDrive`]（ioctl）。テストはフェイクで差し替える
pub trait Drive: Send + Sync {
    fn status(&self) -> Result<DriveState, DriveError>;
    fn read_toc(&self) -> Result<Toc, DriveError>;
    fn eject(&self) -> Result<(), DriveError>;
}

/// READ TOC の 1 エントリ（LBA 形式）。[`toc_from_entries`] の入力
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TocEntry {
    pub track: u8,
    /// Q サブチャネルの control（4 bit）。[`CONTROL_DATA`] が立っていればデータトラック
    pub control: u8,
    pub lba: u32,
}

/// READ TOC のエントリ（番号順）とリードアウトの LBA から [`Toc`] を作る。検証は [`Toc::new`]
pub fn toc_from_entries(entries: &[TocEntry], leadout_lba: u32) -> Result<Toc, TocError> {
    let tracks = entries
        .iter()
        .map(|e| TocTrack {
            number: e.track,
            start_lba: e.lba,
            is_audio: e.control & CONTROL_DATA == 0,
        })
        .collect();
    Toc::new(tracks, leadout_lba)
}

/// `/dev/sr0` などを ioctl で扱う。開くのは操作のたび（ディスクの出し入れで FD の状態が
/// 古くならないように）。`O_NONBLOCK` を付けないとディスク無しで open が待つ
pub struct LinuxDrive {
    path: PathBuf,
}

impl LinuxDrive {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn open(&self) -> Result<std::fs::File, DriveError> {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&self.path)
            .map_err(|source| DriveError::Io {
                what: "open",
                source,
            })
    }

    fn ioctl<T>(
        file: &std::fs::File,
        what: &'static str,
        request: libc::c_ulong,
        arg: *mut T,
    ) -> Result<i32, DriveError> {
        // SAFETY: request は linux/cdrom.h の番号で、arg はその要求が期待する型
        // （`CdromTocHdr` / `CdromTocEntry` / c_int）を指す有効なポインタか、値渡しの整数
        let r = unsafe { libc::ioctl(file.as_raw_fd(), request, arg) };
        if r < 0 {
            return Err(DriveError::Io {
                what,
                source: std::io::Error::last_os_error(),
            });
        }
        Ok(r)
    }

    fn toc_entry(file: &std::fs::File, track: u8) -> Result<CdromTocEntry, DriveError> {
        let mut e = CdromTocEntry {
            track,
            format: CDROM_LBA,
            ..Default::default()
        };
        Self::ioctl(file, "READ TOC", CDROMREADTOCENTRY, &mut e as *mut _)?;
        Ok(e)
    }
}

impl Drive for LinuxDrive {
    fn status(&self) -> Result<DriveState, DriveError> {
        let file = self.open()?;
        // 引数は値渡しの整数（ポインタではない）
        let r = Self::ioctl::<libc::c_void>(
            &file,
            "CDROM_DRIVE_STATUS",
            CDROM_DRIVE_STATUS,
            CDSL_CURRENT as usize as *mut libc::c_void,
        )?;
        Ok(match r {
            CDS_NO_INFO | CDS_DRIVE_NOT_READY => DriveState::NotReady,
            CDS_NO_DISC => DriveState::NoDisc,
            CDS_TRAY_OPEN => DriveState::TrayOpen,
            CDS_DISC_OK => DriveState::DiscOk,
            other => return Err(DriveError::UnknownStatus(other)),
        })
    }

    fn read_toc(&self) -> Result<Toc, DriveError> {
        let file = self.open()?;
        let mut hdr = CdromTocHdr::default();
        Self::ioctl(&file, "READ TOC", CDROMREADTOCHDR, &mut hdr as *mut _)?;
        let mut entries = Vec::with_capacity(usize::from(hdr.trk1.saturating_sub(hdr.trk0)) + 1);
        for track in hdr.trk0..=hdr.trk1 {
            let e = Self::toc_entry(&file, track)?;
            entries.push(TocEntry {
                track,
                control: e.adr_ctrl >> 4,
                lba: lba_from_kernel(e.addr)?,
            });
        }
        let leadout = Self::toc_entry(&file, CDROM_LEADOUT)?;
        Ok(toc_from_entries(&entries, lba_from_kernel(leadout.addr)?)?)
    }

    /// トレイを開ける。先に `CDROM_LOCKDOOR 0` で扉のロックを外す: kernel の CDROMEJECT も内部で
    /// 外すことになっているが、Pioneer BDR-209M（TrueNAS 25.10 の kernel 6.12）ではそれだけだと
    /// CHECK CONDITION（戻り値 2 = SCSI status。負でないので `ioctl` は成功扱い）でトレイが動かない。
    /// 明示的に外してから叩けば開く。CDROMEJECT の正常な戻り値は 0 なので、それ以外は失敗にする
    fn eject(&self) -> Result<(), DriveError> {
        let file = self.open()?;
        Self::ioctl::<libc::c_void>(
            &file,
            "CDROM_LOCKDOOR",
            CDROM_LOCKDOOR,
            std::ptr::null_mut(),
        )?;
        let r = Self::ioctl::<libc::c_void>(&file, "CDROMEJECT", CDROMEJECT, std::ptr::null_mut())?;
        if r != 0 {
            return Err(DriveError::Io {
                what: "CDROMEJECT",
                source: std::io::Error::other(format!("ドライブが拒んだ（SCSI status {r}）")),
            });
        }
        Ok(())
    }
}

/// kernel の LBA（MSF − 150。先頭トラックが 0）。負なら壊れた応答
fn lba_from_kernel(v: i32) -> Result<u32, DriveError> {
    u32::try_from(v).map_err(|_| DriveError::Toc(TocError::Unparsable(format!("LBA が負: {v}"))))
}

/// ドライブの状態のスナップショット（`GET /api/cd/status` が返す）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveStatus {
    pub state: DriveState,
    /// DiscOk で TOC を読めたら Some。抜かれたら None に戻る
    pub toc: Option<Toc>,
    /// 直近の失敗（開けない・状態を取れない・TOC を読めない）。成功したら None
    pub error: Option<String>,
    /// 最後にドライブを見た時刻（epoch 秒）。まだなら 0
    pub checked_at: i64,
}

impl Default for DriveStatus {
    fn default() -> Self {
        Self {
            state: DriveState::Unknown,
            toc: None,
            error: None,
            checked_at: 0,
        }
    }
}

/// ポーラが更新し、API が読む状態
#[derive(Debug, Default)]
pub struct DriveMonitor {
    status: RwLock<DriveStatus>,
}

impl DriveMonitor {
    pub fn snapshot(&self) -> DriveStatus {
        self.status
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 1 周回。状態を取り、DiscOk で TOC が無ければ読む（blocking。`spawn_blocking` の中で呼ぶ）
    pub fn poll(&self, drive: &dyn Drive, now: i64) {
        let prev = self.snapshot();
        let had_toc = prev.toc.is_some();
        let next = match drive.status() {
            Err(e) => DriveStatus {
                state: DriveState::NoDrive,
                toc: None,
                error: Some(e.to_string()),
                checked_at: now,
            },
            Ok(DriveState::DiscOk) => {
                let (toc, error) = match prev.toc {
                    Some(t) => (Some(t), None),
                    None => match drive.read_toc() {
                        Ok(t) => (Some(t), None),
                        Err(e) => (None, Some(e.to_string())),
                    },
                };
                DriveStatus {
                    state: DriveState::DiscOk,
                    toc,
                    error,
                    checked_at: now,
                }
            }
            Ok(state) => DriveStatus {
                state,
                toc: None,
                error: None,
                checked_at: now,
            },
        };
        if next.state != prev.state || next.toc.is_some() != had_toc {
            tracing::info!(
                state = ?next.state,
                toc = next.toc.as_ref().map(Toc::ctdb_toc),
                error = next.error.as_deref(),
                "CD ドライブの状態が変わった"
            );
        }
        *self.status.write().unwrap_or_else(|e| e.into_inner()) = next;
    }
}

/// `interval` ごとに [`DriveMonitor::poll`] を `spawn_blocking` で回す。`shutdown` で止まる
pub fn spawn_poller(
    drive: Arc<dyn Drive>,
    monitor: Arc<DriveMonitor>,
    interval: Duration,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let d = Arc::clone(&drive);
            let m = Arc::clone(&monitor);
            let now = crate::db::now_epoch();
            if let Err(e) = tokio::task::spawn_blocking(move || m.poll(d.as_ref(), now)).await {
                tracing::warn!(error = %e, "CD ドライブの監視タスクが異常終了");
            }
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(interval) => {}
            }
        }
    })
}

//! ドライブ制御（SPEC §7.2「ディスク検出」「TOC 取得」、§15 `cd/device.rs`。P2-1 / P2-2）。
//!
//! udev はコンテナに届かないので、`CDROM_DRIVE_STATUS` ioctl を周期的に叩いてディスクの有無を
//! 見る（[`spawn_poller`]）。TOC は kernel の `CDROMREADTOCHDR` / `CDROMREADTOCENTRY` ioctl
//! （中身は READ TOC コマンド。`/dev/sg*` も外部プロセスも要らない）で LBA 形式で読み、
//! [`Toc`] へ組み立てる。DiscOk になってから TOC が取れるまでは毎周回読み直し、取れたら
//! ディスクが抜かれるまで読み直さない。TOC が取れたら続けて ISRC（トラックごと）と MCN
//! （JAN / UPC。入っている盤だけ）を SG_IO の READ SUB-CHANNEL で 1 回読む（[`DiscIds`]。
//! MusicBrainz の DiscID 以外の識別経路。読めなくても TOC は成立する）。eject は `CDROMEJECT`。
//!
//! ioctl の呼び出しだけが unsafe で、[`LinuxDrive`] に閉じ込める。ポーラの遷移
//! （[`DriveMonitor::poll`]）はドライブを [`Drive`] トレイトで受けるので、フェイクで検証できる

use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
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

/// SG_IO（scsi/sg.h）
const SG_IO: libc::c_ulong = 0x2285;
const SG_INTERFACE_ID_ORIG: libc::c_int = b'S' as libc::c_int;
const SG_DXFER_FROM_DEV: libc::c_int = -3;
/// READ SUB-CHANNEL（MMC）: opcode と `Sub Q` ビット、format の MCN / ISRC
const READ_SUBCHANNEL: u8 = 0x42;
const SUBQ: u8 = 0x40;
const SUBCHANNEL_MCN: u8 = 0x02;
const SUBCHANNEL_ISRC: u8 = 0x03;
/// READ SUB-CHANNEL の応答の長さ（ヘッダ 4 + データ 20）
const SUBCHANNEL_LEN: usize = 24;
/// SG_IO のタイムアウト（ms）。サブチャネルの読みは一瞬で、これは万一の保険
const SG_TIMEOUT_MS: u32 = 10_000;

/// `struct sg_io_hdr`（scsi/sg.h、64 bit）。ポインタは 8 バイト境界に置かれ、全体で 88 バイト
#[repr(C)]
struct SgIoHdr {
    interface_id: libc::c_int,
    dxfer_direction: libc::c_int,
    cmd_len: u8,
    mx_sb_len: u8,
    iovec_count: u16,
    dxfer_len: u32,
    dxferp: *mut libc::c_void,
    cmdp: *mut u8,
    sbp: *mut u8,
    timeout: u32,
    flags: u32,
    pack_id: libc::c_int,
    usr_ptr: *mut libc::c_void,
    status: u8,
    masked_status: u8,
    msg_status: u8,
    sb_len_wr: u8,
    host_status: u16,
    driver_status: u16,
    resid: libc::c_int,
    duration: u32,
    info: u32,
}

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
    /// TOC の音声トラックごとの ISRC と、ディスクの MCN。補助なので失敗しても TOC は成立する
    fn read_ids(&self, toc: &Toc) -> Result<DiscIds, DriveError>;
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

/// TOC 以外のディスクの識別子（Q サブチャネル）。MusicBrainz の ISRC 照会 / バーコード照会に使う
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DiscIds {
    /// 音声トラックごとの ISRC（TOC の音声トラックと同じ順。無いトラックは None）
    pub isrcs: Vec<Option<String>>,
    /// メディアカタログ番号（JAN / UPC の 13 桁）。入っていない盤が多い
    pub mcn: Option<String>,
}

/// READ SUB-CHANNEL の応答の本体（byte 8 の bit 7 が有効ビット、9 から文字列）。正規化後に
/// ちょうど `len` 文字でなければ無し（途中で切れた応答を短い ISRC / MCN として受けない）
fn subchannel_text(resp: &[u8; SUBCHANNEL_LEN], format: u8, len: usize) -> Option<String> {
    if resp[4] != format || resp[8] & 0x80 == 0 {
        return None;
    }
    let raw = &resp[9..9 + len + 1];
    let text = std::str::from_utf8(raw).ok()?.trim_end_matches('\0').trim();
    if text.len() != len
        || !text.is_ascii()
        || text.chars().all(|c| c == '0')
        || text.chars().any(|c| !c.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(text.to_ascii_uppercase())
}

/// READ SUB-CHANNEL（format 3）の応答から ISRC（12 文字）。Tcval が立っていて中身があるときだけ
pub fn parse_subchannel_isrc(resp: &[u8; SUBCHANNEL_LEN]) -> Option<String> {
    subchannel_text(resp, SUBCHANNEL_ISRC, 12)
}

/// READ SUB-CHANNEL（format 2）の応答から MCN（13 桁の数字）。MCval が立っていて全部 0 でないときだけ
pub fn parse_subchannel_mcn(resp: &[u8; SUBCHANNEL_LEN]) -> Option<String> {
    subchannel_text(resp, SUBCHANNEL_MCN, 13).filter(|t| t.chars().all(|c| c.is_ascii_digit()))
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

    /// READ SUB-CHANNEL を SG_IO で 1 回。`track` は ISRC のとき対象トラック（MCN では 0）
    fn read_subchannel(
        file: &std::fs::File,
        format: u8,
        track: u8,
    ) -> Result<[u8; SUBCHANNEL_LEN], DriveError> {
        let mut cdb = [0u8; 10];
        cdb[0] = READ_SUBCHANNEL;
        cdb[2] = SUBQ;
        cdb[3] = format;
        cdb[6] = track;
        cdb[7] = (SUBCHANNEL_LEN >> 8) as u8;
        cdb[8] = SUBCHANNEL_LEN as u8;
        let mut resp = [0u8; SUBCHANNEL_LEN];
        let mut sense = [0u8; 32];
        let mut hdr = SgIoHdr {
            interface_id: SG_INTERFACE_ID_ORIG,
            dxfer_direction: SG_DXFER_FROM_DEV,
            cmd_len: cdb.len() as u8,
            mx_sb_len: sense.len() as u8,
            iovec_count: 0,
            dxfer_len: SUBCHANNEL_LEN as u32,
            dxferp: resp.as_mut_ptr().cast(),
            cmdp: cdb.as_mut_ptr(),
            sbp: sense.as_mut_ptr(),
            timeout: SG_TIMEOUT_MS,
            flags: 0,
            pack_id: 0,
            usr_ptr: std::ptr::null_mut(),
            status: 0,
            masked_status: 0,
            msg_status: 0,
            sb_len_wr: 0,
            host_status: 0,
            driver_status: 0,
            resid: 0,
            duration: 0,
            info: 0,
        };
        // hdr が指す cdb / resp / sense は呼び出しの間生きている（このスコープ）
        Self::ioctl(file, "READ SUB-CHANNEL", SG_IO, &mut hdr as *mut _)?;
        if hdr.status != 0 || hdr.host_status != 0 || hdr.driver_status != 0 {
            return Err(DriveError::Io {
                what: "READ SUB-CHANNEL",
                source: std::io::Error::other(format!(
                    "SCSI status {} host {} driver {} sense {:02x?}",
                    hdr.status,
                    hdr.host_status,
                    hdr.driver_status,
                    &sense[..usize::from(hdr.sb_len_wr).min(sense.len())]
                )),
            });
        }
        // 応答が短い（resid > 0）ときは文字列の位置まで届いていないかもしれないので使わない
        if hdr.resid != 0 {
            return Err(DriveError::Io {
                what: "READ SUB-CHANNEL",
                source: std::io::Error::other(format!("応答が {} バイト足りない", hdr.resid)),
            });
        }
        Ok(resp)
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

    /// ISRC はトラックごとに READ SUB-CHANNEL（format 3）。MCN は format 2 を 1 回。
    /// 1 トラックの読みが失敗しても他は続け、そのトラックだけ None にする（ログに出す）
    fn read_ids(&self, toc: &Toc) -> Result<DiscIds, DriveError> {
        let file = self.open()?;
        let mut isrcs = Vec::new();
        for t in toc.audio_tracks() {
            match Self::read_subchannel(&file, SUBCHANNEL_ISRC, t.number) {
                Ok(resp) => isrcs.push(parse_subchannel_isrc(&resp)),
                Err(e) => {
                    tracing::warn!(track = t.number, error = %e, "ISRC を読めない");
                    isrcs.push(None);
                }
            }
        }
        let mcn = match Self::read_subchannel(&file, SUBCHANNEL_MCN, 0) {
            Ok(resp) => parse_subchannel_mcn(&resp),
            Err(e) => {
                tracing::warn!(error = %e, "MCN を読めない");
                None
            }
        };
        Ok(DiscIds { isrcs, mcn })
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
    /// TOC と一緒に読んだ ISRC / MCN。TOC が無ければ空
    pub ids: DiscIds,
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
            ids: DiscIds::default(),
            error: None,
            checked_at: 0,
        }
    }
}

/// ポーラが更新し、API が読む状態。ドライブ操作（定期 poll、eject + 直後の poll）は `io` で
/// 直列化する: 読み → ドライブ → 書きを一体にしないと、定期 poll が古い DiscOk / TOC を eject の
/// 後に書き戻し、次のディスクでも「TOC がある」として読み直さなくなる
#[derive(Debug, Default)]
pub struct DriveMonitor {
    status: RwLock<DriveStatus>,
    io: Mutex<()>,
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
        let _io = self.io.lock().unwrap_or_else(|e| e.into_inner());
        self.poll_locked(drive, now);
    }

    /// トレイを開けて、同じ排他の中で状態を見直す（次の周期を待たずに反映する）
    pub fn eject_and_poll(&self, drive: &dyn Drive, now: i64) -> Result<(), DriveError> {
        let _io = self.io.lock().unwrap_or_else(|e| e.into_inner());
        let r = drive.eject();
        self.poll_locked(drive, now);
        r
    }

    fn poll_locked(&self, drive: &dyn Drive, now: i64) {
        let prev = self.snapshot();
        let had_toc = prev.toc.is_some();
        let next = match drive.status() {
            Err(e) => DriveStatus {
                state: DriveState::NoDrive,
                toc: None,
                ids: DiscIds::default(),
                error: Some(e.to_string()),
                checked_at: now,
            },
            Ok(DriveState::DiscOk) => {
                // TOC は 1 回読めたら抜かれるまで使い回す。ISRC / MCN も同じ回に読む（補助なので
                // 失敗はログだけ）
                let (toc, ids, error) = match prev.toc {
                    Some(t) => (Some(t), prev.ids, None),
                    None => match drive.read_toc() {
                        Ok(t) => {
                            let ids = drive.read_ids(&t).unwrap_or_else(|e| {
                                tracing::warn!(error = %e, "ISRC / MCN を読めない");
                                DiscIds::default()
                            });
                            (Some(t), ids, None)
                        }
                        Err(e) => (None, DiscIds::default(), Some(e.to_string())),
                    },
                };
                DriveStatus {
                    state: DriveState::DiscOk,
                    toc,
                    ids,
                    error,
                    checked_at: now,
                }
            }
            Ok(state) => DriveStatus {
                state,
                toc: None,
                ids: DiscIds::default(),
                error: None,
                checked_at: now,
            },
        };
        if next.state != prev.state || next.toc.is_some() != had_toc {
            tracing::info!(
                state = ?next.state,
                toc = next.toc.as_ref().map(Toc::ctdb_toc),
                isrcs = ?next.ids.isrcs,
                mcn = next.ids.mcn.as_deref(),
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

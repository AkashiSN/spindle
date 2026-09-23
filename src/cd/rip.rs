//! 吸い出し（SPEC §7.2、D-12 / D-66 / D-83、P2-5）の部品。
//!
//! - [`read_disc`]: `cd-paranoia` で音声トラック全体を 1 本の raw PCM（s16le / 2ch）として読む（D-12）。
//!   オフセットは掛けない（常に 0 で読み、手動・学習済み・照合で見つけた値はすべて後から
//!   [`shift_pcm`] で当てる。リードイン / リードアウトの外を読みに行かないので overread の可否に
//!   左右されない。D-83）。範囲は TOC の長さから `first-last[mm:ss.ff]` で明示する（Enhanced CD の
//!   最後の音声トラックの終端を cd-paranoia の解釈に任せない）。`-e` の進捗行から、書けたセクタ数と
//!   トラックごとの読み取りの問題（scratch / repair / skip / read error）を数える
//! - [`shift_pcm`]: 照合で見つかったオフセットを吸い出した PCM に当てる。`CrcTable` の `offset` は
//!   「DB 側の窓が自分のデータのどこから始まるか」なので、見つかった `r` だけ前へ詰め（`r` が正）、
//!   反対の端を無音で埋める。端の `|r|` サンプルは読めなかったものとして 0（EAC の overread 無しと同じ。
//!   AccurateRip / CTDB の除外範囲に収まるので CRC は変わらない）

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::crctable::MAX_OFFSET;
use super::riplog::TrackRead;
use super::toc::Toc;
use super::LayoutError;
use crate::jobs::process::{ExternalCommand, ProcessError};

/// 1 回の吸い出しの上限（傷の多い盤は paranoia が粘るので長めに取る）
pub const READ_TIMEOUT: Duration = Duration::from_secs(4 * 3600);
/// 1 セクタの 16 bit 語数（cd-paranoia の進捗の位置の単位）
const WORDS_PER_SECTOR: i64 = 1176;
/// 1 セクタのサンプル数（ステレオフレーム）
const SECTOR_FRAMES: u64 = 588;
/// cd-paranoia のコールバックのコード（libcdio `paranoia_cb_mode_t`）
const CB_SCRATCH: i32 = 4;
const CB_REPAIR: i32 = 5;
const CB_SKIP: i32 = 6;
const CB_READERR: i32 = 12;
const CB_WROTE: i32 = 14;

#[derive(Debug, thiserror::Error)]
pub enum RipError {
    #[error("TOC からレイアウトを作れない: {0}")]
    Layout(#[from] LayoutError),
    #[error("音声トラックが無い")]
    NoAudio,
    #[error("読み取りに失敗: {0}")]
    Process(#[from] ProcessError),
    #[error("読めた PCM の長さが TOC と合わない: 期待 {expected} バイト、実際 {actual}")]
    Length { expected: u64, actual: u64 },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("キャンセルされた")]
    Cancelled,
}

/// `cd-paranoia -e` の進捗の 1 行（`##: <code> [<name>] @ <pos>`）。位置は 16 bit 語
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParanoiaEvent {
    pub code: i32,
    pub pos: Option<i64>,
}

pub fn parse_paranoia_line(line: &str) -> Option<ParanoiaEvent> {
    let rest = line.trim().strip_prefix("##:")?.trim_start();
    let (code, rest) = rest.split_once(' ')?;
    let code = code.parse().ok()?;
    let pos = rest
        .rsplit_once('@')
        .and_then(|(_, p)| p.trim().parse::<i64>().ok());
    Some(ParanoiaEvent { code, pos })
}

/// トラック内のセクタ数を `[mm:ss.ff]`（cd-paranoia の範囲の書式。75 フレーム = 1 秒）で
fn span_msf(sectors: u64) -> String {
    format!(
        "[{}:{:02}.{:02}]",
        sectors / 4500,
        (sectors / 75) % 60,
        sectors % 75
    )
}

/// 読む範囲: 最初の音声トラックの頭から、最後の音声トラックの TOC 上の長さの最後のセクタまで（含む）
pub fn paranoia_span(toc: &Toc) -> Result<String, RipError> {
    let sectors = toc.audio_track_sectors();
    let (Some(first), Some(last)) = (sectors.first(), sectors.last()) else {
        return Err(RipError::NoAudio);
    };
    Ok(format!(
        "{}-{}{}",
        first.0,
        last.0,
        span_msf(u64::from(last.1.saturating_sub(1)))
    ))
}

/// 音声トラック全体を `out` に raw PCM で読む。`progress(書けたセクタ数, 全セクタ数)` は書けるたびに
/// 呼ぶ（間引きは呼び出し側）。返り値は音声トラック順の読み取りの問題の数（`rereads` に scratch /
/// repair / skip / read error の回数。cd-paranoia は C2 を使わないので `c2_errors` は 0）
pub async fn read_disc(
    program: &Path,
    device: &Path,
    toc: &Toc,
    out: &Path,
    mut progress: impl FnMut(u64, u64) + Send,
    token: &CancellationToken,
) -> Result<Vec<TrackRead>, RipError> {
    let layout = toc.track_layout()?;
    let span = paranoia_span(toc)?;
    let starts: Vec<i64> = toc.audio_tracks().map(|t| i64::from(t.start_lba)).collect();
    let first_lba = *starts.first().ok_or(RipError::NoAudio)?;
    let total = layout.total_samples() / SECTOR_FRAMES;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    // `-d` の値はオプション引数なので `--` の前に置くしかない（設定のデバイスパス）。範囲と出力は
    // `--` の後
    let cmd = ExternalCommand::new(program)
        .args(["-e", "-r", "-d"])
        .arg(device)
        .arg("--")
        .arg(&span)
        .arg(out)
        .timeout(READ_TIMEOUT)
        .stderr_lines(tx);
    let mut reads = vec![TrackRead::default(); starts.len()];
    let consume = async {
        let mut done = 0u64;
        while let Some(line) = rx.recv().await {
            let Some(ev) = parse_paranoia_line(&line) else {
                continue;
            };
            let Some(pos) = ev.pos else {
                continue;
            };
            let sector = pos.div_euclid(WORDS_PER_SECTOR);
            match ev.code {
                CB_WROTE => {
                    let n = u64::try_from(sector + 1 - first_lba)
                        .unwrap_or(0)
                        .min(total);
                    if n > done {
                        done = n;
                        progress(done, total);
                    }
                }
                CB_SCRATCH | CB_REPAIR | CB_SKIP | CB_READERR => {
                    let idx = starts.partition_point(|&s| s <= sector).saturating_sub(1);
                    if let Some(r) = reads.get_mut(idx) {
                        r.rereads += 1;
                    }
                }
                _ => {}
            }
        }
    };
    let (ran, ()) = tokio::join!(cmd.run(token), consume);
    match ran {
        Ok(_) => {}
        Err(ProcessError::Cancelled) => return Err(RipError::Cancelled),
        Err(e) => return Err(e.into()),
    }
    let expected = layout.total_samples() * FRAME_BYTES;
    let actual = std::fs::metadata(out)?.len();
    if actual != expected {
        return Err(RipError::Length { expected, actual });
    }
    Ok(reads)
}

/// 1 サンプル（ステレオ 1 フレーム）のバイト数
const FRAME_BYTES: u64 = 4;

/// `src` の PCM（s16le / 2ch）を `r` サンプルずらして `dst` に書く。`r > 0` なら先頭の `r` サンプルを
/// 捨てて末尾に `r` サンプルの無音、`r < 0` なら先頭に `|r|` サンプルの無音を足して末尾を捨てる。
/// 長さは変わらない。`|r|` が [`MAX_OFFSET`] を超える・長さより大きいなら `InvalidInput`
pub fn shift_pcm(src: &Path, dst: &Path, r: i32) -> std::io::Result<()> {
    let mut input = File::open(src)?;
    let len = input.metadata()?.len();
    let shift = u64::from(r.unsigned_abs()) * FRAME_BYTES;
    if r.unsigned_abs() > MAX_OFFSET.unsigned_abs() || shift > len || len % FRAME_BYTES != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("PCM をずらせない: r = {r}、長さ {len} バイト"),
        ));
    }
    let mut out = BufWriter::with_capacity(1 << 20, File::create(dst)?);
    let zeros = vec![0u8; shift as usize];
    if r > 0 {
        input.seek(SeekFrom::Start(shift))?;
        std::io::copy(&mut BufReader::with_capacity(1 << 20, input), &mut out)?;
        out.write_all(&zeros)?;
    } else {
        out.write_all(&zeros)?;
        std::io::copy(
            &mut BufReader::with_capacity(1 << 20, input).take(len - shift),
            &mut out,
        )?;
    }
    let file = out.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()?;
    Ok(())
}

// ---------------------------------------------------------------- 吸い出しの本体

use std::sync::Arc;

use serde::Serialize;

use super::accuraterip::ArDiscEntry;
use super::crctable::{CrcSampler, CrcTable};
use super::ctdb::CtdbEntry;
use super::device::{DiscIds, Drive, DriveError};
use super::driveoffsets::DriveEntry;
use super::metadata::{DiscMetadata, MetadataError};
use super::place::{place_disc, PlaceEnv, PlaceError, PlaceInput, Placed};
use super::repair::{DbSyndromes, RepairApplier, SyndromeSampler, MAX_NPAR};
use super::riplog::{OffsetSource, RipReport, TrackCrcs};
use super::verify::{ctdb_candidates, match_accuraterip, match_ctdb, MethodResult, Outcome};
use super::LookupError;
use crate::config::DriveOffset;
use crate::db::drive_offsets::{self, OffsetMethod};
use crate::db::{now_epoch, Db, DbError};
use crate::jobs::BoxFuture;

/// 読み取り（実装は [`ParanoiaReader`]。テストはフェイク）
pub trait DiscReader: Send + Sync {
    fn read<'a>(
        &'a self,
        toc: &'a Toc,
        out: &'a Path,
        progress: &'a (dyn Fn(u64, u64) + Send + Sync),
        token: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<TrackRead>, RipError>>;
}

/// `cd-paranoia` で読む（[`read_disc`]）
pub struct ParanoiaReader {
    pub program: std::path::PathBuf,
    pub device: std::path::PathBuf,
}

impl DiscReader for ParanoiaReader {
    fn read<'a>(
        &'a self,
        toc: &'a Toc,
        out: &'a Path,
        progress: &'a (dyn Fn(u64, u64) + Send + Sync),
        token: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<TrackRead>, RipError>> {
        Box::pin(read_disc(
            &self.program,
            &self.device,
            toc,
            out,
            progress,
            token,
        ))
    }
}

/// 照会（実装は CTDB / AccurateRip のクライアント。テストはフェイク）
pub trait RipLookup: Send + Sync {
    fn ctdb<'a>(&'a self, toc: &'a Toc) -> BoxFuture<'a, Result<Vec<CtdbEntry>, LookupError>>;
    fn accuraterip<'a>(
        &'a self,
        toc: &'a Toc,
    ) -> BoxFuture<'a, Result<Vec<ArDiscEntry>, LookupError>>;
    fn syndromes<'a>(
        &'a self,
        entry: &'a CtdbEntry,
        npar: usize,
    ) -> BoxFuture<'a, Result<DbSyndromes, LookupError>>;
    /// AccurateRip のドライブ表の型番の項（表が取れなければ None。D-83 追記）
    fn table_offset<'a>(&'a self, model: &'a str) -> BoxFuture<'a, Option<DriveEntry>>;
}

/// 本番の照会
pub struct HttpLookup {
    pub ctdb: super::ctdb::CtdbClient,
    pub accuraterip: super::accuraterip::AccurateRipClient,
    pub drives: Arc<super::driveoffsets::DriveOffsetTable>,
}

impl RipLookup for HttpLookup {
    fn ctdb<'a>(&'a self, toc: &'a Toc) -> BoxFuture<'a, Result<Vec<CtdbEntry>, LookupError>> {
        Box::pin(self.ctdb.lookup(toc))
    }
    fn accuraterip<'a>(
        &'a self,
        toc: &'a Toc,
    ) -> BoxFuture<'a, Result<Vec<ArDiscEntry>, LookupError>> {
        Box::pin(async move { self.accuraterip.lookup(&toc.accuraterip_id()).await })
    }
    fn syndromes<'a>(
        &'a self,
        entry: &'a CtdbEntry,
        npar: usize,
    ) -> BoxFuture<'a, Result<DbSyndromes, LookupError>> {
        Box::pin(self.ctdb.fetch_syndromes(entry, npar))
    }
    fn table_offset<'a>(&'a self, model: &'a str) -> BoxFuture<'a, Option<DriveEntry>> {
        Box::pin(self.drives.lookup(model))
    }
}

/// 吸い出しの相（SSE の `job` イベントの `detail.phase`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RipPhase {
    /// ドライブから読む（`done` / `total` はセクタ）
    Read,
    /// CRC を取って照合する（バイト）
    Verify,
    /// CTDB のパリティで直す（バイト）
    Repair,
    /// FLAC にする（トラック）
    Encode,
    /// Inbox に置く
    Place,
}

/// 進捗の 1 報（SSE の `job` イベントの `detail`）
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RipProgress {
    pub phase: RipPhase,
    /// 何回目の吸い出しか（1 始まり）
    pub attempt: u32,
    pub disc_no: u8,
    /// いま扱っている TOC のトラック番号（読み取り・エンコード。分からなければ None）
    pub track_no: Option<u8>,
    pub done: u64,
    pub total: u64,
}

pub type ProgressFn = Arc<dyn Fn(RipProgress) + Send + Sync>;

pub struct RipEnv {
    pub reader: Arc<dyn DiscReader>,
    pub lookup: Arc<dyn RipLookup>,
    pub drive: Arc<dyn Drive>,
    pub db: Arc<Db>,
    pub place: PlaceEnv,
    /// `[rip].drive_offset`
    pub drive_offset: DriveOffset,
    /// `[rip].retry_on_mismatch`（照合が通らなければ吸い直す回数）
    pub retries: u32,
    /// rip.log に書くデバイスのパス
    pub device: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RipJobError {
    #[error("メタデータが不正: {0}")]
    Metadata(#[from] MetadataError),
    #[error("ドライブの盤が取り込みを始めた盤と違う（期待 {expected}、実際 {actual}）")]
    DiscChanged { expected: String, actual: String },
    #[error("ドライブを読めない: {0}")]
    Drive(#[from] DriveError),
    #[error(transparent)]
    Read(#[from] RipError),
    #[error("CRC を計算できない: {0}")]
    Crc(String),
    #[error(transparent)]
    Place(#[from] PlaceError),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("キャンセルされた")]
    Cancelled,
}

impl From<std::io::Error> for RipJobError {
    fn from(e: std::io::Error) -> Self {
        RipJobError::Read(RipError::Io(e))
    }
}

/// 読みながら i16 に直して `f` に渡す（`on_bytes(読んだバイト, 全体)` で進捗）。同期 I/O と計算なので
/// `spawn_blocking` の中で呼ぶ。チャンク（1 MB）ごとに取り消しを見る
fn stream_pcm(
    path: &Path,
    mut f: impl FnMut(&[i16]) -> Result<(), String>,
    mut on_bytes: impl FnMut(u64, u64),
    token: &CancellationToken,
) -> Result<(), RipJobError> {
    let mut file = File::open(path)?;
    let total = file.metadata()?.len();
    let mut buf = vec![0u8; 1 << 20];
    let mut samples: Vec<i16> = Vec::with_capacity(buf.len() / 2);
    let mut done = 0u64;
    let mut carry: Option<u8> = None;
    loop {
        if token.is_cancelled() {
            return Err(RipJobError::Cancelled);
        }
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        samples.clear();
        let mut bytes = &buf[..n];
        if let Some(lo) = carry.take() {
            samples.push(i16::from_le_bytes([lo, bytes[0]]));
            bytes = &bytes[1..];
        }
        let (pairs, rest) = bytes.as_chunks::<2>();
        samples.extend(pairs.iter().map(|c| i16::from_le_bytes(*c)));
        carry = rest.first().copied();
        f(&samples).map_err(RipJobError::Crc)?;
        done += n as u64;
        on_bytes(done, total);
    }
    Ok(())
}

/// PCM ファイルの CRC 表
fn crc_table_of(
    path: &Path,
    toc: &Toc,
    on_bytes: impl FnMut(u64, u64),
    token: &CancellationToken,
) -> Result<CrcTable, RipJobError> {
    let mut sampler = CrcSampler::new(&toc.track_layout().map_err(RipError::from)?);
    stream_pcm(
        path,
        |s| sampler.push(s).map_err(|e| e.to_string()),
        on_bytes,
        token,
    )?;
    sampler
        .finish()
        .map_err(|e| RipJobError::Crc(e.to_string()))
}

/// 1 回の吸い出しの照合結果
struct Checked {
    table: CrcTable,
    ctdb: Option<MethodResult>,
    ar: Option<MethodResult>,
}

impl Checked {
    /// どちらかの手法で全トラックが**オフセット 0 で**一致した（ずれたまま一致したものは、ずれを当てて
    /// からでないと配置しない）
    fn verified(&self) -> bool {
        [&self.ctdb, &self.ar]
            .into_iter()
            .flatten()
            .any(|m| m.outcome == Outcome::Verified && m.offset == 0)
    }

    /// 比べる相手（どちらかの手法の候補）があるか。無ければ吸い直しても結果は変わらない
    fn comparable(&self) -> bool {
        [&self.ctdb, &self.ar]
            .into_iter()
            .flatten()
            .any(|m| m.outcome != Outcome::NotFound)
    }

    /// 当てるずれ: まず全トラックが一致した手法（両方なら CTDB。D-13）、無ければ一部でも一致した
    /// 手法（同じく CTDB を優先）のオフセット。部分一致を全曲一致より優先すると、傷のある盤で別の
    /// オフセットに引きずられる（codex 指摘）
    fn residual(&self) -> Option<i32> {
        let methods = [&self.ctdb, &self.ar];
        let full = methods
            .iter()
            .filter_map(|m| m.as_ref())
            .find(|m| m.outcome == Outcome::Verified);
        let partial = || {
            methods
                .iter()
                .filter_map(|m| m.as_ref())
                .find(|m| m.tracks.iter().any(|t| t.matched))
        };
        full.or_else(partial).map(|m| m.offset)
    }

    /// オフセット 0 で全トラックが一致した手法と信頼度（学習の記録。両方なら CTDB）
    fn verified_by(&self) -> Option<(OffsetMethod, u32)> {
        let ok = |m: &&MethodResult| m.outcome == Outcome::Verified && m.offset == 0;
        if let Some(m) = self.ctdb.as_ref().filter(ok) {
            return Some((OffsetMethod::Ctdb, m.confidence));
        }
        self.ar
            .as_ref()
            .filter(ok)
            .map(|m| (OffsetMethod::AccurateRip, m.confidence))
    }
}

fn check(
    table: CrcTable,
    toc: &Toc,
    ctdb: Option<&[CtdbEntry]>,
    ar: Option<&[ArDiscEntry]>,
) -> Checked {
    Checked {
        ctdb: ctdb.map(|e| match_ctdb(&table, toc, e)),
        ar: ar.map(|e| match_accuraterip(&table, e)),
        table,
    }
}

/// 修復の結果
struct Repaired {
    /// 直した語数
    words: u64,
    /// パリティで見つかったずれ（直した PCM はまだずれている。呼び出し側が `shift_pcm` で当てる）
    offset: i32,
}

/// CTDB のパリティで直した PCM を `out` に書く。直せなければ None（理由はログ）
#[allow(clippy::too_many_arguments)]
async fn try_repair(
    env: &RipEnv,
    toc: &Toc,
    entries: &[CtdbEntry],
    table: CrcTable,
    pcm: &Path,
    out: &Path,
    report: Arc<dyn Fn(RipPhase, u64, u64) + Send + Sync>,
    token: &CancellationToken,
) -> Result<(Option<Repaired>, CrcTable), RipJobError> {
    let mut candidates: Vec<&CtdbEntry> = ctdb_candidates(toc, entries)
        .into_iter()
        .filter(|e| e.has_parity.is_some() && e.npar > 0)
        .collect();
    candidates.sort_by_key(|e| std::cmp::Reverse(e.confidence));
    let Some(entry) = candidates.first().copied() else {
        return Ok((None, table));
    };
    let npar = (entry.npar as usize).min(MAX_NPAR);
    let db = match env.lookup.syndromes(entry, npar).await {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!(error = %super::error_chain(&e), "CTDB のパリティを取れないので修復しない");
            return Ok((None, table));
        }
    };
    let frames = toc.track_layout().map_err(RipError::from)?.total_samples();
    let (pcm, out, token, expected_crc) = (
        pcm.to_path_buf(),
        out.to_path_buf(),
        token.clone(),
        entry.crc32,
    );
    // 2 回の走査（数秒ずつ、最大 800 MB を 2 回読む）は blocking に置く
    tokio::task::spawn_blocking(move || -> Result<(Option<Repaired>, CrcTable), RipJobError> {
        // 1 回目の走査: シンドローム表 → オフセット → 計画
        let mut sampler =
            SyndromeSampler::new(frames, npar).map_err(|e| RipJobError::Crc(e.to_string()))?;
        stream_pcm(
            &pcm,
            |s| sampler.push(s).map_err(|e| e.to_string()),
            |d, n| report(RipPhase::Repair, d, n * 2),
            &token,
        )?;
        let syn = sampler
            .finish()
            .map_err(|e| RipJobError::Crc(e.to_string()))?;
        let Some(found) = syn.find_offset(db.column(0), MAX_OFFSET) else {
            tracing::warn!("CTDB のパリティとオフセットが合わないので修復しない");
            return Ok((None, table));
        };
        let Some(our_crc) = table.ctdb_disc(found.offset) else {
            return Ok((None, table));
        };
        let plan = match syn.plan(&db, found.offset, expected_crc, our_crc) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "CTDB のパリティで直せない（能力超え / CRC 不一致）");
                return Ok((None, table));
            }
        };
        // 2 回目の走査: 直しながら書く
        let mut applier = RepairApplier::new(&plan);
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&out)?);
        let mut write_err: Option<std::io::Error> = None;
        stream_pcm(
            &pcm,
            |s| {
                let mut s = s.to_vec();
                applier.apply(&mut s);
                let bytes: Vec<u8> = s.iter().flat_map(|v| v.to_le_bytes()).collect();
                if let Err(e) = w.write_all(&bytes) {
                    write_err = Some(e);
                    return Err("書けない".to_owned());
                }
                Ok(())
            },
            |d, n| report(RipPhase::Repair, n + d, n * 2),
            &token,
        )
        .map_err(|e| match write_err.take() {
            Some(io) => RipJobError::from(io),
            None => e,
        })?;
        w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        let crc = applier
            .finish()
            .map_err(|e| RipJobError::Crc(e.to_string()))?;
        if crc != plan.crc {
            tracing::warn!("修復後の CRC が計画と違うので使わない");
            return Ok((None, table));
        }
        Ok((
            Some(Repaired {
                words: plan.fixes.len() as u64,
                offset: found.offset,
            }),
            table,
        ))
    })
    .await
    .map_err(|e| std::io::Error::other(format!("修復タスクが異常終了: {e}")))?
}

/// 吸うときのオフセットと出所（D-83）: 設定の整数 → 学習済み → AccurateRip のドライブ表 → 0。
/// 探索範囲（±2939）の外の値（壊れた DB・表）は使わない
pub fn choose_offset(
    config: DriveOffset,
    learned: Option<i32>,
    table: Option<i32>,
) -> (i32, OffsetSource) {
    let ok = |v: &i32| v.unsigned_abs() <= MAX_OFFSET.unsigned_abs();
    if let DriveOffset::Samples(n) = config {
        return (n, OffsetSource::Manual);
    }
    if let Some(v) = learned.filter(ok) {
        return (v, OffsetSource::Learned);
    }
    if let Some(v) = table.filter(ok) {
        return (v, OffsetSource::Table);
    }
    (0, OffsetSource::Unknown)
}

/// 照合が通ったオフセットをドライブの型番ごとに覚える（D-83。手動指定・型番が分からないときは覚えない）
async fn learn(
    env: &RipEnv,
    model: Option<String>,
    offset: i32,
    method: OffsetMethod,
    confidence: u32,
) -> Result<(), RipJobError> {
    let (DriveOffset::Auto, Some(m)) = (env.drive_offset, model) else {
        return Ok(());
    };
    let now = now_epoch();
    env.db
        .write(move |c| drive_offsets::set(c, &m, offset, method, confidence, now))
        .await?;
    Ok(())
}

fn tmp_pcm(dir: &Path, what: &str) -> Result<crate::jobs::TempGuard, RipJobError> {
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).map_err(|e| std::io::Error::other(e.to_string()))?;
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    Ok(crate::jobs::TempGuard::new(
        dir.join(format!("spindle-rip-{what}-{hex}.pcm")),
    ))
}

/// 盤を吸い、照合し（オフセットを当て、直せなければ吸い直し）、Inbox に置く（SPEC §7.2、D-83）。
/// `progress` は相ごとの進捗
pub async fn rip_disc(
    env: &RipEnv,
    toc: &Toc,
    meta: &DiscMetadata,
    ids: &DiscIds,
    progress: ProgressFn,
    token: &CancellationToken,
) -> Result<Placed, RipJobError> {
    meta.validate(toc)?;
    let started_at = now_epoch();
    // ドライブの盤が取り込みを始めた盤であること（入れ替えたまま別の盤を吸わない）
    let (drive, want) = (Arc::clone(&env.drive), toc.ctdb_toc());
    let (actual, model) = tokio::task::spawn_blocking(move || {
        let toc = drive.read_toc()?;
        let model = drive.model().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "ドライブの型番を読めない");
            None
        });
        Ok::<_, DriveError>((toc.ctdb_toc(), model))
    })
    .await
    .map_err(|e| std::io::Error::other(format!("ドライブのタスクが異常終了: {e}")))??;
    if actual != want {
        return Err(RipJobError::DiscChanged {
            expected: want,
            actual,
        });
    }
    // 吸うときのオフセット（D-83）: 手動 → 学習済み → AccurateRip のドライブ表 → 0
    let (learned, table) = match (env.drive_offset, model.clone()) {
        (DriveOffset::Auto, Some(m)) => {
            let key = m.clone();
            // 範囲外の学習値（壊れた DB）は無いものとして表を引く（choose_offset と同じ規則）
            let learned = env
                .db
                .read(move |c| drive_offsets::get(c, &key))
                .await?
                .map(|l| l.offset)
                .filter(|v| v.unsigned_abs() <= MAX_OFFSET.unsigned_abs());
            let table = if learned.is_none() {
                env.lookup.table_offset(&m).await
            } else {
                None
            };
            (learned, table.map(|t| t.offset))
        }
        _ => (None, None),
    };
    let (base, base_source) = choose_offset(env.drive_offset, learned, table);
    // 照会（失敗しても吸い出しは続け、その手法は「照会しなかった」にする）
    let ctdb_entries = match env.lookup.ctdb(toc).await {
        Ok(e) => Some(e),
        Err(e) => {
            tracing::warn!(error = %super::error_chain(&e), "CTDB を照会できない");
            None
        }
    };
    let ar_entries = match env.lookup.accuraterip(toc).await {
        Ok(e) => Some(e),
        Err(e) => {
            tracing::warn!(error = %super::error_chain(&e), "AccurateRip を照会できない");
            None
        }
    };

    let tmp_dir = env.place.tmp_dir.clone();
    std::fs::create_dir_all(&tmp_dir)?;
    let raw = tmp_pcm(&tmp_dir, "raw")?;
    let aligned = tmp_pcm(&tmp_dir, "aligned")?;
    let repaired = tmp_pcm(&tmp_dir, "repaired")?;
    let repaired_aligned = tmp_pcm(&tmp_dir, "repaired-aligned")?;
    let disc_no = meta.disc_no;
    let starts: Vec<(u8, u64)> = toc
        .audio_tracks()
        .scan(0u64, |acc, t| {
            let s = *acc;
            *acc += toc
                .audio_track_sectors()
                .iter()
                .find(|(n, _)| *n == t.number)
                .map(|(_, n)| u64::from(*n))
                .unwrap_or(0);
            Some((t.number, s))
        })
        .collect();
    let max_attempts = env.retries.saturating_add(1);
    let mut attempt = 0;
    let (final_pcm, reads, checked, offset, source, repaired_words) = loop {
        attempt += 1;
        if token.is_cancelled() {
            return Err(RipJobError::Cancelled);
        }
        let p = Arc::clone(&progress);
        let starts_c = starts.clone();
        let on_read = move |done: u64, total: u64| {
            let track_no = starts_c
                .iter()
                .rev()
                .find(|(_, s)| *s < done.max(1))
                .map(|(n, _)| *n);
            p(RipProgress {
                phase: RipPhase::Read,
                attempt,
                disc_no,
                track_no,
                done,
                total,
            });
        };
        let reads = match env.reader.read(toc, raw.path(), &on_read, token).await {
            Ok(r) => r,
            Err(RipError::Cancelled) => return Err(RipJobError::Cancelled),
            Err(e) => return Err(e.into()),
        };
        // 照合（まず吸ったときのオフセットで）
        let (toc_c, ctdb_c, ar_c) = (toc.clone(), ctdb_entries.clone(), ar_entries.clone());
        let p = Arc::clone(&progress);
        let tok = token.clone();
        let verify = move |pcm: std::path::PathBuf, raw: std::path::PathBuf, shift: i32| {
            let (toc, ctdb, ar, p, tok) = (
                toc_c.clone(),
                ctdb_c.clone(),
                ar_c.clone(),
                Arc::clone(&p),
                tok.clone(),
            );
            async move {
                tokio::task::spawn_blocking(move || -> Result<Checked, RipJobError> {
                    if shift != 0 {
                        shift_pcm(&raw, &pcm, shift)?;
                    }
                    let src = if shift != 0 { &pcm } else { &raw };
                    let table = crc_table_of(
                        src,
                        &toc,
                        |d, n| {
                            p(RipProgress {
                                phase: RipPhase::Verify,
                                attempt,
                                disc_no,
                                track_no: None,
                                done: d,
                                total: n,
                            })
                        },
                        &tok,
                    )?;
                    Ok(check(table, &toc, ctdb.as_deref(), ar.as_deref()))
                })
                .await
                .map_err(|e| std::io::Error::other(format!("照合タスクが異常終了: {e}")))?
            }
        };
        let mut checked =
            verify(aligned.path().to_path_buf(), raw.path().to_path_buf(), base).await?;
        let (mut offset, mut source) = (base, base_source);
        if let Some(r) = checked.residual() {
            if let Some(total) = base
                .checked_add(r)
                .filter(|t| r != 0 && t.unsigned_abs() <= MAX_OFFSET.unsigned_abs())
            {
                offset = total;
                checked = verify(
                    aligned.path().to_path_buf(),
                    raw.path().to_path_buf(),
                    offset,
                )
                .await?;
                source = OffsetSource::Detected;
            }
            // 照合が通った盤で覚える（D-83。手動指定のときは覚えない）。記録は当てた後に通った手法
            if let Some((method, conf)) = checked.verified_by() {
                learn(env, model.clone(), offset, method, conf).await?;
            }
        }
        let current: std::path::PathBuf = if offset != 0 {
            aligned.path().to_path_buf()
        } else {
            raw.path().to_path_buf()
        };
        // 通った、または比べる相手が無い（照会できない・DB に無い盤）なら、これで確定
        if checked.verified() || !checked.comparable() {
            break (current, reads, checked, offset, source, None);
        }
        // CTDB のパリティで直す（D-66）。パリティが見つけたずれ（未学習のドライブで、傷のために CRC では
        // 見つからなかったもの）は、直した PCM に当ててからオフセット 0 で照合し直す
        if let Some(entries) = ctdb_entries.as_deref() {
            let p = Arc::clone(&progress);
            let report: Arc<dyn Fn(RipPhase, u64, u64) + Send + Sync> =
                Arc::new(move |phase: RipPhase, d: u64, n: u64| {
                    p(RipProgress {
                        phase,
                        attempt,
                        disc_no,
                        track_no: None,
                        done: d,
                        total: n,
                    })
                });
            let Checked { table, ctdb, ar } = checked;
            let (repair, table) = try_repair(
                env,
                toc,
                entries,
                table,
                &current,
                repaired.path(),
                report,
                token,
            )
            .await?;
            checked = Checked { table, ctdb, ar };
            if let Some(rep) = repair {
                let total = offset
                    .checked_add(rep.offset)
                    .filter(|t| t.unsigned_abs() <= MAX_OFFSET.unsigned_abs());
                if let Some(total) = total {
                    let fixed = verify(
                        repaired_aligned.path().to_path_buf(),
                        repaired.path().to_path_buf(),
                        rep.offset,
                    )
                    .await?;
                    if fixed.verified() {
                        let path = if rep.offset != 0 {
                            repaired_aligned.path().to_path_buf()
                        } else {
                            repaired.path().to_path_buf()
                        };
                        if rep.offset != 0 {
                            source = OffsetSource::Detected;
                        }
                        if let Some((method, conf)) = fixed.verified_by() {
                            learn(env, model.clone(), total, method, conf).await?;
                        }
                        break (path, reads, fixed, total, source, Some(rep.words));
                    }
                }
            }
        }
        if attempt >= max_attempts {
            // 直せなかった: mismatch のまま取り込む（SPEC §7.2。要確認の表示は照合結果から）
            break (current, reads, checked, offset, source, None);
        }
        tracing::info!(attempt, "照合が通らないので吸い直す");
    };

    // 記録
    let crcs: Vec<TrackCrcs> = (0..checked.table.track_count())
        .map(|i| TrackCrcs {
            ar_v1: checked.table.ar_v1(i, 0).unwrap_or(0),
            ar_v2: checked.table.ar_v2(i),
            ctdb: checked.table.ctdb_track(i, 0).unwrap_or(0),
        })
        .collect();
    let report = RipReport {
        drive: model,
        device: env.device.clone(),
        read_offset: offset,
        offset_source: source,
        started_at,
        finished_at: now_epoch(),
        attempts: attempt,
        encoder: format!("flac -{} --verify", env.place.compression.min(8)),
        reads,
        crcs,
        ctdb: checked.ctdb,
        accuraterip: checked.ar,
        repaired_words,
    };
    progress(RipProgress {
        phase: RipPhase::Encode,
        attempt,
        disc_no,
        track_no: starts.first().map(|(n, _)| *n),
        done: 0,
        total: starts.len() as u64,
    });
    let p = Arc::clone(&progress);
    let on_encoded = move |number: u8, done: u64, total: u64| {
        p(RipProgress {
            phase: RipPhase::Encode,
            attempt,
            disc_no,
            track_no: Some(number),
            done,
            total,
        })
    };
    let placed = place_disc(
        &env.place,
        PlaceInput {
            toc,
            metadata: meta,
            pcm: &final_pcm,
            report: &report,
            ids,
            on_encoded: Some(&on_encoded),
        },
        token,
    )
    .await
    .map_err(|e| match e {
        PlaceError::Cancelled => RipJobError::Cancelled,
        other => other.into(),
    })?;
    progress(RipProgress {
        phase: RipPhase::Place,
        attempt,
        disc_no,
        track_no: None,
        done: 1,
        total: 1,
    });
    drop((raw, aligned, repaired, repaired_aligned));
    Ok(placed)
}

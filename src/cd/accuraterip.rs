//! AccurateRip の CRC（SPEC §7.2「CRC 計算」、D-13 では補助）。
//!
//! 定義は CUETools（`CUETools.AccurateRip/AccurateRip.cs` の `CalculateCRCs` / `CRC` /
//! `CRCV2` / `CRC450`）に合わせる。
//!
//! - 「サンプル」は 32 bit 語 `L | (R << 16)`（各 16 bit、符号拡張しない）
//! - v1 = Σ (n × 語) mod 2^32。n はトラック内の 1 始まりの位置
//! - v2 = v1 + Σ ((n × 語) >> 32) mod 2^32（積を 64 bit で取り、上位も足す）
//! - **先頭トラックは頭 5×588−1 サンプル、末尾トラックは尻 5×588 サンプルを除外する。**
//!   非対称なのは AccurateRip 側の仕様。位置 n は除外分も数える
//! - crc450 はトラック先頭から 450 セクタ目の 1 セクタ（588 サンプル）を位置 1 から数えた v1。
//!   AccurateRip DB の応答に入っていて、プレス違い（オフセット差）の検出に使う。
//!   トラックが 451 セクタ未満なら無し

use super::toc::AccurateRipId;
use super::{CrcError, FrameCursor, LookupError, TrackLayout, SECTOR_SAMPLES};

/// 先頭トラックで除外する頭のサンプル数（5 セクタ − 1）
pub const SKIP_HEAD_SAMPLES: u64 = 5 * SECTOR_SAMPLES - 1;
/// 末尾トラックで除外する尻のサンプル数（5 セクタ）
pub const SKIP_TAIL_SAMPLES: u64 = 5 * SECTOR_SAMPLES;
/// crc450 の窓の先頭（トラック内位置、0 始まり）
const CRC450_START: u64 = 450 * SECTOR_SAMPLES;

/// 1 トラック分の CRC
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackCrc {
    pub v1: u32,
    pub v2: u32,
    /// 451 セクタ未満のトラックは `None`
    pub crc450: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default)]
struct TrackState {
    v1: u32,
    /// 積の上位 32 bit の和。v2 = v1 + hi
    hi: u32,
    crc450: u32,
}

/// トラック単位の AccurateRip CRC を、サンプルを順に流して求める
pub struct ArCalculator {
    cursor: FrameCursor,
    lengths: Vec<u64>,
    states: Vec<TrackState>,
}

#[inline]
fn word(frame: (i16, i16)) -> u32 {
    u32::from(frame.0 as u16) | (u32::from(frame.1 as u16) << 16)
}

impl ArCalculator {
    pub fn new(layout: &TrackLayout) -> Self {
        Self {
            cursor: FrameCursor::new(layout),
            lengths: layout.lengths().to_vec(),
            states: vec![TrackState::default(); layout.track_count()],
        }
    }

    /// インターリーブ i16（L, R, L, R, …）を流す。奇数個で終わった L は次回に持ち越す
    pub fn push(&mut self, interleaved: &[i16]) -> Result<(), CrcError> {
        let last = self.lengths.len() - 1;
        let lengths = &self.lengths;
        let states = &mut self.states;
        self.cursor.feed(interleaved, |track, pos, frames| {
            let len = lengths[track];
            let state = &mut states[track];
            let chunk_end = pos + frames.len() as u64;

            // 除外を適用した範囲 [lo, hi) とチャンクの交差
            let lo = if track == 0 { SKIP_HEAD_SAMPLES } else { 0 };
            let hi = if track == last {
                len.saturating_sub(SKIP_TAIL_SAMPLES)
            } else {
                len
            };
            let from = lo.max(pos);
            let to = hi.min(chunk_end);
            if from < to {
                let mut v1 = state.v1;
                let mut hi_sum = state.hi;
                for (i, frame) in frames[(from - pos) as usize..(to - pos) as usize]
                    .iter()
                    .enumerate()
                {
                    let n = from + i as u64 + 1;
                    let p = u64::from(word(*frame)) * n;
                    v1 = v1.wrapping_add(p as u32);
                    hi_sum = hi_sum.wrapping_add((p >> 32) as u32);
                }
                state.v1 = v1;
                state.hi = hi_sum;
            }

            // crc450 の窓 [450×588, 451×588) との交差。除外規則とは独立
            let w_from = CRC450_START.max(pos);
            let w_to = (CRC450_START + SECTOR_SAMPLES).min(chunk_end);
            if w_from < w_to {
                let mut c = state.crc450;
                for (i, frame) in frames[(w_from - pos) as usize..(w_to - pos) as usize]
                    .iter()
                    .enumerate()
                {
                    let n = (w_from - CRC450_START) as u32 + i as u32 + 1;
                    c = c.wrapping_add(n.wrapping_mul(word(*frame)));
                }
                state.crc450 = c;
            }
        })
    }

    /// 全サンプルを受け取っていれば各トラックの CRC を返す
    pub fn finish(self) -> Result<Vec<TrackCrc>, CrcError> {
        self.cursor.finish()?;
        Ok(self
            .states
            .iter()
            .zip(&self.lengths)
            .map(|(s, &len)| TrackCrc {
                v1: s.v1,
                v2: s.v1.wrapping_add(s.hi),
                crc450: (len >= CRC450_START + SECTOR_SAMPLES).then_some(s.crc450),
            })
            .collect())
    }
}

// ---------------------------------------------------------------- DB の照会

/// DB の 1 エントリ（プレスごと）の 1 トラック分
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArTrackEntry {
    /// このプレスで同じ CRC を提出した人数
    pub confidence: u8,
    /// v1 か v2（エントリによって違い、区別されない）
    pub crc: u32,
    pub crc450: u32,
}

/// DB の 1 エントリ。同じ ID の下に複数のプレスが並ぶ
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArDiscEntry {
    pub id: AccurateRipId,
    pub tracks: Vec<ArTrackEntry>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ArParseError {
    #[error("応答が途中で切れている（{at} バイト目）")]
    Truncated { at: usize },
}

/// `dBAR-*.bin` の解釈。エントリの並び: トラック数 u8、id1 / id2 / cddb の u32 LE、
/// トラックごとに confidence u8、crc u32 LE、crc450 u32 LE
pub fn parse_response(bytes: &[u8]) -> Result<Vec<ArDiscEntry>, ArParseError> {
    let mut entries = Vec::new();
    let mut at = 0usize;
    let take = |at: &mut usize, n: usize| -> Result<&[u8], ArParseError> {
        let end = at
            .checked_add(n)
            .ok_or(ArParseError::Truncated { at: *at })?;
        let s = bytes
            .get(*at..end)
            .ok_or(ArParseError::Truncated { at: *at })?;
        *at = end;
        Ok(s)
    };
    let u32le = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    while at < bytes.len() {
        let n = take(&mut at, 1)?[0];
        let id1 = u32le(take(&mut at, 4)?);
        let id2 = u32le(take(&mut at, 4)?);
        let cddb = u32le(take(&mut at, 4)?);
        let mut tracks = Vec::with_capacity(usize::from(n));
        for _ in 0..n {
            let confidence = take(&mut at, 1)?[0];
            let crc = u32le(take(&mut at, 4)?);
            let crc450 = u32le(take(&mut at, 4)?);
            tracks.push(ArTrackEntry {
                confidence,
                crc,
                crc450,
            });
        }
        entries.push(ArDiscEntry {
            id: AccurateRipId {
                id1,
                id2,
                cddb,
                audio_tracks: n,
            },
            tracks,
        });
    }
    Ok(entries)
}

/// DB 上のパス: id1 の下位 4 bit から順に 3 段のディレクトリ + `dBAR-<音声トラック数>-<id1>-<id2>-<cddb>.bin`
pub fn db_path(id: &AccurateRipId) -> String {
    format!(
        "{:x}/{:x}/{:x}/dBAR-{:03}-{:08x}-{:08x}-{:08x}.bin",
        id.id1 & 0xf,
        (id.id1 >> 4) & 0xf,
        (id.id1 >> 8) & 0xf,
        id.audio_tracks,
        id.id1,
        id.id2,
        id.cddb
    )
}

/// AccurateRip DB の照会。`base` は `http://www.accuraterip.com/accuraterip/` のように
/// `/` で終わる URL（設定で差し替え可）
#[derive(Debug, Clone)]
pub struct AccurateRipClient {
    base: String,
    http: reqwest::Client,
}

impl AccurateRipClient {
    pub fn new(base: impl Into<String>, user_agent: &str) -> Result<Self, LookupError> {
        let mut base = base.into();
        if !base.ends_with('/') {
            base.push('/');
        }
        Ok(Self {
            base,
            http: super::http_client(user_agent)?,
        })
    }

    /// ID のエントリを取る。DB に無ければ（404）空
    pub async fn lookup(&self, id: &AccurateRipId) -> Result<Vec<ArDiscEntry>, LookupError> {
        let url = format!("{}{}", self.base, db_path(id));
        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        if !status.is_success() {
            return Err(LookupError::Status(status.as_u16()));
        }
        let bytes = resp.bytes().await?;
        parse_response(&bytes).map_err(|e| LookupError::Parse(e.to_string()))
    }
}

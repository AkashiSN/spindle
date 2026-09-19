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

use super::{CrcError, FrameCursor, TrackLayout, SECTOR_SAMPLES};

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

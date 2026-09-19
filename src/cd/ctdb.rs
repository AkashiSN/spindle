//! CUETools DB（CTDB）の CRC32（SPEC §7.2「CRC 計算」、D-13 で主）。
//! 照会と修復適用は P2-7。
//!
//! 定義は CUETools（`CUETools.AccurateRip/AccurateRip.cs` の `CTDBCRC`、
//! `CUETools.AccurateRip/CDRepair.cs` の `stride` / `laststride`、
//! `CUETools.CTDB/CUEToolsDB.cs` の `stride = 10 * 588 * 2`）に合わせる。
//!
//! - CRC32 は zlib 互換（IEEE 多項式、初期値 0xFFFFFFFF、最終 XOR）。
//!   入力はリトルエンディアン 16 bit の L, R の順のバイト列
//! - ディスク CRC は先頭 10 セクタ（[`HEAD_SKIP_SAMPLES`]）と
//!   末尾 10 セクタ + 総サンプル数 mod 5880（[`tail_skip_samples`]）を除いた範囲。
//!   パリティの stride（10 セクタ）の先頭 1 本と、端数を吸収した末尾 1 本を除くことに相当する
//! - トラック CRC は先頭トラックの頭と末尾トラックの尻に同じ除外を適用し、中間は全体

use super::{CrcError, FrameCursor, TrackLayout, SECTOR_SAMPLES};

/// 除外の単位（パリティ stride の半分 = 10 セクタ）
const STRIDE_SAMPLES: u64 = 10 * SECTOR_SAMPLES;
/// ディスクと先頭トラックで除外する頭のサンプル数
pub const HEAD_SKIP_SAMPLES: u64 = STRIDE_SAMPLES;

/// ディスクと末尾トラックで除外する尻のサンプル数。総サンプル数の端数を吸収する
pub fn tail_skip_samples(total_samples: u64) -> u64 {
    STRIDE_SAMPLES + total_samples % STRIDE_SAMPLES
}

/// ディスク全体と各トラックの CRC32
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscCrc {
    pub disc: u32,
    pub tracks: Vec<u32>,
}

/// CTDB の CRC32 を、サンプルを順に流して求める
pub struct Crc32Calculator {
    cursor: FrameCursor,
    lengths: Vec<u64>,
    /// 各トラックの先頭のディスク内位置
    starts: Vec<u64>,
    total: u64,
    disc: crc32fast::Hasher,
    tracks: Vec<crc32fast::Hasher>,
    /// バイト列化の作業領域（確保を使い回す）
    bytes: Vec<u8>,
}

impl Crc32Calculator {
    pub fn new(layout: &TrackLayout) -> Self {
        let lengths = layout.lengths().to_vec();
        let mut starts = Vec::with_capacity(lengths.len());
        let mut acc = 0u64;
        for &n in &lengths {
            starts.push(acc);
            acc += n;
        }
        Self {
            cursor: FrameCursor::new(layout),
            tracks: vec![crc32fast::Hasher::new(); lengths.len()],
            lengths,
            starts,
            total: layout.total_samples(),
            disc: crc32fast::Hasher::new(),
            bytes: Vec::new(),
        }
    }

    /// インターリーブ i16（L, R, L, R, …）を流す。奇数個で終わった L は次回に持ち越す
    pub fn push(&mut self, interleaved: &[i16]) -> Result<(), CrcError> {
        let last = self.lengths.len() - 1;
        let disc_lo = HEAD_SKIP_SAMPLES;
        let tail = tail_skip_samples(self.total);
        let disc_hi = self.total.saturating_sub(tail);
        let lengths = &self.lengths;
        let starts = &self.starts;
        let disc = &mut self.disc;
        let tracks = &mut self.tracks;
        let bytes = &mut self.bytes;
        self.cursor.feed(interleaved, |track, pos, frames| {
            bytes.clear();
            for &(l, r) in frames {
                bytes.extend_from_slice(&l.to_le_bytes());
                bytes.extend_from_slice(&r.to_le_bytes());
            }
            let chunk_end = pos + frames.len() as u64;

            // トラック CRC: 先頭は頭を、末尾は尻を除く
            let lo = if track == 0 { HEAD_SKIP_SAMPLES } else { 0 };
            let hi = if track == last {
                lengths[track].saturating_sub(tail)
            } else {
                lengths[track]
            };
            let from = lo.max(pos);
            let to = hi.min(chunk_end);
            if from < to {
                tracks[track]
                    .update(&bytes[((from - pos) * 4) as usize..((to - pos) * 4) as usize]);
            }

            // ディスク CRC: ディスク内位置で除外を判定
            let abs = starts[track] + pos;
            let from = disc_lo.max(abs);
            let to = disc_hi.min(abs + frames.len() as u64);
            if from < to {
                disc.update(&bytes[((from - abs) * 4) as usize..((to - abs) * 4) as usize]);
            }
        })
    }

    /// 全サンプルを受け取っていればディスクと各トラックの CRC32 を返す
    pub fn finish(self) -> Result<DiscCrc, CrcError> {
        self.cursor.finish()?;
        Ok(DiscCrc {
            disc: self.disc.finalize(),
            tracks: self.tracks.into_iter().map(|h| h.finalize()).collect(),
        })
    }
}

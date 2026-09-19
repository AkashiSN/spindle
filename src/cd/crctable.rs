//! オフセット付きの CRC 表（SPEC §7.3、P2-9）。
//!
//! AccurateRip / CTDB の DB に登録されているのは他人のドライブで吸った値で、読み取り
//! オフセットの補正が違えば同じ盤でも全サンプルが数個ずれる。CUETools はこれを
//! ±(5×588−1) サンプルの範囲で探す。1 オフセットごとにデコードし直すのは論外なので、
//! 1 回流す間に必要な途中状態だけを記録し、後から任意のオフセットの CRC を O(1) で出す。
//!
//! - AccurateRip v1 は Σ(n × 語) で位置に線形なので、ディスク全体の累積和 P(k) = Σ_{i<k}(i+1)·w_i
//!   と Q(k) = Σ_{i<k} w_i から、窓 [x, y) の CRC = (P(y) − P(x)) − x·(Q(y) − Q(x)) mod 2^32
//! - CTDB の CRC32 は途中状態 C(k)（先頭 k サンプルの CRC32）から、窓 [x, y) の CRC =
//!   `crc32_combine(C(x), C(y), (y−x)×4)`（zlib の combine。XOR が自己逆元なので部分列も取れる）
//! - v2（64 bit 積の上位）は線形でないので、CUETools と同じくオフセット 0 だけ
//!
//! 窓の端はトラック境界 ± オフセット（除外規則のぶんずれる）にしか来ないので、状態は各境界の
//! ±MAX_OFFSET の近傍だけ持つ（1 境界あたり 5879 個 × 12 バイト）

use super::accuraterip::{SKIP_HEAD_SAMPLES, SKIP_TAIL_SAMPLES};
use super::ctdb::{tail_skip_samples, Crc32Combiner, HEAD_SKIP_SAMPLES};
use super::{CrcError, FrameCursor, TrackLayout, SECTOR_SAMPLES};

/// 探索するオフセットの上限（CUETools `_arOffsetRange`）
pub const MAX_OFFSET: i32 = 5 * 588 - 1;
const CRC450_START: u64 = 450 * SECTOR_SAMPLES;

/// 位置 k の直前までの状態
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct State {
    /// Σ_{i<k} (i+1)·w_i
    p: u32,
    /// Σ_{i<k} w_i
    q: u32,
    /// 先頭 k サンプルの CRC32（zlib 互換の最終値）
    crc: u32,
}

/// 位置 `lo..=hi` の状態。`states[k]` は位置 `lo + k` の直前まで
#[derive(Debug, Clone)]
struct Segment {
    lo: u64,
    hi: u64,
    states: Vec<State>,
}

#[inline]
fn word(frame: (i16, i16)) -> u32 {
    u32::from(frame.0 as u16) | (u32::from(frame.1 as u16) << 16)
}

/// 境界の近傍を並べて重なりを併合する
fn segments_for(lengths: &[u64]) -> Vec<Segment> {
    let total: u64 = lengths.iter().sum();
    let m = MAX_OFFSET as u64;
    let mut points = Vec::new();
    let mut start = 0u64;
    for &len in lengths {
        points.push(start);
        if len >= CRC450_START + SECTOR_SAMPLES {
            points.push(start + CRC450_START);
            points.push(start + CRC450_START + SECTOR_SAMPLES);
        }
        start += len;
    }
    points.push(total);
    points.push(SKIP_HEAD_SAMPLES);
    points.push(total.saturating_sub(SKIP_TAIL_SAMPLES));
    points.push(HEAD_SKIP_SAMPLES);
    points.push(total.saturating_sub(tail_skip_samples(total)));
    let mut ranges: Vec<(u64, u64)> = points
        .into_iter()
        .map(|b| (b.saturating_sub(m), (b + m).min(total)))
        .collect();
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (lo, hi) in ranges {
        match merged.last_mut() {
            Some(last) if lo <= last.1 + 1 => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    merged
        .into_iter()
        .map(|(lo, hi)| Segment {
            lo,
            hi,
            states: Vec::with_capacity((hi - lo + 1) as usize),
        })
        .collect()
}

/// サンプルを順に流して [`CrcTable`] を作る
pub struct CrcSampler {
    cursor: FrameCursor,
    lengths: Vec<u64>,
    starts: Vec<u64>,
    total: u64,
    segments: Vec<Segment>,
    seg_idx: usize,
    /// 流した総サンプル数（ディスク内位置）
    pos: u64,
    p: u32,
    q: u32,
    hasher: crc32fast::Hasher,
    /// オフセット 0 の v1 / 上位和（トラック内位置、除外規則あり）
    v1: Vec<u32>,
    hi: Vec<u32>,
    bytes: Vec<u8>,
}

impl CrcSampler {
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
            segments: segments_for(&lengths),
            v1: vec![0; lengths.len()],
            hi: vec![0; lengths.len()],
            lengths,
            starts,
            total: layout.total_samples(),
            seg_idx: 0,
            pos: 0,
            p: 0,
            q: 0,
            hasher: crc32fast::Hasher::new(),
            bytes: Vec::new(),
        }
    }

    /// インターリーブ i16（L, R, …）を流す。奇数個で終わった L は次回に持ち越す
    pub fn push(&mut self, interleaved: &[i16]) -> Result<(), CrcError> {
        let last = self.lengths.len() - 1;
        let lengths = &self.lengths;
        let starts = &self.starts;
        let segments = &mut self.segments;
        let seg_idx = &mut self.seg_idx;
        let pos = &mut self.pos;
        let p = &mut self.p;
        let q = &mut self.q;
        let hasher = &mut self.hasher;
        let v1s = &mut self.v1;
        let his = &mut self.hi;
        let bytes = &mut self.bytes;
        self.cursor.feed(interleaved, |track, in_track, frames| {
            // オフセット 0 の v1 / v2（P2-6 と同じ）
            let len = lengths[track];
            let lo = if track == 0 { SKIP_HEAD_SAMPLES } else { 0 };
            let hi = if track == last {
                len.saturating_sub(SKIP_TAIL_SAMPLES)
            } else {
                len
            };
            let chunk_end = in_track + frames.len() as u64;
            let from = lo.max(in_track);
            let to = hi.min(chunk_end);
            if from < to {
                let mut v1 = v1s[track];
                let mut h = his[track];
                for (i, f) in frames[(from - in_track) as usize..(to - in_track) as usize]
                    .iter()
                    .enumerate()
                {
                    let prod = u64::from(word(*f)) * (from + i as u64 + 1);
                    v1 = v1.wrapping_add(prod as u32);
                    h = h.wrapping_add((prod >> 32) as u32);
                }
                v1s[track] = v1;
                his[track] = h;
            }

            // ディスク内位置での累積和と CRC32。近傍の中では 1 サンプルずつ状態を残し、
            // 外ではまとめて流す
            bytes.clear();
            for &(l, r) in frames {
                bytes.extend_from_slice(&l.to_le_bytes());
                bytes.extend_from_slice(&r.to_le_bytes());
            }
            let base = starts[track] + in_track;
            debug_assert_eq!(base, *pos);
            let mut i = 0usize;
            while i < frames.len() {
                let g = base + i as u64;
                while *seg_idx < segments.len() && g > segments[*seg_idx].hi {
                    *seg_idx += 1;
                }
                let in_segment = *seg_idx < segments.len() && g >= segments[*seg_idx].lo;
                if in_segment {
                    segments[*seg_idx].states.push(State {
                        p: *p,
                        q: *q,
                        crc: hasher.clone().finalize(),
                    });
                    let w = word(frames[i]);
                    *p = p.wrapping_add((g as u32).wrapping_add(1).wrapping_mul(w));
                    *q = q.wrapping_add(w);
                    hasher.update(&bytes[i * 4..i * 4 + 4]);
                    i += 1;
                } else {
                    let run_end = match segments.get(*seg_idx) {
                        Some(s) => ((s.lo - base) as usize).min(frames.len()),
                        None => frames.len(),
                    };
                    for (k, f) in frames[i..run_end].iter().enumerate() {
                        let w = word(*f);
                        let g = base + (i + k) as u64;
                        *p = p.wrapping_add((g as u32).wrapping_add(1).wrapping_mul(w));
                        *q = q.wrapping_add(w);
                    }
                    hasher.update(&bytes[i * 4..run_end * 4]);
                    i = run_end;
                }
            }
            *pos = base + frames.len() as u64;
        })
    }

    /// 全サンプルを受け取っていれば表を返す
    pub fn finish(mut self) -> Result<CrcTable, CrcError> {
        self.cursor.finish()?;
        // 末尾の位置 N（全部流した後）の状態
        let end = State {
            p: self.p,
            q: self.q,
            crc: self.hasher.clone().finalize(),
        };
        for s in &mut self.segments {
            if s.hi == self.total && s.states.len() as u64 == s.hi - s.lo {
                s.states.push(end);
            }
        }
        // CTDB の窓の長さはオフセットによらずトラックごとに一定なので、combine の演算子を前計算する
        let last = self.lengths.len() - 1;
        let tail = tail_skip_samples(self.total);
        let track_combiners = self
            .lengths
            .iter()
            .enumerate()
            .map(|(i, &len)| {
                let head = if i == 0 { HEAD_SKIP_SAMPLES } else { 0 };
                let t = if i == last { tail } else { 0 };
                Crc32Combiner::new(len.saturating_sub(head + t) * 4)
            })
            .collect();
        let disc_combiner =
            Crc32Combiner::new(self.total.saturating_sub(HEAD_SKIP_SAMPLES + tail) * 4);
        Ok(CrcTable {
            lengths: self.lengths,
            starts: self.starts,
            total: self.total,
            segments: self.segments,
            v1: self.v1,
            hi: self.hi,
            track_combiners,
            disc_combiner,
        })
    }
}

/// 任意のオフセットでトラック CRC を出せる表。`offset` は「DB 側の窓が自分のデータの
/// どこから始まるか」で、正なら自分のデータの後ろ、負なら前を見る。範囲外や窓がディスクの
/// 外に出るときは `None`
pub struct CrcTable {
    lengths: Vec<u64>,
    starts: Vec<u64>,
    total: u64,
    segments: Vec<Segment>,
    v1: Vec<u32>,
    hi: Vec<u32>,
    track_combiners: Vec<Crc32Combiner>,
    disc_combiner: Crc32Combiner,
}

impl CrcTable {
    pub fn track_count(&self) -> usize {
        self.lengths.len()
    }

    fn state_at(&self, g: u64) -> Option<State> {
        let idx = self.segments.partition_point(|s| s.lo <= g);
        let s = self.segments.get(idx.checked_sub(1)?)?;
        s.states
            .get(usize::try_from(g.checked_sub(s.lo)?).ok()?)
            .copied()
    }

    /// 窓 [x, y) の端をディスク内位置に解決する。範囲外なら `None`
    fn window(&self, x: i64, y: i64, offset: i32) -> Option<(u64, u64)> {
        if offset.abs() > MAX_OFFSET {
            return None;
        }
        let x = x + i64::from(offset);
        let y = y + i64::from(offset);
        if x < 0 || y < x || y > self.total as i64 {
            return None;
        }
        Some((x as u64, y as u64))
    }

    /// 窓 [x, y) の Σ(n × 語)。n は `origin` を 1 として数える（除外で窓の先頭が
    /// トラック先頭からずれていても、位置はトラック先頭から数える）。
    /// origin は負でもよい（mod 2^32 の環なので 2 の補数のまま掛けてよい）
    fn ar_window(&self, x: u64, y: u64, origin: i64) -> Option<u32> {
        let a = self.state_at(x)?;
        let b = self.state_at(y)?;
        let sum = b.q.wrapping_sub(a.q);
        Some(
            b.p.wrapping_sub(a.p)
                .wrapping_sub((origin as u32).wrapping_mul(sum)),
        )
    }

    /// `combiner` は長さ (y − x) × 4 バイト用に前計算したもの
    fn crc_window(&self, x: u64, y: u64, combiner: &Crc32Combiner) -> Option<u32> {
        let a = self.state_at(x)?;
        let b = self.state_at(y)?;
        Some(combiner.combine(a.crc, b.crc))
    }

    /// AccurateRip v1（除外規則つき）
    pub fn ar_v1(&self, track: usize, offset: i32) -> Option<u32> {
        let len = *self.lengths.get(track)?;
        let start = self.starts[track];
        let head = if track == 0 { SKIP_HEAD_SAMPLES } else { 0 };
        let tail = if track + 1 == self.lengths.len() {
            SKIP_TAIL_SAMPLES
        } else {
            0
        };
        let (x, y) = self.window(
            (start + head) as i64,
            (start + len.saturating_sub(tail)) as i64,
            offset,
        )?;
        if x >= y {
            return Some(0);
        }
        self.ar_window(x, y, start as i64 + i64::from(offset))
    }

    /// AccurateRip v2。オフセット 0 のみ
    pub fn ar_v2(&self, track: usize) -> u32 {
        self.v1[track].wrapping_add(self.hi[track])
    }

    /// 450 セクタ目の 1 セクタの v1（位置 1 から）。451 セクタ未満のトラックは `None`
    pub fn ar_crc450(&self, track: usize, offset: i32) -> Option<u32> {
        let len = *self.lengths.get(track)?;
        if len < CRC450_START + SECTOR_SAMPLES {
            return None;
        }
        let start = self.starts[track] + CRC450_START;
        let (x, y) = self.window(start as i64, (start + SECTOR_SAMPLES) as i64, offset)?;
        self.ar_window(x, y, x as i64)
    }

    /// CTDB のトラック CRC32（先頭の頭・末尾の尻を除く）
    pub fn ctdb_track(&self, track: usize, offset: i32) -> Option<u32> {
        let len = *self.lengths.get(track)?;
        let start = self.starts[track];
        let head = if track == 0 { HEAD_SKIP_SAMPLES } else { 0 };
        let tail = if track + 1 == self.lengths.len() {
            tail_skip_samples(self.total)
        } else {
            0
        };
        let (x, y) = self.window(
            (start + head) as i64,
            (start + len.saturating_sub(tail)) as i64,
            offset,
        )?;
        self.crc_window(x, y, &self.track_combiners[track])
    }

    /// CTDB のディスク CRC32
    pub fn ctdb_disc(&self, offset: i32) -> Option<u32> {
        let (x, y) = self.window(
            HEAD_SKIP_SAMPLES as i64,
            self.total.saturating_sub(tail_skip_samples(self.total)) as i64,
            offset,
        )?;
        self.crc_window(x, y, &self.disc_combiner)
    }
}

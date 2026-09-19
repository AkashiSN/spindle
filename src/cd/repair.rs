//! CTDB のパリティによる修復（SPEC §7.2「照合」「不一致時の既定動作」、D-13 / D-66、P2-7）。
//!
//! 定義は CUETools（`CUETools.AccurateRip/CDRepair.cs`、`CUETools.AccurateRip/AccurateRip.cs` の
//! `GetSyndrome`、`CUETools.Parity/RsDecode.cs`、`CUETools.Parity/Parity2Syndrome.cs`）に合わせる。
//!
//! - ディスクの 16 bit 語（L, R の順）を [`STRIDE_WORDS`] 語ずつの行に並べ、**列ごと**に
//!   GF(2^16)（生成多項式 0x1100B、原始元 α = 2）上の Reed-Solomon 符号語とみなす。データ行は
//!   先頭の 1 行（leadin）と末尾の 1 行 + 端数（leadout、`laststride = stride + 2N mod stride`）を除いた
//!   1..=K 行（`K = 2N / stride − 2`）。この範囲は CTDB のディスク CRC の範囲と同じ
//! - 列 c のシンドローム `S_i(c) = Σ_{r=1..K} d_r · α^{i(K−r)}`（i = 0..npar）。CTDB のパリティファイル
//!   （エントリの `hasparity` の URL）は各列のシンドロームを面順（npar 面 × stride 語、リトルエンディアン）
//!   で持ち、XML の `syndrome` 属性はその列 0（旧い `parity` 属性は列 0 のパリティ 8 語で、
//!   [`parity_to_syndrome`] で同じ形にする）
//! - 自分のシンドロームと DB のシンドロームの XOR が、列ごとの誤り（DB との差）のシンドローム。
//!   Berlekamp-Massey → Chien 探索 → Forney で位置と値を出し、列あたり npar/2 個まで直せる。
//!   直した後のディスク CRC が DB の値と一致することを適用前に確かめる（誤訂正の防護）
//! - オフセット（DB のサンプル k = 自分のサンプル k + offset。[`super::crctable`] と同じ向き）は
//!   列 0 だけを見て探す。ずらした列のシンドロームは、隣の列に leadin / leadout の 1 語を足し引きして
//!   出る（`GetSyndrome` と同じ。|2·offset| < stride）
//!
//! 使い方（吸い出し P2-5）: 1 回目の走査で [`SyndromeSampler`] にサンプルを流して [`SyndromeTable`] を作り、
//! DB の列 0 で [`SyndromeTable::find_offset`]、誤りがあればパリティファイルを取って
//! [`SyndromeTable::plan`]、2 回目の走査で [`RepairApplier`] が語を XOR する

use std::sync::OnceLock;

use base64::Engine;

use super::ctdb::{crc32_combine, CtdbEntry};

/// パリティの stride（16 bit 語）。CTDB は 10 セクタ = 5880 サンプル = 11760 語で固定
pub const STRIDE_WORDS: usize = 2 * 5880;
/// パリティ数の上限（CUETools `maxNpar`）
pub const MAX_NPAR: usize = 16;

/// GF(2^16) の位数 − 1
const GF_MAX: usize = 0xffff;
const GF_POLY: u32 = 0x1100B;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RepairError {
    #[error("npar {0} は 1..=16 の範囲外")]
    BadNpar(usize),
    #[error("stride {0} は 0 か奇数")]
    BadStride(usize),
    #[error("ディスクが短すぎる（{words} 語、stride {stride}。データ行が 1 行も無い）")]
    TooShort { words: u64, stride: usize },
    #[error("ディスクが長すぎる（{rows} 行。符号長が GF(2^16) の位数を超える）")]
    TooLong { rows: u64 },
    #[error("サンプルが多すぎる（期待 {expected} 語）")]
    TooManySamples { expected: u64 },
    #[error("サンプルが足りない（期待 {expected} 語、受信 {received}）")]
    TooFewSamples { expected: u64, received: u64 },
    #[error("オフセット {0} は stride の半分を超える")]
    OffsetOutOfRange(i32),
    #[error("syndrome / parity 属性を読めない: {0}")]
    BadSyndrome(String),
    #[error("パリティファイルが短い（期待 {expected} バイト、受信 {received}）")]
    BadParityFile { expected: usize, received: usize },
    #[error("npar が合わない（自分 {table}、DB {db}）")]
    NparMismatch { table: usize, db: usize },
    #[error("stride が合わない（自分 {table}、DB {db}）")]
    StrideMismatch { table: usize, db: usize },
    #[error("列 {column} の誤りが多すぎて直せない")]
    Uncorrectable { column: usize },
    #[error("直した後の CRC が合わない（期待 {expected:08x}、実際 {actual:08x}）")]
    CrcMismatch { expected: u32, actual: u32 },
}

// ---------------------------------------------------------------- GF(2^16)

/// GF(2^16) の対数表。`exp` は 2 周ぶん持ち、log の和をそのまま引ける
pub struct Gf16 {
    exp: Vec<u16>,
    log: Vec<u16>,
}

impl Gf16 {
    fn build() -> Self {
        let mut exp = vec![0u16; GF_MAX * 2];
        let mut log = vec![0u16; GF_MAX + 1];
        let mut d: u32 = 1;
        for i in 0..GF_MAX {
            exp[i] = d as u16;
            exp[GF_MAX + i] = d as u16;
            log[d as usize] = i as u16;
            d <<= 1;
            if d & 0x10000 != 0 {
                d = (d ^ GF_POLY) & GF_MAX as u32;
            }
        }
        Self { exp, log }
    }

    /// α^e（e < 2·65535）
    #[inline]
    pub fn exp(&self, e: usize) -> u16 {
        self.exp[e]
    }

    /// log_α a（a ≠ 0）
    #[inline]
    pub fn log(&self, a: u16) -> usize {
        usize::from(self.log[usize::from(a)])
    }

    #[inline]
    pub fn mul(&self, a: u16, b: u16) -> u16 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[self.log(a) + self.log(b)]
        }
    }

    /// a · α^e（e < 65535）
    #[inline]
    fn mul_exp(&self, a: u16, e: usize) -> u16 {
        if a == 0 {
            0
        } else {
            self.exp[self.log(a) + e]
        }
    }

    /// a / α^e（e ≤ 65535）
    #[inline]
    fn div_exp(&self, a: u16, e: usize) -> u16 {
        if a == 0 {
            0
        } else {
            self.exp[self.log(a) + GF_MAX - e]
        }
    }

    pub fn inv(&self, a: u16) -> u16 {
        self.exp[GF_MAX - self.log(a)]
    }

    pub fn div(&self, a: u16, b: u16) -> u16 {
        if a == 0 {
            0
        } else {
            self.exp[self.log(a) + GF_MAX - self.log(b)]
        }
    }
}

/// 共有の表（初回に 400 KB を作る）
pub fn gf16() -> &'static Gf16 {
    static TABLE: OnceLock<Gf16> = OnceLock::new();
    TABLE.get_or_init(Gf16::build)
}

/// 定数 α^i を掛けるバイト表（下位 8 bit と上位 8 bit）。Horner の内側で log / exp を引かないため
fn mul_alpha_tables(npar: usize) -> Vec<[u16; 512]> {
    let g = gf16();
    (0..npar)
        .map(|i| {
            let mut t = [0u16; 512];
            for b in 0..256 {
                t[b] = g.mul_exp(b as u16, i);
                t[256 + b] = g.mul_exp((b as u16) << 8, i);
            }
            t
        })
        .collect()
}

// ---------------------------------------------------------------- シンドローム表

/// ディスク 1 枚のシンドロームを、サンプルを順に流して作る
pub struct SyndromeSampler {
    stride: usize,
    npar: usize,
    rows: usize,
    total_words: u64,
    pos: u64,
    /// 列 c のシンドローム i は `syn[c * npar + i]`
    syn: Vec<u16>,
    /// 先頭 2·stride 語
    leadin: Vec<u16>,
    /// 末尾 stride + laststride 語（自然な順）
    leadout: Vec<u16>,
    tables: Vec<[u16; 512]>,
}

impl SyndromeSampler {
    /// `total_frames` は L/R の組の数（語数の半分）。stride は CTDB の既定
    pub fn new(total_frames: u64, npar: usize) -> Result<Self, RepairError> {
        Self::with_stride(total_frames, npar, STRIDE_WORDS)
    }

    /// stride を変える（テスト用。本番は [`STRIDE_WORDS`]）
    pub fn with_stride(total_frames: u64, npar: usize, stride: usize) -> Result<Self, RepairError> {
        if npar == 0 || npar > MAX_NPAR {
            return Err(RepairError::BadNpar(npar));
        }
        if stride == 0 || !stride.is_multiple_of(2) {
            return Err(RepairError::BadStride(stride));
        }
        let total_words = total_frames
            .checked_mul(2)
            .ok_or(RepairError::TooLong { rows: u64::MAX })?;
        let strides = total_words / stride as u64;
        if strides < 3 {
            return Err(RepairError::TooShort {
                words: total_words,
                stride,
            });
        }
        // 符号長（行数 + パリティ）が GF(2^16) の位数を超えると位置を表せない（CUETools "invalid stride"）
        if strides - 2 + MAX_NPAR as u64 > GF_MAX as u64 {
            return Err(RepairError::TooLong { rows: strides - 2 });
        }
        let rows = (strides - 2) as usize;
        let laststride = stride + (total_words % stride as u64) as usize;
        Ok(Self {
            stride,
            npar,
            rows,
            total_words,
            pos: 0,
            syn: vec![0; stride * npar],
            leadin: vec![0; 2 * stride],
            leadout: vec![0; stride + laststride],
            tables: mul_alpha_tables(npar),
        })
    }

    /// インターリーブした 16 bit サンプルを流す（1 サンプル = 1 語）
    pub fn push(&mut self, samples: &[i16]) -> Result<(), RepairError> {
        let end = self.pos + samples.len() as u64;
        if end > self.total_words {
            return Err(RepairError::TooManySamples {
                expected: self.total_words,
            });
        }
        let stride = self.stride as u64;
        let npar = self.npar;
        let region_end = stride * (self.rows as u64 + 1);
        let leadout_start = self.total_words - self.leadout.len() as u64;
        for (k, &s) in samples.iter().enumerate() {
            let j = self.pos + k as u64;
            let w = s as u16;
            if j < self.leadin.len() as u64 {
                self.leadin[j as usize] = w;
            }
            if j >= leadout_start {
                self.leadout[(j - leadout_start) as usize] = w;
            }
            if j >= stride && j < region_end {
                let c = (j % stride) as usize;
                let syn = &mut self.syn[c * npar..(c + 1) * npar];
                for (i, t) in self.tables.iter().enumerate() {
                    let s = syn[i];
                    syn[i] = w ^ t[usize::from(s & 0xff)] ^ t[256 + usize::from(s >> 8)];
                }
            }
        }
        self.pos = end;
        Ok(())
    }

    pub fn finish(self) -> Result<SyndromeTable, RepairError> {
        if self.pos != self.total_words {
            return Err(RepairError::TooFewSamples {
                expected: self.total_words,
                received: self.pos,
            });
        }
        Ok(SyndromeTable {
            stride: self.stride,
            npar: self.npar,
            rows: self.rows,
            total_words: self.total_words,
            syn: self.syn,
            leadin: self.leadin,
            leadout: self.leadout,
        })
    }
}

/// オフセット探索の結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OffsetMatch {
    pub offset: i32,
    /// 列 0 の誤り数（0 なら列 0 は DB と完全一致）
    pub errors: usize,
}

/// 1 語の修正
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fix {
    /// 自分のデータ内の語の位置（サンプル列の添字。L, R の順）
    pub word: u64,
    /// XOR する値（DB との差）
    pub xor: u16,
}

/// 修復の計画（[`SyndromeTable::plan`]）。適用は [`RepairApplier`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairPlan {
    pub offset: i32,
    /// 語の位置順
    pub fixes: Vec<Fix>,
    /// 直した後のディスク CRC（DB の値と一致することを確認済み）
    pub crc: u32,
    /// CRC の範囲（自分の語の位置。データ行）
    region: std::ops::Range<u64>,
    total_words: u64,
}

impl RepairPlan {
    /// 直す語のあるセクタ（0 始まり、重複なし・昇順）
    pub fn affected_sectors(&self) -> Vec<u64> {
        let mut out: Vec<u64> = self.fixes.iter().map(|f| f.word / (2 * 588)).collect();
        out.dedup();
        out
    }
}

/// ディスク 1 枚のシンドローム（[`SyndromeSampler::finish`]）
pub struct SyndromeTable {
    stride: usize,
    npar: usize,
    rows: usize,
    total_words: u64,
    syn: Vec<u16>,
    leadin: Vec<u16>,
    leadout: Vec<u16>,
}

impl SyndromeTable {
    pub fn stride(&self) -> usize {
        self.stride
    }

    pub fn npar(&self) -> usize {
        self.npar
    }

    /// データ行の数 K
    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn total_words(&self) -> u64 {
        self.total_words
    }

    /// 列 c のシンドローム（オフセット 0）
    pub fn column(&self, c: usize) -> &[u16] {
        &self.syn[c * self.npar..(c + 1) * self.npar]
    }

    fn check_offset(&self, offset: i32) -> Result<i64, RepairError> {
        let shift = 2 * i64::from(offset);
        if shift.unsigned_abs() >= self.stride as u64 {
            return Err(RepairError::OffsetOutOfRange(offset));
        }
        Ok(shift)
    }

    /// DB のサンプル k が自分のサンプル k + offset のときの、DB の列 c のシンドローム。
    /// 列 c + 2·offset が行をまたぐときは leadin / leadout の 1 語で行をずらす
    pub fn column_at(&self, c: usize, offset: i32) -> Result<Vec<u16>, RepairError> {
        let shift = self.check_offset(offset)?;
        let mut out = vec![0u16; self.npar];
        self.column_at_into(c, shift, &mut out);
        Ok(out)
    }

    fn column_at_into(&self, c: usize, shift: i64, out: &mut [u16]) {
        let g = gf16();
        let stride = self.stride as i64;
        let part = c as i64 + shift;
        let k = self.rows;
        if (0..stride).contains(&part) {
            out.copy_from_slice(self.column(part as usize));
        } else if part >= stride {
            // DB の行 r = 自分の行 r + 1: d_1 が抜けて d_{K+1} が入る
            let part = (part - stride) as usize;
            let d1 = self.leadin[self.stride + part];
            let dk1 = self.leadout[self.stride + part];
            for (i, o) in out.iter_mut().enumerate() {
                let s = self.column(part)[i];
                *o = g.mul_exp(s, i) ^ g.mul_exp(d1, (i * k) % GF_MAX) ^ dk1;
            }
        } else {
            // DB の行 r = 自分の行 r − 1: d_0 が入って d_K が抜ける
            let part = (part + stride) as usize;
            let d0 = self.leadin[part];
            let dk = self.leadout[part];
            for (i, o) in out.iter_mut().enumerate() {
                let s = self.column(part)[i];
                *o = g.div_exp(s ^ g.mul_exp(d0, (i * k) % GF_MAX) ^ dk, i);
            }
        }
    }

    /// DB の列 0 のシンドローム（XML の `syndrome`）でオフセットを探す。|offset| ≤ max_offset を
    /// 0 から外側へ見て、完全一致があればそれ、無ければ列 0 の誤りが npar/2 未満で直せるうち最少のもの
    /// （CUETools `FindOffset`）。どれも無ければ None
    pub fn find_offset(&self, db_column0: &[u16], max_offset: i32) -> Option<OffsetMatch> {
        let npar = self.npar.min(db_column0.len());
        if npar == 0 {
            return None;
        }
        let decoder = RsDecoder::new(npar);
        let mut best: Option<OffsetMatch> = None;
        let mut buf = vec![0u16; self.npar];
        let mut syn = vec![0u16; npar];
        let mut order = vec![0i32];
        for o in 1..=max_offset {
            order.push(-o);
            order.push(o);
        }
        for offset in order {
            let Ok(shift) = self.check_offset(offset) else {
                continue;
            };
            self.column_at_into(0, shift, &mut buf);
            let mut any = false;
            for i in 0..npar {
                syn[i] = buf[i] ^ db_column0[i];
                any |= syn[i] != 0;
            }
            if !any {
                return Some(OffsetMatch { offset, errors: 0 });
            }
            let Some(errors) = decoder.count_errors(&syn, self.rows) else {
                continue;
            };
            if errors < npar / 2 && best.is_none_or(|b| errors < b.errors) {
                best = Some(OffsetMatch { offset, errors });
            }
        }
        best
    }

    /// 修復を計画する。`expected_crc` は DB エントリのディスク CRC、`our_crc` は自分のデータの
    /// 同じオフセットでのディスク CRC（[`super::crctable::CrcTable::ctdb_disc`]）。直した後の CRC が
    /// `expected_crc` に一致しなければ計画ごと捨てる
    pub fn plan(
        &self,
        db: &DbSyndromes,
        offset: i32,
        expected_crc: u32,
        our_crc: u32,
    ) -> Result<RepairPlan, RepairError> {
        if db.stride != self.stride {
            return Err(RepairError::StrideMismatch {
                table: self.stride,
                db: db.stride,
            });
        }
        if db.npar != self.npar {
            return Err(RepairError::NparMismatch {
                table: self.npar,
                db: db.npar,
            });
        }
        let shift = self.check_offset(offset)?;
        let npar = self.npar;
        let k = self.rows;
        let stride = self.stride as u64;
        let region_words = stride * k as u64;
        let region_start = (stride as i64 + shift) as u64;
        let decoder = RsDecoder::new(npar);
        let mut buf = vec![0u16; npar];
        let mut syn = vec![0u16; npar];
        let mut fixes = Vec::new();
        let mut crc = our_crc;
        for c in 0..self.stride {
            self.column_at_into(c, shift, &mut buf);
            let mut any = false;
            for i in 0..npar {
                syn[i] = buf[i] ^ db.column(c)[i];
                any |= syn[i] != 0;
            }
            if !any {
                continue;
            }
            let decoded = decoder
                .decode(&syn, k)
                .ok_or(RepairError::Uncorrectable { column: c })?;
            for (row_index, value) in decoded {
                // row_index は 0 = 先頭のデータ行（d_1）
                let pos = row_index as u64 * stride + c as u64;
                let word = region_start + pos;
                let tail_bytes = (region_words - pos - 1) * 2;
                crc ^= crc32_combine(crc32_raw(&value.to_le_bytes()), 0, tail_bytes);
                fixes.push(Fix { word, xor: value });
            }
        }
        if crc != expected_crc {
            return Err(RepairError::CrcMismatch {
                expected: expected_crc,
                actual: crc,
            });
        }
        fixes.sort_unstable_by_key(|f| f.word);
        Ok(RepairPlan {
            offset,
            fixes,
            crc,
            region: region_start..region_start + region_words,
            total_words: self.total_words,
        })
    }
}

/// CRC-32（IEEE、反転形）を初期値 0・最終 XOR 無しで。誤りパターンの CRC への寄与は線形なので、
/// これを零送り（[`crc32_combine`]）した値を XOR すれば直した後の CRC になる
fn crc32_raw(bytes: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    crc
}

// ---------------------------------------------------------------- DB 側

/// DB のシンドローム（パリティファイル、または自分の表から）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbSyndromes {
    stride: usize,
    npar: usize,
    /// 列順（`syn[c * npar + i]`）
    syn: Vec<u16>,
}

impl DbSyndromes {
    /// パリティファイル（npar 面 × stride 語、リトルエンディアン）を読む。余りは無視する
    pub fn parse(bytes: &[u8], stride: usize, npar: usize) -> Result<Self, RepairError> {
        if npar == 0 || npar > MAX_NPAR {
            return Err(RepairError::BadNpar(npar));
        }
        if stride == 0 {
            return Err(RepairError::BadStride(stride));
        }
        let expected = stride * npar * 2;
        if bytes.len() < expected {
            return Err(RepairError::BadParityFile {
                expected,
                received: bytes.len(),
            });
        }
        let mut syn = vec![0u16; stride * npar];
        for i in 0..npar {
            for c in 0..stride {
                let at = (i * stride + c) * 2;
                syn[c * npar + i] = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
            }
        }
        Ok(Self { stride, npar, syn })
    }

    /// 自分の表（オフセット 0）を DB の形に。テストと、将来の提出用
    pub fn from_table(table: &SyndromeTable) -> Self {
        Self {
            stride: table.stride,
            npar: table.npar,
            syn: table.syn.clone(),
        }
    }

    pub fn stride(&self) -> usize {
        self.stride
    }

    pub fn npar(&self) -> usize {
        self.npar
    }

    pub fn column(&self, c: usize) -> &[u16] {
        &self.syn[c * self.npar..(c + 1) * self.npar]
    }
}

/// 列 0 のパリティ（旧い `parity` 属性。x^(npar−1) の係数から順）→ シンドローム npar 個
/// （CUETools `Parity2Syndrome`: `S_x = Σ p[j] · α^(−x(j+1))`）
pub fn parity_to_syndrome(parity: &[u16], npar: usize) -> Vec<u16> {
    let g = gf16();
    (0..npar)
        .map(|x| {
            let mut s = 0u16;
            for (j, &p) in parity.iter().enumerate() {
                if p != 0 {
                    s ^= g.exp(g.log(p) + GF_MAX - ((j + 1) * x) % GF_MAX);
                }
            }
            s
        })
        .collect()
}

/// エントリの列 0 のシンドローム。`syndrome` 属性（リトルエンディアン 16 bit × min(npar, 16)）か、
/// 無ければ旧い `parity` 属性（パリティ 8 語）から。どちらも無ければ None
pub fn decode_entry_syndrome(entry: &CtdbEntry) -> Result<Option<Vec<u16>>, RepairError> {
    let b64 = base64::engine::general_purpose::STANDARD;
    if let Some(s) = &entry.syndrome {
        let bytes = b64
            .decode(s.trim())
            .map_err(|e| RepairError::BadSyndrome(e.to_string()))?;
        let npar = (entry.npar as usize).min(MAX_NPAR);
        if bytes.len() < npar * 2 {
            return Err(RepairError::BadSyndrome(format!(
                "syndrome が短い（{} バイト、npar {npar}）",
                bytes.len()
            )));
        }
        return Ok(Some(
            bytes[..npar * 2]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_le_bytes(*b))
                .collect(),
        ));
    }
    if let Some(p) = &entry.parity {
        let bytes = b64
            .decode(p.trim())
            .map_err(|e| RepairError::BadSyndrome(e.to_string()))?;
        if bytes.len() < 16 {
            return Err(RepairError::BadSyndrome(format!(
                "parity が短い（{} バイト）",
                bytes.len()
            )));
        }
        let parity: Vec<u16> = bytes[..16]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .collect();
        return Ok(Some(parity_to_syndrome(&parity, 8)));
    }
    Ok(None)
}

// ---------------------------------------------------------------- RS 復号（CUETools RsDecode）

/// 1 列ぶんの Reed-Solomon 復号。シンドローム S_0..S_{npar−1} から誤り位置と値を出す
struct RsDecoder {
    npar: usize,
}

impl RsDecoder {
    fn new(npar: usize) -> Self {
        Self { npar }
    }

    /// 誤り位置多項式 σ（Berlekamp-Massey の変形）。戻りは次数（誤り数）。求まらなければ None
    fn calc_sigma(&self, syn: &[u16], sigma: &mut [u16]) -> Option<usize> {
        let g = gf16();
        let npar = self.npar;
        let mut sg0 = vec![0u16; npar + 1];
        let mut sg1 = vec![0u16; npar + 1];
        let mut wk = vec![0u16; npar + 1];
        sg0[1] = 1;
        sg1[0] = 1;
        let mut jisu0 = 1usize;
        let mut jisu1 = 0usize;
        let mut m: i64 = -1;
        for n in 0..npar {
            let mut d = syn[n];
            for i in 1..=jisu1 {
                d ^= g.mul(sg1[i], syn[n - i]);
            }
            if d != 0 {
                let logd = g.log(d);
                for i in 0..=n {
                    wk[i] = sg1[i] ^ g.mul_exp(sg0[i], logd);
                }
                let js = n as i64 - m;
                if js > jisu1 as i64 {
                    for i in 0..=jisu0 {
                        sg0[i] = g.div_exp(sg1[i], logd);
                    }
                    m = n as i64 - jisu1 as i64;
                    jisu1 = js as usize;
                    jisu0 = js as usize;
                }
                sg1[..npar].copy_from_slice(&wk[..npar]);
            }
            for i in (1..=jisu0).rev() {
                sg0[i] = sg0[i - 1];
            }
            sg0[0] = 0;
            jisu0 += 1;
        }
        if sg1[jisu1] == 0 {
            return None;
        }
        let n = sigma.len().min(npar);
        sigma[..n].copy_from_slice(&sg1[..n]);
        Some(jisu1)
    }

    /// Chien 探索: σ(z) = 0 の解をデータ長 n の範囲で探す。解は α^i（i は誤り項の次数）で、
    /// 見つからなければ None
    fn chien(&self, sigma: &[u16], jisu: usize, n: usize) -> Option<Vec<u16>> {
        let g = gf16();
        let mut pos = vec![0u16; jisu];
        let mut last = sigma[1];
        if jisu == 1 {
            return (last != 0 && g.log(last) < n).then(|| {
                pos[0] = last;
                pos
            });
        }
        let mut sg: Vec<u16> = sigma[..=jisu].to_vec();
        let mut pos_idx = jisu - 1;
        for i in 0..n {
            let mut wk = 1u16;
            for s in &sg[1..=jisu] {
                wk ^= s;
            }
            for (j, s) in sg.iter_mut().enumerate().skip(1) {
                *s = g.div_exp(*s, j);
            }
            if wk == 0 {
                let pv = g.exp(i);
                last ^= pv;
                pos[pos_idx] = pv;
                if pos_idx == 1 {
                    // 残りの 1 解は last（σ1 は解の総和なので、見つけた解を引いていくと残る）
                    pos[0] = last;
                    return (last != 0 && g.log(last) < n).then_some(pos);
                }
                pos_idx -= 1;
            }
        }
        None
    }

    /// Forney: 位置 ps（= α^i）の誤りの値
    fn forney(&self, jisu: usize, ps: u16, sigma: &[u16], omega: &[u16]) -> u16 {
        let g = gf16();
        let zlog = GF_MAX - g.log(ps);
        let mut ov = omega[0];
        for (j, &o) in omega.iter().enumerate().take(jisu).skip(1) {
            ov ^= g.mul_exp(o, (zlog * j) % GF_MAX);
        }
        let mut dv = sigma[1];
        let mut j = 2;
        while j < jisu {
            dv ^= g.mul_exp(sigma[j + 1], (zlog * j) % GF_MAX);
            j += 2;
        }
        g.mul(ps, g.div(ov, dv))
    }

    /// 誤り数だけ（オフセット探索用）。位置がデータ長に収まらなければ None
    fn count_errors(&self, syn: &[u16], n: usize) -> Option<usize> {
        let mut sigma = vec![0u16; self.npar / 2 + 2];
        let jisu = self.calc_sigma(syn, &mut sigma)?;
        if jisu == 0 || jisu > self.npar / 2 {
            return None;
        }
        self.chien(&sigma, jisu, n).map(|_| jisu)
    }

    /// 復号: (行の添字（0 = 先頭のデータ行）, 誤りの値) の列。npar/2 個を超える、または位置が
    /// 求まらなければ None
    fn decode(&self, syn: &[u16], n: usize) -> Option<Vec<(usize, u16)>> {
        let g = gf16();
        let sf_len = self.npar / 2 + 2;
        let of_len = self.npar / 2 + 1;
        let mut sigma = vec![0u16; sf_len];
        let jisu = self.calc_sigma(syn, &mut sigma)?;
        if jisu == 0 || jisu > self.npar / 2 {
            return None;
        }
        let pos = self.chien(&sigma, jisu, n)?;
        // ω = σ · S（下位 of_len 項）
        let mut omega = vec![0u16; of_len];
        for (ia, &a) in sigma.iter().enumerate() {
            if a == 0 {
                continue;
            }
            let loga = g.log(a);
            for ib in 0..syn.len().min(of_len.saturating_sub(ia)) {
                let b = syn[ib];
                if b != 0 {
                    omega[ia + ib] ^= g.exp(loga + g.log(b));
                }
            }
        }
        let mut out = Vec::with_capacity(jisu);
        for &ps in &pos {
            // toPos: n − 1 − log(ps) が 0 = 最高次（先頭のデータ行）の添字
            let row = n - 1 - g.log(ps);
            out.push((row, self.forney(jisu, ps, &sigma, &omega)));
        }
        Some(out)
    }
}

// ---------------------------------------------------------------- 適用

/// 計画を 2 回目の走査で適用する。語を XOR しながらデータ行の CRC も取り、終わりに返す
pub struct RepairApplier {
    fixes: Vec<Fix>,
    next: usize,
    pos: u64,
    total_words: u64,
    region: std::ops::Range<u64>,
    hasher: crc32fast::Hasher,
}

impl RepairApplier {
    pub fn new(plan: &RepairPlan) -> Self {
        Self {
            fixes: plan.fixes.clone(),
            next: 0,
            pos: 0,
            total_words: plan.total_words,
            region: plan.region.clone(),
            hasher: crc32fast::Hasher::new(),
        }
    }

    /// インターリーブしたサンプルをその場で直す
    pub fn apply(&mut self, samples: &mut [i16]) {
        let start = self.pos;
        let end = start + samples.len() as u64;
        while let Some(f) = self.fixes.get(self.next) {
            if f.word >= end {
                break;
            }
            if f.word >= start {
                let s = &mut samples[(f.word - start) as usize];
                *s = ((*s as u16) ^ f.xor) as i16;
            }
            self.next += 1;
        }
        // データ行にかかる部分の CRC
        let lo = self.region.start.max(start);
        let hi = self.region.end.min(end);
        if lo < hi {
            let mut bytes = Vec::with_capacity(((hi - lo) * 2) as usize);
            for s in &samples[(lo - start) as usize..(hi - start) as usize] {
                bytes.extend_from_slice(&s.to_le_bytes());
            }
            self.hasher.update(&bytes);
        }
        self.pos = end;
    }

    /// 全部流し終えたら、データ行の CRC（[`RepairPlan::crc`] と一致するはず）
    pub fn finish(self) -> Result<u32, RepairError> {
        if self.pos != self.total_words {
            return Err(RepairError::TooFewSamples {
                expected: self.total_words,
                received: self.pos,
            });
        }
        Ok(self.hasher.finalize())
    }
}

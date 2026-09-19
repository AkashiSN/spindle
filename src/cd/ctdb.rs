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

use quick_xml::events::Event;
use quick_xml::Reader;

use super::toc::Toc;
use super::{CrcError, FrameCursor, LookupError, TrackLayout, SECTOR_SAMPLES};

/// 除外の単位（パリティ stride の半分 = 10 セクタ）
const STRIDE_SAMPLES: u64 = 10 * SECTOR_SAMPLES;
/// ディスクと先頭トラックで除外する頭のサンプル数
pub const HEAD_SKIP_SAMPLES: u64 = STRIDE_SAMPLES;

/// ディスクと末尾トラックで除外する尻のサンプル数。総サンプル数の端数を吸収する
pub fn tail_skip_samples(total_samples: u64) -> u64 {
    STRIDE_SAMPLES + total_samples % STRIDE_SAMPLES
}

/// GF(2) 上の 32×32 行列とベクトルの積（zlib `gf2_matrix_times`）
fn gf2_matrix_times(mat: &[u32; 32], mut vec: u32) -> u32 {
    let mut sum = 0;
    let mut i = 0;
    while vec != 0 {
        if vec & 1 != 0 {
            sum ^= mat[i];
        }
        vec >>= 1;
        i += 1;
    }
    sum
}

fn gf2_matrix_square(square: &mut [u32; 32], mat: &[u32; 32]) {
    for n in 0..32 {
        square[n] = gf2_matrix_times(mat, mat[n]);
    }
}

/// `len2` バイトぶんの零送りを表す行列。同じ長さで何度も combine するときに前計算しておく
/// （オフセット探索では 1 トラックにつき 5879 回呼ぶ）
#[derive(Debug, Clone)]
pub struct Crc32Combiner {
    /// `None` は len2 = 0（恒等）
    mat: Option<[u32; 32]>,
}

impl Crc32Combiner {
    pub fn new(mut len2: u64) -> Self {
        if len2 == 0 {
            return Self { mat: None };
        }
        // odd = 1 バイト（8 bit）ぶんの零ビット送りを表す行列、even はその 2 乗（zlib と同じ）
        let mut even = [0u32; 32];
        let mut odd = [0u32; 32];
        odd[0] = 0xedb8_8320;
        let mut row = 1u32;
        for entry in odd.iter_mut().skip(1) {
            *entry = row;
            row <<= 1;
        }
        gf2_matrix_square(&mut even, &odd);
        gf2_matrix_square(&mut odd, &even);
        // 結果の行列 = Π（len2 の立っているビットに対応する 2^k バイト送り）
        let mut result: Option<[u32; 32]> = None;
        loop {
            gf2_matrix_square(&mut even, &odd);
            if len2 & 1 != 0 {
                result = Some(Self::multiply(result.as_ref(), &even));
            }
            len2 >>= 1;
            if len2 == 0 {
                break;
            }
            gf2_matrix_square(&mut odd, &even);
            if len2 & 1 != 0 {
                result = Some(Self::multiply(result.as_ref(), &odd));
            }
            len2 >>= 1;
            if len2 == 0 {
                break;
            }
        }
        Self { mat: result }
    }

    /// 行列の積（`acc` が無ければ `m` そのもの）。列ベクトル基底で `m ∘ acc`
    fn multiply(acc: Option<&[u32; 32]>, m: &[u32; 32]) -> [u32; 32] {
        match acc {
            None => *m,
            Some(a) => {
                let mut out = [0u32; 32];
                for (o, &col) in out.iter_mut().zip(a.iter()) {
                    *o = gf2_matrix_times(m, col);
                }
                out
            }
        }
    }

    /// crc(A‖B) = combine(crc(A), crc(B))。B の長さは `new` で決めたもの
    pub fn combine(&self, crc1: u32, crc2: u32) -> u32 {
        match &self.mat {
            None => crc1,
            Some(m) => gf2_matrix_times(m, crc1) ^ crc2,
        }
    }
}

/// zlib の `crc32_combine`: `crc1` を `len2` バイトぶん進めて `crc2` と合成する。
/// crc(A‖B) = combine(crc(A), crc(B), |B|)。XOR は自己逆元なので
/// crc(B) = combine(crc(A), crc(A‖B), |B|) で部分列の CRC も取れる（[`crate::cd::crctable`]）
pub fn crc32_combine(crc1: u32, crc2: u32, len2: u64) -> u32 {
    Crc32Combiner::new(len2).combine(crc1, crc2)
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

// ---------------------------------------------------------------- DB の照会

/// CTDB の 1 エントリ（`lookup2.php` の `<entry …/>`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtdbEntry {
    pub id: i64,
    /// 同じ結果を提出した人数
    pub confidence: u32,
    /// ディスク CRC32
    pub crc32: u32,
    /// トラックごとの CRC32。エントリの TOC の音声トラック数ぶん
    pub track_crcs: Vec<u32>,
    /// CUETools 形式の TOC 文字列（[`Toc::ctdb_toc`] と同じ形）
    pub toc: String,
    pub npar: u32,
    /// パリティの stride（16 bit 単位ではなくサンプル数で返る。既定 5880）
    pub stride: u32,
    /// パリティデータの URL（あれば修復に使える。P2-7）
    pub has_parity: Option<String>,
    /// シンドローム / パリティ（base64 のまま。P2-7 で解釈する）
    pub syndrome: Option<String>,
    pub parity: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CtdbParseError {
    #[error("XML が壊れている: {0}")]
    Xml(String),
    #[error("entry の属性 {attr} が不正: {value}")]
    Attribute { attr: String, value: String },
    #[error("entry に属性 {0} が無い")]
    Missing(&'static str),
}

fn parse_hex_u32(attr: &str, value: &str) -> Result<u32, CtdbParseError> {
    u32::from_str_radix(value.trim(), 16).map_err(|_| CtdbParseError::Attribute {
        attr: attr.to_owned(),
        value: value.to_owned(),
    })
}

fn parse_dec<T: std::str::FromStr>(attr: &str, value: &str) -> Result<T, CtdbParseError> {
    value.trim().parse().map_err(|_| CtdbParseError::Attribute {
        attr: attr.to_owned(),
        value: value.to_owned(),
    })
}

/// `lookup2.php` の応答（XML）を解釈する。`<entry>` 以外（`<metadata>` 等）は読み飛ばす
pub fn parse_response(xml: &str) -> Result<Vec<CtdbEntry>, CtdbParseError> {
    let mut reader = Reader::from_str(xml);
    let mut entries = Vec::new();
    loop {
        let event = reader
            .read_event()
            .map_err(|e| CtdbParseError::Xml(e.to_string()))?;
        let element = match &event {
            Event::Start(e) | Event::Empty(e) => e,
            Event::Eof => break,
            _ => continue,
        };
        if element.local_name().as_ref() != "entry" {
            continue;
        }
        let mut id = None;
        let mut confidence = None;
        let mut crc32 = None;
        let mut track_crcs = None;
        let mut toc = None;
        let mut npar = None;
        let mut stride = None;
        let mut has_parity = None;
        let mut syndrome = None;
        let mut parity = None;
        for attr in element.attributes() {
            let attr = attr.map_err(|e| CtdbParseError::Xml(e.to_string()))?;
            let key = attr.key.local_name().as_ref().to_owned();
            let value = attr
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map_err(|e| CtdbParseError::Xml(e.to_string()))?
                .into_owned();
            match key.as_str() {
                "id" => id = Some(parse_dec::<i64>(&key, &value)?),
                "confidence" => confidence = Some(parse_dec::<u32>(&key, &value)?),
                "crc32" => crc32 = Some(parse_hex_u32(&key, &value)?),
                "trackcrcs" => {
                    track_crcs = Some(
                        value
                            .split_whitespace()
                            .map(|v| parse_hex_u32(&key, v))
                            .collect::<Result<Vec<u32>, _>>()?,
                    )
                }
                "toc" => toc = Some(value),
                "npar" => npar = Some(parse_dec::<u32>(&key, &value)?),
                "stride" => stride = Some(parse_dec::<u32>(&key, &value)?),
                "hasparity" => has_parity = Some(value),
                "syndrome" => syndrome = Some(value),
                "parity" => parity = Some(value),
                _ => {}
            }
        }
        entries.push(CtdbEntry {
            id: id.ok_or(CtdbParseError::Missing("id"))?,
            confidence: confidence.ok_or(CtdbParseError::Missing("confidence"))?,
            crc32: crc32.ok_or(CtdbParseError::Missing("crc32"))?,
            track_crcs: track_crcs.unwrap_or_default(),
            toc: toc.ok_or(CtdbParseError::Missing("toc"))?,
            npar: npar.unwrap_or(0),
            stride: stride.unwrap_or(0),
            has_parity,
            syndrome,
            parity,
        });
    }
    Ok(entries)
}

/// CTDB の照会。`endpoint` は `http://db.cuetools.net/lookup2.php`（設定で差し替え可）
#[derive(Debug, Clone)]
pub struct CtdbClient {
    endpoint: String,
    http: reqwest::Client,
}

impl CtdbClient {
    pub fn new(endpoint: impl Into<String>, user_agent: &str) -> Result<Self, LookupError> {
        Ok(Self {
            endpoint: endpoint.into(),
            http: super::http_client(user_agent)?,
        })
    }

    /// TOC で照会する（`fuzzy=1`: プレス違いのエントリも返る。TOC は各エントリの `toc` で見分ける）
    pub async fn lookup(&self, toc: &Toc) -> Result<Vec<CtdbEntry>, LookupError> {
        let resp = self
            .http
            .get(&self.endpoint)
            .query(&[
                ("version", "3"),
                ("ctdb", "1"),
                ("fuzzy", "1"),
                ("toc", toc.ctdb_toc().as_str()),
            ])
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(LookupError::Status(status.as_u16()));
        }
        let text = resp.text().await?;
        parse_response(&text).map_err(|e| LookupError::Parse(e.to_string()))
    }
}

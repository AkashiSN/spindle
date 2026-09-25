//! MP4 の音声トラックの edit list（`elst`）の読み取り（D-89 追記）。
//!
//! symphonia 0.6.1 の MP4 読みは edit list を読まないが、ffmpeg は edit list に従って先頭を削り、終端以降で
//! 始まるパケットを捨てる。ALAC のプロセス内デコード（[`crate::media::fingerprint::FrameLimit`]）の出力範囲を
//! ffmpeg に揃えるためにこれを読む。
//!
//! 読むのはボックスの見出しと `moov` だけで、`mdat` は読み飛ばす。MP4 でない（先頭が `ftyp` でない）・
//! 壊れている・音声トラックが無いときは `None`。

use std::io::{Read, Seek, SeekFrom};

/// `moov` をこれより大きく読まない（壊れたサイズで巨大な確保をしない）
const MAX_MOOV: u64 = 64 << 20;

/// 最初の音声トラック（`hdlr` が `soun`）の時間軸と edit list
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioEdit {
    /// `mvhd` の timescale（`elst` の segment_duration の単位）
    pub movie_timescale: u32,
    /// `mdhd` の timescale（`elst` の media_time とトラックの宣言長の単位）
    pub media_timescale: u32,
    /// `elst` の項。`edts` / `elst` が無ければ空
    pub entries: Vec<EditEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditEntry {
    /// movie timescale での長さ
    pub segment_duration: u64,
    /// media timescale での開始位置。-1 は空の編集
    pub media_time: i64,
    /// 再生速度（16.16 固定小数の整数部と小数部）
    pub rate: (i16, i16),
}

/// ffmpeg（5.1）が出力する範囲（media timescale）。`start` から出し、`end` 以降で始まるパケットは捨てる
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditWindow {
    pub start: u64,
    pub end: i64,
}

impl AudioEdit {
    /// edit list が「1 区間・media_time ≥ 0・等速」の単純な形なら、ffmpeg が出力する範囲を返す。
    /// 区間の長さは movie timescale から media timescale へ換算し、ffmpeg（`av_rescale`）と同じく
    /// 四捨五入する（端数 0.5 で切り上げ、0.4 で切り捨てることを ffmpeg 5.1 で確かめた）。
    /// edit list が無い・単純な形でない・長さ 0 の区間なら `None`（何も削らない）
    pub fn window(&self) -> Option<EditWindow> {
        let [e] = self.entries.as_slice() else {
            return None;
        };
        if e.media_time < 0
            || e.rate != (1, 0)
            || self.movie_timescale == 0
            || e.segment_duration == 0
        {
            return None;
        }
        let movie = u128::from(self.movie_timescale);
        let dur = (u128::from(e.segment_duration) * u128::from(self.media_timescale) * 2 + movie)
            / (2 * movie);
        let end = i64::try_from(dur).ok()?.checked_add(e.media_time)?;
        Some(EditWindow {
            start: u64::try_from(e.media_time).ok()?,
            end,
        })
    }
}

/// `r` の先頭から MP4 を読み、最初の音声トラックの edit list を返す。読み終えた位置は不定なので、
/// 呼び出し側が巻き戻すこと
pub fn read_audio_edit<R: Read + Seek>(r: &mut R) -> Option<AudioEdit> {
    let len = r.seek(SeekFrom::End(0)).ok()?;
    r.seek(SeekFrom::Start(0)).ok()?;
    let mut pos = 0u64;
    let mut first = true;
    while pos + 8 <= len {
        r.seek(SeekFrom::Start(pos)).ok()?;
        let (typ, hdr, size) = read_header(r, len - pos)?;
        if first && &typ != b"ftyp" {
            return None;
        }
        first = false;
        if &typ == b"moov" {
            let body = size - hdr;
            if body > MAX_MOOV {
                return None;
            }
            let mut buf = vec![0u8; usize::try_from(body).ok()?];
            r.read_exact(&mut buf).ok()?;
            return parse_moov(&buf);
        }
        pos += size;
    }
    None
}

/// ボックスの見出し（型, 見出しの長さ, ボックス全体の長さ）。`avail` は親の残り
fn read_header<R: Read>(r: &mut R, avail: u64) -> Option<([u8; 4], u64, u64)> {
    let mut h = [0u8; 8];
    r.read_exact(&mut h).ok()?;
    let typ: [u8; 4] = h[4..8].try_into().ok()?;
    let (hdr, size) = match u32::from_be_bytes(h[..4].try_into().ok()?) {
        0 => (8, avail),
        1 => {
            let mut ext = [0u8; 8];
            r.read_exact(&mut ext).ok()?;
            (16, u64::from_be_bytes(ext))
        }
        s => (8, u64::from(s)),
    };
    (size >= hdr && size <= avail).then_some((typ, hdr, size))
}

/// `data` 直下のボックスを (型, 本体) で列挙する。壊れたところで打ち切る
fn children(data: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut rest = data;
    while rest.len() >= 8 {
        let mut cur = rest;
        let Some((typ, hdr, size)) = read_header(&mut cur, rest.len() as u64) else {
            break;
        };
        let (hdr, size) = (hdr as usize, size as usize);
        out.push((typ, &rest[hdr..size]));
        rest = &rest[size..];
    }
    out
}

fn child<'a>(data: &'a [u8], typ: &[u8; 4]) -> Option<&'a [u8]> {
    children(data)
        .into_iter()
        .find_map(|(t, body)| (&t == typ).then_some(body))
}

fn be_u32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

fn be_u64(d: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(d.get(at..at + 8)?.try_into().ok()?))
}

fn be_i16(d: &[u8], at: usize) -> Option<i16> {
    Some(i16::from_be_bytes(d.get(at..at + 2)?.try_into().ok()?))
}

/// `mvhd` / `mdhd` の timescale（version 0 / 1 とも full box の後ろ）
fn timescale(full_box: &[u8]) -> Option<u32> {
    match full_box.first()? {
        0 => be_u32(full_box, 12),
        1 => be_u32(full_box, 20),
        _ => None,
    }
}

fn parse_moov(moov: &[u8]) -> Option<AudioEdit> {
    let movie_timescale = timescale(child(moov, b"mvhd")?)?;
    for (typ, trak) in children(moov) {
        if &typ != b"trak" {
            continue;
        }
        let Some(mdia) = child(trak, b"mdia") else {
            continue;
        };
        let is_audio = child(mdia, b"hdlr").and_then(|h| h.get(8..12)) == Some(b"soun");
        if !is_audio {
            continue;
        }
        let media_timescale = timescale(child(mdia, b"mdhd")?)?;
        let entries = match child(trak, b"edts").and_then(|e| child(e, b"elst")) {
            Some(elst) => parse_elst(elst)?,
            None => Vec::new(),
        };
        return Some(AudioEdit {
            movie_timescale,
            media_timescale,
            entries,
        });
    }
    None
}

fn parse_elst(elst: &[u8]) -> Option<Vec<EditEntry>> {
    let version = *elst.first()?;
    let count = be_u32(elst, 4)? as usize;
    let width = if version == 1 { 20 } else { 12 };
    if elst.len() < 8 + count.checked_mul(width)? {
        return None;
    }
    (0..count)
        .map(|k| {
            let at = 8 + k * width;
            let (segment_duration, media_time, rate_at) = if version == 1 {
                (be_u64(elst, at)?, be_u64(elst, at + 8)? as i64, at + 16)
            } else {
                (
                    u64::from(be_u32(elst, at)?),
                    i64::from(be_u32(elst, at + 4)? as i32),
                    at + 8,
                )
            };
            Some(EditEntry {
                segment_duration,
                media_time,
                rate: (be_i16(elst, rate_at)?, be_i16(elst, rate_at + 2)?),
            })
        })
        .collect()
}

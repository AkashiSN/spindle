//! `adb track-devices -l` の出力の解析（P5-3b、D-98）。
//!
//! 出力は「16 進 4 桁の長さ + 本文」のフレームの並び。本文は接続中の端末 1 台 1 行で、
//! `-l` 付きは `<serial><空白…><state> key:value …`、無しは `<serial>\t<state>`。
//! 抜けると `offline` の後に空のフレーム（`0000`）、挿すと `offline` → `authorizing` → `device`（実機で確認）

use super::quote::valid_serial;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedDevice {
    pub serial: String,
    /// `device` / `offline` / `unauthorized` / `authorizing` など（adb の語のまま）
    pub state: String,
    /// `model:` の値（`-l` のときだけ）
    pub model: Option<String>,
}

/// 1 フレームの本文を読む。シリアルが不正な行は捨てる（端末へ渡す文字列の allowlist と揃える）
pub fn parse_frame(body: &str) -> Vec<TrackedDevice> {
    body.lines()
        .filter_map(|line| {
            // 短い形式はタブ区切り（シリアルに空白が混ざる行を「不正」として捨てるため、空白では割らない）
            let mut it = line.split_whitespace();
            let serial = if line.contains('\t') {
                line.split('\t').next()?.trim()
            } else {
                it.next()?
            };
            if line.contains('\t') {
                it = line.split_once('\t')?.1.split_whitespace();
            }
            let state = it.next()?;
            if !valid_serial(serial) {
                return None;
            }
            let model = it
                .filter_map(|kv| kv.strip_prefix("model:"))
                .next()
                .map(str::to_owned);
            Some(TrackedDevice {
                serial: serial.to_owned(),
                state: state.to_owned(),
                model,
            })
        })
        .collect()
}

#[derive(Debug, thiserror::Error)]
#[error("track-devices の出力を読めない: {0}")]
pub struct FrameError(String);

/// フレームの切れ目を越えて読むための状態
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    /// 読んだバイト列を足し、揃ったフレームを順に返す
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<TrackedDevice>>, FrameError> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            if self.buf.len() < 4 {
                return Ok(out);
            }
            let head = std::str::from_utf8(&self.buf[..4])
                .ok()
                .and_then(|h| usize::from_str_radix(h, 16).ok())
                .ok_or_else(|| FrameError(String::from_utf8_lossy(&self.buf[..4]).into_owned()))?;
            if self.buf.len() < 4 + head {
                return Ok(out);
            }
            let body = String::from_utf8_lossy(&self.buf[4..4 + head]).into_owned();
            self.buf.drain(..4 + head);
            out.push(parse_frame(&body));
        }
    }
}

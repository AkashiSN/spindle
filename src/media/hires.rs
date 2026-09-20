//! 偽ハイレゾ検出の解析（SPEC §7.10、P3-5、D-71）。I/O も DB も知らない。
//!
//! [`HiresSink`] は [`PcmSink`] としてデコード結果を受け、
//! - スペクトル（`sample_rate > 48000` のとき）: チャンネルごとに Hann 窓 [`FRAME`] 点 / ホップ
//!   [`FRAME`] の FFT を取り、線形パワーを累積する。RMS が −70 dBFS 未満のフレームは無音として捨てる
//! - ビット（`bit_depth ≤ 24` のとき）: `round(sample × 2^(bit_depth−1))` を i32 に戻して全サンプルを OR
//!
//! [`HiresSink::finish`] が [`Measurement`]（カットオフ周波数・崖・実効ビット）を出し、
//! [`judge`] が `[hires]` のしきい値で [`Verdict`] を下す。

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use crate::media::decode::{PcmInfo, PcmSink};

/// FFT の点数（= ホップ）
pub const FRAME: usize = 8192;
/// これを超えるレートだけスペクトルを取る
const SPECTRUM_ABOVE_HZ: u32 = 48_000;
/// 無音とみなすフレームの RMS（dBFS）
const SILENCE_DBFS: f64 = -70.0;
/// パワー → dB の下限（−200 dB）
const POWER_FLOOR: f64 = 1e-20;
/// ノイズ床の下限。24 bit の量子化ノイズ（1 ビンあたり約 −186 dB）より上、16 bit のディザの直下
const FLOOR_MIN_DB: f64 = -150.0;
/// ノイズ床を取る帯域（Nyquist 直下の割合）
const FLOOR_BAND: f64 = 0.05;
/// 床からこれだけ上を「信号あり」とする（dB）
const ABOVE_FLOOR_DB: f64 = 10.0;
/// 平滑化の片側幅（オクターブ。±1/6 = 1/3 オクターブ幅）
const SMOOTH_HALF_OCTAVES: f64 = 1.0 / 6.0;
/// エッジを探す範囲（候補から下へ何オクターブか）
const REFINE_OCTAVES: f64 = 1.0 / 3.0;
/// エッジ検出の段差を測る幅（Hz、片側）
const STEP_HZ: f64 = 200.0;
/// 崖を測る幅（Hz、片側）
const CLIFF_HZ: f64 = 1000.0;
/// 崖の飽和（dB）
const CLIFF_MAX_DB: f64 = 200.0;
/// f32 で正確に戻せる最大ビット数
const MAX_EXACT_BITS: u32 = 24;

/// `[hires]` のしきい値
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    pub cutoff_hz: u32,
    pub cliff_db: f64,
    /// これ以下のカットオフは崖に関わらず upsampled（D-71 追記）
    pub hard_cutoff_hz: u32,
}

/// 計測値。計測しなかった側は `None`
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Measurement {
    pub cutoff_hz: Option<u32>,
    pub cliff_db: Option<f64>,
    pub effective_bits: Option<u32>,
}

/// 判定（`decode_error` は解析の外で付く）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    Upsampled,
    Padded,
    Both,
    Inconclusive,
}

/// SPEC §7.10 の優先順位: 計測できたものが無い → inconclusive、both → upsampled → padded →
/// 崖なしの低いカットオフ → inconclusive、それ以外 → ok。
/// upsampled はカットオフが `cutoff_hz` 以下で、崖が `cliff_db` 以上か、カットオフが
/// `hard_cutoff_hz` 以下（44.1 kHz の Nyquist の下。崖の有無を問わない）
pub fn judge(m: &Measurement, t: &Thresholds) -> Verdict {
    if m.cutoff_hz.is_none() && m.effective_bits.is_none() {
        return Verdict::Inconclusive;
    }
    let low = m.cutoff_hz.is_some_and(|c| c <= t.cutoff_hz);
    let hard = m.cutoff_hz.is_some_and(|c| c <= t.hard_cutoff_hz);
    let upsampled = low && (hard || m.cliff_db.is_some_and(|c| c >= t.cliff_db));
    let padded = m.effective_bits.is_some_and(|b| b <= 16);
    match (upsampled, padded) {
        (true, true) => Verdict::Both,
        (true, false) => Verdict::Upsampled,
        (false, true) => Verdict::Padded,
        (false, false) if low => Verdict::Inconclusive,
        (false, false) => Verdict::Ok,
    }
}

/// デコード結果を受けて累積する
pub struct HiresSink {
    bit_depth: Option<u32>,
    /// `2^(bit_depth−1)`。24 bit 超・不明なら None（ビットを見ない）
    scale: Option<f64>,
    or_bits: i32,
    channels: usize,
    sample_rate: u32,
    /// 次に来るサンプルのチャンネル（push の境界はフレーム境界でもチャンネル境界でもない）
    next_channel: usize,
    /// スペクトルを取るときだけ Some
    fft: Option<Arc<dyn Fft<f32>>>,
    window: Vec<f32>,
    /// `|X|² × norm` でフルスケール正弦波が 0 dB
    norm: f64,
    pending: Vec<Vec<f32>>,
    accum: Vec<Vec<f64>>,
    frames: Vec<u64>,
    buf: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
}

impl HiresSink {
    pub fn new(bit_depth: Option<u32>) -> Self {
        let scale = bit_depth
            .filter(|b| (1..=MAX_EXACT_BITS).contains(b))
            .map(|b| 2f64.powi(b as i32 - 1));
        Self {
            bit_depth,
            scale,
            or_bits: 0,
            channels: 0,
            sample_rate: 0,
            next_channel: 0,
            fft: None,
            window: Vec::new(),
            norm: 0.0,
            pending: Vec::new(),
            accum: Vec::new(),
            frames: Vec::new(),
            buf: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// 1 フレームを解析して累積する（無音なら捨てる）
    fn analyze_frame(&mut self, ch: usize) {
        let frame = &self.pending[ch][..FRAME];
        let ms = frame
            .iter()
            .map(|v| f64::from(*v) * f64::from(*v))
            .sum::<f64>()
            / FRAME as f64;
        if 10.0 * ms.max(POWER_FLOOR).log10() < SILENCE_DBFS {
            return;
        }
        let Some(fft) = self.fft.as_ref() else {
            return;
        };
        for (i, (x, w)) in frame.iter().zip(&self.window).enumerate() {
            self.buf[i] = Complex::new(x * w, 0.0);
        }
        fft.process_with_scratch(&mut self.buf, &mut self.scratch);
        let norm = self.norm;
        for (k, acc) in self.accum[ch].iter_mut().enumerate() {
            *acc += f64::from(self.buf[k].norm_sqr()) * norm;
        }
        self.frames[ch] += 1;
    }

    /// チャンネルごとの有音フレーム数（スペクトルを取らないときは空）。チャンネルの割り当てが
    /// push の境界でずれていないことをテストで確かめるために公開する
    pub fn voiced_frames(&self) -> &[u64] {
        &self.frames
    }

    /// 累積を計測値にする
    pub fn finish(self) -> Measurement {
        let effective_bits = match (self.scale, self.bit_depth) {
            (Some(_), Some(bits)) if self.or_bits != 0 => {
                Some(bits.saturating_sub(self.or_bits.trailing_zeros()).max(1))
            }
            _ => None,
        };
        let mut best: Option<(u32, Option<f64>)> = None;
        if self.fft.is_some() {
            for ch in 0..self.channels {
                if self.frames[ch] == 0 {
                    continue;
                }
                let n = self.frames[ch] as f64;
                let mean: Vec<f64> = self.accum[ch].iter().map(|v| v / n).collect();
                let (cutoff, cliff) = analyze_spectrum(&mean, self.sample_rate);
                if best.is_none_or(|(c, _)| cutoff > c) {
                    best = Some((cutoff, cliff));
                }
            }
        }
        Measurement {
            cutoff_hz: best.map(|(c, _)| c),
            cliff_db: best.and_then(|(_, cl)| cl),
            effective_bits,
        }
    }
}

impl PcmSink for HiresSink {
    fn start(&mut self, info: &PcmInfo) -> anyhow::Result<()> {
        self.channels = info.channels as usize;
        self.sample_rate = info.sample_rate;
        if self.channels == 0 {
            anyhow::bail!("チャンネル数が 0");
        }
        if info.sample_rate > SPECTRUM_ABOVE_HZ {
            let fft = FftPlanner::<f32>::new().plan_fft_forward(FRAME);
            self.scratch = vec![Complex::new(0.0, 0.0); fft.get_inplace_scratch_len()];
            self.buf = vec![Complex::new(0.0, 0.0); FRAME];
            // Hann 窓。正規化はフルスケール正弦波が 0 dB: |X|² × (2 / Σw)²
            self.window = (0..FRAME)
                .map(|i| {
                    let t = i as f64 / FRAME as f64;
                    (0.5 - 0.5 * (std::f64::consts::TAU * t).cos()) as f32
                })
                .collect();
            let sum: f64 = self.window.iter().map(|w| f64::from(*w)).sum();
            self.norm = (2.0 / sum).powi(2);
            self.pending = vec![Vec::with_capacity(FRAME * 2); self.channels];
            self.accum = vec![vec![0.0; FRAME / 2 + 1]; self.channels];
            self.frames = vec![0; self.channels];
            self.fft = Some(fft);
        }
        Ok(())
    }

    fn push(&mut self, interleaved: &[f32]) -> anyhow::Result<()> {
        if self.channels == 0 {
            anyhow::bail!("start の前に push された");
        }
        if let Some(scale) = self.scale {
            for v in interleaved {
                // 24 bit までは f32 で正確（仮数 24 bit）。round で元の整数に戻る
                let q = (f64::from(*v) * scale).round();
                self.or_bits |= q as i32;
            }
        }
        if self.fft.is_some() {
            let mut ch = self.next_channel;
            for v in interleaved {
                self.pending[ch].push(*v);
                ch += 1;
                if ch == self.channels {
                    ch = 0;
                }
            }
            self.next_channel = ch;
            for ch in 0..self.channels {
                while self.pending[ch].len() >= FRAME {
                    self.analyze_frame(ch);
                    self.pending[ch].drain(..FRAME);
                }
            }
        }
        Ok(())
    }
}

fn db(power: f64) -> f64 {
    10.0 * power.max(POWER_FLOOR).log10()
}

fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// 平均線形パワー（未平滑、`FRAME/2 + 1` ビン）から（カットオフ Hz、崖 dB）を出す。境界の
/// テストのために公開する（通常は [`HiresSink::finish`] が呼ぶ）。
/// 1. 1/3 オクターブ平滑化 S で候補 f_s（床 + 10 dB を上回る最高ビン）。無ければ Nyquist
/// 2. `[f_s / 2^(1/3), f_s]` で 200 Hz 幅の dB 平均の段差が最大のビンをエッジにする
/// 3. 崖 = エッジ前後 1 kHz の平均線形パワーの比（帯域は [0, Nyquist] で切り、空なら None）
pub fn analyze_spectrum(p: &[f64], sample_rate: u32) -> (u32, Option<f64>) {
    let n = p.len();
    let bin_hz = f64::from(sample_rate) / FRAME as f64;
    let nyquist = f64::from(sample_rate) / 2.0;
    let p_db: Vec<f64> = p.iter().map(|v| db(*v)).collect();

    // S[k]: ±1/6 オクターブの線形平均を dB に
    let mut prefix = vec![0.0; n + 1];
    for k in 0..n {
        prefix[k + 1] = prefix[k] + p[k];
    }
    let ratio = 2f64.powf(SMOOTH_HALF_OCTAVES);
    let s_db: Vec<f64> = (0..n)
        .map(|k| {
            let hi = ((k as f64 * ratio).floor() as usize).min(n - 1);
            let lo = ((k as f64 / ratio).ceil() as usize).min(hi);
            db((prefix[hi + 1] - prefix[lo]) / (hi - lo + 1) as f64)
        })
        .collect();

    // ノイズ床: Nyquist 直下 5% の S の中央値（下限あり）
    let top = (((n - 1) as f64) * (1.0 - FLOOR_BAND)).ceil() as usize;
    let mut band: Vec<f64> = s_db[top.min(n - 1)..].to_vec();
    band.sort_by(|a, b| a.total_cmp(b));
    let floor_db = band[band.len() / 2].max(FLOOR_MIN_DB);
    let threshold = floor_db + ABOVE_FLOOR_DB;

    // 候補。最上位のビンまで信号があれば Nyquist
    let ks = match (0..n).rev().find(|&k| s_db[k] > threshold) {
        Some(k) if k < n - 1 => k,
        _ => return (nyquist.round() as u32, None),
    };

    // エッジ: 段差が最大のビン
    let klo = ((ks as f64) / 2f64.powf(REFINE_OCTAVES)).floor() as usize;
    let m = ((STEP_HZ / bin_hz).round() as usize).max(1);
    let step = |k: usize| -> f64 {
        let pre = &p_db[k.saturating_sub(m)..k];
        let post = &p_db[k..(k + m).min(n)];
        if pre.is_empty() || post.is_empty() {
            f64::NEG_INFINITY
        } else {
            mean(pre) - mean(post)
        }
    };
    let kc = (klo.max(1)..=ks)
        .max_by(|a, b| step(*a).total_cmp(&step(*b)))
        .unwrap_or(ks);
    let cutoff_hz = (kc as f64 * bin_hz).round() as u32;

    // 崖
    let c = ((CLIFF_HZ / bin_hz).round() as usize).max(1);
    let pre = &p[kc.saturating_sub(c)..kc];
    let post = &p[kc..(kc + c).min(n)];
    let cliff = if pre.is_empty() || post.is_empty() {
        None
    } else {
        let (a, b) = (mean(pre), mean(post));
        // 分母（カットオフより上）が完全ゼロなら飽和（SPEC §7.10）
        Some(if b <= 0.0 {
            CLIFF_MAX_DB
        } else {
            (db(a) - db(b)).clamp(-CLIFF_MAX_DB, CLIFF_MAX_DB)
        })
    };
    (cutoff_hz, cliff)
}

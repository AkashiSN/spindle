//! ReplayGain の解析（SPEC §6「ReplayGain の内部表現」、D-22）。
//!
//! - 測定は EBU R128 / ITU-R BS.1770 の積分ラウドネス（`ebur128`、絶対 -70 LUFS + 相対 -10 LU の
//!   ゲート）と true peak（4x オーバーサンプリング）
//! - 内部表現は **RG 2.0 / -18 LUFS 基準の dB 値**。`gain = reference - 測定 LUFS`。フォーマット
//!   ごとの変換（Opus の `R128_*` 等）は書き出し側（P1-2）で行い、ここでは触らない
//! - album gain は構成トラックの状態をまとめてゲートし直した積分ラウドネス
//!   （[`album_loudness`]）。トラック平均ではない。2ch 以外を集計に入れるかは呼び出し側が決める
//!   （SPEC §6: 除外）
//! - 無音（絶対ゲート以下）は積分ラウドネスが `-inf`。gain は「補正なし」の 0 dB に倒す

use ebur128::{EbuR128, Mode};

/// 測定できない・状態が壊れている（`ebur128` の内部エラー。チャンネル数 0 など）
#[derive(Debug, thiserror::Error)]
#[error("ラウドネス解析に失敗: {0}")]
pub struct LoudnessError(#[from] ebur128::Error);

/// 1 トラックの測定結果
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackLoudness {
    /// 積分ラウドネス（LUFS）。無音は `-inf`
    pub lufs: f64,
    /// true peak（フルスケールに対する線形値。1.0 = 0 dBTP）。全チャンネルの最大
    pub peak: f64,
}

impl TrackLoudness {
    /// `reference` LUFS 基準の gain（dB）。測定できない（無音）なら 0
    pub fn gain(&self, reference: f64) -> f64 {
        gain_db(self.lufs, reference)
    }
}

/// `reference` LUFS 基準の gain（dB）。`lufs` が有限でなければ 0（補正なし）
pub fn gain_db(lufs: f64, reference: f64) -> f64 {
    if lufs.is_finite() {
        reference - lufs
    } else {
        0.0
    }
}

/// 1 トラック分のラウドネス計。インターリーブした f32 PCM を任意の長さで流せる
/// （フレーム境界の端数は次の呼び出しまで持ち越す）
pub struct LoudnessMeter {
    state: EbuR128,
    channels: usize,
    /// フレーム境界に満たない端数
    carry: Vec<f32>,
}

impl LoudnessMeter {
    pub fn new(channels: u32, sample_rate: u32) -> Result<Self, LoudnessError> {
        let state = EbuR128::new(channels, sample_rate, Mode::I | Mode::TRUE_PEAK)?;
        Ok(Self {
            state,
            channels: channels as usize,
            carry: Vec::new(),
        })
    }

    pub fn channels(&self) -> u32 {
        self.state.channels()
    }

    pub fn sample_rate(&self) -> u32 {
        self.state.rate()
    }

    /// インターリーブした PCM（-1.0..1.0）を追加する
    pub fn push(&mut self, interleaved: &[f32]) -> Result<(), LoudnessError> {
        let mut data = interleaved;
        if !self.carry.is_empty() {
            // 前回の端数を先頭に足してフレームを揃える
            let need = self.channels - self.carry.len();
            if data.len() < need {
                self.carry.extend_from_slice(data);
                return Ok(());
            }
            self.carry.extend_from_slice(&data[..need]);
            data = &data[need..];
            let carry = std::mem::take(&mut self.carry);
            self.state.add_frames_f32(&carry)?;
        }
        let whole = data.len() - data.len() % self.channels;
        if whole > 0 {
            self.state.add_frames_f32(&data[..whole])?;
        }
        self.carry.extend_from_slice(&data[whole..]);
        Ok(())
    }

    /// ここまでの測定結果。無音は `lufs = -inf`
    pub fn loudness(&self) -> TrackLoudness {
        // Mode::I | TRUE_PEAK で作っているので InvalidMode にはならない。万一のときは無音扱い
        let lufs = self.state.loudness_global().unwrap_or(f64::NEG_INFINITY);
        let peak = (0..self.state.channels())
            .filter_map(|ch| self.state.true_peak(ch).ok())
            .fold(0.0f64, f64::max);
        TrackLoudness { lufs, peak }
    }

    fn state(&self) -> &EbuR128 {
        &self.state
    }
}

/// 複数トラックの状態をまとめてゲートし直した積分ラウドネス（album gain の元。LUFS）。
/// レートやチャンネル数が違う状態を混ぜてもよい。空なら `-inf`
pub fn album_loudness(members: &[&LoudnessMeter]) -> Result<f64, LoudnessError> {
    if members.is_empty() {
        return Ok(f64::NEG_INFINITY);
    }
    Ok(EbuR128::loudness_global_multiple(
        members.iter().map(|m| m.state()),
    )?)
}

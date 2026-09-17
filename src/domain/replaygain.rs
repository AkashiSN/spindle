//! ReplayGain の解析（SPEC §6「ReplayGain の内部表現」、D-22）。
//!
//! - 測定は EBU R128 / ITU-R BS.1770 の積分ラウドネス（`ebur128`、絶対 -70 LUFS + 相対 -10 LU の
//!   ゲート）と true peak（4x オーバーサンプリング）
//! - 内部表現は **RG 2.0 / -18 LUFS 基準の dB 値**。`gain = reference - 測定 LUFS`。フォーマット
//!   ごとの変換（Opus の `R128_*` 等）は [`tag_changes`] で書き出し時に行う（P1-2）
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

// ---------------------------------------------------------------- タグへの変換（P1-2）

use crate::domain::tags::{Codec, TagChange, TagSet};

/// 1 トラック分の値（内部表現）。album 側は集計に入らないトラック（2ch 以外、album 無し）で `None`
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Values {
    pub track_gain: f64,
    pub track_peak: f64,
    pub album_gain: Option<f64>,
    pub album_peak: Option<f64>,
}

/// RG 2.0 の Vorbis Comment 名（FLAC / Ogg。他形式へは lofty が写像する）
pub const RG2_TRACK_GAIN: &str = "REPLAYGAIN_TRACK_GAIN";
pub const RG2_TRACK_PEAK: &str = "REPLAYGAIN_TRACK_PEAK";
pub const RG2_ALBUM_GAIN: &str = "REPLAYGAIN_ALBUM_GAIN";
pub const RG2_ALBUM_PEAK: &str = "REPLAYGAIN_ALBUM_PEAK";
/// Opus の gain（RFC 7845 §5.2.1。Q7.8 固定小数、-23 LUFS 基準。peak は無い）
pub const R128_TRACK_GAIN: &str = "R128_TRACK_GAIN";
pub const R128_ALBUM_GAIN: &str = "R128_ALBUM_GAIN";

/// spindle が書き込み時に所有する（値を置くか消す）キーの全体
pub const RG_KEYS: [&str; 6] = [
    RG2_TRACK_GAIN,
    RG2_TRACK_PEAK,
    RG2_ALBUM_GAIN,
    RG2_ALBUM_PEAK,
    R128_TRACK_GAIN,
    R128_ALBUM_GAIN,
];

/// 内部の gain（`reference` LUFS 基準）を RG 2.0 の -18 LUFS 基準に置き直した dB 値
pub fn rg2_gain_db(gain: f64, reference: f64) -> f64 {
    gain + (-18.0 - reference)
}

/// 内部の gain を Opus の `R128_*` 値へ変換する（SPEC §6）。-23 LUFS 基準の Q7.8 固定小数で、
/// `round((G18 - 5.0) * 256)` を符号付き 16bit に飽和させる（基準が 5 dB 低いので必ず減算）。
/// 測定できない値（NaN）は補正なしの 0
pub fn opus_r128(gain: f64, reference: f64) -> i16 {
    let g23 = gain + (-23.0 - reference);
    if !g23.is_finite() {
        return 0;
    }
    // `as` は飽和変換（NaN は上で除いた）
    (g23 * 256.0).round() as i16
}

/// RG 2.0 の gain 表記（`+0.00 dB`。-0.0 は +0.00 にする）
fn format_gain(db: f64) -> String {
    let db = if db == 0.0 { 0.0 } else { db };
    format!("{db:+.2} dB")
}

/// RG 2.0 の peak 表記（線形値。true peak なので 1.0 を超えうる）
fn format_peak(peak: f64) -> String {
    format!("{peak:.6}")
}

/// `codec` のファイルへ書く RG タグの変更集合。値の無いキー（album を持たない・別形式のキー）は
/// 削除として含める（値だけ書いて他形式のキーや古い album 値を残すと、プレイヤーが古い方を
/// 拾う）。Opus は `R128_*` のみで `REPLAYGAIN_*` を消し、他形式は `REPLAYGAIN_*` のみ
/// （`R128_*` は Opus 専用なので触らない）
pub fn tag_changes(codec: Codec, v: &Values, reference: f64) -> Vec<TagChange> {
    let set = |key: &str, value: Option<String>| TagChange {
        key: key.to_owned(),
        values: value.map(|s| vec![s]),
    };
    match codec {
        Codec::Opus => vec![
            set(
                R128_TRACK_GAIN,
                Some(opus_r128(v.track_gain, reference).to_string()),
            ),
            set(
                R128_ALBUM_GAIN,
                v.album_gain.map(|g| opus_r128(g, reference).to_string()),
            ),
            set(RG2_TRACK_GAIN, None),
            set(RG2_TRACK_PEAK, None),
            set(RG2_ALBUM_GAIN, None),
            set(RG2_ALBUM_PEAK, None),
        ],
        _ => vec![
            set(
                RG2_TRACK_GAIN,
                Some(format_gain(rg2_gain_db(v.track_gain, reference))),
            ),
            set(RG2_TRACK_PEAK, Some(format_peak(v.track_peak))),
            set(
                RG2_ALBUM_GAIN,
                v.album_gain.map(|g| format_gain(rg2_gain_db(g, reference))),
            ),
            set(RG2_ALBUM_PEAK, v.album_peak.map(format_peak)),
        ],
    }
}

/// ファイルのタグ集合が `v` の書き込み結果と一致するか（`rg_written_at` を立ててよいか）。
/// [`tag_changes`] の全キーについて、値を持つキーはその 1 値だけ、消すキーは不在であること
pub fn file_matches(codec: Codec, tags: &TagSet, v: &Values, reference: f64) -> bool {
    tag_changes(codec, v, reference).iter().all(|c| {
        let current: Vec<&str> = tags.values(&c.key).collect();
        match &c.values {
            Some(want) => current.len() == want.len() && current.iter().eq(want.iter()),
            None => current.is_empty(),
        }
    })
}

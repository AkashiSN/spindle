//! ReplayGain の解析（SPEC §6「ReplayGain の内部表現」、docs/TASKS.md P1-1）。
//! 内部表現は RG 2.0 / -18 LUFS 基準の dB。ebur128 の積分ラウドネスと true peak から
//! track / album の値を出す

use spindle::domain::replaygain::{album_loudness, gain_db, LoudnessMeter, TrackLoudness};

/// 1 kHz の正弦波（ステレオ、両チャンネル同じ振幅）。BS.1770 の K 特性は 1 kHz 付近で 0 dB
/// なので、両チャンネルに振幅 `amp` を入れると積分ラウドネスは `20*log10(amp)` LUFS になる
fn stereo_sine(amp: f32, rate: u32, secs: f32) -> Vec<f32> {
    let n = (rate as f32 * secs) as usize;
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n {
        let t = i as f32 / rate as f32;
        let v = (t * 1000.0 * std::f32::consts::TAU).sin() * amp;
        out.push(v);
        out.push(v);
    }
    out
}

fn dbfs(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[test]
fn gain_is_reference_minus_loudness() {
    assert_eq!(gain_db(-18.0, -18.0), 0.0);
    assert_eq!(gain_db(-23.0, -18.0), 5.0);
    assert_eq!(gain_db(-13.0, -18.0), -5.0);
    // 基準を変えても同じ式
    assert_eq!(gain_db(-23.0, -23.0), 0.0);
}

#[test]
fn track_loudness_of_stereo_sine() {
    let mut m = LoudnessMeter::new(2, 48_000).unwrap();
    m.push(&stereo_sine(dbfs(-20.0), 48_000, 5.0)).unwrap();
    let t = m.loudness();
    assert!((t.lufs - -20.0).abs() < 0.2, "lufs = {}", t.lufs);
    assert!((t.peak - 0.1).abs() < 0.005, "peak = {}", t.peak);
    assert!((t.gain(-18.0) - 2.0).abs() < 0.2);
}

#[test]
fn push_accepts_chunks_not_aligned_to_frames() {
    // 奇数長のチャンクを流しても、まとめて流したときと同じ結果になる（フレーム境界で
    // バッファリングしている）
    let pcm = stereo_sine(dbfs(-20.0), 48_000, 3.0);
    let mut whole = LoudnessMeter::new(2, 48_000).unwrap();
    whole.push(&pcm).unwrap();
    let mut chunked = LoudnessMeter::new(2, 48_000).unwrap();
    for c in pcm.chunks(4097) {
        chunked.push(c).unwrap();
    }
    let (a, b) = (whole.loudness(), chunked.loudness());
    assert!((a.lufs - b.lufs).abs() < 1e-6);
    assert!((a.peak - b.peak).abs() < 1e-9);
}

#[test]
fn silence_has_no_gain() {
    // 無音は積分ラウドネスが -inf（絶対ゲート以下）。gain は 0（補正なし）、peak は 0
    let mut m = LoudnessMeter::new(2, 44_100).unwrap();
    m.push(&vec![0.0f32; 44_100 * 2 * 3]).unwrap();
    let t = m.loudness();
    assert!(t.lufs.is_infinite());
    assert_eq!(t.gain(-18.0), 0.0);
    assert_eq!(t.peak, 0.0);
    // 直接構築した値でも同じ
    let t = TrackLoudness {
        lufs: f64::NEG_INFINITY,
        peak: 0.0,
    };
    assert_eq!(t.gain(-18.0), 0.0);
}

#[test]
fn album_loudness_is_gated_power_mean_of_members() {
    // -20 と -14 LUFS の同じ長さの 2 曲。相対ゲートには両方かかる（差 6 LU < 10 LU）ので
    // 電力平均: 10*log10((10^-2 + 10^-1.4) / 2) ≈ -16.04 LUFS
    let mut a = LoudnessMeter::new(2, 48_000).unwrap();
    a.push(&stereo_sine(dbfs(-20.0), 48_000, 4.0)).unwrap();
    let mut b = LoudnessMeter::new(2, 48_000).unwrap();
    b.push(&stereo_sine(dbfs(-14.0), 48_000, 4.0)).unwrap();
    let album = album_loudness(&[&a, &b]).unwrap();
    assert!((album - -16.04).abs() < 0.3, "album = {album}");
    // 1 曲だけなら track と同じ
    let solo = album_loudness(&[&a]).unwrap();
    assert!((solo - a.loudness().lufs).abs() < 1e-6);
}

#[test]
fn album_of_mixed_rates_is_accepted() {
    // 44.1k と 48k の曲が同じ album にあっても集計できる（状態ごとにレートを持つ）
    let mut a = LoudnessMeter::new(2, 44_100).unwrap();
    a.push(&stereo_sine(dbfs(-20.0), 44_100, 3.0)).unwrap();
    let mut b = LoudnessMeter::new(2, 48_000).unwrap();
    b.push(&stereo_sine(dbfs(-20.0), 48_000, 3.0)).unwrap();
    let album = album_loudness(&[&a, &b]).unwrap();
    assert!((album - -20.0).abs() < 0.2, "album = {album}");
}

#[test]
fn rejects_zero_channels() {
    assert!(LoudnessMeter::new(0, 44_100).is_err());
}

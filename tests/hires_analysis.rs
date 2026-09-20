//! 偽ハイレゾ検出の解析（SPEC §7.10、P3-5、D-71）。合成信号で計測値と判定を固定する。
//! 信号は逆 FFT で作る（ビンごとに振幅を与え位相は乱数。共役対称で実信号）。
//! 96 kHz、2^18 サンプル（約 2.7 秒 = 32 フレーム）

use std::f64::consts::TAU;

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;
use spindle::media::decode::{PcmInfo, PcmSink};
use spindle::media::hires::{
    analyze_spectrum, judge, HiresSink, Measurement, Thresholds, Verdict, FRAME,
};

const RATE: u32 = 96_000;
const LEN: usize = 1 << 18;
const THRESHOLDS: Thresholds = Thresholds {
    cutoff_hz: 25_000,
    cliff_db: 10.0,
    hard_cutoff_hz: 22_500,
};

struct Xorshift(u64);

impl Xorshift {
    fn next_f64(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// ビンの周波数 f（Hz）に振幅 `mag(f)`（0 なら無し）を置いた実信号。ピークが 0.5 になるよう正規化
fn synth(mag: impl Fn(f64) -> f64, seed: u64) -> Vec<f32> {
    let mut planner = FftPlanner::<f64>::new();
    let ifft = planner.plan_fft_inverse(LEN);
    let mut buf = vec![Complex::new(0.0, 0.0); LEN];
    let mut rng = Xorshift(seed | 1);
    for k in 1..LEN / 2 {
        let f = k as f64 * RATE as f64 / LEN as f64;
        let a = mag(f);
        if a <= 0.0 {
            continue;
        }
        let c = Complex::from_polar(a, rng.next_f64() * TAU);
        buf[k] = c;
        buf[LEN - k] = c.conj();
    }
    ifft.process(&mut buf);
    let peak = buf.iter().map(|c| c.re.abs()).fold(0.0_f64, f64::max);
    buf.iter().map(|c| (c.re / peak * 0.5) as f32).collect()
}

/// 帯域 `edge` Hz までフラット、それより上は `outside`（0 なら完全ゼロ）
fn brickwall(edge: f64, outside: f64) -> impl Fn(f64) -> f64 {
    move |f| if f <= edge { 1.0 } else { outside }
}

fn analyze(channels: &[Vec<f32>], rate: u32, bit_depth: Option<u32>) -> Measurement {
    let n = channels[0].len();
    let mut sink = HiresSink::new(bit_depth);
    sink.start(&PcmInfo {
        channels: channels.len() as u32,
        sample_rate: rate,
    })
    .unwrap();
    // 4096 フレームずつ、インターリーブして流す（push の境界がフレーム境界と揃わないこと）
    let mut i = 0;
    while i < n {
        let end = (i + 4096).min(n);
        let mut chunk = Vec::with_capacity((end - i) * channels.len());
        for t in i..end {
            for ch in channels {
                chunk.push(ch[t]);
            }
        }
        sink.push(&chunk).unwrap();
        i = end;
    }
    sink.finish()
}

/// 16 bit に量子化してから 24 bit の値域に置く（下位 8 bit がゼロ）
fn quantize16(x: &[f32]) -> Vec<f32> {
    x.iter()
        .map(|v| ((f64::from(*v) * 32_767.0).round() * 256.0 / 8_388_608.0) as f32)
        .collect()
}

#[test]
fn brickwall_at_22050_on_96k_is_upsampled_with_edge_near_the_cutoff() {
    let x = synth(brickwall(22_050.0, 1e-7), 1);
    let m = analyze(&[x], RATE, Some(24));
    let cutoff = m.cutoff_hz.unwrap();
    assert!((21_600..=22_500).contains(&cutoff), "{m:?}");
    assert!(
        m.cliff_db.unwrap() >= 30.0,
        "合成の brickwall は崖が大きい: {m:?}"
    );
    assert_eq!(m.effective_bits, Some(24), "{m:?}");
    assert_eq!(judge(&m, &THRESHOLDS), Verdict::Upsampled);
}

#[test]
fn brickwall_at_24000_from_a_48k_master_is_still_caught_by_the_default_threshold() {
    let x = synth(brickwall(24_000.0, 1e-7), 2);
    let m = analyze(&[x], RATE, Some(24));
    let cutoff = m.cutoff_hz.unwrap();
    assert!((23_500..=24_500).contains(&cutoff), "{m:?}");
    assert_eq!(judge(&m, &THRESHOLDS), Verdict::Upsampled);
}

#[test]
fn completely_zero_out_of_band_gives_finite_values_and_the_same_verdict() {
    let x = synth(brickwall(22_050.0, 0.0), 3);
    let m = analyze(&[x], RATE, Some(24));
    assert!((21_600..=22_500).contains(&m.cutoff_hz.unwrap()), "{m:?}");
    let cliff = m.cliff_db.unwrap();
    assert!(
        cliff.is_finite() && (30.0..=200.0).contains(&cliff),
        "{m:?}"
    );
    assert_eq!(judge(&m, &THRESHOLDS), Verdict::Upsampled);
}

#[test]
fn gentle_rolloff_into_a_noise_floor_is_inconclusive() {
    // 18 kHz から 500 Hz ごとに 3 dB 落ち、-60 dB の白色雑音に沈む（本物の膝に近い形）。
    // カットオフは hard_cutoff_hz（22.5 kHz）より上、cutoff_hz（25 kHz）以下で、崖は 10 dB 未満
    let x = synth(
        |f| {
            let env = if f <= 18_000.0 {
                1.0
            } else {
                10f64.powf(-(f - 18_000.0) / 500.0 * 3.0 / 20.0)
            };
            env + 1e-3
        },
        4,
    );
    let m = analyze(&[x], RATE, Some(24));
    let cutoff = m.cutoff_hz.unwrap();
    assert!(cutoff <= 25_000 && cutoff > 22_500, "{m:?}");
    assert!(m.cliff_db.unwrap() < 10.0, "{m:?}");
    assert_eq!(judge(&m, &THRESHOLDS), Verdict::Inconclusive);
}

#[test]
fn a_cutoff_below_the_44k_nyquist_is_upsampled_even_without_a_cliff() {
    // 「Miss you」型: 15 kHz から緩やかに落ちて 22 kHz 手前で床に沈む。段差は無いが、96 kHz の
    // 録音で 22.5 kHz 以上が空なのは成立しない（実機の観察。D-71 追記）
    let x = synth(
        |f| {
            let env = if f <= 15_000.0 {
                1.0
            } else {
                10f64.powf(-(f - 15_000.0) / 500.0 * 3.0 / 20.0)
            };
            env + 1e-3
        },
        15,
    );
    let m = analyze(&[x], RATE, Some(24));
    let cutoff = m.cutoff_hz.unwrap();
    assert!(cutoff <= 22_500 && cutoff > 19_000, "{m:?}");
    assert!(m.cliff_db.unwrap() < 10.0, "{m:?}");
    assert_eq!(judge(&m, &THRESHOLDS), Verdict::Upsampled);
}

#[test]
fn full_band_content_reaches_nyquist_and_is_ok() {
    let x = synth(|_| 1.0, 5);
    let m = analyze(&[x], RATE, Some(24));
    assert_eq!(m.cutoff_hz, Some(48_000), "{m:?}");
    assert_eq!(m.cliff_db, None);
    assert_eq!(m.effective_bits, Some(24));
    assert_eq!(judge(&m, &THRESHOLDS), Verdict::Ok);
}

#[test]
fn lower_eight_bits_zero_is_padded_and_with_brickwall_is_both() {
    let full = quantize16(&synth(|_| 1.0, 6));
    let m = analyze(&[full], RATE, Some(24));
    assert_eq!(m.effective_bits, Some(16), "{m:?}");
    assert_eq!(judge(&m, &THRESHOLDS), Verdict::Padded);
    let bw = quantize16(&synth(brickwall(22_050.0, 1e-7), 7));
    let m = analyze(&[bw], RATE, Some(24));
    assert_eq!(m.effective_bits, Some(16), "{m:?}");
    assert_eq!(judge(&m, &THRESHOLDS), Verdict::Both);
}

#[test]
fn bits_only_target_at_44k_measures_no_spectrum() {
    // sample_rate ≤ 48000 ならスペクトルは取らない（cutoff / cliff は NULL）。ビットだけで判定
    let x = quantize16(&synth(|_| 1.0, 8));
    let m = analyze(&[x], 44_100, Some(24));
    assert_eq!((m.cutoff_hz, m.cliff_db), (None, None), "{m:?}");
    assert_eq!(m.effective_bits, Some(16));
    assert_eq!(judge(&m, &THRESHOLDS), Verdict::Padded);
    let y = synth(|_| 1.0, 9);
    let m = analyze(&[y], 44_100, Some(24));
    assert_eq!(m.effective_bits, Some(24));
    assert_eq!(judge(&m, &THRESHOLDS), Verdict::Ok);
}

#[test]
fn silence_and_unmeasurable_inputs_are_inconclusive_with_null_measurements() {
    let zeros = vec![0.0f32; LEN];
    let m = analyze(std::slice::from_ref(&zeros), RATE, Some(24));
    assert_eq!(m, Measurement::default(), "{m:?}");
    assert_eq!(judge(&m, &THRESHOLDS), Verdict::Inconclusive);
    // 32 bit は f32 で正確に戻せないのでビットは NULL。≤ 48 kHz ならスペクトルも無い
    let m = analyze(&[synth(|_| 1.0, 10)], 48_000, Some(32));
    assert_eq!(m, Measurement::default(), "{m:?}");
    assert_eq!(judge(&m, &THRESHOLDS), Verdict::Inconclusive);
    // bit_depth 不明も同じ
    let m = analyze(&[synth(|_| 1.0, 11)], 48_000, None);
    assert_eq!(m.effective_bits, None);
    // -70 dBFS 未満のフレームは無音扱い（RMS ≈ -90 dBFS の微小信号）
    let tiny: Vec<f32> = synth(|_| 1.0, 12).iter().map(|v| v * 1e-4).collect();
    let m = analyze(&[tiny], RATE, Some(24));
    assert_eq!(m.cutoff_hz, None, "{m:?}");
}

#[test]
fn multichannel_takes_the_channel_with_the_highest_cutoff_and_its_cliff() {
    let l = synth(|_| 1.0, 13);
    let r = synth(brickwall(22_050.0, 1e-7), 14);
    let m = analyze(&[l.clone(), r.clone()], RATE, Some(24));
    assert_eq!(m.cutoff_hz, Some(48_000), "{m:?}");
    assert_eq!(m.cliff_db, None, "L の値を対で採る");
    assert_eq!(judge(&m, &THRESHOLDS), Verdict::Ok);
    // 順序を入れ替えても同じ
    let m = analyze(&[r, l], RATE, Some(24));
    assert_eq!(m.cutoff_hz, Some(48_000), "{m:?}");
    assert_eq!(m.cliff_db, None);
}

#[test]
fn judge_follows_the_documented_precedence() {
    let t = THRESHOLDS;
    let m = |c: Option<u32>, cl: Option<f64>, b: Option<u32>| Measurement {
        cutoff_hz: c,
        cliff_db: cl,
        effective_bits: b,
    };
    assert_eq!(judge(&m(None, None, None), &t), Verdict::Inconclusive);
    assert_eq!(
        judge(&m(Some(22_050), Some(50.0), Some(16)), &t),
        Verdict::Both
    );
    assert_eq!(
        judge(&m(Some(22_050), Some(50.0), Some(24)), &t),
        Verdict::Upsampled
    );
    assert_eq!(
        judge(&m(Some(22_050), Some(50.0), None), &t),
        Verdict::Upsampled
    );
    assert_eq!(
        judge(&m(Some(23_000), Some(4.0), Some(16)), &t),
        Verdict::Padded,
        "崖なし + padded は padded"
    );
    assert_eq!(judge(&m(None, None, Some(16)), &t), Verdict::Padded);
    assert_eq!(
        judge(&m(Some(23_000), Some(4.0), Some(24)), &t),
        Verdict::Inconclusive
    );
    assert_eq!(
        judge(&m(Some(23_000), None, Some(24)), &t),
        Verdict::Inconclusive
    );
    assert_eq!(
        judge(&m(Some(23_000), Some(10.0), Some(24)), &t),
        Verdict::Upsampled,
        "崖はしきい値ちょうどを含む"
    );
    assert_eq!(
        judge(&m(Some(22_500), None, Some(24)), &t),
        Verdict::Upsampled,
        "hard_cutoff 以下は崖なしでも"
    );
    assert_eq!(
        judge(&m(Some(22_050), Some(4.0), Some(16)), &t),
        Verdict::Both
    );
    assert_eq!(
        judge(&m(Some(25_000), Some(50.0), Some(24)), &t),
        Verdict::Upsampled,
        "しきい値ちょうどは含む"
    );
    assert_eq!(
        judge(&m(Some(25_001), Some(50.0), Some(24)), &t),
        Verdict::Ok
    );
    assert_eq!(judge(&m(Some(48_000), None, Some(24)), &t), Verdict::Ok);
    assert_eq!(judge(&m(Some(48_000), None, None), &t), Verdict::Ok);
}

/// `chunk` 個ずつ（チャンネル数の倍数でない切れ目で）流し、有音フレーム数と計測値を返す
fn analyze_chunked(channels: &[Vec<f32>], rate: u32, chunk: usize) -> (Vec<u64>, Measurement) {
    let n = channels[0].len();
    let mut all = Vec::with_capacity(n * channels.len());
    for t in 0..n {
        for ch in channels {
            all.push(ch[t]);
        }
    }
    let mut sink = HiresSink::new(Some(24));
    sink.start(&PcmInfo {
        channels: channels.len() as u32,
        sample_rate: rate,
    })
    .unwrap();
    for c in all.chunks(chunk) {
        sink.push(c).unwrap();
    }
    let frames = sink.voiced_frames().to_vec();
    (frames, sink.finish())
}

#[test]
fn channel_phase_survives_push_boundaries_that_are_not_channel_aligned() {
    // ffmpeg 経路の push は任意のサンプル数で切れる（decode.rs は 4 byte 境界しか揃えない）。
    // 3ch を 1001 値ずつ（1001 mod 3 = 2）流しても、チャンネルの割り当てがずれないこと。
    // ずれると無音の ch に有音のサンプルが混ざり、有音フレームが 0 でなくなる
    let full = synth(|_| 1.0, 20);
    let zeros = vec![0.0f32; LEN];
    let frames_per_ch = (LEN / FRAME) as u64;
    let (frames, m) = analyze_chunked(&[full.clone(), zeros.clone(), zeros.clone()], RATE, 1001);
    assert_eq!(frames, vec![frames_per_ch, 0, 0], "{m:?}");
    assert_eq!(m.cutoff_hz, Some(48_000));
    let (frames, _) = analyze_chunked(&[zeros.clone(), zeros.clone(), full.clone()], RATE, 1001);
    assert_eq!(frames, vec![0, 0, frames_per_ch]);
    // 揃った切れ目でも同じ
    let (frames, _) = analyze_chunked(&[zeros.clone(), full, zeros], RATE, 3 * 4096);
    assert_eq!(frames, vec![0, frames_per_ch, 0]);
}

#[test]
fn analyze_spectrum_saturates_the_cliff_when_the_band_above_is_exactly_zero() {
    // 平均パワーを直接与える境界: 1000 ビンまで 1.0、上は完全ゼロ
    let n = FRAME / 2 + 1;
    let bin_hz = RATE as f64 / FRAME as f64;
    let mut p = vec![0.0; n];
    for v in &mut p[..=1000] {
        *v = 1.0;
    }
    let (cutoff, cliff) = analyze_spectrum(&p, RATE);
    let edge = (1001.0 * bin_hz).round() as u32;
    assert!(
        (edge - 30..=edge + 30).contains(&cutoff),
        "{cutoff} vs {edge}"
    );
    assert_eq!(cliff, Some(200.0), "分母 0 は +200 dB に飽和");
    // 上端まで信号があれば Nyquist で崖なし
    let (cutoff, cliff) = analyze_spectrum(&vec![1.0; n], RATE);
    assert_eq!((cutoff, cliff), (48_000, None));
    // 全ゼロ（有音フレームがあっても平均が 0）でも落ちず有限
    let (cutoff, cliff) = analyze_spectrum(&vec![0.0; n], RATE);
    assert_eq!(cutoff, 48_000, "床と同じなので候補が無い");
    assert_eq!(cliff, None);
}

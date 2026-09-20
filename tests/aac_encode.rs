//! Derived 用の AAC エンコード（SPEC §7.6「aac 系統」、D-75、docs/TASKS.md P4-8）。ffmpeg 1 パスで
//! gain の焼き込みと 48 kHz 上限。ffmpeg が無ければ skip

#![cfg(target_os = "linux")]

mod common;

use std::fs::File;
use std::path::Path;

use tokio_util::sync::CancellationToken;

use spindle::domain::replaygain::{LoudnessMeter, TrackLoudness};
use spindle::domain::tags::read_audio_file;
use spindle::media::decode::{Decoder, PcmInfo, PcmSink};
use spindle::media::encode::AacEncoder;

/// デコードしたサンプルを ebur128 に流す
struct Meter(Option<LoudnessMeter>);

impl PcmSink for Meter {
    fn start(&mut self, info: &PcmInfo) -> anyhow::Result<()> {
        self.0 = Some(LoudnessMeter::new(info.channels, info.sample_rate)?);
        Ok(())
    }
    fn push(&mut self, interleaved: &[f32]) -> anyhow::Result<()> {
        if let Some(m) = &mut self.0 {
            m.push(interleaved)?;
        }
        Ok(())
    }
}

/// `p` の積分ラウドネスと true peak、サンプルレート
async fn measure(ffmpeg: &Path, p: &Path) -> (TrackLoudness, u32) {
    let ext = p.extension().and_then(|e| e.to_str());
    let (info, sink) = Decoder::new(ffmpeg)
        .decode(
            File::open(p).unwrap(),
            ext,
            Meter(None),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    (sink.0.unwrap().loudness(), info.sample_rate)
}

fn close(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol
}

/// 2 秒 2ch の正弦波（`amp_db` dBFS）を `rate` Hz の 24 bit WAV に
fn tone_wav(p: &Path, amp_db: f32, rate: u32) {
    let amp = 10f32.powf(amp_db / 20.0) * 8_388_607.0;
    let n = rate as usize * 2;
    let mut s = Vec::with_capacity(n * 2);
    for i in 0..n {
        let t = i as f32 / rate as f32;
        let v = ((t * 440.0 * std::f32::consts::TAU).sin() * amp) as i32;
        s.push(v);
        s.push(v);
    }
    common::write_wav_ex(p, &s, 24, rate, 2);
}

fn tmp_count(dir: &Path) -> usize {
    std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0)
}

#[tokio::test]
async fn bakes_gain_and_keeps_44k1() {
    let ffmpeg = require_ffmpeg!(common::ffmpeg());
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.wav");
    tone_wav(&src, -20.0, 44_100);
    let (before, _) = measure(&ffmpeg, &src).await;
    let enc = AacEncoder::new(&ffmpeg, 256, dir.path().join("tmp"));
    assert_eq!(enc.bitrate_kbps(), 256);
    for gain in [0.0, -6.0, 4.5] {
        let out = enc
            .encode(
                File::open(&src).unwrap(),
                gain,
                Some(44_100),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let (after, rate) = measure(&ffmpeg, out.guard.path()).await;
        assert!(
            close(after.lufs, before.lufs + gain, 0.5),
            "gain {gain}: {} → {}",
            before.lufs,
            after.lufs
        );
        assert_eq!(rate, 44_100, "44.1 kHz は据え置き");
        let af = read_audio_file(File::open(out.guard.path()).unwrap(), Some("m4a")).unwrap();
        assert_eq!(af.codec.as_str(), "aac");
        assert!(!af.lossless);
        assert!(
            af.tags.items().is_empty(),
            "メタデータは移さない: {:?}",
            af.tags.items()
        );
        drop(out);
    }
    assert_eq!(tmp_count(&dir.path().join("tmp")), 0);
}

#[tokio::test]
async fn downsamples_above_48k_and_keeps_48k() {
    let ffmpeg = require_ffmpeg!(common::ffmpeg());
    let dir = tempfile::tempdir().unwrap();
    let enc = AacEncoder::new(&ffmpeg, 256, dir.path().join("tmp"));
    for (rate, expect) in [(96_000u32, 48_000u32), (48_000, 48_000), (88_200, 48_000)] {
        let src = dir.path().join(format!("src{rate}.wav"));
        tone_wav(&src, -20.0, rate);
        let out = enc
            .encode(
                File::open(&src).unwrap(),
                0.0,
                Some(rate),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let (_, got) = measure(&ffmpeg, out.guard.path()).await;
        assert_eq!(got, expect, "{rate}");
    }
    // レート不明なら据え置き（ffmpeg が原本のレートで書く）
    let src = dir.path().join("src96000.wav");
    let out = enc
        .encode(
            File::open(&src).unwrap(),
            0.0,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let (_, got) = measure(&ffmpeg, out.guard.path()).await;
    assert_eq!(got, 96_000);
}

#[tokio::test]
async fn encodes_lossy_sources_too() {
    let ffmpeg = require_ffmpeg!(common::ffmpeg());
    let dir = tempfile::tempdir().unwrap();
    let enc = AacEncoder::new(&ffmpeg, 128, dir.path().join("tmp"));
    for (name, ext) in [("a.opus", "opus"), ("b.m4a", "m4a"), ("c.mp3", "mp3")] {
        let src = common::make_audio(dir.path(), name, ext, 2).unwrap();
        let out = enc
            .encode(
                File::open(&src).unwrap(),
                -3.0,
                Some(44_100),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let af = read_audio_file(File::open(out.guard.path()).unwrap(), Some("m4a")).unwrap();
        assert_eq!(af.codec.as_str(), "aac", "{name}");
        let src_af = read_audio_file(File::open(&src).unwrap(), Some(ext)).unwrap();
        let (a, b) = (af.duration_ms.unwrap(), src_af.duration_ms.unwrap());
        assert!((a as i64 - b as i64).abs() < 150, "{name}: 長さ {a} vs {b}");
    }
}

#[tokio::test]
async fn cancel_removes_tmp() {
    let ffmpeg = require_ffmpeg!(common::ffmpeg());
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.wav");
    tone_wav(&src, -20.0, 48_000);
    let enc = AacEncoder::new(&ffmpeg, 256, dir.path().join("tmp"));
    let token = CancellationToken::new();
    token.cancel();
    let r = enc
        .encode(File::open(&src).unwrap(), 0.0, Some(48_000), &token)
        .await;
    assert!(matches!(r, Err(e) if e.is_cancelled()));
    assert_eq!(tmp_count(&dir.path().join("tmp")), 0);
}

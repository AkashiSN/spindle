//! PCM デコード（SPEC §15 `media/decode.rs`）。symphonia が扱える形式はプロセス内で、
//! Opus は ffmpeg で f32 に落とす。どちらの経路でも同じ `PcmSink` に同じ形で流れること

#![cfg(target_os = "linux")]

mod common;

use std::fs::File;

use tokio_util::sync::CancellationToken;

use spindle::media::decode::{DecodeError, Decoder, PcmInfo, PcmSink};

#[derive(Default)]
struct Collect {
    info: Option<PcmInfo>,
    samples: Vec<f32>,
}

impl PcmSink for Collect {
    fn start(&mut self, info: &PcmInfo) -> anyhow::Result<()> {
        assert!(self.info.is_none(), "start が 2 回呼ばれた");
        self.info = Some(*info);
        Ok(())
    }

    fn push(&mut self, interleaved: &[f32]) -> anyhow::Result<()> {
        assert!(self.info.is_some(), "start の前に push された");
        self.samples.extend_from_slice(interleaved);
        Ok(())
    }
}

fn rms(s: &[f32]) -> f64 {
    (s.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / s.len() as f64).sqrt()
}

#[tokio::test]
async fn decodes_flac_in_process() {
    let ffmpeg = require_ffmpeg!(common::ffmpeg());
    let dir = tempfile::tempdir().unwrap();
    let path = common::make_audio(dir.path(), "a.flac", "flac", 1).unwrap();
    let dec = Decoder::new(&ffmpeg);
    let (info, out) = dec
        .decode(
            File::open(&path).unwrap(),
            Some("flac"),
            Collect::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        info,
        PcmInfo {
            channels: 2,
            sample_rate: 44_100
        }
    );
    assert_eq!(out.info, Some(info));
    let expected = common::pcm_samples(1);
    assert_eq!(out.samples.len(), expected.len());
    for (got, want) in out.samples.iter().zip(&expected) {
        let want = *want as f32 / 32768.0;
        assert!((got - want).abs() < 1e-4, "{got} != {want}");
    }
}

/// ALAC の末尾サンプルを長さ 0 と宣言し直し、プロセス内デコードのフレーム数と ffmpeg のフレーム数を返す
async fn alac_frames_after_zeroing(edit: common::EditEnd) -> Option<(usize, usize, usize)> {
    let dir = tempfile::tempdir().unwrap();
    let path = common::make_audio(dir.path(), "a.m4a", "alac.m4a", 1)?;
    let dropped = common::zero_last_stts_delta(&path, edit) as usize;
    let ffmpeg = common::ffmpeg()?;
    let (_, out) = Decoder::new(&ffmpeg)
        .decode(
            File::open(&path).unwrap(),
            Some("m4a"),
            Collect::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    Some((
        out.samples.len() / 2,
        common::ffmpeg_frames(&path)?,
        dropped,
    ))
}

#[tokio::test]
async fn alac_stops_where_ffmpeg_stops() {
    // D-89: edit list が宣言長で終わる ALAC の長さ 0 のサンプルは RG / hirescheck の PCM にも入れない
    let (ours, theirs, dropped) =
        require_ffmpeg!(alac_frames_after_zeroing(common::EditEnd::AtDeclared).await);
    assert_eq!(ours, 44_100 - dropped);
    assert_eq!(ours, theirs);
}

#[tokio::test]
async fn alac_keeps_the_tail_ffmpeg_keeps() {
    // D-89 追記: edit list が宣言長より後ろで終わる・無いときは、ffmpeg と同じく全部出す
    for edit in [common::EditEnd::Beyond, common::EditEnd::Removed] {
        let (ours, theirs, _) = require_ffmpeg!(alac_frames_after_zeroing(edit).await);
        assert_eq!(ours, 44_100, "{edit:?}");
        assert_eq!(ours, theirs, "{edit:?}");
    }
}

#[tokio::test]
async fn aac_in_mp4_is_not_cut_at_the_declared_length() {
    // D-89: 打ち切りは ALAC だけ。symphonia の MP4 は edit list を読まず AAC の encoder delay も
    // delay / padding に載せないので、AAC の num_frames（stts の合計。1 秒 / 44.1k で 45,124 前後）で切ると
    // 中途半端な長さになる。従来どおり全パケット（1,024 フレーム単位）を出すこと
    let dir = tempfile::tempdir().unwrap();
    let path = require_ffmpeg!(common::make_audio(dir.path(), "a.m4a", "m4a", 1));
    let ffmpeg = common::ffmpeg().unwrap();
    let (_, out) = Decoder::new(&ffmpeg)
        .decode(
            File::open(&path).unwrap(),
            Some("m4a"),
            Collect::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let frames = out.samples.len() / 2;
    assert!(frames >= 44_100, "{frames}");
    assert_eq!(frames % 1024, 0, "宣言長で切られている: {frames}");
}

#[tokio::test]
async fn decodes_opus_via_ffmpeg() {
    let ffmpeg = require_ffmpeg!(common::ffmpeg());
    let dir = tempfile::tempdir().unwrap();
    let path = common::make_audio(dir.path(), "a.opus", "opus", 1).unwrap();
    let dec = Decoder::new(&ffmpeg);
    let (info, out) = dec
        .decode(
            File::open(&path).unwrap(),
            Some("opus"),
            Collect::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        info,
        PcmInfo {
            channels: 2,
            sample_rate: 48_000
        }
    );
    // 1 秒 ± 数十 ms（pre-skip / パディング）
    let frames = out.samples.len() / 2;
    assert!((47_000..=49_000).contains(&frames), "frames = {frames}");
    // 非可逆でも実効値はほぼ保たれる
    let source: Vec<f32> = common::pcm_samples(1)
        .iter()
        .map(|v| *v as f32 / 32768.0)
        .collect();
    let (a, b) = (rms(&source), rms(&out.samples));
    assert!((a - b).abs() / a < 0.05, "rms {a} vs {b}");
}

#[tokio::test]
async fn ffmpeg_failure_is_reported() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let dir = tempfile::tempdir().unwrap();
    let path = common::make_audio(dir.path(), "a.opus", "opus", 1).unwrap();
    let dec = Decoder::new("/nonexistent/ffmpeg");
    let err = dec
        .decode(
            File::open(&path).unwrap(),
            Some("opus"),
            Collect::default(),
            &CancellationToken::new(),
        )
        .await
        .err()
        .expect("失敗するはず");
    assert!(matches!(err, DecodeError::Process(_)), "{err:?}");
}

#[tokio::test]
async fn garbage_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.flac");
    std::fs::write(&path, b"not audio at all").unwrap();
    let dec = Decoder::new("ffmpeg");
    let err = dec
        .decode(
            File::open(&path).unwrap(),
            Some("flac"),
            Collect::default(),
            &CancellationToken::new(),
        )
        .await
        .err()
        .expect("失敗するはず");
    assert!(!matches!(err, DecodeError::Cancelled), "{err:?}");
}

#[tokio::test]
async fn cancel_stops_in_process_decode() {
    let ffmpeg = require_ffmpeg!(common::ffmpeg());
    let dir = tempfile::tempdir().unwrap();
    let path = common::make_audio(dir.path(), "a.flac", "flac", 1).unwrap();
    let token = CancellationToken::new();
    token.cancel();
    let err = Decoder::new(&ffmpeg)
        .decode(
            File::open(&path).unwrap(),
            Some("flac"),
            Collect::default(),
            &token,
        )
        .await
        .err()
        .expect("キャンセルされるはず");
    assert!(matches!(err, DecodeError::Cancelled), "{err:?}");
}

#[tokio::test]
async fn formats_symphonia_cannot_probe_fall_back_to_ffmpeg_with_lofty_properties() {
    // WavPack は symphonia に demuxer が無い。チャンネル数とレートは lofty から取り、
    // PCM は ffmpeg に出させる
    let ffmpeg = require_ffmpeg!(common::ffmpeg());
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("src.wav");
    common::write_wav(&wav, &common::pcm_samples(1), 16);
    let path = dir.path().join("a.wv");
    if common::encode(&wav, &path, &["-c:a", "wavpack"]).is_none() {
        eprintln!("ffmpeg に wavpack が無いので skip");
        return;
    }
    let dec = Decoder::new(&ffmpeg);
    let (info, out) = dec
        .decode(
            File::open(&path).unwrap(),
            Some("wv"),
            Collect::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        info,
        PcmInfo {
            channels: 2,
            sample_rate: 44_100
        }
    );
    let expected = common::pcm_samples(1);
    assert_eq!(out.samples.len(), expected.len());
    for (got, want) in out.samples.iter().zip(&expected) {
        let want = *want as f32 / 32768.0;
        assert!((got - want).abs() < 1e-4, "{got} != {want}");
    }
}

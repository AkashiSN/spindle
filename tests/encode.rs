//! FLAC エンコード（ロスレス正規化の変換段。SPEC §7.4）。ffmpeg でデコード → flac -8 で
//! エンコードした結果の STREAMINFO MD5 が、symphonia で計算した元 PCM の MD5 と一致すること。
//! ffmpeg / flac が無い環境では skip

#![cfg(target_os = "linux")]

mod common;

use std::fs::File;
use std::path::PathBuf;
use std::process::Command;

use tokio_util::sync::CancellationToken;

use spindle::media::encode::FlacEncoder;
use spindle::media::fingerprint::{decoded_pcm_md5, flac_streaminfo_md5};

fn flac_bin() -> Option<PathBuf> {
    let p = Command::new("flac").arg("--version").output().ok()?;
    p.status.success().then(|| PathBuf::from("flac"))
}

macro_rules! require_tools {
    () => {
        match (common::ffmpeg(), flac_bin()) {
            (Some(f), Some(l)) => (f, l),
            _ => {
                eprintln!("ffmpeg / flac が無いので skip");
                return;
            }
        }
    };
}

async fn roundtrip(name: &str, ext: &str, seed: u32, bit_depth: u32) {
    let (ffmpeg, flac) = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = if ext == "wav" {
        let p = dir.path().join(name);
        common::write_wav(&p, &common::pcm_samples(seed), bit_depth);
        p
    } else {
        common::make_audio(dir.path(), name, ext, seed).unwrap()
    };
    let src_ext = src.extension().and_then(|e| e.to_str());
    let expected = decoded_pcm_md5(File::open(&src).unwrap(), src_ext).unwrap();

    let enc = FlacEncoder::new(ffmpeg, flac, 8, dir.path().join("tmp"));
    let out = enc
        .encode(
            File::open(&src).unwrap(),
            bit_depth,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        out.streaminfo_md5,
        Some(expected),
        "{name}: MD5 が一致しない"
    );
    // 生成物を開き直しても同じ MD5
    let again = flac_streaminfo_md5(File::open(out.guard.path()).unwrap()).unwrap();
    assert_eq!(again, Some(expected));
    let path = out.guard.path().to_path_buf();
    drop(out);
    assert!(!path.exists(), "guard の drop で tmp が消える");
    // WAV の作業ファイルも残っていない
    let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("tmp"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "作業ファイルが残っている: {leftovers:?}"
    );
}

#[tokio::test]
async fn wav_16bit() {
    roundtrip("a.wav", "wav", 1, 16).await;
}

#[tokio::test]
async fn wav_24bit() {
    roundtrip("b.wav", "wav", 2, 24).await;
}

#[tokio::test]
async fn alac() {
    roundtrip("c.m4a", "alac.m4a", 3, 16).await;
}

#[tokio::test]
async fn aiff() {
    roundtrip("d.aiff", "aiff", 4, 16).await;
}

#[tokio::test]
async fn cancelled_leaves_nothing() {
    let (ffmpeg, flac) = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.wav");
    common::write_wav(&src, &common::pcm_samples(9), 16);
    let enc = FlacEncoder::new(ffmpeg, flac, 8, dir.path().join("tmp"));
    let token = CancellationToken::new();
    token.cancel();
    let err = enc
        .encode(File::open(&src).unwrap(), 16, &token)
        .await
        .unwrap_err();
    assert!(err.is_cancelled(), "{err}");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("tmp"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "作業ファイルが残っている: {leftovers:?}"
    );
}

#[test]
fn unsupported_bit_depth() {
    assert!(FlacEncoder::pcm_codec(20).is_err());
    assert_eq!(FlacEncoder::pcm_codec(24).unwrap(), "pcm_s24le");
}

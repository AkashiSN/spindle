//! Derived 用の Opus エンコード（SPEC §7.6、D-9、docs/TASKS.md P1-10）。ffmpeg でデコード →
//! opusenc。タグは lofty で後から書く。ffmpeg / opusenc が無い環境では skip

#![cfg(target_os = "linux")]

mod common;

use std::fs::File;
use std::path::PathBuf;
use std::process::Command;

use lofty::picture::{MimeType, Picture, PictureType};
use tokio_util::sync::CancellationToken;

use spindle::domain::tags::{read_audio_file, write_opus_tags, TransferTags};
use spindle::media::encode::OpusEncoder;

fn opusenc_bin() -> Option<PathBuf> {
    let p = Command::new("opusenc").arg("--version").output().ok()?;
    p.status.success().then(|| PathBuf::from("opusenc"))
}

macro_rules! require_tools {
    () => {
        match (common::ffmpeg(), opusenc_bin()) {
            (Some(f), Some(o)) => (f, o),
            _ => {
                eprintln!("ffmpeg / opusenc が無いので skip");
                return;
            }
        }
    };
}

fn tmp_count(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0)
}

#[tokio::test]
async fn encodes_flac_alac_and_wav_to_opus_without_comments() {
    let (ffmpeg, opusenc) = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    for (name, ext, bits) in [
        ("src.flac", "flac", Some(16)),
        ("src.m4a", "alac.m4a", Some(16)),
        ("src.wav", "wav", Some(24)),
    ] {
        let src = if ext == "wav" {
            let p = dir.path().join(name);
            common::write_wav(&p, &common::pcm_samples(3), 24);
            p
        } else {
            common::make_audio(dir.path(), name, ext, 3).unwrap()
        };
        common::set_basic_tags(&src, "曲", "a", "al", "aa", 1, 1);
        let enc = OpusEncoder::new(&ffmpeg, &opusenc, 128, dir.path().join("tmp"));
        let out = enc
            .encode(File::open(&src).unwrap(), bits, &CancellationToken::new())
            .await
            .unwrap();
        let af = read_audio_file(File::open(out.guard.path()).unwrap(), Some("opus")).unwrap();
        assert_eq!(af.codec.as_str(), "opus", "{name}");
        assert!(!af.lossless);
        let src_ext = name.rsplit_once('.').unwrap().1;
        let src_af = read_audio_file(File::open(&src).unwrap(), Some(src_ext)).unwrap();
        let (a, b) = (af.duration_ms.unwrap(), src_af.duration_ms.unwrap());
        assert!((a as i64 - b as i64).abs() < 100, "{name}: 長さ {a} vs {b}");
        // コメントは移していない（呼び出し側が lofty で書く。opusenc 自身の ENCODER* は残る）
        let foreign: Vec<_> = af
            .tags
            .items()
            .iter()
            .filter(|(k, _)| !k.starts_with("ENCODER"))
            .collect();
        assert!(foreign.is_empty(), "{name}: {foreign:?}");
        drop(out);
    }
    assert_eq!(tmp_count(&dir.path().join("tmp")), 0);
}

#[tokio::test]
async fn unsupported_or_unknown_bit_depth_still_encodes() {
    let (ffmpeg, opusenc) = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = common::make_audio(dir.path(), "src.flac", "flac", 5).unwrap();
    let enc = OpusEncoder::new(&ffmpeg, &opusenc, 96, dir.path().join("tmp"));
    // 20 bit（ffmpeg に PCM エンコーダが無い）でも、不明でも失敗しない
    enc.encode(
        File::open(&src).unwrap(),
        Some(20),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    enc.encode(File::open(&src).unwrap(), None, &CancellationToken::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn cancel_leaves_no_tmp() {
    let (ffmpeg, opusenc) = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = common::make_audio(dir.path(), "src.flac", "flac", 5).unwrap();
    let tmp = dir.path().join("tmp");
    let enc = OpusEncoder::new(&ffmpeg, &opusenc, 128, &tmp);
    let token = CancellationToken::new();
    token.cancel();
    let err = enc
        .encode(File::open(&src).unwrap(), Some(16), &token)
        .await
        .unwrap_err();
    assert!(err.is_cancelled(), "{err}");
    assert_eq!(tmp_count(&tmp), 0);
}

#[tokio::test]
async fn broken_opusenc_fails_and_cleans_tmp() {
    let (ffmpeg, _) = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = common::make_audio(dir.path(), "src.flac", "flac", 5).unwrap();
    let tmp = dir.path().join("tmp");
    let enc = OpusEncoder::new(&ffmpeg, "/bin/false", 128, &tmp);
    let err = enc
        .encode(
            File::open(&src).unwrap(),
            Some(16),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(!err.is_cancelled());
    assert_eq!(tmp_count(&tmp), 0);
}

#[tokio::test]
async fn write_opus_tags_replaces_comments_and_pictures() {
    let (ffmpeg, opusenc) = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = common::make_audio(dir.path(), "src.flac", "flac", 5).unwrap();
    let enc = OpusEncoder::new(&ffmpeg, &opusenc, 128, dir.path().join("tmp"));
    let out = enc
        .encode(
            File::open(&src).unwrap(),
            Some(16),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let path = out.guard.path().to_path_buf();
    let webp = {
        let p = dir.path().join("c.webp");
        let st = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg("color=c=blue:s=8x8")
            .args(["-frames:v", "1"])
            .arg(&p)
            .status()
            .unwrap();
        assert!(st.success());
        std::fs::read(p).unwrap()
    };
    let tags = TransferTags {
        items: vec![
            ("TITLE".into(), "曲".into()),
            ("ARTIST".into(), "a".into()),
            ("ARTIST".into(), "b".into()),
            ("R128_TRACK_GAIN".into(), "-2816".into()),
        ],
        pictures: vec![Picture::unchecked(webp)
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Unknown("image/webp".into()))
            .build()],
    };
    let mut f = File::options().read(true).write(true).open(&path).unwrap();
    write_opus_tags(&mut f, &tags).unwrap();
    drop(f);
    let af = read_audio_file(File::open(&path).unwrap(), Some("opus")).unwrap();
    assert_eq!(af.tags.first("TITLE"), Some("曲"));
    assert_eq!(af.tags.values("ARTIST").collect::<Vec<_>>(), vec!["a", "b"]);
    assert_eq!(af.tags.first("R128_TRACK_GAIN"), Some("-2816"));
    assert_eq!(
        af.tags
            .first("PICTURE")
            .map(|s| s.starts_with("image/webp:")),
        Some(true)
    );
    // 音声はまだ読める
    assert!(af.duration_ms.unwrap() > 500);

    // 二度目は置き換え（増えない）
    let mut f = File::options().read(true).write(true).open(&path).unwrap();
    write_opus_tags(
        &mut f,
        &TransferTags {
            items: vec![("TITLE".into(), "x".into())],
            pictures: vec![],
        },
    )
    .unwrap();
    drop(f);
    let af = read_audio_file(File::open(&path).unwrap(), Some("opus")).unwrap();
    assert_eq!(af.tags.first("TITLE"), Some("x"));
    assert!(af.tags.first("ARTIST").is_none());
    assert!(af.tags.first("PICTURE").is_none());
}

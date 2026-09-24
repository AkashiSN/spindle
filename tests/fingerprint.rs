//! 音声フィンガープリント（SPEC §6「変更検出と版の遷移」、D-30）。
//! FLAC は STREAMINFO の MD5、ALAC / WAV はデコードした PCM の MD5、非可逆はパケット列の SHA-256。
//! 受け入れ: docs/TASKS.md P0-5 (c) (l)
//!
//! 圧縮形式のファイルは ffmpeg で作る。ffmpeg が無い環境ではそのテストを skip する
//! （CI には ffmpeg を入れる）。

use std::fs::File;
use std::io::Cursor;

mod common;

use md5::{Digest, Md5};

use spindle::media::fingerprint::{
    decoded_pcm_md5, flac_streaminfo_md5, flac_streaminfo_md5_at, packet_fp, FingerprintError,
    FrameLimit,
};

// ---------------------------------------------------------------- 生成ヘルパ

use common::{encode, pack_pcm, pcm_samples, write_wav, zero_last_stts_delta};

/// 手組みの最小 FLAC（fLaC マーカー + STREAMINFO のみ）。`md5` を埋める
fn minimal_flac(md5: [u8; 16]) -> Vec<u8> {
    let mut v = b"fLaC".to_vec();
    v.push(0x80); // last-metadata-block, type 0 = STREAMINFO
    v.extend_from_slice(&[0, 0, 34]);
    let mut info = [0u8; 34];
    // min / max blocksize
    info[0..2].copy_from_slice(&4096u16.to_be_bytes());
    info[2..4].copy_from_slice(&4096u16.to_be_bytes());
    // sample rate 44100 (20bit) | channels-1 (3bit) | bps-1 (5bit) | total samples (36bit)
    let packed: u64 = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36) | 44_100u64;
    info[10..18].copy_from_slice(&packed.to_be_bytes());
    info[18..34].copy_from_slice(&md5);
    v.extend_from_slice(&info);
    v
}

fn md5_of(bytes: &[u8]) -> [u8; 16] {
    Md5::digest(bytes).into()
}

// ---------------------------------------------------------------- FLAC STREAMINFO

#[test]
fn flac_md5_is_read_from_streaminfo_without_decoding() {
    let md5 = [0x11u8; 16];
    let flac = minimal_flac(md5);
    assert_eq!(flac_streaminfo_md5(Cursor::new(flac)).unwrap(), Some(md5));
}

#[test]
fn flac_without_md5_is_none_not_an_error() {
    // 受け入れ (c): MD5 未設定（全ゼロ）を「MD5 なし」として扱い、落ちない
    let flac = minimal_flac([0u8; 16]);
    assert_eq!(flac_streaminfo_md5(Cursor::new(flac)).unwrap(), None);
}

#[test]
fn flac_md5_skips_preceding_metadata_blocks() {
    // STREAMINFO は常に先頭だが、fLaC の前に ID3v2 が付いた FLAC は存在する
    let md5 = [0x22u8; 16];
    let mut v = b"ID3\x04\x00\x00\x00\x00\x00\x0a".to_vec(); // 10 バイトのボディ
    v.extend_from_slice(&[0u8; 10]);
    v.extend_from_slice(&minimal_flac(md5));
    assert_eq!(flac_streaminfo_md5(Cursor::new(v)).unwrap(), Some(md5));
}

#[test]
fn flac_md5_offset_points_at_the_16_bytes_in_streaminfo() {
    // MD5 補填（P1-5b）はこの位置の 16 バイトだけを書き換える。ID3v2 が前置されていれば
    // その分ずれる
    let md5 = [0x33u8; 16];
    let flac = minimal_flac(md5);
    let (offset, current) = flac_streaminfo_md5_at(Cursor::new(&flac)).unwrap();
    assert_eq!(offset, 4 + 4 + 18);
    assert_eq!(current, md5);
    assert_eq!(&flac[offset as usize..offset as usize + 16], &md5);

    let mut v = b"ID3\x04\x00\x00\x00\x00\x00\x0a".to_vec();
    v.extend_from_slice(&[0u8; 10]);
    v.extend_from_slice(&minimal_flac([0u8; 16]));
    let (offset, current) = flac_streaminfo_md5_at(Cursor::new(&v)).unwrap();
    assert_eq!(offset, 20 + 26);
    assert_eq!(current, [0u8; 16], "全ゼロもそのまま返す（None にしない）");
}

#[test]
fn non_flac_and_truncated_input_are_errors() {
    assert!(matches!(
        flac_streaminfo_md5(Cursor::new(b"RIFF....WAVE".to_vec())),
        Err(FingerprintError::NotFlac)
    ));
    let mut short = minimal_flac([1u8; 16]);
    short.truncate(20);
    assert!(flac_streaminfo_md5(Cursor::new(short)).is_err());
}

// ---------------------------------------------------------------- WAV / ALAC → PCM MD5

#[test]
fn wav_pcm_md5_matches_raw_sample_bytes_16bit() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    let samples = pcm_samples(0);
    write_wav(&wav, &samples, 16);
    let got = decoded_pcm_md5(File::open(&wav).unwrap(), Some("wav")).unwrap();
    assert_eq!(got, md5_of(&pack_pcm(&samples, 16)));
}

#[test]
fn wav_pcm_md5_matches_raw_sample_bytes_24bit() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    let samples: Vec<i32> = pcm_samples(0).iter().map(|s| s * 200).collect();
    write_wav(&wav, &samples, 24);
    let got = decoded_pcm_md5(File::open(&wav).unwrap(), Some("wav")).unwrap();
    assert_eq!(got, md5_of(&pack_pcm(&samples, 24)));
}

#[test]
fn flac_alac_and_wav_of_same_pcm_share_audio_md5() {
    // WAV → FLAC 正規化や ALAC からの移行で audio_md5 が変わらないための性質
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    let samples = pcm_samples(0);
    write_wav(&wav, &samples, 16);
    let expected = md5_of(&pack_pcm(&samples, 16));

    let flac = dir.path().join("a.flac");
    require_ffmpeg!(encode(&wav, &flac, &["-c:a", "flac"]));
    assert_eq!(
        flac_streaminfo_md5(File::open(&flac).unwrap()).unwrap(),
        Some(expected)
    );

    let alac = dir.path().join("a.m4a");
    encode(&wav, &alac, &["-c:a", "alac"]).unwrap();
    assert_eq!(
        decoded_pcm_md5(File::open(&alac).unwrap(), Some("m4a")).unwrap(),
        expected
    );
}

#[test]
fn alac_24bit_shares_audio_md5_with_flac() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    let samples: Vec<i32> = pcm_samples(0).iter().map(|s| s * 200).collect();
    write_wav(&wav, &samples, 24);
    let flac = dir.path().join("a.flac");
    require_ffmpeg!(encode(&wav, &flac, &["-c:a", "flac", "-sample_fmt", "s32"]));
    let alac = dir.path().join("a.m4a");
    encode(&wav, &alac, &["-c:a", "alac", "-sample_fmt", "s32p"]).unwrap();
    let from_flac = flac_streaminfo_md5(File::open(&flac).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(from_flac, md5_of(&pack_pcm(&samples, 24)));
    assert_eq!(
        decoded_pcm_md5(File::open(&alac).unwrap(), Some("m4a")).unwrap(),
        from_flac
    );
}

#[test]
fn alac_trailing_zero_duration_sample_is_not_hashed() {
    // D-89: 実機の ALAC は stts の末尾に長さ 0 のサンプルを持つ（宣言長の外）。symphonia 0.6.1 は
    // それもデコードして返すので、宣言長で打ち切らないと ffmpeg（宣言どおり捨てる）と PCM MD5 が
    // 食い違い、正規化の照合が失敗する
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    let samples: Vec<i32> = pcm_samples(0).iter().map(|s| s * 200).collect();
    write_wav(&wav, &samples, 24);
    let alac = dir.path().join("a.m4a");
    require_ffmpeg!(encode(
        &wav,
        &alac,
        &["-c:a", "alac", "-sample_fmt", "s32p"]
    ));
    let dropped = zero_last_stts_delta(&alac) as usize;
    let frames = samples.len() / 2;
    assert!(dropped > 0 && dropped < frames);
    let kept = &samples[..(frames - dropped) * 2];
    assert_eq!(
        decoded_pcm_md5(File::open(&alac).unwrap(), Some("m4a")).unwrap(),
        md5_of(&pack_pcm(kept, 24))
    );
}

#[test]
fn frame_limit_cuts_at_the_declared_length() {
    let mut l = FrameLimit::new(Some(10), None, None);
    assert_eq!(l.take(4), 4);
    assert!(!l.exhausted());
    assert_eq!(l.take(4), 4);
    assert_eq!(l.take(4), 2);
    assert!(l.exhausted());
    assert_eq!(l.take(4), 0);
    // delay / padding 0 は「無い」と同じ
    let mut l = FrameLimit::new(Some(3), Some(0), Some(0));
    assert_eq!(l.take(4), 3);
}

#[test]
fn frame_limit_is_off_without_a_declared_length_or_with_encoder_trims() {
    // 宣言長が無い
    let mut l = FrameLimit::new(None, None, None);
    assert_eq!(l.take(4096), 4096);
    assert!(!l.exhausted());
    // 宣言長は delay / padding を除いた長さ。trim を適用していないので打ち切ると本物の末尾を失う
    let mut l = FrameLimit::new(Some(10), Some(576), None);
    assert_eq!(l.take(4096), 4096);
    let mut l = FrameLimit::new(Some(10), None, Some(1000));
    assert_eq!(l.take(4096), 4096);
}

// ---------------------------------------------------------------- 非可逆のパケット列

fn assert_packet_fp_survives_retag(ext: &str, args: &[&str]) {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    write_wav(&wav, &pcm_samples(0), 16);
    let dst = dir.path().join(format!("a.{ext}"));
    require_ffmpeg!(encode(&wav, &dst, args));

    let before = packet_fp(File::open(&dst).unwrap(), Some(ext)).unwrap();
    let size_before = std::fs::metadata(&dst).unwrap().len();
    common::retag(&dst, |tag| {
        use lofty::tag::{Accessor, ItemKey};
        tag.set_title("書き換え後のタイトル".to_owned());
        tag.insert_text(ItemKey::Comment, "x".repeat(4096));
    });
    let size_after = std::fs::metadata(&dst).unwrap().len();
    assert_ne!(
        size_before, size_after,
        "タグ書き換えでコンテナサイズが変わる前提"
    );
    let after = packet_fp(File::open(&dst).unwrap(), Some(ext)).unwrap();
    assert_eq!(
        before, after,
        "{ext}: タグだけの変更で audio_fp が変わってはいけない"
    );

    // 別の音声なら別の値
    let wav2 = dir.path().join("b.wav");
    write_wav(&wav2, &pcm_samples(7), 16);
    let dst2 = dir.path().join(format!("b.{ext}"));
    encode(&wav2, &dst2, args).unwrap();
    assert_ne!(
        packet_fp(File::open(&dst2).unwrap(), Some(ext)).unwrap(),
        before
    );
}

#[test]
fn mp3_packet_fp_survives_tag_rewrite() {
    assert_packet_fp_survives_retag("mp3", &["-c:a", "libmp3lame", "-b:a", "128k"]);
}

#[test]
fn opus_packet_fp_survives_tag_rewrite() {
    assert_packet_fp_survives_retag("opus", &["-c:a", "libopus", "-b:a", "96k"]);
}

#[test]
fn aac_in_mp4_packet_fp_survives_tag_rewrite() {
    assert_packet_fp_survives_retag("m4a", &["-c:a", "aac", "-b:a", "128k"]);
}

#[test]
fn ogg_vorbis_packet_fp_survives_tag_rewrite() {
    assert_packet_fp_survives_retag("ogg", &["-c:a", "libvorbis", "-q:a", "3"]);
}

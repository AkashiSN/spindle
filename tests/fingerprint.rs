//! 音声フィンガープリント（SPEC §6「変更検出と版の遷移」、D-30）。
//! FLAC は STREAMINFO の MD5、ALAC / WAV はデコードした PCM の MD5、非可逆はパケット列の SHA-256。
//! 受け入れ: docs/TASKS.md P0-5 (c) (l)
//!
//! 圧縮形式のファイルは ffmpeg で作る。ffmpeg が無い環境ではそのテストを skip する
//! （CI には ffmpeg を入れる）。

use std::fs::File;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use md5::{Digest, Md5};

use spindle::media::fingerprint::{
    decoded_pcm_md5, flac_streaminfo_md5, packet_fp, FingerprintError,
};

// ---------------------------------------------------------------- 生成ヘルパ

/// 1 秒のステレオ正弦波。左右で周波数を変える
fn pcm_samples(seed: u32) -> Vec<i32> {
    let rate = 44_100usize;
    let mut out = Vec::with_capacity(rate * 2);
    for i in 0..rate {
        let t = i as f64 / rate as f64;
        let l = ((t * 440.0 * std::f64::consts::TAU).sin() * 20_000.0) as i32;
        let r = ((t * (660.0 + seed as f64) * std::f64::consts::TAU).sin() * 18_000.0) as i32;
        out.push(l);
        out.push(r);
    }
    out
}

/// FLAC の STREAMINFO MD5 と同じ流儀（LE インターリーブ、bps ぶんのバイト）で並べたバイト列
fn pack_pcm(samples: &[i32], bits: u32) -> Vec<u8> {
    let bytes = (bits / 8) as usize;
    let mut out = Vec::with_capacity(samples.len() * bytes);
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes()[..bytes]);
    }
    out
}

fn write_wav(path: &Path, samples: &[i32], bits: u32) {
    let data = pack_pcm(samples, bits);
    let channels = 2u16;
    let rate = 44_100u32;
    let block_align = channels * (bits / 8) as u16;
    let mut f = File::create(path).unwrap();
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + data.len() as u32).to_le_bytes())
        .unwrap();
    f.write_all(b"WAVEfmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
    f.write_all(&channels.to_le_bytes()).unwrap();
    f.write_all(&rate.to_le_bytes()).unwrap();
    f.write_all(&(rate * block_align as u32).to_le_bytes())
        .unwrap();
    f.write_all(&block_align.to_le_bytes()).unwrap();
    f.write_all(&(bits as u16).to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&(data.len() as u32).to_le_bytes()).unwrap();
    f.write_all(&data).unwrap();
}

fn ffmpeg() -> Option<PathBuf> {
    let p = Command::new("ffmpeg").arg("-version").output().ok()?;
    p.status.success().then(|| PathBuf::from("ffmpeg"))
}

/// `ffmpeg -i src <args> dst`。ffmpeg が無ければ None
fn encode(src: &Path, dst: &Path, args: &[&str]) -> Option<()> {
    let ffmpeg = ffmpeg()?;
    let st = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(src)
        .args(args)
        .arg(dst)
        .status()
        .unwrap();
    assert!(st.success(), "ffmpeg {args:?} failed");
    Some(())
}

macro_rules! require_ffmpeg {
    ($e:expr) => {
        match $e {
            Some(v) => v,
            None => {
                eprintln!("ffmpeg が無いので skip");
                return;
            }
        }
    };
}

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

// ---------------------------------------------------------------- 非可逆のパケット列

fn retag_with_lofty(path: &Path) {
    use lofty::config::WriteOptions;
    use lofty::file::TaggedFileExt;
    use lofty::tag::{Accessor, ItemKey, TagExt};

    let mut tagged = lofty::read_from_path(path).unwrap();
    let tag = match tagged.primary_tag_mut() {
        Some(t) => t,
        None => {
            let ty = tagged.primary_tag_type();
            tagged.insert_tag(lofty::tag::Tag::new(ty));
            tagged.primary_tag_mut().unwrap()
        }
    };
    tag.set_title("書き換え後のタイトル".to_owned());
    tag.insert_text(ItemKey::Comment, "x".repeat(4096));
    tag.save_to_path(path, WriteOptions::default()).unwrap();
}

fn assert_packet_fp_survives_retag(ext: &str, args: &[&str]) {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    write_wav(&wav, &pcm_samples(0), 16);
    let dst = dir.path().join(format!("a.{ext}"));
    require_ffmpeg!(encode(&wav, &dst, args));

    let before = packet_fp(File::open(&dst).unwrap(), Some(ext)).unwrap();
    let size_before = std::fs::metadata(&dst).unwrap().len();
    retag_with_lofty(&dst);
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

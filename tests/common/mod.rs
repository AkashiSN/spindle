//! テスト用の音声ファイル生成。WAV は Rust で書き、圧縮形式は ffmpeg で作る。
//! ffmpeg が無い環境では `require_ffmpeg!` で該当テストを skip する（CI には入れてある）。

#![allow(dead_code)]

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 1 秒のステレオ正弦波。`seed` で右チャンネルの周波数を変える
pub fn pcm_samples(seed: u32) -> Vec<i32> {
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
pub fn pack_pcm(samples: &[i32], bits: u32) -> Vec<u8> {
    let bytes = (bits / 8) as usize;
    let mut out = Vec::with_capacity(samples.len() * bytes);
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes()[..bytes]);
    }
    out
}

pub fn write_wav(path: &Path, samples: &[i32], bits: u32) {
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

pub fn ffmpeg() -> Option<PathBuf> {
    let p = Command::new("ffmpeg").arg("-version").output().ok()?;
    p.status.success().then(|| PathBuf::from("ffmpeg"))
}

/// `ffmpeg -i src <args> dst`。ffmpeg が無ければ None
pub fn encode(src: &Path, dst: &Path, args: &[&str]) -> Option<()> {
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

/// 拡張子ごとのエンコード引数
pub fn encode_args(ext: &str) -> &'static [&'static str] {
    match ext {
        "flac" => &["-c:a", "flac"],
        "opus" => &["-c:a", "libopus", "-b:a", "96k"],
        "mp3" => &["-c:a", "libmp3lame", "-b:a", "128k"],
        "m4a" => &["-c:a", "aac", "-b:a", "128k"],
        "alac.m4a" => &["-c:a", "alac"],
        "ogg" => &["-c:a", "libvorbis", "-q:a", "3"],
        other => panic!("unknown ext {other}"),
    }
}

/// `dir/name` に `ext` 形式の 1 秒音声を作る（`seed` で内容を変える）。ffmpeg が無ければ None
pub fn make_audio(dir: &Path, name: &str, ext: &str, seed: u32) -> Option<PathBuf> {
    let wav = dir.join(format!(".{name}.src.wav"));
    write_wav(&wav, &pcm_samples(seed), 16);
    let dst = dir.join(name);
    let r = encode(&wav, &dst, encode_args(ext));
    let _ = std::fs::remove_file(&wav);
    r.map(|()| dst)
}

#[macro_export]
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

/// lofty で主要タグを書き換える（テストは lofty を直接使ってよい）
pub fn retag(path: &Path, f: impl FnOnce(&mut lofty::tag::Tag)) {
    use lofty::config::WriteOptions;
    use lofty::file::TaggedFileExt;
    use lofty::tag::TagExt;

    let mut tagged = lofty::read_from_path(path).unwrap();
    if tagged.primary_tag_mut().is_none() {
        let ty = tagged.primary_tag_type();
        tagged.insert_tag(lofty::tag::Tag::new(ty));
    }
    let tag = tagged.primary_tag_mut().unwrap();
    f(tag);
    tag.save_to_path(path, WriteOptions::default()).unwrap();
}

/// タイトル等の基本タグを一括で付ける
pub fn set_basic_tags(
    path: &Path,
    title: &str,
    artist: &str,
    album: &str,
    albumartist: &str,
    track: u32,
    disc: u32,
) {
    use lofty::tag::{Accessor, ItemKey};
    retag(path, |tag| {
        tag.set_title(title.to_owned());
        tag.set_artist(artist.to_owned());
        tag.set_album(album.to_owned());
        tag.insert_text(ItemKey::AlbumArtist, albumartist.to_owned());
        tag.set_track(track);
        tag.set_disk(disc);
    });
}

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
    write_wav_ex(path, samples, bits, 44_100, 2);
}

/// レートとチャンネル数を指定して書く（ハイレゾの合成用）
pub fn write_wav_ex(path: &Path, samples: &[i32], bits: u32, rate: u32, channels: u16) {
    let data = pack_pcm(samples, bits);
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
        "webm" => &["-c:a", "libopus", "-b:a", "96k"],
        "mp3" => &["-c:a", "libmp3lame", "-b:a", "128k"],
        "m4a" => &["-c:a", "aac", "-b:a", "128k"],
        "alac.m4a" => &["-c:a", "alac"],
        "aiff" => &["-c:a", "pcm_s16be", "-f", "aiff"],
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

/// Derived の opus 系統を on（`bitrate` kbps）にする。起動時の `db::derived::sync_variants` と同じで、
/// transcode の投入を見るテストのフィクスチャが `Db::open` の直後に呼ぶ（SPEC §7.6、D-75）
pub fn enable_opus_variant(db_path: &Path, bitrate: u32) {
    enable_variants(db_path, bitrate, None);
}

/// Derived の系統を on にする。起動時の `db::derived::sync_variants` と同じ。`aac` は None なら
/// 節省略の既定（off）
pub fn enable_variants(
    db_path: &Path,
    opus_bitrate: u32,
    aac: Option<spindle::config::AacVariantConfig>,
) {
    let c = rusqlite::Connection::open(db_path).unwrap();
    let cfg = spindle::config::DerivedConfig {
        opus: spindle::config::OpusVariantConfig {
            enabled: true,
            bitrate: opus_bitrate,
        },
        aac: aac.unwrap_or_default(),
    };
    spindle::db::derived::sync_variants(&c, &cfg, 0).unwrap();
}

/// CD の吸い出しの記録（サイドカーの `rip`）。`files` は音声トラック順のファイル名、`ctdb_matched` は
/// トラックごとの CTDB の一致。CRC はトラック `i` で `ar_v1 = 1 + i`、`ar_v2 = 11 + i`、`ctdb = 100 + i`
/// （どのファイルに結びついたかをテストが見分けられるように）。TOC は 10 秒 × ファイル数
pub fn rip_entry(files: &[&str], ctdb_matched: &[bool]) -> spindle::import::sidecar::RipEntry {
    use spindle::cd::metadata::{DiscMetadata, DiscTrackMetadata, MetadataSource};
    use spindle::cd::riplog::{OffsetSource, RipReport, TrackCrcs, TrackRead};
    use spindle::cd::toc::Toc;
    use spindle::cd::verify::{MethodResult, Outcome, TrackVerdict};
    let n = files.len();
    let toc = Toc::from_audio_sample_counts(vec![750 * 588; n]).unwrap();
    let all = ctdb_matched.iter().all(|&m| m);
    spindle::import::sidecar::RipEntry {
        toc: toc.ctdb_toc(),
        metadata: DiscMetadata {
            source: MetadataSource::Manual,
            release_id: None,
            release_group_id: None,
            album: String::new(),
            album_artist: String::new(),
            date: None,
            label: None,
            catalog_number: None,
            barcode: None,
            disc_no: 1,
            disc_count: 1,
            category: None,
            tracks: (1..=n as u8)
                .map(|number| DiscTrackMetadata {
                    number,
                    title: String::new(),
                    artist: String::new(),
                    mb: None,
                })
                .collect(),
        },
        files: files.iter().map(|f| f.to_string()).collect(),
        log: "rip.log".into(),
        report: RipReport {
            drive: Some("TEST DRIVE".into()),
            device: "/dev/sr0".into(),
            read_offset: 6,
            offset_source: OffsetSource::Learned,
            started_at: 1_789_000_000,
            finished_at: 1_789_000_600,
            attempts: 1,
            encoder: "flac -8 --verify".into(),
            reads: vec![TrackRead::default(); n],
            crcs: (0..n as u32)
                .map(|i| TrackCrcs {
                    ar_v1: 1 + i,
                    ar_v2: 11 + i,
                    ctdb: 100 + i,
                })
                .collect(),
            ctdb: Some(MethodResult {
                outcome: if all {
                    Outcome::Verified
                } else {
                    Outcome::Mismatch
                },
                offset: 0,
                confidence: if all { 3 } else { 0 },
                tracks: ctdb_matched
                    .iter()
                    .enumerate()
                    .map(|(i, &matched)| TrackVerdict {
                        matched,
                        confidence: if matched { 3 } else { 0 },
                        crc: 100 + i as u32,
                        crc_v2: None,
                    })
                    .collect(),
            }),
            accuraterip: None,
            repaired_words: None,
        },
    }
}

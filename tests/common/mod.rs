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

/// ffmpeg でデコードしたときのフレーム数（1ch の s16 に落として数える）。ffmpeg が無ければ None
pub fn ffmpeg_frames(path: &Path) -> Option<usize> {
    let out = Command::new(ffmpeg()?)
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:a", "-ac", "1", "-f", "s16le", "-"])
        .output()
        .ok()?;
    assert!(
        out.status.success(),
        "ffmpeg が失敗: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(out.stdout.len() / 2)
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
        isrcs: vec![None; n],
        mcn: None,
        log: "rip.log".into(),
        report: RipReport {
            drive: Some("TEST DRIVE".into()),
            device: "/dev/sr0".into(),
            read_offset: 6,
            offset_source: OffsetSource::Learned,
            started_at: 1_789_000_000,
            finished_at: 1_789_000_600,
            attempts: 1,
            attempt_slips: Vec::new(),
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

/// [`zero_last_stts_delta`] のとき edit list（`elst`）をどうするか（D-89 追記）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditEnd {
    /// `elst` の終端も宣言長まで縮める。ffmpeg は長さ 0 のサンプルを捨てる（D-89 の 7 本と同じ形）
    AtDeclared,
    /// `elst` はそのまま（宣言長より後ろで終わる）。ffmpeg は長さ 0 のサンプルもデコードする
    /// （2026-09-26 の再移行で正規化に失敗した 46 本と同じ形）
    Beyond,
    /// `edts` を `free` に書き換えて edit list を消す。ffmpeg は長さ 0 のサンプルもデコードする
    Removed,
}

/// MP4 の音声トラックの末尾サンプルを「長さ 0」と宣言し直す（D-89 の回帰試験用）。
/// `stts` 最後の項の delta を 0 に、`mdhd` の duration をその分だけ減らす（どちらも同じ長さで上書き）。
/// 実機の ALAC（`stts` が `[(N, 4096), (1, 0)]`）と同じ形になる。edit list は `edit` に従う。
/// 末尾の項が 1 サンプルでなければ、その項を分けられないので panic する。宣言から外したフレーム数を返す
pub fn zero_last_stts_delta(path: &Path, edit: EditEnd) -> u32 {
    let mut data = std::fs::read(path).unwrap();
    let mut stts = None;
    let mut mdhd = None;
    let mut mvhd = None;
    let mut elst = None;
    let mut edts = None;
    find_boxes(&data, 0, data.len(), &mut |typ, body, end| match typ {
        b"stts" => stts = Some((body, end)),
        b"mdhd" => mdhd = Some(body),
        b"mvhd" => mvhd = Some(body),
        b"elst" => elst = Some(body),
        b"edts" => edts = Some(body),
        _ => {}
    });
    let (body, end) = stts.expect("stts が無い");
    let be32 = |d: &[u8], at: usize| u32::from_be_bytes(d[at..at + 4].try_into().unwrap());
    let count = be32(&data, body + 4) as usize;
    assert!(
        count > 0 && body + 8 + count * 8 <= end,
        "stts が壊れている"
    );
    let last = body + 8 + (count - 1) * 8;
    assert_eq!(be32(&data, last), 1, "末尾の stts 項が 1 サンプルでない");
    let dropped = be32(&data, last + 4);
    data[last + 4..last + 8].copy_from_slice(&0u32.to_be_bytes());
    let mdhd = mdhd.expect("mdhd が無い");
    assert_eq!(data[mdhd], 0, "mdhd が version 0 でない");
    let media_ts = be32(&data, mdhd + 12);
    let declared = be32(&data, mdhd + 16) - dropped;
    data[mdhd + 16..mdhd + 20].copy_from_slice(&declared.to_be_bytes());
    match edit {
        EditEnd::Beyond => {}
        EditEnd::AtDeclared => {
            let mvhd = mvhd.expect("mvhd が無い");
            assert_eq!(data[mvhd], 0, "mvhd が version 0 でない");
            let movie_ts = be32(&data, mvhd + 12);
            let seg = u64::from(declared) * u64::from(movie_ts);
            assert_eq!(
                seg % u64::from(media_ts),
                0,
                "宣言長が movie timescale で割り切れない"
            );
            let elst = elst.expect("elst が無い");
            assert_eq!(data[elst], 0, "elst が version 0 でない");
            assert_eq!(be32(&data, elst + 4), 1, "elst が 1 項でない");
            let seg = (seg / u64::from(media_ts)) as u32;
            data[elst + 8..elst + 12].copy_from_slice(&seg.to_be_bytes());
        }
        EditEnd::Removed => {
            let edts = edts.expect("edts が無い");
            data[edts - 4..edts].copy_from_slice(b"free");
        }
    }
    std::fs::write(path, data).unwrap();
    dropped
}

/// 音声トラックの edit list を 1 区間に書き換える（`mvhd` の timescale も）。D-89 追記 2 の試験用。
/// ffmpeg の muxer が書いた `elst`（version 0・1 項）を前提にする
pub fn set_edit(path: &Path, movie_timescale: u32, segment_duration: u32, media_time: i32) {
    let mut data = std::fs::read(path).unwrap();
    let mut mvhd = None;
    let mut elst = None;
    find_boxes(&data, 0, data.len(), &mut |typ, body, _| match typ {
        b"mvhd" => mvhd = Some(body),
        b"elst" => elst = Some(body),
        _ => {}
    });
    let mvhd = mvhd.expect("mvhd が無い");
    assert_eq!(data[mvhd], 0, "mvhd が version 0 でない");
    data[mvhd + 12..mvhd + 16].copy_from_slice(&movie_timescale.to_be_bytes());
    let elst = elst.expect("elst が無い");
    assert_eq!(data[elst], 0, "elst が version 0 でない");
    assert_eq!(
        &data[elst + 4..elst + 8],
        &1u32.to_be_bytes(),
        "elst が 1 項でない"
    );
    data[elst + 8..elst + 12].copy_from_slice(&segment_duration.to_be_bytes());
    data[elst + 12..elst + 16].copy_from_slice(&media_time.to_be_bytes());
    std::fs::write(path, data).unwrap();
}

fn find_boxes(data: &[u8], mut off: usize, end: usize, f: &mut dyn FnMut(&[u8; 4], usize, usize)) {
    while off + 8 <= end {
        let size = u32::from_be_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        let typ: [u8; 4] = data[off + 4..off + 8].try_into().unwrap();
        let (hdr, size) = match size {
            1 => (
                16,
                u64::from_be_bytes(data[off + 8..off + 16].try_into().unwrap()) as usize,
            ),
            0 => (8, end - off),
            s => (8, s),
        };
        let (body, stop) = (off + hdr, (off + size).min(end));
        f(&typ, body, stop);
        if matches!(
            &typ,
            b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"edts"
        ) {
            find_boxes(data, body, stop, f);
        }
        off += size.max(8);
    }
}

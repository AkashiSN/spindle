//! 音声フィンガープリント（SPEC §6「変更検出と版の遷移」、D-30）。
//!
//! | 形式 | 値 | 用途 |
//! |---|---|---|
//! | FLAC | STREAMINFO の MD5（[`flac_streaminfo_md5`]、デコード不要） | 同一性と音声版 |
//! | ALAC / WAV | デコードした PCM の MD5（[`decoded_pcm_md5`]） | 同上 |
//! | 非可逆 | demux したパケット列の SHA-256（[`packet_fp`]、デコードしない） | 音声版のみ |
//!
//! `decoded_pcm_md5` は FLAC エンコーダが STREAMINFO に書くのと同じ流儀（チャンネルインター
//! リーブ、リトルエンディアン、bps ぶんのバイト）で MD5 を取るので、同じ PCM なら
//! WAV / ALAC / FLAC で同じ `audio_md5` になる（WAV → FLAC 正規化で同一性が保たれる）。
//!
//! ALAC のデコード出力は、ffmpeg と同じく edit list の範囲に揃える（[`FrameLimit`]、D-89 と追記）。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use md5::{Digest, Md5};
use sha2::Sha256;
use symphonia::core::audio::sample::Sample;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, Track, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

use crate::media::mp4edit::{read_audio_edits, AudioEdit};

#[derive(Debug, thiserror::Error)]
pub enum FingerprintError {
    #[error("FLAC ではない（fLaC マーカーが無い）")]
    NotFlac,
    #[error("STREAMINFO が無いか壊れている")]
    BadStreamInfo,
    #[error("音声トラックが無い")]
    NoAudioTrack,
    #[error("対応していない形式: {0}")]
    Unsupported(String),
    #[error("デコードに失敗: {0}")]
    Decode(#[from] SymphoniaError),
    #[error("読み取りに失敗: {0}")]
    Io(#[from] std::io::Error),
    #[error("受け手が失敗: {0}")]
    Sink(anyhow::Error),
}

/// FLAC の STREAMINFO から非圧縮音声の MD5 を読む。全ゼロ（未設定）は `Ok(None)`。
/// 先頭の ID3v2 タグは読み飛ばす
pub fn flac_streaminfo_md5<R: Read + Seek>(r: R) -> Result<Option<[u8; 16]>, FingerprintError> {
    let (_, md5) = flac_streaminfo_md5_at(r)?;
    Ok((md5 != [0u8; 16]).then_some(md5))
}

/// STREAMINFO の内容（デコード不要）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlacStreamInfo {
    pub sample_rate: u32,
    pub channels: u32,
    pub bits_per_sample: u32,
    /// チャンネルをまたがないサンプル数。0 は不明
    pub total_samples: u64,
    /// 全ゼロは未設定
    pub md5: [u8; 16],
    /// MD5 16 バイトのファイル内オフセット
    pub md5_offset: u64,
}

/// STREAMINFO を読む。先頭の ID3v2 タグは読み飛ばす
pub fn flac_streaminfo<R: Read + Seek>(mut r: R) -> Result<FlacStreamInfo, FingerprintError> {
    let (info, end) = read_streaminfo(&mut r)?;
    // 10..18: sample rate 20 bit、channels−1 3 bit、bps−1 5 bit、total samples 36 bit
    let packed = u64::from_be_bytes([
        info[10], info[11], info[12], info[13], info[14], info[15], info[16], info[17],
    ]);
    let sample_rate = (packed >> 44) as u32;
    let channels = ((packed >> 41) & 0x7) as u32 + 1;
    let bits_per_sample = ((packed >> 36) & 0x1f) as u32 + 1;
    let total_samples = packed & 0xf_ffff_ffff;
    let mut md5 = [0u8; 16];
    md5.copy_from_slice(&info[18..34]);
    Ok(FlacStreamInfo {
        sample_rate,
        channels,
        bits_per_sample,
        total_samples,
        md5,
        md5_offset: end - 16,
    })
}

/// STREAMINFO の MD5 16 バイトの位置（ファイル先頭からのオフセット）と現在値。全ゼロもそのまま返す。
/// MD5 の補填（P1-5b）はこの位置だけを書き換える
pub fn flac_streaminfo_md5_at<R: Read + Seek>(
    mut r: R,
) -> Result<(u64, [u8; 16]), FingerprintError> {
    let (info, end) = read_streaminfo(&mut r)?;
    let mut md5 = [0u8; 16];
    md5.copy_from_slice(&info[18..34]);
    // 読み終えた位置から 16 バイト戻ったところが MD5
    Ok((end - 16, md5))
}

/// STREAMINFO の 34 バイトと、読み終えた位置
fn read_streaminfo<R: Read + Seek>(mut r: R) -> Result<([u8; 34], u64), FingerprintError> {
    let mut marker = [0u8; 4];
    r.read_exact(&mut marker)?;
    if marker[..3] == *b"ID3" {
        // ID3v2 ヘッダ: "ID3" ver(2) flags(1) size(4, synchsafe)。footer フラグなら +10。
        // marker で "ID3" + ver 上位 1 バイトを読んでいるので残りは 6 バイト
        let mut rest = [0u8; 6];
        r.read_exact(&mut rest)?;
        let flags = rest[1];
        let size = rest[2..6]
            .iter()
            .fold(0u64, |acc, b| (acc << 7) | u64::from(b & 0x7f));
        let skip = size + if flags & 0x10 != 0 { 10 } else { 0 };
        r.seek(SeekFrom::Current(skip as i64))?;
        r.read_exact(&mut marker)?;
    }
    if &marker != b"fLaC" {
        return Err(FingerprintError::NotFlac);
    }
    // STREAMINFO は必ず最初のメタデータブロック
    let mut header = [0u8; 4];
    r.read_exact(&mut header)?;
    let block_type = header[0] & 0x7f;
    let len = u32::from_be_bytes([0, header[1], header[2], header[3]]);
    if block_type != 0 || len != 34 {
        return Err(FingerprintError::BadStreamInfo);
    }
    let mut info = [0u8; 34];
    r.read_exact(&mut info)
        .map_err(|_| FingerprintError::BadStreamInfo)?;
    let end = r.stream_position()?;
    Ok((info, end))
}

/// プロセス内デコードの出力範囲を ffmpeg に揃える（D-89 と追記）。
///
/// 正規化の照合（D-46）は元ファイルを symphonia、生成物を ffmpeg で読むので、同じ ALAC から出る PCM の
/// 範囲が食い違うと必ず失敗する。symphonia 0.6.1 の MP4 読みは edit list（`elst`）を読まず全パケットを
/// 返すが、ffmpeg は edit list に従う。本番の ffmpeg（5.1）の規則は次のとおりで、実 ALAC から作った
/// ファイルで確かめた（D-89 追記 2）:
///
/// - 先頭は media_time サンプルちょうど削る
/// - 終端（media_time + 区間の長さを media timescale へ換算して四捨五入）以降で**始まる**パケットは捨て、
///   終端をまたぐパケットは丸ごと残す。`stts` 末尾の長さ 0 のサンプルも同じ規則で、始まりが終端より前なら残る
/// - edit list が無ければ何も削らない
///
/// 単純な形（1 区間・media_time ≥ 0・等速）でない edit list は再現せず、何も削らない（照合で止まるだけで
/// 音声は失わない）。**ALAC だけ**に掛ける: AAC は正規化の対象でなく、既存の RG 値を変えない。delay /
/// padding を持つトラックも掛けない
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLimit {
    /// 出力の先頭で捨てる残りフレーム数
    skip: u64,
    /// この位置（トラックの時間軸 = サンプル）以降で始まるパケットは読まない
    end: Option<i64>,
}

impl FrameLimit {
    /// 何も削らない
    pub const NONE: Self = Self { skip: 0, end: None };

    pub fn new(skip: u64, end: Option<i64>) -> Self {
        Self { skip, end }
    }

    /// `edits` は同じファイルから [`read_audio_edits`] で読んだもの。symphonia が選んだトラック
    /// （`Track::id` = `tkhd` の track ID）の edit list だけを使う
    pub fn of_track(track: &Track, edits: &[AudioEdit]) -> Self {
        use symphonia::core::codecs::audio::well_known::CODEC_ID_ALAC;
        let Some(params) = track.codec_params.as_ref().and_then(|p| p.audio()) else {
            return Self::NONE;
        };
        let trimmed = track.delay.unwrap_or(0) != 0 || track.padding.unwrap_or(0) != 0;
        if params.codec != CODEC_ID_ALAC || trimmed {
            return Self::NONE;
        }
        // パケットの pts を「サンプル」として比べるので、トラックの時間軸がサンプル単位のときだけ
        let Some(edit) = edits
            .iter()
            .find(|e| e.track_id == track.id && Some(e.media_timescale) == params.sample_rate)
        else {
            return Self::NONE;
        };
        match edit.window() {
            Some(w) => Self::new(w.start, Some(w.end)),
            None => Self::NONE,
        }
    }

    /// 開始位置 `pts` のパケットで打ち切る（以降のパケットは読まない）
    pub fn stops_at(&self, pts: i64) -> bool {
        self.end.is_some_and(|end| pts >= end)
    }

    /// デコードした `frames` フレームの塊のうち残す範囲を返し、先頭の削り残りを減らす
    pub fn take(&mut self, frames: usize) -> std::ops::Range<usize> {
        let skip = self.skip.min(frames as u64);
        self.skip -= skip;
        skip as usize..frames
    }
}

/// MP4 なら音声トラックごとの edit list を読み、`file` を先頭へ戻す（D-89 追記）
pub fn read_edits_and_rewind(file: &mut File) -> std::io::Result<Vec<AudioEdit>> {
    let edits = read_audio_edits(file);
    file.seek(SeekFrom::Start(0))?;
    Ok(edits)
}

fn open_format(file: File, ext: Option<&str>) -> Result<Box<dyn FormatReader>, FingerprintError> {
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if let Some(ext) = ext {
        hint.with_extension(ext);
    }
    let reader = symphonia::default::get_probe().probe(
        &hint,
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    )?;
    Ok(reader)
}

/// 16 bit の可逆ファイルをプロセス内でデコードし、インターリーブ i16 をチャンクごとに `sink` に渡す
/// （CD の CRC 計算用。P2-9）。bps が 16 でなければ `Unsupported`。返り値はフレーム数
/// （チャンネルをまたがないサンプル数）。ブロッキングなので `spawn_blocking` で呼ぶ
pub fn decode_s16(
    mut file: File,
    ext: Option<&str>,
    mut sink: impl FnMut(&[i16]) -> anyhow::Result<()>,
) -> Result<u64, FingerprintError> {
    let edits = read_edits_and_rewind(&mut file)?;
    let mut reader = open_format(file, ext)?;
    let track = reader
        .default_track(TrackType::Audio)
        .ok_or(FingerprintError::NoAudioTrack)?;
    let track_id = track.id;
    let mut limit = FrameLimit::of_track(track, &edits);
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or(FingerprintError::NoAudioTrack)?
        .clone();
    let bits = params.bits_per_sample.or_else(|| alac_bit_depth(&params));
    if bits != Some(16) {
        return Err(FingerprintError::Unsupported(format!(
            "16 bit ではない: {}",
            bits.map(|b| b.to_string()).unwrap_or_else(|| "不明".into())
        )));
    }
    let channels = params.channels.as_ref().map(|c| c.count()).unwrap_or(0) as u64;
    if channels == 0 {
        return Err(FingerprintError::Unsupported("チャンネル数が不明".into()));
    }
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|e| FingerprintError::Unsupported(e.to_string()))?;
    let mut interleaved: Vec<i32> = Vec::new();
    let mut out: Vec<i16> = Vec::new();
    let mut frames = 0u64;
    while let Some(packet) = reader.next_packet()? {
        if packet.track_id != track_id {
            continue;
        }
        if limit.stops_at(packet.pts.get()) {
            break;
        }
        let buf = decoder.decode(&packet)?;
        // デコーダは 32 bit 左詰めで返すので 16 bit へ戻す
        buf.copy_to_vec_interleaved(&mut interleaved);
        let keep = limit.take(interleaved.len() / channels as usize);
        let keep = keep.start * channels as usize..keep.end * channels as usize;
        out.clear();
        out.extend(interleaved[keep].iter().map(|&s| (s >> 16) as i16));
        frames += out.len() as u64 / channels;
        sink(&out).map_err(FingerprintError::Sink)?;
    }
    Ok(frames)
}

/// ALAC / WAV をデコードし、PCM の MD5 を FLAC の STREAMINFO と同じ流儀で算出する
pub fn decoded_pcm_md5(mut file: File, ext: Option<&str>) -> Result<[u8; 16], FingerprintError> {
    let edits = read_edits_and_rewind(&mut file)?;
    let mut reader = open_format(file, ext)?;
    let track = reader
        .default_track(TrackType::Audio)
        .ok_or(FingerprintError::NoAudioTrack)?;
    let track_id = track.id;
    let mut limit = FrameLimit::of_track(track, &edits);
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or(FingerprintError::NoAudioTrack)?
        .clone();
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|e| FingerprintError::Unsupported(e.to_string()))?;

    let mut hasher = Md5::new();
    let mut interleaved: Vec<i32> = Vec::new();
    while let Some(packet) = reader.next_packet()? {
        if packet.track_id != track_id {
            continue;
        }
        if limit.stops_at(packet.pts.get()) {
            break;
        }
        let buf = decoder.decode(&packet)?;
        // デコーダはサンプルを 32 ビット幅に左詰めで返す（S16 は << 16、i24 は << 8、
        // ALAC は bit_depth ぶん左シフト済み）。i32 に揃えてから元の bps へ戻す
        let bits = params
            .bits_per_sample
            .or_else(|| alac_bit_depth(&params))
            .unwrap_or_else(|| effective_bits(&buf));
        let bits = bits.clamp(8, 32);
        let bytes = bits.div_ceil(8) as usize;
        let shift = 32 - bits;
        let channels = buf.spec().channels().count().max(1);
        buf.copy_to_vec_interleaved(&mut interleaved);
        let keep = limit.take(interleaved.len() / channels);
        for s in &interleaved[keep.start * channels..keep.end * channels] {
            let v = *s >> shift;
            hasher.update(&v.to_le_bytes()[..bytes]);
        }
    }
    Ok(hasher.finalize().into())
}

/// ALAC は `bits_per_sample` が demuxer から来ないので magic cookie（`extra_data`）から読む。
/// 先頭に `frma` / `alac` atom（各 12 バイト）が付くことがある。bit depth は cookie の 5 バイト目
fn alac_bit_depth(params: &symphonia::core::codecs::audio::AudioCodecParameters) -> Option<u32> {
    use symphonia::core::codecs::audio::well_known::CODEC_ID_ALAC;
    if params.codec != CODEC_ID_ALAC {
        return None;
    }
    let mut cookie: &[u8] = params.extra_data.as_deref()?;
    for atom in [b"frma", b"alac"] {
        if cookie.len() >= 12 && &cookie[4..8] == atom {
            cookie = &cookie[12..];
        }
    }
    let depth = *cookie.get(5)?;
    (8..=32).contains(&depth).then_some(u32::from(depth))
}

fn effective_bits(buf: &symphonia::core::audio::GenericAudioBufferRef<'_>) -> u32 {
    use symphonia::core::audio::GenericAudioBufferRef as B;
    match buf {
        B::U8(_) | B::S8(_) => u8::EFF_BITS,
        B::U16(_) | B::S16(_) => i16::EFF_BITS,
        B::U24(_) | B::S24(_) => symphonia::core::audio::sample::i24::EFF_BITS,
        _ => 32,
    }
}

/// 非可逆のエンコード済みパケット列（demux のみ）の SHA-256。
/// タグの書き換えでコンテナのサイズや配置が変わっても、音声パケットが同じなら同じ値
pub fn packet_fp(file: File, ext: Option<&str>) -> Result<[u8; 32], FingerprintError> {
    let mut reader = open_format(file, ext)?;
    let track_id = reader
        .default_track(TrackType::Audio)
        .ok_or(FingerprintError::NoAudioTrack)?
        .id;
    let mut hasher = Sha256::new();
    while let Some(packet) = reader.next_packet()? {
        if packet.track_id != track_id {
            continue;
        }
        hasher.update(&packet.data);
    }
    Ok(hasher.finalize().into())
}

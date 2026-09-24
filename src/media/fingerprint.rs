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
//! ALAC のデコード出力はコンテナの宣言長で打ち切る（[`FrameLimit`]、D-89）。

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

/// コンテナが宣言したトラック長（フレーム数）でデコード出力を打ち切るための残り枠（D-89）。
///
/// symphonia 0.6.1 の MP4 読みは `stts` 末尾の長さ 0 のサンプルもパケットとして返すが、宣言長
/// （`mdhd` の duration）はそれを含まない。ffmpeg は宣言どおり捨てるので、打ち切らないと同じ ALAC から
/// 出る PCM の長さが食い違い、正規化の照合（D-46）が失敗する。宣言長は delay / padding を除いた再生
/// フレーム数で、パケットの trim は適用していないので、delay / padding を持つトラック（LAME ヘッダ付きの
/// MP3、Opus 等）は打ち切らない。宣言長が無ければ打ち切らない。
///
/// [`Self::of_track`] は **ALAC だけ**に掛ける。symphonia の MP4 は edit list を読まず、AAC の encoder
/// delay を `delay` / `padding` に載せないので、AAC の宣言長（`stts` の合計）は trim 前でも再生長でもなく、
/// 打ち切ると既存の RG 値が中途半端な長さで変わる。FLAC / WAV / AIFF は宣言長とデコード長が一致する
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLimit {
    remaining: Option<u64>,
}

impl FrameLimit {
    pub fn new(num_frames: Option<u64>, delay: Option<u32>, padding: Option<u32>) -> Self {
        let trimmed = delay.unwrap_or(0) != 0 || padding.unwrap_or(0) != 0;
        Self {
            remaining: if trimmed { None } else { num_frames },
        }
    }

    pub fn of_track(track: &Track) -> Self {
        use symphonia::core::codecs::audio::well_known::CODEC_ID_ALAC;
        let is_alac = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .is_some_and(|p| p.codec == CODEC_ID_ALAC);
        if !is_alac {
            return Self { remaining: None };
        }
        Self::new(track.num_frames, track.delay, track.padding)
    }

    /// `frames` フレームの塊のうち残すフレーム数を返し、枠を減らす
    pub fn take(&mut self, frames: usize) -> usize {
        match &mut self.remaining {
            None => frames,
            Some(rem) => {
                let keep = (*rem).min(frames as u64);
                *rem -= keep;
                keep as usize
            }
        }
    }

    /// 枠を使い切った（以降のパケットは読まなくてよい）
    pub fn exhausted(&self) -> bool {
        self.remaining == Some(0)
    }
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
    file: File,
    ext: Option<&str>,
    mut sink: impl FnMut(&[i16]) -> anyhow::Result<()>,
) -> Result<u64, FingerprintError> {
    let mut reader = open_format(file, ext)?;
    let track = reader
        .default_track(TrackType::Audio)
        .ok_or(FingerprintError::NoAudioTrack)?;
    let track_id = track.id;
    let mut limit = FrameLimit::of_track(track);
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
        if limit.exhausted() {
            break;
        }
        if packet.track_id != track_id {
            continue;
        }
        let buf = decoder.decode(&packet)?;
        // デコーダは 32 bit 左詰めで返すので 16 bit へ戻す
        buf.copy_to_vec_interleaved(&mut interleaved);
        let keep = limit.take(interleaved.len() / channels as usize) * channels as usize;
        out.clear();
        out.extend(interleaved[..keep].iter().map(|&s| (s >> 16) as i16));
        frames += out.len() as u64 / channels;
        sink(&out).map_err(FingerprintError::Sink)?;
    }
    Ok(frames)
}

/// ALAC / WAV をデコードし、PCM の MD5 を FLAC の STREAMINFO と同じ流儀で算出する
pub fn decoded_pcm_md5(file: File, ext: Option<&str>) -> Result<[u8; 16], FingerprintError> {
    let mut reader = open_format(file, ext)?;
    let track = reader
        .default_track(TrackType::Audio)
        .ok_or(FingerprintError::NoAudioTrack)?;
    let track_id = track.id;
    let mut limit = FrameLimit::of_track(track);
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
        if limit.exhausted() {
            break;
        }
        if packet.track_id != track_id {
            continue;
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
        let keep = limit.take(interleaved.len() / channels) * channels;
        for s in &interleaved[..keep] {
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

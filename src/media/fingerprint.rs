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

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use md5::{Digest, Md5};
use sha2::Sha256;
use symphonia::core::audio::sample::Sample;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
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
}

/// FLAC の STREAMINFO から非圧縮音声の MD5 を読む。全ゼロ（未設定）は `Ok(None)`。
/// 先頭の ID3v2 タグは読み飛ばす
pub fn flac_streaminfo_md5<R: Read + Seek>(r: R) -> Result<Option<[u8; 16]>, FingerprintError> {
    let (_, md5) = flac_streaminfo_md5_at(r)?;
    Ok((md5 != [0u8; 16]).then_some(md5))
}

/// STREAMINFO の MD5 16 バイトの位置（ファイル先頭からのオフセット）と現在値。全ゼロもそのまま返す。
/// MD5 の補填（P1-5b）はこの位置だけを書き換える
pub fn flac_streaminfo_md5_at<R: Read + Seek>(
    mut r: R,
) -> Result<(u64, [u8; 16]), FingerprintError> {
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
    let mut md5 = [0u8; 16];
    md5.copy_from_slice(&info[18..34]);
    // 読み終えた位置から 16 バイト戻ったところが MD5
    let end = r.stream_position()?;
    Ok((end - 16, md5))
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

/// ALAC / WAV をデコードし、PCM の MD5 を FLAC の STREAMINFO と同じ流儀で算出する
pub fn decoded_pcm_md5(file: File, ext: Option<&str>) -> Result<[u8; 16], FingerprintError> {
    let mut reader = open_format(file, ext)?;
    let track = reader
        .default_track(TrackType::Audio)
        .ok_or(FingerprintError::NoAudioTrack)?;
    let track_id = track.id;
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
        buf.copy_to_vec_interleaved(&mut interleaved);
        for s in &interleaved {
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

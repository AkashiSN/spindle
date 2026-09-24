//! PCM デコード（SPEC §15 `media/decode.rs`）。ラウドネス解析のように「サンプルを順に流す」
//! 用途向けで、全体をメモリに置かない。
//!
//! - symphonia が扱える形式（FLAC / ALAC / WAV / AIFF / MP3 / AAC / Vorbis）はプロセス内で
//!   デコードする（`spawn_blocking`）
//! - symphonia がデコーダを持たない形式（**Opus**）は ffmpeg に `f32le` で吐かせ、stdout を
//!   チャンクのまま受け取る（[`ExternalCommand::stdout_channel`]）。チャンネル数とレートは
//!   symphonia の demux（OpusHead）から取る
//! - symphonia が demux もできない形式（WavPack / APE）は lofty のプロパティからチャンネル数と
//!   レートを取り、同じく ffmpeg に回す
//!
//! プロセス内の経路は ALAC に限りコンテナの宣言長で打ち切る（[`FrameLimit`]、D-89。ffmpeg は自分で打ち切る）。
//!
//! どちらの経路でも [`PcmSink`] には `start(info)` → `push(interleaved f32)` の順で同じ形で流れる。
//! サンプルは -1.0..1.0 のインターリーブ

use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::jobs::process::{ExternalCommand, PathStyle, ProcessError};
use crate::media::fingerprint::FrameLimit;

/// ffmpeg デコードの上限。長尺の 24/96 でも数分だが NAS の CPU は遅い
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(1800);
/// ffmpeg → 消費側のチャネル深さ（64KiB チャンク）
const CHANNEL_DEPTH: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmInfo {
    pub channels: u32,
    pub sample_rate: u32,
}

/// デコードしたサンプルの受け手。`start` が 1 度、その後 `push` が 0 回以上呼ばれる。
/// ブロッキングスレッドで呼ばれるので、計算してよい
pub trait PcmSink: Send + 'static {
    fn start(&mut self, info: &PcmInfo) -> anyhow::Result<()>;
    fn push(&mut self, interleaved: &[f32]) -> anyhow::Result<()>;
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("音声トラックが無い")]
    NoAudioTrack,
    #[error("形式を判別できない: {0}")]
    Probe(SymphoniaError),
    #[error("デコードに失敗: {0}")]
    Decode(SymphoniaError),
    #[error("対応していない形式: {0}")]
    Unsupported(String),
    #[error("受け手が失敗: {0}")]
    Sink(anyhow::Error),
    #[error(transparent)]
    Process(ProcessError),
    #[error("キャンセルされた")]
    Cancelled,
    #[error("I/O エラー: {0}")]
    Io(#[from] std::io::Error),
}

impl From<ProcessError> for DecodeError {
    fn from(e: ProcessError) -> Self {
        match e {
            ProcessError::Cancelled => DecodeError::Cancelled,
            other => DecodeError::Process(other),
        }
    }
}

impl DecodeError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, DecodeError::Cancelled)
    }
}

#[derive(Debug, Clone)]
pub struct Decoder {
    ffmpeg: PathBuf,
}

/// プロセス内デコードの結果。デコーダが無ければ ffmpeg 用の情報を返す
enum InProcess<S> {
    Done(PcmInfo, S),
    Fallback(PcmInfo, S),
}

impl Decoder {
    pub fn new(ffmpeg: impl AsRef<Path>) -> Self {
        Self {
            ffmpeg: ffmpeg.as_ref().to_path_buf(),
        }
    }

    /// `file`（読み取りで開いたもの）をデコードして `sink` に流す。`ext` は判別のヒント。
    /// 終わったら [`PcmInfo`] と `sink` を返す
    pub async fn decode<S: PcmSink>(
        &self,
        file: File,
        ext: Option<&str>,
        sink: S,
        token: &CancellationToken,
    ) -> Result<(PcmInfo, S), DecodeError> {
        let ext = ext.map(str::to_owned);
        let ffmpeg_input = file.try_clone()?;
        let probe_file = file.try_clone()?;
        let in_token = token.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            decode_in_process(file, probe_file, ext.as_deref(), sink, &in_token)
        })
        .await
        .map_err(|e| std::io::Error::other(format!("デコードタスクが異常終了: {e}")))??;
        match outcome {
            InProcess::Done(info, sink) => Ok((info, sink)),
            InProcess::Fallback(info, sink) => {
                self.decode_with_ffmpeg(ffmpeg_input, info, sink, token)
                    .await
            }
        }
    }

    async fn decode_with_ffmpeg<S: PcmSink>(
        &self,
        mut file: File,
        info: PcmInfo,
        mut sink: S,
        token: &CancellationToken,
    ) -> Result<(PcmInfo, S), DecodeError> {
        // demux で進んだオフセットを戻す（`/dev/stdin` の再 open は先頭から読むが念のため）
        file.seek(SeekFrom::Start(0))?;
        sink.start(&info).map_err(DecodeError::Sink)?;
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(CHANNEL_DEPTH);
        let consumer = tokio::task::spawn_blocking(move || -> Result<S, DecodeError> {
            let mut carry: Vec<u8> = Vec::new();
            let mut samples: Vec<f32> = Vec::new();
            while let Some(chunk) = rx.blocking_recv() {
                let data = if carry.is_empty() {
                    chunk
                } else {
                    carry.extend_from_slice(&chunk);
                    std::mem::take(&mut carry)
                };
                let (whole, rest) = data.as_chunks::<4>();
                samples.clear();
                samples.extend(whole.iter().map(|b| f32::from_le_bytes(*b)));
                carry.extend_from_slice(rest);
                sink.push(&samples).map_err(DecodeError::Sink)?;
            }
            Ok(sink)
        });
        let run = ExternalCommand::new(&self.ffmpeg)
            .path_style(PathStyle::DotSlash)
            .args([
                "-hide_banner",
                "-nostdin",
                "-loglevel",
                "error",
                "-i",
                "/dev/stdin",
                "-map",
                "0:a:0",
                "-vn",
                "-f",
                "f32le",
                "pipe:1",
            ])
            .stdin_file(file)
            .stdout_channel(tx)
            .timeout(FFMPEG_TIMEOUT)
            .run(token)
            .await;
        let consumed = consumer
            .await
            .map_err(|e| std::io::Error::other(format!("消費タスクが異常終了: {e}")))?;
        // 受け手の失敗（sink のエラー）を優先して報告する。ffmpeg 側はそれで EPIPE になっている
        match (run, consumed) {
            (_, Err(DecodeError::Sink(e))) => Err(DecodeError::Sink(e)),
            (Err(e), _) => Err(e.into()),
            (Ok(_), Err(e)) => Err(e),
            (Ok(_), Ok(sink)) => Ok((info, sink)),
        }
    }
}

/// symphonia が扱えないファイルのチャンネル数とレートを lofty から読む
fn properties_via_lofty(mut file: File, ext: Option<&str>) -> Option<PcmInfo> {
    // dup した FD はオフセットを共有する。probe で進んだ分を戻す
    file.seek(SeekFrom::Start(0)).ok()?;
    let af = crate::domain::tags::read_audio_file(file, ext).ok()?;
    Some(PcmInfo {
        channels: af.channels?,
        sample_rate: af.sample_rate?,
    })
}

fn decode_in_process<S: PcmSink>(
    file: File,
    probe_file: File,
    ext: Option<&str>,
    mut sink: S,
    token: &CancellationToken,
) -> Result<InProcess<S>, DecodeError> {
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if let Some(ext) = ext {
        hint.with_extension(ext);
    }
    let mut reader = match symphonia::default::get_probe().probe(
        &hint,
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    ) {
        Ok(r) => r,
        Err(e @ SymphoniaError::Unsupported(_)) => {
            // demuxer が無い（WavPack / APE）。lofty が属性を読めれば ffmpeg に回す
            let Some(info) = properties_via_lofty(probe_file, ext) else {
                return Err(DecodeError::Probe(e));
            };
            return Ok(InProcess::Fallback(info, sink));
        }
        Err(e) => return Err(DecodeError::Probe(e)),
    };
    let track = reader
        .default_track(TrackType::Audio)
        .ok_or(DecodeError::NoAudioTrack)?;
    let track_id = track.id;
    let mut limit = FrameLimit::of_track(track);
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or(DecodeError::NoAudioTrack)?
        .clone();
    let info_of = |channels: usize, rate: u32| PcmInfo {
        channels: channels as u32,
        sample_rate: rate,
    };
    let mut decoder = match symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
    {
        Ok(d) => d,
        Err(SymphoniaError::Unsupported(what)) => {
            // Opus など。demux の情報で ffmpeg に回す
            let (Some(ch), Some(rate)) = (params.channels.as_ref(), params.sample_rate) else {
                return Err(DecodeError::Unsupported(format!(
                    "{what}（チャンネル数かレートが不明で ffmpeg にも回せない）"
                )));
            };
            return Ok(InProcess::Fallback(info_of(ch.count(), rate), sink));
        }
        Err(e) => return Err(DecodeError::Decode(e)),
    };

    let mut info: Option<PcmInfo> = None;
    let mut interleaved: Vec<f32> = Vec::new();
    loop {
        if token.is_cancelled() {
            return Err(DecodeError::Cancelled);
        }
        if limit.exhausted() {
            break;
        }
        let Some(packet) = reader.next_packet().map_err(DecodeError::Decode)? else {
            break;
        };
        if packet.track_id != track_id {
            continue;
        }
        let buf = decoder.decode(&packet).map_err(DecodeError::Decode)?;
        let spec = buf.spec();
        let this = info_of(spec.channels().count(), spec.rate());
        match info {
            None => {
                sink.start(&this).map_err(DecodeError::Sink)?;
                info = Some(this);
            }
            Some(first) if first != this => {
                return Err(DecodeError::Unsupported(format!(
                    "途中でチャンネル数かレートが変わった: {first:?} → {this:?}"
                )));
            }
            Some(_) => {}
        }
        buf.copy_to_vec_interleaved(&mut interleaved);
        let channels = this.channels.max(1) as usize;
        let keep = limit.take(interleaved.len() / channels) * channels;
        sink.push(&interleaved[..keep]).map_err(DecodeError::Sink)?;
    }
    let info = match info {
        Some(i) => i,
        None => {
            // パケットが 1 つも無い（空のストリーム）。demux の情報で start だけ呼ぶ
            let (Some(ch), Some(rate)) = (params.channels.as_ref(), params.sample_rate) else {
                return Err(DecodeError::NoAudioTrack);
            };
            let i = info_of(ch.count(), rate);
            sink.start(&i).map_err(DecodeError::Sink)?;
            i
        }
    };
    Ok(InProcess::Done(info, sink))
}

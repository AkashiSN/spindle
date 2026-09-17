//! FLAC エンコード（ロスレス正規化。SPEC §7.4、D-10 / D-11 / D-45）。
//!
//! ```text
//! 元ファイル（WAV / ALAC / AIFF。root から開いた FD）
//!   → ffmpeg でデコードして tmp の WAV へ（stdin に FD を繋ぎ `/dev/stdin` を入力にする。
//!     MP4 は moov が末尾にあると pipe では読めないので、seek できる FD が要る）
//!   → flac -<level> --verify で tmp の FLAC へ
//!   → STREAMINFO の MD5 を読む（呼び出し側が元の PCM MD5 と突き合わせる）
//! ```
//!
//! デコードは ffmpeg、MD5 の照合値は symphonia（[`crate::media::fingerprint::decoded_pcm_md5`]）
//! と、独立した 2 つのデコーダで同じ PCM が出ることを要求する。どちらかが元ファイルを読み違えれば
//! 不一致になり、正規化は中止される。
//!
//! 作業ファイルは `[paths].data/tmp` に置く（Library には置かない。完成した FLAC は呼び出し側が
//! Library の tmp へコピーして rename する）。失敗・キャンセルでは [`TempGuard`] が消す

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::jobs::process::{ExternalCommand, PathStyle, ProcessError};
use crate::jobs::TempGuard;
use crate::media::fingerprint::{self, FingerprintError};

/// 作業ファイル名の前置き
const TMP_PREFIX: &str = "spindle-normalize-";
/// デコード・エンコードそれぞれの上限。24/96 の長尺でも数分で終わるが、NAS の CPU は遅い
const STEP_TIMEOUT: Duration = Duration::from_secs(1800);

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("対応していないビット深度: {0}")]
    UnsupportedBitDepth(u32),
    #[error("作業ディレクトリを用意できない: {path}: {source}")]
    TmpDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error("生成した FLAC を読めない: {0}")]
    Output(#[from] FingerprintError),
    #[error("I/O エラー: {0}")]
    Io(#[from] std::io::Error),
}

impl EncodeError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, EncodeError::Process(ProcessError::Cancelled))
    }
}

/// エンコードした FLAC（tmp）。`guard` を drop すると消える
#[derive(Debug)]
pub struct EncodedFlac {
    pub guard: TempGuard,
    /// STREAMINFO の MD5。`flac` は必ず書くので `None` は異常
    pub streaminfo_md5: Option<[u8; 16]>,
}

#[derive(Debug, Clone)]
pub struct FlacEncoder {
    ffmpeg: PathBuf,
    flac: PathBuf,
    compression: u8,
    tmp_dir: PathBuf,
}

impl FlacEncoder {
    pub fn new(
        ffmpeg: impl Into<PathBuf>,
        flac: impl Into<PathBuf>,
        compression: u8,
        tmp_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            ffmpeg: ffmpeg.into(),
            flac: flac.into(),
            compression: compression.min(8),
            tmp_dir: tmp_dir.into(),
        }
    }

    pub fn tmp_dir(&self) -> &Path {
        &self.tmp_dir
    }

    /// ビット深度に対応する ffmpeg の PCM エンコーダ名
    pub fn pcm_codec(bit_depth: u32) -> Result<&'static str, EncodeError> {
        Ok(match bit_depth {
            8 => "pcm_u8",
            16 => "pcm_s16le",
            24 => "pcm_s24le",
            32 => "pcm_s32le",
            other => return Err(EncodeError::UnsupportedBitDepth(other)),
        })
    }

    fn tmp_path(&self, ext: &str) -> Result<PathBuf, EncodeError> {
        let mut buf = [0u8; 8];
        getrandom::fill(&mut buf).map_err(|e| std::io::Error::other(e.to_string()))?;
        let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
        Ok(self.tmp_dir.join(format!("{TMP_PREFIX}{hex}.{ext}")))
    }

    /// `source`（読み取りで開いた元ファイル）を FLAC にエンコードする。`token` が倒れたら
    /// 子プロセスごと止めて [`ProcessError::Cancelled`]
    pub async fn encode(
        &self,
        source: File,
        bit_depth: u32,
        token: &CancellationToken,
    ) -> Result<EncodedFlac, EncodeError> {
        let pcm_codec = Self::pcm_codec(bit_depth)?;
        std::fs::create_dir_all(&self.tmp_dir).map_err(|source| EncodeError::TmpDir {
            path: self.tmp_dir.clone(),
            source,
        })?;
        let wav = TempGuard::new(self.tmp_path("wav")?);
        let flac = TempGuard::new(self.tmp_path("flac")?);

        // 1. デコード。メタデータは移さない（タグは lofty で別に写す。RIFF INFO を残すと flac が
        //    未知チャンクとして扱う）。bitexact でエンコーダ名の INFO も抑える
        ExternalCommand::new(&self.ffmpeg)
            .path_style(PathStyle::DotSlash)
            .args([
                "-hide_banner",
                "-nostdin",
                "-loglevel",
                "error",
                "-y",
                "-i",
                "/dev/stdin",
                "-map",
                "0:a:0",
                "-vn",
                "-map_metadata",
                "-1",
                "-fflags",
                "+bitexact",
                "-flags",
                "+bitexact",
                "-c:a",
                pcm_codec,
                "-f",
                "wav",
            ])
            .path_arg(wav.path())
            .stdin_file(source)
            .timeout(STEP_TIMEOUT)
            .run(token)
            .await?;

        // 2. エンコード。--verify はエンコード結果をデコードして入力と照合する（flac 自身の検査。
        //    元ファイルとの照合は STREAMINFO の MD5 で呼び出し側が行う）
        let level = format!("-{}", self.compression);
        ExternalCommand::new(&self.flac)
            .path_style(PathStyle::DoubleDash)
            .args([level.as_str(), "--verify", "--silent", "-o"])
            .arg(flac.path())
            .path_arg(wav.path())
            .timeout(STEP_TIMEOUT)
            .run(token)
            .await?;
        drop(wav);

        let streaminfo_md5 = {
            let path = flac.path().to_path_buf();
            tokio::task::spawn_blocking(move || -> Result<Option<[u8; 16]>, EncodeError> {
                let f = File::open(&path)?;
                Ok(fingerprint::flac_streaminfo_md5(f)?)
            })
            .await
            .map_err(|e| std::io::Error::other(format!("MD5 読み取りタスクが異常終了: {e}")))??
        };
        Ok(EncodedFlac {
            guard: flac,
            streaminfo_md5,
        })
    }
}

// ---------------------------------------------------------------- Derived の Opus

/// 作業ファイル名の前置き（Derived 用）
const OPUS_TMP_PREFIX: &str = "spindle-transcode-";

/// Derived の Opus エンコード（SPEC §7.6、D-9、D-51）。
///
/// ```text
/// Library の可逆（root から開いた FD） → ffmpeg で tmp の WAV へ（FlacEncoder と同じ経路）
///   → opusenc --vbr --music で tmp の Opus へ（コメント・画像は移さない。タグは呼び出し側が
///     lofty で書く。`domain::tags::write_opus_tags`）
/// ```
#[derive(Debug, Clone)]
pub struct OpusEncoder {
    ffmpeg: PathBuf,
    opusenc: PathBuf,
    bitrate_kbps: u32,
    tmp_dir: PathBuf,
}

/// エンコードした Opus（tmp）。`guard` を drop すると消える
#[derive(Debug)]
pub struct EncodedOpus {
    pub guard: TempGuard,
}

impl OpusEncoder {
    pub fn new(
        ffmpeg: impl Into<PathBuf>,
        opusenc: impl Into<PathBuf>,
        bitrate_kbps: u32,
        tmp_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            ffmpeg: ffmpeg.into(),
            opusenc: opusenc.into(),
            bitrate_kbps: bitrate_kbps.max(1),
            tmp_dir: tmp_dir.into(),
        }
    }

    pub fn tmp_dir(&self) -> &Path {
        &self.tmp_dir
    }

    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
    }

    fn tmp_path(&self, ext: &str) -> Result<PathBuf, EncodeError> {
        let mut buf = [0u8; 8];
        getrandom::fill(&mut buf).map_err(|e| std::io::Error::other(e.to_string()))?;
        let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
        Ok(self.tmp_dir.join(format!("{OPUS_TMP_PREFIX}{hex}.{ext}")))
    }

    /// `source`（読み取りで開いた Library のファイル）を Opus にする。`bit_depth` は中間 WAV の
    /// PCM 形式に使う（ffmpeg にエンコーダが無い深度・不明なら 24 bit。非可逆にするので
    /// 切り上げは無害）。`token` が倒れたら子プロセスごと止めて [`ProcessError::Cancelled`]
    pub async fn encode(
        &self,
        source: File,
        bit_depth: Option<u32>,
        token: &CancellationToken,
    ) -> Result<EncodedOpus, EncodeError> {
        let pcm_codec = bit_depth
            .and_then(|b| FlacEncoder::pcm_codec(b).ok())
            .unwrap_or("pcm_s24le");
        std::fs::create_dir_all(&self.tmp_dir).map_err(|source| EncodeError::TmpDir {
            path: self.tmp_dir.clone(),
            source,
        })?;
        let wav = TempGuard::new(self.tmp_path("wav")?);
        let opus = TempGuard::new(self.tmp_path("opus")?);

        // 1. デコード（FlacEncoder と同じ。メタデータは移さない）
        ExternalCommand::new(&self.ffmpeg)
            .path_style(PathStyle::DotSlash)
            .args([
                "-hide_banner",
                "-nostdin",
                "-loglevel",
                "error",
                "-y",
                "-i",
                "/dev/stdin",
                "-map",
                "0:a:0",
                "-vn",
                "-map_metadata",
                "-1",
                "-fflags",
                "+bitexact",
                "-flags",
                "+bitexact",
                "-c:a",
                pcm_codec,
                "-f",
                "wav",
            ])
            .path_arg(wav.path())
            .stdin_file(source)
            .timeout(STEP_TIMEOUT)
            .run(token)
            .await?;

        // 2. エンコード。opusenc は 48 kHz へのリサンプルを自分で行う。コメントと画像は捨てる
        //    （タグは lofty で書く。WAV には無いが明示しておく）
        let bitrate = self.bitrate_kbps.to_string();
        ExternalCommand::new(&self.opusenc)
            .path_style(PathStyle::DoubleDash)
            .args([
                "--quiet",
                "--bitrate",
                bitrate.as_str(),
                "--vbr",
                "--music",
                "--discard-comments",
                "--discard-pictures",
            ])
            .path_arg(wav.path())
            .path_arg(opus.path())
            .timeout(STEP_TIMEOUT)
            .run(token)
            .await?;
        drop(wav);
        Ok(EncodedOpus { guard: opus })
    }
}

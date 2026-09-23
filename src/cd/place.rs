//! CD の吸い出し結果を Inbox に置く（SPEC §7.2「エンコード」「ログ出力」、D-67 追記、P2-5）。
//! 吸い出しジョブの最終段として [`place_disc`] を呼ぶ。Library へは直接置かない: 名前と category は
//! Inbox の承認画面で直し、配置（`import::inbox::place_item`）が `source_type = 'cd_rip'` と検証記録を
//! 入れる。
//!
//! ```text
//! 1. 検証        DiscMetadata を TOC と照合（名前は空でもよい）、PCM の長さ = TOC のサンプル数 × 4
//! 2. MD5         トラックごとに PCM の MD5（STREAMINFO と同じ流儀。冪等性の判定に使う）
//! 3. エンコード   raw PCM を flac -N --verify --skip/--until で tmp へ。STREAMINFO の MD5 が
//!                PCM の MD5 と一致するときだけ成果物。タグは lofty で書く（空のタイトルは Track NN）
//! 4. 組み立て    Inbox 直下の隠しディレクトリ `.spindle-rip-<DiscID>` に `NN.flac`、rip.log /
//!                disc.cue / disc.toc、サイドカー（category と吸い出しの記録）を置く
//! 5. 公開        `CD/<albumartist - album> [<DiscID>]`（名前が無ければ `CD/[<DiscID>]`）へ
//!                ディレクトリごと RENAME_NOREPLACE
//! 6. 後続        inbox ジョブを投入して件を出す
//! ```
//!
//! 走査は `.` で始まるディレクトリを見ないので、組み立て中の盤が件として見え、サイドカーが揃う前に
//! 承認されることはない。公開の rename は 1 回で、件はファイルが全部揃った状態で現れる。
//!
//! 冪等性: 公開先が既にあり、全トラックの STREAMINFO の MD5 が自分の PCM と一致し、サイドカーの
//! 記録が同じファイル名を持つなら自分の成果物（公開の後に落ちた再実行）とみなして組み立てを飛ばす。
//! 違えば [`PlaceError::Conflict`]。組み立ての残骸（前の実行の隠しディレクトリ）は自分のものなので
//! 消してから作り直す

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use md5::{Digest as _, Md5};
use tokio_util::sync::CancellationToken;

use super::metadata::{DiscMetadata, MetadataError};
use super::riplog::{companion_names, render_cue, render_log, render_toc, RipReport};
use super::toc::Toc;
use super::verify::track_state;
use super::{LayoutError, TrackLayout};
use crate::db::jobs::EnqueueResult;
use crate::domain::pathgen::sanitize_component;
use crate::domain::relpath::RelPath;
use crate::domain::tags::{write_flac_tags, TagWriteError, TransferTags};
use crate::fsroot::{FileKind, FsError, RootDir};
use crate::import::placement::{place_one, PlacementError};
use crate::import::sidecar::{RipEntry, Sidecar, SidecarError};
use crate::jobs::handlers::inbox::new_inbox_job;
use crate::jobs::process::{ExternalCommand, PathStyle, ProcessError};
use crate::jobs::{Jobs, TempGuard};
use crate::media::fingerprint::flac_streaminfo_md5;

/// 1 トラックのエンコードのタイムアウト
const ENCODE_TIMEOUT: Duration = Duration::from_secs(1800);
/// tmp のエンコード出力の前置き
const TMP_PREFIX: &str = "spindle-rip-";
/// 公開先の親（Inbox 相対）
pub const INBOX_CD_DIR: &str = "CD";
/// 公開先のディレクトリ名に使う名前部分の上限（バイト。DiscID と括弧を足しても 255 に収める）
const LABEL_MAX_BYTES: usize = 160;

pub struct PlaceEnv {
    pub inbox: Arc<RootDir>,
    pub jobs: Arc<Jobs>,
    pub flac: PathBuf,
    pub compression: u8,
    /// エンコード出力の作業領域（`[paths].data/tmp`）
    pub tmp_dir: PathBuf,
    /// テスト用: 組み立ての後・公開の前に呼ぶ（その間に落ちた / 公開先ができた状況を作る）
    #[doc(hidden)]
    pub before_publish: Option<PlaceHook>,
}

/// テスト用フック
pub type PlaceHook = Arc<dyn Fn() + Send + Sync>;

pub struct PlaceInput<'a> {
    pub toc: &'a Toc,
    /// 吸い出しを始めたときの内容（名前は空でもよい）
    pub metadata: &'a DiscMetadata,
    /// オフセット適用済みの s16le / 2ch / 44.1 kHz の raw PCM（TOC の音声部分ぴったり）
    pub pcm: &'a Path,
    pub report: &'a RipReport,
    /// エンコードの進捗（`(トラック番号, 済んだ数, 全数)`。トラックを 1 本エンコードするたび）
    pub on_encoded: Option<&'a (dyn Fn(u8, u64, u64) + Send + Sync)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    /// Inbox 相対の件のディレクトリ
    pub rel_dir: RelPath,
    /// 音声トラック順のファイル名
    pub files: Vec<String>,
    /// 公開先に自分の成果物が既にあった（組み立てを飛ばした）
    pub reused: bool,
    /// 投入した inbox ジョブ
    pub inbox_job: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum PlaceError {
    #[error("メタデータが不正: {0}")]
    Metadata(#[from] MetadataError),
    #[error("TOC からレイアウトを作れない: {0}")]
    Layout(#[from] LayoutError),
    #[error("PCM の長さが TOC と合わない: 期待 {expected} バイト、実際 {actual}")]
    PcmLength { expected: u64, actual: u64 },
    #[error("エンコードした FLAC の MD5 が PCM と一致しない: トラック {number}")]
    Md5Mismatch { number: u8 },
    #[error("Inbox の置き場所が衝突: {0}")]
    Conflict(String),
    #[error("エンコードに失敗: {0}")]
    Encode(#[from] ProcessError),
    #[error("タグを書けない: {0}")]
    Tag(#[from] TagWriteError),
    #[error("サイドカーを書けない: {0}")]
    Sidecar(#[from] SidecarError),
    #[error(transparent)]
    Fs(#[from] FsError),
    #[error(transparent)]
    Placement(PlacementError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("inbox ジョブを投入できない: {0}")]
    Enqueue(String),
    #[error("キャンセルされた")]
    Cancelled,
}

impl From<PlacementError> for PlaceError {
    fn from(e: PlacementError) -> Self {
        match e {
            PlacementError::Conflict(r) => PlaceError::Conflict(r),
            other => PlaceError::Placement(other),
        }
    }
}

// ---------------------------------------------------------------- MD5

/// トラックごとの PCM の MD5（STREAMINFO と同じ LE インターリーブのバイト列の MD5）。
/// ファイル長が `total_samples × 4` でなければ `InvalidData`
pub fn pcm_md5s(pcm: &Path, layout: &TrackLayout) -> std::io::Result<Vec<[u8; 16]>> {
    let mut f = File::open(pcm)?;
    let expected = layout.total_samples() * 4;
    let actual = f.metadata()?.len();
    if actual != expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("PCM の長さが TOC と合わない: 期待 {expected}、実際 {actual}"),
        ));
    }
    let mut buf = vec![0u8; 64 * 1024];
    let mut out = Vec::with_capacity(layout.track_count());
    for &n in layout.lengths() {
        let mut remaining = n * 4;
        let mut h = Md5::new();
        while remaining > 0 {
            let want = (buf.len() as u64).min(remaining) as usize;
            f.read_exact(&mut buf[..want])?;
            h.update(&buf[..want]);
            remaining -= want as u64;
        }
        out.push(<[u8; 16]>::from(h.finalize()));
    }
    Ok(out)
}

// ---------------------------------------------------------------- 名前

/// 件のディレクトリ（Inbox 相対）: `CD/<albumartist - album> [<DiscID>]`。名前が両方空なら
/// `CD/[<DiscID>]`、片方なら空でない方。名前は `sanitize_component` を通し、先頭の `.` は落とす。
/// DiscID は `.` で始まり得るので必ず括弧で包む（隠しディレクトリにすると走査に見えない）。DiscID は
/// 盤ごとに違うので、複数枚組もディスクごとに別の件になる
pub fn inbox_dir(toc: &Toc, meta: &DiscMetadata) -> Result<RelPath, PlaceError> {
    let disc_id = toc.musicbrainz_disc_id();
    let names: Vec<&str> = [meta.album_artist.trim(), meta.album.trim()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    let bare = format!("[{disc_id}]");
    let name = if names.is_empty() {
        bare
    } else {
        let label = sanitize_component(&names.join(" - "));
        let label = label.trim_start_matches('.');
        let mut cut = label.len().min(LABEL_MAX_BYTES);
        while !label.is_char_boundary(cut) {
            cut -= 1;
        }
        let label = label[..cut].trim_end_matches(['.', ' ']);
        if label.is_empty() {
            bare
        } else {
            format!("{label} {bare}")
        }
    };
    RelPath::parse(&format!("{INBOX_CD_DIR}/{name}"))
        .map_err(|e| PlaceError::Conflict(format!("件のディレクトリ名を作れない: {e}")))
}

/// 組み立て用の隠しディレクトリ（Inbox 直下。同じ盤の再実行は同じ名前）
fn staging_dir(toc: &Toc) -> Result<RelPath, PlaceError> {
    RelPath::parse(&format!(".spindle-rip-{}", toc.musicbrainz_disc_id()))
        .map_err(|e| PlaceError::Conflict(format!("組み立て用のディレクトリ名を作れない: {e}")))
}

/// 音声トラック順のファイル名（`NN.flac`。TOC の番号）。名前での対応付けの鍵になるので、承認で
/// 変わる値（タイトル）を入れない
pub fn track_file_names(toc: &Toc) -> Vec<String> {
    toc.audio_tracks()
        .map(|t| format!("{:02}.flac", t.number))
        .collect()
}

// ---------------------------------------------------------------- エンコード

fn tmp_path(dir: &Path, ext: &str) -> Result<PathBuf, PlaceError> {
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).map_err(|e| std::io::Error::other(e.to_string()))?;
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    Ok(dir.join(format!("{TMP_PREFIX}{hex}.{ext}")))
}

/// トラックごとに raw PCM の該当区間を `flac` でエンコードし、STREAMINFO の MD5 を PCM の MD5 と
/// 照合してタグを書く。結果は tmp（`TempGuard`。drop で消える）
#[allow(clippy::too_many_arguments)]
pub async fn encode_tracks(
    env: &PlaceEnv,
    pcm: &Path,
    layout: &TrackLayout,
    toc: &Toc,
    meta: &DiscMetadata,
    md5s: &[[u8; 16]],
    on_encoded: Option<&(dyn Fn(u8, u64, u64) + Send + Sync)>,
    token: &CancellationToken,
) -> Result<Vec<TempGuard>, PlaceError> {
    std::fs::create_dir_all(&env.tmp_dir)?;
    let level = format!("-{}", env.compression.min(8));
    let mut out = Vec::with_capacity(layout.track_count());
    let mut skip: u64 = 0;
    for (i, &len) in layout.lengths().iter().enumerate() {
        if token.is_cancelled() {
            return Err(PlaceError::Cancelled);
        }
        let until = skip + len;
        let guard = TempGuard::new(tmp_path(&env.tmp_dir, "flac")?);
        ExternalCommand::new(&env.flac)
            .path_style(PathStyle::DoubleDash)
            .args([
                level.as_str(),
                "--verify",
                "--silent",
                "--force-raw-format",
                "--endian=little",
                "--sign=signed",
                "--channels=2",
                "--bps=16",
                "--sample-rate=44100",
            ])
            .arg(format!("--skip={skip}"))
            .arg(format!("--until={until}"))
            .arg("-o")
            .arg(guard.path())
            .path_arg(pcm)
            .timeout(ENCODE_TIMEOUT)
            .run(token)
            .await
            .map_err(|e| match e {
                ProcessError::Cancelled => PlaceError::Cancelled,
                other => PlaceError::Encode(other),
            })?;
        skip = until;
        let path = guard.path().to_path_buf();
        let tags = TransferTags {
            items: meta.tags_for(toc, i),
            pictures: Vec::new(),
        };
        let expected = md5s.get(i).copied();
        let number = meta.tracks.get(i).map(|t| t.number).unwrap_or(0);
        tokio::task::spawn_blocking(move || -> Result<(), PlaceError> {
            let mut f = File::options().read(true).write(true).open(&path)?;
            let md5 =
                flac_streaminfo_md5(&mut f).map_err(|e| std::io::Error::other(e.to_string()))?;
            if md5.is_none() || md5 != expected {
                return Err(PlaceError::Md5Mismatch { number });
            }
            write_flac_tags(&mut f, &tags)?;
            f.sync_all()?;
            Ok(())
        })
        .await
        .map_err(|e| std::io::Error::other(format!("エンコード後処理のタスクが異常終了: {e}")))??;
        out.push(guard);
        if let Some(f) = on_encoded {
            f(number, out.len() as u64, layout.track_count() as u64);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- 組み立てと公開

struct Companion {
    name: String,
    body: String,
}

/// 組み立て用のディレクトリを消す（中は平らなファイルだけのはず。ディレクトリがあれば Err）
fn remove_staging(inbox: &RootDir, dir: &RelPath) -> Result<(), PlaceError> {
    let entries = match inbox.read_dir(Some(dir)) {
        Ok(e) => e,
        Err(FsError::NotFound) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for e in entries {
        let Some(name) = e.name.to_str() else {
            return Err(PlaceError::Conflict(format!(
                "{dir} に UTF-8 でない名前がある"
            )));
        };
        if e.kind == FileKind::Dir {
            return Err(PlaceError::Conflict(format!(
                "{dir} に想定しないディレクトリがある: {name}"
            )));
        }
        let rel = dir
            .join(name)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        match inbox.unlink(&rel) {
            Ok(()) | Err(FsError::NotFound) => {}
            Err(e) => return Err(e.into()),
        }
    }
    match inbox.remove_dir(dir) {
        Ok(()) | Err(FsError::NotFound) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// 組み立て用のディレクトリにトラック・同梱ファイル・サイドカーを置く
fn build_staging(
    inbox: &RootDir,
    staging: &RelPath,
    files: &[String],
    encoded: &[PathBuf],
    companions: &[Companion],
    sidecar: &Sidecar,
) -> Result<(), PlaceError> {
    remove_staging(inbox, staging)?;
    inbox.create_dir_all(staging)?;
    let join = |name: &str| {
        staging
            .join(name)
            .map_err(|e| PlaceError::Io(std::io::Error::other(e.to_string())))
    };
    for (name, src) in files.iter().zip(encoded) {
        place_one(
            inbox,
            staging,
            &join(name)?,
            File::open(src)?,
            |_| Ok(()),
            |_| Ok(false),
        )?;
    }
    for c in companions {
        place_one(
            inbox,
            staging,
            &join(&c.name)?,
            c.body.as_bytes(),
            |_| Ok(()),
            |_| Ok(false),
        )?;
    }
    sidecar.write(inbox, staging)?;
    inbox.fsync_dir(Some(staging))?;
    Ok(())
}

/// 公開先が自分の成果物か: 全トラックが STREAMINFO の MD5 で一致し、サイドカーの記録が同じ
/// ファイル名を持つ
fn is_own_result(
    inbox: &RootDir,
    dir: &RelPath,
    files: &[String],
    md5s: &[[u8; 16]],
) -> Result<bool, PlaceError> {
    for (name, md5) in files.iter().zip(md5s) {
        let rel = dir
            .join(name)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut f = match inbox.open_file(&rel) {
            Ok(f) => f,
            Err(FsError::NotFound) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        // FLAC として読めないものも自分の成果物ではない
        let got = flac_streaminfo_md5(&mut f).ok().flatten();
        if got != Some(*md5) {
            return Ok(false);
        }
    }
    Ok(match Sidecar::read(inbox, dir) {
        Ok(Some(s)) => s.rip.is_some_and(|r| r.files == files),
        Ok(None) | Err(_) => false,
    })
}

/// 組み立て用のディレクトリを消す（失敗はログだけ。次の実行も作り直す前に消す）
async fn discard_staging(env: &PlaceEnv, staging: &RelPath) {
    let (inbox, dir) = (Arc::clone(&env.inbox), staging.clone());
    let res = tokio::task::spawn_blocking(move || remove_staging(&inbox, &dir)).await;
    if !matches!(res, Ok(Ok(()))) {
        tracing::warn!(dir = %staging, "組み立て用のディレクトリを消せない");
    }
}

/// 吸い出した PCM を Inbox に 1 件として置き、inbox ジョブを投入する（モジュールの説明を見よ）
pub async fn place_disc(
    env: &PlaceEnv,
    input: PlaceInput<'_>,
    token: &CancellationToken,
) -> Result<Placed, PlaceError> {
    let toc = input.toc.clone();
    let original = input.metadata.clone();
    original.validate(&toc)?;
    let meta = original.with_placeholder_titles();
    let layout = toc.track_layout()?;
    let expected_len = layout.total_samples() * 4;
    let actual_len = std::fs::metadata(input.pcm)?.len();
    if actual_len != expected_len {
        return Err(PlaceError::PcmLength {
            expected: expected_len,
            actual: actual_len,
        });
    }
    let md5s = {
        let pcm = input.pcm.to_path_buf();
        let layout = layout.clone();
        tokio::task::spawn_blocking(move || pcm_md5s(&pcm, &layout))
            .await
            .map_err(|e| std::io::Error::other(format!("MD5 タスクが異常終了: {e}")))??
    };
    let rel_dir = inbox_dir(&toc, &meta)?;
    let staging = staging_dir(&toc)?;
    let files = track_file_names(&toc);

    // 公開の後に落ちた再実行なら、公開先は自分の成果物
    let existing = {
        let (inbox, dir, files, md5s) = (
            Arc::clone(&env.inbox),
            rel_dir.clone(),
            files.clone(),
            md5s.clone(),
        );
        tokio::task::spawn_blocking(move || -> Result<Option<bool>, PlaceError> {
            match inbox.stat(&dir) {
                Ok(_) => Ok(Some(is_own_result(&inbox, &dir, &files, &md5s)?)),
                Err(FsError::NotFound) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await
        .map_err(|e| std::io::Error::other(format!("確認タスクが異常終了: {e}")))??
    };
    let reused = match existing {
        Some(true) => true,
        Some(false) => {
            return Err(PlaceError::Conflict(format!(
                "Inbox に同じ名前の件が既にある: {rel_dir}"
            )))
        }
        None => false,
    };

    if !reused {
        let encoded = encode_tracks(
            env,
            input.pcm,
            &layout,
            &toc,
            &meta,
            &md5s,
            input.on_encoded,
            token,
        )
        .await?;
        if token.is_cancelled() {
            return Err(PlaceError::Cancelled);
        }
        let names = companion_names(meta.disc_no, meta.disc_count);
        let states: Vec<&str> = (0..files.len())
            .map(|i| {
                track_state(
                    input.report.ctdb.as_ref(),
                    input.report.accuraterip.as_ref(),
                    i,
                )
                .map(|s| s.as_str())
                .unwrap_or("not_attempted")
            })
            .collect();
        let companions = vec![
            Companion {
                name: names.cue.clone(),
                body: render_cue(&toc, &meta, &files),
            },
            Companion {
                name: names.toc.clone(),
                body: render_toc(&toc, &meta, &files),
            },
            Companion {
                name: names.log.clone(),
                body: render_log(&toc, &meta, &files, input.report, &states),
            },
        ];
        let mut sidecar = Sidecar::default();
        sidecar.category = original
            .category
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_owned);
        sidecar.rip = Some(RipEntry {
            toc: toc.ctdb_toc(),
            metadata: original.clone(),
            files: files.clone(),
            log: names.log.clone(),
            report: input.report.clone(),
        });
        let built = {
            let (inbox, staging, files) = (Arc::clone(&env.inbox), staging.clone(), files.clone());
            let paths: Vec<PathBuf> = encoded.iter().map(|g| g.path().to_path_buf()).collect();
            tokio::task::spawn_blocking(move || {
                build_staging(&inbox, &staging, &files, &paths, &companions, &sidecar)
            })
            .await
            .map_err(|e| {
                PlaceError::Io(std::io::Error::other(format!(
                    "組み立てタスクが異常終了: {e}"
                )))
            })
            .and_then(|r| r)
        };
        drop(encoded);
        // 組み立てに失敗した・公開の前に取り消された: 組み立てたものは自分のものなので残さない
        // （隠しディレクトリは走査に見えないので、残すと同じ盤を吸い直すまで誰も片付けない）
        if let Err(e) = built {
            discard_staging(env, &staging).await;
            return Err(e);
        }
        if let Some(hook) = &env.before_publish {
            hook();
        }
        if token.is_cancelled() {
            discard_staging(env, &staging).await;
            return Err(PlaceError::Cancelled);
        }
        let (inbox, from, to) = (Arc::clone(&env.inbox), staging.clone(), rel_dir.clone());
        tokio::task::spawn_blocking(move || -> Result<(), PlaceError> {
            let parent = to
                .parent()
                .ok_or_else(|| PlaceError::Conflict(format!("{to} の親が無い")))?;
            inbox.create_dir_all(&parent)?;
            let published = match inbox.rename_noreplace(&from, &to) {
                Ok(()) => Ok(()),
                Err(FsError::Exists) => Err(PlaceError::Conflict(format!(
                    "Inbox に同じ名前の件が既にある: {to}"
                ))),
                Err(e) => Err(e.into()),
            };
            if let Err(e) = published {
                // 組み立てたものは自分のものなので残さない（次の実行も作り直す）
                if let Err(re) = remove_staging(&inbox, &from) {
                    tracing::warn!(dir = %from, error = %re, "組み立て用のディレクトリを消せない");
                }
                return Err(e);
            }
            inbox.fsync_dir(Some(&parent))?;
            inbox.fsync_dir(None)?;
            Ok(())
        })
        .await
        .map_err(|e| std::io::Error::other(format!("公開タスクが異常終了: {e}")))??;
    }

    let inbox_job = match env.jobs.enqueue(new_inbox_job()).await {
        Ok(EnqueueResult::Inserted(id) | EnqueueResult::Duplicate(id)) => id,
        Err(e) => return Err(PlaceError::Enqueue(e.to_string())),
    };
    tracing::info!(dir = %rel_dir, tracks = files.len(), reused, "CD を Inbox に置いた");
    Ok(Placed {
        rel_dir,
        files,
        reused,
        inbox_job,
    })
}

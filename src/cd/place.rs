//! CD の配置（SPEC §7.2「エンコード」「ログ出力」「後続ジョブ投入」、D-67、P2-8）。
//! 吸い出しジョブ（P2-5）の最終段として [`place_disc`] を呼ぶ。
//!
//! ```text
//! 1. 検証        DiscMetadata を TOC と照合、PCM の長さ = TOC のサンプル数 × 4
//! 2. MD5         トラックごとに PCM の MD5（STREAMINFO と同じ流儀。冪等性の判定に使う）
//! 3. 計画        [layout] のテンプレート（category 無し → unsorted、複数枚組 → multi_disc）で
//!                pathgen::plan。複数枚組の 2 枚目以降は宛先の同名 album に合流
//! 4. エンコード   raw PCM を flac -N --verify --skip/--until で tmp へ。STREAMINFO の MD5 が
//!                PCM の MD5 と一致するときだけ成果物。タグは lofty で書く
//! 5. 配置        job_mutexes の `library` を取り（scan / gc と同じ排他）、ディレクトリを作り、
//!                トラックと同梱ファイル（rip.log / disc.cue / disc.toc）を tmp → fsync →
//!                RENAME_NOREPLACE → dir fsync で置く
//! 6. 登録        1 トランザクションで albums / tracks（source_type = cd_rip、verification）/
//!                track_tags / album_verifications（source = rip）/ track_verifications
//! 7. 後続        rg（album）と transcode（Derived）を投入し、library イベントを流す
//! ```
//!
//! 冪等性は MD5 で判定する: 宛先に既にファイルがあれば STREAMINFO の MD5 が自分の PCM の MD5 と
//! 一致するときだけ自分の成果物とみなして飛ばす。DB に同じ `rel_path` の行があり `audio_md5` も
//! 一致すれば（配置の後に落ちてスキャナが先に拾った）その行を採用して出自と検証だけ書く。
//! どちらでもなければ [`PlaceError::Conflict`] で失敗し、この呼び出しで置いたファイルは消す
//! （Library を汚さない）

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use md5::{Digest as _, Md5};
use rusqlite::{params, Connection, OptionalExtension as _};
use tokio_util::sync::CancellationToken;

use super::metadata::{DiscMetadata, MetadataError};
use super::riplog::{companion_names, render_cue, render_log, render_toc, RipReport};
use super::toc::Toc;
use super::verify::track_state;
use super::{LayoutError, TrackLayout};
use crate::config::LayoutConfig;
use crate::db::jobs as dbjobs;
use crate::db::now_epoch;
use crate::db::scans::{self, AlbumMeta, Fingerprint, PictureState};
use crate::db::verify::{
    self as dbv, DiscRecord, DiscResult, Method, MethodRecord, RecordOutcome, TrackRecord,
    VerifySource,
};
use crate::db::{Db, DbError};
use crate::domain::pathgen::{
    self, AlbumVariant, Occupancy, PlanItem, Planned, Template, TrackFields,
};
use crate::domain::relpath::{canonical_key, RelPath};
use crate::domain::tags::{read_audio_file, write_flac_tags, TagWriteError, TransferTags};
use crate::fsroot::{self, FsError, RootDir};
use crate::import::placement::{
    find_or_create_album, place_one, register_track, release_key, remove_placed, PlacedFile,
    PlacementError,
};
use crate::import::scanner::track_content;
use crate::jobs::handlers::rg::new_album_job;
use crate::jobs::process::{ExternalCommand, PathStyle, ProcessError};
use crate::jobs::{Event, Jobs, LibraryEvent, TempGuard};
use crate::media::fingerprint::flac_streaminfo_md5;

/// `job_mutexes` の名前（scan / gc と同じ）
const LIBRARY_MUTEX: &str = "library";
/// 1 トラックのエンコードのタイムアウト
const ENCODE_TIMEOUT: Duration = Duration::from_secs(1800);
/// tmp のエンコード出力の前置き
const TMP_PREFIX: &str = "spindle-rip-";

pub struct PlaceEnv {
    pub db: Arc<Db>,
    pub root: Arc<RootDir>,
    pub jobs: Arc<Jobs>,
    pub flac: PathBuf,
    pub compression: u8,
    /// エンコード出力の作業領域（`[paths].data/tmp`）
    pub tmp_dir: PathBuf,
    pub layout: LayoutConfig,
    /// テスト用: エンコードの後・排他の前に呼ぶ（その間にライブラリが動いた状況を作る）
    #[doc(hidden)]
    pub before_lock: Option<PlaceHook>,
    /// テスト用: 配置の後・登録の前に呼ぶ（rename が並走した状況を作る）
    #[doc(hidden)]
    pub before_register: Option<PlaceHook>,
}

/// テスト用フック
pub type PlaceHook = Arc<dyn Fn() + Send + Sync>;

pub struct PlaceInput<'a> {
    pub toc: &'a Toc,
    pub metadata: &'a DiscMetadata,
    /// オフセット適用済みの s16le / 2ch / 44.1 kHz の raw PCM（TOC の音声部分ぴったり）
    pub pcm: &'a Path,
    pub report: &'a RipReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub album_id: i64,
    pub rel_dir: RelPath,
    /// 音声トラック順
    pub track_ids: Vec<i64>,
    pub job_ids: Vec<i64>,
    /// 既存の行を採用した数（配置の後に落ちてスキャナが拾っていた）
    pub adopted: usize,
    /// 宛先に既にあった自分の成果物を使った数
    pub reused_files: usize,
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
    #[error("配置先が衝突: {0}")]
    Conflict(String),
    #[error("library の排他を取れない（scan / gc が走っている）")]
    Busy,
    #[error("エンコードに失敗: {0}")]
    Encode(#[from] ProcessError),
    #[error("タグを書けない: {0}")]
    Tag(#[from] TagWriteError),
    #[error(transparent)]
    Fs(#[from] FsError),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Placement(PlacementError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
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

// ---------------------------------------------------------------- 計画

/// パスの計画
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// 音声トラック順の宛先
    pub paths: Vec<RelPath>,
    pub rel_dir: RelPath,
    /// 合流する既存 album（複数枚組の 2 枚目以降）
    pub join_album: Option<i64>,
    /// リリースの同一性キー（D-43）
    pub release: String,
    /// 語彙に一致した category（DB の表記）
    pub category: Option<(i64, String)>,
}

fn parse_template(name: &str, s: &str) -> Result<Template, PlaceError> {
    Template::parse(s).map_err(|e| PlaceError::Conflict(format!("[layout].{name} が不正: {e}")))
}

/// パスを決める（読み取りのみ）。テンプレートの選択・降格・衝突は D-43 の規則
pub fn plan_paths(
    conn: &Connection,
    layout_cfg: &LayoutConfig,
    toc: &Toc,
    meta: &DiscMetadata,
    md5s: &[[u8; 16]],
) -> Result<Plan, PlaceError> {
    meta.validate(toc)?;
    let category = match meta.category.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => {
            crate::db::categories::find_by_key(conn, name)?.map(|c| (c.id, c.name))
        }
        _ => None,
    };
    let template = if category.is_none() {
        parse_template("unsorted", &layout_cfg.unsorted)?
    } else if meta.disc_count > 1 {
        parse_template("multi_disc", &layout_cfg.multi_disc)?
    } else {
        parse_template("single_disc", &layout_cfg.single_disc)?
    };
    let fields: Vec<TrackFields> = meta
        .tracks
        .iter()
        .enumerate()
        .map(|(i, t)| TrackFields {
            category: category.as_ref().map(|(_, n)| n.clone()),
            albumartist: Some(meta.album_artist.trim().to_owned()),
            artist: Some(meta.track_artist(i).to_owned()),
            album: Some(meta.album.trim().to_owned()),
            title: Some(t.title.trim().to_owned()),
            disc_no: Some(i64::from(meta.disc_no)),
            track_no: Some(i64::from(t.number)),
            year: meta.year(),
            edition: None,
            ext: "flac".to_owned(),
            stem: format!("{:02}", t.number),
        })
        .collect();
    let Some(first) = fields.first() else {
        return Err(PlaceError::Metadata(MetadataError::TrackCount {
            expected: toc.audio_tracks().count(),
            got: 0,
        }));
    };
    // 素の宛先ディレクトリ（降格前）で合流先を探す
    let plain_dir = template
        .render(first, AlbumVariant::Plain)
        .map_err(|e| PlaceError::Conflict(e.to_string()))?
        .parent();
    // 合流は release_id の無い（手入力の）複数枚組だけ。release_id があれば同一性は `mb:` で決まり、
    // 別リリースなら降格する（合流先を持ったまま降格すると「ディレクトリ = album」が壊れる）
    let has_release_id = meta
        .release_id
        .as_deref()
        .is_some_and(|r| !r.trim().is_empty());
    let join = match &plain_dir {
        Some(dir) if !has_release_id => find_join_album(conn, dir, meta)?,
        _ => None,
    };
    // リリースキーは既存行の規則（`load_occupancy`: mb → disc → album）に揃える。自分が登録した
    // album も `discid` を持つので、再実行で自分の成果物を別リリースと見ない
    let release = match (&meta.release_id, &join) {
        (Some(mb), _) if !mb.trim().is_empty() => format!("mb:{}", mb.trim()),
        (_, Some((_, release))) => release.clone(),
        _ => format!("disc:{}", toc.musicbrainz_disc_id()),
    };
    // 占有: 自分と同じ音声（MD5 一致）の行は除く（再実行で自分の成果物を衝突にしない）
    let mut occ: Occupancy = crate::edit::rename::load_occupancy(conn, &HashSet::new())?;
    {
        let mut st = conn.prepare_cached(
            "SELECT rel_path_key FROM tracks WHERE audio_md5 = ?1 AND missing_since IS NULL",
        )?;
        for md5 in md5s {
            let keys = st
                .query_map([md5.as_slice()], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for k in keys {
                occ.path_keys.remove(&k);
            }
        }
    }
    let items: Vec<PlanItem> = fields
        .into_iter()
        .enumerate()
        .map(|(i, f)| PlanItem {
            track_id: -(i as i64) - 1,
            template: template.clone(),
            fields: f,
            release: release.clone(),
            current_rel_path: String::new(),
        })
        .collect();
    let mut paths = Vec::with_capacity(items.len());
    for (planned, t) in pathgen::plan(&items, &occ).into_iter().zip(&meta.tracks) {
        match planned {
            Planned::Path(p) => paths.push(p),
            Planned::Conflict(r) => {
                return Err(PlaceError::Conflict(format!("トラック {}: {r}", t.number)))
            }
            Planned::Unchanged => {
                return Err(PlaceError::Conflict(format!(
                    "トラック {}: パスを決められない",
                    t.number
                )))
            }
        }
    }
    let Some(rel_dir) = paths.first().and_then(RelPath::parent) else {
        return Err(PlaceError::Conflict("宛先が root 直下になる".into()));
    };
    if paths.iter().any(|p| p.parent().as_ref() != Some(&rel_dir)) {
        return Err(PlaceError::Conflict(
            "トラックの宛先が 1 つのディレクトリに揃わない".into(),
        ));
    }
    Ok(Plan {
        paths,
        rel_dir,
        join_album: join.map(|(id, _)| id),
        release,
        category,
    })
}

/// 複数枚組の合流先: 宛先ディレクトリに albumartist と album が一致する album があり、
/// どちらかが複数枚組で、その `disc_no` のトラックがまだ無ければ `(album_id, release key)`
fn find_join_album(
    conn: &Connection,
    dir: &RelPath,
    meta: &DiscMetadata,
) -> Result<Option<(i64, String)>, PlaceError> {
    struct AlbumRow {
        id: i64,
        albumartist: Option<String>,
        album: Option<String>,
        disc_count: Option<i64>,
        mb: Option<String>,
        discid: Option<String>,
    }
    let row: Option<AlbumRow> = conn
        .query_row(
            "SELECT id, albumartist, album, disc_count, mb_release_id, discid FROM albums
              WHERE rel_dir_key = ?1 AND missing_since IS NULL",
            [dir.key()],
            |r| {
                Ok(AlbumRow {
                    id: r.get(0)?,
                    albumartist: r.get(1)?,
                    album: r.get(2)?,
                    disc_count: r.get(3)?,
                    mb: r.get(4)?,
                    discid: r.get(5)?,
                })
            },
        )
        .optional()?;
    let Some(AlbumRow {
        id,
        albumartist,
        album,
        disc_count,
        mb,
        discid,
    }) = row
    else {
        return Ok(None);
    };
    let same = |a: Option<&str>, b: &str| a.is_some_and(|a| canonical_key(a) == canonical_key(b));
    if !same(albumartist.as_deref(), meta.album_artist.trim())
        || !same(album.as_deref(), meta.album.trim())
    {
        return Ok(None);
    }
    // 宛先の既存 album が複数枚組であること（D-67。`disc_count > 1`、または active な構成トラックの
    // disc_no の最大が 2 以上。D-43 の multi_disc 判定と同じ）。1 枚組の album には合流しない
    let max_disc: Option<i64> = conn.query_row(
        "SELECT max(disc_no) FROM tracks WHERE album_id = ?1 AND missing_since IS NULL",
        [id],
        |r| r.get(0),
    )?;
    let existing_multi = disc_count.is_some_and(|n| n > 1) || max_disc.is_some_and(|n| n > 1);
    if meta.disc_count <= 1 || !existing_multi {
        return Ok(None);
    }
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM tracks WHERE album_id = ?1 AND disc_no = ?2 AND missing_since IS NULL",
        params![id, i64::from(meta.disc_no)],
        |r| r.get(0),
    )?;
    if n > 0 {
        return Ok(None);
    }
    Ok(Some((
        id,
        release_key(id, mb.as_deref(), discid.as_deref()),
    )))
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
pub async fn encode_tracks(
    env: &PlaceEnv,
    pcm: &Path,
    layout: &TrackLayout,
    toc: &Toc,
    meta: &DiscMetadata,
    md5s: &[[u8; 16]],
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
    }
    Ok(out)
}

// ---------------------------------------------------------------- 配置

/// 配置の結果（登録の材料）
struct PlacedFiles {
    tracks: Vec<(scans::Physical, scans::TrackContent)>,
    reused_files: usize,
    /// この呼び出しで新しく置いたファイル（登録に失敗したら消す）
    placed_new: Vec<RelPath>,
}

struct Companion {
    name: String,
    body: String,
}

/// ディレクトリを作り、トラックと同梱ファイルを置く。失敗したらこの呼び出しで新しく置いたものを消す
fn place_files(
    root: &RootDir,
    plan: &Plan,
    encoded: &[PathBuf],
    md5s: &[[u8; 16]],
    companions: &[Companion],
) -> Result<PlacedFiles, PlaceError> {
    root.create_dir_all(&plan.rel_dir)?;
    let mut placed_new: Vec<RelPath> = Vec::new();
    let mut reused_files = 0;
    let result = (|| -> Result<Vec<(scans::Physical, scans::TrackContent)>, PlaceError> {
        let mut tracks = Vec::with_capacity(plan.paths.len());
        for (i, target) in plan.paths.iter().enumerate() {
            let src = File::open(&encoded[i])?;
            let expected = md5s[i];
            let outcome = place_one(
                root,
                &plan.rel_dir,
                target,
                src,
                |_| Ok(()),
                move |mut f| {
                    let md5 = flac_streaminfo_md5(&mut f)
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                    Ok(md5 == Some(expected))
                },
            )?;
            match outcome {
                PlacedFile::New => placed_new.push(target.clone()),
                PlacedFile::Reused => reused_files += 1,
            }
            let file = root.open_file(target)?;
            let st = fsroot::fstat(&file)?;
            let af = read_audio_file(file, Some("flac"))
                .map_err(|e| std::io::Error::other(format!("{target}: {e}")))?;
            let mut content = track_content(af);
            content.picture = PictureState::Absent;
            tracks.push((st.into(), content));
        }
        for c in companions {
            let target = plan
                .rel_dir
                .join(&c.name)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let body = c.body.clone();
            let outcome = place_one(
                root,
                &plan.rel_dir,
                &target,
                c.body.as_bytes(),
                |_| Ok(()),
                move |mut f| {
                    let mut existing = String::new();
                    f.read_to_string(&mut existing)?;
                    Ok(existing == body)
                },
            )?;
            match outcome {
                PlacedFile::New => placed_new.push(target),
                PlacedFile::Reused => reused_files += 1,
            }
        }
        root.fsync_dir(Some(&plan.rel_dir))?;
        Ok(tracks)
    })();
    match result {
        Ok(tracks) => Ok(PlacedFiles {
            tracks,
            reused_files,
            placed_new,
        }),
        Err(e) => {
            remove_placed(root, &plan.rel_dir, &placed_new);
            Err(e)
        }
    }
}

// ---------------------------------------------------------------- 登録

struct Registered {
    album_id: i64,
    track_ids: Vec<i64>,
    job_ids: Vec<i64>,
    adopted: usize,
    reused_files: usize,
}

/// 検証の記録（手法ごと。照会していない手法は行を作らない）
fn disc_record(report: &RipReport, meta: &DiscMetadata, track_ids: &[i64]) -> DiscRecord {
    let disc_result = |m: &super::verify::MethodResult| match m.outcome {
        super::verify::Outcome::Verified => DiscResult::Verified,
        super::verify::Outcome::Mismatch => DiscResult::Mismatch,
        super::verify::Outcome::NotFound => DiscResult::NotFound,
    };
    let mut methods = Vec::new();
    if let Some(m) = &report.ctdb {
        methods.push(MethodRecord {
            method: Method::Ctdb,
            result: disc_result(m),
            detected_offset: Some(m.offset),
            confidence: Some(m.confidence),
            tracks: track_ids
                .iter()
                .enumerate()
                .map(|(i, &id)| TrackRecord {
                    track_id: id,
                    crc_v1: None,
                    crc_v2: None,
                    ctdb_crc: Some(report.crcs.get(i).map(|c| c.ctdb).unwrap_or(0)),
                    matched: m.tracks.get(i).is_some_and(|v| v.matched),
                })
                .collect(),
        });
    }
    if let Some(m) = &report.accuraterip {
        methods.push(MethodRecord {
            method: Method::AccurateRip,
            result: disc_result(m),
            detected_offset: Some(m.offset),
            confidence: Some(m.confidence),
            tracks: track_ids
                .iter()
                .enumerate()
                .map(|(i, &id)| TrackRecord {
                    track_id: id,
                    crc_v1: Some(report.crcs.get(i).map(|c| c.ar_v1).unwrap_or(0)),
                    crc_v2: Some(report.crcs.get(i).map(|c| c.ar_v2).unwrap_or(0)),
                    ctdb_crc: None,
                    matched: m.tracks.get(i).is_some_and(|v| v.matched),
                })
                .collect(),
        });
    }
    let states = track_ids
        .iter()
        .enumerate()
        .filter_map(|(i, &id)| {
            track_state(report.ctdb.as_ref(), report.accuraterip.as_ref(), i).map(|s| (id, s))
        })
        .collect();
    DiscRecord {
        disc_no: i64::from(meta.disc_no),
        methods,
        states,
    }
}

#[allow(clippy::too_many_arguments)]
fn register(
    conn: &mut Connection,
    plan: &Plan,
    toc: &Toc,
    meta: &DiscMetadata,
    report: &RipReport,
    placed: &PlacedFiles,
    md5s: &[[u8; 16]],
    log_rel: &str,
    job_id: i64,
) -> Result<Result<Registered, PlaceError>, DbError> {
    let tx = conn.transaction()?;
    let now = now_epoch();
    let key = plan.rel_dir.key();
    // album: 合流 → 既存（missing なら復活）→ 新規
    let album_id = match plan.join_album {
        Some(id) => {
            // 合流先が計画どおりの場所に active であること（計画と登録は同じ排他の中だが、
            // 「ディレクトリ = album」を登録の時点でも確かめる）
            let at: Option<(String, Option<i64>)> = tx
                .query_row(
                    "SELECT rel_dir_key, missing_since FROM albums WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match at {
                Some((dir_key, None)) if dir_key == key => id,
                _ => {
                    drop(tx);
                    return Ok(Err(PlaceError::Conflict(format!(
                        "合流先の album {id} が {} に無い",
                        plan.rel_dir
                    ))));
                }
            }
        }
        None => {
            let meta_row = AlbumMeta {
                category_id: plan.category.as_ref().map(|(id, _)| *id),
                albumartist: Some(meta.album_artist.trim().to_owned()),
                album: Some(meta.album.trim().to_owned()),
                date: meta.date.clone(),
                original_date: None,
                mb_release_id: meta.release_id.clone(),
                discid: Some(toc.musicbrainz_disc_id()),
                disc_count: Some(i64::from(meta.disc_count)),
            };
            match find_or_create_album(&tx, &plan.rel_dir, &plan.release, &meta_row) {
                Ok(Ok(id)) => id,
                Ok(Err(reason)) => {
                    drop(tx);
                    return Ok(Err(PlaceError::Conflict(reason)));
                }
                Err(e) => {
                    drop(tx);
                    return Ok(Err(e.into()));
                }
            }
        }
    };
    let album_name: Option<String> = tx
        .query_row("SELECT album FROM albums WHERE id = ?1", [album_id], |r| {
            r.get(0)
        })
        .optional()?
        .flatten();
    // tracks: 同じパスに同じ MD5 の行があれば採用、無ければ挿入
    let mut track_ids = Vec::with_capacity(plan.paths.len());
    let mut expected = Vec::with_capacity(plan.paths.len());
    let mut adopted = 0;
    for (i, rel) in plan.paths.iter().enumerate() {
        let (ph, content) = &placed.tracks[i];
        let id = match register_track(&tx, rel, ph, content, Fingerprint::Md5(Some(md5s[i])), now) {
            Ok(Ok(r)) => {
                if r.adopted {
                    adopted += 1;
                }
                expected.push((r.id, r.audio_version));
                r.id
            }
            Ok(Err(reason)) => {
                drop(tx);
                return Ok(Err(PlaceError::Conflict(reason)));
            }
            Err(e) => {
                drop(tx);
                return Ok(Err(e.into()));
            }
        };
        scans::set_track_album(&tx, id, album_id, album_name.as_deref())?;
        scans::set_source_type(&tx, id, "cd_rip")?;
        track_ids.push(id);
    }
    let disc = disc_record(report, meta, &track_ids);
    match dbv::record_album(
        &tx,
        album_id,
        job_id,
        VerifySource::Rip,
        &expected,
        &[disc],
        Some(log_rel),
        now,
    )? {
        RecordOutcome::Recorded(_) | RecordOutcome::AlreadyRecorded => {}
        RecordOutcome::Changed { track_id } => {
            drop(tx);
            return Ok(Err(PlaceError::Conflict(format!(
                "track {track_id} の音声版が登録の途中で進んだ"
            ))));
        }
    }
    // CD 取り込みの album は album gain on（アルバムとして通して聴く単位。D-74）。合流でも on にする
    let _ = crate::db::replaygain::set_album_gain(&tx, album_id, true, now)?;
    let mut job_ids = vec![dbjobs::enqueue(&tx, &new_album_job(album_id), now)?.id()];
    for &id in &track_ids {
        if let Some(j) = crate::db::derived::enqueue_if_stale(&tx, id, now)? {
            job_ids.push(j);
        }
    }
    tx.commit()?;
    Ok(Ok(Registered {
        album_id,
        track_ids,
        job_ids,
        adopted,
        reused_files: 0,
    }))
}

// ---------------------------------------------------------------- 本体

/// 吸い出した PCM と確定したメタデータを Library に配置して登録する（モジュールの説明を見よ）
pub async fn place_disc(
    env: &PlaceEnv,
    input: PlaceInput<'_>,
    job_id: i64,
    token: &CancellationToken,
) -> Result<Placed, PlaceError> {
    let toc = input.toc.clone();
    let meta = input.metadata.clone();
    meta.validate(&toc)?;
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
    // エンコード（数分〜数十分）の前に一度計画して、衝突ならその前に止める。確定する計画は排他の
    // 中で取り直す（その間に scan / rename が album を動かし得る）
    plan_for(env, &toc, &meta, &md5s).await?;
    let encoded = encode_tracks(env, input.pcm, &layout, &toc, &meta, &md5s, token).await?;
    if let Some(hook) = &env.before_lock {
        hook();
    }
    if token.is_cancelled() {
        return Err(PlaceError::Cancelled);
    }

    // 排他。以降は必ず解放してから返す
    let acquired = env
        .db
        .write(move |c| dbjobs::acquire_mutex(c, LIBRARY_MUTEX, job_id, now_epoch()))
        .await?;
    if !acquired {
        return Err(PlaceError::Busy);
    }
    let result = place_locked(env, &toc, &meta, input.report, &encoded, &md5s, job_id).await;
    if let Err(e) = env
        .db
        .write(move |c| dbjobs::release_mutexes(c, job_id))
        .await
    {
        tracing::warn!(job_id, error = %e, "library の排他を解放できない");
    }
    let (plan, registered) = result?;
    drop(encoded);
    env.jobs.notify_enqueued(&registered.job_ids).await;
    // rip には走査が無いので scan_run_id は 0
    env.jobs.publish(Event::Library(LibraryEvent::Ids {
        scan_run_id: 0,
        track_ids: registered.track_ids.clone(),
    }));
    tracing::info!(
        job_id,
        album_id = registered.album_id,
        dir = %plan.rel_dir,
        tracks = registered.track_ids.len(),
        adopted = registered.adopted,
        "CD を配置した"
    );
    Ok(Placed {
        album_id: registered.album_id,
        rel_dir: plan.rel_dir.clone(),
        track_ids: registered.track_ids,
        job_ids: registered.job_ids,
        adopted: registered.adopted,
        reused_files: registered.reused_files,
    })
}

/// 計画（読み取りのみ）
async fn plan_for(
    env: &PlaceEnv,
    toc: &Toc,
    meta: &DiscMetadata,
    md5s: &[[u8; 16]],
) -> Result<Plan, PlaceError> {
    let (layout_cfg, toc, meta, md5s) =
        (env.layout.clone(), toc.clone(), meta.clone(), md5s.to_vec());
    env.db
        .read(move |c| Ok(plan_paths(c, &layout_cfg, &toc, &meta, &md5s)))
        .await?
}

/// `library` の排他を持っている間の処理: 計画の取り直し → 同梱ファイルの描画 → 配置 → 登録
async fn place_locked(
    env: &PlaceEnv,
    toc: &Toc,
    meta: &DiscMetadata,
    report: &RipReport,
    encoded: &[TempGuard],
    md5s: &[[u8; 16]],
    job_id: i64,
) -> Result<(Plan, Registered), PlaceError> {
    let plan = plan_for(env, toc, meta, md5s).await?;
    let file_names: Vec<String> = plan
        .paths
        .iter()
        .map(|p| p.file_name().to_owned())
        .collect();
    let names = companion_names(meta.disc_no, meta.disc_count);
    let states: Vec<&str> = (0..plan.paths.len())
        .map(|i| {
            track_state(report.ctdb.as_ref(), report.accuraterip.as_ref(), i)
                .map(|s| s.as_str())
                .unwrap_or("not_attempted")
        })
        .collect();
    let companions = vec![
        Companion {
            name: names.cue.clone(),
            body: render_cue(toc, meta, &file_names),
        },
        Companion {
            name: names.toc.clone(),
            body: render_toc(toc, meta, &file_names),
        },
        Companion {
            name: names.log.clone(),
            body: render_log(toc, meta, &file_names, report, &states),
        },
    ];
    let log_rel = format!("{}/{}", plan.rel_dir, names.log);
    let placed = {
        let root = Arc::clone(&env.root);
        let plan = plan.clone();
        let paths: Vec<PathBuf> = encoded.iter().map(|g| g.path().to_path_buf()).collect();
        let md5s = md5s.to_vec();
        tokio::task::spawn_blocking(move || place_files(&root, &plan, &paths, &md5s, &companions))
            .await
            .map_err(|e| std::io::Error::other(format!("配置タスクが異常終了: {e}")))??
    };
    if let Some(hook) = &env.before_register {
        hook();
    }
    let reused_files = placed.reused_files;
    let placed_new = placed.placed_new.clone();
    let (plan_tx, toc, meta, report, md5s) = (
        plan.clone(),
        toc.clone(),
        meta.clone(),
        report.clone(),
        md5s.to_vec(),
    );
    let registered = env
        .db
        .write(move |c| {
            register(
                c, &plan_tx, &toc, &meta, &report, &placed, &md5s, &log_rel, job_id,
            )
        })
        .await;
    // 登録できなかったら（衝突・DB エラー）この呼び出しで新しく置いたファイルを Library に残さない
    let registered: Result<Registered, PlaceError> = match registered {
        Ok(inner) => inner,
        Err(e) => Err(PlaceError::Db(e)),
    };
    let registered = match registered {
        Ok(r) => r,
        Err(e) => {
            let root = Arc::clone(&env.root);
            let dir = plan.rel_dir.clone();
            let _ =
                tokio::task::spawn_blocking(move || remove_placed(&root, &dir, &placed_new)).await;
            return Err(e);
        }
    };
    let mut registered = registered;
    registered.reused_files = reused_files;
    Ok((plan, registered))
}

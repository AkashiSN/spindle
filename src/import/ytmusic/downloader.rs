//! YouTube のダウンロード（SPEC §7.7、D-70、P3-3）。Library には触らず、**Inbox に置くところまで**。
//!
//! ```text
//! dump      yt-dlp --dump-single-json --flat-playlist（playlist なら entries ごとに ytdl を投入して終わり）
//! plugin    メタデータプラグイン（ok → youtube/<albumartist>/<album>、skip → 終わり、他 → youtube/_unmatched/<channel>）
//! dedup     SOURCE_URL が Library にあれば Fatal。Inbox の自分の宛先にあれば自分の成果物（再実行）として
//!           ダウンロードせずサイドカーと投入だけ済ませる。Inbox の別の場所なら Fatal
//! download  yt-dlp -f "ba[ext=webm]" --write-thumbnail
//! remux     ffmpeg -c:a copy → .opus（再エンコードなし）
//! tags      lofty（TRACKNUMBER は書かない。採番は Inbox）+ PICTURE + SOURCE_URL
//! archive   Archive/youtube/<id>.webm
//! inbox     <YYYYMMDD> <title> [<id>].opus（同名があれば SOURCE_URL が同じときだけ自分の成果物として採用）と
//!           spindle-inbox.json → inbox ジョブを投入
//! ```
//!
//! 再実行（Inbox に置いた後・サイドカーや投入の前に落ちた）は、置いたファイルの `SOURCE_URL` で自分の
//! 成果物と見分けて続きから済ませる（冪等）。取り込み済み（同じ `SOURCE_URL` が Library / Inbox にある）は
//! 失敗でなく [`Downloaded::AlreadyImported`]（P4-18）。失敗の区分: 再試行しても変わらないもの（
//! webm の音声なし・プラグインの故障・対応していない URL・宛先の同名で別の内容）は
//! [`DownloadError::Fatal`]、それ以外（yt-dlp / ffmpeg の非ゼロ終了、I/O）は [`DownloadError::Failed`] で
//! 指数バックオフ

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::db::inbox::{find_source_url, SourceLocated};
use crate::db::Db;
use crate::domain::pathgen::sanitize_component;
use crate::domain::relpath::{canonical_key, RelPath};
use crate::domain::tags::{write_tag_changes, TagChange};
use crate::fsroot::{FsError, RootDir};
use crate::import::ytmusic::metadata::SOURCE_URL_KEY;
use crate::import::ytmusic::sidecar::{FileEntry, Sidecar};
use crate::import::ytmusic::{Item, MetadataProvider, Outcome, ProviderError, Track};
use crate::jobs::process::{ExternalCommand, PathStyle, ProcessError};
use crate::jobs::{Jobs, NewJob};

/// `ytdl` ジョブの dedup キーの接頭辞（`ytdl:<url>`）
pub const DEDUP_PREFIX: &str = "ytdl:";
/// Inbox / Archive の中で YouTube 由来を置くディレクトリ
pub const SUBDIR: &str = "youtube";
/// 判定できなかったものの受け皿（`youtube/_unmatched/<channel>/`）
pub const UNMATCHED_DIR: &str = "_unmatched";
/// dump（メタデータの取得）の上限
const DUMP_TIMEOUT: Duration = Duration::from_secs(120);
/// remux の上限
const REMUX_TIMEOUT: Duration = Duration::from_secs(300);

pub fn new_ytdl_job(url: &str) -> NewJob {
    NewJob::new(
        crate::jobs::JobType::Ytdl,
        serde_json::json!({ "url": url }),
    )
    .dedup_key(format!("{DEDUP_PREFIX}{url}"))
}

/// 購読の同期が投入する ytdl（P4-16）。dedup は同じ `ytdl:<url>`（別の投入が走行中なら Duplicate。
/// その場合の扱いは同期側の規則: 「別の投入が走行中」として結果に出すだけ）
pub fn new_subscription_ytdl_job(url: &str, subscription_id: i64, position: u32) -> NewJob {
    NewJob::new(
        crate::jobs::JobType::Ytdl,
        serde_json::json!({ "url": url, "subscription_id": subscription_id, "position": position }),
    )
    .dedup_key(format!("{DEDUP_PREFIX}{url}"))
}

/// ytdl ジョブの入力（payload）
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DownloadRequest {
    pub url: String,
    /// 購読由来なら購読 id（追記先と category は購読から。P4-16）
    #[serde(default)]
    pub subscription_id: Option<i64>,
    /// 再生リストの位置（購読の `align` が on なら TRACKNUMBER に書く）
    #[serde(default)]
    pub position: Option<u32>,
}

impl DownloadRequest {
    pub fn url(url: &str) -> Self {
        Self {
            url: url.to_owned(),
            subscription_id: None,
            position: None,
        }
    }
}

/// 購読の追記先（`album_id` が束ねてあれば album 行の値、無ければ購読の文字列）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionTarget {
    pub albumartist: String,
    pub album: String,
    pub category: Option<String>,
    pub align: bool,
}

/// 購読の追記先を引く。購読が消えていれば None（通常の ytdl として振る舞う）
pub fn subscription_target(
    conn: &rusqlite::Connection,
    subscription_id: i64,
) -> Result<Option<SubscriptionTarget>, crate::db::DbError> {
    use rusqlite::OptionalExtension;
    let Some(sub) = crate::db::subscriptions::get(conn, subscription_id)? else {
        return Ok(None);
    };
    let bound: Option<(Option<String>, Option<String>, Option<String>)> = match sub.album_id {
        Some(album_id) => conn
            .query_row(
                "SELECT a.albumartist, a.album, c.name FROM albums a
                   LEFT JOIN categories c ON c.id = a.category_id
                  WHERE a.id = ?1 AND a.missing_since IS NULL",
                [album_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?,
        None => None,
    };
    Ok(Some(match bound {
        // album 行の category が未推定（NULL）なら購読の値（宛先のディレクトリは category で決まる）
        Some((aa, al, cat)) => SubscriptionTarget {
            albumartist: aa.filter(|s| !s.is_empty()).unwrap_or(sub.albumartist),
            album: al.filter(|s| !s.is_empty()).unwrap_or(sub.album),
            category: cat.or(sub.category),
            align: sub.align,
        },
        None => SubscriptionTarget {
            albumartist: sub.albumartist,
            album: sub.album,
            category: sub.category,
            align: sub.align,
        },
    }))
}

pub struct DownloaderEnv {
    pub db: Arc<Db>,
    pub inbox: Arc<RootDir>,
    pub archive: Arc<RootDir>,
    pub jobs: Arc<Jobs>,
    pub provider: MetadataProvider,
    /// yt-dlp（引数配列。先頭がプログラム）
    pub ytdlp: Vec<String>,
    pub ffmpeg: PathBuf,
    /// 作業領域の親（`<tmp_root>/<job_id>/`）
    pub tmp_root: PathBuf,
    pub download_timeout: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// 再試行しても変わらない
    #[error("{0}")]
    Fatal(String),
    #[error(transparent)]
    Failed(anyhow::Error),
    #[error("キャンセルされた")]
    Cancelled,
}

impl From<ProcessError> for DownloadError {
    fn from(e: ProcessError) -> Self {
        match e {
            ProcessError::Cancelled => DownloadError::Cancelled,
            other => DownloadError::Failed(other.into()),
        }
    }
}

impl From<std::io::Error> for DownloadError {
    fn from(e: std::io::Error) -> Self {
        DownloadError::Failed(e.into())
    }
}

impl From<FsError> for DownloadError {
    fn from(e: FsError) -> Self {
        DownloadError::Failed(e.into())
    }
}

impl From<crate::db::DbError> for DownloadError {
    fn from(e: crate::db::DbError) -> Self {
        DownloadError::Failed(e.into())
    }
}

/// ジョブ 1 件の結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Downloaded {
    /// playlist を展開して entries ごとのジョブを投入した。`skipped` は Library / Inbox に `SOURCE_URL` が
    /// あって投入しなかった数（取り込み済みの失敗ジョブで一覧を埋めない。P4-13）
    Playlist { enqueued: usize, skipped: usize },
    /// プラグインが `skip`。ダウンロードしていない
    Skipped { message: String },
    /// Inbox に置いた（Inbox 相対）
    Staged { rel_path: RelPath, verdict: String },
    /// Library / Inbox に同じ `SOURCE_URL` がある。やることが無い（失敗ではない。P4-18）。
    /// `location` は "Library" / "Inbox"、`path` は所在
    AlreadyImported {
        location: &'static str,
        path: String,
    },
}

// ---------------------------------------------------------------- dump の解釈

/// `--dump-single-json` の結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dump {
    /// entries の URL（`--flat-playlist`）
    Playlist(Vec<String>),
    Video(VideoInfo),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoInfo {
    pub id: String,
    /// 正規形の URL（`SOURCE_URL` に書く）
    pub webpage_url: String,
    /// プラグインに渡す channel（`uploader`。無ければ `channel`）
    pub uploader: Option<String>,
    pub channel: Option<String>,
    pub title: String,
    /// `YYYYMMDD`
    pub upload_date: Option<String>,
    pub duration_ms: Option<u64>,
    /// `ba[ext=webm]` で取れる音声があるか
    pub has_webm_audio: bool,
}

#[derive(Deserialize)]
struct RawDump {
    #[serde(default, rename = "_type")]
    kind: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    webpage_url: Option<String>,
    #[serde(default)]
    uploader: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    upload_date: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    formats: Vec<RawFormat>,
    #[serde(default)]
    entries: Vec<RawEntry>,
}

#[derive(Deserialize)]
struct RawFormat {
    #[serde(default)]
    ext: Option<String>,
    #[serde(default)]
    acodec: Option<String>,
}

#[derive(Deserialize)]
struct RawEntry {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    webpage_url: Option<String>,
    #[serde(default)]
    ie_key: Option<String>,
}

impl RawEntry {
    /// entry の URL を、ファイルに書く `SOURCE_URL`（動画の `webpage_url` の正規形）と同じ形にする。
    /// YouTube は `--flat-playlist` の `url` が youtu.be 形や `list=` 付きで来ることがあるので、id から
    /// `https://www.youtube.com/watch?v=<id>` を組む。それ以外は webpage_url → url の順
    fn canonical_url(self) -> Option<String> {
        // ホスト名で判定する（path や query に youtube.com を含むだけの別サイトを YouTube にしない）
        fn is_youtube_host(u: &str) -> bool {
            url::Url::parse(u)
                .ok()
                .and_then(|p| p.host_str().map(|h| h.to_ascii_lowercase()))
                .is_some_and(|h| {
                    ["youtube.com", "youtu.be"]
                        .iter()
                        .any(|d| h == *d || h.ends_with(&format!(".{d}")))
                })
        }
        let is_youtube = self.ie_key.as_deref() == Some("Youtube")
            || [&self.url, &self.webpage_url]
                .into_iter()
                .flatten()
                .any(|u| is_youtube_host(u));
        if is_youtube {
            if let Some(id) = self.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                return Some(format!("https://www.youtube.com/watch?v={id}"));
            }
        }
        self.webpage_url
            .or(self.url)
            .map(|u| u.trim().to_owned())
            .filter(|u| !u.is_empty())
    }
}

/// yt-dlp の dump（JSON 1 つ）を解釈する。playlist は entries の URL だけ、動画は必要な値だけ取る
pub fn parse_dump(bytes: &[u8]) -> Result<Dump, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("UTF-8 でない: {e}"))?;
    let raw: RawDump = serde_json::from_str(text.trim()).map_err(|e| e.to_string())?;
    if raw.kind.as_deref() == Some("playlist") || raw.kind.as_deref() == Some("multi_video") {
        let urls = raw
            .entries
            .into_iter()
            .filter_map(RawEntry::canonical_url)
            .collect();
        return Ok(Dump::Playlist(urls));
    }
    let id = raw.id.filter(|s| !s.trim().is_empty()).ok_or("id が無い")?;
    let webpage_url = raw
        .webpage_url
        .filter(|s| !s.trim().is_empty())
        .ok_or("webpage_url が無い")?;
    let title = raw
        .title
        .filter(|s| !s.trim().is_empty())
        .ok_or("title が無い")?;
    let has_webm_audio = raw.formats.iter().any(|f| {
        f.ext.as_deref() == Some("webm")
            && f.acodec
                .as_deref()
                .is_some_and(|c| c != "none" && !c.is_empty())
    });
    Ok(Dump::Video(VideoInfo {
        id,
        webpage_url,
        uploader: raw.uploader.filter(|s| !s.trim().is_empty()),
        channel: raw.channel.filter(|s| !s.trim().is_empty()),
        title,
        upload_date: raw
            .upload_date
            .filter(|d| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit())),
        duration_ms: raw
            .duration
            .filter(|d| d.is_finite() && *d >= 0.0)
            .map(|d| (d * 1000.0).round() as u64),
        has_webm_audio,
    }))
}

impl VideoInfo {
    /// プラグインの channel（`uploader` が無ければ `channel`、それも無ければ空）
    pub fn channel_key(&self) -> &str {
        self.uploader
            .as_deref()
            .or(self.channel.as_deref())
            .unwrap_or("")
    }

    /// プラグインへの Request
    pub fn item(&self) -> Item {
        Item {
            source: "youtube".into(),
            channel: self.channel_key().to_owned(),
            channel_title: self.channel.clone(),
            id: Some(self.id.clone()),
            url: Some(self.webpage_url.clone()),
            title: self.title.clone(),
            uploaded_at: self
                .upload_date
                .as_deref()
                .map(|d| format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..8])),
            duration_ms: self.duration_ms,
        }
    }

    /// Inbox に置くファイル名 `<YYYYMMDD> <title> [<id>].opus`（名前順 = 公開順 = 採番順。
    /// 日付が無ければ日付なし）。`title` は判定できたトラック名か動画タイトル
    pub fn inbox_file_name(&self, title: &str) -> String {
        let id = sanitize_component(&self.id);
        // 切り詰めは title だけに効かせる（id と日付は落とさない）
        let suffix = format!(" [{id}].opus");
        let prefix = self
            .upload_date
            .as_deref()
            .map(|d| format!("{d} "))
            .unwrap_or_default();
        let budget = crate::domain::relpath::MAX_COMPONENT_BYTES
            .saturating_sub(prefix.len() + suffix.len())
            .max(1);
        let mut title = sanitize_component(title);
        while title.len() > budget {
            title.pop();
        }
        let title = title.trim_end();
        let title = if title.is_empty() { "_" } else { title };
        format!("{prefix}{title}{suffix}")
    }
}

/// Inbox のディレクトリ: `youtube/<albumartist>/<album>`（判定できたとき）か
/// `youtube/_unmatched/<channel>`（受け皿）
pub fn inbox_dir(track: Option<&Track>, channel: &str) -> Result<RelPath, String> {
    let (a, b) = match track {
        Some(t) => (
            sanitize_component(&t.albumartist),
            sanitize_component(&t.album),
        ),
        None => (UNMATCHED_DIR.to_owned(), sanitize_component(channel)),
    };
    let b = if b.is_empty() { "_".to_owned() } else { b };
    RelPath::parse(&format!("{SUBDIR}/{a}/{b}")).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- 本体

/// URL 1 件を処理する。`job_id` は作業領域の名前に使う
pub async fn download_one(
    env: &DownloaderEnv,
    job_id: i64,
    request: &DownloadRequest,
    token: &CancellationToken,
) -> Result<Downloaded, DownloadError> {
    let url = request.url.as_str();
    let check_cancel = || {
        if token.is_cancelled() {
            Err(DownloadError::Cancelled)
        } else {
            Ok(())
        }
    };
    // 1. dump
    let out = match ytdlp(env)
        .args([
            "--dump-single-json",
            "--flat-playlist",
            "--no-download",
            "--no-warnings",
        ])
        .timeout(DUMP_TIMEOUT)
        .arg("--")
        .arg(url)
        .run(token)
        .await
    {
        Ok(out) => out,
        // 対応していない URL は再試行しても変わらない。それ以外の失敗（存在しない・一時的な障害）は
        // yt-dlp の出力から区別できないので再試行
        Err(ProcessError::Failed { stderr, .. }) if is_unsupported_url(&stderr) => {
            return Err(DownloadError::Fatal(format!(
                "yt-dlp が対応していない URL: {}",
                stderr.lines().last().unwrap_or("").trim()
            )));
        }
        Err(e) => return Err(e.into()),
    };
    let video = match parse_dump(&out.stdout)
        .map_err(|e| DownloadError::Fatal(format!("yt-dlp の出力を読めない: {e}")))?
    {
        Dump::Playlist(urls) => {
            let mut enqueued = 0;
            let mut skipped = 0;
            for u in urls {
                check_cancel()?;
                // 取り込み済み（Library / Inbox に SOURCE_URL）は投入しない。entries の URL は
                // `https://www.youtube.com/watch?v=<id>` で、ファイルに書く webpage_url の正規形と同じ
                let key = u.clone();
                if env
                    .db
                    .read(move |c| find_source_url(c, &key))
                    .await?
                    .is_some()
                {
                    skipped += 1;
                    continue;
                }
                if let crate::jobs::EnqueueResult::Inserted(_) =
                    env.jobs.enqueue(new_ytdl_job(&u)).await?
                {
                    enqueued += 1;
                }
            }
            tracing::info!(url, enqueued, skipped, "playlist を展開した");
            return Ok(Downloaded::Playlist { enqueued, skipped });
        }
        Dump::Video(v) => v,
    };
    check_cancel()?;
    // 購読由来（P4-16）か。購読が消えていれば通常の ytdl として振る舞う（skip も通常どおり）
    let subscription = match request.subscription_id {
        Some(id) => env.db.read(move |c| subscription_target(c, id)).await?,
        None => None,
    };
    // 2. プラグイン（ダウンロードの前。skip ならここで終わり、宛先もここで決まる）
    let item = video.item();
    let (track, verdict, message) = match env.provider.resolve(&item, token).await {
        Ok(Outcome::Track(t)) => (Some(t), "ok".to_owned(), None),
        // 購読由来は skip でも投入する（再生リストは人が選んだもの。P4-16）
        Ok(Outcome::Declined { reason, message }) if reason == "skip" && subscription.is_none() => {
            tracing::info!(url, message, "プラグインが skip");
            return Ok(Downloaded::Skipped { message });
        }
        Ok(Outcome::Declined { reason, message }) => {
            tracing::info!(url, reason, "判定できないので受け皿へ");
            (None, reason, Some(message))
        }
        Err(ProviderError::Process(ProcessError::Cancelled)) => {
            return Err(DownloadError::Cancelled)
        }
        Err(e) => return Err(DownloadError::Fatal(e.to_string())),
    };
    // 購読由来（P4-16）: 追記先（albumartist / album / category）は購読の値。プラグインの判定は
    // TITLE / ARTIST に使い、skip / 判定不能でも投入する（再生リストは人が選んだもの）
    let (track, verdict) = match &subscription {
        Some(t) => {
            let track = match track {
                Some(mut tr) => {
                    tr.albumartist = t.albumartist.clone();
                    tr.album = t.album.clone();
                    tr.category = t.category.clone();
                    tr
                }
                None => Track {
                    title: video.title.clone(),
                    artists: vec![t.albumartist.clone()],
                    albumartist: t.albumartist.clone(),
                    album: t.album.clone(),
                    category: t.category.clone(),
                    date: video
                        .upload_date
                        .as_deref()
                        .map(|d| format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..8])),
                    tags: Vec::new(),
                },
            };
            (Some(track), verdict)
        }
        None => (track, verdict),
    };
    let title = track
        .as_ref()
        .map(|t| t.title.clone())
        .unwrap_or_else(|| video.title.clone());
    let dir = inbox_dir(track.as_ref(), video.channel_key()).map_err(DownloadError::Fatal)?;
    let name = video.inbox_file_name(&title);
    let target = dir
        .join(&name)
        .map_err(|e| DownloadError::Fatal(e.to_string()))?;
    let entry = FileEntry {
        source: "youtube".into(),
        url: Some(video.webpage_url.clone()),
        channel: Some(video.channel_key().to_owned()),
        verdict: verdict.clone(),
        message,
        subscription_id: subscription.as_ref().and(request.subscription_id),
        position: subscription.as_ref().and(request.position),
    };
    let category = track.as_ref().and_then(|t| t.category.clone());
    // 揃える購読なら TRACKNUMBER = 再生リストの位置（同期が先に既存の行を揃えて隙間を空けている）
    let track_no = match &subscription {
        Some(t) if t.align => request.position.map(i64::from),
        _ => None,
    };
    // 3. 取り込み済み。Inbox の自分の宛先（rel_path_key で比べる）にあるなら「置いた後に落ちて走査が
    //    先に拾った」再実行の可能性があるので、実ファイルの SOURCE_URL を読み直して自分の成果物なら
    //    ダウンロードせずに続き（サイドカーと投入）だけ済ませる。行はキャッシュで、ファイルが正
    let canonical = video.webpage_url.clone();
    match env.db.read(move |c| find_source_url(c, &canonical)).await? {
        Some(SourceLocated::Library(p)) => {
            return Ok(Downloaded::AlreadyImported {
                location: "Library",
                path: p,
            });
        }
        Some(SourceLocated::Inbox(p)) if canonical_key(&p) == target.key() => {
            if is_own_product(&env.inbox, &target, &video.webpage_url) {
                tracing::info!(url, path = %target, "Inbox に置いた自分の成果物があるので続きだけ済ませる");
                finish_staging(env, &dir, &name, category.as_deref(), entry).await?;
                return Ok(Downloaded::Staged {
                    rel_path: target,
                    verdict,
                });
            }
            match env.inbox.stat(&target) {
                // 行だけ残って実ファイルが無い（消された）: 普通に置き直す
                Err(FsError::NotFound) => {}
                _ => {
                    return Err(DownloadError::Fatal(format!(
                        "Inbox に同名で別の内容のファイルがある: {target}"
                    )))
                }
            }
        }
        Some(SourceLocated::Inbox(p)) => {
            return Ok(Downloaded::AlreadyImported {
                location: "Inbox",
                path: p,
            });
        }
        None => {}
    }
    if !video.has_webm_audio {
        return Err(DownloadError::Fatal(
            "webm の音声形式（ba[ext=webm]）が無い動画".into(),
        ));
    }
    check_cancel()?;
    // 4. download（作業領域はどの終わり方でも消す）
    let work = WorkDir::create(&env.tmp_root, job_id)?;
    let template = work.path.join("%(id)s.%(ext)s");
    ytdlp(env)
        .args([
            "-f",
            "ba[ext=webm]",
            "--no-playlist",
            "--no-warnings",
            "--write-thumbnail",
            "--convert-thumbnails",
            "jpg",
            "-o",
        ])
        .arg(&template)
        .timeout(env.download_timeout)
        .arg("--")
        .arg(url)
        .run(token)
        .await?;
    let webm = work.path.join(format!("{}.webm", video.id));
    if !webm.is_file() {
        return Err(DownloadError::Failed(anyhow::anyhow!(
            "yt-dlp は成功したが {} が無い",
            webm.display()
        )));
    }
    let thumb = work.path.join(format!("{}.jpg", video.id));
    check_cancel()?;
    // 5. remux
    let opus = work.path.join(format!("{}.opus", video.id));
    ExternalCommand::new(&env.ffmpeg)
        .path_style(PathStyle::DotSlash)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .path_arg(&webm)
        .args(["-vn", "-c:a", "copy", "-map_metadata", "-1"])
        .path_arg(&opus)
        .timeout(REMUX_TIMEOUT)
        .run(token)
        .await?;
    // 6. タグ
    let changes = tag_changes(track.as_ref(), &video, track_no);
    let picture = match std::fs::read(&thumb) {
        Ok(bytes) if !bytes.is_empty() => Some(
            lofty::picture::Picture::unchecked(bytes)
                .pic_type(lofty::picture::PictureType::CoverFront)
                .mime_type(lofty::picture::MimeType::Jpeg)
                .build(),
        ),
        Ok(_) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    {
        let mut f = File::options().read(true).write(true).open(&opus)?;
        write_tag_changes(
            &mut f,
            Some("opus"),
            &changes,
            picture.as_ref().map(std::slice::from_ref),
        )
        .map_err(|e| DownloadError::Failed(anyhow::anyhow!("タグを書けない: {e}")))?;
        f.sync_all()?;
    }
    check_cancel()?;
    // 7. category の語彙（プラグインの定義が正。D-69）
    if let Some(cat) = category.clone() {
        env.db
            .write(move |c| crate::db::categories::ensure(c, &cat).map(|_| ()))
            .await?;
    }
    // 8. Archive/youtube/<id>.webm
    let archive_dir = RelPath::parse(SUBDIR).map_err(|e| DownloadError::Fatal(e.to_string()))?;
    let archive_rel = archive_dir
        .join(&format!("{}.webm", sanitize_component(&video.id)))
        .map_err(|e| DownloadError::Fatal(e.to_string()))?;
    env.archive.create_dir_all(&archive_dir)?;
    match put_file(&env.archive, &archive_dir, &archive_rel, &webm) {
        Ok(()) | Err(FsError::Exists) => {}
        Err(e) => return Err(e.into()),
    }
    // 9. Inbox。同名があれば SOURCE_URL が同じときだけ自分の成果物（置いた後に落ちた再実行）として採用
    env.inbox.create_dir_all(&dir)?;
    match put_file(&env.inbox, &dir, &target, &opus) {
        Ok(()) => {}
        Err(FsError::Exists) => {
            if !is_own_product(&env.inbox, &target, &video.webpage_url) {
                return Err(DownloadError::Fatal(format!(
                    "Inbox に同名で別の内容のファイルがある: {target}"
                )));
            }
            tracing::info!(path = %target, "Inbox に置いた自分の成果物があるので採用する");
        }
        Err(e) => return Err(e.into()),
    }
    finish_staging(env, &dir, &name, category.as_deref(), entry).await?;
    tracing::info!(url, path = %target, verdict, "Inbox に置いた");
    Ok(Downloaded::Staged {
        rel_path: target,
        verdict,
    })
}

/// 置いた後の仕上げ: サイドカーの項を足し、ディレクトリを fsync し、inbox ジョブを投入する。
/// どれも失敗すれば再試行（ファイルは置けているので、次の実行は自分の成果物として採用して
/// ここへ戻ってくる）
async fn finish_staging(
    env: &DownloaderEnv,
    dir: &RelPath,
    name: &str,
    category: Option<&str>,
    entry: FileEntry,
) -> Result<(), DownloadError> {
    Sidecar::upsert(&env.inbox, dir, category, name, entry).map_err(|e| {
        DownloadError::Failed(anyhow::anyhow!("spindle-inbox.json を更新できない: {e}"))
    })?;
    env.inbox.fsync_dir(Some(dir))?;
    env.jobs
        .enqueue(crate::jobs::handlers::inbox::new_inbox_job())
        .await?;
    Ok(())
}

/// `target` の `SOURCE_URL` が `url` なら自分の成果物（同じ動画の同じ remux）
fn is_own_product(inbox: &RootDir, target: &RelPath, url: &str) -> bool {
    let Ok(file) = inbox.open_file(target) else {
        return false;
    };
    let ext = target.file_name().rsplit_once('.').map(|(_, e)| e);
    match crate::domain::tags::read_audio_file(file, ext) {
        Ok(af) => af.tags.first(SOURCE_URL_KEY) == Some(url),
        Err(_) => false,
    }
}

/// yt-dlp の stderr が「対応していない URL」か（`Unsupported URL:` / `is not a valid URL`）
fn is_unsupported_url(stderr: &str) -> bool {
    stderr.contains("Unsupported URL") || stderr.contains("is not a valid URL")
}

fn ytdlp(env: &DownloaderEnv) -> ExternalCommand {
    let program = env.ytdlp.first().map(String::as_str).unwrap_or("yt-dlp");
    ExternalCommand::new(program).args(env.ytdlp.iter().skip(1))
}

/// ファイルに書くタグ。判定できたら `Track::tags(track_no)`（`track_no` は購読の位置。無ければ
/// TRACKNUMBER 無しで採番は Inbox）、できなければ TITLE に動画タイトル。どちらも `SOURCE_URL`
fn tag_changes(track: Option<&Track>, video: &VideoInfo, track_no: Option<i64>) -> Vec<TagChange> {
    let pairs: Vec<(String, String)> = match track {
        Some(t) => t.tags(track_no),
        None => vec![("TITLE".to_owned(), video.title.clone())],
    };
    let mut changes: Vec<TagChange> = Vec::new();
    for (k, v) in pairs {
        match changes.iter_mut().find(|c| c.key == k) {
            Some(c) => c.values.get_or_insert_with(Vec::new).push(v),
            None => changes.push(TagChange {
                key: k,
                values: Some(vec![v]),
            }),
        }
    }
    changes.push(TagChange {
        key: SOURCE_URL_KEY.to_owned(),
        values: Some(vec![video.webpage_url.clone()]),
    });
    changes
}

/// `src` の内容を `root` の `dir` 内 tmp へ写して `target` に `RENAME_NOREPLACE`
fn put_file(root: &RootDir, dir: &RelPath, target: &RelPath, src: &Path) -> Result<(), FsError> {
    let (tmp_rel, mut tmp) = root.create_tmp(Some(dir))?;
    let written = (|| -> std::io::Result<()> {
        let mut from = File::open(src)?;
        std::io::copy(&mut from, &mut tmp)?;
        tmp.flush()?;
        tmp.sync_all()
    })();
    if let Err(e) = written {
        let _ = root.unlink(&tmp_rel);
        return Err(e.into());
    }
    if let Err(e) = root.rename_noreplace(&tmp_rel, target) {
        let _ = root.unlink(&tmp_rel);
        return Err(e);
    }
    Ok(())
}

/// 作業領域（`<tmp_root>/<job_id>/`）。drop で消す
struct WorkDir {
    path: PathBuf,
}

impl WorkDir {
    fn create(root: &Path, job_id: i64) -> std::io::Result<WorkDir> {
        let path = root.join(job_id.to_string());
        // 前回の残り（再試行）は消してから
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        std::fs::create_dir_all(&path)?;
        Ok(WorkDir { path })
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %self.path.display(), error = %e, "作業領域を消せない");
            }
        }
    }
}

/// 起動時: 前のプロセスが残した作業領域（`<tmp_root>/<job_id>/`）を消す。消した数を返す。
/// running だったジョブは queued に戻り、走り直せば作業領域を作り直す
pub fn sweep_tmp(tmp_root: &Path) -> usize {
    let entries = match std::fs::read_dir(tmp_root) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    let mut removed = 0;
    for e in entries.flatten() {
        let p = e.path();
        let result = if p.is_dir() {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
        match result {
            Ok(()) => removed += 1,
            Err(err) => {
                tracing::warn!(path = %p.display(), error = %err, "作業領域の残りを消せない")
            }
        }
    }
    removed
}

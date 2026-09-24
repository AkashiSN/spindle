//! `POST /api/ytmusic/download`（SPEC §9、D-70、P3-3）。URL ごとに `ytdl` ジョブを投入する。
//! `POST /api/ytmusic/lookup` / `POST /api/ytmusic/playlist`（D-87）は YouTube 画面の ① の照合。
//! `[ytmusic].enabled` でなければ 404（ハンドラが登録されないので投入しても動かない）

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use std::time::Duration;

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::db::inbox::{find_source_url, SourceLocated};
use crate::import::ytmusic::downloader::new_ytdl_job;
use crate::import::ytmusic::playlist::{parse_playlist_dump, watch_url, Availability};
use crate::jobs::process::{ExternalCommand, ProcessError};

use super::subscriptions::list_id_of;

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;

/// URL 1 件の上限（文字）
const MAX_URL_CHARS: usize = 2048;

#[derive(Deserialize)]
pub struct DownloadBody {
    #[serde(default)]
    pub urls: Vec<String>,
}

#[derive(Serialize)]
pub struct DownloadAccepted {
    /// `urls` と同じ順。同じ URL が既に queued / running ならそのジョブの id
    pub job_ids: Vec<i64>,
}

pub async fn download(
    State(state): State<AppState>,
    Json(body): Json<DownloadBody>,
) -> Result<Response, ApiError> {
    if !state.config.ytmusic.enabled {
        return Ok(error_response(StatusCode::NOT_FOUND, "not_found"));
    }
    let urls: Vec<String> = body.urls.iter().map(|u| u.trim().to_owned()).collect();
    if urls.is_empty() {
        return Ok(error_response_with_message(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "urls が空",
        ));
    }
    for u in &urls {
        if u.is_empty() || u.chars().count() > MAX_URL_CHARS {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("URL が空か長すぎる（{MAX_URL_CHARS} 文字まで）"),
            ));
        }
        if !is_acceptable_url(u) {
            return Ok(error_response_with_message(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("URL の形が不正（http / https でホストのあるもの）: {u}"),
            ));
        }
    }
    let mut job_ids = Vec::with_capacity(urls.len());
    for u in &urls {
        job_ids.push(state.jobs.enqueue(new_ytdl_job(u)).await?.id());
    }
    Ok((StatusCode::ACCEPTED, Json(DownloadAccepted { job_ids })).into_response())
}

/// http / https でホストがあり、空白・制御文字を含まない URL か。ホストは限定しない（yt-dlp の
/// 対応サイトは広く、対応していなければジョブが Fatal で伝える）
fn is_acceptable_url(u: &str) -> bool {
    if u.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    match url::Url::parse(u) {
        Ok(p) => {
            matches!(p.scheme(), "http" | "https") && p.host_str().is_some_and(|h| !h.is_empty())
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------- 照合（D-87）

/// 照合 1 回の URL の上限
const MAX_LOOKUP_URLS: usize = 200;
/// 再生リストの列挙のタイムアウト（同期ジョブの列挙より短い。画面が待つので）
const PLAYLIST_TIMEOUT: Duration = Duration::from_secs(90);
/// 再生リストの列挙を同時に走らせる数（入力のたびに yt-dlp が積み上がらないように）
static PLAYLIST_PERMITS: Semaphore = Semaphore::const_new(2);

#[derive(Deserialize)]
pub struct LookupBody {
    #[serde(default)]
    pub urls: Vec<String>,
}

#[derive(Serialize)]
pub struct LookupResponse {
    /// `urls` と同じ順
    pub items: Vec<LookupItem>,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UrlKind {
    /// YouTube の動画 1 本
    Video,
    /// YouTube の再生リスト（`list=` 付きの動画 URL も yt-dlp は再生リストとして扱う）
    Playlist,
    /// http / https だが YouTube ではない（yt-dlp に任せる。照合はしない）
    Other,
    /// URL として受けない（投入すると 400）
    Invalid,
}

#[derive(Serialize)]
pub struct LookupItem {
    pub url: String,
    pub kind: UrlKind,
    /// 動画の正規形（ファイルの `SOURCE_URL` と同じ形）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_url: Option<String>,
    /// 動画が既にある場所。無ければ null（動画のときだけ）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub located: Option<Located>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_id: Option<String>,
    /// 再生リストの購読。無ければ省く
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription: Option<SubscriptionRef>,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct Located {
    /// "library" / "inbox"
    pub location: &'static str,
    pub path: String,
}

#[derive(Serialize)]
pub struct SubscriptionRef {
    pub id: i64,
    pub albumartist: String,
    pub album: String,
}

/// `POST /api/ytmusic/lookup { urls }`（D-87）: URL ごとに種類と、動画ならライブラリ / Inbox の所在
/// （`SOURCE_URL` の一致）、再生リストなら購読の有無を返す。DB だけを見る（yt-dlp は呼ばない）
pub async fn lookup(
    State(state): State<AppState>,
    Json(body): Json<LookupBody>,
) -> Result<Response, ApiError> {
    if !state.config.ytmusic.enabled {
        return Ok(error_response(StatusCode::NOT_FOUND, "not_found"));
    }
    if body.urls.len() > MAX_LOOKUP_URLS {
        return Ok(error_response_with_message(
            StatusCode::BAD_REQUEST,
            "bad_request",
            format!("URL が多すぎる（{MAX_LOOKUP_URLS} 件まで）"),
        ));
    }
    let urls: Vec<String> = body.urls.iter().map(|u| u.trim().to_owned()).collect();
    let items = state
        .db
        .read(move |c| {
            let mut out = Vec::with_capacity(urls.len());
            for url in urls {
                out.push(lookup_one(c, url)?);
            }
            Ok(out)
        })
        .await?;
    Ok(Json(LookupResponse { items }).into_response())
}

fn lookup_one(c: &rusqlite::Connection, url: String) -> crate::db::Result<LookupItem> {
    let mut item = LookupItem {
        kind: classify(&url),
        url,
        video_url: None,
        located: None,
        list_id: None,
        subscription: None,
    };
    match item.kind {
        UrlKind::Playlist => {
            let list_id = list_id_of(&item.url);
            if let Some(id) = &list_id {
                item.subscription =
                    crate::db::subscriptions::by_list_id(c, id)?.map(|s| SubscriptionRef {
                        id: s.id,
                        albumartist: s.albumartist,
                        album: s.album,
                    });
            }
            item.list_id = list_id;
        }
        UrlKind::Video => {
            if let Some(v) = video_url_of(&item.url) {
                item.located = find_source_url(c, &v)?.map(located);
                item.video_url = Some(v);
            }
        }
        UrlKind::Other | UrlKind::Invalid => {}
    }
    Ok(item)
}

fn located(l: SourceLocated) -> Located {
    match l {
        SourceLocated::Library(path) => Located {
            location: "library",
            path,
        },
        SourceLocated::Inbox(path) => Located {
            location: "inbox",
            path,
        },
    }
}

/// URL の種類。YouTube は再生リスト（`list=`）を動画より先に見る（yt-dlp と同じ扱い）
pub fn classify(url: &str) -> UrlKind {
    if url.is_empty() || url.chars().count() > MAX_URL_CHARS || !is_acceptable_url(url) {
        return UrlKind::Invalid;
    }
    if list_id_of(url).is_some() {
        UrlKind::Playlist
    } else if video_url_of(url).is_some() {
        UrlKind::Video
    } else {
        UrlKind::Other
    }
}

fn is_youtube_host(host: &str) -> bool {
    ["youtube.com", "youtu.be"]
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

fn is_video_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// YouTube の動画 URL の正規形 `https://www.youtube.com/watch?v=<id>`（ファイルの `SOURCE_URL` と同じ）。
/// `watch?v=` / `youtu.be/<id>` / `shorts/<id>` / `live/<id>` を受ける。それ以外は None
pub fn video_url_of(url: &str) -> Option<String> {
    let p = url::Url::parse(url.trim()).ok()?;
    if !matches!(p.scheme(), "http" | "https") {
        return None;
    }
    let host = p.host_str()?.to_ascii_lowercase();
    if !is_youtube_host(&host) {
        return None;
    }
    let mut segs = p.path_segments()?.filter(|s| !s.is_empty());
    let id = if host == "youtu.be" || host.ends_with(".youtu.be") {
        segs.next().map(str::to_owned)
    } else {
        match segs.next() {
            Some("watch") => p
                .query_pairs()
                .find(|(k, _)| k == "v")
                .map(|(_, v)| v.into_owned()),
            Some("shorts") | Some("live") => segs.next().map(str::to_owned),
            _ => None,
        }
    }?;
    let id = id.trim();
    is_video_id(id).then(|| watch_url(id))
}

#[derive(Deserialize)]
pub struct PlaylistBody {
    pub url: String,
}

#[derive(Serialize)]
pub struct PlaylistInfo {
    pub list_id: String,
    pub title: Option<String>,
    /// 再生リストの位置の数（非公開・削除も位置を占める）
    pub entries: usize,
    /// 取れない（非公開・削除）
    pub unavailable: usize,
    /// ライブラリに `SOURCE_URL` がある
    pub in_library: usize,
    /// Inbox に `SOURCE_URL` がある（ライブラリに無いもの）
    pub in_inbox: usize,
    /// どちらにも無い（ダウンロードすると取れるもの）
    pub new: usize,
    /// yt-dlp が一部を取りこぼした（`entries < playlist_count`）
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription: Option<SubscriptionRef>,
}

/// `POST /api/ytmusic/playlist { url }`（D-87）: 再生リストを yt-dlp で列挙し、本数と取り込み済みの数を返す。
/// 読むだけ（ジョブにしない）。同時に 2 本まで、応答を待たずに切られたら yt-dlp も止める（kill_on_drop）
pub async fn playlist(
    State(state): State<AppState>,
    Json(body): Json<PlaylistBody>,
) -> Result<Response, ApiError> {
    if !state.config.ytmusic.enabled {
        return Ok(error_response(StatusCode::NOT_FOUND, "not_found"));
    }
    let url = body.url.trim().to_owned();
    let Some(list_id) = (classify(&url) == UrlKind::Playlist)
        .then(|| list_id_of(&url))
        .flatten()
    else {
        return Ok(error_response_with_message(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "YouTube の再生リストの URL（list= を含む）ではない",
        ));
    };
    let Ok(_permit) = PLAYLIST_PERMITS.acquire().await else {
        return Ok(error_response(StatusCode::SERVICE_UNAVAILABLE, "busy"));
    };
    let cmd = state.config.ytdlp_command();
    let program = cmd.first().map(String::as_str).unwrap_or("yt-dlp");
    let out = ExternalCommand::new(program)
        .args(cmd.iter().skip(1))
        .args([
            "--dump-single-json",
            "--flat-playlist",
            "--no-download",
            "--no-warnings",
        ])
        .timeout(PLAYLIST_TIMEOUT)
        .arg("--")
        .arg(&url)
        .run(&CancellationToken::new())
        .await;
    let out = match out {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!(url, error = %e, "再生リストを列挙できない");
            let why = match &e {
                ProcessError::Failed { stderr, .. } => {
                    stderr.lines().last().unwrap_or("").trim().to_owned()
                }
                other => other.to_string(),
            };
            return Ok(error_response_with_message(
                StatusCode::BAD_GATEWAY,
                "ytdlp_failed",
                format!("再生リストを取れない: {why}"),
            ));
        }
    };
    let dump = match parse_playlist_dump(&out.stdout) {
        Ok(d) => d,
        Err(e) => {
            return Ok(error_response_with_message(
                StatusCode::BAD_GATEWAY,
                "ytdlp_failed",
                format!("yt-dlp の出力を読めない: {e}"),
            ))
        }
    };
    let info = state
        .db
        .read(move |c| {
            let mut info = PlaylistInfo {
                title: dump.title.clone(),
                entries: dump.entries.len(),
                unavailable: 0,
                in_library: 0,
                in_inbox: 0,
                new: 0,
                truncated: dump.truncated(),
                subscription: crate::db::subscriptions::by_list_id(c, &list_id)?.map(|s| {
                    SubscriptionRef {
                        id: s.id,
                        albumartist: s.albumartist,
                        album: s.album,
                    }
                }),
                list_id,
            };
            for e in &dump.entries {
                if e.availability != Availability::Available {
                    info.unavailable += 1;
                    continue;
                }
                match find_source_url(c, &e.url)? {
                    Some(SourceLocated::Library(_)) => info.in_library += 1,
                    Some(SourceLocated::Inbox(_)) => info.in_inbox += 1,
                    None => info.new += 1,
                }
            }
            Ok(info)
        })
        .await?;
    Ok(Json(info).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_youtube_urls() {
        let v = "https://www.youtube.com/watch?v=Ab3dE4fG5hI";
        assert_eq!(classify(v), UrlKind::Video);
        assert_eq!(video_url_of(v).as_deref(), Some(v));
        assert_eq!(
            video_url_of("https://youtu.be/Ab3dE4fG5hI?si=x").as_deref(),
            Some(v)
        );
        assert_eq!(
            video_url_of("https://music.youtube.com/watch?v=Ab3dE4fG5hI&feature=share").as_deref(),
            Some(v)
        );
        assert_eq!(
            video_url_of("https://m.youtube.com/shorts/Ab3dE4fG5hI").as_deref(),
            Some(v)
        );
        // list= があれば再生リスト（yt-dlp もそう扱う）
        assert_eq!(
            classify("https://www.youtube.com/watch?v=Ab3dE4fG5hI&list=PLabc_123"),
            UrlKind::Playlist
        );
        assert_eq!(
            classify("https://music.youtube.com/playlist?list=OLAK5uy_x"),
            UrlKind::Playlist
        );
        // YouTube だが動画でも再生リストでもない / 別サイト / 形が不正
        assert_eq!(classify("https://www.youtube.com/@channel"), UrlKind::Other);
        assert_eq!(
            classify("https://example.com/watch?v=Ab3dE4fG5hI"),
            UrlKind::Other
        );
        assert_eq!(classify("https://notyoutube.com/watch?v=x"), UrlKind::Other);
        assert_eq!(classify("ftp://youtube.com/watch?v=x"), UrlKind::Invalid);
        assert_eq!(classify("not a url"), UrlKind::Invalid);
        assert_eq!(classify(""), UrlKind::Invalid);
        // id に使えない文字
        assert_eq!(video_url_of("https://www.youtube.com/watch?v=a%2Fb"), None);
    }
}

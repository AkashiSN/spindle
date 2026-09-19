//! MusicBrainz 照会（SPEC §7.2「メタデータ照会」、D-21、P2-3）。
//!
//! DiscID（[`Toc::musicbrainz_disc_id`]）で `ws/2/discid/<id>` を引き、無ければ（404）同じ
//! エンドポイントに `?toc=` を付けて fuzzy に引く（MB 側が TOC の近いリリースを返す。
//! プレス違い・登録漏れの DiscID に効く）。
//!
//! 候補は「リリース × medium」。リリースの media のうち、自分の DiscID を持つもの（exact）か、
//! 音声トラック数が同じもの（fuzzy）を候補にする。同人・VTuber・インディーズの国内盤は未登録が
//! 常態なので、0 件は普通の結果（手入力経路は P2-4）。
//!
//! MB の規約: UA 必須（`[musicbrainz].user_agent`）、1 req/s（`[musicbrainz].rate_limit_per_sec`）。
//! 連続する照会はクライアント内で間隔を空け、503（負荷制限）は 1 度だけ待って再試行する

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::toc::Toc;
use super::LookupError;

/// 照会で要求する付帯情報。トラック（recordings）、アーティスト表記、レーベル / カタログ番号、
/// リリースグループ、ISRC
const INC: &str = "recordings artist-credits labels release-groups isrcs";

/// 候補の 1 トラック
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TrackCandidate {
    /// 表示用の番号（"1"、ビニールなら "A1" など）
    pub number: String,
    /// medium 内の位置（1 始まり）
    pub position: u32,
    pub title: String,
    /// アーティスト表記（トラック固有が無ければリリースのもの）
    pub artist: String,
    pub length_ms: Option<u64>,
    pub recording_id: String,
    pub track_id: String,
    pub isrcs: Vec<String>,
}

/// 候補（リリース × medium）
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReleaseCandidate {
    pub release_id: String,
    pub release_group_id: Option<String>,
    pub title: String,
    /// アーティスト表記（credit を joinphrase で繋いだもの）
    pub artist: String,
    pub date: Option<String>,
    pub country: Option<String>,
    pub status: Option<String>,
    pub barcode: Option<String>,
    pub disambiguation: Option<String>,
    /// (レーベル名, カタログ番号)
    pub labels: Vec<(String, Option<String>)>,
    /// この medium が自分の DiscID を持つ
    pub exact: bool,
    pub medium_position: u32,
    pub medium_count: usize,
    pub medium_title: Option<String>,
    pub format: Option<String>,
    pub tracks: Vec<TrackCandidate>,
}

/// 照会の結果
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DiscLookup {
    pub discid: String,
    /// DiscID そのもので引けた（false なら TOC の fuzzy 照会）
    pub exact: bool,
    pub candidates: Vec<ReleaseCandidate>,
}

#[derive(Debug, thiserror::Error)]
pub enum MbParseError {
    #[error("応答を解釈できない: {0}")]
    Json(#[from] serde_json::Error),
}

// ---------------------------------------------------------------- 応答の形（必要な部分だけ）

#[derive(Deserialize)]
struct Response {
    releases: Vec<Release>,
}

#[derive(Deserialize)]
struct Release {
    id: String,
    title: String,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    barcode: Option<String>,
    #[serde(default)]
    disambiguation: Option<String>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<Credit>,
    #[serde(rename = "release-group", default)]
    release_group: Option<ReleaseGroup>,
    #[serde(rename = "label-info", default)]
    label_info: Vec<LabelInfo>,
    #[serde(default)]
    media: Vec<Medium>,
}

#[derive(Deserialize)]
struct Credit {
    name: String,
    #[serde(default)]
    joinphrase: String,
}

#[derive(Deserialize)]
struct ReleaseGroup {
    id: String,
}

#[derive(Deserialize)]
struct LabelInfo {
    #[serde(rename = "catalog-number", default)]
    catalog_number: Option<String>,
    #[serde(default)]
    label: Option<Label>,
}

#[derive(Deserialize)]
struct Label {
    name: String,
}

#[derive(Deserialize)]
struct Medium {
    position: u32,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(rename = "track-count")]
    track_count: usize,
    #[serde(default)]
    discs: Vec<Disc>,
    #[serde(default)]
    tracks: Vec<Track>,
}

#[derive(Deserialize)]
struct Disc {
    id: String,
}

#[derive(Deserialize)]
struct Track {
    id: String,
    number: String,
    position: u32,
    title: String,
    #[serde(default)]
    length: Option<u64>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<Credit>,
    recording: Recording,
}

#[derive(Deserialize)]
struct Recording {
    id: String,
    #[serde(default)]
    isrcs: Vec<String>,
}

fn join_credit(credit: &[Credit]) -> String {
    let mut s = String::new();
    for c in credit {
        s.push_str(&c.name);
        s.push_str(&c.joinphrase);
    }
    s
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.trim().is_empty())
}

/// 応答（`ws/2/discid/…` の JSON）を候補に直す。`discid` を持つ medium は exact、
/// そうでなければ音声トラック数 `audio_tracks` が同じ medium を fuzzy の候補にする。exact が先
pub fn parse_lookup(
    json: &str,
    discid: &str,
    audio_tracks: usize,
) -> Result<Vec<ReleaseCandidate>, MbParseError> {
    let resp: Response = serde_json::from_str(json)?;
    let mut out = Vec::new();
    for r in &resp.releases {
        let artist = join_credit(&r.artist_credit);
        for m in &r.media {
            let exact = m.discs.iter().any(|d| d.id == discid);
            if !exact && m.track_count != audio_tracks {
                continue;
            }
            let tracks = m
                .tracks
                .iter()
                .map(|t| TrackCandidate {
                    number: t.number.clone(),
                    position: t.position,
                    title: t.title.clone(),
                    artist: if t.artist_credit.is_empty() {
                        artist.clone()
                    } else {
                        join_credit(&t.artist_credit)
                    },
                    length_ms: t.length,
                    recording_id: t.recording.id.clone(),
                    track_id: t.id.clone(),
                    isrcs: t.recording.isrcs.clone(),
                })
                .collect();
            out.push(ReleaseCandidate {
                release_id: r.id.clone(),
                release_group_id: r.release_group.as_ref().map(|g| g.id.clone()),
                title: r.title.clone(),
                artist: artist.clone(),
                date: non_empty(r.date.clone()),
                country: non_empty(r.country.clone()),
                status: non_empty(r.status.clone()),
                barcode: non_empty(r.barcode.clone()),
                disambiguation: non_empty(r.disambiguation.clone()),
                labels: r
                    .label_info
                    .iter()
                    .filter_map(|li| {
                        li.label
                            .as_ref()
                            .map(|l| (l.name.clone(), non_empty(li.catalog_number.clone())))
                    })
                    .collect(),
                exact,
                medium_position: m.position,
                medium_count: r.media.len(),
                medium_title: non_empty(m.title.clone()),
                format: non_empty(m.format.clone()),
                tracks,
            });
        }
    }
    // exact を先に（安定ソートで MB の順は保つ）
    out.sort_by_key(|c| !c.exact);
    Ok(out)
}

// ---------------------------------------------------------------- クライアント

/// MusicBrainz の照会。`base` は `https://musicbrainz.org/ws/2/`（末尾 `/`。設定で差し替え可）
#[derive(Debug, Clone)]
pub struct MusicBrainzClient {
    base: String,
    http: reqwest::Client,
    /// 直前の要求の時刻。ロックを持ったまま待つので、並行する照会も直列に間隔が空く
    last_request: Arc<tokio::sync::Mutex<Option<Instant>>>,
    min_interval: Duration,
}

impl MusicBrainzClient {
    /// `min_interval` は要求の最小間隔（規約は 1 秒。`rate_limit_per_sec` から `1s / n`）
    pub fn new(
        base: impl Into<String>,
        user_agent: &str,
        min_interval: Duration,
    ) -> Result<Self, LookupError> {
        let mut base = base.into();
        if !base.ends_with('/') {
            base.push('/');
        }
        Ok(Self {
            base,
            http: super::http_client(user_agent)?,
            last_request: Arc::new(tokio::sync::Mutex::new(None)),
            min_interval,
        })
    }

    /// 間隔を空けて GET。503 は 1 度だけ間隔ぶん待って再試行する
    async fn get(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<(reqwest::StatusCode, String), LookupError> {
        let url = format!("{}{}", self.base, path);
        for attempt in 0..2 {
            {
                let mut last = self.last_request.lock().await;
                if let Some(t) = *last {
                    let wait = self.min_interval.saturating_sub(t.elapsed());
                    if !wait.is_zero() {
                        tokio::time::sleep(wait).await;
                    }
                }
                *last = Some(Instant::now());
            }
            let resp = self.http.get(&url).query(query).send().await?;
            let status = resp.status();
            if status == reqwest::StatusCode::SERVICE_UNAVAILABLE && attempt == 0 {
                tracing::warn!(url, "MusicBrainz が 503。間隔を空けて再試行");
                continue;
            }
            let text = resp.text().await?;
            return Ok((status, text));
        }
        Err(LookupError::Status(503))
    }

    /// TOC から DiscID を出して引く。無ければ TOC で fuzzy に引く。どちらも無ければ候補は空
    pub async fn lookup_disc(&self, toc: &Toc) -> Result<DiscLookup, LookupError> {
        let discid = toc.musicbrainz_disc_id();
        let audio_tracks = toc.audio_tracks().count();
        let path = format!("discid/{discid}");
        // cdstubs=no: 未登録 DiscID に CD stub（品質の低い匿名投稿）があると 200 で別の形が返り、
        // 404 → TOC の fuzzy に進めない。候補にも入れない（D-64）
        let (status, body) = self
            .get(&path, &[("inc", INC), ("fmt", "json"), ("cdstubs", "no")])
            .await?;
        if status.is_success() {
            let candidates = parse_lookup(&body, &discid, audio_tracks)
                .map_err(|e| LookupError::Parse(e.to_string()))?;
            return Ok(DiscLookup {
                discid,
                exact: true,
                candidates,
            });
        }
        if status != reqwest::StatusCode::NOT_FOUND {
            return Err(LookupError::Status(status.as_u16()));
        }
        let mb_toc = toc.musicbrainz_toc();
        let (status, body) = self
            .get(
                &path,
                &[
                    ("toc", mb_toc.as_str()),
                    ("inc", INC),
                    ("fmt", "json"),
                    ("cdstubs", "no"),
                ],
            )
            .await?;
        let candidates = if status.is_success() {
            parse_lookup(&body, &discid, audio_tracks)
                .map_err(|e| LookupError::Parse(e.to_string()))?
        } else if status == reqwest::StatusCode::NOT_FOUND {
            Vec::new()
        } else {
            return Err(LookupError::Status(status.as_u16()));
        };
        Ok(DiscLookup {
            discid,
            exact: false,
            candidates,
        })
    }
}

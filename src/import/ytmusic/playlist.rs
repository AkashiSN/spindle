//! 再生リストの列挙と番号揃えの計画（SPEC §7.7「再生リストの購読」、D-78、P4-16）。純粋な部分だけ。
//! yt-dlp の起動・DB・バッチの投入は `jobs::handlers::playlist_sync`
//!
//! - 列挙は `yt-dlp --flat-playlist --dump-single-json` の JSON。entry の位置（1 始まり）が再生リストの
//!   位置で、非公開・削除の entry も位置を占める（`title` が無い、または `[Private video]` 等）
//! - 揃えは「再生リストの位置 = TRACKNUMBER」。`SOURCE_URL` で一致した行だけ動かし、それ以外の行
//!   （`SOURCE_URL` 無し・再生リストに無い・disc 2 以降・`SOURCE_URL` 重複）は固定して番号を塞ぐ

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

/// 取れない entry の種別。現行の yt-dlp は `title: null` で来るので区別できず `unknown`。旧版の
/// `[Private video]` / `[Deleted video]` から分かるときだけ private / deleted
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnavailableKind {
    Private,
    Deleted,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Available,
    Unavailable(UnavailableKind),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistEntry {
    /// 1 始まり
    pub position: u32,
    pub id: String,
    /// `https://www.youtube.com/watch?v=<id>`（ファイルの `SOURCE_URL` と同じ正規形）
    pub url: String,
    pub title: Option<String>,
    pub availability: Availability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistDump {
    pub title: Option<String>,
    /// yt-dlp が報告する総数。entries より多ければ取りこぼし（古い yt-dlp）
    pub playlist_count: Option<usize>,
    pub entries: Vec<PlaylistEntry>,
}

impl PlaylistDump {
    /// 古い yt-dlp が continuation を黙って取りこぼした（`entries < playlist_count`）
    pub fn truncated(&self) -> bool {
        self.playlist_count
            .is_some_and(|count| self.entries.len() < count)
    }
}

#[derive(Deserialize)]
struct RawDump {
    #[serde(default, rename = "_type")]
    kind: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    playlist_count: Option<i64>,
    #[serde(default)]
    entries: Vec<RawEntry>,
}

#[derive(Deserialize)]
struct RawEntry {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

/// 動画の正規形 URL（`SOURCE_URL` と同じ）
pub fn watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

/// yt-dlp の `--flat-playlist --dump-single-json` を解釈する。playlist でなければ Err
pub fn parse_playlist_dump(bytes: &[u8]) -> Result<PlaylistDump, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("UTF-8 でない: {e}"))?;
    let raw: RawDump = serde_json::from_str(text.trim()).map_err(|e| e.to_string())?;
    if !matches!(raw.kind.as_deref(), Some("playlist") | Some("multi_video")) {
        return Err("再生リストでない（動画 1 本の URL）".into());
    }
    let entries = raw
        .entries
        .into_iter()
        .enumerate()
        .map(|(i, e)| {
            let position = u32::try_from(i + 1).unwrap_or(u32::MAX);
            let id = e.id.map(|s| s.trim().to_owned()).unwrap_or_default();
            let title = e
                .title
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty());
            let unavailable = match title.as_deref().map(str::to_ascii_lowercase).as_deref() {
                Some("[private video]") => Some(UnavailableKind::Private),
                Some("[deleted video]") => Some(UnavailableKind::Deleted),
                Some("[unavailable video]") | None => Some(UnavailableKind::Unknown),
                Some(_) if id.is_empty() => Some(UnavailableKind::Unknown),
                Some(_) => None,
            };
            let (title, availability) = match unavailable {
                Some(kind) => (None, Availability::Unavailable(kind)),
                None => (title, Availability::Available),
            };
            PlaylistEntry {
                position,
                url: watch_url(&id),
                id,
                title,
                availability,
            }
        })
        .collect();
    Ok(PlaylistDump {
        title: raw.title.filter(|t| !t.trim().is_empty()),
        playlist_count: raw.playlist_count.and_then(|n| usize::try_from(n).ok()),
        entries,
    })
}

// ---------------------------------------------------------------- 番号揃えの計画

/// 追記先 album の active な行（DB から）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryRow {
    pub track_id: i64,
    /// NULL は 1 とみなす
    pub disc_no: Option<i64>,
    pub track_no: Option<i64>,
    pub source_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AlignMove {
    pub track_id: i64,
    pub position: u32,
    pub current_no: Option<i64>,
    pub target_no: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockReason {
    /// 目標番号を固定行（`SOURCE_URL` 無し・再生リストに無い・重複）が使っている
    NumberTaken { by_track_id: i64 },
    /// 同じ `SOURCE_URL` の行が複数ある
    DuplicateSourceUrl,
    /// disc 2 以降の行
    OtherDisc { disc_no: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AlignBlocked {
    pub track_id: i64,
    pub position: u32,
    pub current_no: Option<i64>,
    pub reason: BlockReason,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AlignPlan {
    /// 位置順
    pub moves: Vec<AlignMove>,
    pub blocked: Vec<AlignBlocked>,
    /// 一致していて番号も合っている行数
    pub unchanged: usize,
    /// Library（この album）に無い entry の位置
    pub missing: Vec<u32>,
    /// `SOURCE_URL` 付きだが再生リストに無い行数（触らない）
    pub outsiders: usize,
    /// `SOURCE_URL` の無い行数（触らない）
    pub unnumbered: usize,
}

impl AlignPlan {
    pub fn is_noop(&self) -> bool {
        self.moves.is_empty()
    }
}

/// 位置 ↔ TRACKNUMBER の差分。対象同士の swap / cycle は可（番号は UNIQUE でなく、rename は一時パス
/// 経由）。固定行の番号（disc 1 のもの）だけを塞ぐ
pub fn plan_align(entries: &[PlaylistEntry], rows: &[LibraryRow]) -> AlignPlan {
    let mut by_url: HashMap<&str, Vec<&LibraryRow>> = HashMap::new();
    let mut plan = AlignPlan::default();
    let in_playlist: HashMap<&str, u32> = entries
        .iter()
        .map(|e| (e.url.as_str(), e.position))
        .collect();
    // 固定行 = 対象でない disc 1 の行が使っている番号（重複 URL の行も動かさないので固定）
    let mut fixed: BTreeMap<i64, i64> = BTreeMap::new();
    for r in rows {
        match r.source_url.as_deref() {
            Some(u) if in_playlist.contains_key(u) => by_url.entry(u).or_default().push(r),
            Some(_) => plan.outsiders += 1,
            None => plan.unnumbered += 1,
        }
    }
    let is_disc1 = |r: &LibraryRow| r.disc_no.unwrap_or(1) == 1;
    for r in rows {
        let target = r
            .source_url
            .as_deref()
            .is_some_and(|u| by_url.get(u).is_some_and(|v| v.len() == 1));
        if !target && is_disc1(r) {
            if let Some(n) = r.track_no {
                fixed.entry(n).or_insert(r.track_id);
            }
        }
    }
    for e in entries {
        let Some(matched) = by_url.get(e.url.as_str()) else {
            plan.missing.push(e.position);
            continue;
        };
        if matched.len() > 1 {
            for r in matched {
                plan.blocked.push(AlignBlocked {
                    track_id: r.track_id,
                    position: e.position,
                    current_no: r.track_no,
                    reason: BlockReason::DuplicateSourceUrl,
                });
            }
            continue;
        }
        let r = matched[0];
        let target_no = i64::from(e.position);
        if !is_disc1(r) {
            plan.blocked.push(AlignBlocked {
                track_id: r.track_id,
                position: e.position,
                current_no: r.track_no,
                reason: BlockReason::OtherDisc {
                    disc_no: r.disc_no.unwrap_or(1),
                },
            });
            continue;
        }
        if r.track_no == Some(target_no) {
            plan.unchanged += 1;
            continue;
        }
        if let Some(&by) = fixed.get(&target_no) {
            plan.blocked.push(AlignBlocked {
                track_id: r.track_id,
                position: e.position,
                current_no: r.track_no,
                reason: BlockReason::NumberTaken { by_track_id: by },
            });
            continue;
        }
        plan.moves.push(AlignMove {
            track_id: r.track_id,
            position: e.position,
            current_no: r.track_no,
            target_no,
        });
    }
    plan
}

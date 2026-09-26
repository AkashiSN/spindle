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
//! DiscID 以外の経路（D-64 追記、追記 4）: 段は 3 つで、上の段で候補が残れば下は引かない。
//! `discid`（`ws/2/discid/<id>`）→ `ids`（ディスクが持つ ISRC の検索 `ws/2/recording?query=isrc:…` と
//! MCN = バーコードの検索 `ws/2/release?query=barcode:…`）→ `toc`（`ws/2/discid/<id>?toc=` の近似）。
//! TOC 近似はトラック長の近い別の盤を大量に返すので最後の手段にする。ユーザが貼ったリリース
//! URL / MBID（`ws/2/release/<id>`）と、ユーザが盤から読んで入れた品番の検索（`ws/2/release?query=catno:…`。
//! D-94）は段に関係なく常に足す。同じリリース × medium は 1 件に束ねて
//! 経路（[`MatchedBy`]）を付ける。[`DiscQuery::widen`] を立てると段を打ち切らずに全部引く。
//!
//! DiscID が 200 でも medium のトラック数が合わず候補が 0 件のときは次の段へ落とす（そこで止めると
//! 行き止まりになる）。ただし `exact` は真のままにする（DiscID は登録済みなので登録を勧めない）
//!
//! MB の規約: UA 必須（`[musicbrainz].user_agent`）、1 req/s（`[musicbrainz].rate_limit_per_sec`）。
//! 連続する照会はクライアント内で間隔を空け、503（負荷制限）は 1 度だけ待って再試行する

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::toc::Toc;
use super::LookupError;
use crate::config::AddressFamily;

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
    /// (レーベル名, カタログ番号)。レーベルが未登録（label が null）で品番だけある label-info は
    /// レーベル名を空文字にして残す（品番で版を探すのに要る。D-94）
    pub labels: Vec<(String, Option<String>)>,
    /// この medium が自分の DiscID を持つ
    pub exact: bool,
    /// どの経路で出てきたか（強い順。[`merge_candidates`] が付ける）
    pub matched_by: Vec<MatchedBy>,
    pub medium_position: u32,
    pub medium_count: usize,
    /// リリース全体の収録構成（この medium を含む。DVD 付き / BD 付き / デジタルの区別に使う）
    pub media: Vec<MediumInfo>,
    pub medium_title: Option<String>,
    pub format: Option<String>,
    pub tracks: Vec<TrackCandidate>,
}

/// 候補が出てきた経路。並びは強い順（束ねたときの表示順・並び順に使う）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchedBy {
    /// ユーザが盤から読んで入れた品番が、リリースのカタログ番号と一致（D-94）。収録が同じ版どうしは
    /// DiscID が同じになるので、版を決めるのは品番の方。入力があるときだけ付く
    Catno,
    /// medium が自分の DiscID を持つ
    Discid,
    /// ユーザが指定したリリース
    Release,
    /// ディスクの ISRC がそのリリースの recording に付いている
    Isrc,
    /// ディスクの MCN がリリースのバーコードと一致
    Barcode,
    /// TOC の fuzzy 照会
    Toc,
}

/// リリースに入っている 1 枚（候補の medium かどうかに関わらず並べる）
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MediumInfo {
    pub position: u32,
    /// 形式（`CD` / `Blu-ray` / `Digital Media` など。MB に無ければ null）
    pub format: Option<String>,
    pub track_count: usize,
}

/// 照会の結果
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DiscLookup {
    pub discid: String,
    /// DiscID そのもので引けた（false なら ISRC / バーコード / TOC 近似）。候補が 0 件でも、
    /// DiscID が登録されていれば真（登録を勧めないため）
    pub exact: bool,
    /// どの段で止まったか
    pub stage: LookupStage,
    pub candidates: Vec<ReleaseCandidate>,
    /// 候補に入れられなかった理由（指定リリースにトラック数の合う medium が無い、など）
    pub notes: Vec<String>,
}

impl DiscLookup {
    /// まだ引いていない段があるか（画面の「さらに広げて探す」を出すか）。
    /// `exact` は見ない: DiscID で当たっても候補が 0 件なら [`LookupStage::Ids`] まで落ちていて、
    /// そこからは広げられる
    pub fn can_widen(&self) -> bool {
        self.stage == LookupStage::Ids
    }
}

/// 照会が止まった段（強い順）。上の段で候補が残れば下は引かない（D-64 追記 4）
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LookupStage {
    /// DiscID そのもので当たって、候補も残った
    Discid,
    /// ISRC / バーコード（と指定リリース）で当たった。TOC 近似は引いていない
    Ids,
    /// TOC 近似まで引いた（最後の段）
    Toc,
}

/// 照会の入力。`isrcs` はディスクから読めたものだけ（None は除く）
#[derive(Debug, Clone, Copy)]
pub struct DiscQuery<'a> {
    pub toc: &'a Toc,
    pub isrcs: &'a [String],
    pub mcn: Option<&'a str>,
    /// ユーザが貼ったリリース URL か MBID（[`parse_release_ref`]）
    pub release: Option<&'a str>,
    /// ユーザが盤から読んで入れた品番（D-94）。形は [`normalize_catno`] が通すもの
    pub catno: Option<&'a str>,
    /// 覚えている結果を捨てて引き直す（画面の「MusicBrainz に照会」。自動の照会は false）
    pub refresh: bool,
    /// 段を打ち切らずに全部引く（画面の「さらに広げて探す」）
    pub widen: bool,
}

/// 覚えるときの鍵。入力が同じなら同じ結果になる。`DiscQuery` を全フィールド分解して作るので、
/// 入力を足したらここが壊れて気づく
#[derive(Debug, Clone, PartialEq, Eq)]
struct CacheKey {
    toc: String,
    isrcs: Vec<String>,
    mcn: Option<String>,
    release: Option<ReleaseKey>,
    /// 品番は正規化した形（表記揺れは同じ結果になる）
    catno: Option<String>,
    /// 広げて引いた結果は別物（普通の照会に返してはいけない）
    widen: bool,
}

/// リリースの指定。「無し」「読めない文字列」「MBID」は結果が違う（読めない指定は note を返す）ので
/// 別の鍵にする
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReleaseKey {
    Id(String),
    Unparsable(String),
}

impl DiscQuery<'_> {
    fn cache_key(&self) -> CacheKey {
        let DiscQuery {
            toc,
            isrcs,
            mcn,
            release,
            catno,
            // 覚えるかどうかの指示で、結果そのものは変わらない
            refresh: _,
            widen,
        } = *self;
        CacheKey {
            toc: toc.ctdb_toc(),
            isrcs: isrcs.to_vec(),
            mcn: mcn.map(str::to_owned),
            release: release.map(|r| match parse_release_ref(r) {
                Some(id) => ReleaseKey::Id(id),
                None => ReleaseKey::Unparsable(r.to_owned()),
            }),
            catno: catno.map(|c| normalize_catno(c).unwrap_or_else(|| c.to_owned())),
            widen,
        }
    }
}

/// 品番を比べる形にする（D-94）: 大文字にし、ハイフン・空白を落とす。MusicBrainz の検索も
/// 同じ揺れを吸収する（`catno:"upcj 9001"` と `UPCJ9001` は `UPCJ-9001` に当たる）。英数字・
/// ハイフン・空白以外を含む、または英数字が無いものは `None`（検索に載せない。Lucene の記号を通さない）
pub fn normalize_catno(s: &str) -> Option<String> {
    let s = s.trim();
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == ' ')
    {
        return None;
    }
    let n: String = s
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect();
    (!n.is_empty()).then_some(n)
}

/// 候補のカタログ番号のどれかが品番（[`normalize_catno`] 済み）と一致するか
pub fn candidate_has_catno(c: &ReleaseCandidate, normalized: &str) -> bool {
    c.labels
        .iter()
        .any(|(_, cat)| cat.as_deref().and_then(normalize_catno).as_deref() == Some(normalized))
}

/// 覚えておく期限。ディスクを入れ替えずに画面を開き直したときの引き直しを止めるのが目的で、
/// 長く持つほどではない（DiscID を登録した直後に引き直したいことがある）
const CACHE_TTL: Duration = Duration::from_secs(10 * 60);
/// 覚えておく件数（1 セッションで扱う枚数は多くない）。超えたら古いものから捨てる
const CACHE_CAPACITY: usize = 8;

/// ISRC 検索で 1 度に取りに行くリリースの上限（1 req/s なので秒数がそのまま増える）
const ISRC_FETCH_LIMIT: usize = 5;
/// バーコード検索で取りに行くリリースの上限
const BARCODE_FETCH_LIMIT: usize = 3;
/// 品番検索で取りに行くリリースの上限（D-94。バーコードと同じ）
const CATNO_FETCH_LIMIT: usize = 3;

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
    Ok(candidates_from(&resp.releases, discid, audio_tracks))
}

/// `ws/2/release/<id>`（リリース 1 件）の応答を候補に直す。基準は [`parse_lookup`] と同じ
pub fn parse_release(
    json: &str,
    discid: &str,
    audio_tracks: usize,
) -> Result<Vec<ReleaseCandidate>, MbParseError> {
    let release: Release = serde_json::from_str(json)?;
    Ok(candidates_from(
        std::slice::from_ref(&release),
        discid,
        audio_tracks,
    ))
}

/// `ws/2/recording?query=isrc:…`（検索）の応答から、recording が載るリリースの id を
/// 「ディスクの ISRC が付いた recording の数」の多い順（同数は出てきた順）に
pub fn parse_recording_search(json: &str) -> Result<Vec<String>, MbParseError> {
    #[derive(Deserialize)]
    struct Resp {
        #[serde(default)]
        recordings: Vec<Rec>,
    }
    #[derive(Deserialize)]
    struct Rec {
        #[serde(default)]
        releases: Vec<Ref>,
    }
    #[derive(Deserialize)]
    struct Ref {
        id: String,
    }
    let resp: Resp = serde_json::from_str(json)?;
    let mut order: Vec<String> = Vec::new();
    let mut hits: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for rec in &resp.recordings {
        let mut seen_in_rec = std::collections::HashSet::new();
        for r in &rec.releases {
            if !seen_in_rec.insert(r.id.clone()) {
                continue;
            }
            let n = hits.entry(r.id.clone()).or_insert(0);
            if *n == 0 {
                order.push(r.id.clone());
            }
            *n += 1;
        }
    }
    // 一致数の多い順。安定ソートで出てきた順は保つ
    order.sort_by_key(|id| std::cmp::Reverse(hits.get(id).copied().unwrap_or(0)));
    Ok(order)
}

/// `ws/2/release?query=…`（検索）の応答からリリース id を順に
pub fn parse_release_search(json: &str) -> Result<Vec<String>, MbParseError> {
    #[derive(Deserialize)]
    struct Resp {
        #[serde(default)]
        releases: Vec<Ref>,
    }
    #[derive(Deserialize)]
    struct Ref {
        id: String,
    }
    let resp: Resp = serde_json::from_str(json)?;
    Ok(resp.releases.into_iter().map(|r| r.id).collect())
}

/// リリースグループの版（`ws/2/release?release-group=<id>` の 1 件。D-93）。Inbox の承認画面で
/// 「表の画像だけ別の版から取る」ために並べる
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GroupRelease {
    pub release_id: String,
    pub title: String,
    pub disambiguation: Option<String>,
    pub date: Option<String>,
    pub country: Option<String>,
    pub status: Option<String>,
    /// 媒体の形式（媒体の順。形式の無い媒体は None）
    pub formats: Vec<Option<String>>,
    /// 最初のレーベル
    pub label: Option<String>,
    /// カタログ番号（重ねずに ` / ` で並べる。複数枚組は枚ごとに番号がある）
    pub catalog_number: Option<String>,
    /// Cover Art Archive に表の画像がある（`cover-art-archive.front`）
    pub front: bool,
}

/// リリースグループの版の一覧と、グループにある版の総数（1 回で取るのは [`GROUP_RELEASE_LIMIT`] 件まで）
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GroupReleases {
    pub releases: Vec<GroupRelease>,
    pub total: usize,
}

/// リリースグループの版を 1 回で取る件数（MusicBrainz の browse の上限）。これを超えるグループは
/// 先頭のぶんだけ並べる（残りは URL の貼り付けで指せる）
pub const GROUP_RELEASE_LIMIT: usize = 100;

/// `ws/2/release?release-group=…`（browse）の応答を版の一覧に直す。表の画像のある版を先に、
/// 同じなら日付の古い順（日付の無い版は後ろ。それ以外は MusicBrainz の順）
pub fn parse_group_releases(json: &str) -> Result<GroupReleases, MbParseError> {
    #[derive(Deserialize)]
    struct Resp {
        #[serde(rename = "release-count", default)]
        release_count: usize,
        releases: Vec<R>,
    }
    #[derive(Deserialize)]
    struct R {
        id: String,
        title: String,
        #[serde(default)]
        date: Option<String>,
        #[serde(default)]
        country: Option<String>,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        disambiguation: Option<String>,
        #[serde(rename = "label-info", default)]
        label_info: Vec<LabelInfo>,
        #[serde(default)]
        media: Vec<M>,
        #[serde(rename = "cover-art-archive", default)]
        cover_art_archive: Option<Caa>,
    }
    #[derive(Deserialize)]
    struct M {
        #[serde(default)]
        format: Option<String>,
    }
    #[derive(Deserialize)]
    struct Caa {
        #[serde(default)]
        front: bool,
    }
    let resp: Resp = serde_json::from_str(json)?;
    let mut releases: Vec<GroupRelease> = resp
        .releases
        .into_iter()
        .map(|r| {
            let mut catalogs: Vec<String> = Vec::new();
            for c in r
                .label_info
                .iter()
                .filter_map(|l| non_empty(l.catalog_number.clone()))
            {
                if !catalogs.contains(&c) {
                    catalogs.push(c);
                }
            }
            GroupRelease {
                release_id: r.id.to_ascii_lowercase(),
                title: r.title,
                disambiguation: non_empty(r.disambiguation),
                date: non_empty(r.date),
                country: non_empty(r.country),
                status: non_empty(r.status),
                formats: r.media.into_iter().map(|m| non_empty(m.format)).collect(),
                label: r
                    .label_info
                    .iter()
                    .find_map(|l| l.label.as_ref().map(|x| x.name.clone())),
                catalog_number: (!catalogs.is_empty()).then(|| catalogs.join(" / ")),
                front: r.cover_art_archive.is_some_and(|c| c.front),
            }
        })
        .collect();
    releases.sort_by(|a, b| {
        b.front
            .cmp(&a.front)
            .then_with(|| match (&a.date, &b.date) {
                (Some(x), Some(y)) => x.cmp(y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
    });
    let total = resp.release_count.max(releases.len());
    Ok(GroupReleases { releases, total })
}

/// ユーザが貼ったリリースの指定から MBID（小文字）を取り出す。素の UUID か、musicbrainz.org
/// （サブドメイン可）の `/release/<uuid>` を含む URL。他のエンティティ（recording 等）は受けない
pub fn parse_release_ref(s: &str) -> Option<String> {
    let s = s.trim();
    let is_uuid = |t: &str| {
        t.len() == 36
            && t.bytes().enumerate().all(|(i, b)| match i {
                8 | 13 | 18 | 23 => b == b'-',
                _ => b.is_ascii_hexdigit(),
            })
    };
    if is_uuid(s) {
        return Some(s.to_ascii_lowercase());
    }
    let rest = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let (host, path) = rest.split_once('/')?;
    if !(host == "musicbrainz.org" || host.ends_with(".musicbrainz.org")) {
        return None;
    }
    let after = path.strip_prefix("release/")?;
    let id = after.split(['/', '?', '#']).next()?;
    is_uuid(id).then(|| id.to_ascii_lowercase())
}

/// 経路ごとの候補を 1 つの一覧に束ねる。同じリリース × medium は 1 件（経路を強い順に並べて
/// 持つ）。並びは最強の経路の順、同じ経路の中は出てきた順。DiscID を持つ medium は fuzzy の応答に
/// 混ざっていても（D-64）discid 経路として扱う
pub fn merge_candidates(groups: Vec<(MatchedBy, Vec<ReleaseCandidate>)>) -> Vec<ReleaseCandidate> {
    let mut out: Vec<ReleaseCandidate> = Vec::new();
    let mut index: std::collections::HashMap<(String, u32), usize> =
        std::collections::HashMap::new();
    let mut groups = groups;
    groups.sort_by_key(|(by, _)| *by);
    for (by, cands) in groups {
        for mut c in cands {
            let key = (c.release_id.clone(), c.medium_position);
            match index.get(&key) {
                Some(&i) => {
                    let existing = &mut out[i];
                    if !existing.matched_by.contains(&by) {
                        existing.matched_by.push(by);
                    }
                    if c.exact {
                        existing.exact = true;
                    }
                }
                None => {
                    c.matched_by = vec![by];
                    index.insert(key, out.len());
                    out.push(c);
                }
            }
        }
    }
    for c in &mut out {
        if c.exact && !c.matched_by.contains(&MatchedBy::Discid) {
            c.matched_by.push(MatchedBy::Discid);
        }
        c.matched_by.sort();
        c.matched_by.dedup();
    }
    out.sort_by_key(|c| c.matched_by.first().copied().unwrap_or(MatchedBy::Toc));
    out
}

fn candidates_from(
    releases: &[Release],
    discid: &str,
    audio_tracks: usize,
) -> Vec<ReleaseCandidate> {
    let mut out = Vec::new();
    for r in releases {
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
                        let catno = non_empty(li.catalog_number.clone());
                        match &li.label {
                            Some(l) => Some((l.name.clone(), catno)),
                            None => catno.map(|c| (String::new(), Some(c))),
                        }
                    })
                    .collect(),
                exact,
                matched_by: Vec::new(),
                media: r
                    .media
                    .iter()
                    .map(|m| MediumInfo {
                        position: m.position,
                        format: non_empty(m.format.clone()),
                        track_count: m.track_count,
                    })
                    .collect(),
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
    out
}

// ---------------------------------------------------------------- クライアント

/// MusicBrainz の照会。`base` は `https://musicbrainz.org/ws/2/`（末尾 `/`。設定で差し替え可）
#[derive(Debug, Clone)]
pub struct MusicBrainzClient {
    base: String,
    http: reqwest::Client,
    /// 照会結果（鍵 → 結果と時刻）。同じディスクで 1 枚あたり 10 本前後の要求が繰り返し飛ぶのを止める
    cache: Arc<std::sync::Mutex<Vec<(CacheKey, DiscLookup, Instant)>>>,
    cache_ttl: Duration,
    /// 直前の要求が**返った**時刻。ロックを持ったまま待って送るので、並行する照会も直列に
    /// 間隔が空く。送信前ではなく応答後に打つのは、規約が数えるのがサーバ側の受信間隔だから
    /// （送信前だと、1 本目の接続確立ぶんだけサーバから見た間隔が縮む）
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
        Self::with_address_family(base, user_agent, min_interval, AddressFamily::Auto)
    }

    /// 接続に使う IP のバージョンを選べる版（`[musicbrainz].address_family`）
    pub fn with_address_family(
        base: impl Into<String>,
        user_agent: &str,
        min_interval: Duration,
        family: AddressFamily,
    ) -> Result<Self, LookupError> {
        let mut base = base.into();
        if !base.ends_with('/') {
            base.push('/');
        }
        Ok(Self {
            base,
            http: super::http_client_with(user_agent, family)?,
            cache: Arc::new(std::sync::Mutex::new(Vec::new())),
            cache_ttl: CACHE_TTL,
            last_request: Arc::new(tokio::sync::Mutex::new(None)),
            min_interval,
        })
    }

    /// 覚えておく期限を変える（テスト用。既定は [`CACHE_TTL`]）
    pub fn with_cache_ttl(mut self, ttl: Duration) -> Self {
        self.cache_ttl = ttl;
        self
    }

    /// 覚えている結果（期限内）
    fn cached(&self, key: &CacheKey) -> Option<DiscLookup> {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .iter()
            .find_map(|(k, v, at)| (k == key && at.elapsed() < self.cache_ttl).then(|| v.clone()))
    }

    /// 覚えているものを捨てる（`refresh`。引き直しが失敗しても古い結果は返さない）
    fn forget(&self, key: &CacheKey) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.retain(|(k, _, at)| k != key && at.elapsed() < self.cache_ttl);
    }

    fn remember(&self, key: CacheKey, value: &DiscLookup) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.retain(|(k, _, at)| k != &key && at.elapsed() < self.cache_ttl);
        cache.push((key, value.clone(), Instant::now()));
        if cache.len() > CACHE_CAPACITY {
            cache.remove(0);
        }
    }

    /// 間隔を空けて GET。503（負荷制限）と接続の失敗（TLS の失敗・idle なコネクションの再利用・
    /// 一過性の切断）は 1 度だけ張り直す。状態のある要求ではないので、同じ GET をもう一度出してよい
    async fn get(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<(reqwest::StatusCode, String), LookupError> {
        let url = format!("{}{}", self.base, path);
        for attempt in 0..2 {
            let sent = {
                let mut last = self.last_request.lock().await;
                if let Some(t) = *last {
                    let wait = self.min_interval.saturating_sub(t.elapsed());
                    if !wait.is_zero() {
                        tokio::time::sleep(wait).await;
                    }
                }
                let sent = self.http.get(&url).query(query).send().await;
                // 失敗（接続不能等）でも要求は出しているので、次の間隔はここから数える
                *last = Some(Instant::now());
                sent
            };
            let resp = match sent {
                Ok(r) => r,
                // 応答が返る前に落ちた（TLS の失敗・idle なコネクションの再利用・一過性の切断）。
                // 同じ GET をもう一度出す（状態は持たない）
                Err(e) if attempt == 0 && !e.is_timeout() => {
                    tracing::warn!(
                        url,
                        error = super::error_chain(&e),
                        "MusicBrainz への接続に失敗。張り直して再試行"
                    );
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
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

    /// TOC だけで引く（DiscID → 無ければ TOC の fuzzy）。[`Self::lookup`] の省略形
    pub async fn lookup_disc(&self, toc: &Toc) -> Result<DiscLookup, LookupError> {
        self.lookup(&DiscQuery {
            toc,
            isrcs: &[],
            mcn: None,
            release: None,
            catno: None,
            refresh: false,
            widen: false,
        })
        .await
    }

    /// ディスクの識別子から候補を集める。DiscID で当たれば exact（ISRC / バーコードは引かない）、
    /// 無ければ ISRC / バーコード、それでも出なければ TOC の近似（段階照会。D-64 追記 4）。
    /// 指定リリースはどの段でも足す。
    /// 個々の経路の「見つからない」は候補ゼロ（指定リリースだけ notes に理由）で、照会自体の失敗
    /// （届かない・壊れている・503）だけ Err
    pub async fn lookup(&self, q: &DiscQuery<'_>) -> Result<DiscLookup, LookupError> {
        // 同じ入力なら覚えている結果を返す（失敗は覚えないので、押し直せばまた引きにいく）
        // `refresh` は引く前に捨てる: 引き直しが失敗したのに古い結果が残ると、次の自動照会が
        // それを期限まで返してしまう
        let key = q.cache_key();
        if q.refresh {
            self.forget(&key);
        } else if let Some(hit) = self.cached(&key) {
            tracing::debug!(?key, "覚えている照会結果を返す");
            return Ok(hit);
        }
        let discid = q.toc.musicbrainz_disc_id();
        let audio_tracks = q.toc.audio_tracks().count();
        let mut groups: Vec<(MatchedBy, Vec<ReleaseCandidate>)> = Vec::new();
        let mut notes = Vec::new();
        // 取得済みのリリース（経路をまたいで 1 回しか取らない。中身は経路ごとに使い回す）
        let mut cache: std::collections::HashMap<String, Vec<ReleaseCandidate>> =
            std::collections::HashMap::new();
        // 残った候補の数。段を進めるかの判定に使う（「残った」はトラック数で絞ったあとで数える）。
        // `discid_hits` は段 1 の分だけ。指定リリースは段に関係なく足すので `hits` にしか入れない
        // （指定があっても ISRC / バーコードは引く。D-64 の「複数経路を束ねる」を壊さないため）
        let mut hits = 0usize;
        let mut discid_hits = 0usize;

        let path = format!("discid/{discid}");
        // cdstubs=no: 未登録 DiscID に CD stub（品質の低い匿名投稿）があると 200 で別の形が返り、
        // 404 → 次の段に進めない。候補にも入れない（D-64）
        let (status, body) = self
            .get(&path, &[("inc", INC), ("fmt", "json"), ("cdstubs", "no")])
            .await?;
        let exact = if status.is_success() {
            let c = parse_lookup(&body, &discid, audio_tracks)
                .map_err(|e| LookupError::Parse(e.to_string()))?;
            discid_hits = c.len();
            hits += c.len();
            groups.push((MatchedBy::Discid, c));
            // DiscID は登録されている。候補が 0 件でもここは真のままにする
            // （`offersDiscidSubmission` が登録を勧めてしまう）
            true
        } else if status == reqwest::StatusCode::NOT_FOUND {
            false
        } else {
            return Err(LookupError::Status(status.as_u16()));
        };

        // 指定リリースは段に関係なく常に足す（ユーザが名指ししたものなので最優先）
        if let Some(r) = q.release {
            match parse_release_ref(r) {
                None => notes.push(format!(
                    "リリースの指定を読めない: {r:?}（musicbrainz.org/release/<id> の URL か MBID）"
                )),
                Some(id) => match self.fetch_release(&id, &discid, audio_tracks).await? {
                    Ok(c) if c.is_empty() => {
                        cache.insert(id.clone(), Vec::new());
                        notes.push(format!(
                            "指定したリリース {id} に音声 {audio_tracks} トラックの medium が無い"
                        ));
                    }
                    Ok(c) => {
                        cache.insert(id.clone(), c.clone());
                        hits += c.len();
                        groups.push((MatchedBy::Release, c));
                    }
                    Err(status) => {
                        cache.insert(id.clone(), Vec::new());
                        notes.push(format!("指定したリリース {id} を取れない（HTTP {status}）"));
                    }
                },
            }
        }

        // 品番も段に関係なく常に引く（D-94）。DiscID で当たっても引く: 収録が同じ版どうしは DiscID が
        // 同じで、DiscID が別の版にしか登録されていないとき、手元の版は品番でしか見つからない。
        // 検索は部分一致もありうるので、カタログ番号が正規化して一致するものだけを候補にする
        if let Some(raw) = q.catno {
            match normalize_catno(raw) {
                None => notes.push(format!(
                    "品番を読めない: {raw:?}（英数字とハイフン・空白だけ）"
                )),
                Some(norm) => {
                    let query = format!("catno:\"{}\"", raw.trim());
                    let (status, body) = self
                        .get("release", &[("query", query.as_str()), ("fmt", "json")])
                        .await?;
                    if !status.is_success() {
                        return Err(LookupError::Status(status.as_u16()));
                    }
                    let ids = parse_release_search(&body)
                        .map_err(|e| LookupError::Parse(e.to_string()))?;
                    let mut c = Vec::new();
                    for id in ids.into_iter().take(CATNO_FETCH_LIMIT) {
                        c.extend(
                            self.fetch_cached(&mut cache, &id, &discid, audio_tracks)
                                .await?
                                .into_iter()
                                .filter(|x| candidate_has_catno(x, &norm)),
                        );
                    }
                    if c.is_empty() {
                        notes.push(format!(
                            "品番 {} で音声 {audio_tracks} トラックのリリースは MusicBrainz に見つからない",
                            raw.trim()
                        ));
                    }
                    hits += c.len();
                    groups.push((MatchedBy::Catno, c));
                }
            }
        }

        let mut stage = LookupStage::Discid;
        // DiscID で当たって候補も残ったときだけ打ち切る。200 でも候補 0 件なら次の段へ落とす
        // （登録済みの DiscID でも medium のトラック数が合わなければ候補にならない。そこで止めると
        //  行き止まりになる）
        if discid_hits == 0 {
            // 段 2: ディスクが持っている識別子（ISRC / バーコード）。DiscID が未登録でもここで当たる
            stage = LookupStage::Ids;
            // ISRC: 検索を 1 回（複数 ISRC を OR）→ 一致数の多いリリースから上限まで取得。
            // クエリに載せる値は英数字だけ（API が検証済みだが、ここでも Lucene の記号を通さない）
            let isrcs: Vec<&String> = q
                .isrcs
                .iter()
                .filter(|i| i.len() == 12 && i.chars().all(|c| c.is_ascii_alphanumeric()))
                .collect();
            if !isrcs.is_empty() {
                let query = isrcs
                    .iter()
                    .map(|i| format!("isrc:{i}"))
                    .collect::<Vec<_>>()
                    .join(" OR ");
                let (status, body) = self
                    .get("recording", &[("query", query.as_str()), ("fmt", "json")])
                    .await?;
                if !status.is_success() {
                    return Err(LookupError::Status(status.as_u16()));
                }
                let ids =
                    parse_recording_search(&body).map_err(|e| LookupError::Parse(e.to_string()))?;
                let mut c = Vec::new();
                for id in ids.into_iter().take(ISRC_FETCH_LIMIT) {
                    c.extend(
                        self.fetch_cached(&mut cache, &id, &discid, audio_tracks)
                            .await?,
                    );
                }
                hits += c.len();
                groups.push((MatchedBy::Isrc, c));
            }
            // バーコード（数字だけ）
            if let Some(mcn) = q
                .mcn
                .filter(|m| !m.is_empty() && m.chars().all(|c| c.is_ascii_digit()))
            {
                let query = format!("barcode:{mcn}");
                let (status, body) = self
                    .get("release", &[("query", query.as_str()), ("fmt", "json")])
                    .await?;
                if !status.is_success() {
                    return Err(LookupError::Status(status.as_u16()));
                }
                let ids =
                    parse_release_search(&body).map_err(|e| LookupError::Parse(e.to_string()))?;
                let mut c = Vec::new();
                for id in ids.into_iter().take(BARCODE_FETCH_LIMIT) {
                    c.extend(
                        self.fetch_cached(&mut cache, &id, &discid, audio_tracks)
                            .await?,
                    );
                }
                hits += c.len();
                groups.push((MatchedBy::Barcode, c));
            }
            // 段 3: TOC 近似は最後の手段。上の段で 1 件も残らなかったとき（か `widen`）だけ引く。
            // トラック長の違う別の盤が大量に出て、当たっている候補を埋めてしまうため（D-64 追記 4）
            if hits == 0 || q.widen {
                stage = LookupStage::Toc;
                let mb_toc = q.toc.musicbrainz_toc();
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
                if status.is_success() {
                    let c = parse_lookup(&body, &discid, audio_tracks)
                        .map_err(|e| LookupError::Parse(e.to_string()))?;
                    groups.push((MatchedBy::Toc, c));
                } else if status != reqwest::StatusCode::NOT_FOUND {
                    return Err(LookupError::Status(status.as_u16()));
                }
            }
        }

        let result = DiscLookup {
            discid,
            exact,
            stage,
            candidates: merge_candidates(groups),
            notes,
        };
        self.remember(key, &result);
        Ok(result)
    }

    /// リリースグループの版を並べる（D-93）。知らないグループ（404）は `Ok(None)`。
    /// 他の失敗（503 の再試行後を含む）は `Err`（503 は呼び出し側が musicbrainz_unavailable にする）
    pub async fn releases_in_group(
        &self,
        group_id: &str,
    ) -> Result<Option<GroupReleases>, LookupError> {
        let limit = GROUP_RELEASE_LIMIT.to_string();
        let (status, body) = self
            .get(
                "release",
                &[
                    ("release-group", group_id),
                    ("inc", "media labels"),
                    ("limit", &limit),
                    ("fmt", "json"),
                ],
            )
            .await?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(LookupError::Status(status.as_u16()));
        }
        parse_group_releases(&body)
            .map(Some)
            .map_err(|e| LookupError::Parse(e.to_string()))
    }

    /// リリースを 1 件取って候補に直す。404 等の「取れない」は `Ok(Err(status))`（経路ごとに
    /// 扱いが違う: 指定なら notes、検索由来なら黙って飛ばす）
    async fn fetch_release(
        &self,
        id: &str,
        discid: &str,
        audio_tracks: usize,
    ) -> Result<Result<Vec<ReleaseCandidate>, u16>, LookupError> {
        let (status, body) = self
            .get(&format!("release/{id}"), &[("inc", INC), ("fmt", "json")])
            .await?;
        if !status.is_success() {
            return Ok(Err(status.as_u16()));
        }
        let c = parse_release(&body, discid, audio_tracks)
            .map_err(|e| LookupError::Parse(e.to_string()))?;
        Ok(Ok(c))
    }

    /// 取得済みならその候補を、まだなら取って（取れなければ空。ログだけ）覚える。同じリリースが
    /// 複数の経路に出たとき、中身は 1 回の取得で経路ごとに使い回す（束ねるときに経路が足される）
    async fn fetch_cached(
        &self,
        cache: &mut std::collections::HashMap<String, Vec<ReleaseCandidate>>,
        id: &str,
        discid: &str,
        audio_tracks: usize,
    ) -> Result<Vec<ReleaseCandidate>, LookupError> {
        if let Some(c) = cache.get(id) {
            return Ok(c.clone());
        }
        let c = match self.fetch_release(id, discid, audio_tracks).await? {
            Ok(c) => c,
            Err(status) => {
                tracing::info!(id, status, "検索に出たリリースを取れない。飛ばす");
                Vec::new()
            }
        };
        cache.insert(id.to_owned(), c.clone());
        Ok(c)
    }
}

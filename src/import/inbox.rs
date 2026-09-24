//! Inbox 取り込み（SPEC §7.8、D-20 / D-68、P2-10）。
//!
//! ```text
//! scan_inbox   Inbox を歩き、音声ファイルのあるディレクトリを 1 件として inbox_items / inbox_files に
//!              写す。stat が変わったファイルだけタグを読み直す。消えたディレクトリの行は消す。
//!              approved の件でファイルが変わっていたら pending に戻す（再承認）
//! proposal     タグから下書き（InboxDraft）を作る。承認画面の初期値
//! InboxDraft   承認の入力。validate で不足（album / albumartist / title、番号）を弾く
//! place_item   approved の件を Library に配置して登録する（配置は cd::place と同じ規則）
//! ```
//!
//! 正は Inbox のファイルで、行はキャッシュ。承認の補正はファイルのタグに書いてから置く
//! （ファイルが正のまま再スキャンしても DB と一致する）

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::db::inbox::{self as dbinbox, FileRow, Item, ItemState};
use crate::db::{Db, DbError};
use crate::domain::relpath::{canonical_key, RelPath};
use crate::domain::tags::{read_audio_file, Codec};
use crate::fsroot::{FileKind, FsError, RootDir};

// ---------------------------------------------------------------- 下書き

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftTrack {
    /// Inbox 相対（件のファイルと 1:1）
    pub rel_path: String,
    pub disc_no: u32,
    pub track_no: u32,
    pub title: String,
    /// 空ならアルバムアーティスト。`keep_artists` が true なら表示用（ファイルの ARTIST の全値を
    /// `"; "` で結合したもの）で、配置は見ない
    #[serde(default)]
    pub artist: String,
    /// ファイルの ARTIST をそのまま保つ（配置で触れない。P4-4、D-70）。提案は多値なら true。None は
    /// この欄が無かった旧下書きで、旧規則（多値で `artist` が先頭値のままなら保つ）で解釈する
    #[serde(default)]
    pub keep_artists: Option<bool>,
    /// ファイルのタグの変更（D-86）。キー（大文字）→ 値の配列、`None` はそのタグを消す。書くのは
    /// ここにあるキーだけで、無いキーはファイルのまま。上の欄が扱うキー（[`COVERED_TAG_KEYS`]）と
    /// 同一性の判定に使うキー（[`is_locked_tag_key`]）は受け付けない
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, Option<Vec<String>>>,
    /// 埋め込み画像を差し替える（`<mime>:<sha256hex>`。`POST /api/artwork/upload` で置いた画像。D-86）。
    /// `None` ならファイルの画像のまま
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
}

/// 承認画面の固定の欄が扱うキー。`DraftTrack::tags` では受け付けない（二通りの直し方を作らない）
pub const COVERED_TAG_KEYS: &[&str] = &[
    "TITLE",
    "ARTIST",
    "ALBUM",
    "ALBUMARTIST",
    "DATE",
    "TRACKNUMBER",
    "DISCNUMBER",
    "DISCTOTAL",
    "PICTURE",
];

/// 曲・盤の同一性の判定に使うキー（D-86）。`SOURCE_URL` は YouTube の二重取り込みの判定（D-70）、
/// `MUSICBRAINZ_*` は CD の盤とリリースの識別（D-67 追記 3）。手で書き換えると判定が静かに壊れるので
/// 承認画面では直させない（リリースは MusicBrainz の引き直しで付け替える）
pub fn is_locked_tag_key(key: &str) -> bool {
    key == "SOURCE_URL" || key.starts_with("MUSICBRAINZ_")
}

/// 画像の形式（`POST /api/artwork/upload` が受け付けるもの）
const DRAFT_PICTURE_MIMES: &[&str] = &["image/jpeg", "image/png", "image/webp"];

/// 承認の入力（アルバム単位の補正とトラック単位の補正）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxDraft {
    /// 配置先の category（統制語彙の名前）。None なら `_Unsorted`
    #[serde(default)]
    pub category: Option<String>,
    pub albumartist: String,
    pub album: String,
    /// `YYYY[-MM[-DD]]`
    #[serde(default)]
    pub date: Option<String>,
    pub tracks: Vec<DraftTrack>,
    /// album gain を計算する album にする（D-74）。既定 off。追記先の album ならその属性を上書きする
    #[serde(default)]
    pub album_gain: bool,
    /// MusicBrainz のリリース（`MUSICBRAINZ_ALBUMID`）。提案はファイルのタグの最頻値、承認画面で候補を
    /// 選ぶと入る（P4-21）。配置でタグに書き、album の `mb_release_id` とリリースキー `mb:` になる。
    /// None なら触らない（ファイルのタグのまま）
    #[serde(default)]
    pub release_id: Option<String>,
    /// 同じくリリースグループ（`MUSICBRAINZ_RELEASEGROUPID`）
    #[serde(default)]
    pub release_group_id: Option<String>,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum DraftError {
    #[error("アルバム名が空")]
    EmptyAlbum,
    #[error("アルバムアーティストが空")]
    EmptyAlbumArtist,
    #[error("タイトルが空: {rel_path}")]
    EmptyTitle { rel_path: String },
    #[error("トラック番号 / ディスク番号は 1 以上: {rel_path}")]
    BadNumber { rel_path: String },
    #[error("番号が重複: disc {disc_no} track {track_no}")]
    DuplicateNumber { disc_no: u32, track_no: u32 },
    #[error("件に無いファイル: {0}")]
    UnknownFile(String),
    #[error("下書きに無いファイル: {0}")]
    MissingFile(String),
    #[error("下書きに同じファイルが 2 回: {0}")]
    DuplicateFile(String),
    #[error("日付の形が不正: {0}（YYYY / YYYY-MM / YYYY-MM-DD）")]
    BadDate(String),
    #[error("category が空")]
    EmptyCategory,
    #[error("MusicBrainz のリリース ID の形が不正: {0}")]
    BadReleaseId(String),
    #[error("MusicBrainz のリリースグループ ID の形が不正: {0}")]
    BadReleaseGroupId(String),
    #[error("タグのキーが不正: {key}（{rel_path}。空・小文字・= や制御文字は使えない）")]
    BadTagKey { rel_path: String, key: String },
    #[error("{key} は上の欄で直す（{rel_path}）")]
    CoveredTagKey { rel_path: String, key: String },
    #[error("{key} は曲・盤の識別に使うので直せない（{rel_path}）")]
    LockedTagKey { rel_path: String, key: String },
    #[error("画像の指定が不正: {value}（{rel_path}）")]
    BadPicture { rel_path: String, value: String },
}

/// タグのキーの形（`tagops` の `normalize_key` と同じ文字の規則。下書きは正規化済みのキーだけを受ける）
fn is_valid_tag_key(key: &str) -> bool {
    !key.is_empty()
        && key == key.trim()
        && key == key.to_uppercase()
        && key.chars().all(|c| (' '..='}').contains(&c) && c != '=')
}

/// MusicBrainz の MBID（UUID `8-4-4-4-12`）か。大文字も通す（foobar2000 などが書いた既存のタグ。値は
/// スキャナが album の `mb_release_id` に入れる値と揃えるため正規化しない。`/api/cd/lookup` の検証と同じ）
fn is_mbid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// `YYYY[-MM[-DD]]` か
fn is_valid_date(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.is_empty() || parts.len() > 3 {
        return false;
    }
    let digits = |p: &str, n: usize| p.len() == n && p.bytes().all(|b| b.is_ascii_digit());
    if !digits(parts[0], 4) {
        return false;
    }
    if let Some(m) = parts.get(1) {
        if !digits(m, 2) || !(1..=12).contains(&m.parse::<u32>().unwrap_or(0)) {
            return false;
        }
    }
    if let Some(d) = parts.get(2) {
        if !digits(d, 2) || !(1..=31).contains(&d.parse::<u32>().unwrap_or(0)) {
            return false;
        }
    }
    true
}

impl InboxDraft {
    /// 全部の問題（承認画面の警告に使う）。`files` は件のファイルの rel_path
    pub fn problems(&self, files: &[String]) -> Vec<DraftError> {
        let mut out = Vec::new();
        if self.album.trim().is_empty() {
            out.push(DraftError::EmptyAlbum);
        }
        if self.albumartist.trim().is_empty() {
            out.push(DraftError::EmptyAlbumArtist);
        }
        if let Some(c) = &self.category {
            if c.trim().is_empty() {
                out.push(DraftError::EmptyCategory);
            }
        }
        if let Some(d) = &self.date {
            if !is_valid_date(d.trim()) {
                out.push(DraftError::BadDate(d.clone()));
            }
        }
        let known: HashSet<String> = files.iter().map(|f| canonical_key(f)).collect();
        let mut seen_files: HashSet<String> = HashSet::new();
        let mut numbers: HashSet<(u32, u32)> = HashSet::new();
        for t in &self.tracks {
            let key = canonical_key(&t.rel_path);
            if !known.contains(&key) {
                out.push(DraftError::UnknownFile(t.rel_path.clone()));
            }
            if !seen_files.insert(key) {
                // 同じ音声を複数の行として配置させない
                out.push(DraftError::DuplicateFile(t.rel_path.clone()));
            }
            if t.title.trim().is_empty() {
                out.push(DraftError::EmptyTitle {
                    rel_path: t.rel_path.clone(),
                });
            }
            for key in t.tags.keys() {
                let e = (t.rel_path.clone(), key.clone());
                if !is_valid_tag_key(key) {
                    out.push(DraftError::BadTagKey {
                        rel_path: e.0,
                        key: e.1,
                    });
                } else if COVERED_TAG_KEYS.contains(&key.as_str()) {
                    out.push(DraftError::CoveredTagKey {
                        rel_path: e.0,
                        key: e.1,
                    });
                } else if is_locked_tag_key(key) {
                    out.push(DraftError::LockedTagKey {
                        rel_path: e.0,
                        key: e.1,
                    });
                }
            }
            if let Some(p) = &t.picture {
                let ok = crate::edit::picture::parse_picture_value(p)
                    .is_some_and(|(mime, _)| DRAFT_PICTURE_MIMES.contains(&mime));
                if !ok {
                    out.push(DraftError::BadPicture {
                        rel_path: t.rel_path.clone(),
                        value: p.clone(),
                    });
                }
            }
            if t.disc_no == 0 || t.track_no == 0 {
                out.push(DraftError::BadNumber {
                    rel_path: t.rel_path.clone(),
                });
            } else if !numbers.insert((t.disc_no, t.track_no)) {
                out.push(DraftError::DuplicateNumber {
                    disc_no: t.disc_no,
                    track_no: t.track_no,
                });
            }
        }
        for f in files {
            if !seen_files.contains(&canonical_key(f)) {
                out.push(DraftError::MissingFile(f.clone()));
            }
        }
        if let Some(id) = self.release_id.as_deref().filter(|id| !is_mbid(id)) {
            out.push(DraftError::BadReleaseId(id.to_owned()));
        }
        if let Some(id) = self.release_group_id.as_deref().filter(|id| !is_mbid(id)) {
            out.push(DraftError::BadReleaseGroupId(id.to_owned()));
        }
        out
    }

    /// 最初の問題（API の 400）
    pub fn validate(&self, files: &[String]) -> Result<(), DraftError> {
        match self.problems(files).into_iter().next() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// 枚数（disc_no の最大）
    pub fn disc_count(&self) -> u32 {
        self.tracks
            .iter()
            .map(|t| t.disc_no)
            .max()
            .unwrap_or(1)
            .max(1)
    }

    /// トラック `index` のアーティスト（空ならアルバムアーティスト）
    pub fn track_artist(&self, index: usize) -> &str {
        match self.tracks.get(index).map(|t| t.artist.trim()) {
            Some(a) if !a.is_empty() => a,
            _ => self.albumartist.trim(),
        }
    }

    /// 発売年
    pub fn year(&self) -> Option<String> {
        let d = self.date.as_deref()?.trim();
        let y: String = d.chars().take(4).collect();
        (y.len() == 4 && y.chars().all(|c| c.is_ascii_digit())).then_some(y)
    }
}

/// 値の最頻値（同数なら先に出たもの。空は数えない）
fn mode<'a>(values: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut order: Vec<(String, usize)> = Vec::new();
    for v in values {
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        match order.iter_mut().find(|(k, _)| k == v) {
            Some((_, n)) => *n += 1,
            None => order.push((v.to_owned(), 1)),
        }
    }
    let best = order.iter().map(|(_, n)| *n).max()?;
    order.into_iter().find(|(_, n)| *n == best).map(|(k, _)| k)
}

fn tag<'a>(f: &'a FileRow, key: &str) -> Option<&'a str> {
    f.tags
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// 承認画面が ARTIST の全値を見せるときの区切り（foobar2000 等の多値の慣習。Library の一覧が使う
/// `", "` とは区別する。D-70）
pub const ARTIST_JOIN: &str = "; ";

/// `tags` の ARTIST の全値（TagSet の値そのまま。出現順。多値の判定・表示・パス用の結合はすべて
/// これを使い、Library の `artist_display` と同じ値の並びにする）
pub fn artist_values(tags: &[(String, String)]) -> Vec<&str> {
    tags.iter()
        .filter(|(k, _)| k == "ARTIST")
        .map(|(_, v)| v.as_str())
        .collect()
}

/// 提案の `artist`: ARTIST の全値を `"; "` で結合（1 値なら同じ、無ければ空）
pub fn joined_artist(tags: &[(String, String)]) -> String {
    artist_values(tags).join(ARTIST_JOIN)
}

/// 下書きのトラックがファイルの ARTIST を**そのまま**保つ（配置で触れない）か。`keep_artists` が
/// Some なら現在の個数に関係なくその値（true は承認後にファイルが 1 値に変わっていても安全側）。
/// 旧下書き（欄が無い）は旧規則をそのまま再現する: ファイルの ARTIST が複数で、先頭が下書きの実効
/// アーティスト `effective`（`InboxDraft::track_artist`。空ならアルバムアーティスト）と一致するとき（D-70）
pub fn keeps_artists(
    keep_artists: Option<bool>,
    effective: &str,
    tags: &[(String, String)],
) -> bool {
    match keep_artists {
        Some(keep) => keep,
        None => {
            let values = artist_values(tags);
            values.len() > 1 && values.first() == Some(&effective)
        }
    }
}

// ---------------------------------------------------------------- 埋め込み画像（P4-4、D-70）

/// `tags` の `PICTURE`（`"<mime>:<sha256 hex>"`）を `(mime, hash)` にほどく（出現順）
pub fn picture_hashes(tags: &[(String, String)]) -> Vec<(&str, &str)> {
    tags.iter()
        .filter(|(k, _)| k == "PICTURE")
        .filter_map(|(_, v)| v.split_once(':'))
        .collect()
}

/// `hash`（sha256 hex、小文字）の埋め込み画像を件のファイルから探す（`GET /api/inbox/:id/artwork/:hash`）。
/// `PICTURE` にその hash を持つファイルを走査順に開き（symlink / 境界外 / 消失は次の候補へ）、
/// 内容の sha256 が一致する画像を `(mime, bytes)` で返す。MIME はタグの値ではなく内容の sniff で、
/// sniff できない画像は一致しなかったものとして次へ。どれも一致しなければ None（呼び出し側は 404）。
/// その他の I/O 失敗は Err
pub fn embedded_picture(
    inbox: &RootDir,
    files: &[FileRow],
    hash: &str,
) -> Result<Option<(&'static str, Vec<u8>)>, InboxError> {
    for f in files
        .iter()
        .filter(|f| picture_hashes(&f.tags).iter().any(|(_, h)| *h == hash))
    {
        let Ok(rel) = RelPath::parse(&f.rel_path) else {
            continue;
        };
        let file = match inbox.open_file(&rel) {
            Ok(f) => f,
            Err(FsError::NotFound | FsError::Symlink | FsError::Escaped) => continue,
            Err(FsError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let ext = rel
            .file_name()
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase());
        // 形式として読めない（書き換わった・途中で切れている等）は次の候補へ。読み取りの I/O 失敗
        // （権限・デバイス。lofty の Parse に包まれたものも含む）は 500 に伝える
        let pictures =
            match crate::domain::tags::read_audio_file_with_pictures(file, ext.as_deref()) {
                Ok((_, pictures)) => pictures,
                Err(e) => match e.io_kind() {
                    Some(std::io::ErrorKind::NotFound | std::io::ErrorKind::UnexpectedEof)
                    | None => continue,
                    Some(_) => return Err(std::io::Error::other(e).into()),
                },
            };
        let found = pictures.into_iter().find_map(|pic| {
            let digest = crate::media::artwork::ArtworkStore::hash_of(pic.data());
            if crate::media::artwork::ArtworkStore::hex(&digest) != hash {
                return None;
            }
            let info = crate::media::artwork::sniff(pic.data())?;
            Some((info.mime, pic.data().to_vec()))
        });
        if found.is_some() {
            return Ok(found);
        }
    }
    Ok(None)
}

fn leading_int(s: &str) -> Option<u32> {
    let digits: String = s
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// タグから下書きを作る。albumartist / album / date は最頻値（ALBUMARTIST が無ければ ARTIST）、
/// category は GENRE の最頻値のうち `genre_map`（canonical key → category id）に当たるもの、
/// トラックは TRACKNUMBER（無ければ 0 = 不足）/ DISCNUMBER（無ければ 1）/ TITLE / ARTIST（全値を `"; "` で結合）
pub fn proposal(
    files: &[FileRow],
    categories: &[(i64, String)],
    genre_map: &[(String, i64)],
) -> InboxDraft {
    let albumartist = mode(files.iter().filter_map(|f| tag(f, "ALBUMARTIST")))
        .or_else(|| mode(files.iter().filter_map(|f| tag(f, "ARTIST"))))
        .unwrap_or_default();
    let album = mode(files.iter().filter_map(|f| tag(f, "ALBUM"))).unwrap_or_default();
    let date = mode(files.iter().filter_map(|f| tag(f, "DATE")));
    let genre = mode(
        files
            .iter()
            .flat_map(|f| f.tags.iter().filter(|(k, _)| k == "GENRE"))
            .map(|(_, v)| v.as_str())
            .filter(|g| {
                let key = canonical_key(g);
                genre_map.iter().any(|(gk, _)| *gk == key)
            }),
    );
    let category = genre.and_then(|g| {
        let key = canonical_key(&g);
        let id = genre_map.iter().find(|(gk, _)| *gk == key)?.1;
        categories
            .iter()
            .find(|(cid, _)| *cid == id)
            .map(|(_, name)| name.clone())
    });
    let tracks = files
        .iter()
        .map(|f| DraftTrack {
            rel_path: f.rel_path.clone(),
            disc_no: tag(f, "DISCNUMBER").and_then(leading_int).unwrap_or(1),
            track_no: tag(f, "TRACKNUMBER").and_then(leading_int).unwrap_or(0),
            title: tag(f, "TITLE").unwrap_or("").trim().to_owned(),
            artist: joined_artist(&f.tags),
            keep_artists: Some(artist_values(&f.tags).len() > 1),
            tags: Default::default(),
            picture: None,
        })
        .collect();
    InboxDraft {
        category,
        albumartist,
        album,
        date,
        tracks,
        album_gain: false,
        release_id: mode(files.iter().filter_map(|f| tag(f, "MUSICBRAINZ_ALBUMID"))),
        release_group_id: mode(
            files
                .iter()
                .filter_map(|f| tag(f, "MUSICBRAINZ_RELEASEGROUPID")),
        ),
    }
}

/// `track_no` が 0（TRACKNUMBER 無し）のトラックに、`start` から順に番号を振る（D-70）。
/// ファイル名順（`rel_path` の昇順。ダウンローダの命名は公開日順）に振り、下書きで既に使われて
/// いる番号は飛ばす。トラックの並びは変えない。disc は下書きの値のまま
pub fn number_missing(draft: &mut InboxDraft, start: u32) {
    let mut used: HashSet<(u32, u32)> = draft
        .tracks
        .iter()
        .filter(|t| t.track_no != 0)
        .map(|t| (t.disc_no, t.track_no))
        .collect();
    let mut order: Vec<usize> = (0..draft.tracks.len())
        .filter(|&i| draft.tracks[i].track_no == 0)
        .collect();
    order.sort_by(|&a, &b| draft.tracks[a].rel_path.cmp(&draft.tracks[b].rel_path));
    let mut next = start.max(1);
    for i in order {
        let disc = draft.tracks[i].disc_no;
        while used.contains(&(disc, next)) {
            next += 1;
        }
        draft.tracks[i].track_no = next;
        used.insert((disc, next));
        next += 1;
    }
}

/// 承認後にファイルが増えて `pending` に戻った件の提案（D-70）: アルバム単位の補正と、既知の
/// ファイルのトラックは保存した下書きから、`proposed` にだけあるファイルは提案から。保存した
/// 下書きにしか無いファイル（消えたもの）は落とす。並びは `proposed`（件のファイルの順）
pub fn merge_saved(saved: &InboxDraft, proposed: &InboxDraft) -> InboxDraft {
    let tracks = proposed
        .tracks
        .iter()
        .map(|p| {
            let key = canonical_key(&p.rel_path);
            saved
                .tracks
                .iter()
                .find(|s| canonical_key(&s.rel_path) == key)
                .cloned()
                .unwrap_or_else(|| p.clone())
        })
        .collect();
    InboxDraft {
        category: saved.category.clone(),
        albumartist: saved.albumartist.clone(),
        album: saved.album.clone(),
        date: saved.date.clone(),
        tracks,
        album_gain: saved.album_gain,
        // 旧下書き（欄が無い）は提案（ファイルのタグ）の値
        release_id: saved
            .release_id
            .clone()
            .or_else(|| proposed.release_id.clone()),
        release_group_id: saved
            .release_group_id
            .clone()
            .or_else(|| proposed.release_group_id.clone()),
    }
}

/// `GET /api/inbox` が返す提案一式
#[derive(Debug, Clone)]
pub struct Proposed {
    pub draft: InboxDraft,
    /// 追記先の既存 album（D-70）
    pub destination: Option<Destination>,
    /// `files` と同じ順。サイドカーの項（無ければ None）
    pub sources: Vec<Option<crate::import::sidecar::FileEntry>>,
    /// 下書きのトラックと同じ順。追記先の album にある同名の行（P4-19）
    pub same_titles: Vec<Vec<SameTitle>>,
    pub warnings: Vec<String>,
    /// CD の件（サイドカーに `rip`）の照会の材料。承認画面が MusicBrainz を引き直すのに使う（P4-21）
    pub lookup: Option<RipLookup>,
}

/// 承認画面から `POST /api/cd/lookup` を引き直す材料（P4-21）。CD 画面の照会と同じ入力
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RipLookup {
    /// CTDB 形式の TOC
    pub toc: String,
    /// ドライブが読んだ ISRC（音声トラック順。読めなかったトラックは null。旧サイドカーは空）
    pub isrcs: Vec<Option<String>>,
    pub mcn: Option<String>,
}

/// タイトルの照合鍵（P4-19）。**NFKD + casefold**（パスの `canonical_key` は NFD だが、タイトルは
/// ファイル名と違って全角・半角の揺れ（`ＭＡＤ` と `MAD`）を同じものとして扱いたい）に加えて、空白
/// （全角空白を含む）を 1 つに畳み前後を落とす。**注記は落とさない**（`(Cover)` / `【… Live ver.】` は
/// 正当な別曲。同名とみなすと毎回警告が出て意味がなくなる）
pub fn title_key(title: &str) -> String {
    use unicode_normalization::UnicodeNormalization as _;
    let folded = caseless::default_case_fold_str(&title.nfkd().collect::<String>());
    let mut out = String::with_capacity(folded.len());
    for part in folded.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(part);
    }
    out.nfkd().collect()
}

#[cfg(test)]
mod title_key_tests {
    use super::title_key;

    #[test]
    fn folds_case_width_and_whitespace_but_keeps_annotations() {
        assert_eq!(title_key("New"), title_key("ｎｅｗ"));
        assert_eq!(title_key(" New\u{3000}Song "), title_key("new song"));
        assert_eq!(
            title_key("ハロー"),
            title_key("﻿ﾊﾛｰ".trim_start_matches('\u{feff}'))
        );
        assert_ne!(title_key("New"), title_key("New (Cover)"));
        assert_ne!(title_key("New"), title_key("New 【Live ver.】"));
        assert_ne!(title_key("New"), title_key("News"));
    }
}

/// 追記先の album に同じ [`title_key`] を持つ active なトラックがあるか（P4-19。承認画面の警告）
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SameTitle {
    pub track_id: i64,
    pub rel_path: String,
    pub duration_ms: Option<i64>,
}

/// 照合が通らず、読み取りでずれ（paranoia が検出・補正したジッター）が起きた吸い出しの警告。照合が
/// 通っていれば（どちらかの手法で verified）結果は DB と一致したので出さない。トラック番号は TOC の
/// 音声トラックの番号（先頭がデータトラックの盤でも実番号）
fn slip_warning(entry: &crate::import::sidecar::RipEntry) -> Option<String> {
    use crate::cd::verify::Outcome;
    let report = &entry.report;
    let numbers: Vec<u8> = crate::cd::toc::Toc::parse(&entry.toc)
        .map(|t| t.audio_tracks().map(|a| a.number).collect())
        .unwrap_or_default();
    let verified = [&report.ctdb, &report.accuraterip]
        .into_iter()
        .flatten()
        .any(|m| m.outcome == Outcome::Verified);
    let parts: Vec<String> = report
        .reads
        .iter()
        .enumerate()
        .filter(|(_, r)| r.slips > 0)
        .map(|(i, r)| {
            let n = numbers.get(i).map_or(i + 1, |&n| usize::from(n));
            format!("トラック {n}: {} 回", r.slips)
        })
        .collect();
    (!verified && !parts.is_empty()).then(|| {
        format!(
            "吸い出しで読み取り位置のずれ（ドライブのジッター）を補正した箇所がある（{}）。照合が通らないのはこのためかもしれない（別のドライブで吸い直すと確かめられる）",
            parts.join("、")
        )
    })
}

/// 件の提案（D-68 / D-70）: タグからの下書き → サイドカーの category（語彙にあるとき）→ `pending` で
/// 保存した下書きがあれば merge → 追記先の album を引き（配置済みの件は引かない）→ TRACKNUMBER の無いトラックを採番。
/// サイドカーが壊れていれば無いものとして扱い、警告に載せる
pub fn propose(
    conn: &rusqlite::Connection,
    inbox: &RootDir,
    layout: &LayoutConfig,
    item: &Item,
    files: &[FileRow],
    categories: &[(i64, String)],
    genre_map: &[(String, i64)],
) -> Result<Proposed, InboxError> {
    let mut warnings = Vec::new();
    let mut draft = proposal(files, categories, genre_map);
    let sidecar = if item.rel_dir.is_empty() {
        None
    } else {
        match RelPath::parse(&item.rel_dir) {
            Ok(dir) => match Sidecar::read(inbox, &dir) {
                Ok(s) => s,
                Err(e) => {
                    warnings.push(e.to_string());
                    None
                }
            },
            Err(_) => None,
        }
    };
    if let Some(name) = sidecar.as_ref().and_then(|s| s.category.as_deref()) {
        let key = canonical_key(name);
        match categories.iter().find(|(_, n)| canonical_key(n) == key) {
            Some((_, n)) => draft.category = Some(n.clone()),
            None => warnings.push(format!("サイドカーの category が語彙に無い: {name}")),
        }
    }
    // CD の吸い出しはアルバムとして通して聴く単位なので album gain on で提案する（D-74）。
    // 保存した下書きがあればそちらが勝つ（merge_saved）
    if sidecar.as_ref().is_some_and(|s| s.rip.is_some()) {
        draft.album_gain = true;
    }
    if item.state == ItemState::Pending {
        if let Some(saved) = item
            .draft
            .as_ref()
            .and_then(|v| serde_json::from_value::<InboxDraft>(v.clone()).ok())
        {
            draft = merge_saved(&saved, &draft);
        }
    }
    // 配置済みの件は追記先を引かない。引くと自分が置いた album に当たり、自分のトラックを同名と数え、
    // 「既存の album に追加」と見せてしまう（行く先は placed_album_id が示す）
    let destination = if item.state == ItemState::Placed {
        None
    } else {
        destination(conn, layout, &draft, files)?
    };
    let start = destination
        .as_ref()
        .and_then(|d| u32::try_from(d.max_track_no).ok())
        .unwrap_or(0)
        + 1;
    number_missing(&mut draft, start);
    let sources = files
        .iter()
        .map(|f| {
            let name = f.rel_path.rsplit('/').next().unwrap_or(&f.rel_path);
            sidecar.as_ref().and_then(|s| s.files.get(name).cloned())
        })
        .collect();
    // 同名の警告（P4-19）: 追記先の album に同じタイトルの active な行があれば、そのトラックに出す。
    // 追記先が無い（新しいアルバム）なら衝突しようがないので空
    let same_titles = match destination.as_ref() {
        Some(d) => draft
            .tracks
            .iter()
            .map(|t| dbinbox::same_title_in_album(conn, d.album_id, &title_key(&t.title)))
            .collect::<Result<Vec<_>, _>>()?,
        None => draft.tracks.iter().map(|_| Vec::new()).collect(),
    };
    warnings.extend(self::warnings(files, &draft));
    // 吸い出しの記録が件と合わなければ配置で止まる（D-67 追記）。承認の前に見せる
    if let Some(entry) = sidecar.as_ref().and_then(|s| s.rip.as_ref()) {
        if let Err(r) = bind_rip(entry, &draft) {
            warnings.push(format!(
                "吸い出しの記録と件が合わない（このままでは配置できない）: {r}"
            ));
        }
        if let Some(w) = slip_warning(entry) {
            warnings.push(w);
        }
    }
    let lookup = sidecar
        .as_ref()
        .and_then(|s| s.rip.as_ref())
        .map(|r| RipLookup {
            toc: r.toc.clone(),
            isrcs: r.isrcs.clone(),
            mcn: r.mcn.clone(),
        });
    Ok(Proposed {
        draft,
        destination,
        sources,
        same_titles,
        warnings,
        lookup,
    })
}

/// 下書きの問題を人間向けの文字列で
pub fn warnings(files: &[FileRow], draft: &InboxDraft) -> Vec<String> {
    let names: Vec<String> = files.iter().map(|f| f.rel_path.clone()).collect();
    draft
        .problems(&names)
        .into_iter()
        .map(|e| e.to_string())
        .collect()
}

// ---------------------------------------------------------------- 走査

#[derive(Debug, thiserror::Error)]
pub enum InboxError {
    #[error("下書きが不正: {0}")]
    Draft(#[from] DraftError),
    #[error("配置先が衝突: {0}")]
    Conflict(String),
    #[error("Inbox 側が変わった: {0}")]
    Changed(String),
    #[error(transparent)]
    Placement(#[from] crate::import::placement::PlacementError),
    #[error("タグを書けない: {0}")]
    Tag(#[from] crate::domain::tags::TagWriteError),
    #[error(transparent)]
    Fs(#[from] FsError),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("キャンセルされた")]
    Cancelled,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanOutcome {
    pub items_seen: usize,
    pub items_new: usize,
    pub items_removed: usize,
    pub files_read: usize,
}

/// `placed` の行を残す時間
pub const PLACED_RETENTION_SECS: i64 = 86_400;

/// 破棄待ちの間にファイルが変わって破棄を解いたときに `error` へ残す理由（D-90）
pub const DISCARD_CANCELLED_BY_CHANGE: &str =
    "破棄待ちの間にファイルが変わったので削除を取り消した（確かめてからもう一度削除する）";

/// 走査で見た 1 ファイル
struct Seen {
    rel: RelPath,
    st: crate::fsroot::Stat,
}

/// Inbox の指紋（P4-18）。音声ファイルの集合（パス・inode・size・mtime・ctime）から決まり、非音声と
/// 空ディレクトリは効かない（走査が拾う物と同じ = [`walk`] の結果）。周期の監視はこれが前回投入時と
/// 違うときだけ inbox ジョブを投入する（変化が無い毎分のジョブ行で一覧を埋めない）。プロセス内でだけ
/// 比較する値で、永続化しない
pub fn fingerprint(root: &RootDir) -> Result<u64, FsError> {
    use std::hash::{Hash as _, Hasher as _};
    let groups = walk(root)?;
    let mut entries: Vec<(String, u64, u64, i64, i64)> = groups
        .values()
        .flat_map(|(_, seen)| {
            seen.iter().map(|s| {
                (
                    s.rel.key(),
                    s.st.inode,
                    s.st.size,
                    s.st.mtime_ns,
                    s.st.ctime_ns,
                )
            })
        })
        .collect();
    entries.sort();
    let mut h = std::hash::DefaultHasher::new();
    entries.hash(&mut h);
    Ok(h.finish())
}

/// Inbox を歩いて音声ファイルを dir_key ごとにまとめる
fn walk(root: &RootDir) -> Result<HashMap<String, (String, Vec<Seen>)>, FsError> {
    let mut groups: HashMap<String, (String, Vec<Seen>)> = HashMap::new();
    let mut stack: Vec<Option<RelPath>> = vec![None];
    while let Some(dir) = stack.pop() {
        let entries = match root.read_dir(dir.as_ref()) {
            Ok(e) => e,
            Err(FsError::NotFound) => continue,
            Err(e) => return Err(e),
        };
        for e in entries {
            let Some(name) = e.name.to_str() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let child = match &dir {
                Some(d) => d.join(name),
                None => RelPath::parse(name),
            };
            let Ok(child) = child else {
                continue;
            };
            match e.kind {
                FileKind::Dir => stack.push(Some(child)),
                FileKind::File => {
                    let is_audio = name
                        .rsplit_once('.')
                        .and_then(|(_, ext)| Codec::from_extension(ext))
                        .is_some();
                    if !is_audio {
                        continue;
                    }
                    let st = match root.stat(&child) {
                        Ok(st) if st.kind == FileKind::File => st,
                        Ok(_) | Err(FsError::NotFound) => continue,
                        Err(e) => return Err(e),
                    };
                    let (dir_str, dir_key) = match &dir {
                        Some(d) => (d.as_str().to_owned(), d.key()),
                        None => (String::new(), String::new()),
                    };
                    groups
                        .entry(dir_key)
                        .or_insert_with(|| (dir_str, Vec::new()))
                        .1
                        .push(Seen { rel: child, st });
                }
                FileKind::Symlink | FileKind::Other => {}
            }
        }
    }
    Ok(groups)
}

fn stat_matches(row: &FileRow, st: &crate::fsroot::Stat) -> bool {
    row.inode == st.inode as i64
        && row.size == st.size as i64
        && row.mtime_ns == st.mtime_ns
        && row.ctime_ns == st.ctime_ns
}

fn read_row(root: &RootDir, seen: &Seen) -> Result<FileRow, FsError> {
    let ext = seen.rel.file_name().rsplit_once('.').map(|(_, e)| e);
    let file = root.open_file(&seen.rel)?;
    let (af, pictures) = crate::domain::tags::read_audio_file_with_pictures(file, ext)
        .map_err(|e| FsError::Io(std::io::Error::other(e.to_string())))?;
    let mut tags = af.tags.items().to_vec();
    // 代表画像（Library の pick_embedded と同じ: front cover 優先、無ければ先頭）を PICTURE の
    // 先頭に置く。画面は先頭を代表にする（P4-4、D-70）
    if let Some(pick) = crate::media::artwork::pick_embedded(&pictures) {
        let hash = crate::media::artwork::ArtworkStore::hex(
            &crate::media::artwork::ArtworkStore::hash_of(pick.data()),
        );
        let first = tags.iter().position(|(k, _)| k == "PICTURE");
        let at = tags
            .iter()
            .position(|(k, v)| k == "PICTURE" && v.split_once(':').map(|(_, h)| h) == Some(&hash));
        if let (Some(first), Some(at)) = (first, at) {
            let picked = tags.remove(at);
            tags.insert(first, picked);
        }
    }
    Ok(FileRow {
        rel_path: seen.rel.as_str().to_owned(),
        inode: seen.st.inode as i64,
        size: seen.st.size as i64,
        mtime_ns: seen.st.mtime_ns,
        ctime_ns: seen.st.ctime_ns,
        codec: af.codec.as_str().to_owned(),
        lossless: af.lossless,
        sample_rate: af.sample_rate,
        bit_depth: af.bit_depth,
        channels: af.channels,
        duration_ms: af.duration_ms,
        tags,
    })
}

/// 1 件の走査結果
struct Synced {
    dir: String,
    dir_key: String,
    /// 変わったときだけ Some（入れ替える全ファイル）
    files: Option<Vec<FileRow>>,
}

/// Inbox を走査して `inbox_items` / `inbox_files` を同期する
pub async fn scan_inbox(
    db: &Db,
    inbox: &Arc<RootDir>,
    now: i64,
) -> Result<ScanOutcome, InboxError> {
    // 1. 既存の行
    let existing: Vec<(Item, Vec<FileRow>)> = db
        .read(|c| {
            let items = dbinbox::list(c)?;
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                let files = dbinbox::files(c, it.id)?;
                out.push((it, files));
            }
            Ok(out)
        })
        .await?;
    let by_key: HashMap<String, (Item, HashMap<String, FileRow>)> = existing
        .into_iter()
        .map(|(it, files)| {
            let key = canonical_key(&it.rel_dir);
            let files = files
                .into_iter()
                .map(|f| (canonical_key(&f.rel_path), f))
                .collect();
            (key, (it, files))
        })
        .collect();
    // 2. 歩いて、変わったファイルだけ読む（blocking）
    let (synced, files_read) = {
        let root = Arc::clone(inbox);
        let by_key = Arc::new(by_key);
        let by_key2 = Arc::clone(&by_key);
        let res = tokio::task::spawn_blocking(move || -> Result<(Vec<Synced>, usize), InboxError> {
            let groups = walk(&root)?;
            let mut out = Vec::with_capacity(groups.len());
            let mut files_read = 0;
            let mut keys: Vec<&String> = groups.keys().collect();
            keys.sort();
            for key in keys {
                let (dir, seen) = &groups[key];
                let prev = by_key2.get(key).map(|(_, f)| f);
                let mut changed = prev.is_none_or(|p| p.len() != seen.len());
                let mut rows = Vec::with_capacity(seen.len());
                for s in seen {
                    let rk = s.rel.key();
                    match prev.and_then(|p| p.get(&rk)) {
                        Some(row) if stat_matches(row, &s.st) => rows.push(row.clone()),
                        _ => {
                            changed = true;
                            match read_row(&root, s) {
                                Ok(row) => {
                                    files_read += 1;
                                    rows.push(row);
                                }
                                Err(e) => {
                                    tracing::warn!(path = %s.rel, error = %e, "Inbox のファイルを読めない。今回は飛ばす");
                                }
                            }
                        }
                    }
                }
                if rows.is_empty() {
                    continue;
                }
                rows.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
                out.push(Synced {
                    dir: dir.clone(),
                    dir_key: key.clone(),
                    files: changed.then_some(rows),
                });
            }
            Ok((out, files_read))
        })
        .await
        .map_err(|e| std::io::Error::other(format!("Inbox の走査タスクが異常終了: {e}")))??;
        drop(by_key);
        res
    };
    // 3. DB に写す（1 トランザクション）
    let outcome = db
        .transaction(move |c| {
            let mut o = ScanOutcome {
                files_read,
                ..ScanOutcome::default()
            };
            for s in &synced {
                o.items_seen += 1;
                let item = match dbinbox::find_by_dir_key(c, &s.dir_key)? {
                    Some(it) => {
                        dbinbox::touch(c, it.id, now)?;
                        it
                    }
                    None => {
                        o.items_new += 1;
                        let id = dbinbox::insert_item(c, &s.dir, &s.dir_key, now)?;
                        dbinbox::get(c, id)?.ok_or_else(|| {
                            DbError::Internal(format!("挿入した inbox_items {id} が無い"))
                        })?
                    }
                };
                if item.state == ItemState::Placing {
                    // 配置中の件は触らない（同じジョブの中でしか placing にならない）
                    continue;
                }
                if let Some(files) = &s.files {
                    dbinbox::replace_files(c, item.id, files)?;
                    if item.discard_requested_at.is_some() {
                        // 破棄待ちの間にファイルが変わった（足された・差し替えられた）。人が見ていない
                        // ものを消さないよう破棄を解き、rejected のまま残す（D-90）
                        dbinbox::cancel_discard(c, item.id, Some(DISCARD_CANCELLED_BY_CHANGE))?;
                    }
                    if item.state == ItemState::Approved {
                        dbinbox::set_state(
                            c,
                            item.id,
                            ItemState::Pending,
                            Some("ファイルが変わったので再承認が必要"),
                            now,
                        )?;
                    }
                }
                if item.state == ItemState::Placed {
                    // 配置済みの件のディレクトリに音声がある = 消せなかった原本か、配置の後に
                    // 置かれた（または差し替えられた）ファイル。placed の裏に隠さず件として出し直す
                    dbinbox::set_state(
                        c,
                        item.id,
                        ItemState::Pending,
                        Some("配置の後も Inbox に音声が残っている（再承認で配置し直す）"),
                        now,
                    )?;
                }
            }
            for it in dbinbox::stale_items(c, now)? {
                if it.state == ItemState::Placing {
                    continue;
                }
                dbinbox::delete_item(c, it.id)?;
                o.items_removed += 1;
            }
            o.items_removed += dbinbox::expire_placed(c, now - PLACED_RETENTION_SECS)?;
            Ok(o)
        })
        .await?;
    Ok(outcome)
}

// ---------------------------------------------------------------- 配置

use std::fs::File;
use std::io::Read as _;

use rusqlite::OptionalExtension as _;
use tokio_util::sync::CancellationToken;

use crate::config::LayoutConfig;
use crate::db::now_epoch;
use crate::db::replaygain as dbrg;
use crate::db::scans::{self, AlbumMeta, Fingerprint, PictureState};
use crate::domain::pathgen::{self, PlanItem, Planned, Template, TrackFields};
use crate::domain::tags::{write_tag_changes, TagChange};
use crate::edit::Editor;
use crate::import::placement::{
    find_or_create_album, place_one, register_track, remove_placed, PlacedFile, PlacementError,
};
use crate::import::scanner::{read_fingerprint, track_content};
use crate::import::sidecar::{RipEntry, Sidecar};
use crate::jobs::handlers::rg::{new_album_job, new_track_job};
use crate::jobs::{Event, Jobs, LibraryEvent};

/// 配置に要する環境
pub struct PlaceItemEnv {
    pub db: Arc<Db>,
    pub library: Arc<RootDir>,
    pub inbox: Arc<RootDir>,
    pub jobs: Arc<Jobs>,
    pub layout: LayoutConfig,
    /// 無ければ normalize は投入しない
    pub editor: Option<Arc<Editor>>,
    pub wav_to_flac: bool,
    /// テスト用: Inbox 側を読んだ後・配置の前に呼ぶ（その間に原本が差し替えられた状況を作る）
    #[doc(hidden)]
    pub before_place: Option<crate::cd::place::PlaceHook>,
    /// 配置直後に album のアートワークを解決するためのキャッシュ（無ければ次のスキャンに任せる。P3-4）
    pub artwork: Option<Arc<crate::media::artwork::ArtworkStore>>,
    /// テスト用: 配置の後・アートワーク解決の前に呼ぶ（排他の保持と I/O 失敗の状況を作る）
    #[doc(hidden)]
    pub before_artwork: Option<crate::cd::place::PlaceHook>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemPlaced {
    pub album_id: i64,
    pub rel_dir: RelPath,
    /// draft の順
    pub track_ids: Vec<i64>,
    pub job_ids: Vec<i64>,
    /// 投入した normalize の編集バッチ
    pub normalize_batch: Option<i64>,
    /// 件のサイドカーにあった購読 id（重複なし。配置の後続で同期を投入する。P4-16）
    pub subscription_ids: Vec<i64>,
}

/// 計画（draft の順）
#[derive(Debug, Clone)]
struct ItemPlan {
    paths: Vec<RelPath>,
    rel_dir: RelPath,
    release: String,
    category_id: Option<i64>,
}

/// 配置前に Inbox 側で読んだ 1 ファイル（draft の順）
#[derive(Debug, Clone)]
struct Source {
    rel: RelPath,
    ext: Option<String>,
    fp: Fingerprint,
    changes: Vec<TagChange>,
}

/// 件の吸い出しの記録を下書きのトラックへ名前で結びつけたもの（D-67 追記）
#[derive(Debug, Clone)]
pub struct RipBinding {
    pub entry: RipEntry,
    /// 下書きの順 → `entry.report` の位置
    pub index: Vec<usize>,
    /// 下書きで揃っているディスク番号（検証記録の `disc_no`）
    pub disc_no: u32,
}

/// サイドカーの吸い出しの記録を下書きのトラックへ結びつける。対応の鍵はファイルの basename
/// （承認で番号もタイトルも直せるので、配列の順は信じない）。記録の形が崩れている、件のファイルと
/// 1 対 1 に対応しない、下書きのディスク番号が 1 つに揃わない（1 件 = 1 枚）なら Err（理由）で、
/// 呼び出し側は配置しない
pub fn bind_rip(entry: &RipEntry, draft: &InboxDraft) -> Result<RipBinding, String> {
    entry.check().map_err(|e| e.to_string())?;
    if draft.tracks.len() != entry.files.len() {
        return Err(format!(
            "件のファイル数（{}）が記録のトラック数（{}）と違う",
            draft.tracks.len(),
            entry.files.len()
        ));
    }
    let mut index = Vec::with_capacity(draft.tracks.len());
    let mut used = HashSet::new();
    for t in &draft.tracks {
        let name = t.rel_path.rsplit('/').next().unwrap_or(&t.rel_path);
        let i = entry
            .index_of(name)
            .ok_or_else(|| format!("記録に無いファイル: {}", t.rel_path))?;
        if !used.insert(i) {
            return Err(format!(
                "記録の同じトラックに 2 本が対応する: {}",
                t.rel_path
            ));
        }
        index.push(i);
    }
    let discs: HashSet<u32> = draft.tracks.iter().map(|t| t.disc_no).collect();
    let disc_no = match discs.into_iter().collect::<Vec<_>>().as_slice() {
        [d] => *d,
        _ => return Err("1 枚の吸い出しなのにディスク番号が揃っていない".to_owned()),
    };
    Ok(RipBinding {
        entry: entry.clone(),
        index,
        disc_no,
    })
}

fn parse_template(name: &str, s: &str) -> Result<Template, InboxError> {
    Template::parse(s).map_err(|e| InboxError::Conflict(format!("[layout].{name} が不正: {e}")))
}

/// 補正で変わるタグ（現在値と違うキーだけ）
fn tag_changes(draft: &InboxDraft, index: usize, current: &[(String, String)]) -> Vec<TagChange> {
    let t = &draft.tracks[index];
    let mut desired: Vec<(&str, String)> = vec![
        ("ALBUMARTIST", draft.albumartist.trim().to_owned()),
        ("ALBUM", draft.album.trim().to_owned()),
        ("TITLE", t.title.trim().to_owned()),
        ("ARTIST", draft.track_artist(index).to_owned()),
        ("TRACKNUMBER", t.track_no.to_string()),
        ("DISCNUMBER", t.disc_no.to_string()),
    ];
    if let Some(d) = draft
        .date
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        desired.push(("DATE", d.to_owned()));
    }
    if draft.disc_count() > 1 {
        desired.push(("DISCTOTAL", draft.disc_count().to_string()));
    }
    // 承認画面で選んだ MusicBrainz のリリース（P4-21）。None なら触らない
    for (k, v) in [
        ("MUSICBRAINZ_ALBUMID", &draft.release_id),
        ("MUSICBRAINZ_RELEASEGROUPID", &draft.release_group_id),
    ] {
        if let Some(v) = v.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            desired.push((k, v.to_owned()));
        }
    }
    let mut out: Vec<TagChange> = desired
        .into_iter()
        .filter(|(k, v)| {
            let now: Vec<&str> = current
                .iter()
                .filter(|(ck, _)| ck == k)
                .map(|(_, cv)| cv.as_str())
                .collect();
            // ARTIST は多値（プラグインの artists 等）があり得る。提案は全値の結合なので、結合文字列の
            // まま（未編集）なら多値を保つ。編集していれば 1 値で置き換える（D-70）
            if *k == "ARTIST" && keeps_artists(t.keep_artists, draft.track_artist(index), current) {
                return false;
            }
            now != [v.as_str()]
        })
        .map(|(k, v)| TagChange {
            key: k.to_owned(),
            values: Some(vec![v]),
        })
        .collect();
    // 承認画面で直したファイルのタグ（D-86）。現在値と同じものは書かない
    for (key, values) in &t.tags {
        let values: Option<Vec<String>> = values
            .as_ref()
            .map(|vs| {
                vs.iter()
                    .map(|v| v.trim().to_owned())
                    .filter(|v| !v.is_empty())
                    .collect::<Vec<_>>()
            })
            .filter(|vs| !vs.is_empty());
        let now: Vec<&str> = current
            .iter()
            .filter(|(ck, _)| ck == key)
            .map(|(_, cv)| cv.as_str())
            .collect();
        let same = match &values {
            Some(vs) => now == vs.iter().map(String::as_str).collect::<Vec<_>>(),
            None => now.is_empty(),
        };
        if !same {
            out.push(TagChange {
                key: key.clone(),
                values,
            });
        }
    }
    out
}

/// 下書きの `picture`（D-86）を store から front cover として読む（下書きの順）。指定があるのに
/// 置き場が無い・画像が無いなら Conflict（承認し直して選び直してもらう）
fn load_draft_pictures(
    store: Option<&crate::media::artwork::ArtworkStore>,
    draft: &InboxDraft,
) -> Result<Vec<Option<lofty::picture::Picture>>, InboxError> {
    draft
        .tracks
        .iter()
        .map(|t| {
            let Some(value) = t.picture.as_deref() else {
                return Ok(None);
            };
            let Some(store) = store else {
                return Err(InboxError::Conflict(
                    "画像の置き場（artwork）が無いので画像を埋め込めない".into(),
                ));
            };
            match crate::edit::picture::load_picture(store, value)? {
                Some(p) => Ok(Some(p)),
                None => Err(InboxError::Conflict(format!(
                    "{}: 画像が見つからない（{value}）。承認画面で選び直す",
                    t.rel_path
                ))),
            }
        })
        .collect()
}

/// Inbox 側を読む（配置の前）: 行と stat が一致することを確かめ、音声のフィンガープリントと
/// 補正で変わるタグを出す
fn read_sources(
    inbox: &RootDir,
    draft: &InboxDraft,
    files: &HashMap<String, FileRow>,
) -> Result<Vec<Source>, InboxError> {
    let mut out = Vec::with_capacity(draft.tracks.len());
    for (i, t) in draft.tracks.iter().enumerate() {
        let row = files
            .get(&canonical_key(&t.rel_path))
            .ok_or_else(|| InboxError::Draft(DraftError::UnknownFile(t.rel_path.clone())))?;
        let rel = RelPath::parse(&t.rel_path)
            .map_err(|e| InboxError::Conflict(format!("{}: {e}", t.rel_path)))?;
        let st = inbox.stat(&rel)?;
        if !stat_matches(row, &st) {
            return Err(InboxError::Changed(t.rel_path.clone()));
        }
        let ext = rel
            .file_name()
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase());
        let af = read_audio_file(inbox.open_file(&rel)?, ext.as_deref())
            .map_err(|e| InboxError::Conflict(format!("{}: {e}", t.rel_path)))?;
        let fp = read_fingerprint(inbox, &rel, &af);
        out.push(Source {
            rel,
            ext,
            fp,
            changes: tag_changes(draft, i, &row.tags),
        });
    }
    Ok(out)
}

/// 自分の成果物の候補: 同じ音声の active な行がある album ごとの、そのリリースキーと行の `rel_path_key`
/// （再実行で見分ける。どれが本当に自分のものかは [`pick_self_album`] が決める）
#[derive(Debug)]
struct SelfCandidate {
    album_id: i64,
    release: String,
    keys: HashSet<String>,
}

/// 同じ音声（フィンガープリント一致）の active な行を album ごとに集める（album の無い行は数えない）
fn self_candidates(
    conn: &rusqlite::Connection,
    sources: &[Source],
) -> Result<Vec<SelfCandidate>, InboxError> {
    let mut out: Vec<SelfCandidate> = Vec::new();
    let mut st = conn.prepare_cached(
        "SELECT t.rel_path_key, t.album_id, a.mb_release_id
           FROM tracks t JOIN albums a ON a.id = t.album_id
          WHERE t.missing_since IS NULL AND ((?1 IS NOT NULL AND t.audio_md5 = ?1)
                                           OR (?2 IS NOT NULL AND t.audio_fp = ?2))",
    )?;
    for src in sources {
        let (md5, afp): (Option<Vec<u8>>, Option<Vec<u8>>) = match src.fp {
            Fingerprint::Md5(Some(m)) => (Some(m.to_vec()), None),
            Fingerprint::Fp(Some(f)) => (None, Some(f.to_vec())),
            _ => continue,
        };
        let rows = st
            .query_map(rusqlite::params![md5, afp], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (key, album_id, mb) in rows {
            match out.iter_mut().find(|c| c.album_id == album_id) {
                Some(c) => {
                    c.keys.insert(key);
                }
                None => out.push(SelfCandidate {
                    album_id,
                    release: crate::import::placement::release_key(album_id, mb.as_deref()),
                    keys: HashSet::from([key]),
                }),
            }
        }
    }
    Ok(out)
}

/// 候補から自分の成果物の album を決める（D-29: 同じ音声を無条件に自分とはみなさない。D-67 追記 3）。
/// 件に MBID があればそのリリースの album だけ（配置のキーも `mb:` なので一致する）。無ければ MB キーの
/// 無い album のうち、件を入れられる（[`fits_plain_album`]。CD とそれ以外を混ぜない・ディスク番号が
/// 自分の行以外と重ならない）ものが**ちょうど 1 つ**のときだけ。決まらなければ None（新規として計画し、
/// 衝突なら衝突にする）
fn pick_self_album(
    conn: &rusqlite::Connection,
    candidates: Vec<SelfCandidate>,
    incoming_mb: Option<&str>,
    incoming_cd: bool,
    discs: &[u32],
) -> Result<Option<SelfCandidate>, InboxError> {
    let mut fit = Vec::new();
    for c in candidates {
        let ok = match incoming_mb {
            Some(m) => c.release == crate::import::placement::mb_key(m),
            None => {
                c.release.starts_with("album:")
                    && fits_plain_album(&album_rows(conn, c.album_id, &c.keys)?, incoming_cd, discs)
                        .is_ok()
            }
        };
        if ok {
            fit.push(c);
        }
    }
    Ok(if fit.len() == 1 { fit.pop() } else { None })
}

/// 下書きの宛先ディレクトリに既にある album（追記先。D-70）。MB リリースの album は別リリースなので
/// 対象にしない（従来どおり降格か衝突）。CD とそれ以外は混ぜない（D-67 追記 3）
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Destination {
    pub album_id: i64,
    pub album: Option<String>,
    /// active なトラック数
    pub track_count: i64,
    /// active な `track_no` の最大（採番の起点）
    pub max_track_no: i64,
    /// 追記先の album gain の属性（承認画面のチェックボックスの初期値。D-74）
    pub album_gain: bool,
    /// active なトラックの `(disc_no, track_no)`（承認の検証と、承認画面の「番号が重なる」警告。応答では
    /// `[[disc, track], …]` の昇順）
    #[serde(serialize_with = "sorted_numbers")]
    pub numbers: HashSet<(u32, u32)>,
}

fn sorted_numbers<S: serde::Serializer>(
    numbers: &HashSet<(u32, u32)>,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    let mut v: Vec<&(u32, u32)> = numbers.iter().collect();
    v.sort_unstable();
    serde::Serialize::serialize(&v, s)
}

impl Destination {
    pub fn release_key(&self) -> String {
        format!("album:{}", self.album_id)
    }
}

/// ファイル名を stem と拡張子（小文字）に分ける
fn split_name(rel_path: &str) -> (String, String) {
    match rel_path
        .rsplit('/')
        .next()
        .unwrap_or(rel_path)
        .rsplit_once('.')
    {
        Some((s, e)) => (s.to_owned(), e.to_ascii_lowercase()),
        None => (rel_path.to_owned(), String::new()),
    }
}

/// 下書きのトラック `i` のテンプレート値。`files` は件のファイル（多値の ARTIST を保つときの
/// `{artist}` を Library の `artist_display` と同じ `", "` 結合にするため）
fn track_fields(
    draft: &InboxDraft,
    category: Option<&str>,
    i: usize,
    files: &[FileRow],
) -> TrackFields {
    let t = &draft.tracks[i];
    let (stem, ext) = split_name(&t.rel_path);
    let key = canonical_key(&t.rel_path);
    let artist = match files.iter().find(|f| canonical_key(&f.rel_path) == key) {
        Some(f)
            if keeps_artists(t.keep_artists, draft.track_artist(i), &f.tags)
                && !artist_values(&f.tags).is_empty() =>
        {
            artist_values(&f.tags).join(crate::import::scanner::ARTIST_SEPARATOR)
        }
        _ => draft.track_artist(i).to_owned(),
    };
    TrackFields {
        category: category.map(str::to_owned),
        albumartist: Some(draft.albumartist.trim().to_owned()),
        artist: Some(artist),
        album: Some(draft.album.trim().to_owned()),
        title: Some(t.title.trim().to_owned()),
        disc_no: Some(i64::from(t.disc_no)),
        track_no: Some(i64::from(t.track_no)),
        year: draft.year(),
        edition: None,
        ext,
        stem,
    }
}

/// 下書きの category（統制語彙に無ければ None = `_Unsorted`）とテンプレート
fn resolve_template(
    conn: &rusqlite::Connection,
    layout: &LayoutConfig,
    draft: &InboxDraft,
) -> Result<(Option<crate::db::categories::Category>, Template), InboxError> {
    let category = match draft.category.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => crate::db::categories::find_by_key(conn, name)?,
        _ => None,
    };
    let template = if category.is_none() {
        parse_template("unsorted", &layout.unsorted)?
    } else if draft.disc_count() > 1 {
        parse_template("multi_disc", &layout.multi_disc)?
    } else {
        parse_template("single_disc", &layout.single_disc)?
    };
    Ok((category, template))
}

/// CD から来たことを示すタグ（D-67 追記 3。値は 1 枚ごとの DiscID）
const DISCID_KEY: &str = "MUSICBRAINZ_DISCID";

/// album が CD の album か（active なトラックのどれかに `MUSICBRAINZ_DISCID` がある。D-67 追記 3）。
/// 購読の束ね先の再検証に使う
pub fn album_is_cd(conn: &rusqlite::Connection, album_id: i64) -> Result<bool, DbError> {
    Ok(album_rows(conn, album_id, &HashSet::new())?
        .iter()
        .any(|r| r.cd))
}

/// 件が CD から来たか（どれかのファイルに `MUSICBRAINZ_DISCID` がある）
fn incoming_is_cd(files: &[FileRow]) -> bool {
    files.iter().any(|f| tag(f, DISCID_KEY).is_some())
}

/// album の active なトラックの `(disc_no, CD か, 自分の成果物か)`。CD かはトラックのタグに
/// `MUSICBRAINZ_DISCID` があること、自分の成果物かは `own` の `rel_path_key` にあること（D-67 追記 3）
fn album_rows(
    conn: &rusqlite::Connection,
    album_id: i64,
    own: &HashSet<String>,
) -> Result<Vec<AlbumRow>, DbError> {
    let mut st = conn.prepare_cached(
        "SELECT t.rel_path_key, t.disc_no,
                EXISTS (SELECT 1 FROM track_tags g WHERE g.track_id = t.id AND g.key = ?2)
           FROM tracks t
          WHERE t.album_id = ?1 AND t.missing_since IS NULL",
    )?;
    let rows = st
        .query_map(rusqlite::params![album_id, DISCID_KEY], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?.unwrap_or(1),
                r.get::<_, bool>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(key, disc_no, cd)| AlbumRow {
            disc_no,
            cd,
            own: own.contains(&key),
        })
        .collect())
}

/// [`album_rows`] の 1 行
#[derive(Debug, Clone, Copy)]
struct AlbumRow {
    disc_no: i64,
    cd: bool,
    own: bool,
}

/// MB キーの無い album に件を入れてよいか（D-67 追記 3）。空の album には何でも入る。CD とそれ以外は
/// 混ぜない（**全行で**判定する。同じ音声の行を自分の成果物と見て除くと、他の album の行まで除いてしまう）。
/// CD 同士はディスク番号が重ならないときだけ（同じ番号は同名の別の盤。自分の成果物の行は除く = 再実行）。
/// 入れられなければ Err(理由)
fn fits_plain_album(rows: &[AlbumRow], incoming_cd: bool, discs: &[u32]) -> Result<(), String> {
    if rows.is_empty() {
        return Ok(());
    }
    let album_cd = rows.iter().any(|r| r.cd);
    if incoming_cd != album_cd {
        return Err(if incoming_cd {
            "CD の件を CD でない album に入れない".into()
        } else {
            "CD でない件を CD の album に入れない".into()
        });
    }
    if incoming_cd {
        if let Some(d) = discs
            .iter()
            .find(|&&d| rows.iter().any(|r| !r.own && r.disc_no == i64::from(d)))
        {
            return Err(format!("album に同じディスク番号 {d} の盤が既にある"));
        }
    }
    Ok(())
}

/// 件のリリース（配置のリリースキー `mb:` の元）: 下書きで選んだもの（P4-21）→ 件のファイルの
/// MUSICBRAINZ_ALBUMID の最頻値
fn incoming_release_id(draft: &InboxDraft, files: &[FileRow]) -> Option<String> {
    draft
        .release_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .or_else(|| mode(files.iter().filter_map(|f| tag(f, "MUSICBRAINZ_ALBUMID"))))
}

/// albumartist / album / category から追記先の album を引く（購読の追記先。P4-16）。下書きの規則
/// （[`destination`]）と同じで、Inbox の件が置かれる先と一致する
pub fn destination_of(
    conn: &rusqlite::Connection,
    layout: &LayoutConfig,
    category: Option<&str>,
    albumartist: &str,
    album: &str,
) -> Result<Option<Destination>, InboxError> {
    let draft = InboxDraft {
        category: category.map(str::to_owned),
        albumartist: albumartist.to_owned(),
        album: album.to_owned(),
        date: None,
        tracks: vec![DraftTrack {
            rel_path: "x.opus".to_owned(),
            disc_no: 1,
            track_no: 1,
            title: "x".to_owned(),
            artist: String::new(),
            keep_artists: None,
            tags: Default::default(),
            picture: None,
        }],
        album_gain: false,
        release_id: None,
        release_group_id: None,
    };
    destination(conn, layout, &draft, &[])
}

/// 下書きの宛先ディレクトリ（降格前の素のパス）に active な album があり、それが MB リリースでなければ
/// 返す（追記先）。件のファイルに MUSICBRAINZ_ALBUMID があれば別リリースなので追記先は無い（配置は
/// `mb:` のキーで降格か衝突になる）。トラックが無い下書きも None。
///
/// **CD とそれ以外は混ぜない**（D-67 追記 3）: 「CD」はトラックのタグに `MUSICBRAINZ_DISCID` があること
/// （ファイルにあるので DB を作り直しても同じ判定）。CD の件は CD の album にだけ、しかも**まだ無い
/// ディスク番号**のときだけ追記する（MBID の無い複数枚組の 2 枚目を 1 枚目に合流させる。同じ番号なら
/// 同名の別の盤）。CD でない件（YouTube・手で置いたもの）は CD でない album にだけ追記する
pub fn destination(
    conn: &rusqlite::Connection,
    layout: &LayoutConfig,
    draft: &InboxDraft,
    files: &[FileRow],
) -> Result<Option<Destination>, InboxError> {
    if draft.tracks.is_empty() || incoming_release_id(draft, files).is_some() {
        return Ok(None);
    }
    let (category, template) = resolve_template(conn, layout, draft)?;
    let fields = track_fields(draft, category.as_ref().map(|c| c.name.as_str()), 0, files);
    let Ok(path) = template.render(&fields, pathgen::AlbumVariant::Plain) else {
        return Ok(None);
    };
    let Some(rel_dir) = path.parent() else {
        return Ok(None);
    };
    let found: Option<(i64, Option<String>, i64)> = conn
        .query_row(
            "SELECT id, album, album_gain FROM albums
              WHERE rel_dir_key = ?1 AND missing_since IS NULL
                AND (mb_release_id IS NULL OR mb_release_id = '')",
            [rel_dir.key()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((album_id, album, album_gain)) = found else {
        return Ok(None);
    };
    let discs: Vec<u32> = draft.tracks.iter().map(|t| t.disc_no).collect();
    if fits_plain_album(
        &album_rows(conn, album_id, &HashSet::new())?,
        incoming_is_cd(files),
        &discs,
    )
    .is_err()
    {
        return Ok(None);
    }
    let mut st = conn.prepare_cached(
        "SELECT disc_no, track_no FROM tracks WHERE album_id = ?1 AND missing_since IS NULL",
    )?;
    let mut numbers = HashSet::new();
    let mut track_count = 0;
    let mut max_track_no = 0;
    for row in st.query_map([album_id], |r| {
        Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?))
    })? {
        let (disc_no, track_no) = row?;
        track_count += 1;
        let track_no = track_no.unwrap_or(0);
        max_track_no = max_track_no.max(track_no);
        if let (Ok(d), Ok(t)) = (u32::try_from(disc_no.unwrap_or(1)), u32::try_from(track_no)) {
            numbers.insert((d, t));
        }
    }
    Ok(Some(Destination {
        album_id,
        album,
        track_count,
        max_track_no,
        album_gain: album_gain == 1,
        numbers,
    }))
}

fn plan_item(
    conn: &rusqlite::Connection,
    layout: &LayoutConfig,
    item: &Item,
    draft: &InboxDraft,
    files: &[FileRow],
    sources: &[Source],
) -> Result<ItemPlan, InboxError> {
    let (category, template) = resolve_template(conn, layout, draft)?;
    // 自分の成果物（同じ音声の行）は占有から外し、その album のリリースキーに揃える（再実行）。
    // 同じ音声の行が他の album にあっても、件を入れられない album は自分とみなさない（D-29 / D-67 追記 3）
    let incoming_mb = incoming_release_id(draft, files);
    let discs: Vec<u32> = draft.tracks.iter().map(|t| t.disc_no).collect();
    let own = pick_self_album(
        conn,
        self_candidates(conn, sources)?,
        incoming_mb.as_deref(),
        incoming_is_cd(files),
        &discs,
    )?;
    let self_keys = own.as_ref().map(|c| c.keys.clone()).unwrap_or_default();
    let self_album = own.map(|c| (c.album_id, c.release));
    // リリースキー: MUSICBRAINZ_ALBUMID の最頻値があれば mb:、自分の成果物の album があればそれ、
    // 宛先に追記できる album があればそれ（D-70）、無ければ件ごとの新規
    let release = match incoming_mb
        .map(|m| crate::import::placement::mb_key(&m))
        .or_else(|| self_album.map(|(_, k)| k))
    {
        Some(k) => k,
        None => match destination(conn, layout, draft, files)? {
            Some(d) => d.release_key(),
            None => format!("inbox:{}", item.id),
        },
    };
    let items: Vec<PlanItem> = (0..draft.tracks.len())
        .map(|i| PlanItem {
            track_id: -(i as i64) - 1,
            template: template.clone(),
            fields: track_fields(draft, category.as_ref().map(|c| c.name.as_str()), i, files),
            release: release.clone(),
            current_rel_path: String::new(),
        })
        .collect();
    let mut occ = crate::edit::rename::load_occupancy(conn, &HashSet::new())?;
    for k in &self_keys {
        occ.path_keys.remove(k);
    }
    let mut paths = Vec::with_capacity(items.len());
    for (planned, t) in pathgen::plan(&items, &occ).into_iter().zip(&draft.tracks) {
        match planned {
            Planned::Path(p) => paths.push(p),
            Planned::Conflict(r) => {
                return Err(InboxError::Conflict(format!("{}: {r}", t.rel_path)))
            }
            Planned::Unchanged => {
                return Err(InboxError::Conflict(format!(
                    "{}: パスを決められない",
                    t.rel_path
                )))
            }
        }
    }
    let Some(rel_dir) = paths.first().and_then(RelPath::parent) else {
        return Err(InboxError::Conflict("宛先が root 直下になる".into()));
    };
    if paths.iter().any(|p| p.parent().as_ref() != Some(&rel_dir)) {
        return Err(InboxError::Conflict(
            "トラックの宛先が 1 つのディレクトリに揃わない".into(),
        ));
    }
    Ok(ItemPlan {
        paths,
        rel_dir,
        release,
        category_id: category.map(|c| c.id),
    })
}

/// 配置先の見込み（承認画面の ④。`POST /api/inbox/:id/preview`、D-86）
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlacePreview {
    /// 置くディレクトリ（Library 相対）
    pub rel_dir: String,
    /// 置くファイル（Library 相対。下書きの順）
    pub paths: Vec<String>,
}

/// 配置の計画（[`plan_item`]）を音声を読まずに引く。自分の成果物（同じ音声の既存行）は見ないので、
/// 途中で失敗した件の再承認では実際の配置と食い違うことがある（見込みとして出す）
pub fn preview(
    conn: &rusqlite::Connection,
    layout: &LayoutConfig,
    item: &Item,
    draft: &InboxDraft,
    files: &[FileRow],
) -> Result<PlacePreview, InboxError> {
    let names: Vec<String> = files.iter().map(|f| f.rel_path.clone()).collect();
    draft.validate(&names)?;
    let plan = plan_item(conn, layout, item, draft, files, &[])?;
    Ok(PlacePreview {
        rel_dir: plan.rel_dir.to_string(),
        paths: plan.paths.iter().map(ToString::to_string).collect(),
    })
}

/// 下書きの `picture` のうち `artwork` 行の無いもの（承認の前に弾く。D-86）
pub fn missing_pictures(
    conn: &rusqlite::Connection,
    draft: &InboxDraft,
) -> Result<Vec<String>, DbError> {
    let mut st = conn.prepare_cached("SELECT EXISTS (SELECT 1 FROM artwork WHERE sha256 = ?1)")?;
    let mut out = Vec::new();
    for p in draft.tracks.iter().filter_map(|t| t.picture.as_deref()) {
        let Some((_, hash)) = crate::edit::picture::parse_picture_value(p) else {
            continue;
        };
        let exists: bool = st.query_row([hash.to_vec()], |r| r.get(0))?;
        if !exists && !out.iter().any(|o| o == p) {
            out.push(p.to_owned());
        }
    }
    Ok(out)
}

/// 置いた結果（登録の材料）
struct Placed {
    tracks: Vec<(scans::Physical, scans::TrackContent, Fingerprint)>,
    placed_new: Vec<RelPath>,
    /// 作ったディレクトリのうち最上位（無ければ全部既存）。失敗時にここまで消す
    created_top: Option<RelPath>,
    /// 移した同梱ファイル（Inbox 側の rel）
    companions: Vec<RelPath>,
}

fn same_audio(a: Fingerprint, b: Fingerprint) -> bool {
    match (a, b) {
        (Fingerprint::Md5(Some(x)), Fingerprint::Md5(Some(y))) => x == y,
        (Fingerprint::Fp(Some(x)), Fingerprint::Fp(Some(y))) => x == y,
        _ => false,
    }
}

/// `dir` までを作り、新しく作った最上位のディレクトリを返す（失敗時の後始末に使う）
fn create_dirs(root: &RootDir, dir: &RelPath) -> Result<Option<RelPath>, FsError> {
    let mut top: Option<RelPath> = None;
    let mut prefix: Option<RelPath> = None;
    for name in dir.components() {
        let next = match &prefix {
            Some(p) => p.join(name),
            None => RelPath::parse(name),
        }
        .map_err(|e| FsError::Io(std::io::Error::other(e)))?;
        if top.is_none() && matches!(root.stat(&next), Err(FsError::NotFound)) {
            top = Some(next.clone());
        }
        prefix = Some(next);
    }
    root.create_dir_all(dir)?;
    Ok(top)
}

/// 失敗時の後始末: 置いたファイルを消し、作ったディレクトリを `created_top` まで消す
fn cleanup(root: &RootDir, dir: &RelPath, placed_new: &[RelPath], created_top: Option<&RelPath>) {
    remove_placed(root, dir, placed_new);
    let Some(top) = created_top else {
        return;
    };
    let mut cur = dir.parent();
    while let Some(d) = cur {
        if d.key().len() < top.key().len() {
            break;
        }
        let _ = root.remove_dir(&d);
        cur = d.parent();
    }
}

/// Inbox からコピーし、補正タグを書き、Library に置く
#[allow(clippy::too_many_arguments)]
fn place_files(
    library: &RootDir,
    inbox: &RootDir,
    item: &Item,
    plan: &ItemPlan,
    files: &HashMap<String, FileRow>,
    sources: &[Source],
    pictures: &[Option<lofty::picture::Picture>],
    draft: &InboxDraft,
) -> Result<Placed, InboxError> {
    let created_top = create_dirs(library, &plan.rel_dir)?;
    let mut placed_new: Vec<RelPath> = Vec::new();
    let result = (|| -> Result<Placed, InboxError> {
        let mut tracks = Vec::with_capacity(plan.paths.len());
        for (i, (target, src)) in plan.paths.iter().zip(sources).enumerate() {
            let ext_for_write = src.ext.clone();
            let changes = src.changes.clone();
            let picture: Option<Vec<lofty::picture::Picture>> =
                pictures.get(i).cloned().flatten().map(|p| vec![p]);
            let want_picture = picture
                .as_ref()
                .and_then(|p| p.first())
                .map(|p| crate::media::artwork::ArtworkStore::hash_of(p.data()));
            let src_fp = src.fp;
            // コピーに使う FD そのものを承認時の行（inode / size / mtime / ctime）と照合する。
            // read_sources の stat からここまでの間に差し替えられていれば Changed（D-68「コピー中に
            // stat が変わったら失敗」）。コピーの後にも同じ FD を見て、読んでいる間の変更を弾く
            let src_file = inbox.open_file(&src.rel)?;
            let before = crate::fsroot::fstat(&src_file)?;
            let row = files
                .get(&src.rel.key())
                .ok_or_else(|| InboxError::Changed(src.rel.to_string()))?;
            if !stat_matches(row, &before) {
                return Err(InboxError::Changed(src.rel.to_string()));
            }
            let outcome = place_one(
                library,
                &plan.rel_dir,
                target,
                &src_file,
                move |tmp: &mut File| {
                    if !changes.is_empty() || picture.is_some() {
                        write_tag_changes(
                            tmp,
                            ext_for_write.as_deref(),
                            &changes,
                            picture.as_deref(),
                        )
                        .map_err(|e| PlacementError::Io(std::io::Error::other(e.to_string())))?;
                    }
                    Ok(())
                },
                |existing: File| {
                    // 宛先に既にあるファイルが同じ音声なら自分の成果物（再実行）。置き換えはしない
                    // （ファイルが正。外部の変更を履歴なしに上書きしない）。代わりに、今回の補正
                    // （タグ・画像。D-86）がそのファイルに既に入っていることを確かめ、違えば失敗にして
                    // 件と下書きを残す（補正を黙って捨てて成功にしない。codex 指摘）
                    // タグ・画像と音声は同じ実体（渡された FD の複製）から読む（パスを開き直すと
                    // 途中で差し替えられた別の実体を混ぜうる。codex 指摘）
                    let ext = target.file_name().rsplit_once('.').map(|(_, e)| e);
                    let fp_file = existing.try_clone()?;
                    let (af, pics) =
                        match crate::domain::tags::read_audio_file_with_pictures(existing, ext) {
                            Ok(v) => v,
                            Err(_) => return Ok(false),
                        };
                    let fp = crate::import::scanner::read_fingerprint_fd(&fp_file, target, &af);
                    if !same_audio(fp, src_fp) {
                        return Ok(false);
                    }
                    let tags_ok = tag_changes(draft, i, af.tags.items()).is_empty();
                    // 指定した画像は front cover として書く。同じ bytes が front 以外（裏表紙等）として
                    // あるだけなら補正済みとみなさない（codex 指摘）
                    let picture_ok = want_picture.is_none_or(|want| {
                        pics.iter().any(|p| {
                            p.pic_type() == lofty::picture::PictureType::CoverFront
                                && crate::media::artwork::ArtworkStore::hash_of(p.data()) == want
                        })
                    });
                    if tags_ok && picture_ok {
                        Ok(true)
                    } else {
                        Err(PlacementError::Conflict(format!(
                            "{target}: 前回の配置の途中のファイルが宛先にあり、今回の補正（タグ・画像）と違う。\
                             Library のこのファイルを確かめて消すか、配置後にライブラリの編集で直す"
                        )))
                    }
                },
            )?;
            if outcome == PlacedFile::New {
                placed_new.push(target.clone());
            }
            let after = crate::fsroot::fstat(&src_file)?;
            if !stat_matches(row, &after) {
                return Err(InboxError::Changed(src.rel.to_string()));
            }
            let file = library.open_file(target)?;
            let ph = crate::fsroot::fstat(&file)?;
            let af = read_audio_file(file, src.ext.as_deref())
                .map_err(|e| InboxError::Conflict(format!("{target}: {e}")))?;
            let fp = read_fingerprint(library, target, &af);
            // 置いたものの音声が承認時に読んだ音声と同じことを確かめる（コピー中の書き換え、
            // 宛先の既存ファイルの採用、いずれも指紋で閉じる）
            if !same_audio(fp, src_fp) {
                return Err(InboxError::Changed(src.rel.to_string()));
            }
            let mut content = track_content(af);
            content.picture = PictureState::Unread;
            tracks.push((ph.into(), content, fp));
        }
        // 既知の同梱ファイル（cover 画像 / cue / toc / log）
        let mut companions = Vec::new();
        if !item.rel_dir.is_empty() {
            let dir =
                RelPath::parse(&item.rel_dir).map_err(|e| InboxError::Conflict(e.to_string()))?;
            for e in inbox.read_dir(Some(&dir))? {
                let Some(name) = e.name.to_str() else {
                    continue;
                };
                if e.kind != FileKind::File || !crate::cd::riplog::is_companion_name(name) {
                    continue;
                }
                let (Ok(from), Ok(to)) = (dir.join(name), plan.rel_dir.join(name)) else {
                    continue;
                };
                let mut body = Vec::new();
                inbox.open_file(&from)?.read_to_end(&mut body)?;
                let expected = body.clone();
                match place_one(
                    library,
                    &plan.rel_dir,
                    &to,
                    body.as_slice(),
                    |_| Ok(()),
                    move |mut f: File| {
                        let mut existing = Vec::new();
                        f.read_to_end(&mut existing)?;
                        Ok(existing == expected)
                    },
                ) {
                    Ok(PlacedFile::New) => {
                        placed_new.push(to);
                        companions.push(from);
                    }
                    Ok(PlacedFile::Reused) => companions.push(from),
                    Err(PlacementError::Conflict(r)) => {
                        tracing::warn!(reason = %r, "同梱ファイルは宛先に別の内容があるので Inbox に残す")
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
        library.fsync_dir(Some(&plan.rel_dir))?;
        Ok(Placed {
            tracks,
            placed_new: std::mem::take(&mut placed_new),
            created_top: created_top.clone(),
            companions,
        })
    })();
    match result {
        Ok(p) => Ok(p),
        Err(e) => {
            cleanup(library, &plan.rel_dir, &placed_new, created_top.as_ref());
            Err(e)
        }
    }
}

struct RegisteredItem {
    album_id: i64,
    track_ids: Vec<i64>,
    job_ids: Vec<i64>,
    /// 同じトランザクションで記録した normalize バッチ（WAV / ALAC / AIFF があり、有効なとき）
    normalize_batch: Option<i64>,
    batch_event: Option<crate::jobs::BatchEvent>,
}

/// 登録（1 トランザクション）: album / tracks / track_tags、rg / transcode の投入、WAV / ALAC / AIFF の
/// normalize バッチ（`normalize` のとき）、件の placed。commit の後に落ちても投入が欠けない
#[allow(clippy::too_many_arguments)]
fn register_item(
    conn: &mut rusqlite::Connection,
    item_id: i64,
    plan: &ItemPlan,
    draft: &InboxDraft,
    placed: &Placed,
    files: &HashMap<String, FileRow>,
    normalize: bool,
    rip: Option<&RipBinding>,
    log_rel: Option<&str>,
) -> Result<Result<RegisteredItem, InboxError>, DbError> {
    let tx = conn.transaction()?;
    let now = now_epoch();
    let mb = {
        let list: Vec<FileRow> = files.values().cloned().collect();
        incoming_release_id(draft, &list)
    };
    let meta = AlbumMeta {
        category_id: plan.category_id,
        albumartist: Some(draft.albumartist.trim().to_owned()),
        album: Some(draft.album.trim().to_owned()),
        date: draft
            .date
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_owned),
        original_date: None,
        mb_release_id: mb,
        disc_count: Some(i64::from(draft.disc_count())),
    };
    let album_id = match find_or_create_album(&tx, &plan.rel_dir, &plan.release, &meta) {
        Ok(Ok(id)) => id,
        Ok(Err(reason)) => {
            drop(tx);
            return Ok(Err(InboxError::Conflict(reason)));
        }
        Err(e) => {
            drop(tx);
            return Ok(Err(e.into()));
        }
    };
    let album_name: Option<String> = tx
        .query_row("SELECT album FROM albums WHERE id = ?1", [album_id], |r| {
            r.get(0)
        })
        .optional()?
        .flatten();
    // 追記先の active なトラックと番号が重ならないこと（承認の検証と同じ。承認と配置の間に足された
    // 分を弾く。自分の成果物 = 同じパスの行は除く。D-70）
    if let Err(reason) = check_numbers_free(&tx, album_id, plan, draft)? {
        drop(tx);
        return Ok(Err(InboxError::Conflict(reason)));
    }
    // CD と CD 以外を混ぜない（D-67 追記 3）。計画の後にタグ編集や別の配置で album が変わり得るので、
    // 登録のトランザクションで引き直す。MB キーの album は同じリリースなので対象外
    let album_mb: Option<String> = tx.query_row(
        "SELECT mb_release_id FROM albums WHERE id = ?1",
        [album_id],
        |r| r.get(0),
    )?;
    if album_mb.as_deref().is_none_or(str::is_empty) {
        let own: HashSet<String> = plan.paths.iter().map(RelPath::key).collect();
        let list: Vec<FileRow> = files.values().cloned().collect();
        let discs: Vec<u32> = draft.tracks.iter().map(|t| t.disc_no).collect();
        if let Err(reason) = fits_plain_album(
            &album_rows(&tx, album_id, &own)?,
            incoming_is_cd(&list),
            &discs,
        ) {
            drop(tx);
            return Ok(Err(InboxError::Conflict(reason)));
        }
    }
    // CD の吸い出しなら出自は cd_rip（D-67 追記。スキャナの rip.log 判定と同じ値）
    let source_type = if rip.is_some() { "cd_rip" } else { "download" };
    let mut track_ids = Vec::with_capacity(plan.paths.len());
    let mut expected = Vec::with_capacity(plan.paths.len());
    for (rel, (ph, content, fp)) in plan.paths.iter().zip(&placed.tracks) {
        let id = match register_track(&tx, rel, ph, content, *fp, now) {
            Ok(Ok(r)) => {
                expected.push((r.id, r.audio_version));
                r.id
            }
            Ok(Err(reason)) => {
                drop(tx);
                return Ok(Err(InboxError::Conflict(reason)));
            }
            Err(e) => {
                drop(tx);
                return Ok(Err(e.into()));
            }
        };
        scans::set_track_album(&tx, id, album_id, album_name.as_deref())?;
        scans::set_source_type(&tx, id, source_type)?;
        track_ids.push(id);
    }
    // 吸い出しの検証記録（D-67 追記）。記録の配列は音声トラック順なので、名前で結びつけた位置へ
    // 行を並べ直して渡す。登録と同じトランザクションなので、配置されたのに記録が無い状態を作らない
    // ただし全トラックに吸い出しの記録が既にあれば書かない: 登録の commit の後・Inbox を消す前に
    // 落ちると、残った音声で件が pending に戻り、再承認で同じファイルと行を採用してここへ戻る
    // （Inbox の記録は job_id を持たないので、行の側で見分ける）
    let recorded = match rip {
        Some(_) => crate::db::verify::all_have_rip_records(&tx, &track_ids)?,
        None => false,
    };
    if let Some(rip) = rip.filter(|_| !recorded) {
        let mut ids = vec![0; track_ids.len()];
        for (&id, &i) in track_ids.iter().zip(&rip.index) {
            ids[i] = id;
        }
        let disc = rip.entry.report.disc_record(i64::from(rip.disc_no), &ids);
        match crate::db::verify::record_album(
            &tx,
            album_id,
            None,
            crate::db::verify::VerifySource::Rip,
            &expected,
            &[disc],
            log_rel,
            now,
        )? {
            crate::db::verify::RecordOutcome::Recorded(_)
            | crate::db::verify::RecordOutcome::AlreadyRecorded => {}
            crate::db::verify::RecordOutcome::Changed { track_id } => {
                drop(tx);
                return Ok(Err(InboxError::Conflict(format!(
                    "track {track_id} の音声版が登録の途中で進んだ"
                ))));
            }
        }
    }
    // album gain の属性（D-74）。新規は下書きの値、追記先は下書きの値で上書き（承認画面の初期値は
    // 追記先の現在値）。off にしたときの album 値の片付けは set_album_gain、Derived の追随はここ。
    // rg は on なら album 単位、off なら登録した track ごと
    let gain_change = dbrg::set_album_gain(&tx, album_id, draft.album_gain, now)?.unwrap_or(
        dbrg::AlbumGainChange {
            changed: false,
            cleared: Vec::new(),
        },
    );
    let mut job_ids = Vec::new();
    if draft.album_gain {
        job_ids.push(crate::db::jobs::enqueue(&tx, &new_album_job(album_id), now)?.id());
    } else {
        for &id in &track_ids {
            job_ids.push(crate::db::jobs::enqueue(&tx, &new_track_job(id), now)?.id());
        }
    }
    for &id in track_ids.iter().chain(gain_change.cleared.iter()) {
        job_ids.extend(crate::db::derived::enqueue_if_stale(&tx, id, now)?);
    }
    // 後続の normalize（D-46 の予告。WAV / ALAC / AIFF を FLAC へ）も同じトランザクションで記録する。
    // 別トランザクションにすると、登録の commit からその間に落ちたとき投入が永久に欠ける
    let mut normalize_batch = None;
    let mut batch_event = None;
    if normalize {
        let ids: Vec<i64> = track_ids
            .iter()
            .zip(&draft.tracks)
            .filter(|(_, t)| {
                files
                    .get(&canonical_key(&t.rel_path))
                    .is_some_and(|f| matches!(f.codec.as_str(), "wav" | "alac" | "aiff"))
            })
            .map(|(id, _)| *id)
            .collect();
        if !ids.is_empty() {
            match record_normalize(&tx, &ids, now) {
                Ok(Some(p)) => {
                    job_ids.extend(p.job_ids);
                    normalize_batch = Some(p.batch_id);
                    batch_event = p.event;
                }
                Ok(None) => {}
                Err(e) => {
                    return Ok(Err(InboxError::Conflict(format!(
                        "normalize を投入できない: {e}"
                    ))))
                }
            }
        }
    }
    // 件の placed も同じトランザクションで確定する。commit の直後に落ちても placing のまま残らず
    // （Inbox の原本が残っていれば走査が pending に戻す）、24 時間の placed 表示も失わない
    dbinbox::set_placed(&tx, item_id, album_id, now)?;
    tx.commit()?;
    Ok(Ok(RegisteredItem {
        album_id,
        track_ids,
        job_ids,
        normalize_batch,
        batch_event,
    }))
}

/// `album_id` の active なトラック（計画のパスにある行 = 自分の成果物を除く）に、下書きと同じ
/// `(disc_no, track_no)` があれば Err(理由)
fn check_numbers_free(
    tx: &rusqlite::Connection,
    album_id: i64,
    plan: &ItemPlan,
    draft: &InboxDraft,
) -> Result<Result<(), String>, DbError> {
    let own: HashSet<String> = plan.paths.iter().map(RelPath::key).collect();
    let mut st = tx.prepare_cached(
        "SELECT disc_no, track_no, rel_path_key FROM tracks
          WHERE album_id = ?1 AND missing_since IS NULL",
    )?;
    let taken: HashSet<(i64, i64)> = st
        .query_map([album_id], |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?.unwrap_or(1),
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                r.get::<_, String>(2)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .filter(|(_, _, key)| !own.contains(key))
        .map(|(d, t, _)| (d, t))
        .collect();
    for t in &draft.tracks {
        if taken.contains(&(i64::from(t.disc_no), i64::from(t.track_no))) {
            return Ok(Err(format!(
                "宛先の album に同じ番号のトラックがある: disc {} track {}",
                t.disc_no, t.track_no
            )));
        }
    }
    Ok(Ok(()))
}

/// 可逆（WAV / ALAC / AIFF）を FLAC に正規化する編集バッチを、登録のトランザクションの中で記録する。
/// 宛先が衝突するトラックは警告して外す。記録する対象が無ければ None
fn record_normalize(
    tx: &rusqlite::Connection,
    ids: &[i64],
    now: i64,
) -> Result<Option<crate::edit::Prepared>, crate::edit::EditError> {
    use crate::edit::{
        plan_normalize_tx, prepare_normalize_in, EditError, NormalizePlan, NormalizeTarget,
    };
    let targets: Vec<NormalizeTarget> = plan_normalize_tx(tx, ids)?
        .into_iter()
        .filter_map(|p| match p.planned {
            NormalizePlan::Path(new_rel_path) => Some(NormalizeTarget {
                track_id: p.track_id,
                new_rel_path,
                new_codec: "flac".to_owned(),
                expected: None,
                planned_conflict: None,
            }),
            NormalizePlan::Unchanged => None,
            NormalizePlan::Conflict(r) => {
                tracing::warn!(track_id = p.track_id, reason = %r, "normalize の宛先が衝突");
                None
            }
        })
        .collect();
    if targets.is_empty() {
        return Ok(None);
    }
    match prepare_normalize_in(tx, Some("Inbox 取り込み"), &targets, None, now) {
        Ok(p) => Ok(Some(p)),
        Err(EditError::NoChanges) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Inbox 側を消す（コピー中に変わっていなければ）。空になったディレクトリも消す
fn consume_inbox(
    inbox: &RootDir,
    item: &Item,
    files: &HashMap<String, FileRow>,
    sources: &[Source],
    companions: &[RelPath],
    sidecar_key: SidecarKey,
) {
    for src in sources {
        let unchanged = files
            .get(&src.rel.key())
            .zip(inbox.stat(&src.rel).ok())
            .is_some_and(|(row, st)| stat_matches(row, &st));
        if !unchanged {
            tracing::warn!(path = %src.rel, "Inbox のファイルが配置の間に変わったので残す");
            continue;
        }
        if let Err(e) = inbox.unlink(&src.rel) {
            tracing::warn!(path = %src.rel, error = %e, "Inbox のファイルを消せない");
        }
    }
    for rel in companions {
        if let Err(e) = inbox.unlink(rel) {
            tracing::warn!(path = %rel, error = %e, "Inbox の同梱ファイルを消せない");
        }
    }
    if !item.rel_dir.is_empty() {
        if let Ok(dir) = RelPath::parse(&item.rel_dir) {
            // サイドカー（D-70）は Library に持っていかず、配置の成功で役目を終える。登録の後に
            // 差し替えられたものは読んでいないので消さない
            if let Ok(sidecar) = dir.join(crate::import::sidecar::SIDECAR_NAME) {
                if current_sidecar_key(inbox, &item.rel_dir) != sidecar_key {
                    tracing::warn!(path = %sidecar, "サイドカーが配置の間に変わったので残す");
                } else {
                    match inbox.unlink(&sidecar) {
                        Ok(()) | Err(FsError::NotFound) => {}
                        Err(e) => {
                            tracing::warn!(path = %sidecar, error = %e, "サイドカーを消せない")
                        }
                    }
                }
            }
            match inbox.remove_dir(&dir) {
                Ok(()) | Err(FsError::NotFound) => {}
                Err(FsError::Io(e))
                    if e.raw_os_error() == Some(rustix::io::Errno::NOTEMPTY.raw_os_error()) => {}
                Err(e) => tracing::warn!(dir = %dir, error = %e, "Inbox のディレクトリを消せない"),
            }
        }
    }
}

/// 破棄待ちの件のファイルを消した結果（D-90）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscardOutcome {
    /// 消した音声・同梱ファイルの数とバイト数
    Deleted { files: usize, bytes: u64 },
    /// 走査が写した後にファイルが足された・変わった（何も消していない）。理由
    Changed(String),
}

/// GC が破棄待ちの件のファイルを消す（D-90。物理削除は GC だけ）。消すのは件のディレクトリの直下の
/// **走査が写した音声**（stat が一致するもの）・既知の同梱ファイル（cover 画像 / cue / toc / log）・
/// サイドカーだけで、ディレクトリは空になったときだけ消す。先に全部を確かめ、写した後に音声が足された・
/// 変わったなら何も消さずに [`DiscardOutcome::Changed`]。サブディレクトリ（別の件）と symlink は辿らない。
/// Inbox 直下の件（`rel_dir` が空）は音声だけを消す
pub fn discard_item_files(
    inbox: &RootDir,
    item: &Item,
    files: &[FileRow],
) -> Result<DiscardOutcome, FsError> {
    let dir = if item.rel_dir.is_empty() {
        None
    } else {
        Some(RelPath::parse(&item.rel_dir).map_err(|e| FsError::Io(std::io::Error::other(e)))?)
    };
    let rows: HashMap<String, &FileRow> = files
        .iter()
        .map(|f| (canonical_key(&f.rel_path), f))
        .collect();
    // 1. 確かめる: 直下の音声がすべて写した行にあり、stat が同じ
    let entries = match inbox.read_dir(dir.as_ref()) {
        Ok(e) => e,
        Err(FsError::NotFound) => Vec::new(),
        Err(e) => return Err(e),
    };
    let child = |name: &str| match &dir {
        Some(d) => d.join(name),
        None => RelPath::parse(name),
    };
    let mut audio = Vec::new();
    let mut companions = Vec::new();
    for e in &entries {
        let Some(name) = e.name.to_str() else {
            continue;
        };
        if name.starts_with('.') || e.kind != FileKind::File {
            continue;
        }
        let Ok(rel) = child(name) else {
            continue;
        };
        let is_audio = name
            .rsplit_once('.')
            .and_then(|(_, ext)| Codec::from_extension(ext))
            .is_some();
        if is_audio {
            let st = match inbox.stat(&rel) {
                Ok(st) => st,
                Err(FsError::NotFound) => continue,
                Err(e) => return Err(e),
            };
            match rows.get(&rel.key()) {
                Some(row) if stat_matches(row, &st) => audio.push((rel, st.size)),
                Some(_) => return Ok(DiscardOutcome::Changed(format!("{rel} が変わっている"))),
                None => return Ok(DiscardOutcome::Changed(format!("{rel} が足されている"))),
            }
        } else if dir.is_some() && crate::cd::riplog::is_companion_name(name) {
            companions.push(rel);
        }
    }
    let sidecar = current_sidecar_key(inbox, &item.rel_dir);
    // 2. 消す（1 件ずつ。消す直前にもう一度 stat を照合する）
    let mut out_files = 0;
    let mut bytes = 0;
    for (rel, size) in audio {
        let unchanged = rows
            .get(&rel.key())
            .zip(inbox.stat(&rel).ok())
            .is_some_and(|(row, st)| stat_matches(row, &st));
        if !unchanged {
            tracing::warn!(path = %rel, "破棄の間に変わったので残す");
            continue;
        }
        match inbox.unlink(&rel) {
            Ok(()) => {
                out_files += 1;
                bytes += size;
            }
            Err(FsError::NotFound) => {}
            Err(e) => tracing::warn!(path = %rel, error = %e, "Inbox のファイルを消せない"),
        }
    }
    for rel in companions {
        let size = inbox.stat(&rel).map(|st| st.size).unwrap_or(0);
        match inbox.unlink(&rel) {
            Ok(()) => {
                out_files += 1;
                bytes += size;
            }
            Err(FsError::NotFound) => {}
            Err(e) => tracing::warn!(path = %rel, error = %e, "Inbox の同梱ファイルを消せない"),
        }
    }
    if let Some(d) = &dir {
        // サイドカーは確かめた時と同じもの（別の取り込みが書き直していない）だけ消す
        if let (Some(rel), Some(_)) = (sidecar_rel(&item.rel_dir), sidecar) {
            if current_sidecar_key(inbox, &item.rel_dir) == sidecar {
                match inbox.unlink(&rel) {
                    Ok(()) | Err(FsError::NotFound) => {}
                    Err(e) => tracing::warn!(path = %rel, error = %e, "サイドカーを消せない"),
                }
            } else {
                tracing::warn!(path = %rel, "サイドカーが破棄の間に変わったので残す");
            }
        }
        match inbox.remove_dir(d) {
            Ok(()) | Err(FsError::NotFound) => {}
            Err(FsError::Io(e))
                if e.raw_os_error() == Some(rustix::io::Errno::NOTEMPTY.raw_os_error()) => {}
            Err(e) => tracing::warn!(dir = %d, error = %e, "Inbox のディレクトリを消せない"),
        }
    }
    Ok(DiscardOutcome::Deleted {
        files: out_files,
        bytes,
    })
}

/// approved の件を Library に配置して登録する。`library` の排他は呼び出し側（ハンドラ）が取る
pub async fn place_item(
    env: &PlaceItemEnv,
    item: &Item,
    token: &CancellationToken,
) -> Result<ItemPlaced, InboxError> {
    let draft: InboxDraft = match &item.draft {
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|e| InboxError::Conflict(format!("下書きを読めない: {e}")))?,
        None => return Err(InboxError::Conflict("下書きが無い".into())),
    };
    let item_id = item.id;
    let rows = env.db.read(move |c| dbinbox::files(c, item_id)).await?;
    let names: Vec<String> = rows.iter().map(|f| f.rel_path.clone()).collect();
    draft.validate(&names)?;
    let files: Arc<HashMap<String, FileRow>> = Arc::new(
        rows.into_iter()
            .map(|f| (canonical_key(&f.rel_path), f))
            .collect(),
    );
    // 1. Inbox 側を読む（stat の照合、フィンガープリント、補正）
    let sources: Arc<Vec<Source>> = {
        let (inbox, draft, files) = (Arc::clone(&env.inbox), draft.clone(), Arc::clone(&files));
        Arc::new(
            tokio::task::spawn_blocking(move || read_sources(&inbox, &draft, &files))
                .await
                .map_err(|e| {
                    std::io::Error::other(format!("Inbox の読み取りタスクが異常終了: {e}"))
                })??,
        )
    };
    // 差し替える画像（D-86）。承認後に GC された・置き場が無いなら配置しない（選び直してもらう）
    let pictures: Arc<Vec<Option<lofty::picture::Picture>>> = {
        let (store, draft) = (env.artwork.clone(), draft.clone());
        Arc::new(
            tokio::task::spawn_blocking(move || load_draft_pictures(store.as_deref(), &draft))
                .await
                .map_err(|e| {
                    std::io::Error::other(format!("画像の読み込みタスクが異常終了: {e}"))
                })??,
        )
    };
    // サイドカー（配置の成功で消えるので、消す前に読む）: 購読由来か（P4-16）と、CD の吸い出しの
    // 記録（D-67 追記。名前でトラックへ結びつけ、合わなければ配置しない）
    let (sidecar, sidecar_key): (Option<Sidecar>, SidecarKey) = {
        let (inbox, rel_dir) = (Arc::clone(&env.inbox), item.rel_dir.clone());
        tokio::task::spawn_blocking(move || read_sidecar(&inbox, &rel_dir))
            .await
            .unwrap_or_default()
    };
    let subscription_ids = subscription_ids(sidecar.as_ref());
    let rip: Option<Arc<RipBinding>> = match sidecar.as_ref().and_then(|s| s.rip.as_ref()) {
        Some(entry) => Some(Arc::new(bind_rip(entry, &draft).map_err(|r| {
            InboxError::Conflict(format!("吸い出しの記録と件が合わない: {r}"))
        })?)),
        None => None,
    };
    // 2. 計画
    let plan = {
        let (layout, item, draft, files, sources) = (
            env.layout.clone(),
            item.clone(),
            draft.clone(),
            Arc::clone(&files),
            Arc::clone(&sources),
        );
        env.db
            .read(move |c| {
                let list: Vec<FileRow> = files.values().cloned().collect();
                Ok(plan_item(c, &layout, &item, &draft, &list, &sources))
            })
            .await??
    };
    if token.is_cancelled() {
        return Err(InboxError::Cancelled);
    }
    if let Some(hook) = &env.before_place {
        hook();
    }
    // 3. 配置
    let placed = {
        let (library, inbox, item, plan, files, sources, pictures, draft) = (
            Arc::clone(&env.library),
            Arc::clone(&env.inbox),
            item.clone(),
            plan.clone(),
            Arc::clone(&files),
            Arc::clone(&sources),
            Arc::clone(&pictures),
            draft.clone(),
        );
        tokio::task::spawn_blocking(move || {
            place_files(
                &library, &inbox, &item, &plan, &files, &sources, &pictures, &draft,
            )
        })
        .await
        .map_err(|e| std::io::Error::other(format!("配置タスクが異常終了: {e}")))??
    };
    let placed_new = placed.placed_new.clone();
    let created_top = placed.created_top.clone();
    let companions = placed.companions.clone();
    // 読んだサイドカーが今も同じこと（差し替えられていれば古い記録を登録しない。件は pending に戻る）
    let sidecar_now = {
        let (inbox, rel_dir) = (Arc::clone(&env.inbox), item.rel_dir.clone());
        tokio::task::spawn_blocking(move || current_sidecar_key(&inbox, &rel_dir))
            .await
            .unwrap_or(None)
    };
    if sidecar_now != sidecar_key {
        let library = Arc::clone(&env.library);
        let dir = plan.rel_dir.clone();
        let _ = tokio::task::spawn_blocking(move || {
            cleanup(&library, &dir, &placed_new, created_top.as_ref())
        })
        .await;
        return Err(InboxError::Changed(format!(
            "{}/{}",
            item.rel_dir,
            crate::import::sidecar::SIDECAR_NAME
        )));
    }
    // 検証記録の log_path は移した rip.log の Library 相対（宛先に別の内容があって移せなければ無し）
    let log_rel: Option<String> = rip.as_ref().and_then(|r| {
        companions
            .iter()
            .any(|c| canonical_key(c.file_name()) == canonical_key(&r.entry.log))
            .then(|| format!("{}/{}", plan.rel_dir, r.entry.log))
    });
    // 4. 登録（normalize の投入と件の placed も同じトランザクション）
    let normalize = env.wav_to_flac && env.editor.as_ref().is_some_and(|e| e.can_normalize());
    let registered = {
        let (plan_tx, draft_tx, files_tx, rip_tx) =
            (plan.clone(), draft.clone(), Arc::clone(&files), rip.clone());
        env.db
            .write(move |c| {
                register_item(
                    c,
                    item_id,
                    &plan_tx,
                    &draft_tx,
                    &placed,
                    &files_tx,
                    normalize,
                    rip_tx.as_deref(),
                    log_rel.as_deref(),
                )
            })
            .await
    };
    let registered: Result<RegisteredItem, InboxError> = match registered {
        Ok(inner) => inner,
        Err(e) => Err(InboxError::Db(e)),
    };
    let registered = match registered {
        Ok(r) => r,
        Err(e) => {
            let library = Arc::clone(&env.library);
            let dir = plan.rel_dir.clone();
            let _ = tokio::task::spawn_blocking(move || {
                cleanup(&library, &dir, &placed_new, created_top.as_ref())
            })
            .await;
            return Err(e);
        }
    };
    // 5. Inbox 側を消す
    {
        let (inbox, item, files, sources) = (
            Arc::clone(&env.inbox),
            item.clone(),
            Arc::clone(&files),
            Arc::clone(&sources),
        );
        let _ = tokio::task::spawn_blocking(move || {
            consume_inbox(&inbox, &item, &files, &sources, &companions, sidecar_key)
        })
        .await;
    }
    // 6. 通知（投入は登録のトランザクションで済んでいる）
    env.jobs.notify_enqueued(&registered.job_ids).await;
    if let Some(ev) = registered.batch_event {
        env.jobs.publish(Event::Batch(ev));
    }
    env.jobs.publish(Event::Library(LibraryEvent::Ids {
        scan_run_id: 0,
        track_ids: registered.track_ids.clone(),
    }));
    tracing::info!(
        item_id = item.id,
        album_id = registered.album_id,
        dir = %plan.rel_dir,
        tracks = registered.track_ids.len(),
        "Inbox の件を配置した"
    );
    Ok(ItemPlaced {
        album_id: registered.album_id,
        rel_dir: plan.rel_dir,
        track_ids: registered.track_ids,
        job_ids: registered.job_ids,
        normalize_batch: registered.normalize_batch,
        subscription_ids,
    })
}

/// サイドカーの同一性（inode / size / mtime / ctime）。無ければ None。読んだものが登録まで同じかを
/// 確かめるのに使う（外から tmp + rename で差し替えられたら、古い記録を登録しない。ファイルが正）
type SidecarKey = Option<(u64, u64, i64, i64)>;

fn sidecar_key(st: &crate::fsroot::Stat) -> SidecarKey {
    Some((st.inode, st.size, st.mtime_ns, st.ctime_ns))
}

fn sidecar_rel(rel_dir: &str) -> Option<RelPath> {
    if rel_dir.is_empty() {
        return None;
    }
    RelPath::parse(rel_dir)
        .ok()?
        .join(crate::import::sidecar::SIDECAR_NAME)
        .ok()
}

/// 件のサイドカーと、読んだ FD の同一性。無い・読めない（壊れている）なら内容は None（読めないものは
/// 提案の警告に出ている）で、同一性はパスの stat（無ければ None）
fn read_sidecar(inbox: &RootDir, rel_dir: &str) -> (Option<Sidecar>, SidecarKey) {
    let Some(rel) = sidecar_rel(rel_dir) else {
        return (None, None);
    };
    let mut file = match inbox.open_file(&rel) {
        Ok(f) => f,
        Err(FsError::NotFound) => return (None, None),
        Err(e) => {
            tracing::warn!(path = %rel, error = %e, "サイドカーを開けないので無いものとして配置する");
            return (None, current_sidecar_key(inbox, rel_dir));
        }
    };
    let key = crate::fsroot::fstat(&file)
        .ok()
        .and_then(|st| sidecar_key(&st));
    let mut bytes = Vec::new();
    let parsed = file
        .read_to_end(&mut bytes)
        .map_err(crate::import::sidecar::SidecarError::from)
        .and_then(|_| Sidecar::parse(&bytes));
    match parsed {
        Ok(s) => (Some(s), key),
        Err(e) => {
            tracing::warn!(path = %rel, error = %e, "サイドカーを読めないので無いものとして配置する");
            (None, key)
        }
    }
}

/// いまのサイドカーの同一性（パスの stat）
fn current_sidecar_key(inbox: &RootDir, rel_dir: &str) -> SidecarKey {
    let rel = sidecar_rel(rel_dir)?;
    inbox.stat(&rel).ok().and_then(|st| sidecar_key(&st))
}

/// サイドカーの項にある購読 id（昇順・重複なし）
fn subscription_ids(sidecar: Option<&Sidecar>) -> Vec<i64> {
    let mut ids: Vec<i64> = sidecar
        .map(|s| s.files.values().filter_map(|e| e.subscription_id).collect())
        .unwrap_or_default();
    ids.sort_unstable();
    ids.dedup();
    ids
}

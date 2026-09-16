//! パス生成: テンプレート展開・ファイル名正規化・切り詰め・衝突降格
//! （SPEC §5「パステンプレート」「ファイル名正規化」、D-7、docs/TASKS.md P0-11）。
//!
//! - タグ値は NFC のまま保持し、置換テーブルは**ファイル名にだけ**適用する
//! - 置換テーブルは ytmusic の foo_fileops 互換表を継承し、SPEC の追加分と、表に無い
//!   SMB / exFAT 禁止文字の全角化（`/ \ | "`）を加える。制御文字は落とす
//! - 各要素は 255 バイト以下、パス全体は 240 UTF-16 単位以下。超えたら省略記号付きで
//!   切り詰める（要素はその要素、全体はファイル名の stem）
//! - 衝突時のみ `{album}` → `{album} ({year})` → `{album} ({edition})` に降格する。
//!   衝突の単位は**リリース**（MB Release ID / DiscID / album 行）であり、同名 ≠ 同一リリース。
//!   降格しても解決しなければマージせず conflict として報告する
//!
//! DB には依存しない。テンプレートの選択（single / multi / unsorted）と値の取得は呼び出し側

use std::collections::{HashMap, HashSet};
use std::fmt;

use unicode_normalization::UnicodeNormalization;

use super::relpath::{canonical_key, RelPath, RelPathError};

/// 各要素の上限（バイト。ZFS / SMB）
pub const MAX_COMPONENT_BYTES: usize = 255;
/// パス全体の上限（UTF-16 単位。Windows / Android 互換）
pub const MAX_PATH_UTF16: usize = 240;
/// 切り詰めに使う省略記号
const ELLIPSIS: char = '…';

/// 値が無いときのフォールバック
pub const UNKNOWN_ARTIST: &str = "Unknown Artist";
pub const UNKNOWN_ALBUM: &str = "Unknown Album";

/// ytmusic の foo_fileops 互換置換テーブル + SPEC §5 の追加分。順に適用する
pub const REPLACEMENTS: &[(char, char)] = &[
    ('~', '～'),
    ('*', '＊'),
    ('∕', '／'),
    (':', '：'),
    ('>', '＞'),
    ('<', '＜'),
    ('?', '？'),
    ('Ø', 'O'),
    ('À', 'A'),
    ('ô', 'o'),
    ('è', 'e'),
    ('é', 'e'),
    ('ë', 'e'),
    ('ゔ', 'う'),
    // 表に無い SMB / exFAT 禁止文字。表と同じ流儀で全角にする
    ('/', '／'),
    ('\\', '＼'),
    ('|', '｜'),
    ('"', '＂'),
];

/// Windows の予約デバイス名（`relpath` と同じ）
const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    #[error("テンプレートが空")]
    Empty,
    #[error("`{{` が閉じていない")]
    Unclosed,
    #[error("未知のプレースホルダ: {0}")]
    UnknownField(String),
    #[error("書式が不正: {0}")]
    BadFormat(String),
    #[error("空の要素を含む")]
    EmptyComponent,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    #[error("降格に必要な値が無い: {0}")]
    MissingVariantValue(&'static str),
    #[error("切り詰めても上限に収まらない（{0}）")]
    TooLong(&'static str),
    #[error("生成したパスが不正: {0}")]
    RelPath(#[from] RelPathError),
}

/// プレースホルダ
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Category,
    AlbumArtist,
    Artist,
    Album,
    Title,
    Disc,
    Track,
    Year,
    Edition,
}

impl Field {
    /// 0 埋め（`:0N`）を受け付ける整数フィールド
    fn is_integer(self) -> bool {
        matches!(self, Field::Disc | Field::Track)
    }

    fn parse(name: &str) -> Option<Field> {
        Some(match name {
            "category" => Field::Category,
            "albumartist" => Field::AlbumArtist,
            "artist" => Field::Artist,
            "album" => Field::Album,
            "title" => Field::Title,
            "disc" => Field::Disc,
            "track" => Field::Track,
            "year" => Field::Year,
            "edition" => Field::Edition,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Literal(String),
    Field {
        field: Field,
        /// `{track:02}` の 0 埋め幅。整数フィールド以外では無視
        pad: usize,
    },
}

/// 解析済みテンプレート。`/` 区切りの要素ごとに piece の列を持つ
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    components: Vec<Vec<Piece>>,
}

impl Template {
    pub fn parse(s: &str) -> Result<Template, TemplateError> {
        if s.is_empty() {
            return Err(TemplateError::Empty);
        }
        let mut components = Vec::new();
        for comp in s.split('/') {
            if comp.is_empty() {
                return Err(TemplateError::EmptyComponent);
            }
            components.push(parse_component(comp)?);
        }
        Ok(Template { components })
    }

    /// `fields` で展開し、正規化・切り詰めを経た root 相対パスを返す
    pub fn render(&self, f: &TrackFields, variant: AlbumVariant) -> Result<RelPath, RenderError> {
        let mut comps: Vec<String> = Vec::with_capacity(self.components.len());
        for pieces in &self.components {
            let mut s = String::new();
            for p in pieces {
                match p {
                    Piece::Literal(l) => s.push_str(l),
                    Piece::Field { field, pad } => s.push_str(&f.value(*field, *pad, variant)?),
                }
            }
            comps.push(sanitize_component(&s));
        }
        // 拡張子はテンプレートに含めない（形式はファイルが決める）
        let last = comps.len() - 1;
        let ext = sanitize_component(&f.ext);
        // 切り詰めた stem には最低 1 文字 + 省略記号を残す
        let min_stem_bytes = 1 + ELLIPSIS.len_utf8();
        let min_stem_units = 1 + ELLIPSIS.len_utf16();
        for (i, c) in comps.iter_mut().enumerate() {
            let budget = if i == last {
                let budget = MAX_COMPONENT_BYTES.saturating_sub(ext.len() + 1);
                if budget < min_stem_bytes {
                    return Err(RenderError::TooLong("拡張子が長すぎる"));
                }
                budget
            } else {
                MAX_COMPONENT_BYTES
            };
            if !truncate_bytes(c, budget) {
                return Err(RenderError::TooLong("要素を切り詰めても元の文字が残らない"));
            }
        }
        let mut path = comps.join("/");
        path.push('.');
        path.push_str(&ext);
        let over = path.encode_utf16().count().saturating_sub(MAX_PATH_UTF16);
        if over > 0 {
            let stem_units = comps[last].encode_utf16().count();
            if stem_units.saturating_sub(over) < min_stem_units {
                return Err(RenderError::TooLong(
                    "ディレクトリだけでパス長の上限を超える",
                ));
            }
            if !truncate_utf16(&mut comps[last], stem_units - over) {
                return Err(RenderError::TooLong(
                    "ファイル名を切り詰めても元の文字が残らない",
                ));
            }
            path = comps.join("/");
            path.push('.');
            path.push_str(&ext);
        }
        if path.encode_utf16().count() > MAX_PATH_UTF16 {
            return Err(RenderError::TooLong("パス全体"));
        }
        Ok(RelPath::parse(&path)?)
    }
}

fn parse_component(comp: &str) -> Result<Vec<Piece>, TemplateError> {
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut rest = comp;
    while let Some(start) = rest.find('{') {
        literal.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            return Err(TemplateError::Unclosed);
        };
        let spec = &after[..end];
        let (name, pad) = match spec.split_once(':') {
            Some((n, fmt)) => (n, parse_pad(fmt)?),
            None => (spec, 0),
        };
        let field =
            Field::parse(name).ok_or_else(|| TemplateError::UnknownField(name.to_owned()))?;
        if pad > 0 && !field.is_integer() {
            return Err(TemplateError::BadFormat(format!(
                "{name} は整数フィールドではないので 0 埋めできない"
            )));
        }
        if !literal.is_empty() {
            pieces.push(Piece::Literal(std::mem::take(&mut literal)));
        }
        pieces.push(Piece::Field { field, pad });
        rest = &after[end + 1..];
    }
    if rest.contains('}') {
        return Err(TemplateError::BadFormat(rest.to_owned()));
    }
    literal.push_str(rest);
    if !literal.is_empty() {
        pieces.push(Piece::Literal(literal));
    }
    Ok(pieces)
}

/// `02` のような 0 埋め幅（`0N`、N は 1 以上の整数）
fn parse_pad(fmt: &str) -> Result<usize, TemplateError> {
    let bad = || TemplateError::BadFormat(format!("0 埋めは `0N` の形: {fmt:?}"));
    let digits = fmt.strip_prefix('0').ok_or_else(bad)?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(bad());
    }
    let n: usize = digits.parse().map_err(|_| bad())?;
    if n == 0 {
        return Err(bad());
    }
    Ok(n)
}

/// `{album}` の降格段階（D-7）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlbumVariant {
    Plain,
    WithYear,
    WithEdition,
}

impl AlbumVariant {
    fn next(self) -> Option<AlbumVariant> {
        match self {
            AlbumVariant::Plain => Some(AlbumVariant::WithYear),
            AlbumVariant::WithYear => Some(AlbumVariant::WithEdition),
            AlbumVariant::WithEdition => None,
        }
    }
}

/// テンプレートに与える 1 トラック分の値。値は NFC 済みのタグ値（正規化はここではしない）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackFields {
    pub category: Option<String>,
    pub albumartist: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub title: Option<String>,
    pub disc_no: Option<i64>,
    pub track_no: Option<i64>,
    /// 発売年（4 桁）。`{year}` と降格に使う
    pub year: Option<String>,
    pub edition: Option<String>,
    /// 拡張子（ドットなし）
    pub ext: String,
    /// 現在のファイル名（拡張子なし）。title が無いときのフォールバック
    pub stem: String,
}

impl TrackFields {
    fn value(
        &self,
        field: Field,
        pad: usize,
        variant: AlbumVariant,
    ) -> Result<String, RenderError> {
        let int = |v: Option<i64>| format!("{:0width$}", v.unwrap_or(0), width = pad);
        Ok(match field {
            Field::Category => self.category.clone().unwrap_or_default(),
            Field::AlbumArtist => self
                .albumartist
                .clone()
                .or_else(|| self.artist.clone())
                .unwrap_or_else(|| UNKNOWN_ARTIST.to_owned()),
            Field::Artist => self
                .artist
                .clone()
                .or_else(|| self.albumartist.clone())
                .unwrap_or_else(|| UNKNOWN_ARTIST.to_owned()),
            Field::Album => {
                let album = self
                    .album
                    .clone()
                    .unwrap_or_else(|| UNKNOWN_ALBUM.to_owned());
                match variant {
                    AlbumVariant::Plain => album,
                    AlbumVariant::WithYear => {
                        let year = self
                            .year
                            .as_deref()
                            .filter(|y| !y.is_empty())
                            .ok_or(RenderError::MissingVariantValue("year"))?;
                        format!("{album} ({year})")
                    }
                    AlbumVariant::WithEdition => {
                        let edition = self
                            .edition
                            .as_deref()
                            .filter(|e| !e.is_empty())
                            .ok_or(RenderError::MissingVariantValue("edition"))?;
                        format!("{album} ({edition})")
                    }
                }
            }
            Field::Title => self.title.clone().unwrap_or_else(|| self.stem.clone()),
            Field::Disc => int(self.disc_no.or(Some(1))),
            Field::Track => int(self.track_no),
            Field::Year => self.year.clone().unwrap_or_default(),
            Field::Edition => self.edition.clone().unwrap_or_default(),
        })
    }
}

// ---------------------------------------------------------------- 正規化

/// 1 要素（ディレクトリ名またはファイル名）をファイル名として安全な形にする。
/// NFC → 置換テーブル → 制御文字除去 → 末尾のドット・スペース除去 → 予約名回避 → 空なら `_`。
/// 長さの切り詰めはしない（[`Template::render`] が拡張子込みで行う）
pub fn sanitize_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.nfc() {
        if c.is_control() {
            continue;
        }
        match REPLACEMENTS.iter().find(|(from, _)| *from == c) {
            Some((_, to)) => out.push(*to),
            None => out.push(c),
        }
    }
    let trimmed = out.trim_end_matches(['.', ' ']);
    let mut out = trimmed.to_owned();
    if out.is_empty() {
        return "_".to_owned();
    }
    let stem_len = out.find('.').unwrap_or(out.len());
    let stem = &out[..stem_len];
    if RESERVED_NAMES.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        out.insert(stem_len, '_');
    }
    out
}

/// `s` を `max` バイト以下に切り詰める（超えていれば末尾を省略記号にする）。
/// 元の文字が 1 つも残らなければ `false`（切り詰めでは上限を満たせない）
fn truncate_bytes(s: &mut String, max: usize) -> bool {
    if s.len() <= max {
        return true;
    }
    let budget = max.saturating_sub(ELLIPSIS.len_utf8());
    let mut cut = budget;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    trim_for_ellipsis(s);
    if s.is_empty() {
        return false;
    }
    s.push(ELLIPSIS);
    true
}

/// `s` を `max` UTF-16 単位以下に切り詰める。元の文字が 1 つも残らなければ `false`
fn truncate_utf16(s: &mut String, max: usize) -> bool {
    if s.encode_utf16().count() <= max {
        return true;
    }
    let budget = max.saturating_sub(ELLIPSIS.len_utf16());
    let mut units = 0;
    let mut cut = 0;
    for (i, c) in s.char_indices() {
        if units + c.len_utf16() > budget {
            break;
        }
        units += c.len_utf16();
        cut = i + c.len_utf8();
    }
    s.truncate(cut);
    trim_for_ellipsis(s);
    if s.is_empty() {
        return false;
    }
    s.push(ELLIPSIS);
    true
}

/// 省略記号の前に末尾のスペース・ドットを残さない（SMB 制約は `…` が末尾なので満たす）
fn trim_for_ellipsis(s: &mut String) {
    let t = s.trim_end_matches(['.', ' ']).len();
    s.truncate(t);
}

// ---------------------------------------------------------------- 衝突降格

/// 計画の入力 1 件
#[derive(Debug, Clone)]
pub struct PlanItem {
    pub track_id: i64,
    pub template: Template,
    pub fields: TrackFields,
    /// リリースの同一性キー（`mb:<id>` / `disc:<id>` / `album:<album_id>` など）。
    /// 同じキー = 同一リリース。同名でもキーが違えば別リリース（D-7）
    pub release: String,
    /// 現在の rel_path
    pub current_rel_path: String,
}

/// 選択に含まれない既存トラックの占有状況（衝突判定の相手）
#[derive(Debug, Clone, Default)]
pub struct Occupancy {
    /// ディレクトリ key → そこにある（選択外の）トラックのリリースキー集合
    pub dir_releases: HashMap<String, HashSet<String>>,
    /// 選択外のトラックが持つ rel_path_key
    pub path_keys: HashSet<String>,
}

/// 計画の結果 1 件
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Planned {
    /// この rel_path へ移す
    Path(RelPath),
    /// 現在のパスと同じ（op にしない）
    Unchanged,
    /// 衝突・生成不能。理由付きで報告する（ファイルも DB も触らない）
    Conflict(String),
}

impl fmt::Display for Planned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Planned::Path(p) => write!(f, "{p}"),
            Planned::Unchanged => f.write_str("(unchanged)"),
            Planned::Conflict(r) => write!(f, "(conflict: {r})"),
        }
    }
}

struct Draft {
    variant: AlbumVariant,
    path: Option<RelPath>,
    error: Option<String>,
}

/// 全件のパスを決め、ディレクトリ衝突を降格で解き、ファイル名衝突を報告する。
/// 入力と同じ順で返す
pub fn plan(items: &[PlanItem], occ: &Occupancy) -> Vec<Planned> {
    let mut drafts: Vec<Draft> = items
        .iter()
        .map(|_| Draft {
            variant: AlbumVariant::Plain,
            path: None,
            error: None,
        })
        .collect();
    // 降格は高々 2 段。各段で全件を展開し直してディレクトリ衝突を見る
    for _round in 0..=2 {
        for (item, d) in items.iter().zip(drafts.iter_mut()) {
            if d.error.is_some() {
                continue;
            }
            match item.template.render(&item.fields, d.variant) {
                Ok(p) => d.path = Some(p),
                Err(e) => {
                    d.path = None;
                    d.error = Some(format!("パスを生成できない: {e}"));
                }
            }
        }
        // ディレクトリ key → 選択内のリリース集合（宛先）
        let mut dir_releases: HashMap<String, HashSet<&str>> = HashMap::new();
        for (item, d) in items.iter().zip(drafts.iter()) {
            if let Some(p) = &d.path {
                dir_releases
                    .entry(dir_key(p))
                    .or_default()
                    .insert(item.release.as_str());
            }
        }
        // ディレクトリ key → 既にそこにいるリリース（選択外の占有者 + 選択内で現在そこにいるもの）。
        // 自分のリリースだけが既にいるディレクトリへの移動は合流であり衝突ではない
        let mut incumbents: HashMap<String, HashSet<&str>> = HashMap::new();
        for (key, rel) in &occ.dir_releases {
            incumbents
                .entry(key.clone())
                .or_default()
                .extend(rel.iter().map(String::as_str));
        }
        for item in items {
            if let Ok(cur) = RelPath::parse(&item.current_rel_path) {
                incumbents
                    .entry(dir_key(&cur))
                    .or_default()
                    .insert(item.release.as_str());
            }
        }
        let mut demoted = false;
        for (item, d) in items.iter().zip(drafts.iter_mut()) {
            let Some(p) = &d.path else { continue };
            let key = dir_key(p);
            let incumbent = incumbents.get(&key);
            let mut releases: HashSet<&str> = dir_releases[&key].clone();
            if let Some(o) = incumbent {
                releases.extend(o.iter().copied());
            }
            if releases.len() <= 1 {
                continue;
            }
            let joins_own =
                incumbent.is_some_and(|o| o.len() == 1 && o.contains(item.release.as_str()));
            if joins_own {
                continue;
            }
            match d.variant.next() {
                Some(v) => {
                    d.variant = v;
                    demoted = true;
                }
                None => {
                    d.path = None;
                    d.error =
                        Some("同名の別リリースと衝突（年・edition でも解決しない）".to_owned());
                }
            }
        }
        if !demoted {
            break;
        }
    }

    // ファイル名の衝突: 選択内で同じ key、または選択外の占有
    let mut target_count: HashMap<String, usize> = HashMap::new();
    for d in &drafts {
        if let Some(p) = &d.path {
            *target_count.entry(p.key()).or_default() += 1;
        }
    }
    items
        .iter()
        .zip(drafts)
        .map(|(item, d)| {
            if let Some(e) = d.error {
                return Planned::Conflict(e);
            }
            let Some(p) = d.path else {
                return Planned::Conflict("パスを生成できない".to_owned());
            };
            let key = p.key();
            if target_count.get(&key).copied().unwrap_or(0) > 1 {
                return Planned::Conflict(format!("宛先が選択内の別のトラックと重複: {p}"));
            }
            if occ.path_keys.contains(&key) {
                return Planned::Conflict(format!("宛先を別のトラックが占有: {p}"));
            }
            if p.as_str() == item.current_rel_path {
                return Planned::Unchanged;
            }
            Planned::Path(p)
        })
        .collect()
}

/// 親ディレクトリの canonical key（root 直下なら空）
fn dir_key(p: &RelPath) -> String {
    p.parent().map(|d| d.key()).unwrap_or_default()
}

/// 現在のパスの key（呼び出し側が [`Occupancy`] を作るときに使う）
pub fn path_key(rel_path: &str) -> String {
    canonical_key(rel_path)
}

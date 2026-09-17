//! m3u8 の生成とパス写像（SPEC §10「パスマッピング」、D-53）。
//!
//! 行の `path` は media root の名前付き相対パス（`Library/…` / `Derived/…`。`delivery` ビューの
//! `path` と同じ形）。書き出し先 `Playlists/<profile>/` は `Library/` と同じ深さなので、相対
//! プロファイルは `../../` を前置するだけで解決する。UTF-8 / BOM なし

/// 書き出しファイルの拡張子。プレイリスト名の長さの検証はこれを含めて行う（D-53）
pub const EXPORT_EXT: &str = ".m3u8";

/// `export_profiles` の 1 行のうち m3u8 の生成に要る部分
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportProfile {
    pub name: String,
    pub source: Source,
    pub path_style: PathStyle,
    pub path_prefix: Option<String>,
    pub path_sep: String,
}

/// `master`（Library 原本）/ `delivery`（Derived 優先の配布ビュー）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Master,
    Delivery,
}

impl Source {
    pub fn parse(s: &str) -> Option<Source> {
        match s {
            "master" => Some(Source::Master),
            "delivery" => Some(Source::Delivery),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Source::Master => "master",
            Source::Delivery => "delivery",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathStyle {
    Relative,
    Absolute,
}

impl PathStyle {
    pub fn parse(s: &str) -> Option<PathStyle> {
        match s {
            "relative" => Some(PathStyle::Relative),
            "absolute" => Some(PathStyle::Absolute),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PathStyle::Relative => "relative",
            PathStyle::Absolute => "absolute",
        }
    }
}

/// 書き出す 1 行
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportTrack {
    /// `Library/<rel_path>` または `Derived/<rel_path>`
    pub path: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub duration_ms: Option<i64>,
}

/// `path` をプロファイルの流儀に写す
pub fn map_path(profile: &ExportProfile, path: &str) -> String {
    let mut out = match profile.path_style {
        PathStyle::Relative => format!("../../{path}"),
        PathStyle::Absolute => format!("{}{path}", profile.path_prefix.as_deref().unwrap_or("")),
    };
    if profile.path_sep != "/" {
        // prefix は既にプロファイルの区切りで書かれている。写すのは path の部分だけ
        let head = match profile.path_style {
            PathStyle::Absolute => profile.path_prefix.as_deref().unwrap_or("").len(),
            PathStyle::Relative => 0,
        };
        let tail = out.split_off(head).replace('/', &profile.path_sep);
        out.push_str(&tail);
    }
    out
}

/// `#EXTM3U` + 各行 `#EXTINF:<秒>,<Artist - Title>` + パス。末尾は改行
pub fn render_m3u8(profile: &ExportProfile, tracks: &[ExportTrack]) -> String {
    let mut out = String::from("#EXTM3U\n");
    for t in tracks {
        let secs = t
            .duration_ms
            .map(|ms| (ms as f64 / 1000.0).round() as i64)
            .unwrap_or(-1);
        out.push_str(&format!("#EXTINF:{secs},{}\n", display_name(t)));
        out.push_str(&map_path(profile, &t.path));
        out.push('\n');
    }
    out
}

/// EXTINF の表示名。タイトルが無ければファイル名の stem。改行は 1 行に潰す
fn display_name(t: &ExportTrack) -> String {
    let title = match t.title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(title) => title.to_owned(),
        None => {
            let name = t.path.rsplit('/').next().unwrap_or(&t.path);
            name.rsplit_once('.')
                .map(|(s, _)| s)
                .unwrap_or(name)
                .to_owned()
        }
    };
    let name = match t.artist.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(artist) => format!("{artist} - {title}"),
        None => title,
    };
    name.replace(['\r', '\n'], " ")
}

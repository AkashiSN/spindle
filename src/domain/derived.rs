//! Derived（配布用の非可逆。系統ごとに 1 本）の判定（SPEC §6「版管理」/ §7.6、D-8 / D-22 / D-25 /
//! D-51 / D-75）。
//!
//! ここは純粋関数だけ。DB 越しの判定と投入は `db::derived`、実際の生成は
//! `jobs::handlers::transcode` にある。
//!
//! - 系統（[`Variant`]）は `opus`（Android の同期・Web 再生・配布ビュー）と `aac`（Apple 向け。P4-8）
//! - `opus` の対象は Library 内の可逆で active（missing でない）な 1ch / 2ch のトラック。非可逆は
//!   原本をそのまま配る（D-8）。マルチチャンネルとチャンネル数不明は作らない（D-22 / D-51）
//! - Derived のパスは `<variant>/` 以下に Library と完全ミラーで、拡張子だけ系統のもの
//! - 系統の設定（[`VariantSettings`]）が off なら凍結（何もしない）。音声版か `audio_profile` が
//!   違えば再エンコード、パスが違えば rename、タグ版・埋めた画像・RG の解析世代・`tag_profile` が
//!   違えばタグの上書きだけ。この順で判定し、再エンコードは残り全部を兼ねる

use lofty::picture::Picture;
use serde::{Deserialize, Serialize};

use crate::domain::replaygain::{tag_changes, Values};
use crate::domain::tags::{Codec, TransferTags};

/// Derived の系統（SPEC §7.6、D-75）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Variant {
    /// Android の同期・Web 再生・配布ビュー
    Opus,
    /// Apple 向け（ミュージック.app に取り込む。P4-8）
    Aac,
}

impl Variant {
    pub const ALL: [Variant; 2] = [Variant::Opus, Variant::Aac];

    pub fn as_str(self) -> &'static str {
        match self {
            Variant::Opus => "opus",
            Variant::Aac => "aac",
        }
    }

    pub fn parse(s: &str) -> Option<Variant> {
        match s {
            "opus" => Some(Variant::Opus),
            "aac" => Some(Variant::Aac),
            _ => None,
        }
    }

    /// `Derived/` 直下のディレクトリ名
    pub fn dir(self) -> &'static str {
        self.as_str()
    }

    /// 成果物の拡張子
    pub fn ext(self) -> &'static str {
        match self {
            Variant::Opus => "opus",
            Variant::Aac => "m4a",
        }
    }

    /// `derived_files.codec` に書く値
    pub fn codec(self) -> &'static str {
        self.as_str()
    }
}

impl std::fmt::Display for Variant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 系統の設定（`config.toml` の `[encode.derived.<variant>]` を `derived_variants` 表に写したもの）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantSettings {
    pub variant: Variant,
    /// off なら凍結: 新しく作らず、既存の行とファイルも触らない
    pub enabled: bool,
    /// 音声に効く設定の世代。行の値と違えば再エンコード
    pub audio_profile: String,
    /// タグに効く設定の世代。行の値と違えばタグ上書き
    pub tag_profile: String,
}

/// opus 系統の設定の世代。`opusenc --vbr --music --bitrate <bitrate>`（引数を変えるときは版を上げる）
pub fn opus_profiles(bitrate: u32) -> (String, String) {
    (format!("opus:{bitrate}:v1"), "opus:v1".to_owned())
}

/// Library の `rel_path` に対応する Derived の `rel_path`（`<variant>/` 以下に、末尾要素の拡張子を
/// 系統のものに替える。拡張子が無ければ付ける）
pub fn expected_rel_path(variant: Variant, library_rel_path: &str) -> String {
    let (dir, name) = match library_rel_path.rsplit_once('/') {
        Some((d, n)) => (Some(d), n),
        None => (None, library_rel_path),
    };
    let stem = match name.rsplit_once('.') {
        Some((s, _)) if !s.is_empty() => s,
        _ => name,
    };
    let ext = variant.ext();
    match dir {
        Some(d) => format!("{}/{d}/{stem}.{ext}", variant.dir()),
        None => format!("{}/{stem}.{ext}", variant.dir()),
    }
}

/// 判定に要るトラック側の現在値
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub track_id: i64,
    pub lossless: bool,
    pub missing: bool,
    pub channels: Option<i64>,
    pub library_rel_path: String,
    pub audio_version: i64,
    pub tag_version: i64,
    /// 埋める画像: トラック自身の `artwork_id`、無ければ所属 album の `artwork_id`（D-61。どちらも
    /// 無ければ None）
    pub artwork_id: Option<i64>,
    /// `tracks.rg_scanned_at`（未解析なら None）
    pub rg_scanned_at: Option<i64>,
}

/// `derived_files` の現在の行
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Current {
    pub rel_path: String,
    pub src_audio_version: i64,
    pub src_tag_version: i64,
    pub src_artwork_id: Option<i64>,
    /// 書いた R128_* の元になった `rg_scanned_at`
    pub src_rg_scanned_at: Option<i64>,
    /// 作ったときの設定の世代（D-75）
    pub audio_profile: String,
    pub tag_profile: String,
}

/// トラックに対して行う処理
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// 対象外（非可逆 / missing / マルチチャンネル）か系統が凍結（off）。Derived があっても触らない
    Skip,
    /// 再エンコード（無い・音声版が古い）。パスとタグも同時に揃う
    Encode,
    /// パスだけ違う
    Move,
    /// タグ版・画像・RG の解析世代だけ違う
    Retag,
    /// パスとタグの両方
    MoveAndRetag,
    /// 何もしない
    UpToDate,
}

impl Plan {
    /// ジョブを投入する必要があるか
    pub fn needs_job(self) -> bool {
        !matches!(self, Plan::Skip | Plan::UpToDate)
    }
}

/// その系統の Derived を作る対象か。チャンネル数が不明（None）なのは属性を読めなかったファイルで、
/// マルチチャンネルかもしれないので対象にしない（deep scan で埋まってから）。`aac` 系統の対象は
/// P4-8 で決める（それまでは無し）
pub fn eligible(variant: Variant, t: &Target) -> bool {
    match variant {
        Variant::Opus => t.lossless && !t.missing && matches!(t.channels, Some(1 | 2)),
        Variant::Aac => false,
    }
}

pub fn plan(s: &VariantSettings, t: &Target, current: Option<&Current>) -> Plan {
    if !s.enabled || !eligible(s.variant, t) {
        return Plan::Skip;
    }
    let Some(c) = current else {
        return Plan::Encode;
    };
    if c.src_audio_version != t.audio_version || c.audio_profile != s.audio_profile {
        return Plan::Encode;
    }
    let moved = c.rel_path != expected_rel_path(s.variant, &t.library_rel_path);
    let retag = c.src_tag_version != t.tag_version
        || c.src_artwork_id != t.artwork_id
        || c.src_rg_scanned_at != t.rg_scanned_at
        || c.tag_profile != s.tag_profile;
    match (moved, retag) {
        (true, true) => Plan::MoveAndRetag,
        (true, false) => Plan::Move,
        (false, true) => Plan::Retag,
        (false, false) => Plan::UpToDate,
    }
}

/// Opus に書くタグ集合。Library のタグをそのまま写し、RG 系のキーは全部落として DB の解析値から
/// `R128_*` を入れ直す（SPEC §7.6「再解析しない」。未解析なら RG は書かない）。画像は album の
/// カバー 1 枚だけ（トラック自身の埋め込み画像は使わない。D-51）
pub fn opus_tags(
    src: &TransferTags,
    rg: Option<&Values>,
    reference: f64,
    cover: Option<Picture>,
) -> TransferTags {
    let mut items: Vec<(String, String)> = src
        .items
        .iter()
        .filter(|(k, _)| !k.starts_with("REPLAYGAIN_") && !k.starts_with("R128_"))
        .cloned()
        .collect();
    if let Some(v) = rg {
        for c in tag_changes(Codec::Opus, v, reference) {
            if let Some(values) = c.values {
                for value in values {
                    items.push((c.key.clone(), value));
                }
            }
        }
    }
    TransferTags {
        items,
        pictures: cover.into_iter().collect(),
    }
}

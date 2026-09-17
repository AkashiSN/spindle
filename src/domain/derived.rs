//! Derived（配布用 Opus）の判定（SPEC §6「版管理」/ §7.6、D-8 / D-22 / D-25 / D-51）。
//!
//! ここは純粋関数だけ。DB 越しの判定と投入は `db::derived`、実際の生成は
//! `jobs::handlers::transcode` にある。
//!
//! - 対象は Library 内の可逆で active（missing でない）な 1ch / 2ch のトラック。非可逆は原本を
//!   そのまま配る（D-8）。マルチチャンネルとチャンネル数不明は作らない（D-22 / D-51）
//! - Derived のパスは Library と完全ミラーで、拡張子だけ `.opus`
//! - 音声版が違えば再エンコード、パスが違えば rename、タグ版・埋めた画像・RG の解析世代が
//!   違えばタグの上書きだけ。この順で判定し、再エンコードは残り全部を兼ねる

use lofty::picture::Picture;

use crate::domain::replaygain::{tag_changes, Values};
use crate::domain::tags::{Codec, TransferTags};

/// Library の `rel_path` に対応する Derived の `rel_path`（末尾要素の拡張子を `.opus` に替える。
/// 拡張子が無ければ付ける）
pub fn expected_rel_path(library_rel_path: &str) -> String {
    let (dir, name) = match library_rel_path.rsplit_once('/') {
        Some((d, n)) => (Some(d), n),
        None => (None, library_rel_path),
    };
    let stem = match name.rsplit_once('.') {
        Some((s, _)) if !s.is_empty() => s,
        _ => name,
    };
    match dir {
        Some(d) => format!("{d}/{stem}.opus"),
        None => format!("{stem}.opus"),
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
    /// 所属 album の `artwork_id`（album が無い・画像が無ければ None）
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
}

/// トラックに対して行う処理
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// 対象外（非可逆 / missing / マルチチャンネル）。Derived があっても触らない
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

/// Derived を作る対象か。チャンネル数が不明（None）なのは属性を読めなかったファイルで、
/// マルチチャンネルかもしれないので対象にしない（deep scan で埋まってから）
pub fn eligible(t: &Target) -> bool {
    t.lossless && !t.missing && matches!(t.channels, Some(1 | 2))
}

pub fn plan(t: &Target, current: Option<&Current>) -> Plan {
    if !eligible(t) {
        return Plan::Skip;
    }
    let Some(c) = current else {
        return Plan::Encode;
    };
    if c.src_audio_version != t.audio_version {
        return Plan::Encode;
    }
    let moved = c.rel_path != expected_rel_path(&t.library_rel_path);
    let retag = c.src_tag_version != t.tag_version
        || c.src_artwork_id != t.artwork_id
        || c.src_rg_scanned_at != t.rg_scanned_at;
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

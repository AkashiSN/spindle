//! 端末への配信の純粋関数（docs/superpowers/specs/2026-09-29-device-delivery-design.md ③、D-95）。
//! DB も HTTP も知らない。入力は `db::devices` が組み立てる。
//!
//! トークンはすべて不透明な値（解析しない）。構成要素を正準 JSON（serde_json の Map は BTreeMap なので
//! キーは昇順、`to_vec` は空白を入れない）にした SHA-256 の 16 進小文字。スキーマ版 `v` を必ず含める

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::domain::derived::{expected_rel_path, Current, Variant, VariantSettings};

/// トークンのスキーマ版
pub const TOKEN_VERSION: u32 = 1;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 正準 JSON の SHA-256。`Value` の直列化は失敗しない（Map のキーは文字列）ので、万一の失敗は空列の
/// ハッシュに倒す（呼び出し側は結果をトークンとして比較するだけ）
pub fn canonical_sha256(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    sha256_hex(&bytes)
}

/// 原本（Library のファイル）の意味トークン
pub fn semantic_master(audio_version: i64, tag_version: i64) -> String {
    canonical_sha256(&json!({
        "v": TOKEN_VERSION, "kind": "m",
        "audio_version": audio_version, "tag_version": tag_version,
    }))
}

/// Derived の意味トークン。**行に記録された**版（ファイルの中身を作った時の値）から作る。
/// `rel_path` は中身ではないので含めない
pub fn semantic_derived(variant: Variant, row: &Current) -> String {
    canonical_sha256(&json!({
        "v": TOKEN_VERSION, "kind": "d", "variant": variant.as_str(),
        "src_audio_version": row.src_audio_version,
        "src_tag_version": row.src_tag_version,
        "src_artwork_id": row.src_artwork_id,
        "src_rg_scanned_at": row.src_rg_scanned_at,
        "audio_profile": row.audio_profile,
        "tag_profile": row.tag_profile,
    }))
}

/// 配信トークン: 端末の中身の同一性と HTTP の強い ETag はこれで判定する
pub fn delivery_token(semantic: &str, sha256: &str) -> String {
    canonical_sha256(&json!({
        "v": TOKEN_VERSION, "kind": "x", "semantic": semantic, "sha256": sha256,
    }))
}

/// プレイリストのトークン（`content_sha256` は書き出したバイト列のハッシュ）
pub fn playlist_token(playlist_id: i64, name: &str, content_sha256: &str) -> String {
    canonical_sha256(&json!({
        "v": TOKEN_VERSION, "kind": "p", "playlist_id": playlist_id,
        "name": name, "sha256": content_sha256,
    }))
}

/// 送る元の種類（`source_hashes.source` の値）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SourceKind {
    Master,
    Derived(Variant),
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Master => "master",
            SourceKind::Derived(v) => v.as_str(),
        }
    }

    pub fn parse(s: &str) -> Option<SourceKind> {
        match s {
            "master" => Some(SourceKind::Master),
            other => Variant::parse(other).map(SourceKind::Derived),
        }
    }
}

/// `source_hashes` の 1 行（identity は D-62 と同じく dev を持たない）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceHash {
    pub semantic: String,
    pub inode: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub sha256: String,
}

/// マニフェストの入力（選曲に入った 1 曲。missing とマルチチャンネルは呼び出し側で remove に回す）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInput {
    pub track_id: i64,
    pub rel_path: String,
    pub lossless: bool,
    pub channels: Option<i64>,
    pub audio_version: i64,
    pub tag_version: i64,
    pub rg_ready: bool,
    pub derived: Option<Current>,
    pub hash_master: Option<SourceHash>,
    pub hash_derived: Option<SourceHash>,
}

/// 送る元
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub kind: SourceKind,
    pub root_rel_path: String,
    pub semantic: String,
}

/// 待ちの理由（失敗ではない。hold）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    NoDerived,
    AudioStale,
    RgPending,
    Hashing,
}

impl Wait {
    pub fn reason(self) -> &'static str {
        match self {
            Wait::NoDerived => "Derived 未生成",
            Wait::AudioStale => "Derived の音声が古い",
            Wait::RgPending => "RG 未解析",
            Wait::Hashing => "ハッシュ計算中",
        }
    }
}

/// 送る元を決める（仕様 ③「送る元の決定」）。ハッシュの有無はここでは見ない（`build_manifest` が見る）
pub fn decide_source(s: &VariantSettings, t: &TrackInput) -> Result<Source, Wait> {
    if s.variant == Variant::Opus && !t.lossless {
        return Ok(Source {
            kind: SourceKind::Master,
            root_rel_path: t.rel_path.clone(),
            semantic: semantic_master(t.audio_version, t.tag_version),
        });
    }
    let Some(row) = &t.derived else {
        return Err(if s.variant == Variant::Aac && !t.rg_ready {
            Wait::RgPending
        } else {
            Wait::NoDerived
        });
    };
    // 音声版の不一致は凍結中でも配らない。profile は有効な系統だけ現在の設定と比べる
    if row.src_audio_version != t.audio_version
        || (s.enabled && row.audio_profile != s.audio_profile)
    {
        return Err(Wait::AudioStale);
    }
    Ok(Source {
        kind: SourceKind::Derived(s.variant),
        root_rel_path: row.rel_path.clone(),
        semantic: semantic_derived(s.variant, row),
    })
}

/// 端末上のパス: Library の rel_path の拡張子だけを送る元に合わせる。Derived の期待パス
/// （`crate::domain::derived::expected_rel_path`）を再利用し、先頭の `<variant.dir()>/` を
/// 取り除いて Library のレイアウトに戻す（拡張子を置き換えるロジックを重複させない）
pub fn dest_path(t: &TrackInput, kind: SourceKind) -> String {
    let SourceKind::Derived(v) = kind else {
        return t.rel_path.clone();
    };
    let expected = expected_rel_path(v, &t.rel_path);
    let prefix = format!("{}/", v.dir());
    expected
        .strip_prefix(prefix.as_str())
        .map(str::to_owned)
        .unwrap_or(expected)
}

pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

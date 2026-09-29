//! 端末への配信の純粋関数（docs/superpowers/specs/2026-09-29-device-delivery-design.md ③、D-95）。
//! DB も HTTP も知らない。入力は `db::devices` が組み立てる。
//!
//! トークンはすべて不透明な値（解析しない）。構成要素を正準 JSON（serde_json の Map は BTreeMap なので
//! キーは昇順、`to_vec` は空白を入れない）にした SHA-256 の 16 進小文字。スキーマ版 `v` を必ず含める

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::domain::derived::{Current, Variant};

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

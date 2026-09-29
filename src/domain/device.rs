//! 端末への配信の純粋関数（docs/superpowers/specs/2026-09-29-device-delivery-design.md ③、D-95）。
//! DB も HTTP も知らない。入力は `db::devices` が組み立てる。
//!
//! トークンはすべて不透明な値（解析しない）。構成要素を正準 JSON（serde_json の Map は BTreeMap なので
//! キーは昇順、`to_vec` は空白を入れない）にした SHA-256 の 16 進小文字。スキーマ版 `v` を必ず含める

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::domain::derived::{expected_rel_path, Current, Variant, VariantSettings};
use crate::domain::relpath::canonical_key;

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

/// 端末上のパス長の上限（root の前置きを含む。SPEC §5）
pub const MAX_DEVICE_PATH_UTF16: usize = 240;
/// root 直下の予約名（manifest とジャーナルの置き場）
pub const RESERVED_NAME: &str = ".spindle";
/// プレイリストの置き場（root 相対）
pub const PLAYLIST_DIR: &str = "Playlists";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemError {
    PathCollision,
    PathTooLong,
}

impl ItemError {
    pub fn reason(self) -> &'static str {
        match self {
            ItemError::PathCollision => "パス衝突",
            ItemError::PathTooLong => "パスが長すぎる",
        }
    }
}

/// hold の理由: 待ち（失敗ではない）かエラー。どちらも端末上の既存の写しに触らない
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    Wait(Wait),
    Error(ItemError),
}

impl Hold {
    pub fn reason(self) -> &'static str {
        match self {
            Hold::Wait(w) => w.reason(),
            Hold::Error(e) => e.reason(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredItem {
    pub track_id: i64,
    pub source: Source,
    pub dest_path: String,
    pub dest_key: String,
    /// 配信トークン
    pub token: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    pub desired: Vec<DesiredItem>,
    pub hold: Vec<(i64, Hold)>,
    pub needs_hash: Vec<(i64, SourceKind)>,
}

impl Manifest {
    pub fn hold_of(&self, track_id: i64) -> Option<Hold> {
        self.hold
            .iter()
            .find(|(id, _)| *id == track_id)
            .map(|(_, h)| *h)
    }
}

/// 端末に反映済みの 1 曲（`device_items` の行）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceItem {
    pub track_id: i64,
    pub dest_path: String,
    pub token: String,
    pub size: u64,
    pub sha256: String,
}

/// 選曲に入った曲からマニフェストを作る（仕様 ③）。`root_prefix_utf16` は端末の root を前置きする長さ
/// （区切りの `/` を含む）
pub fn build_manifest(
    s: &VariantSettings,
    root_prefix_utf16: usize,
    tracks: &[TrackInput],
) -> Manifest {
    let mut m = Manifest::default();
    let mut candidates: Vec<DesiredItem> = Vec::new();
    for t in tracks {
        let source = match decide_source(s, t) {
            Ok(src) => src,
            Err(w) => {
                m.hold.push((t.track_id, Hold::Wait(w)));
                continue;
            }
        };
        let hash = match source.kind {
            SourceKind::Master => t.hash_master.as_ref(),
            SourceKind::Derived(_) => t.hash_derived.as_ref(),
        };
        let Some(hash) = hash.filter(|h| h.semantic == source.semantic) else {
            m.hold.push((t.track_id, Hold::Wait(Wait::Hashing)));
            m.needs_hash.push((t.track_id, source.kind));
            continue;
        };
        let dest = dest_path(t, source.kind);
        if root_prefix_utf16 + utf16_len(&dest) > MAX_DEVICE_PATH_UTF16 {
            m.hold
                .push((t.track_id, Hold::Error(ItemError::PathTooLong)));
            continue;
        }
        candidates.push(DesiredItem {
            track_id: t.track_id,
            token: delivery_token(&source.semantic, &hash.sha256),
            size: hash.size,
            sha256: hash.sha256.clone(),
            dest_key: canonical_key(&dest),
            dest_path: dest,
            source,
        });
    }
    let mut by_key: HashMap<&str, usize> = HashMap::new();
    for c in &candidates {
        *by_key.entry(c.dest_key.as_str()).or_default() += 1;
    }
    let colliding: HashSet<String> = by_key
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(k, _)| k.to_string())
        .collect();
    for c in candidates {
        if colliding.contains(&c.dest_key) {
            m.hold
                .push((c.track_id, Hold::Error(ItemError::PathCollision)));
        } else {
            m.desired.push(c);
        }
    }
    m.desired.sort_by_key(|d| d.track_id);
    m.hold.sort_by_key(|(id, _)| *id);
    m.needs_hash.sort();
    m
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Adb,
    Agent,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Adb => "adb",
            Transport::Agent => "agent",
        }
    }

    pub fn parse(s: &str) -> Option<Transport> {
        match s {
            "adb" => Some(Transport::Adb),
            "agent" => Some(Transport::Agent),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistInput {
    pub playlist_id: i64,
    pub name: String,
    pub track_ids: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredPlaylist {
    pub playlist_id: i64,
    pub name: String,
    pub dest_path: String,
    pub body: Vec<u8>,
    pub token: String,
}

/// プレイリストの中身。adb は m3u8（`Playlists/` から見た相対パス）、agent は `[[track_id, path], …]` の
/// 正準 JSON（ミュージック.app へは track で入れるので、内容の同一性だけ表せればよい）
pub fn render_playlist(transport: Transport, entries: &[(i64, &str)]) -> Vec<u8> {
    match transport {
        Transport::Adb => {
            let mut s = String::from("#EXTM3U\n");
            for (_, path) in entries {
                s.push_str("../");
                s.push_str(path);
                s.push('\n');
            }
            s.into_bytes()
        }
        Transport::Agent => {
            let v: Vec<Value> = entries.iter().map(|(id, p)| json!([id, p])).collect();
            serde_json::to_vec(&v).unwrap_or_default()
        }
    }
}

/// 印の付いたプレイリストを組み立てる。参照するのは**同期の後に端末に実在する曲**だけ: desired は今回の
/// パス、hold で既存の写しがある曲は `current` のパス。名前の衝突（`canonical_key`）と予約名はエラー
/// （返り値の 2 つ目。`(playlist_id, 理由)`）
pub fn build_playlists(
    transport: Transport,
    playlists: &[PlaylistInput],
    manifest: &Manifest,
    current: &[DeviceItem],
) -> (Vec<DesiredPlaylist>, Vec<(i64, &'static str)>) {
    let desired: HashMap<i64, &str> = manifest
        .desired
        .iter()
        .map(|d| (d.track_id, d.dest_path.as_str()))
        .collect();
    let held_copy: HashMap<i64, &str> = current
        .iter()
        .filter(|c| manifest.hold_of(c.track_id).is_some())
        .map(|c| (c.track_id, c.dest_path.as_str()))
        .collect();
    let mut name_count: BTreeMap<String, usize> = BTreeMap::new();
    for p in playlists {
        *name_count.entry(canonical_key(&p.name)).or_default() += 1;
    }
    let reserved = canonical_key(RESERVED_NAME);
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for p in playlists {
        let key = canonical_key(&p.name);
        if key == reserved {
            errors.push((p.playlist_id, "予約名"));
            continue;
        }
        if name_count.get(&key).copied().unwrap_or(0) > 1 {
            errors.push((p.playlist_id, "プレイリスト名の衝突"));
            continue;
        }
        let entries: Vec<(i64, &str)> = p
            .track_ids
            .iter()
            .filter_map(|id| {
                desired
                    .get(id)
                    .or_else(|| held_copy.get(id))
                    .map(|path| (*id, *path))
            })
            .collect();
        let body = render_playlist(transport, &entries);
        let token = playlist_token(p.playlist_id, &p.name, &sha256_hex(&body));
        out.push(DesiredPlaylist {
            playlist_id: p.playlist_id,
            name: p.name.clone(),
            dest_path: format!("{PLAYLIST_DIR}/{}.m3u8", p.name),
            body,
            token,
        });
    }
    out.sort_by_key(|p| p.playlist_id);
    errors.sort();
    (out, errors)
}

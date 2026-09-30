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

/// 印の付いたプレイリストを組み立てる。`manifest` は `resolve_collisions` を通したものを渡すこと。
/// 参照するのは**同期の後に端末に実在する曲**だけ: desired は今回の
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
    let held_ids: HashSet<i64> = manifest.hold.iter().map(|(id, _)| *id).collect();
    let held_copy: HashMap<i64, &str> = current
        .iter()
        .filter(|c| held_ids.contains(&c.track_id))
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

/// 差分の操作。並び順は実行順（削除 → パス変更 → 更新 → 追加。仕様 ⑤）
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    Delete,
    Move,
    UpdateMove,
    Update,
    Add,
}

impl OpKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OpKind::Delete => "delete",
            OpKind::Move => "move",
            OpKind::UpdateMove => "update_move",
            OpKind::Update => "update",
            OpKind::Add => "add",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemOp {
    pub kind: OpKind,
    pub track_id: i64,
    pub from: Option<String>,
    pub to: Option<String>,
    pub token: Option<String>,
    pub size: u64,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistState {
    pub playlist_id: i64,
    pub dest_path: String,
    pub token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaylistOpKind {
    Add,
    Update,
    Delete,
}

impl PlaylistOpKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PlaylistOpKind::Add => "add",
            PlaylistOpKind::Update => "update",
            PlaylistOpKind::Delete => "delete",
        }
    }
}

/// 端末上の項目の種類（`device_errors.kind`、ジャーナルの `kind`）
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Track,
    Playlist,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::Track => "track",
            EntryKind::Playlist => "playlist",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistOp {
    pub kind: PlaylistOpKind,
    pub playlist_id: i64,
    pub from: Option<String>,
    pub to: Option<String>,
    pub token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldItem {
    pub track_id: i64,
    pub hold: Hold,
    /// 端末に既存の写しがある（古い版のまま残る）
    pub has_copy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Diff {
    pub items: Vec<ItemOp>,
    pub playlists: Vec<PlaylistOp>,
    pub held: Vec<HeldItem>,
    pub playlist_errors: Vec<(i64, &'static str)>,
}

/// 行き先の衝突を不動点まで解く（仕様 ③「差分」）。行き先が「動かない管理下の曲」に占められていれば、
/// その曲をパス衝突で保留にし、その曲自身も動かなくなるので不動点まで繰り返す（入れ替えの片側が保留に
/// なれば、もう片側も保留になる）。返すマニフェストの desired と hold が最終的なもの（needs_hash は
/// そのまま）。解いた結果をもう一度渡しても変わらない（冪等）。プレイリストは解いた後の desired と
/// hold から組み立てること（保留になった曲を行き先のパスで載せないため）
pub fn resolve_collisions(manifest: &Manifest, current: &[DeviceItem]) -> Manifest {
    let mut held: BTreeMap<i64, Hold> = manifest.hold.iter().copied().collect();
    let mut desired: BTreeMap<i64, &DesiredItem> =
        manifest.desired.iter().map(|d| (d.track_id, d)).collect();

    loop {
        // 動かない管理下の曲が占めるパス: 現状にあり、削除されず、パスも変えない曲
        let mut stationary: HashMap<String, i64> = HashMap::new();
        for c in current {
            let moving = desired
                .get(&c.track_id)
                .is_some_and(|d| d.dest_key != canonical_key(&c.dest_path));
            let removed = !desired.contains_key(&c.track_id) && !held.contains_key(&c.track_id);
            if !moving && !removed {
                stationary.insert(canonical_key(&c.dest_path), c.track_id);
            }
        }
        let blocked: Vec<i64> = desired
            .values()
            .filter(|d| {
                stationary
                    .get(&d.dest_key)
                    .is_some_and(|owner| *owner != d.track_id)
            })
            .map(|d| d.track_id)
            .collect();
        if blocked.is_empty() {
            break;
        }
        for id in blocked {
            desired.remove(&id);
            held.insert(id, Hold::Error(ItemError::PathCollision));
        }
    }

    Manifest {
        desired: desired.into_values().cloned().collect(),
        hold: held.into_iter().collect(),
        needs_hash: manifest.needs_hash.clone(),
    }
}

/// 差分（仕様 ③「3 つの集合」「差分」）。desired と現状を track_id で突き合わせ、hold は触らず、
/// マニフェストに無い（remove）ものを削除にする。行き先の衝突は内部で `resolve_collisions` を通して
/// 解く（冪等なので、解いた後のマニフェストを渡してもよい。`db::devices::compute` はプレイリストを
/// 組み立てる前に解いたものを渡す）
pub fn diff(
    manifest: &Manifest,
    current: &[DeviceItem],
    playlists: &[DesiredPlaylist],
    current_playlists: &[PlaylistState],
    playlist_errors: Vec<(i64, &'static str)>,
) -> Diff {
    let cur: HashMap<i64, &DeviceItem> = current.iter().map(|c| (c.track_id, c)).collect();
    let resolved = resolve_collisions(manifest, current);
    let held: BTreeMap<i64, Hold> = resolved.hold.iter().copied().collect();
    let desired: BTreeMap<i64, &DesiredItem> =
        resolved.desired.iter().map(|d| (d.track_id, d)).collect();

    let mut items = Vec::new();
    for d in desired.values() {
        let op = match cur.get(&d.track_id) {
            None => Some((OpKind::Add, None)),
            Some(c) => {
                let moved = c.dest_path != d.dest_path;
                match (c.token != d.token, moved) {
                    (false, false) => None,
                    (true, false) => Some((OpKind::Update, None)),
                    (false, true) => Some((OpKind::Move, Some(c.dest_path.clone()))),
                    (true, true) => Some((OpKind::UpdateMove, Some(c.dest_path.clone()))),
                }
            }
        };
        if let Some((kind, from)) = op {
            items.push(ItemOp {
                kind,
                track_id: d.track_id,
                from,
                to: Some(d.dest_path.clone()),
                token: Some(d.token.clone()),
                size: d.size,
                sha256: Some(d.sha256.clone()),
            });
        }
    }
    for c in current {
        if !desired.contains_key(&c.track_id) && !held.contains_key(&c.track_id) {
            items.push(ItemOp {
                kind: OpKind::Delete,
                track_id: c.track_id,
                from: Some(c.dest_path.clone()),
                to: None,
                token: None,
                size: 0,
                sha256: None,
            });
        }
    }
    items.sort_by_key(|o| (o.kind, o.track_id));

    let held: Vec<HeldItem> = held
        .into_iter()
        .map(|(track_id, hold)| HeldItem {
            track_id,
            hold,
            has_copy: cur.contains_key(&track_id),
        })
        .collect();

    let cur_pl: HashMap<i64, &PlaylistState> = current_playlists
        .iter()
        .map(|p| (p.playlist_id, p))
        .collect();
    let mut pl_ops = Vec::new();
    for p in playlists {
        let kind = match cur_pl.get(&p.playlist_id) {
            None => Some(PlaylistOpKind::Add),
            Some(c) if c.token != p.token || c.dest_path != p.dest_path => {
                Some(PlaylistOpKind::Update)
            }
            Some(_) => None,
        };
        if let Some(kind) = kind {
            pl_ops.push(PlaylistOp {
                kind,
                playlist_id: p.playlist_id,
                from: cur_pl.get(&p.playlist_id).map(|c| c.dest_path.clone()),
                to: Some(p.dest_path.clone()),
                token: Some(p.token.clone()),
            });
        }
    }
    let wanted: HashSet<i64> = playlists.iter().map(|p| p.playlist_id).collect();
    let errored: HashSet<i64> = playlist_errors.iter().map(|e| e.0).collect();
    for c in current_playlists {
        // エラーのプレイリストは hold と同じく既存のものに触らない
        if !wanted.contains(&c.playlist_id) && !errored.contains(&c.playlist_id) {
            pl_ops.push(PlaylistOp {
                kind: PlaylistOpKind::Delete,
                playlist_id: c.playlist_id,
                from: Some(c.dest_path.clone()),
                to: None,
                token: None,
            });
        }
    }
    pl_ops.sort_by_key(|p| p.playlist_id);

    Diff {
        items,
        playlists: pl_ops,
        held,
        playlist_errors,
    }
}

/// 計画トークン: 差分画面が表示した計画（generation・操作・トークン・パス）の正準 JSON の SHA-256
pub fn plan_token(generation: i64, d: &Diff) -> String {
    let items: Vec<Value> = d
        .items
        .iter()
        .map(|o| json!([o.kind.as_str(), o.track_id, o.from, o.to, o.token]))
        .collect();
    let playlists: Vec<Value> = d
        .playlists
        .iter()
        .map(|p| json!([p.kind.as_str(), p.playlist_id, p.from, p.to, p.token]))
        .collect();
    canonical_sha256(&json!({
        "v": TOKEN_VERSION, "kind": "plan", "generation": generation,
        "items": items, "playlists": playlists,
    }))
}

/// 曲ごとの端末の状態（④ 可視化 A / D、`device_pending`）。対象外で端末にも無い曲は持たない
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TrackState {
    Synced {
        synced_at: Option<i64>,
    },
    /// 未反映（追加・更新・移動・更新 + 移動）。`reason` は前回の同期・報告のエラー
    Pending {
        op: &'static str,
        reason: Option<String>,
    },
    Waiting {
        reason: &'static str,
        has_copy: bool,
    },
    Error {
        reason: String,
        has_copy: bool,
    },
    /// 対象外で、次の同期で端末から消える
    Removing,
}

/// 差分と反映済みの行から曲ごとの状態を作る。`reported` は `device_errors` の track 行（ref_id, 理由）、
/// `synced_at` は `device_items.synced_at`
pub fn track_states(
    diff: &Diff,
    current: &[DeviceItem],
    reported: &[(i64, String)],
    synced_at: &HashMap<i64, i64>,
) -> BTreeMap<i64, TrackState> {
    let reported: HashMap<i64, &str> = reported.iter().map(|(id, r)| (*id, r.as_str())).collect();
    let mut out = BTreeMap::new();
    for c in current {
        let state = match reported.get(&c.track_id) {
            Some(r) => TrackState::Error {
                reason: (*r).to_owned(),
                has_copy: true,
            },
            None => TrackState::Synced {
                synced_at: synced_at.get(&c.track_id).copied(),
            },
        };
        out.insert(c.track_id, state);
    }
    for o in &diff.items {
        let state = match o.kind {
            OpKind::Delete => TrackState::Removing,
            k => TrackState::Pending {
                op: k.as_str(),
                reason: reported.get(&o.track_id).map(|r| (*r).to_owned()),
            },
        };
        out.insert(o.track_id, state);
    }
    for h in &diff.held {
        let state = match h.hold {
            Hold::Wait(w) => TrackState::Waiting {
                reason: w.reason(),
                has_copy: h.has_copy,
            },
            Hold::Error(e) => TrackState::Error {
                reason: e.reason().to_owned(),
                has_copy: h.has_copy,
            },
        };
        out.insert(h.track_id, state);
    }
    out
}

/// 端末の件数（`GET /api/devices` の counts）。更新 + 移動は移動に数える
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Counts {
    pub add: usize,
    pub update: usize,
    #[serde(rename = "move")]
    pub r#move: usize,
    pub delete: usize,
    pub waiting: usize,
    pub error: usize,
    pub synced: usize,
}

pub fn counts(states: &BTreeMap<i64, TrackState>) -> Counts {
    let mut c = Counts::default();
    for s in states.values() {
        match s {
            TrackState::Synced { .. } => c.synced += 1,
            TrackState::Pending { op: "add", .. } => c.add += 1,
            TrackState::Pending { op: "update", .. } => c.update += 1,
            TrackState::Pending { .. } => c.r#move += 1,
            TrackState::Waiting { .. } => c.waiting += 1,
            TrackState::Error { .. } => c.error += 1,
            TrackState::Removing => c.delete += 1,
        }
    }
    c
}

/// 送る量と、実行順（削除 → パス変更のバッチ → 更新 → 追加。仕様 ⑤）に沿った「今より増える量」の最大値
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Estimate {
    pub transfer_bytes: u64,
    pub peak_bytes: u64,
}

/// 実行順（削除 → パス変更のバッチ → 更新 → 追加。仕様 ⑤）に沿った送る量と「今より増える量」の最大値。
/// `ops` は (操作, track_id, 新しい内容の size)
pub fn peak_bytes(ops: &[(OpKind, i64, u64)], current: &[DeviceItem]) -> Estimate {
    let old: HashMap<i64, i64> = current
        .iter()
        .map(|c| (c.track_id, c.size as i64))
        .collect();
    let old_of = |id: i64| old.get(&id).copied().unwrap_or(0);
    let mut used: i64 = 0;
    let mut peak: i64 = 0;
    let mut transfer: u64 = 0;
    let of = |k: OpKind| ops.iter().filter(move |o| o.0 == k);
    for (_, id, _) in of(OpKind::Delete) {
        used -= old_of(*id);
    }
    // prepared: 更新 + 移動の新しい内容が旧版と並ぶ
    for (_, _, size) in of(OpKind::UpdateMove) {
        used += *size as i64;
        transfer += size;
        peak = peak.max(used);
    }
    // vacating: 更新 + 移動の旧版を消す（移動は増減なし）
    for (_, id, _) in of(OpKind::UpdateMove) {
        used -= old_of(*id);
    }
    for (_, id, size) in of(OpKind::Update) {
        used += *size as i64;
        transfer += size;
        peak = peak.max(used);
        used -= old_of(*id);
    }
    for (_, _, size) in of(OpKind::Add) {
        used += *size as i64;
        transfer += size;
        peak = peak.max(used);
    }
    Estimate {
        transfer_bytes: transfer,
        peak_bytes: peak.max(0) as u64,
    }
}

pub fn estimate(diff: &Diff, current: &[DeviceItem]) -> Estimate {
    let ops: Vec<(OpKind, i64, u64)> = diff
        .items
        .iter()
        .map(|o| (o.kind, o.track_id, o.size))
        .collect();
    peak_bytes(&ops, current)
}

/// 端末ごとの未反映（追加・更新・移動・更新 + 移動）の track_id。DSL の `device_pending` とフィルタ
/// 「端末に未反映」が使う。`by_key` は端末名の `canonical_key`、`by_id` は端末 id
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingSets {
    pub by_key: std::sync::Arc<HashMap<String, Vec<i64>>>,
    pub by_id: std::sync::Arc<HashMap<i64, Vec<i64>>>,
}

impl PendingSets {
    pub fn for_key(&self, name_key: &str) -> &[i64] {
        self.by_key.get(name_key).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn for_id(&self, id: i64) -> &[i64] {
        self.by_id.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }
}

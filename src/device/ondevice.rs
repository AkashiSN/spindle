//! 端末側の正本 `<root>/.spindle/manifest.json`（仕様 ⑤「端末側の正本」）。
//! 端末側が正で、`device_items` はキャッシュ。書き直すのは spindle だけなので、未知のフィールドは
//! 読み捨てて保持しない

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::device::{DeviceItem, EntryKind, PlaylistState, RESERVED_NAME};
use crate::domain::relpath::{canonical_key, RelPath};

/// manifest の形式の版
pub const FORMAT: u64 = 1;
/// 曲とプレイリストを合わせた件数の上限
pub const MAX_ENTRIES: usize = 100_000;
pub const MANIFEST_PATH: &str = ".spindle/manifest.json";
pub const JOURNAL_PATH: &str = ".spindle/journal";
/// パス変更のバッチが旧パスを空ける置き場（`<MOVING_DIR>/<op_id>`、更新 + 移動の新しい内容は `.new`）
pub const MOVING_DIR: &str = ".spindle/moving";
/// 転送中の一時ファイルの接尾辞。回復はこれで終わるファイルを残骸として消す
pub const TMP_SUFFIX: &str = ".spindle-tmp";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestItem {
    pub track_id: i64,
    pub path: String,
    pub token: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestPlaylist {
    pub playlist_id: i64,
    pub path: String,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceManifest {
    pub format: u64,
    pub device_uuid: String,
    pub volume: String,
    pub items: Vec<ManifestItem>,
    pub playlists: Vec<ManifestPlaylist>,
}

impl DeviceManifest {
    pub fn new(device_uuid: &str, volume: &str) -> Self {
        Self {
            format: FORMAT,
            device_uuid: device_uuid.to_owned(),
            volume: volume.to_owned(),
            items: Vec::new(),
            playlists: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest を JSON として読めない: {0}")]
    Json(String),
    #[error("manifest の形式 {0} を知らない")]
    UnknownFormat(u64),
    #[error("manifest の項目が多すぎる（{0} 件）")]
    TooMany(usize),
    #[error("manifest に同じ曲が 2 度ある（track_id {0}）")]
    DuplicateTrack(i64),
    #[error("manifest に同じプレイリストが 2 度ある（playlist_id {0}）")]
    DuplicatePlaylist(i64),
    #[error("manifest に同じパスが 2 度ある: {0}")]
    DuplicatePath(String),
    #[error("manifest のパスが root の外か予約名を指す: {0}")]
    BadPath(String),
}

/// `.spindle` の下（大小文字・正規化の違いを含む）
pub fn is_reserved(path: &str) -> bool {
    path.split('/')
        .next()
        .is_some_and(|first| canonical_key(first) == canonical_key(RESERVED_NAME))
}

/// 管理下のファイルとして置けるパスか（root 相対・`..` なし・`.spindle` の下でない）
pub fn managed_path_ok(path: &str) -> bool {
    RelPath::parse(path).is_ok() && !is_reserved(path)
}

pub fn parse(bytes: &[u8]) -> Result<DeviceManifest, ManifestError> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| ManifestError::Json(e.to_string()))?;
    let format = v
        .get("format")
        .and_then(Value::as_u64)
        .ok_or_else(|| ManifestError::Json("format が無い".into()))?;
    if format != FORMAT {
        return Err(ManifestError::UnknownFormat(format));
    }
    let m: DeviceManifest =
        serde_json::from_value(v).map_err(|e| ManifestError::Json(e.to_string()))?;
    validate(&m)?;
    Ok(m)
}

/// 正本として成り立つか（件数・id とパスの一意性・パスの範囲）。読むときも書くときも通す
pub fn validate(m: &DeviceManifest) -> Result<(), ManifestError> {
    let n = m.items.len() + m.playlists.len();
    if n > MAX_ENTRIES {
        return Err(ManifestError::TooMany(n));
    }
    let mut keys = HashSet::new();
    let mut ids = HashSet::new();
    for it in &m.items {
        if !managed_path_ok(&it.path) {
            return Err(ManifestError::BadPath(it.path.clone()));
        }
        if !ids.insert(it.track_id) {
            return Err(ManifestError::DuplicateTrack(it.track_id));
        }
        if !keys.insert(canonical_key(&it.path)) {
            return Err(ManifestError::DuplicatePath(it.path.clone()));
        }
    }
    let mut pids = HashSet::new();
    for p in &m.playlists {
        if !managed_path_ok(&p.path) {
            return Err(ManifestError::BadPath(p.path.clone()));
        }
        if !pids.insert(p.playlist_id) {
            return Err(ManifestError::DuplicatePlaylist(p.playlist_id));
        }
        if !keys.insert(canonical_key(&p.path)) {
            return Err(ManifestError::DuplicatePath(p.path.clone()));
        }
    }
    Ok(())
}

/// 書き出す。壊れた正本（パスの重複など）は書かない: 書くと以後の回復が必ず失敗する
pub fn render(m: &DeviceManifest) -> Result<Vec<u8>, ManifestError> {
    validate(m)?;
    serde_json::to_vec(m).map_err(|e| ManifestError::Json(e.to_string()))
}

/// 回復と同期が編集する manifest（id で引ける形）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Book {
    pub device_uuid: String,
    pub volume: String,
    pub items: BTreeMap<i64, ManifestItem>,
    pub playlists: BTreeMap<i64, ManifestPlaylist>,
}

impl From<DeviceManifest> for Book {
    fn from(m: DeviceManifest) -> Self {
        Self {
            device_uuid: m.device_uuid,
            volume: m.volume,
            items: m.items.into_iter().map(|i| (i.track_id, i)).collect(),
            playlists: m
                .playlists
                .into_iter()
                .map(|p| (p.playlist_id, p))
                .collect(),
        }
    }
}

impl Book {
    pub fn manifest(&self) -> DeviceManifest {
        DeviceManifest {
            format: FORMAT,
            device_uuid: self.device_uuid.clone(),
            volume: self.volume.clone(),
            items: self.items.values().cloned().collect(),
            playlists: self.playlists.values().cloned().collect(),
        }
    }

    pub fn device_items(&self) -> Vec<DeviceItem> {
        self.items
            .values()
            .map(|i| DeviceItem {
                track_id: i.track_id,
                dest_path: i.path.clone(),
                token: i.token.clone(),
                size: i.size,
                sha256: i.sha256.clone(),
            })
            .collect()
    }

    pub fn playlist_states(&self) -> Vec<PlaylistState> {
        self.playlists
            .values()
            .map(|p| PlaylistState {
                playlist_id: p.playlist_id,
                dest_path: p.path.clone(),
                token: p.token.clone(),
            })
            .collect()
    }

    /// `path` を `kind` / `id` 以外の管理下の項目（曲とプレイリストは同じ名前空間）が占めているか
    pub fn occupied_by_other(&self, kind: EntryKind, id: i64, path: &str) -> bool {
        let key = canonical_key(path);
        let track = self.items.values().any(|i| {
            !(kind == EntryKind::Track && i.track_id == id) && canonical_key(&i.path) == key
        });
        let playlist = self.playlists.values().any(|p| {
            !(kind == EntryKind::Playlist && p.playlist_id == id) && canonical_key(&p.path) == key
        });
        track || playlist
    }

    /// 管理下のパスの `canonical_key`
    pub fn path_keys(&self) -> HashSet<String> {
        self.items
            .values()
            .map(|i| canonical_key(&i.path))
            .chain(self.playlists.values().map(|p| canonical_key(&p.path)))
            .collect()
    }
}

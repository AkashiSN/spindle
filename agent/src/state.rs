//! `state.json`（仕様 ⑥「ミュージック.app への反映」）。エージェントの実状態の正本。
//! 書き込みは同じディレクトリの tmp → fsync → rename → 親ディレクトリの fsync。1 つの遷移で
//! 変わるもの（tracks の更新・バッチの相・pending_* からの除去）は同じ 1 回の書き込みに含める

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

pub const STATE_VERSION: u32 = 1;
/// 外で書き換えられた・中身を失った曲のトークン（本体の `recover::STALE_TOKEN` と同じ値）。
/// 報告するとサーバの差分は「更新」になる（D-100）
pub const STALE_TOKEN: &str = "";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub v: u32,
    #[serde(default)]
    pub server: Option<ServerInfo>,
    #[serde(default)]
    pub setup: Option<Setup>,
    #[serde(default)]
    pub tracks: BTreeMap<i64, TrackEntry>,
    #[serde(default)]
    pub playlists: BTreeMap<i64, PlaylistEntry>,
    #[serde(default)]
    pub pending_ops: Vec<PendingOp>,
    #[serde(default)]
    pub pending_batches: Vec<PendingBatch>,
    #[serde(default)]
    pub plan_id: Option<i64>,
    /// 回復・再発見で state が変わり、まだ報告していない（D-100 の「報告だけの回」）
    #[serde(default)]
    pub needs_report: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            v: STATE_VERSION,
            server: None,
            setup: None,
            tracks: BTreeMap::new(),
            playlists: BTreeMap::new(),
            pending_ops: Vec::new(),
            pending_batches: Vec::new(),
            plan_id: None,
            needs_report: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub url: String,
    pub insecure_http: bool,
    pub device_uuid: String,
    pub device_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupPhase {
    /// 記録しただけ（副作用の前）
    Started,
    Marker,
    FolderCreated,
    FolderRenamed,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setup {
    pub phase: SetupPhase,
    /// 128 bit 乱数（16 進）。`.spindle-device` と一時フォルダ名 `spindle-setup-<nonce>` に使う
    pub nonce: String,
    /// ミュージック.app の「spindle」フォルダの persistent ID
    pub folder_pid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackEntry {
    pub persistent_id: String,
    pub token: String,
    /// root 相対（`/` 区切り）。サーバの `dest_path`
    pub path: String,
    pub size: u64,
    pub sha256: String,
    /// sha256 を読み直さずに済ませるための stat のキャッシュ（D-100）
    pub inode: u64,
    pub mtime_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistEntry {
    pub persistent_id: String,
    pub name: String,
    pub token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingKind {
    Add,
    Update,
    Delete,
    Playlist,
    PlaylistDelete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpPhase {
    /// 取得中（`<to>.spindle-tmp`）
    Fetching,
    /// ファイルを置き終え、`add` の直前（`max_database_id` を記録済み）
    Adding,
    Deleting,
    /// 一時プレイリストを作る直前（persistent ID は未記録）
    Creating,
    /// 一時プレイリストの persistent ID を記録済み
    Filling,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub persistent_id: String,
    pub sha256: String,
}

/// バッチ以外の論理操作。副作用の前に記録し、段ごとに `phase` を進め、終われば消す
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingOp {
    pub op_id: String,
    pub op: PendingKind,
    /// track_id か playlist_id
    pub ref_id: i64,
    pub persistent_id: Option<String>,
    pub from: Option<String>,
    /// 曲は root 相対パス、プレイリストは名前
    pub to: Option<String>,
    pub token: Option<String>,
    pub size: u64,
    pub sha256: Option<String>,
    pub phase: OpPhase,
    pub started_at: i64,
    pub max_database_id: Option<i64>,
    /// コピー設定 ON の疑いで表示した候補（resolve が照合する。D-100）
    #[serde(default)]
    pub candidates: Option<Vec<Candidate>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchPhase {
    Sealed,
    Prepared,
    Vacating,
    Vacated,
    Placing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberOp {
    Move,
    UpdateMove,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchMember {
    pub op_id: String,
    pub op: MemberOp,
    pub track_id: i64,
    pub persistent_id: String,
    pub from: String,
    /// `.moving/<batch_id>-<op_id>`（旧ファイルの置き場）
    pub staging: String,
    /// 更新 + 移動の新しい内容 `.moving/<batch_id>-<op_id>.new`
    pub new: Option<String>,
    pub to: String,
    pub token: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingBatch {
    pub batch_id: String,
    pub phase: BatchPhase,
    pub digest: String,
    pub members: Vec<BatchMember>,
}

pub fn member_digest(members: &[BatchMember]) -> Result<String> {
    let bytes = serde_json::to_vec(members).map_err(|e| Error::State(e.to_string()))?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

pub struct StateFile {
    dir: PathBuf,
}

impl StateFile {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    fn path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    pub fn load(&self) -> Result<State> {
        let bytes = match std::fs::read(self.path()) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::default()),
            Err(e) => return Err(e.into()),
        };
        let s: State = serde_json::from_slice(&bytes).map_err(|e| Error::State(e.to_string()))?;
        if s.v != STATE_VERSION {
            return Err(Error::State(format!("知らない版（{}）", s.v)));
        }
        Ok(s)
    }

    pub fn save(&self, s: &State) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let bytes = serde_json::to_vec_pretty(s).map_err(|e| Error::State(e.to_string()))?;
        write_durable(&self.dir, "state.json", &bytes)
    }
}

/// `dir/name` を tmp → fsync → rename → dir の fsync で書く
pub(crate) fn write_durable(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let tmp = dir.join(format!(".{name}.tmp"));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, dir.join(name))?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

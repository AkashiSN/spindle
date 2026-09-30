//! 端末の root 配下の操作（仕様 ⑤）。本物は `AdbFs`（後続タスク）、テストは偽の端末 FS。
//! パスはすべて root 相対で、呼び出し側が `RelPath` として検証したもの

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RemoteError {
    #[error("端末が接続されていない")]
    NotConnected,
    #[error("端末の空き容量が足りない")]
    NoSpace,
    #[error("キャンセルされた")]
    Cancelled,
    #[error("端末の操作に失敗: {0}")]
    Failed(String),
}

pub type RemoteResult<T> = Result<T, RemoteError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
    /// root 相対
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirState {
    Missing,
    Empty,
    NonEmpty,
}

/// `put` が送った長さと、送りながら取った sha256
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutResult {
    pub size: u64,
    pub sha256: String,
}

#[allow(async_fn_in_trait)]
pub trait DeviceFs {
    /// ファイル全体。無ければ None
    async fn read(&self, path: &str) -> RemoteResult<Option<Vec<u8>>>;
    /// 上書きで書く（親ディレクトリは作る）。耐久化は呼び出し側が `sync` する
    async fn write(&self, path: &str, bytes: &[u8]) -> RemoteResult<()>;
    /// 末尾に追記する（親ディレクトリは作る）
    async fn append(&self, path: &str, bytes: &[u8]) -> RemoteResult<()>;
    /// 開いた FD の中身を全部 `path` へ書く（親ディレクトリは作る）
    async fn put(&self, path: &str, src: std::fs::File) -> RemoteResult<PutResult>;
    /// 端末側で取った sha256。無ければ None
    async fn sha256(&self, path: &str) -> RemoteResult<Option<String>>;
    /// root 配下の全通常ファイル（`.spindle` の下を含む）
    async fn list_files(&self) -> RemoteResult<Vec<RemoteFile>>;
    /// `mv -f`（`to` の親ディレクトリは作る）
    async fn rename(&self, from: &str, to: &str) -> RemoteResult<()>;
    /// `rm -f`（無くても成功）
    async fn remove(&self, path: &str) -> RemoteResult<()>;
    /// `.spindle` 以外の空ディレクトリを消す
    async fn prune_empty_dirs(&self) -> RemoteResult<()>;
    /// ファイルシステム全体の確定（toybox の `sync`）
    async fn sync(&self) -> RemoteResult<()>;
    /// root（無ければ最も近い親）のファイルシステムの空き
    async fn free_bytes(&self) -> RemoteResult<u64>;
    async fn root_state(&self) -> RemoteResult<DirState>;
    /// プレイヤー（Poweramp）へ再スキャンを頼む。無ければ何もしない
    async fn rescan(&self) -> RemoteResult<()>;
}

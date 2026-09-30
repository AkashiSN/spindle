//! adb の実行時の状態（P5-3b、D-98）: 設定、`track-devices` で見た端末の一覧、端末ごとのロック、
//! 最後に測った空き容量。監視タスク（[`spawn_watcher`]）が一覧を更新し、ジョブと API が読む

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt as _;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::adb::{AdbConfig, AdbFs};
use super::quote::root_abs_under;
use super::remote::RemoteError;
use super::track::{FrameReader, TrackedDevice};
use crate::jobs::process::ChildGroup;
use crate::jobs::BoxFuture;

pub struct AdbRuntime {
    cfg: AdbConfig,
    shutdown: CancellationToken,
    devices: Mutex<BTreeMap<String, TrackedDevice>>,
    locks: Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
    free: Mutex<HashMap<i64, u64>>,
    /// 登録を 1 件ずつにする（同じシリアル・保存先の二重登録を防ぐ）
    register: tokio::sync::Mutex<()>,
    /// ボリュームを置く場所（本番は `/storage`）
    storage_base: String,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl AdbRuntime {
    /// `shutdown` はプロセスの停止用。`AdbFs` に渡すのもこれ（ジョブのキャンセルは `Control` で効かせる）
    pub fn new(cfg: AdbConfig, shutdown: CancellationToken) -> Arc<Self> {
        Self::with_storage_base(cfg, shutdown, "/storage".to_owned())
    }

    /// ボリュームを置く場所を差し替えて作る（試験用。手元の一時ディレクトリを端末に見立てる）
    pub fn with_storage_base(
        cfg: AdbConfig,
        shutdown: CancellationToken,
        base: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            shutdown,
            devices: Mutex::default(),
            locks: Mutex::default(),
            free: Mutex::default(),
            register: tokio::sync::Mutex::new(()),
            storage_base: base,
        })
    }

    pub fn storage_base(&self) -> &str {
        &self.storage_base
    }

    pub fn cfg(&self) -> &AdbConfig {
        &self.cfg
    }

    pub fn shutdown(&self) -> &CancellationToken {
        &self.shutdown
    }

    pub fn devices(&self) -> Vec<TrackedDevice> {
        lock(&self.devices).values().cloned().collect()
    }

    pub fn state_of(&self, serial: &str) -> Option<String> {
        lock(&self.devices).get(serial).map(|d| d.state.clone())
    }

    pub fn is_connected(&self, serial: &str) -> bool {
        self.state_of(serial).as_deref() == Some("device")
    }

    /// 一覧を置き換え、新しく `device` になったシリアルを返す
    pub fn apply(&self, list: Vec<TrackedDevice>) -> Vec<String> {
        let mut g = lock(&self.devices);
        let mut became = Vec::new();
        let mut next = BTreeMap::new();
        for d in list {
            let was = g.get(&d.serial).map(|o| o.state.as_str()) == Some("device");
            if d.state == "device" && !was {
                became.push(d.serial.clone());
            }
            next.insert(d.serial.clone(), d);
        }
        *g = next;
        became
    }

    /// 監視が切れた（adb サーバが見えない）。一覧を空にし、次に繋がったら全部を新しく見えたとして扱う
    pub fn lost(&self) {
        lock(&self.devices).clear();
    }

    pub fn device_lock(&self, device_id: i64) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(lock(&self.locks).entry(device_id).or_default())
    }

    /// 登録（API の `create` の adb 分岐）を直列化するロック
    pub fn register_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.register
    }

    pub fn set_free(&self, device_id: i64, bytes: u64) {
        lock(&self.free).insert(device_id, bytes);
    }

    pub fn free(&self, device_id: i64) -> Option<u64> {
        lock(&self.free).get(&device_id).copied()
    }

    /// 端末の保存先を操作する `AdbFs`。トークンはプロセスの停止用
    pub fn fs_for(&self, serial: &str, volume: &str, root: &str) -> Result<AdbFs, RemoteError> {
        let abs = root_abs_under(self.storage_base(), volume, root)
            .ok_or_else(|| RemoteError::Failed(format!("保存先が不正: {volume} / {root}")))?;
        AdbFs::new(self.cfg.clone(), serial, &abs, self.shutdown.clone())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WatchOptions {
    pub min_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            min_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
        }
    }
}

pub type OnConnect = Arc<dyn Fn(String) -> BoxFuture<'static, ()> + Send + Sync>;

/// `adb track-devices -l` を購読し続ける常駐タスク（仕様 ⑤「ホットプラグ」）。EOF・エラーで一覧を空にし、
/// 上限付きの backoff で張り直す（60 秒以上続いた接続の後は最短に戻す）。停止でプロセスグループごと kill
pub fn spawn_watcher(
    rt: Arc<AdbRuntime>,
    opts: WatchOptions,
    on_connect: OnConnect,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = opts.min_backoff;
        loop {
            if rt.shutdown.is_cancelled() {
                return;
            }
            let started = Instant::now();
            match watch_once(&rt, &on_connect).await {
                Ok(()) => tracing::info!("adb track-devices が終わった。張り直す"),
                Err(e) => tracing::warn!(error = %e, "adb track-devices が失敗した。張り直す"),
            }
            rt.lost();
            if started.elapsed() >= Duration::from_secs(60) {
                backoff = opts.min_backoff;
            }
            tokio::select! {
                _ = rt.shutdown.cancelled() => return,
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(opts.max_backoff);
        }
    })
}

async fn watch_once(rt: &AdbRuntime, on_connect: &OnConnect) -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new(&rt.cfg.program);
    cmd.env("ADB_SERVER_SOCKET", &rt.cfg.server)
        .env("HOME", &rt.cfg.home)
        .args(["track-devices", "-l"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = ChildGroup::spawn(cmd)?;
    let mut stdout = child
        .child_mut()
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("stdout を取れない"))?;
    // stderr は動いている間ずっと吸い出す（読まないとパイプが詰まって adb が止まる）。末尾だけ持つ
    let stderr_task = child.child_mut().stderr.take().map(|mut e| {
        tokio::spawn(async move {
            const KEEP: usize = 8192;
            let mut tail: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 4096];
            while let Ok(n) = e.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                tail.extend_from_slice(&chunk[..n]);
                if tail.len() > KEEP {
                    tail.drain(..tail.len() - KEEP);
                }
            }
            tail
        })
    });
    let mut reader = FrameReader::default();
    let mut buf = vec![0u8; 4096];
    let result = loop {
        let n = tokio::select! {
            _ = rt.shutdown.cancelled() => break Ok(()),
            n = stdout.read(&mut buf) => n,
        };
        match n {
            Ok(0) => break Ok(()),
            Ok(n) => match reader.push(&buf[..n]) {
                Ok(frames) => {
                    for list in frames {
                        for serial in rt.apply(list) {
                            on_connect(serial).await;
                        }
                    }
                }
                Err(e) => break Err(anyhow::Error::new(e)),
            },
            Err(e) => break Err(e.into()),
        }
    };
    child.kill_group().await;
    if let Some(task) = stderr_task {
        if let Ok(Ok(bytes)) = tokio::time::timeout(Duration::from_millis(500), task).await {
            let text = String::from_utf8_lossy(&bytes);
            let text = text.trim();
            let skip = text.chars().count().saturating_sub(2000);
            let tail: String = text.chars().skip(skip).collect();
            if !tail.is_empty() {
                tracing::warn!(stderr = %tail, "adb track-devices の stderr");
            }
        }
    }
    result
}

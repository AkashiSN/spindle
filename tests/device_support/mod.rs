//! 端末側のテスト部品: メモリ上の偽の端末 FS（切断の注入つき）、送る元、実行の制御

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::sync::Mutex;

use spindle::device::remote::{
    DeviceFs, DirState, PutResult, RemoteError, RemoteFile, RemoteResult,
};
use spindle::domain::device::sha256_hex;
use spindle::domain::relpath::canonical_key;

#[derive(Clone, Default)]
struct Inner {
    /// キー → (表示のパス, 中身)。大小文字を区別しない FS ではキーが canonical_key
    files: BTreeMap<String, (String, Vec<u8>)>,
    dirs: BTreeSet<String>,
    root_exists: bool,
    case_insensitive: bool,
    capacity: u64,
    fail_after: Option<usize>,
    mutations: usize,
    disconnected: bool,
    rescans: usize,
    rescan_fails: bool,
    calls: Vec<String>,
}

impl Inner {
    fn key(&self, path: &str) -> String {
        if self.case_insensitive {
            canonical_key(path)
        } else {
            path.to_owned()
        }
    }

    fn used(&self) -> u64 {
        self.files.values().map(|(_, b)| b.len() as u64).sum()
    }

    fn add_parents(&mut self, path: &str) {
        let mut acc = String::new();
        let parts: Vec<&str> = path.split('/').collect();
        for p in &parts[..parts.len().saturating_sub(1)] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(p);
            self.dirs.insert(acc.clone());
        }
        self.root_exists = true;
    }

    fn store(&mut self, path: &str, bytes: Vec<u8>) {
        self.add_parents(path);
        let k = self.key(path);
        self.files.insert(k, (path.to_owned(), bytes));
    }
}

/// メモリ上の偽の端末 FS。`fail_after(n)` で n 回目以降の変更系操作を「切断」にする
/// （その操作は反映しない）。`reconnect()` まで以後の操作はすべて `NotConnected`。
/// 切断は操作境界でだけ起き、`sync` は何もしない（未 sync のデータの喪失は模さない）。
/// 耐久化の順序は `calls()` の呼び出し列を確かめるテストで固定する
pub struct FakeFs(Mutex<Inner>);

impl FakeFs {
    /// root がまだ無い端末
    pub fn new(capacity: u64) -> Self {
        FakeFs(Mutex::new(Inner {
            capacity,
            ..Default::default()
        }))
    }

    /// root はあるが空
    pub fn with_root(capacity: u64) -> Self {
        let fs = Self::new(capacity);
        fs.0.lock().unwrap().root_exists = true;
        fs
    }

    pub fn remove_for_test(&self, path: &str) {
        let mut g = self.0.lock().unwrap();
        let k = g.key(path);
        g.files.remove(&k);
    }

    pub fn rename_for_test(&self, from: &str, to: &str) {
        let mut g = self.0.lock().unwrap();
        let k = g.key(from);
        let (_, bytes) = g.files.remove(&k).unwrap();
        g.store(to, bytes);
    }

    /// SD カード（FAT / exFAT）のように大小文字を区別しない
    pub fn case_insensitive(self) -> Self {
        self.0.lock().unwrap().case_insensitive = true;
        self
    }

    /// 状態を複製する（切断点ごとの試験に使う）。数えた回数と注入はリセットする
    pub fn snapshot(&self) -> FakeFs {
        let mut g = self.0.lock().unwrap().clone();
        g.fail_after = None;
        g.mutations = 0;
        g.disconnected = false;
        g.calls.clear();
        g.rescans = 0;
        g.rescan_fails = false;
        FakeFs(Mutex::new(g))
    }

    pub fn set(&self, path: &str, bytes: &[u8]) {
        self.0.lock().unwrap().store(path, bytes.to_vec());
    }

    pub fn get(&self, path: &str) -> Option<Vec<u8>> {
        let g = self.0.lock().unwrap();
        g.files.get(&g.key(path)).map(|(_, b)| b.clone())
    }

    /// 表示のパスの一覧（昇順）
    pub fn paths(&self) -> Vec<String> {
        let g = self.0.lock().unwrap();
        let mut v: Vec<String> = g.files.values().map(|(p, _)| p.clone()).collect();
        v.sort();
        v
    }

    pub fn dirs(&self) -> Vec<String> {
        self.0.lock().unwrap().dirs.iter().cloned().collect()
    }

    pub fn fail_after(&self, n: usize) {
        let mut g = self.0.lock().unwrap();
        g.fail_after = Some(n);
        g.mutations = 0;
    }

    pub fn reconnect(&self) {
        let mut g = self.0.lock().unwrap();
        g.disconnected = false;
        g.fail_after = None;
    }

    pub fn mutations(&self) -> usize {
        self.0.lock().unwrap().mutations
    }

    pub fn rescans(&self) -> usize {
        self.0.lock().unwrap().rescans
    }

    pub fn fail_rescan(&self) {
        self.0.lock().unwrap().rescan_fails = true;
    }

    pub fn calls(&self) -> Vec<String> {
        self.0.lock().unwrap().calls.clone()
    }

    fn look<T>(&self, name: &str, f: impl FnOnce(&Inner) -> T) -> RemoteResult<T> {
        let mut g = self.0.lock().unwrap();
        if g.disconnected {
            return Err(RemoteError::NotConnected);
        }
        g.calls.push(name.to_owned());
        Ok(f(&g))
    }

    fn mutate<T>(
        &self,
        name: &str,
        f: impl FnOnce(&mut Inner) -> RemoteResult<T>,
    ) -> RemoteResult<T> {
        let mut g = self.0.lock().unwrap();
        if g.disconnected {
            return Err(RemoteError::NotConnected);
        }
        if g.fail_after.is_some_and(|n| g.mutations >= n) {
            g.disconnected = true;
            return Err(RemoteError::NotConnected);
        }
        g.mutations += 1;
        g.calls.push(name.to_owned());
        f(&mut g)
    }
}

impl DeviceFs for FakeFs {
    async fn read(&self, path: &str) -> RemoteResult<Option<Vec<u8>>> {
        self.look(&format!("read {path}"), |g| {
            g.files.get(&g.key(path)).map(|(_, b)| b.clone())
        })
    }

    async fn write(&self, path: &str, bytes: &[u8]) -> RemoteResult<()> {
        self.mutate(&format!("write {path}"), |g| {
            let old = g.files.get(&g.key(path)).map_or(0, |(_, b)| b.len() as u64);
            if g.used() - old + bytes.len() as u64 > g.capacity {
                return Err(RemoteError::NoSpace);
            }
            g.store(path, bytes.to_vec());
            Ok(())
        })
    }

    async fn append(&self, path: &str, bytes: &[u8]) -> RemoteResult<()> {
        self.mutate(&format!("append {path}"), |g| {
            if g.used() + bytes.len() as u64 > g.capacity {
                return Err(RemoteError::NoSpace);
            }
            let k = g.key(path);
            let mut cur = g.files.get(&k).map(|(_, b)| b.clone()).unwrap_or_default();
            cur.extend_from_slice(bytes);
            g.store(path, cur);
            Ok(())
        })
    }

    async fn put(&self, path: &str, mut src: std::fs::File) -> RemoteResult<PutResult> {
        let mut bytes = Vec::new();
        src.read_to_end(&mut bytes)
            .map_err(|e| RemoteError::Failed(e.to_string()))?;
        self.mutate(&format!("put {path}"), |g| {
            let old = g.files.get(&g.key(path)).map_or(0, |(_, b)| b.len() as u64);
            if g.used() - old + bytes.len() as u64 > g.capacity {
                return Err(RemoteError::NoSpace);
            }
            let r = PutResult {
                size: bytes.len() as u64,
                sha256: sha256_hex(&bytes),
            };
            g.store(path, bytes);
            Ok(r)
        })
    }

    async fn sha256(&self, path: &str) -> RemoteResult<Option<String>> {
        self.look(&format!("sha256 {path}"), |g| {
            g.files.get(&g.key(path)).map(|(_, b)| sha256_hex(b))
        })
    }

    async fn list_files(&self) -> RemoteResult<Vec<RemoteFile>> {
        self.look("list", |g| {
            g.files
                .values()
                .map(|(p, b)| RemoteFile {
                    path: p.clone(),
                    size: b.len() as u64,
                })
                .collect()
        })
    }

    async fn rename(&self, from: &str, to: &str) -> RemoteResult<()> {
        self.mutate(&format!("rename {from} -> {to}"), |g| {
            let Some((_, bytes)) = g.files.remove(&g.key(from)) else {
                return Err(RemoteError::Failed(format!("mv: {from}: No such file")));
            };
            g.store(to, bytes);
            Ok(())
        })
    }

    async fn remove(&self, path: &str) -> RemoteResult<()> {
        self.mutate(&format!("remove {path}"), |g| {
            let k = g.key(path);
            g.files.remove(&k);
            Ok(())
        })
    }

    async fn prune_empty_dirs(&self) -> RemoteResult<()> {
        self.mutate("prune", |g| {
            let used: BTreeSet<String> = g
                .files
                .values()
                .flat_map(|(p, _)| {
                    let parts: Vec<&str> = p.split('/').collect();
                    (1..parts.len())
                        .map(|n| parts[..n].join("/"))
                        .collect::<Vec<_>>()
                })
                .collect();
            let keep: BTreeSet<String> = g
                .dirs
                .iter()
                .filter(|d| used.contains(*d) || *d == ".spindle" || d.starts_with(".spindle/"))
                .cloned()
                .collect();
            g.dirs = keep;
            Ok(())
        })
    }

    async fn sync(&self) -> RemoteResult<()> {
        self.look("sync", |_| ())
    }

    async fn free_bytes(&self) -> RemoteResult<u64> {
        self.look("free", |g| g.capacity.saturating_sub(g.used()))
    }

    async fn root_state(&self) -> RemoteResult<DirState> {
        self.look("root_state", |g| {
            if !g.root_exists {
                DirState::Missing
            } else if g.files.is_empty() && g.dirs.is_empty() {
                DirState::Empty
            } else {
                DirState::NonEmpty
            }
        })
    }

    async fn rescan(&self) -> RemoteResult<()> {
        let mut g = self.0.lock().unwrap();
        if g.disconnected {
            return Err(RemoteError::NotConnected);
        }
        if g.rescan_fails {
            return Err(RemoteError::Failed("am: 失敗".into()));
        }
        g.calls.push("rescan".to_owned());
        g.rescans += 1;
        Ok(())
    }
}

use std::collections::HashMap;
use std::io::{Seek, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

use spindle::device::plan::{runnable, Runnable, StoredPlan};
use spindle::device::recover::{recover, Expect};
use spindle::device::store::initialize;
use spindle::device::sync::{
    self, Control, SourceError, Sources, SyncError, SyncInput, SyncReport,
};
use spindle::domain::device::{delivery_token, diff, DesiredItem, Manifest, Source, SourceKind};

/// track_id → 中身
#[derive(Clone, Default)]
pub struct MemSources(pub HashMap<i64, Vec<u8>>);

impl Sources for MemSources {
    fn open(&self, track_id: i64) -> Result<std::fs::File, SourceError> {
        let body = self.0.get(&track_id).ok_or(SourceError::Missing)?;
        let mut f = tempfile::tempfile().map_err(|e| SourceError::Other(e.to_string()))?;
        f.write_all(body)
            .map_err(|e| SourceError::Other(e.to_string()))?;
        f.rewind().map_err(|e| SourceError::Other(e.to_string()))?;
        Ok(f)
    }
}

/// `cancel_from` / `stale_from` 回目以降の呼び出しでキャンセル・generation 不一致にする
#[derive(Default)]
pub struct TestControl {
    pub cancel_from: Option<usize>,
    pub stale_from: Option<usize>,
    pub cancel_calls: AtomicUsize,
    pub gen_calls: AtomicUsize,
}

impl Control for TestControl {
    fn cancelled(&self) -> bool {
        let n = self.cancel_calls.fetch_add(1, Ordering::SeqCst) + 1;
        self.cancel_from.is_some_and(|k| n >= k)
    }

    async fn generation_ok(&self) -> bool {
        let n = self.gen_calls.fetch_add(1, Ordering::SeqCst) + 1;
        !self.stale_from.is_some_and(|k| n >= k)
    }

    async fn progress(&self, _done: u64, _total: u64) {}
}

pub fn expect() -> Expect {
    Expect {
        device_uuid: "u1".into(),
        volume: "emulated".into(),
    }
}

/// 目標の状態（track_id, 端末上のパス, 中身）
pub type Want<'a> = (i64, &'a str, &'a [u8]);

pub fn desired(want: &[Want]) -> Manifest {
    let mut m = Manifest::default();
    for (id, path, body) in want {
        let sha = sha256_hex(body);
        m.desired.push(DesiredItem {
            track_id: *id,
            source: Source {
                kind: SourceKind::Master,
                root_rel_path: (*path).to_owned(),
                semantic: "s".into(),
            },
            dest_path: (*path).to_owned(),
            dest_key: canonical_key(path),
            token: delivery_token("s", &sha),
            size: body.len() as u64,
            sha256: sha,
        });
    }
    m.desired.sort_by_key(|d| d.track_id);
    m
}

pub fn sources(want: &[Want]) -> MemSources {
    MemSources(want.iter().map(|(id, _, b)| (*id, b.to_vec())).collect())
}

/// 回復 → 差分 → 計画 → 部分集合 → 実行（P5-3b の device_sync ジョブと同じ流れ）
pub async fn sync_to<C: Control>(
    fs: &FakeFs,
    want: &[Want<'_>],
    control: &C,
) -> Result<SyncReport, String> {
    let rec = recover(fs, &expect()).await.map_err(|e| e.to_string())?;
    let d = diff(&desired(want), &rec.items, &[], &[], Vec::new());
    let plan = StoredPlan::from_diff(1, "tok", &d).unwrap();
    let r: Runnable = runnable(&plan, &rec.items, &rec.playlists, &d);
    let bodies = HashMap::new();
    sync::run(
        fs,
        &sources(want),
        control,
        SyncInput {
            generation: 1,
            start: rec.manifest,
            runnable: &r,
            playlist_bodies: &bodies,
        },
    )
    .await
    .map_err(|e: SyncError| e.to_string())
}

/// 登録済みで `want` の状態にある端末
pub async fn device_at(want: &[Want<'_>], capacity: u64) -> FakeFs {
    let fs = FakeFs::new(capacity);
    initialize(&fs, "u1", "emulated").await.unwrap();
    sync_to(&fs, want, &TestControl::default()).await.unwrap();
    fs
}

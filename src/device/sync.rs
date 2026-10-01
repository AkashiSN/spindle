//! 保存した計画の実行（`device_sync` ジョブの本体。仕様 ⑤「同期」）。
//! 呼び出し側は先に回復（`recover`）し、その結果で差分を取り直して計画の部分集合（`plan::runnable`）を
//! 作ってから渡す。順序は 削除 → パス変更のバッチ → 更新 → 追加 → プレイリスト（先に空きを作る）。
//! 各操作は意図を耐久化してから副作用を起こす。`vacating` より前の書き込みの直前にキャンセルと
//! generation を確かめ、封印済みバッチの `vacating` 以降はどちらでも止めずに完遂する

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::db::devices::ReportedError;
use crate::device::journal::{
    member_digest, random_id, BatchMember, Intent, IntentOp, MemberOp, Phase, Record,
};
use crate::device::ondevice::{
    self, Book, DeviceManifest, ManifestItem, ManifestPlaylist, MOVING_DIR, TMP_SUFFIX,
};
use crate::device::plan::{path_components, PlanItem, PlanPlaylist, Runnable};
use crate::device::recover::complete_batch;
use crate::device::remote::{DeviceFs, RemoteError};
use crate::device::store::{append_durable, compact, StoreError};
use crate::domain::device::{
    peak_bytes, sha256_hex, DeviceItem, EntryKind, OpKind, PlaylistOpKind, PlaylistState,
    SourceHash, SourceKind,
};
use crate::domain::relpath::{canonical_key, RelPath};
use crate::fsroot::{fstat, FileKind, FsError, RootDir};

/// 見積もりに足す余裕（manifest・ジャーナルの分を含めた安全側の値）
pub const MARGIN_BYTES: u64 = 64 * 1024 * 1024;
/// 完了した操作がこれだけ溜まったら manifest を書き直してジャーナルを空にする
pub const COMPACT_EVERY: usize = 50;
/// 置き先を管理外のファイル（manifest に無い、ユーザが置いたもの）が占めているときの理由
pub const UNMANAGED_COLLISION: &str = "管理外のファイルと衝突";
/// 置き先を別の管理下の項目が占めているときの理由。差分は移動中の曲の今のパスを「空く」と扱うが、
/// その移動が実行されなかった（行き先が管理外・準備の失敗で縮んだ・再開時に外れた）ときに出る
pub const PATH_OCCUPIED: &str = "パス衝突（移動しなかった曲が置き先にいる）";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    #[error("送る元が無い")]
    Missing,
    #[error("送る元が変わった（ハッシュを取り直す）")]
    Changed,
    #[error("送る元を開けない: {0}")]
    Other(String),
}

impl SourceError {
    /// `device_errors.reason` に入れる文字列
    pub fn reason(&self) -> String {
        self.to_string()
    }
}

/// 送る元を開く。開いた FD の identity を `source_hashes` と照合してから返す
pub trait Sources {
    fn open(&self, track_id: i64) -> Result<std::fs::File, SourceError>;
}

/// 送る元の 1 曲（`db::devices::Computed.desired` と `source_hashes` から組み立てる）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEntry {
    pub kind: SourceKind,
    pub rel_path: RelPath,
    pub hash: SourceHash,
}

/// Library / Derived の root から `openat2` で開く送る元（仕様 ③「配信時の照合」）
pub struct RootSources {
    library: Arc<RootDir>,
    derived: Arc<RootDir>,
    entries: HashMap<i64, SourceEntry>,
}

impl RootSources {
    pub fn new(
        library: Arc<RootDir>,
        derived: Arc<RootDir>,
        entries: HashMap<i64, SourceEntry>,
    ) -> Self {
        Self {
            library,
            derived,
            entries,
        }
    }
}

impl Sources for RootSources {
    fn open(&self, track_id: i64) -> Result<std::fs::File, SourceError> {
        let e = self.entries.get(&track_id).ok_or(SourceError::Missing)?;
        let root = match e.kind {
            SourceKind::Master => &self.library,
            SourceKind::Derived(_) => &self.derived,
        };
        let file = root
            .open_file_nonblocking(&e.rel_path)
            .map_err(|err| match err {
                FsError::NotFound => SourceError::Missing,
                // symlink に差し替わった送る元は、もう読んでよい通常ファイルではない。「変わった」として扱い、
                // ハッシュ行を消して取り直させる（Other にすると 500 のまま行が残り続ける）
                FsError::Symlink => SourceError::Changed,
                other => SourceError::Other(other.to_string()),
            })?;
        let st = fstat(&file).map_err(|err| SourceError::Other(err.to_string()))?;
        // ディレクトリ・FIFO・デバイスなど通常ファイルでないものに差し替わった場合も「変わった」
        // （FIFO は O_NONBLOCK で開いているのでここまで止まらずに来る）
        if st.kind != FileKind::File {
            return Err(SourceError::Changed);
        }
        let h = &e.hash;
        if (st.inode, st.size, st.mtime_ns, st.ctime_ns)
            != (h.inode, h.size, h.mtime_ns, h.ctime_ns)
        {
            return Err(SourceError::Changed);
        }
        Ok(file)
    }
}

/// 実行の制御（ジョブのキャンセル・端末の generation・進捗）
#[allow(async_fn_in_trait)]
pub trait Control {
    /// ジョブのキャンセル。ジョブのキャンセルはここだけで効かせる（`vacating` より前の書き込みの直前にだけ
    /// 見るので、封印済みバッチの `vacating` 以降は止めずに完遂できる）。`AdbFs` に渡すトークンは
    /// プロセスの停止用で、ジョブのキャンセルトークンを渡さない（渡すと `vacating` 以降の `mv` まで止まる）
    fn cancelled(&self) -> bool;
    /// 開始時の generation のままか。`vacating` より前の書き込みの直前に呼ぶ
    async fn generation_ok(&self) -> bool;
    async fn progress(&self, done: u64, total: u64);
}

pub struct SyncInput<'a> {
    pub generation: i64,
    /// 回復済みの正本（`recover` の結果）
    pub start: DeviceManifest,
    pub runnable: &'a Runnable,
    /// プレイリストの中身（`Computed.playlists` の playlist_id → body）
    pub playlist_bodies: &'a HashMap<i64, Vec<u8>>,
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("端末の空き容量が足りない（必要 {need} バイト、空き {free} バイト）")]
    NoSpace { need: u64, free: u64 },
    #[error("キャンセルされた")]
    Cancelled,
    #[error("端末の設定が変わったので中断した")]
    GenerationChanged,
    #[error("端末のジャーナルが回復されていない（先に回復すること）")]
    NotRecovered,
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl SyncError {
    /// 端末が切れたための失敗か（ジョブを待ちへ戻して、繋がり直したら再開する判定に使う）
    pub fn is_not_connected(&self) -> bool {
        matches!(
            self,
            SyncError::Store(StoreError::Remote(RemoteError::NotConnected))
        )
    }
}

impl From<RemoteError> for SyncError {
    fn from(e: RemoteError) -> Self {
        SyncError::Store(StoreError::Remote(e))
    }
}

impl From<std::io::Error> for SyncError {
    fn from(e: std::io::Error) -> Self {
        SyncError::Store(StoreError::Remote(RemoteError::Failed(e.to_string())))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    /// 同期の後の正本（書き直し済み）
    pub manifest: DeviceManifest,
    /// この同期で出た項目ごとのエラー（`device_errors` を全置換する）
    pub errors: Vec<ReportedError>,
    /// 同期は成功扱いの警告（Poweramp の再スキャンの失敗など）
    pub warnings: Vec<String>,
    /// 送る元の identity か中身が `source_hashes` と違った曲（昇順・重複なし）。
    /// 呼び出し側は `source_hashes` の行を捨ててハッシュを取り直させる
    pub rehash: Vec<i64>,
}

impl SyncReport {
    pub fn items(&self) -> Vec<DeviceItem> {
        Book::from(self.manifest.clone()).device_items()
    }

    pub fn playlists(&self) -> Vec<PlaylistState> {
        Book::from(self.manifest.clone()).playlist_states()
    }
}

struct Exec<'a, F, S, C> {
    fs: &'a F,
    sources: &'a S,
    control: &'a C,
    generation: i64,
    book: Book,
    /// 次の追記に載せる完了
    pending_done: Vec<Record>,
    since_compact: usize,
    errors: Vec<ReportedError>,
    done: u64,
    total: u64,
    /// 開始時に端末にあった管理外のファイルの `canonical_key`（上書きしない）
    unmanaged: HashSet<String>,
    /// 送る元が変わっていた曲（ハッシュの取り直し対象）
    rehash: std::collections::BTreeSet<i64>,
}

pub async fn run<F: DeviceFs, S: Sources, C: Control>(
    fs: &F,
    sources: &S,
    control: &C,
    input: SyncInput<'_>,
) -> Result<SyncReport, SyncError> {
    let r = input.runnable;
    let current = Book::from(input.start.clone()).device_items();
    let ops: Vec<(OpKind, i64, u64)> = r.items.iter().map(|o| (o.op, o.track_id, o.size)).collect();
    let playlist_bytes: u64 = r
        .playlists
        .iter()
        .filter_map(|p| input.playlist_bodies.get(&p.playlist_id))
        .map(|b| b.len() as u64)
        .sum();
    let manifest_bytes = ondevice::render(&input.start)
        .map_err(StoreError::from)?
        .len() as u64;
    let need =
        peak_bytes(&ops, &current).peak_bytes + playlist_bytes + manifest_bytes + MARGIN_BYTES;
    let free = fs.free_bytes().await?;
    if need > free {
        return Err(SyncError::NoSpace { need, free });
    }

    let managed = Book::from(input.start.clone()).path_keys();
    let unmanaged: HashSet<String> = fs
        .list_files()
        .await?
        .into_iter()
        .filter(|f| !ondevice::is_reserved(&f.path) && !f.path.ends_with(TMP_SUFFIX))
        .map(|f| canonical_key(&f.path))
        .filter(|k| !managed.contains(k))
        .collect();

    let mut x = Exec {
        fs,
        sources,
        control,
        generation: input.generation,
        book: Book::from(input.start),
        pending_done: Vec::new(),
        since_compact: 0,
        errors: Vec::new(),
        done: 0,
        total: (r.items.len() + r.playlists.len()) as u64,
        unmanaged,
        rehash: std::collections::BTreeSet::new(),
    };
    let of = |k: OpKind| r.items.iter().filter(move |o| o.op == k);
    for op in of(OpKind::Delete) {
        x.guard().await?;
        x.delete(op).await?;
        x.tick().await?;
    }
    let path_changes: Vec<PlanItem> = r
        .items
        .iter()
        .filter(|o| matches!(o.op, OpKind::Move | OpKind::UpdateMove))
        .cloned()
        .collect();
    if !path_changes.is_empty() {
        x.path_batch(path_changes).await?;
    }
    for op in of(OpKind::Update).chain(of(OpKind::Add)) {
        x.guard().await?;
        x.put_track(op).await?;
        x.tick().await?;
    }
    // プレイリスト: 書き込みが成り立たないものを破壊の前に保留にし（旧パスも残す）、
    // 旧パスの削除（削除と、パスの変わる更新）を全部先に、それから書き込み
    let held = hold_playlists(&r.playlists, input.playlist_bodies, &x.unmanaged, &x.book);
    for p in &r.playlists {
        if p.op == PlaylistOpKind::Delete || (renamed(p) && !held.contains_key(&p.playlist_id)) {
            x.guard().await?;
            x.remove_playlist(p).await?;
        }
    }
    for p in &r.playlists {
        if let Some(reason) = held.get(&p.playlist_id) {
            if let Some(reason) = reason {
                x.errors
                    .push((EntryKind::Playlist, p.playlist_id, reason.clone()));
            }
        } else if p.op != PlaylistOpKind::Delete {
            x.guard().await?;
            x.put_playlist(p, input.playlist_bodies).await?;
        }
        x.tick().await?;
    }
    x.fs.prune_empty_dirs().await?;
    let manifest = x.book.manifest();
    compact(x.fs, &manifest).await?;
    let mut warnings = Vec::new();
    if let Err(e) = x.fs.rescan().await {
        tracing::warn!(error = %e, "Poweramp の再スキャンを頼めなかった");
        warnings.push(format!("Poweramp の再スキャンを頼めなかった: {e}"));
    }
    Ok(SyncReport {
        manifest,
        errors: x.errors,
        warnings,
        rehash: x.rehash.into_iter().collect(),
    })
}

impl<F: DeviceFs, S: Sources, C: Control> Exec<'_, F, S, C> {
    /// `vacating` より前の書き込みの直前に呼ぶ
    async fn guard(&self) -> Result<(), SyncError> {
        if self.control.cancelled() {
            return Err(SyncError::Cancelled);
        }
        if !self.control.generation_ok().await {
            return Err(SyncError::GenerationChanged);
        }
        Ok(())
    }

    async fn tick(&mut self) -> Result<(), SyncError> {
        self.done += 1;
        self.control.progress(self.done, self.total).await;
        if self.since_compact >= COMPACT_EVERY {
            compact(self.fs, &self.book.manifest()).await?;
            self.pending_done.clear();
            self.since_compact = 0;
        }
        Ok(())
    }

    /// 溜めた完了と一緒に意図を耐久化する
    async fn intent(&mut self, intent: Intent) -> Result<(), SyncError> {
        let mut records = std::mem::take(&mut self.pending_done);
        records.push(Record::Intent(intent));
        append_durable(self.fs, &records).await?;
        Ok(())
    }

    fn finished(&mut self, op_id: &str) {
        self.pending_done.push(Record::Done {
            op_id: op_id.to_owned(),
        });
        self.since_compact += 1;
    }

    fn rm_intent(&self, op_id: &str, kind: EntryKind, ref_id: i64, from: &str) -> Intent {
        Intent {
            op_id: op_id.to_owned(),
            generation: self.generation,
            op: IntentOp::Rm,
            kind,
            ref_id,
            from: Some(from.to_owned()),
            to: None,
            token: None,
            size: None,
            sha256: None,
        }
    }

    async fn delete(&mut self, op: &PlanItem) -> Result<(), SyncError> {
        let Some(from) = op.from.as_deref() else {
            return Ok(());
        };
        let intent = self.rm_intent(&op.op_id, EntryKind::Track, op.track_id, from);
        self.intent(intent).await?;
        self.fs.remove(from).await?;
        self.fs.sync().await?;
        let key = canonical_key(from);
        if self
            .book
            .items
            .get(&op.track_id)
            .is_some_and(|i| canonical_key(&i.path) == key)
        {
            self.book.items.remove(&op.track_id);
        }
        self.finished(&op.op_id);
        Ok(())
    }

    /// 送る元を開いて `path` へ送り、送りながらのハッシュと端末側の sha256 を期待値と照合する。
    /// 項目のエラーは `Ok(Err(理由))`（その曲だけ保留にして続ける）
    async fn transfer(
        &mut self,
        track_id: i64,
        path: &str,
        size: u64,
        sha256: &str,
    ) -> Result<Result<(), String>, SyncError> {
        let file = match self.sources.open(track_id) {
            Ok(f) => f,
            Err(e) => {
                if matches!(e, SourceError::Changed) {
                    self.rehash.insert(track_id);
                }
                return Ok(Err(e.reason()));
            }
        };
        let put = match self.fs.put(path, file).await {
            Ok(p) => p,
            Err(e) => {
                if let Err(re) = self.fs.remove(path).await {
                    tracing::warn!(error = %re, path, "送りそこねた一時ファイルを消せなかった");
                }
                return Err(e.into());
            }
        };
        if put.sha256 != sha256 || put.size != size {
            self.fs.remove(path).await?;
            self.rehash.insert(track_id);
            return Ok(Err(SourceError::Changed.reason()));
        }
        if self.fs.sha256(path).await?.as_deref() != Some(sha256) {
            self.fs.remove(path).await?;
            return Ok(Err("端末に書いた内容が一致しない".to_owned()));
        }
        Ok(Ok(()))
    }

    async fn put_track(&mut self, op: &PlanItem) -> Result<(), SyncError> {
        let (Some(to), Some(token), Some(sha)) =
            (op.to.clone(), op.token.clone(), op.sha256.clone())
        else {
            return Ok(());
        };
        if self.unmanaged.contains(&canonical_key(&to)) {
            self.errors.push((
                EntryKind::Track,
                op.track_id,
                UNMANAGED_COLLISION.to_owned(),
            ));
            return Ok(());
        }
        // 空くはずだった置き先に、移動しなかった曲がまだいる（上書きすると manifest のパスが重複する）
        if self
            .book
            .occupied_by_other(EntryKind::Track, op.track_id, &to)
        {
            self.errors
                .push((EntryKind::Track, op.track_id, PATH_OCCUPIED.to_owned()));
            return Ok(());
        }
        // 先に開けるか確かめる（開けない曲に意図を書かない）
        if let Err(e) = self.sources.open(op.track_id) {
            if matches!(e, SourceError::Changed) {
                self.rehash.insert(op.track_id);
            }
            self.errors
                .push((EntryKind::Track, op.track_id, e.reason()));
            return Ok(());
        }
        self.intent(Intent {
            op_id: op.op_id.clone(),
            generation: self.generation,
            op: IntentOp::Put,
            kind: EntryKind::Track,
            ref_id: op.track_id,
            from: None,
            to: Some(to.clone()),
            token: Some(token.clone()),
            size: Some(op.size),
            sha256: Some(sha.clone()),
        })
        .await?;
        let tmp = format!("{to}{TMP_SUFFIX}");
        if let Err(reason) = self.transfer(op.track_id, &tmp, op.size, &sha).await? {
            self.errors.push((EntryKind::Track, op.track_id, reason));
            return Ok(());
        }
        self.fs.sync().await?;
        self.fs.rename(&tmp, &to).await?;
        self.fs.sync().await?;
        self.book.items.insert(
            op.track_id,
            ManifestItem {
                track_id: op.track_id,
                path: to,
                token,
                size: op.size,
                sha256: sha,
            },
        );
        self.finished(&op.op_id);
        Ok(())
    }

    async fn remove_playlist(&mut self, p: &PlanPlaylist) -> Result<(), SyncError> {
        let Some(from) = p.from.as_deref() else {
            return Ok(());
        };
        // 削除とパスの変わる更新で同じ op_id を使わないよう、旧パスの削除には別の op_id を振る
        let op_id = random_id()?;
        let intent = self.rm_intent(&op_id, EntryKind::Playlist, p.playlist_id, from);
        self.intent(intent).await?;
        self.fs.remove(from).await?;
        self.fs.sync().await?;
        let key = canonical_key(from);
        if self
            .book
            .playlists
            .get(&p.playlist_id)
            .is_some_and(|q| canonical_key(&q.path) == key)
        {
            self.book.playlists.remove(&p.playlist_id);
        }
        self.finished(&op_id);
        Ok(())
    }

    async fn put_playlist(
        &mut self,
        p: &PlanPlaylist,
        bodies: &HashMap<i64, Vec<u8>>,
    ) -> Result<(), SyncError> {
        let (Some(to), Some(token)) = (p.to.clone(), p.token.clone()) else {
            return Ok(());
        };
        let Some(body) = bodies.get(&p.playlist_id) else {
            self.errors.push((
                EntryKind::Playlist,
                p.playlist_id,
                "プレイリストの中身が無い".to_owned(),
            ));
            return Ok(());
        };
        if self.unmanaged.contains(&canonical_key(&to)) {
            self.errors.push((
                EntryKind::Playlist,
                p.playlist_id,
                UNMANAGED_COLLISION.to_owned(),
            ));
            return Ok(());
        }
        if self
            .book
            .occupied_by_other(EntryKind::Playlist, p.playlist_id, &to)
        {
            self.errors
                .push((EntryKind::Playlist, p.playlist_id, PATH_OCCUPIED.to_owned()));
            return Ok(());
        }
        let sha = sha256_hex(body);
        self.intent(Intent {
            op_id: p.op_id.clone(),
            generation: self.generation,
            op: IntentOp::Put,
            kind: EntryKind::Playlist,
            ref_id: p.playlist_id,
            from: None,
            to: Some(to.clone()),
            token: Some(token.clone()),
            size: Some(body.len() as u64),
            sha256: Some(sha.clone()),
        })
        .await?;
        let tmp = format!("{to}{TMP_SUFFIX}");
        self.fs.write(&tmp, body).await?;
        if self.fs.sha256(&tmp).await?.as_deref() != Some(sha.as_str()) {
            self.fs.remove(&tmp).await?;
            self.errors.push((
                EntryKind::Playlist,
                p.playlist_id,
                "端末に書いた内容が一致しない".to_owned(),
            ));
            return Ok(());
        }
        self.fs.sync().await?;
        self.fs.rename(&tmp, &to).await?;
        self.fs.sync().await?;
        self.book.playlists.insert(
            p.playlist_id,
            ManifestPlaylist {
                playlist_id: p.playlist_id,
                path: to,
                token,
            },
        );
        self.finished(&p.op_id);
        Ok(())
    }

    /// パス変更を 1 つのバッチで進める（仕様 ⑤「パス変更は大域的な 2 段」）。
    /// 記録 → prepared（更新 + 移動の新しい内容を `new` へ）→ vacating → vacated → placing → done。
    /// 行き先を管理外のファイルが占めるメンバーは、封印の前にその成分ごと外す（上書きしない）。
    /// 準備で失敗したメンバーがいれば、その曲を含む成分を全員外し、旧バッチを `batch_abort`
    /// （`superseded_by` 付き）で閉じて一時ファイルを消してから、残りで新しいバッチを記録する
    async fn path_batch(&mut self, ops: Vec<PlanItem>) -> Result<(), SyncError> {
        let total = ops.len() as u64;
        let blocked: HashSet<i64> = ops
            .iter()
            .filter(|o| {
                o.to.as_deref()
                    .is_some_and(|to| self.unmanaged.contains(&canonical_key(to)))
            })
            .map(|o| o.track_id)
            .collect();
        for o in ops.iter().filter(|o| blocked.contains(&o.track_id)) {
            self.errors
                .push((EntryKind::Track, o.track_id, UNMANAGED_COLLISION.to_owned()));
        }
        let mut ops = drop_components(ops, &blocked);
        let mut superseded: Option<(String, Vec<BatchMember>)> = None;
        loop {
            if ops.is_empty() {
                if let Some((old, members)) = superseded.take() {
                    self.abort_batch(&old, None, &members).await?;
                }
                self.done += total;
                self.control.progress(self.done, self.total).await;
                return Ok(());
            }
            self.guard().await?;
            let batch_id = random_id()?;
            if let Some((old, members)) = superseded.take() {
                self.abort_batch(&old, Some(&batch_id), &members).await?;
            }
            let members = batch_members(&batch_id, &ops);
            let mut records = std::mem::take(&mut self.pending_done);
            records.push(Record::BatchBegin {
                batch_id: batch_id.clone(),
            });
            records.extend(members.iter().cloned().map(Record::BatchMember));
            records.push(Record::BatchSealed {
                batch_id: batch_id.clone(),
                count: members.len(),
                digest: member_digest(&members).map_err(StoreError::from)?,
            });
            append_durable(self.fs, &records).await?;

            let mut failed: HashSet<i64> = HashSet::new();
            for m in &members {
                let Some(new) = &m.new else { continue };
                self.guard().await?;
                if let Err(reason) = self.transfer(m.track_id, new, m.size, &m.sha256).await? {
                    self.errors.push((EntryKind::Track, m.track_id, reason));
                    failed.insert(m.track_id);
                }
            }
            if failed.is_empty() {
                let phase = |phase| Record::BatchPhase {
                    batch_id: batch_id.clone(),
                    phase,
                };
                append_durable(self.fs, &[phase(Phase::Prepared)]).await?;
                self.guard().await?;
                append_durable(self.fs, &[phase(Phase::Vacating)]).await?;
                // ここから先はキャンセルも generation も見ずに完遂する
                complete_batch(
                    self.fs,
                    &batch_id,
                    &members,
                    Phase::Vacating,
                    false,
                    &mut self.book,
                )
                .await?;
                self.done += total;
                self.control.progress(self.done, self.total).await;
                return Ok(());
            }
            ops = drop_components(ops, &failed);
            superseded = Some((batch_id, members));
        }
    }

    /// 先に旧バッチの終端を耐久化し、その後で旧バッチの一時ファイルを消す
    async fn abort_batch(
        &mut self,
        batch_id: &str,
        superseded_by: Option<&str>,
        members: &[BatchMember],
    ) -> Result<(), SyncError> {
        append_durable(
            self.fs,
            &[Record::BatchAbort {
                batch_id: batch_id.to_owned(),
                superseded_by: superseded_by.map(str::to_owned),
            }],
        )
        .await?;
        for m in members {
            if let Some(new) = &m.new {
                self.fs.remove(new).await?;
            }
        }
        Ok(())
    }
}

/// パスの変わるプレイリストの更新か
fn renamed(p: &PlanPlaylist) -> bool {
    p.op == PlaylistOpKind::Update && p.from.is_some() && p.from != p.to
}

/// 書き込み（追加・更新）が成り立たないプレイリストを、旧パスを消す前に決める。
/// 値は項目のエラーの理由（`None` は計画の形が欠けていて黙って飛ばすもの）。
/// 成り立たないのは: 中身が無い・行き先が管理外のファイル・行き先を今回空かない別の管理下の項目が占める。
/// 空くのは、今回実行する削除と（保留にならない）改名の旧パスだけ。ある改名が保留になるとその旧パスは
/// 空かなくなり、そこへ来る改名も保留になるので、不動点まで繰り返す（入れ替え・循環は全員が空けるので通る）
fn hold_playlists(
    ops: &[PlanPlaylist],
    bodies: &HashMap<i64, Vec<u8>>,
    unmanaged: &HashSet<String>,
    book: &Book,
) -> HashMap<i64, Option<String>> {
    let mut held: HashMap<i64, Option<String>> = HashMap::new();
    for p in ops.iter().filter(|p| p.op != PlaylistOpKind::Delete) {
        let (Some(to), Some(_)) = (p.to.as_deref(), p.token.as_deref()) else {
            held.insert(p.playlist_id, None);
            continue;
        };
        if !bodies.contains_key(&p.playlist_id) {
            held.insert(p.playlist_id, Some("プレイリストの中身が無い".to_owned()));
        } else if unmanaged.contains(&canonical_key(to)) {
            held.insert(p.playlist_id, Some(UNMANAGED_COLLISION.to_owned()));
        }
    }
    loop {
        // 今回空くパス（削除と、保留でない改名の旧パス）
        let vacated: HashSet<String> = ops
            .iter()
            .filter(|p| {
                p.op == PlaylistOpKind::Delete || (renamed(p) && !held.contains_key(&p.playlist_id))
            })
            .filter_map(|p| p.from.as_deref().map(canonical_key))
            .collect();
        let newly: Vec<i64> = ops
            .iter()
            .filter(|p| p.op != PlaylistOpKind::Delete && !held.contains_key(&p.playlist_id))
            .filter(|p| {
                let Some(to) = p.to.as_deref() else {
                    return false;
                };
                let key = canonical_key(to);
                let track = book.items.values().any(|i| canonical_key(&i.path) == key);
                let playlist = book.playlists.values().any(|q| {
                    q.playlist_id != p.playlist_id
                        && canonical_key(&q.path) == key
                        && !vacated.contains(&key)
                });
                track || playlist
            })
            .map(|p| p.playlist_id)
            .collect();
        if newly.is_empty() {
            return held;
        }
        for id in newly {
            held.insert(id, Some(PATH_OCCUPIED.to_owned()));
        }
    }
}

/// `tracks` の曲を含む成分（入れ替え・循環・連鎖）を全員外す
fn drop_components(ops: Vec<PlanItem>, tracks: &HashSet<i64>) -> Vec<PlanItem> {
    if tracks.is_empty() {
        return ops;
    }
    let drop: HashSet<usize> = path_components(&ops)
        .into_iter()
        .filter(|c| c.iter().any(|&i| tracks.contains(&ops[i].track_id)))
        .flatten()
        .collect();
    ops.into_iter()
        .enumerate()
        .filter(|(i, _)| !drop.contains(i))
        .map(|(_, o)| o)
        .collect()
}

fn batch_members(batch_id: &str, ops: &[PlanItem]) -> Vec<BatchMember> {
    ops.iter()
        .filter_map(|o| {
            let op = match o.op {
                OpKind::Move => MemberOp::Move,
                OpKind::UpdateMove => MemberOp::UpdateMove,
                _ => return None,
            };
            // 名前に batch_id を含める: 縮めた後の新バッチが旧バッチ（中止済み）と一時ファイルを共有すると、
            // 回復が旧バッチの掃除として新バッチの new を消してしまう（D-95 の P5-3a 追記）
            let staging = format!("{MOVING_DIR}/{batch_id}-{}", o.op_id);
            Some(BatchMember {
                batch_id: batch_id.to_owned(),
                op_id: o.op_id.clone(),
                op,
                track_id: o.track_id,
                from: o.from.clone()?,
                new: (op == MemberOp::UpdateMove).then(|| format!("{staging}.new")),
                staging,
                to: o.to.clone()?,
                token: o.token.clone()?,
                size: o.size,
                sha256: o.sha256.clone()?,
            })
        })
        .collect()
}

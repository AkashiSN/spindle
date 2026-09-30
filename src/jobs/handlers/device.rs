//! 端末ジョブ（P5-3b、仕様 ⑤、D-98）: `device_scan`（回復してキャッシュを置き換える）、`device_sync`（確定した
//! 計画を実行）、`device_verify`（全曲の sha256 を検証）。どれも端末ごとの排他（`job_mutexes` の
//! `device:<id>` と `AdbRuntime::device_lock`）の下で、最初に回復を行う。未接続なら attempts を消費せずに
//! `DISCONNECTED_REQUEUE_SECS` 待つ（scan は待たずに終わる。次の接続でまた投入される）

use std::collections::HashMap;
use std::sync::Arc;

use rusqlite::OptionalExtension as _;

use crate::db::devices::{self as dbdev, Device, PlanEnd};
use crate::db::{now_epoch, Db, DbError};
use crate::device::adb::AdbFs;
use crate::device::plan::runnable;
use crate::device::recover::{recover, Expect, RecoverError, Recovered};
use crate::device::remote::{DeviceFs, RemoteError};
use crate::device::runtime::AdbRuntime;
use crate::device::store::StoreError;
use crate::device::sync::{self, Control, RootSources, SourceEntry, SyncError, SyncInput};
use crate::device::verify::verify;
use crate::domain::device::{OpKind, SourceKind, Transport};
use crate::domain::relpath::RelPath;
use crate::fsroot::RootDir;
use crate::jobs::{
    device_mutex, BoxFuture, Handler, HandlerResult, JobContext, JobError, JobType, Jobs, NewJob,
    Outcome, DISCONNECTED_REQUEUE_SECS,
};

/// 3 つの端末ジョブが共有する依存
pub struct DeviceEnv {
    pub db: Arc<Db>,
    pub jobs: Arc<Jobs>,
    pub rt: Arc<AdbRuntime>,
    pub library: Arc<RootDir>,
    pub derived: Arc<RootDir>,
}

pub fn scan_job(device_id: i64) -> NewJob {
    NewJob::new(
        JobType::DeviceScan,
        serde_json::json!({ "device_id": device_id }),
    )
    .dedup_key(format!("device_scan:{device_id}"))
}

pub fn sync_job(device_id: i64, plan_id: i64) -> NewJob {
    NewJob::new(
        JobType::DeviceSync,
        serde_json::json!({ "device_id": device_id, "plan_id": plan_id }),
    )
    .dedup_key(format!("device_sync:{device_id}"))
    .max_attempts(3)
}

pub fn verify_job(device_id: i64) -> NewJob {
    NewJob::new(
        JobType::DeviceVerify,
        serde_json::json!({ "device_id": device_id }),
    )
    .dedup_key(format!("device_verify:{device_id}"))
}

/// 端末が繋がった（`track-devices` で `device` になった）。登録済みなら差分の計算（`device_scan`）を投入し、
/// 待機中の同期・検証を前倒しする。未登録なら何もしない
pub async fn on_connected(db: &Arc<Db>, jobs: &Arc<Jobs>, serial: String) {
    let found = db.read(move |c| dbdev::find_by_serial(c, &serial)).await;
    match found {
        Ok(Some(d)) => {
            if let Err(e) = jobs.enqueue(scan_job(d.id)).await {
                tracing::warn!(device_id = d.id, error = %e, "端末の差分の計算を投入できない");
            }
            if let Err(e) = jobs.wake_device(d.id).await {
                tracing::warn!(device_id = d.id, error = %e, "待機中の同期を前倒しできない");
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "接続した端末を引けない"),
    }
}

fn fatal(msg: impl Into<String>) -> JobError {
    JobError::Fatal(anyhow::anyhow!(msg.into()))
}

/// `Send` であることをコンパイル時に固定する（エンジンを `AdbFs` / `RootSources` の具体型で呼んでいること）
fn assert_send<T: Send>(t: T) -> T {
    t
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Scan,
    Sync,
    Verify,
}

/// 未接続のときの結果。scan は待たずに終わる（次の接続でまた投入される）
fn disconnected(kind: Kind) -> HandlerResult {
    match kind {
        Kind::Scan => Ok(Outcome::DoneWith("未接続".into())),
        Kind::Sync | Kind::Verify => Ok(Outcome::RequeueAfter(DISCONNECTED_REQUEUE_SECS)),
    }
}

/// 同期・検証の失敗をジョブの結果に写す。計画は open のまま（再開・破棄は API）
fn sync_failure(kind: Kind, e: SyncError) -> HandlerResult {
    if e.is_not_connected() {
        return disconnected(kind);
    }
    match e {
        SyncError::Cancelled => Err(JobError::Cancelled),
        // 再試行しても変わらない
        e @ (SyncError::GenerationChanged | SyncError::NoSpace { .. }) => {
            Err(JobError::Fatal(anyhow::Error::new(e)))
        }
        e => Err(JobError::Failed(anyhow::Error::new(e))),
    }
}

fn payload_i64(ctx: &JobContext, key: &str) -> Result<i64, JobError> {
    ctx.job
        .payload
        .get(key)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| fatal(format!("payload に {key} が無い")))
}

/// `sync::Control` の実装。ジョブのキャンセルはここ（`cancelled`）だけで効かせる
struct JobControl {
    ctx: JobContext,
    db: Arc<Db>,
    device_id: i64,
    generation: i64,
}

impl Control for JobControl {
    fn cancelled(&self) -> bool {
        self.ctx.is_cancel_requested()
    }

    async fn generation_ok(&self) -> bool {
        let (id, g) = (self.device_id, self.generation);
        self.db
            .read(move |c| Ok(dbdev::get(c, id)?.map(|d| d.generation) == Some(g)))
            .await
            .unwrap_or(false)
    }

    async fn progress(&self, done: u64, total: u64) {
        // キャンセルは cancelled() で拾う（ここでの Err は無視してよい）
        let _ = self.ctx.progress(done as i64, total as i64).await;
    }
}

/// `prepare` の結果。`Err` はそのままハンドラの結果にする
struct Prepared {
    device: Device,
    fs: AdbFs,
    rec: Recovered,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

/// 3 つのジョブの共通の前段: 端末の行と排他を取り、接続を確かめ、回復してキャッシュを置き換える
async fn prepare(
    env: &DeviceEnv,
    ctx: &JobContext,
    device_id: i64,
    kind: Kind,
) -> Result<Prepared, HandlerResult> {
    let device = match env.db.read(move |c| dbdev::get(c, device_id)).await {
        Ok(Some(d)) => d,
        Ok(None) => return Err(Ok(Outcome::DoneWith("端末が削除された".into()))),
        Err(e) => return Err(Err(e.into())),
    };
    if device.transport != Transport::Adb {
        return Err(Err(fatal(format!(
            "端末 {device_id} は adb の端末ではない"
        ))));
    }
    let (Some(serial), Some(volume), Some(root)) = (
        device.adb_serial.clone(),
        device.adb_volume.clone(),
        device.adb_root.clone(),
    ) else {
        return Err(Err(fatal(format!(
            "端末 {device_id} の adb の設定（シリアル・ボリューム・root）が欠けている"
        ))));
    };
    match ctx.lock_mutex_named(device_mutex(device_id)).await {
        Ok(true) => {}
        Ok(false) => return Err(Ok(Outcome::Requeue)),
        Err(e) => return Err(Err(e.into())),
    }
    if !env.rt.is_connected(&serial) {
        return Err(disconnected(kind));
    }
    // API の破棄などと直列化する。ジョブの終わりまで持つ
    let guard = env.rt.device_lock(device_id).lock_owned().await;
    let fs = match env.rt.fs_for(&serial, &volume, &root) {
        Ok(fs) => fs,
        Err(RemoteError::NotConnected) => return Err(disconnected(kind)),
        Err(e) => return Err(Err(JobError::Failed(anyhow::Error::new(e)))),
    };
    let expect = Expect {
        device_uuid: device.uuid.clone(),
        volume: volume.clone(),
    };
    let rec = match recover(&fs, &expect).await {
        Ok(r) => r,
        Err(RecoverError::Store(StoreError::Remote(RemoteError::NotConnected))) => {
            return Err(disconnected(kind))
        }
        // ユーザが保存先を確かめる。再試行しても変わらない
        Err(
            e @ (RecoverError::NoManifest
            | RecoverError::UuidMismatch
            | RecoverError::VolumeMismatch),
        ) => return Err(Err(fatal(e.to_string()))),
        Err(e) => return Err(Err(JobError::Failed(anyhow::Error::new(e)))),
    };
    measure_free(env, &fs, device_id).await;
    let (items, playlists) = (rec.items.clone(), rec.playlists.clone());
    let saved = env
        .db
        .write(move |c| {
            dbdev::apply_device_state(c, device_id, &items, &playlists, None, false, now_epoch())
        })
        .await;
    if let Err(e) = saved {
        return Err(Err(e.into()));
    }
    Ok(Prepared {
        device,
        fs,
        rec,
        _guard: guard,
    })
}

/// 空きを測って覚える（失敗は警告ログだけ）
async fn measure_free(env: &DeviceEnv, fs: &AdbFs, device_id: i64) {
    match fs.free_bytes().await {
        Ok(b) => env.rt.set_free(device_id, b),
        Err(e) => tracing::warn!(device_id, error = %e, "端末の空き容量を測れない"),
    }
}

// ---- device_scan ----

pub struct ScanHandler {
    env: Arc<DeviceEnv>,
}

impl ScanHandler {
    pub fn new(env: Arc<DeviceEnv>) -> Self {
        Self { env }
    }
}

async fn run_scan(env: Arc<DeviceEnv>, ctx: JobContext) -> HandlerResult {
    let device_id = payload_i64(&ctx, "device_id")?;
    let p = match prepare(&env, &ctx, device_id, Kind::Scan).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    Ok(Outcome::DoneWith(format!(
        "管理外のファイル {} 件",
        p.rec.unmanaged
    )))
}

impl Handler for ScanHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let fut = assert_send(run_scan(Arc::clone(&self.env), ctx));
        Box::pin(fut)
    }
}

// ---- device_sync ----

pub struct SyncHandler {
    env: Arc<DeviceEnv>,
}

impl SyncHandler {
    pub fn new(env: Arc<DeviceEnv>) -> Self {
        Self { env }
    }
}

async fn run_sync(env: Arc<DeviceEnv>, ctx: JobContext) -> HandlerResult {
    let device_id = payload_i64(&ctx, "device_id")?;
    let plan_id = payload_i64(&ctx, "plan_id")?;
    // 端末が消えていれば計画より先にそれを伝える（計画も端末と一緒に消えている）
    match env.db.read(move |c| dbdev::get(c, device_id)).await? {
        Some(_) => {}
        None => return Ok(Outcome::DoneWith("端末が削除された".into())),
    }
    // 安い早期の打ち切り。確定はロックを取った後でもう一度見る
    let open = match env.db.read(move |c| dbdev::open_plan(c, device_id)).await {
        Ok(Some(o)) if o.id == plan_id => o,
        Ok(_) => return Ok(Outcome::DoneWith("計画は既に終わっている".into())),
        Err(DbError::Internal(msg)) => {
            return Err(fatal(format!(
                "計画を読めない。端末タブで破棄してやり直す: {msg}"
            )))
        }
        Err(e) => return Err(e.into()),
    };
    let p = match prepare(&env, &ctx, device_id, Kind::Sync).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    // 端末のロックを待つ間に API が計画を破棄したかもしれない（破棄は generation を進めない）
    let still_open: Option<i64> = env
        .db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT id FROM device_sync_plans WHERE device_id = ?1 AND state = 'open'",
                [device_id],
                |r| r.get(0),
            )
            .optional()?)
        })
        .await?;
    if still_open != Some(plan_id) {
        return Ok(Outcome::DoneWith("計画は既に終わっている".into()));
    }
    let computed = env
        .db
        .read(move |c| dbdev::compute(c, device_id))
        .await?
        .ok_or_else(|| fatal(format!("端末 {device_id} の差分を計算できない")))?;
    let r = runnable(&open.plan, &p.rec.items, &p.rec.playlists, &computed.diff);

    // 送る元: 記録したハッシュが今の desired と一致するものだけ（合わないものは Missing で項目のエラーになる）
    let desired: HashMap<i64, _> = computed.desired.iter().map(|d| (d.track_id, d)).collect();
    let mut wanted: Vec<(i64, SourceKind, String, String, String)> = Vec::new();
    for op in &r.items {
        if !matches!(op.op, OpKind::Add | OpKind::Update | OpKind::UpdateMove) {
            continue;
        }
        if let Some(d) = desired.get(&op.track_id) {
            wanted.push((
                d.track_id,
                d.source.kind,
                d.source.root_rel_path.clone(),
                d.source.semantic.clone(),
                d.sha256.clone(),
            ));
        }
    }
    let lookup: Vec<(i64, SourceKind)> = wanted.iter().map(|w| (w.0, w.1)).collect();
    let hashes = env
        .db
        .read(move |c| {
            lookup
                .into_iter()
                .map(|(id, kind)| dbdev::source_hash(c, id, kind))
                .collect::<crate::db::Result<Vec<_>>>()
        })
        .await?;
    let mut entries = HashMap::new();
    let mut kinds: HashMap<i64, SourceKind> = HashMap::new();
    for ((track_id, kind, rel, semantic, sha256), hash) in wanted.into_iter().zip(hashes) {
        kinds.insert(track_id, kind);
        let Some(hash) = hash else { continue };
        if hash.semantic != semantic || hash.sha256 != sha256 {
            continue;
        }
        let rel_path = RelPath::parse(&rel)
            .map_err(|e| JobError::Failed(anyhow::anyhow!("送る元のパスが不正（{e}）: {rel:?}")))?;
        entries.insert(
            track_id,
            SourceEntry {
                kind,
                rel_path,
                hash,
            },
        );
    }
    let bodies: HashMap<i64, Vec<u8>> = computed
        .playlists
        .iter()
        .map(|pl| (pl.playlist_id, pl.body.clone()))
        .collect();
    let sources = RootSources::new(Arc::clone(&env.library), Arc::clone(&env.derived), entries);
    let control = JobControl {
        ctx: ctx.clone(),
        db: Arc::clone(&env.db),
        device_id,
        generation: open.plan.generation,
    };
    let result = sync::run(
        &p.fs,
        &sources,
        &control,
        SyncInput {
            generation: open.plan.generation,
            start: p.rec.manifest.clone(),
            runnable: &r,
            playlist_bodies: &bodies,
        },
    )
    .await;
    let report = match result {
        Ok(report) => report,
        Err(e) => return sync_failure(Kind::Sync, e),
    };

    let items = report.items();
    let playlists = report.playlists();
    let errors = report.errors.clone();
    let rehash: Vec<(i64, SourceKind)> = report
        .rehash
        .iter()
        .filter_map(|id| kinds.get(id).map(|k| (*id, *k)))
        .collect();
    let open_id = open.id;
    let closed = env
        .db
        .write(move |c| {
            let now = now_epoch();
            let tx = c.transaction()?;
            dbdev::apply_device_state(
                &tx,
                device_id,
                &items,
                &playlists,
                Some(&errors),
                true,
                now,
            )?;
            for (id, kind) in rehash {
                dbdev::forget_source_hash(&tx, id, kind)?;
            }
            if !dbdev::close_plan(&tx, open_id, PlanEnd::Completed, None, now)? {
                // 同期の途中で閉じられた。キャッシュも last_synced_at も進めない（tx を落として巻き戻す）
                return Ok(false);
            }
            tx.commit()?;
            Ok(true)
        })
        .await?;
    if !closed {
        return Err(fatal("計画が同期の途中で閉じられた"));
    }
    measure_free(&env, &p.fs, device_id).await;
    let sent = (r.items.len() + r.playlists.len()).saturating_sub(report.errors.len());
    let warnings = if report.warnings.is_empty() {
        String::new()
    } else {
        format!("・{}", report.warnings.join("; "))
    };
    tracing::info!(
        device = %p.device.name,
        sent,
        held = report.errors.len(),
        "端末へ同期した"
    );
    Ok(Outcome::DoneWith(format!(
        "送った {} 件・保留 {} 件{}",
        sent,
        report.errors.len(),
        warnings
    )))
}

impl Handler for SyncHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let fut = assert_send(run_sync(Arc::clone(&self.env), ctx));
        Box::pin(fut)
    }
}

// ---- device_verify ----

pub struct VerifyHandler {
    env: Arc<DeviceEnv>,
}

impl VerifyHandler {
    pub fn new(env: Arc<DeviceEnv>) -> Self {
        Self { env }
    }
}

async fn run_verify(env: Arc<DeviceEnv>, ctx: JobContext) -> HandlerResult {
    let device_id = payload_i64(&ctx, "device_id")?;
    let p = match prepare(&env, &ctx, device_id, Kind::Verify).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let control = JobControl {
        ctx: ctx.clone(),
        db: Arc::clone(&env.db),
        device_id,
        // 検証は generation を見ないので、端末の今の値を入れる
        generation: p.device.generation,
    };
    let report = match verify(&p.fs, &control, p.rec.manifest.clone()).await {
        Ok(r) => r,
        Err(e) => return sync_failure(Kind::Verify, e),
    };
    let items = report.items();
    let playlists = report.playlists();
    env.db
        .write(move |c| {
            dbdev::apply_device_state(c, device_id, &items, &playlists, None, false, now_epoch())
        })
        .await?;
    Ok(Outcome::DoneWith(format!(
        "検証した {} 曲・不一致 {}・欠落 {}",
        p.rec.manifest.items.len(),
        report.mismatched.len(),
        report.missing.len()
    )))
}

impl Handler for VerifyHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let fut = assert_send(run_verify(Arc::clone(&self.env), ctx));
        Box::pin(fut)
    }
}

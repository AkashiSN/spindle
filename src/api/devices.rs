//! 端末 API（UI 向け。仕様「API の変更一覧」、D-95）

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;
use crate::db::devices::{
    self as dbdev, Confirm, Device, DevicePatch, NewDevice, PlanEnd, PlaylistCheck, Selection,
    Snapshot, Update,
};
use crate::db::jobs as dbjobs;
use crate::db::{now_epoch, DbError};
use crate::device::adb::probe_volumes_under;
use crate::device::ondevice::is_reserved;
use crate::device::quote::{root_abs_under, valid_serial, valid_volume, DEFAULT_ROOT};
use crate::device::recover::{recover, Expect, RecoverError};
use crate::device::remote::{DeviceFs as _, DirState, RemoteError};
use crate::device::runtime::AdbRuntime;
use crate::device::store::{self, InitError, StoreError};
use crate::domain::derived::Variant;
use crate::domain::device::PendingSets;
use crate::domain::device::{Counts, Hold, TrackState, Transport};
use crate::domain::filter::Filter;
use crate::jobs::handlers::device::{scan_job, sync_job, verify_job};
use crate::playlist::dsl::Rule;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::OptionalExtension as _;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// フィルタが端末の状態を引くときだけ、スナップショットから未反映の集合を埋める
pub async fn attach_pending(state: &AppState, f: &mut Filter) -> Result<(), ApiError> {
    if f.uses_devices() {
        f.pending = state.db.device_snapshot().await?.pending_sets();
    }
    Ok(())
}

/// 一覧の行に端末ごとの状態を付ける（状態の無い端末 = 対象外は付けない）
pub fn annotate(snap: &crate::db::devices::Snapshot, rows: &mut [crate::db::tracks::TrackRow]) {
    if snap.devices.is_empty() {
        return;
    }
    for r in rows {
        r.devices = snap
            .states_of(r.id)
            .into_iter()
            .map(|(device_id, state)| crate::db::tracks::TrackDevice { device_id, state })
            .collect();
    }
}

/// ルールの評価に渡す集合（端末のフィールドを使わないルールなら空）
pub async fn pending_for_rule(state: &AppState, rule: &Rule) -> Result<PendingSets, ApiError> {
    if rule.references_device_fields() {
        Ok(state.db.device_snapshot().await?.pending_sets())
    } else {
        Ok(PendingSets::default())
    }
}

#[derive(Serialize)]
pub struct DeviceView {
    pub id: i64,
    pub name: String,
    pub transport: &'static str,
    pub variant: &'static str,
    pub selection: &'static str,
    pub generation: i64,
    /// 接続状態（adb: 監視が見た状態が `device` か。ADB 同期が無効なら false。agent は常に null）
    pub connected: Option<bool>,
    /// adb の生の状態（`device` / `unauthorized` / `offline` …）。見えていない・agent なら null
    pub adb_state: Option<String>,
    /// adb の保存先のボリュームと root（agent は null）
    pub adb_volume: Option<String>,
    pub adb_root: Option<String>,
    pub counts: Counts,
    pub last_synced_at: Option<i64>,
    pub playlist_ids: Vec<i64>,
    /// open な計画か同期ジョブがある（設定を変えられない）
    pub open_plan: bool,
    /// open な計画がある（同期の途中。続き・破棄の対象）
    pub plan_open: bool,
    /// queued / running の `device_sync`（無ければ null）
    pub sync_job: Option<SyncJobView>,
}

#[derive(Serialize)]
pub struct SyncJobView {
    pub id: i64,
    pub state: String,
}

/// 端末 1 台の view の材料のうち DB から読むもの
struct Extras {
    playlist_ids: Vec<i64>,
    open_plan: bool,
    plan_open: bool,
    sync_job: Option<SyncJobView>,
}

fn read_extras(c: &rusqlite::Connection, id: i64) -> crate::db::Result<Extras> {
    Ok(Extras {
        playlist_ids: dbdev::playlist_ids(c, id)?,
        open_plan: dbdev::has_open_work(c, id)?,
        plan_open: dbdev::open_plan_id(c, id)?.is_some(),
        sync_job: dbdev::active_device_job(c, id, "device_sync")?
            .map(|(id, state)| SyncJobView { id, state }),
    })
}

#[derive(Serialize)]
pub struct DeviceList {
    pub items: Vec<DeviceView>,
}

fn view(d: &Device, snap: &Snapshot, extras: Extras, rt: Option<&AdbRuntime>) -> DeviceView {
    let (connected, adb_state) = match (d.transport, d.adb_serial.as_deref()) {
        (Transport::Adb, Some(serial)) => (
            Some(rt.is_some_and(|rt| rt.is_connected(serial))),
            rt.and_then(|rt| rt.state_of(serial)),
        ),
        (Transport::Adb, None) => (Some(false), None),
        (Transport::Agent, _) => (None, None),
    };
    DeviceView {
        id: d.id,
        name: d.name.clone(),
        transport: d.transport.as_str(),
        variant: d.variant.as_str(),
        selection: d.selection.as_str(),
        generation: d.generation,
        connected,
        adb_state,
        adb_volume: d.adb_volume.clone(),
        adb_root: d.adb_root.clone(),
        counts: snap.get(d.id).map(|s| s.counts).unwrap_or_default(),
        last_synced_at: d.last_synced_at,
        playlist_ids: extras.playlist_ids,
        open_plan: extras.open_plan,
        plan_open: extras.plan_open,
        sync_job: extras.sync_job,
    }
}

async fn view_of(state: &AppState, id: i64) -> Result<Option<DeviceView>, ApiError> {
    let snap = state.db.device_snapshot().await?;
    let found = state
        .db
        .read(move |c| {
            let Some(d) = dbdev::get(c, id)? else {
                return Ok(None);
            };
            let extras = read_extras(c, id)?;
            Ok(Some((d, extras)))
        })
        .await?;
    Ok(found.map(|(d, extras)| view(&d, &snap, extras, state.adb.as_deref())))
}

fn bad_request(msg: impl Into<String>) -> Response {
    error_response_with_message(StatusCode::BAD_REQUEST, "bad_request", msg)
}
fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not_found")
}
fn open_plan() -> Response {
    error_response_with_message(
        StatusCode::CONFLICT,
        "open_plan",
        "同期が途中か実行中なので変更できない（完了させるか、計画を破棄してから）",
    )
}
/// 同じ名前（大小文字・正規化の違いを含む）の端末があるか
fn name_taken(c: &rusqlite::Connection, name: &str) -> crate::db::Result<bool> {
    let key = crate::domain::relpath::canonical_key(name);
    let taken: Option<i64> = c
        .query_row("SELECT id FROM devices WHERE name_key = ?1", [key], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(taken.is_some())
}

fn validate_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("name が空".to_owned());
    }
    if name.chars().count() > 100 {
        return Err("name が長すぎる（100 文字まで）".to_owned());
    }
    Ok(name.to_owned())
}

pub async fn list(State(state): State<AppState>) -> Result<Response, ApiError> {
    // 件数の表示とハッシュの投入の材料（投入は dedup と `hashes_to_enqueue` で絞るので少し古くてよい）
    let snap = state.db.device_snapshot_for_display().await?;
    let rows = state
        .db
        .read(|c| {
            let mut out = Vec::new();
            for d in dbdev::list(c)? {
                let extras = read_extras(c, d.id)?;
                out.push((d, extras));
            }
            Ok(out)
        })
        .await?;
    let items = rows
        .into_iter()
        .map(|(d, extras)| view(&d, &snap, extras, state.adb.as_deref()))
        .collect();
    enqueue_hashes(&state, &snap).await?;
    Ok(Json(DeviceList { items }).into_response())
}

#[derive(Deserialize)]
pub struct CreateBody {
    pub name: String,
    pub transport: String,
    pub variant: String,
    pub selection: String,
    /// adb のみ: シリアルとボリューム（`emulated` か SD の UUID）
    #[serde(default)]
    pub serial: Option<String>,
    #[serde(default)]
    pub volume: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateBody>,
) -> Result<Response, ApiError> {
    let name = match validate_name(&body.name) {
        Ok(n) => n,
        Err(m) => return Ok(bad_request(m)),
    };
    let (Some(transport), Some(variant), Some(selection)) = (
        Transport::parse(&body.transport),
        Variant::parse(&body.variant),
        Selection::parse(&body.selection),
    ) else {
        return Ok(bad_request("transport / variant / selection が不正"));
    };
    if transport == Transport::Adb {
        return create_adb(&state, name, variant, selection, body.serial, body.volume).await;
    }
    let created = state
        .db
        .write(move |c| {
            if name_taken(c, &name)? {
                return Ok(None);
            }
            dbdev::create(
                c,
                &NewDevice {
                    name: &name,
                    transport,
                    variant,
                    selection,
                    adb: None,
                },
                now_epoch(),
            )
            .map(Some)
        })
        .await?;
    let Some(d) = created else {
        return Ok(error_response(StatusCode::CONFLICT, "duplicate"));
    };
    Ok(match view_of(&state, d.id).await? {
        Some(v) => (StatusCode::CREATED, Json(v)).into_response(),
        None => not_found(),
    })
}

#[derive(Deserialize)]
pub struct PatchBody {
    pub name: Option<String>,
    pub selection: Option<String>,
    pub variant: Option<String>,
}

enum Guarded<T> {
    Done(T),
    NotFound,
    OpenPlan,
}

pub async fn patch(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<PatchBody>,
) -> Result<Response, ApiError> {
    let name = match body.name.as_deref().map(validate_name) {
        Some(Ok(n)) => Some(n),
        Some(Err(m)) => return Ok(bad_request(m)),
        None => None,
    };
    let selection = match body.selection.as_deref().map(Selection::parse) {
        Some(None) => return Ok(bad_request("selection が不正")),
        Some(s) => s,
        None => None,
    };
    let variant = match body.variant.as_deref().map(Variant::parse) {
        Some(None) => return Ok(bad_request("variant が不正")),
        Some(v) => v,
        None => None,
    };
    let p = DevicePatch {
        name,
        selection,
        variant,
    };
    let res = state
        .db
        .write(move |c| {
            if !p.only_name() && dbdev::has_open_work(c, id)? {
                return Ok(Guarded::OpenPlan);
            }
            Ok(match dbdev::update(c, id, &p, now_epoch())? {
                Update::NotFound => Guarded::NotFound,
                u => Guarded::Done(u),
            })
        })
        .await?;
    match res {
        Guarded::NotFound => Ok(not_found()),
        Guarded::OpenPlan => Ok(open_plan()),
        Guarded::Done(Update::Duplicate) => Ok(error_response(StatusCode::CONFLICT, "duplicate")),
        Guarded::Done(_) => Ok(match view_of(&state, id).await? {
            Some(v) => Json(v).into_response(),
            None => not_found(),
        }),
    }
}

enum Deleted {
    /// 消した。取り消す待ちのジョブ
    Done(Vec<i64>),
    NotFound,
    Busy,
}

/// 端末を削除する（D-98）。途中の計画があっても消せる（計画は行と一緒に ON DELETE CASCADE で消える）。
/// 端末に戻れない（壊れた・手放した）ときの逃げ道。端末のジョブ（同期・走査・検証）が実行中か、
/// 端末のロックを別の処理（ジョブ・破棄）が持っていれば 409 `busy`。それらは端末上のジャーナルを
/// 回復して行を書き戻すので、消した後に端末へ触り続けさせない。待ちのジョブは断らずに取り消す
/// （未接続の端末を待つジョブを残さない）。端末上のファイルには触らない
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    // 行を消して待ちのジョブを取り消すまで持つ（ADB 同期が無効なら端末のジョブは動かない）
    let _guard = match state.adb.as_ref() {
        Some(rt) => match rt.device_lock(id).try_lock_owned() {
            Ok(g) => Some(g),
            Err(_) => return Ok(busy()),
        },
        None => None,
    };
    let res = state
        .db
        .write(move |c| {
            let tx = c.transaction()?;
            if dbdev::get(&tx, id)?.is_none() {
                return Ok(Deleted::NotFound);
            }
            for t in DEVICE_JOB_TYPES {
                if dbdev::has_running_device_job(&tx, id, t)? {
                    return Ok(Deleted::Busy);
                }
            }
            let queued = dbdev::queued_device_jobs(&tx, id, DEVICE_JOB_TYPES)?;
            dbdev::delete(&tx, id)?;
            tx.commit()?;
            Ok(Deleted::Done(queued))
        })
        .await?;
    match res {
        Deleted::Done(queued) => {
            // 行を消した後に取り消す。間に走り出したものは端末が無いのを見て何もせずに終わる
            for job_id in queued {
                state.jobs.cancel(job_id).await?;
            }
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        Deleted::NotFound => Ok(not_found()),
        Deleted::Busy => Ok(busy()),
    }
}

/// 端末に紐づくジョブの種類（payload の `device_id` で端末を指す）
const DEVICE_JOB_TYPES: &[&str] = &["device_sync", "device_verify", "device_scan"];

#[derive(Deserialize)]
pub struct PlaylistsBody {
    pub playlist_ids: Vec<i64>,
}

enum PutFail {
    Unknown(i64),
    Cycle(i64),
}

pub async fn put_playlists(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<PlaylistsBody>,
) -> Result<Response, ApiError> {
    let ids = body.playlist_ids;
    let res = state
        .db
        .write(move |c| {
            if dbdev::get(c, id)?.is_none() {
                return Ok(Guarded::NotFound);
            }
            if dbdev::has_open_work(c, id)? {
                return Ok(Guarded::OpenPlan);
            }
            match dbdev::check_playlists(c, &ids)? {
                PlaylistCheck::Unknown(p) => return Ok(Guarded::Done(Err(PutFail::Unknown(p)))),
                PlaylistCheck::Cycle(p) => return Ok(Guarded::Done(Err(PutFail::Cycle(p)))),
                PlaylistCheck::Ok => {}
            }
            dbdev::set_playlists(c, id, &ids, now_epoch())?;
            Ok(Guarded::Done(Ok(())))
        })
        .await?;
    match res {
        Guarded::NotFound => Ok(not_found()),
        Guarded::OpenPlan => Ok(open_plan()),
        Guarded::Done(Err(PutFail::Unknown(p))) => {
            Ok(bad_request(format!("プレイリスト {p} が無い")))
        }
        Guarded::Done(Err(PutFail::Cycle(p))) => Ok(cycle(p)),
        Guarded::Done(Ok(())) => Ok(match view_of(&state, id).await? {
            Some(v) => Json(v).into_response(),
            None => not_found(),
        }),
    }
}

/// 端末のフィールドを使うスマートプレイリストは端末に載せられない（③ 循環の禁止）
pub fn cycle(playlist_id: i64) -> Response {
    error_response_with_message(
        StatusCode::BAD_REQUEST,
        "cycle",
        format!("プレイリスト {playlist_id} のルールが端末の状態（on_device / device_pending）を使っているので、端末の選曲に載せられない"),
    )
}

/// 差分の計算で見つかった「ハッシュの無い送る元」に source_hash を投入する（未完了のもの・走査待ちで
/// 止めているものは `dbdev::hashes_to_enqueue` が除く）
pub async fn enqueue_hashes(state: &AppState, snap: &Snapshot) -> Result<(), ApiError> {
    let mut needs: Vec<(i64, crate::domain::device::SourceKind)> = snap
        .devices
        .iter()
        .flat_map(|d| d.computed.needs_hash.iter().copied())
        .collect();
    needs.sort();
    needs.dedup();
    // 先に読みプールで絞り込み、投入するものが無ければ書き手に触らない（書き込みの通番を進めると
    // 端末のスナップショットのキャッシュが無効になる）
    let needs = state
        .db
        .read(move |c| dbdev::hashes_to_enqueue(c, &needs))
        .await?;
    if needs.is_empty() {
        return Ok(());
    }
    let ids = state
        .db
        .write(move |c| dbdev::enqueue_source_hashes(c, &needs, now_epoch()))
        .await?;
    if !ids.is_empty() {
        state.jobs.notify_enqueued(&ids).await;
    }
    Ok(())
}

/// スマートプレイリストの再評価を待つ端末か。選曲がプレイリストで、スマートプレイリストが 1 つでも
/// 載っている端末だけ（全曲・手動だけの端末は評価で中身が変わらない）。Derived の変換が終わるたびに
/// 再評価の印が立つので、全端末で待たせると変換中はずっと同期できない（実機での確認、D-98）
fn waits_for_reevaluation(selection: Selection, smart: &[(i64, String, Option<i64>)]) -> bool {
    selection == Selection::Playlists && !smart.is_empty()
}

#[derive(Serialize)]
pub struct DiffView {
    pub generation: i64,
    pub plan_token: String,
    pub pending_reevaluation: bool,
    pub items: Vec<DiffItem>,
    pub playlists: Vec<DiffPlaylist>,
    pub estimate: EstimateView,
    pub evaluations: Vec<Evaluation>,
    pub counts: Counts,
}
#[derive(Serialize)]
pub struct DiffItem {
    pub op: &'static str, // add | update | move | update_move | delete | waiting | error
    pub track_id: i64,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub from: Option<String>,
    pub dest_path: Option<String>,
    pub reason: Option<String>,
    pub size: u64,
    pub has_copy: bool,
}
#[derive(Serialize)]
pub struct DiffPlaylist {
    pub op: &'static str,
    pub playlist_id: i64,
    pub name: Option<String>,
    pub dest_path: Option<String>,
    pub reason: Option<String>,
}
#[derive(Serialize)]
pub struct EstimateView {
    pub transfer_bytes: u64,
    pub peak_bytes: u64,
    pub free: Option<u64>,
}
#[derive(Serialize)]
pub struct Evaluation {
    pub playlist_id: i64,
    pub name: String,
    pub evaluated_at: Option<i64>,
    pub pending: bool,
}

pub async fn diff(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let snap = state.db.device_snapshot().await?;
    let Some(d) = snap.get(id) else {
        return Ok(not_found());
    };
    let diff = &d.computed.diff;
    let mut track_ids: Vec<i64> = diff.items.iter().map(|o| o.track_id).collect();
    track_ids.extend(diff.held.iter().map(|h| h.track_id));
    // 反映済み（差分に操作も保留も無い）の曲に端末から失敗の報告があるもの。track_states はこれを
    // Error にして counts.error に数えるので、差分にも理由付きで載せる（件数と一覧を食い違わせない）
    let in_diff: std::collections::HashSet<i64> = track_ids.iter().copied().collect();
    let synced_errors: Vec<(&crate::domain::device::DeviceItem, &str)> = d
        .current
        .iter()
        .filter(|c| !in_diff.contains(&c.track_id))
        .filter_map(|c| match d.states.get(&c.track_id) {
            Some(TrackState::Error { reason, .. }) => Some((c, reason.as_str())),
            _ => None,
        })
        .collect();
    track_ids.extend(synced_errors.iter().map(|(c, _)| c.track_id));
    let mut pl_ids: Vec<i64> = diff.playlists.iter().map(|p| p.playlist_id).collect();
    pl_ids.extend(diff.playlist_errors.iter().map(|(p, _)| *p));
    let (titles, names, evals) = state
        .db
        .read(move |c| {
            Ok((
                dbdev::titles(c, &track_ids)?,
                dbdev::playlist_names(c, &pl_ids)?,
                dbdev::smart_evaluations(c, id)?,
            ))
        })
        .await?;
    // 反映済みの行を曲ごとに 1 回で引けるようにする（項目ごとに全行を走査しない）
    let current_of: std::collections::HashMap<i64, &crate::domain::device::DeviceItem> =
        d.current.iter().map(|c| (c.track_id, c)).collect();
    let reason_of = |track_id: i64| match d.states.get(&track_id) {
        Some(TrackState::Pending { reason, .. }) => reason.clone(),
        _ => None,
    };
    let mut items: Vec<DiffItem> = diff
        .items
        .iter()
        .map(|o| DiffItem {
            op: o.kind.as_str(),
            track_id: o.track_id,
            title: titles.get(&o.track_id).map(|t| t.0.clone()),
            artist: titles.get(&o.track_id).map(|t| t.1.clone()),
            from: o.from.clone(),
            dest_path: o.to.clone().or_else(|| o.from.clone()),
            reason: reason_of(o.track_id),
            size: o.size,
            has_copy: o.from.is_some() || current_of.contains_key(&o.track_id),
        })
        .collect();
    for h in &diff.held {
        let (op, reason) = match h.hold {
            Hold::Wait(w) => ("waiting", w.reason().to_owned()),
            Hold::Error(e) => ("error", e.reason().to_owned()),
        };
        items.push(DiffItem {
            op,
            track_id: h.track_id,
            title: titles.get(&h.track_id).map(|t| t.0.clone()),
            artist: titles.get(&h.track_id).map(|t| t.1.clone()),
            from: None,
            dest_path: current_of.get(&h.track_id).map(|c| c.dest_path.clone()),
            reason: Some(reason),
            size: 0,
            has_copy: h.has_copy,
        });
    }
    for (c, reason) in &synced_errors {
        items.push(DiffItem {
            op: "error",
            track_id: c.track_id,
            title: titles.get(&c.track_id).map(|t| t.0.clone()),
            artist: titles.get(&c.track_id).map(|t| t.1.clone()),
            from: None,
            dest_path: Some(c.dest_path.clone()),
            reason: Some((*reason).to_owned()),
            size: 0,
            has_copy: true,
        });
    }
    let mut playlists: Vec<DiffPlaylist> = diff
        .playlists
        .iter()
        .map(|p| DiffPlaylist {
            op: p.kind.as_str(),
            playlist_id: p.playlist_id,
            name: names.get(&p.playlist_id).cloned(),
            dest_path: p.to.clone().or_else(|| p.from.clone()),
            reason: d
                .playlist_errors_reported
                .iter()
                .find(|(i, _)| *i == p.playlist_id)
                .map(|(_, r)| r.clone()),
        })
        .collect();
    for (pid, reason) in &diff.playlist_errors {
        playlists.push(DiffPlaylist {
            op: "error",
            playlist_id: *pid,
            name: names.get(pid).cloned(),
            dest_path: None,
            reason: Some((*reason).to_owned()),
        });
    }
    let pending = state.reeval_pending() && waits_for_reevaluation(d.device.selection, &evals);
    let e = crate::domain::device::estimate(diff, &d.current);
    let view = DiffView {
        generation: d.computed.generation,
        plan_token: d.computed.plan_token.clone(),
        pending_reevaluation: pending,
        items,
        playlists,
        estimate: EstimateView {
            transfer_bytes: e.transfer_bytes,
            peak_bytes: e.peak_bytes,
            free: state.adb.as_ref().and_then(|rt| rt.free(id)),
        },
        evaluations: evals
            .into_iter()
            .map(|(playlist_id, name, evaluated_at)| Evaluation {
                playlist_id,
                name,
                evaluated_at,
                pending,
            })
            .collect(),
        counts: d.counts,
    };
    enqueue_hashes(&state, &snap).await?;
    Ok(Json(view).into_response())
}

#[derive(Deserialize)]
pub struct EstimateParams {
    pub selection: String,
    #[serde(default)]
    pub playlist_ids: String,
}

#[derive(Serialize)]
pub struct EstimateSelectionView {
    pub tracks: usize,
    pub bytes: u64,
    /// 容量が未確定の曲の数（送る元の準備待ち: Derived が無い・古い、送る元のハッシュが無い・古い）。bytes に含まれない
    pub unhashed: usize,
}

pub async fn estimate(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    axum::extract::Query(p): axum::extract::Query<EstimateParams>,
) -> Result<Response, ApiError> {
    let Some(selection) = Selection::parse(&p.selection) else {
        return Ok(bad_request("selection が不正"));
    };
    let mut ids = Vec::new();
    for s in p
        .playlist_ids
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match s.parse::<i64>() {
            Ok(n) => ids.push(n),
            Err(_) => return Ok(bad_request("playlist_ids が不正")),
        }
    }
    let got = state
        .db
        .read(move |c| {
            let Some(d) = dbdev::get(c, id)? else {
                return Ok(None);
            };
            dbdev::estimate_selection(c, &d, selection, &ids).map(Some)
        })
        .await?;
    Ok(match got {
        Some(e) => Json(EstimateSelectionView {
            tracks: e.tracks,
            bytes: e.bytes,
            unhashed: e.unhashed,
        })
        .into_response(),
        None => not_found(),
    })
}

// ---- Android（adb）: 未登録の一覧・登録・同期・再開・破棄・検証（P5-3b） ----

fn adb_disabled() -> Response {
    error_response_with_message(
        StatusCode::SERVICE_UNAVAILABLE,
        "adb_disabled",
        "Android の同期が無効（設定の [devices].adb_server が空）",
    )
}
fn conflict(code: &'static str, msg: impl Into<String>) -> Response {
    error_response_with_message(StatusCode::CONFLICT, code, msg)
}
fn device_failed(msg: impl Into<String>) -> Response {
    error_response_with_message(StatusCode::BAD_GATEWAY, "device_failed", msg)
}
fn busy() -> Response {
    conflict(
        "busy",
        "同期が実行中か、端末を別の処理が使っている。終わってからやり直してください",
    )
}
fn no_open_plan() -> Response {
    error_response_with_message(StatusCode::NOT_FOUND, "no_open_plan", "途中の計画が無い")
}

/// 端末が `device` でないときの 409。`unauthorized` なら許可を促す
fn not_connected(rt: &AdbRuntime, serial: &str) -> Response {
    if rt.state_of(serial).as_deref() == Some("unauthorized") {
        conflict("not_connected", "端末で USB デバッグを許可してください")
    } else {
        conflict("not_connected", "端末が接続されていない")
    }
}

/// 端末の操作の失敗を応答にする（未接続は 409、それ以外は 502）
fn remote_failed(rt: &AdbRuntime, serial: &str, e: RemoteError) -> Response {
    match e {
        RemoteError::NotConnected => not_connected(rt, serial),
        e => device_failed(e.to_string()),
    }
}

fn store_failed(rt: &AdbRuntime, serial: &str, e: StoreError) -> Response {
    match e {
        StoreError::Remote(e) => remote_failed(rt, serial, e),
        e => device_failed(e.to_string()),
    }
}

/// adb の端末と実行時の状態を引く。無ければ 404、agent なら 400、ADB 同期が無効なら 503
async fn adb_device(
    state: &AppState,
    id: i64,
) -> Result<Result<(Device, Arc<AdbRuntime>), Response>, ApiError> {
    let Some(d) = state.db.read(move |c| dbdev::get(c, id)).await? else {
        return Ok(Err(not_found()));
    };
    if d.transport != Transport::Adb {
        return Ok(Err(bad_request("Android（adb）の端末ではない")));
    }
    let Some(rt) = state.adb.clone() else {
        return Ok(Err(adb_disabled()));
    };
    Ok(Ok((d, rt)))
}

#[derive(Serialize)]
pub struct VolumeView {
    pub volume: String,
    pub path: String,
    pub free: u64,
    /// `missing` / `empty` / `nonempty`
    pub state: &'static str,
}

#[derive(Serialize)]
pub struct UnregisteredItem {
    pub serial: String,
    pub model: Option<String>,
    pub state: String,
    pub volumes: Vec<VolumeView>,
    /// ボリュームを調べられなかった理由
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct UnregisteredList {
    pub items: Vec<UnregisteredItem>,
}

fn dir_state_str(s: DirState) -> &'static str {
    match s {
        DirState::Missing => "missing",
        DirState::Empty => "empty",
        DirState::NonEmpty => "nonempty",
    }
}

/// 接続中で未登録の端末と、`device` のものは保存先の候補のボリューム（仕様 ⑤「登録」1〜2）
pub async fn unregistered(State(state): State<AppState>) -> Result<Response, ApiError> {
    let Some(rt) = state.adb.clone() else {
        return Ok(adb_disabled());
    };
    let tracked = rt.devices();
    let serials: Vec<String> = tracked.iter().map(|d| d.serial.clone()).collect();
    let registered: std::collections::HashSet<String> = state
        .db
        .read(move |c| {
            let mut out = std::collections::HashSet::new();
            for s in serials {
                if dbdev::find_by_serial(c, &s)?.is_some() {
                    out.insert(s);
                }
            }
            Ok(out)
        })
        .await?;
    let mut items = Vec::new();
    for d in tracked {
        if registered.contains(&d.serial) {
            continue;
        }
        let (mut volumes, mut error) = (Vec::new(), None);
        if d.state == "device" {
            match probe_volumes_under(
                rt.cfg(),
                &d.serial,
                rt.storage_base(),
                DEFAULT_ROOT,
                rt.shutdown(),
            )
            .await
            {
                Ok(v) => {
                    volumes = v
                        .into_iter()
                        .map(|v| VolumeView {
                            volume: v.volume,
                            path: v.path,
                            free: v.free,
                            state: dir_state_str(v.state),
                        })
                        .collect();
                }
                Err(e) => {
                    tracing::warn!(serial = %d.serial, error = %e, "端末のボリュームを調べられない");
                    error = Some(e.to_string());
                }
            }
        }
        items.push(UnregisteredItem {
            serial: d.serial,
            model: d.model,
            state: d.state,
            volumes,
            error,
        });
    }
    Ok(Json(UnregisteredList { items }).into_response())
}

/// 保存先が空でないときの 409
fn not_empty(rt: &AdbRuntime, volume: &str) -> Response {
    let path = root_abs_under(rt.storage_base(), volume, DEFAULT_ROOT)
        .unwrap_or_else(|| DEFAULT_ROOT.to_owned());
    conflict(
        "not_empty",
        format!("保存先 {path} が空でない。中身を移すか、別のボリュームを選ぶ"),
    )
}

/// adb の端末の登録（仕様 ⑤「登録」）: 接続を確かめ、保存先が空か前の登録の失敗の残りだけなら
/// 行を作って端末に manifest を作り、差分の計算（`device_scan`）を投入する。端末の操作に失敗したら
/// 行を残さない
async fn create_adb(
    state: &AppState,
    name: String,
    variant: Variant,
    selection: Selection,
    serial: Option<String>,
    volume: Option<String>,
) -> Result<Response, ApiError> {
    let (Some(serial), Some(volume)) = (serial, volume) else {
        return Ok(bad_request("Android の登録には serial と volume が要る"));
    };
    if !valid_serial(&serial) {
        return Ok(bad_request("serial が不正"));
    }
    if !valid_volume(&volume) {
        return Ok(bad_request("volume が不正"));
    }
    let Some(rt) = state.adb.clone() else {
        return Ok(adb_disabled());
    };
    if !rt.is_connected(&serial) {
        return Ok(not_connected(&rt, &serial));
    }
    let _registering = rt.register_lock().lock().await;

    let (n, s) = (name.clone(), serial.clone());
    let taken = state
        .db
        .read(move |c| {
            if name_taken(c, &n)? {
                return Ok(Some("duplicate"));
            }
            if dbdev::find_by_serial(c, &s)?.is_some() {
                return Ok(Some("serial_registered"));
            }
            Ok(None)
        })
        .await?;
    match taken {
        Some("duplicate") => return Ok(error_response(StatusCode::CONFLICT, "duplicate")),
        Some(_) => return Ok(conflict("serial_registered", "この端末は登録済み")),
        None => {}
    }

    let fs = match rt.fs_for(&serial, &volume, DEFAULT_ROOT) {
        Ok(fs) => fs,
        Err(e) => return Ok(remote_failed(&rt, &serial, e)),
    };
    match fs.root_state().await {
        Ok(DirState::NonEmpty) => {
            // 中身が `.spindle` の下だけで、その manifest が無い・読めない・生きた登録のものでない
            // （= 前の登録の失敗の残り）なら片付けて続ける。それ以外は推測しないで断る
            let files = match fs.list_files().await {
                Ok(f) => f,
                Err(e) => return Ok(remote_failed(&rt, &serial, e)),
            };
            if !files.iter().all(|f| is_reserved(&f.path)) {
                return Ok(not_empty(&rt, &volume));
            }
            let leftover = match store::read_manifest(&fs).await {
                Ok(None) => true,
                Ok(Some(m)) => {
                    let uuid = m.device_uuid;
                    !state.db.read(move |c| dbdev::uuid_exists(c, &uuid)).await?
                }
                Err(StoreError::Remote(e)) => return Ok(remote_failed(&rt, &serial, e)),
                Err(_) => true,
            };
            if !leftover {
                return Ok(not_empty(&rt, &volume));
            }
            tracing::info!(%serial, %volume, "前の登録の失敗の残り（.spindle）を片付ける");
            if let Err(e) = fs.discard_init().await {
                return Ok(remote_failed(&rt, &serial, e));
            }
        }
        Ok(DirState::Missing | DirState::Empty) => {}
        Err(e) => return Ok(remote_failed(&rt, &serial, e)),
    }

    let (s, v) = (serial.clone(), volume.clone());
    let created = state
        .db
        .write(move |c| {
            if name_taken(c, &name)? {
                return Ok(Err("duplicate"));
            }
            if dbdev::find_by_serial(c, &s)?.is_some() {
                return Ok(Err("serial_registered"));
            }
            dbdev::create(
                c,
                &NewDevice {
                    name: &name,
                    transport: Transport::Adb,
                    variant,
                    selection,
                    adb: Some((&s, &v, DEFAULT_ROOT)),
                },
                now_epoch(),
            )
            .map(Ok)
        })
        .await?;
    let d = match created {
        Ok(d) => d,
        Err("duplicate") => return Ok(error_response(StatusCode::CONFLICT, "duplicate")),
        Err(_) => return Ok(conflict("serial_registered", "この端末は登録済み")),
    };

    if let Err(e) = store::initialize(&fs, &d.uuid, &volume).await {
        tracing::warn!(%serial, %volume, error = %e, "端末の保存先を初期化できない");
        // 空でなかった（確かめた後に何かが置かれた）なら何も書いていないので片付けない
        if !matches!(e, InitError::NotEmpty) {
            if let Err(e) = fs.discard_init().await {
                tracing::warn!(%serial, error = %e, "初期化の途中の .spindle を片付けられない");
            }
        }
        let id = d.id;
        state.db.write(move |c| dbdev::delete(c, id)).await?;
        return Ok(match e {
            InitError::NotEmpty => not_empty(&rt, &volume),
            InitError::Store(e) => store_failed(&rt, &serial, e),
        });
    }
    state.jobs.enqueue(scan_job(d.id)).await?;
    Ok(match view_of(state, d.id).await? {
        Some(v) => (StatusCode::CREATED, Json(v)).into_response(),
        None => not_found(),
    })
}

#[derive(Deserialize)]
pub struct SyncBody {
    pub plan_token: String,
}

#[derive(Serialize)]
pub struct JobAccepted {
    pub job_id: i64,
}

fn accepted(job_id: i64) -> Response {
    (StatusCode::ACCEPTED, Json(JobAccepted { job_id })).into_response()
}

fn plan_unreadable() -> Response {
    conflict(
        "plan_unreadable",
        "計画を読めない。破棄してから差分を取り直してください",
    )
}

enum SyncResult {
    Job(i64),
    OpenPlanExists,
    Mismatch(String),
    Unreadable,
    NotFound,
}

/// 計画を確定して同期を投入する（仕様 ③「計画トークン」）。確定と投入は 1 つのトランザクション
pub async fn sync(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<SyncBody>,
) -> Result<Response, ApiError> {
    let d = match adb_device(&state, id).await? {
        Ok((d, _)) => d,
        Err(r) => return Ok(r),
    };
    if state.reeval_pending() && {
        let evals = state
            .db
            .read(move |c| dbdev::smart_evaluations(c, id))
            .await?;
        waits_for_reevaluation(d.selection, &evals)
    } {
        return Ok(conflict(
            "pending_reevaluation",
            "スマートプレイリストの再評価が終わるまで待ってください",
        ));
    }
    let token = body.plan_token;
    let res = state
        .db
        .write(move |c| {
            let now = now_epoch();
            let tx = c.transaction()?;
            let r = match dbdev::confirm_plan(&tx, id, &token, now) {
                Ok(Confirm::Created(open) | Confirm::Existing(open)) => {
                    let job_id = dbjobs::enqueue(&tx, &sync_job(id, open.id), now)?.id();
                    dbdev::set_plan_job(&tx, open.id, job_id)?;
                    SyncResult::Job(job_id)
                }
                Ok(Confirm::OpenPlanExists(_)) => SyncResult::OpenPlanExists,
                Ok(Confirm::Mismatch { plan_token }) => SyncResult::Mismatch(plan_token),
                Ok(Confirm::NotFound) => SyncResult::NotFound,
                Err(DbError::Internal(msg)) => {
                    tracing::warn!(device_id = id, error = %msg, "計画を確定できない");
                    SyncResult::Unreadable
                }
                Err(e) => return Err(e),
            };
            tx.commit()?;
            Ok(r)
        })
        .await?;
    Ok(match res {
        SyncResult::Job(job_id) => {
            state.jobs.notify_enqueued(&[job_id]).await;
            accepted(job_id)
        }
        SyncResult::OpenPlanExists => conflict(
            "open_plan_exists",
            "別の計画が途中にある。再開するか破棄してから",
        ),
        SyncResult::Mismatch(plan_token) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "plan_changed",
                "message": "計画が変わった。差分を取り直してください",
                "plan_token": plan_token,
            })),
        )
            .into_response(),
        SyncResult::Unreadable => plan_unreadable(),
        SyncResult::NotFound => not_found(),
    })
}

enum ResumeResult {
    Job(i64),
    NoPlan,
    Unreadable,
}

/// open な計画の同期をもう一度投入する（同じ計画の未完了のジョブがあればそれを返す）
pub async fn resume(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    if let Err(r) = adb_device(&state, id).await? {
        return Ok(r);
    }
    let res = state
        .db
        .write(move |c| {
            let now = now_epoch();
            let tx = c.transaction()?;
            let r = match dbdev::open_plan(&tx, id) {
                Ok(Some(open)) => {
                    let job_id = dbjobs::enqueue(&tx, &sync_job(id, open.id), now)?.id();
                    dbdev::set_plan_job(&tx, open.id, job_id)?;
                    ResumeResult::Job(job_id)
                }
                Ok(None) => ResumeResult::NoPlan,
                Err(DbError::Internal(msg)) => {
                    tracing::warn!(device_id = id, error = %msg, "計画を読めない");
                    ResumeResult::Unreadable
                }
                Err(e) => return Err(e),
            };
            tx.commit()?;
            Ok(r)
        })
        .await?;
    Ok(match res {
        ResumeResult::Job(job_id) => {
            state.jobs.notify_enqueued(&[job_id]).await;
            accepted(job_id)
        }
        ResumeResult::NoPlan => no_open_plan(),
        ResumeResult::Unreadable => plan_unreadable(),
    })
}

#[derive(Deserialize, Default)]
pub struct AbandonBody {
    /// 端末につながずに計画を閉じる（D-98）
    #[serde(default)]
    pub force: bool,
}

/// open な計画を破棄する（仕様 ③「計画の終端」）。端末で回復を済ませ（封印済みバッチの `vacating` 以降は
/// 前進で完遂、`prepared` 以前は破棄）、キャッシュを置き換えてから計画を閉じる。同期のジョブと
/// 直列化するため、端末のロックを持ったまま閉じる。`force` なら端末につながない（[`force_abandon`]）
pub async fn abandon(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    body: Option<Json<AbandonBody>>,
) -> Result<Response, ApiError> {
    if body.is_some_and(|Json(b)| b.force) {
        return force_abandon(&state, id).await;
    }
    let (d, rt) = match adb_device(&state, id).await? {
        Ok(x) => x,
        Err(r) => return Ok(r),
    };
    let (Some(serial), Some(volume), Some(root)) = (
        d.adb_serial.clone(),
        d.adb_volume.clone(),
        d.adb_root.clone(),
    ) else {
        return Err(ApiError::Internal(format!(
            "端末 {id} の adb の設定（シリアル・ボリューム・root）が欠けている"
        )));
    };
    let running = state
        .db
        .read(move |c| dbdev::has_running_device_job(c, id, "device_sync"))
        .await?;
    if running {
        return Ok(busy());
    }
    if !rt.is_connected(&serial) {
        return Ok(not_connected(&rt, &serial));
    }
    let Ok(_guard) = rt.device_lock(id).try_lock_owned() else {
        return Ok(busy());
    };
    let Some(plan_id) = state.db.read(move |c| dbdev::open_plan_id(c, id)).await? else {
        return Ok(no_open_plan());
    };
    let fs = match rt.fs_for(&serial, &volume, &root) {
        Ok(fs) => fs,
        Err(e) => return Ok(remote_failed(&rt, &serial, e)),
    };
    let expect = Expect {
        device_uuid: d.uuid.clone(),
        volume,
    };
    let rec = match recover(&fs, &expect).await {
        Ok(r) => r,
        Err(RecoverError::Store(e)) => return Ok(store_failed(&rt, &serial, e)),
        Err(e) => return Ok(device_failed(e.to_string())),
    };
    let (items, playlists) = (rec.items, rec.playlists);
    let queued = state
        .db
        .write(move |c| {
            let now = now_epoch();
            let tx = c.transaction()?;
            dbdev::apply_device_state(&tx, id, &items, &playlists, None, false, now)?;
            dbdev::close_plan(&tx, plan_id, PlanEnd::Abandoned, None, now)?;
            let queued = dbdev::queued_device_jobs(&tx, id, &["device_sync"])?;
            tx.commit()?;
            Ok(queued)
        })
        .await?;
    abandoned(&state, id, queued).await
}

/// 閉じた計画の待ちの同期を取り消して、端末の view を返す
async fn abandoned(state: &AppState, id: i64, queued: Vec<i64>) -> Result<Response, ApiError> {
    // 閉じた計画のジョブは走っても何もしないが、待ちの表示を残さない
    for job_id in queued {
        state.jobs.cancel(job_id).await?;
    }
    Ok(match view_of(state, id).await? {
        Some(v) => Json(v).into_response(),
        None => not_found(),
    })
}

enum ForceResult {
    Closed(Vec<i64>),
    NoPlan,
    Busy,
}

/// 端末につながずに open な計画を閉じる（D-98。端末が戻らないときの逃げ道）。回復も
/// キャッシュの置き換えもしない。次につないだときの回復が、端末のジャーナルから封印済みバッチを
/// 計画の状態と無関係に完遂または取り消すので安全。ADB 同期が無効でも使える（adb を使わない）。
/// 実行中の同期があるか、端末のロックを別の処理が持っていれば 409 `busy`。計画は行の id で閉じる
/// （JSON が壊れていても閉じられる）
async fn force_abandon(state: &AppState, id: i64) -> Result<Response, ApiError> {
    let Some(d) = state.db.read(move |c| dbdev::get(c, id)).await? else {
        return Ok(not_found());
    };
    if d.transport != Transport::Adb {
        return Ok(bad_request("Android（adb）の端末ではない"));
    }
    // 同期のジョブ・通常の破棄と直列化する（ADB 同期が無効ならそれらは動かない）
    let _guard = match state.adb.as_ref() {
        Some(rt) => match rt.device_lock(id).try_lock_owned() {
            Ok(g) => Some(g),
            Err(_) => return Ok(busy()),
        },
        None => None,
    };
    let res = state
        .db
        .write(move |c| {
            let tx = c.transaction()?;
            if dbdev::has_running_device_job(&tx, id, "device_sync")? {
                return Ok(ForceResult::Busy);
            }
            let Some(plan_id) = dbdev::open_plan_id(&tx, id)? else {
                return Ok(ForceResult::NoPlan);
            };
            dbdev::close_plan(&tx, plan_id, PlanEnd::Abandoned, None, now_epoch())?;
            let queued = dbdev::queued_device_jobs(&tx, id, &["device_sync"])?;
            tx.commit()?;
            tracing::info!(device_id = id, plan_id, "端末につながずに計画を破棄した");
            Ok(ForceResult::Closed(queued))
        })
        .await?;
    match res {
        ForceResult::Closed(queued) => abandoned(state, id, queued).await,
        ForceResult::NoPlan => Ok(no_open_plan()),
        ForceResult::Busy => Ok(busy()),
    }
}

/// 端末の全曲の sha256 を検証するジョブを投入する
pub async fn verify(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    if let Err(r) = adb_device(&state, id).await? {
        return Ok(r);
    }
    let job_id = state.jobs.enqueue(verify_job(id)).await?.id();
    Ok(accepted(job_id))
}

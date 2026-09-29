//! 端末 API（UI 向け。仕様「API の変更一覧」、D-95）

use super::error::{error_response, error_response_with_message, ApiError};
use super::AppState;
use crate::db::devices::{
    self as dbdev, Device, DevicePatch, NewDevice, PlaylistCheck, Selection, Snapshot, Update,
};
use crate::db::now_epoch;
use crate::domain::derived::Variant;
use crate::domain::device::PendingSets;
use crate::domain::device::{Counts, Hold, TrackState, Transport};
use crate::domain::filter::Filter;
use crate::playlist::dsl::Rule;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::OptionalExtension as _;
use serde::{Deserialize, Serialize};

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
    /// 接続状態（adb は P5-3 で埋める。agent は常に null）
    pub connected: Option<bool>,
    pub counts: Counts,
    pub last_synced_at: Option<i64>,
    pub playlist_ids: Vec<i64>,
    /// open な計画か同期ジョブがある（設定を変えられない）
    pub open_plan: bool,
}

#[derive(Serialize)]
pub struct DeviceList {
    pub items: Vec<DeviceView>,
}

fn view(d: &Device, snap: &Snapshot, playlist_ids: Vec<i64>, open_plan: bool) -> DeviceView {
    DeviceView {
        id: d.id,
        name: d.name.clone(),
        transport: d.transport.as_str(),
        variant: d.variant.as_str(),
        selection: d.selection.as_str(),
        generation: d.generation,
        connected: None,
        counts: snap.get(d.id).map(|s| s.counts).unwrap_or_default(),
        last_synced_at: d.last_synced_at,
        playlist_ids,
        open_plan,
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
            Ok(Some((
                d,
                dbdev::playlist_ids(c, id)?,
                dbdev::has_open_work(c, id)?,
            )))
        })
        .await?;
    Ok(found.map(|(d, pl, open)| view(&d, &snap, pl, open)))
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
                let pl = dbdev::playlist_ids(c, d.id)?;
                let open = dbdev::has_open_work(c, d.id)?;
                out.push((d, pl, open));
            }
            Ok(out)
        })
        .await?;
    let items = rows
        .iter()
        .map(|(d, pl, open)| view(d, &snap, pl.clone(), *open))
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
        return Ok(bad_request(
            "Android（USB）の登録は接続の検出から行う（P5-3 で対応）",
        ));
    }
    let created = state
        .db
        .write(move |c| {
            let key = crate::domain::relpath::canonical_key(&name);
            let taken: Option<i64> = c
                .query_row("SELECT id FROM devices WHERE name_key = ?1", [key], |r| {
                    r.get(0)
                })
                .optional()?;
            if taken.is_some() {
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

pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let res = state
        .db
        .write(move |c| {
            if dbdev::get(c, id)?.is_none() {
                return Ok(Guarded::NotFound);
            }
            if dbdev::has_open_work(c, id)? {
                return Ok(Guarded::OpenPlan);
            }
            dbdev::delete(c, id).map(Guarded::Done)
        })
        .await?;
    Ok(match res {
        Guarded::Done(_) => StatusCode::NO_CONTENT.into_response(),
        Guarded::NotFound => not_found(),
        Guarded::OpenPlan => open_plan(),
    })
}

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
    let pending = state.reeval_pending();
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
            free: None,
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

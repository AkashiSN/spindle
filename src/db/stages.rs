//! Inbox の承認済みの件の段（④ 可視化 B）: 配置 → RG → 系統 → 端末

use rusqlite::{params, Connection, OptionalExtension as _};
use serde::Serialize;

use crate::db::derived as dbderived;
use crate::db::devices::Snapshot;
use crate::db::inbox::ItemState;
use crate::db::Result;
use crate::domain::derived::{eligible, plan};
use crate::domain::device::TrackState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    Done,
    Running,
    Todo,
    Na,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stage {
    pub key: String,
    pub label: String,
    pub status: StageStatus,
    pub done: usize,
    pub total: usize,
}

/// total が 0 なら Na、全部済みなら Done、動いている / 一部済みなら Running、それ以外 Todo
pub fn status_of(done: usize, total: usize, running: bool) -> StageStatus {
    if total == 0 {
        StageStatus::Na
    } else if done == total {
        StageStatus::Done
    } else if running || done > 0 {
        StageStatus::Running
    } else {
        StageStatus::Todo
    }
}

fn stage(
    key: impl Into<String>,
    label: impl Into<String>,
    done: usize,
    total: usize,
    running: bool,
) -> Stage {
    Stage {
        key: key.into(),
        label: label.into(),
        status: status_of(done, total, running),
        done,
        total,
    }
}

fn rg_running(conn: &Connection, track_id: i64) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM jobs WHERE state IN ('queued', 'running')
               AND (dedup_key = 'rg:track:' || ?1
                    OR dedup_key = 'rg:album:' || (SELECT album_id FROM tracks WHERE id = ?1))
             LIMIT 1",
            params![track_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub fn stages(
    conn: &Connection,
    state: ItemState,
    track_ids: &[i64],
    snap: &Snapshot,
) -> Result<Vec<Stage>> {
    let place = match state {
        ItemState::Placed => stage("place", "配置", track_ids.len(), track_ids.len(), false),
        ItemState::Placing => Stage {
            key: "place".into(),
            label: "配置".into(),
            status: StageStatus::Running,
            done: 0,
            total: 0,
        },
        _ => Stage {
            key: "place".into(),
            label: "配置".into(),
            status: StageStatus::Todo,
            done: 0,
            total: 0,
        },
    };
    if state != ItemState::Placed {
        return Ok(vec![place]);
    }
    let mut out = vec![place];

    let (mut rg_total, mut rg_done, mut rg_run) = (0, 0, false);
    for id in track_ids {
        // 完了は 3 列が揃ったとき（端末配信の track_inputs の rg_ready と同じ条件。時刻だけ残った行は未完了）
        let ready: Option<bool> = conn
            .query_row(
                "SELECT rg_scanned_at IS NOT NULL AND rg_track_gain IS NOT NULL AND rg_track_peak IS NOT NULL
                   FROM tracks WHERE id = ?1 AND missing_since IS NULL",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(ready) = ready else { continue };
        rg_total += 1;
        if ready {
            rg_done += 1;
        } else if rg_running(conn, *id)? {
            rg_run = true;
        }
    }
    out.push(stage("rg", "RG", rg_done, rg_total, rg_run));

    for s in dbderived::variant_settings(conn)? {
        let key = s.variant.as_str();
        if !s.enabled {
            out.push(Stage {
                key: key.into(),
                label: format!("{key}（無効）"),
                status: StageStatus::Na,
                done: 0,
                total: 0,
            });
            continue;
        }
        let (mut total, mut done, mut running) = (0, 0, false);
        for id in track_ids {
            let Some(t) = dbderived::load_target(conn, *id)? else {
                continue;
            };
            if !eligible(&s, &t) {
                continue;
            }
            total += 1;
            let current = dbderived::get(conn, *id, s.variant)?;
            if !plan(&s, &t, current.as_ref()).needs_job() {
                done += 1;
            } else if dbderived::has_active_job(conn, *id, s.variant)? {
                running = true;
            }
        }
        out.push(stage(key, key, done, total, running));
    }

    for d in &snap.devices {
        let (mut total, mut done) = (0, 0);
        for id in track_ids {
            match d.states.get(id) {
                None | Some(TrackState::Removing) => {}
                Some(TrackState::Synced { .. }) => {
                    total += 1;
                    done += 1;
                }
                Some(_) => total += 1,
            }
        }
        out.push(stage(
            format!("device:{}", d.device.id),
            d.device.name.clone(),
            done,
            total,
            false,
        ));
    }
    Ok(out)
}

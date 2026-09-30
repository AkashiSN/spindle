//! 計画の確定・終端とキャッシュの全置換（仕様 ②③「計画の終端」）

use rusqlite::{params, Connection};
use spindle::config::{DerivedConfig, OpusVariantConfig};
use spindle::db::devices::{self, Confirm, NewDevice, PlanEnd, Selection};
use spindle::db::{derived, open_memory_connection};
use spindle::domain::derived::Variant;
use spindle::domain::device::*;

fn conn() -> Connection {
    let c = open_memory_connection().unwrap();
    let cfg = DerivedConfig {
        opus: OpusVariantConfig {
            enabled: true,
            bitrate: 256,
        },
        aac: Default::default(),
    };
    derived::sync_variants(&c, &cfg, false, 0).unwrap();
    c
}

fn device(c: &Connection) -> devices::Device {
    devices::create(
        c,
        &NewDevice {
            name: "Xperia",
            transport: Transport::Adb,
            variant: Variant::Opus,
            selection: Selection::All,
            adb: Some(("SER1", "emulated", "Music/spindle")),
        },
        0,
    )
    .unwrap()
}

/// Library に無い曲の写しが端末にある → 差分は削除 1 件
fn with_orphan(c: &Connection, device_id: i64) {
    devices::replace_items(
        c,
        device_id,
        &[DeviceItem {
            track_id: 99,
            dest_path: "gone.opus".into(),
            token: "t".into(),
            size: 1,
            sha256: "h".into(),
        }],
        0,
    )
    .unwrap();
}

#[test]
fn confirm_creates_then_is_idempotent() {
    let c = conn();
    let d = device(&c);
    with_orphan(&c, d.id);
    let token = devices::compute(&c, d.id).unwrap().unwrap().plan_token;
    let Confirm::Created(p) = devices::confirm_plan(&c, d.id, &token, 10).unwrap() else {
        panic!("作られない");
    };
    assert_eq!(p.plan.items.len(), 1);
    assert_eq!(p.plan.items[0].op, OpKind::Delete);
    assert_eq!(p.plan.plan_token, token);
    let Confirm::Existing(again) = devices::confirm_plan(&c, d.id, &token, 11).unwrap() else {
        panic!("冪等でない");
    };
    assert_eq!(again, p);
    assert_eq!(devices::open_plan(&c, d.id).unwrap(), Some(p.clone()));
    assert!(matches!(
        devices::confirm_plan(&c, d.id, "other", 12).unwrap(),
        Confirm::OpenPlanExists(o) if o.id == p.id
    ));
    // has_open_work（P5-2 の 409 open_plan）が効く
    assert!(devices::has_open_work(&c, d.id).unwrap());
}

#[test]
fn confirm_refuses_a_stale_token() {
    let c = conn();
    let d = device(&c);
    let now = devices::compute(&c, d.id).unwrap().unwrap().plan_token;
    assert_eq!(
        devices::confirm_plan(&c, d.id, "stale", 10).unwrap(),
        Confirm::Mismatch { plan_token: now }
    );
    assert_eq!(devices::open_plan(&c, d.id).unwrap(), None);
    assert_eq!(
        devices::confirm_plan(&c, 12345, "x", 10).unwrap(),
        Confirm::NotFound
    );
}

#[test]
fn close_is_compare_and_set() {
    let c = conn();
    let d = device(&c);
    let token = devices::compute(&c, d.id).unwrap().unwrap().plan_token;
    let Confirm::Created(p) = devices::confirm_plan(&c, d.id, &token, 10).unwrap() else {
        panic!();
    };
    c.execute(
        "INSERT INTO jobs (id, type, payload, created_at) VALUES (500, 'device_sync', '{}', 0)",
        [],
    )
    .unwrap();
    devices::set_plan_job(&c, p.id, 500).unwrap();
    assert_eq!(
        devices::open_plan(&c, d.id).unwrap().unwrap().job_id,
        Some(500)
    );
    assert!(devices::close_plan(&c, p.id, PlanEnd::Completed, None, 20).unwrap());
    assert!(!devices::close_plan(&c, p.id, PlanEnd::Abandoned, None, 21).unwrap());
    let (state, closed): (String, i64) = c
        .query_row(
            "SELECT state, closed_at FROM device_sync_plans WHERE id = ?1",
            [p.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((state.as_str(), closed), ("completed", 20));
    assert_eq!(devices::open_plan(&c, d.id).unwrap(), None);
    // 終端した後は新しい計画を確定できる
    assert!(matches!(
        devices::confirm_plan(&c, d.id, &token, 30).unwrap(),
        Confirm::Created(_)
    ));
}

#[test]
fn apply_device_state_replaces_the_cache() {
    let c = conn();
    let d = device(&c);
    with_orphan(&c, d.id);
    c.execute(
        "INSERT INTO device_errors (device_id, kind, ref_id, reason, reported_at) VALUES (?1, 'track', 5, '古い', 0)",
        params![d.id],
    )
    .unwrap();
    let items = [DeviceItem {
        track_id: 7,
        dest_path: "a.opus".into(),
        token: "t7".into(),
        size: 3,
        sha256: "h7".into(),
    }];
    let pls = [PlaylistState {
        playlist_id: 4,
        dest_path: "Playlists/x.m3u8".into(),
        token: "p".into(),
    }];
    // errors = None: 前回の同期のエラーは残す。synced = false: last_synced_at は進めない
    devices::apply_device_state(&c, d.id, &items, &pls, None, false, 50).unwrap();
    assert_eq!(devices::items(&c, d.id).unwrap(), items.to_vec());
    assert_eq!(devices::playlist_states(&c, d.id).unwrap(), pls.to_vec());
    assert_eq!(devices::device_errors(&c, d.id).unwrap().len(), 1);
    assert_eq!(
        devices::get(&c, d.id).unwrap().unwrap().last_synced_at,
        None
    );

    let errors = [(EntryKind::Playlist, 4, "名前の衝突".to_string())];
    devices::apply_device_state(&c, d.id, &items, &pls, Some(&errors), true, 60).unwrap();
    assert_eq!(
        devices::device_errors(&c, d.id).unwrap(),
        vec![("playlist".to_string(), 4, "名前の衝突".to_string())]
    );
    assert_eq!(
        devices::get(&c, d.id).unwrap().unwrap().last_synced_at,
        Some(60)
    );
}

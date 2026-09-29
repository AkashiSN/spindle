//! 0003_rg_auto_write の性質（D-96）。0002 まで当てた既存 DB に 0003 を流す（jobs の作り直し）

use rusqlite::Connection;
use spindle::db::jobs::JobType;
use spindle::db::migrations;

/// 0002 で足された jobs の参照（`device_sync_plans.job_id`）を含む子の行が、jobs の作り直しの後も同じ
/// id のまま残り、FK が新しい jobs を指し続け（foreign_key_check が空、SET NULL / CASCADE が効く）、
/// rgwrite を受け付け、`rg_write_due` は既存行で 0 になる
#[test]
fn upgrade_from_0002_keeps_jobs_children_and_adds_rgwrite() {
    let mut c = Connection::open_in_memory().unwrap();
    c.pragma_update(None, "foreign_keys", "ON").unwrap();
    let all = migrations::embedded().unwrap();
    let upto2: Vec<_> = all.iter().filter(|m| m.version <= 2).cloned().collect();
    assert_eq!(migrations::apply_list(&mut c, &upto2).unwrap(), vec![1, 2]);

    c.execute_batch(
        "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless, seen_at)
           VALUES (5, 'a/5.flac', 'a/5.flac', 0, 0, 0, 'flac', 1, 0);
         INSERT INTO jobs (id, type, payload, state, created_at)
           VALUES (101, 'device_sync', '{}', 'done', 1), (102, 'rg', '{}', 'running', 2);
         INSERT INTO devices (id, uuid, name, name_key, transport, variant, selection,
                              adb_serial, adb_volume, adb_root, created_at, updated_at)
           VALUES (1, printf('%032x', 1), 'p', 'p', 'adb', 'opus', 'all', 'S', 'emulated', 'M', 0, 0);
         INSERT INTO device_sync_plans (id, device_id, job_id, plan_token, plan, state, created_at)
           VALUES (7, 1, 101, 't', '{}', 'completed', 1);
         INSERT INTO track_locks (track_id, job_id, acquired_at) VALUES (5, 102, 1);",
    )
    .unwrap();

    assert_eq!(migrations::apply_list(&mut c, &all).unwrap(), vec![3]);

    let count = |sql: &str| -> i64 { c.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("PRAGMA foreign_keys"), 1, "適用後は FK が ON に戻る");
    let fk_violations = c
        .prepare("PRAGMA foreign_key_check")
        .unwrap()
        .query_map([], |_| Ok(()))
        .unwrap()
        .count();
    assert_eq!(fk_violations, 0);
    assert_eq!(count("SELECT count(*) FROM jobs"), 2);
    assert_eq!(
        count("SELECT job_id FROM device_sync_plans WHERE id = 7"),
        101
    );
    assert_eq!(
        count("SELECT job_id FROM track_locks WHERE track_id = 5"),
        102
    );
    assert_eq!(
        count("SELECT count(*) FROM sqlite_master WHERE sql LIKE '%jobs_new%'"),
        0
    );
    // 子の FK が作り直した jobs を指している
    c.execute("DELETE FROM jobs WHERE id = 101", []).unwrap();
    assert_eq!(
        count("SELECT job_id IS NULL FROM device_sync_plans WHERE id = 7"),
        1
    );
    c.execute("DELETE FROM jobs WHERE id = 102", []).unwrap();
    assert_eq!(count("SELECT count(*) FROM track_locks"), 0);

    // rgwrite を受け付ける。既存行の書き込み待ちの印は 0
    c.execute(
        "INSERT INTO jobs (type, payload, created_at) VALUES (?1, '{}', 3)",
        [JobType::Rgwrite.as_str()],
    )
    .unwrap();
    assert_eq!(count("SELECT rg_write_due FROM tracks WHERE id = 5"), 0);
    assert!(c
        .execute("UPDATE tracks SET rg_write_due = 2 WHERE id = 5", [])
        .is_err());
}

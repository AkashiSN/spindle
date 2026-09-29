//! 0002_devices の性質（仕様 ②）。空 DB にマイグレーションを流して確かめる

use rusqlite::{params, Connection};
use spindle::db::jobs::JobType;
use spindle::db::{migrations, open_memory_connection};

fn conn() -> Connection {
    open_memory_connection().unwrap()
}

fn insert_device(c: &Connection, id: i64, name: &str) {
    c.execute(
        "INSERT INTO devices (id, uuid, name, name_key, transport, variant, selection,
                              adb_serial, adb_volume, adb_root, created_at, updated_at)
         VALUES (?1, printf('%032x', ?1), ?2, lower(?2), 'adb', 'opus', 'all',
                 'SER' || ?1, 'emulated', 'Music/spindle', 0, 0)",
        params![id, name],
    )
    .unwrap();
}

#[test]
fn device_tables_exist_and_cascade() {
    let c = conn();
    insert_device(&c, 1, "Xperia");
    c.execute(
        "INSERT INTO device_items (device_id, track_id, dest_path, dest_path_key, token, size, sha256, synced_at)
         VALUES (1, 99, 'a.opus', 'a.opus', 't', 1, 's', 0)",
        [],
    )
    .unwrap();
    c.execute("DELETE FROM devices WHERE id = 1", []).unwrap();
    let n: i64 = c
        .query_row("SELECT count(*) FROM device_items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "端末を消せば反映済みのキャッシュも消える");
}

#[test]
fn adb_device_requires_serial_volume_root() {
    let c = conn();
    let err = c.execute(
        "INSERT INTO devices (uuid, name, name_key, transport, variant, selection, created_at, updated_at)
         VALUES ('u', 'X', 'x', 'adb', 'opus', 'all', 0, 0)",
        [],
    );
    assert!(err.is_err(), "adb は serial / volume / root が必須");
}

#[test]
fn only_one_open_plan_per_device() {
    let c = conn();
    insert_device(&c, 1, "Xperia");
    let ins = |id: i64| {
        c.execute(
            "INSERT INTO device_sync_plans (id, device_id, plan_token, plan, state, created_at)
             VALUES (?1, 1, 'p', '[]', 'open', 0)",
            [id],
        )
    };
    ins(1).unwrap();
    assert!(ins(2).is_err(), "open は端末ごとに 1 つまで");
    c.execute(
        "UPDATE device_sync_plans SET state = 'completed' WHERE id = 1",
        [],
    )
    .unwrap();
    ins(2).unwrap();
}

#[test]
fn source_hashes_follow_track_deletes() {
    let c = conn();
    c.execute(
        "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless, seen_at)
         VALUES (1, 'a.flac', 'a.flac', 1, 0, 0, 'flac', 1, 0)",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO source_hashes (track_id, source, token, inode, size, mtime_ns, ctime_ns, sha256, computed_at)
         VALUES (1, 'master', 't', 1, 1, 0, 0, 's', 0)",
        [],
    )
    .unwrap();
    c.execute("DELETE FROM tracks WHERE id = 1", []).unwrap();
    let n: i64 = c
        .query_row("SELECT count(*) FROM source_hashes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn playlists_have_evaluated_at() {
    let c = conn();
    c.execute(
        "INSERT INTO playlists (name, name_key, created_at, updated_at, evaluated_at)
         VALUES ('p', 'p', 0, 0, 5)",
        [],
    )
    .unwrap();
}

#[test]
fn jobs_accept_device_types_and_keep_indexes() {
    let c = conn();
    for ty in ["source_hash", "device_scan", "device_sync", "device_verify"] {
        c.execute(
            "INSERT INTO jobs (type, payload, created_at) VALUES (?1, '{}', 0)",
            [ty],
        )
        .unwrap();
    }
    assert!(c
        .execute(
            "INSERT INTO jobs (type, payload, created_at) VALUES ('nope', '{}', 0)",
            []
        )
        .is_err());
    // 作り直しで索引を落としていない
    for idx in [
        "idx_jobs_dedup_active",
        "idx_jobs_queue",
        "idx_jobs_finished",
        "idx_jobs_batch",
    ] {
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                [idx],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "{idx} が無い");
    }
    // 子表の FK が jobs を指したまま
    let fk: String = c
        .query_row(
            "SELECT \"table\" FROM pragma_foreign_key_list('job_mutexes')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fk, "jobs");
}

#[test]
fn source_hash_job_type_properties() {
    let t: JobType = "source_hash".parse().unwrap();
    assert_eq!(t, JobType::SourceHash);
    assert_eq!(t.concurrency(8), 2);
    assert!(t.cpu_bound());
    assert_eq!(t.version_field(), None);
    assert!(JobType::ALL.contains(&JobType::SourceHash));
}

/// 0001 だけの既存 DB に 0002 を流す（jobs の作り直し）。jobs 行とそれを参照する子の行が同じ id の
/// まま残り、FK が新しい jobs を指し続け（foreign_key_check が空、CASCADE / SET NULL が効く）、
/// 新しいジョブ種別を受け付ける
#[test]
fn upgrade_from_0001_keeps_jobs_and_children() {
    let mut c = Connection::open_in_memory().unwrap();
    c.pragma_update(None, "foreign_keys", "ON").unwrap();
    let all = migrations::embedded().unwrap();
    let v1: Vec<_> = all.iter().filter(|m| m.version == 1).cloned().collect();
    assert_eq!(migrations::apply_list(&mut c, &v1).unwrap(), vec![1]);

    c.execute_batch(
        "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless, seen_at)
           VALUES (5, 'a/5.flac', 'a/5.flac', 0, 0, 0, 'flac', 1, 0);
         INSERT INTO jobs (id, type, payload, state, created_at) VALUES (101, 'scan', '{}', 'done', 1),
                                                                         (102, 'rg', '{}', 'running', 2);
         INSERT INTO edit_batches (id, created_at, state, affected) VALUES (1, 1, 'applied', 1);
         INSERT INTO edit_ops (id, batch_id, ordinal, track_id, kind, result, job_id)
           VALUES (10, 1, 0, 5, 'tags', 'applied', 101);
         INSERT INTO job_mutexes (name, job_id, acquired_at) VALUES ('library', 102, 1);
         INSERT INTO track_locks (track_id, job_id, acquired_at) VALUES (5, 102, 1);",
    )
    .unwrap();

    assert_eq!(migrations::apply_list(&mut c, &all).unwrap(), vec![2, 3]);

    let count = |sql: &str| -> i64 { c.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("PRAGMA foreign_keys"), 1, "適用後は FK が ON に戻る");
    let fk_violations: i64 = c
        .prepare("PRAGMA foreign_key_check")
        .unwrap()
        .query_map([], |_| Ok(()))
        .unwrap()
        .count() as i64;
    assert_eq!(fk_violations, 0);
    let ids: Vec<(i64, String, String)> = c
        .prepare("SELECT id, type, state FROM jobs ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        ids,
        vec![
            (101, "scan".into(), "done".into()),
            (102, "rg".into(), "running".into())
        ]
    );
    assert_eq!(count("SELECT job_id FROM edit_ops WHERE id = 10"), 101);
    assert_eq!(
        count("SELECT job_id FROM job_mutexes WHERE name = 'library'"),
        102
    );
    assert_eq!(
        count("SELECT job_id FROM track_locks WHERE track_id = 5"),
        102
    );
    // 子の FK が作り直した jobs を指している（jobs_new や消えた表を指していない）
    assert_eq!(
        count("SELECT count(*) FROM sqlite_master WHERE sql LIKE '%jobs_new%'"),
        0
    );
    c.execute("DELETE FROM jobs WHERE id = 102", []).unwrap();
    assert_eq!(count("SELECT count(*) FROM job_mutexes"), 0);
    assert_eq!(count("SELECT count(*) FROM track_locks"), 0);
    c.execute("DELETE FROM jobs WHERE id = 101", []).unwrap();
    assert_eq!(
        count("SELECT job_id IS NULL FROM edit_ops WHERE id = 10"),
        1
    );

    for t in ["source_hash", "device_scan", "device_sync", "device_verify"] {
        c.execute(
            "INSERT INTO jobs (type, payload, created_at) VALUES (?1, '{}', 1)",
            [t],
        )
        .unwrap();
    }
}

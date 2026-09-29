//! 0002_devices の性質（仕様 ②）。空 DB にマイグレーションを流して確かめる

use rusqlite::{params, Connection};
use spindle::db::jobs::JobType;
use spindle::db::open_memory_connection;

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

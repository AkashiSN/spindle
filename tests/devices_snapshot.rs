//! 全端末のスナップショット（Task 4）: 状態・件数・未反映の集合と、書き込みで無効化されるキャッシュ

use std::sync::Arc;

use spindle::db::devices::{self, NewDevice, Selection};
use spindle::db::Db;
use spindle::domain::derived::Variant;
use spindle::domain::device::*;

async fn db() -> (Arc<Db>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
    (db, dir)
}

async fn seed(db: &Db) -> i64 {
    db.write(|c| {
        c.execute(
            "INSERT INTO tracks (id, rel_path, rel_path_key, inode, size, mtime_ns, ctime_ns, codec, lossless,
                                 channels, audio_version, tag_version, seen_at)
             VALUES (1, 'YT/a.opus', 'yt/a.opus', 1, 1, 0, 0, 'opus', 0, 2, 1, 1, 0)",
            [],
        )?;
        let d = devices::create(
            c,
            &NewDevice { name: "iPhone", transport: Transport::Agent, variant: Variant::Aac, selection: Selection::All, adb: None },
            10,
        )?;
        Ok(d.id)
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn snapshot_reports_states_and_pending_sets() {
    let (db, _dir) = db().await;
    let id = seed(&db).await;
    let snap = db.device_snapshot().await.unwrap();
    let d = snap.get(id).unwrap();
    // aac 系統で Derived が無い → 待ち（RG 未解析 or Derived 未生成）
    assert!(matches!(d.states.get(&1), Some(TrackState::Waiting { .. })));
    assert_eq!(d.counts.waiting, 1);
    assert!(snap.pending_sets().for_id(id).is_empty());
}

#[tokio::test]
async fn snapshot_cache_is_invalidated_by_any_write() {
    let (db, _dir) = db().await;
    let id = seed(&db).await;
    let a = db.device_snapshot().await.unwrap();
    let b = db.device_snapshot().await.unwrap();
    assert!(Arc::ptr_eq(&a, &b), "書き込みが無ければ使い回す");
    db.write(move |c| devices::set_playlists(c, id, &[], 20))
        .await
        .unwrap();
    let c = db.device_snapshot().await.unwrap();
    assert!(!Arc::ptr_eq(&a, &c), "書き込みの後は計算し直す");
    assert_eq!(c.get(id).unwrap().device.generation, 2);
}

#[tokio::test]
async fn snapshot_marks_pending_for_hashed_track() {
    let (db, _dir) = db().await;
    let id = db
        .write(|c| {
            c.execute(
                "INSERT INTO tracks (id, rel_path, rel_path_key, inode, size, mtime_ns, ctime_ns, codec, lossless,
                                     channels, audio_version, tag_version, seen_at)
                 VALUES (1, 'YT/a.opus', 'yt/a.opus', 1, 1, 0, 0, 'opus', 0, 2, 1, 1, 0)",
                [],
            )?;
            let d = devices::create(
                c,
                &NewDevice { name: "Xperia", transport: Transport::Adb, variant: Variant::Opus, selection: Selection::All,
                             adb: Some(("SER1", "emulated", "Music/spindle")) },
                10,
            )?;
            devices::put_source_hash(
                c, 1, SourceKind::Master,
                &SourceHash { semantic: semantic_master(1, 1), inode: 1, size: 1, mtime_ns: 0, ctime_ns: 0, sha256: "ab".repeat(32) },
                10,
            )?;
            Ok(d.id)
        })
        .await
        .unwrap();
    let snap = db.device_snapshot().await.unwrap();
    assert_eq!(snap.pending_sets().for_id(id), &[1]);
    assert_eq!(snap.pending_sets().for_key("xperia"), &[1]);
    assert_eq!(
        snap.states_of(1),
        vec![(
            id,
            TrackState::Pending {
                op: "add",
                reason: None
            }
        )]
    );
}

/// 同時にキャッシュが外れた要求は 1 回の計算を待って同じ結果を使う（計算を並べて走らせない）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_misses_share_one_computation() {
    let (db, _dir) = db().await;
    seed(&db).await;
    let before = db.device_snapshot().await.unwrap();
    db.write(|c| {
        c.execute("UPDATE devices SET name = name", [])?;
        Ok(())
    })
    .await
    .unwrap();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let db = db.clone();
            tokio::spawn(async move { db.device_snapshot().await.unwrap() })
        })
        .collect();
    let mut snaps = Vec::new();
    for h in handles {
        snaps.push(h.await.unwrap());
    }
    assert!(!Arc::ptr_eq(&before, &snaps[0]), "書き込みの後は計算し直す");
    for s in &snaps[1..] {
        assert!(
            Arc::ptr_eq(&snaps[0], s),
            "同時に外れた要求は同じ計算の結果を使う"
        );
    }
}

/// 表示用は許す古さの間は書き込みの後も使い回し、厳密なものは計算し直す。古さを超えれば表示用も計算し直す
#[tokio::test]
async fn display_snapshot_tolerates_recent_writes_but_strict_does_not() {
    let (db, _dir) = db().await;
    let id = seed(&db).await;
    let a = db.device_snapshot().await.unwrap();
    db.write(move |c| devices::set_playlists(c, id, &[], 20))
        .await
        .unwrap();
    let shown = db.device_snapshot_for_display().await.unwrap();
    assert!(
        Arc::ptr_eq(&a, &shown),
        "2 秒以内なら書き込みの後も表示用は使い回す"
    );
    let strict = db.device_snapshot().await.unwrap();
    assert!(!Arc::ptr_eq(&a, &strict), "厳密なものは計算し直す");
    assert_eq!(strict.get(id).unwrap().device.generation, 2);
    db.write(move |c| devices::set_playlists(c, id, &[], 21))
        .await
        .unwrap();
    let expired = db
        .device_snapshot_within(std::time::Duration::ZERO)
        .await
        .unwrap();
    assert!(!Arc::ptr_eq(&strict, &expired), "古さを超えれば計算し直す");
    assert_eq!(expired.get(id).unwrap().device.generation, 3);
}

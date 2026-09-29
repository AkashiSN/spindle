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

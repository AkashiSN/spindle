//! 保存した計画の実行（仕様 ⑤「同期」）

mod device_support;

use std::collections::HashMap;
use std::sync::Arc;

use device_support::*;
use spindle::device::journal::{parse, Record};
use spindle::device::ondevice::*;
use spindle::device::plan::{runnable, Runnable, StoredPlan};
use spindle::device::recover::{recover, STALE_TOKEN};
use spindle::device::store;
use spindle::device::sync::{self, *};
use spindle::domain::device::*;
use spindle::domain::relpath::RelPath;
use spindle::fsroot::{fstat, RootDir};

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(f)
}

fn manifest_ids(fs: &FakeFs) -> Vec<(i64, String)> {
    let m = parse_manifest(fs);
    m.items
        .iter()
        .map(|i| (i.track_id, i.path.clone()))
        .collect()
}

fn parse_manifest(fs: &FakeFs) -> DeviceManifest {
    spindle::device::ondevice::parse(&fs.get(MANIFEST_PATH).unwrap()).unwrap()
}

#[test]
fn first_sync_adds_everything_and_compacts() {
    block_on(async {
        let want: &[Want] = &[(1, "A/a.opus", b"A1"), (2, "B/b.opus", b"B1")];
        let fs = device_at(&[], 1 << 30).await;
        let rescans = fs.rescans();
        let report = sync_to(&fs, want, &TestControl::default()).await.unwrap();
        assert!(report.errors.is_empty());
        assert_eq!(fs.get("A/a.opus"), Some(b"A1".to_vec()));
        assert_eq!(
            manifest_ids(&fs),
            vec![(1, "A/a.opus".into()), (2, "B/b.opus".into())]
        );
        assert!(parse(&fs.get(JOURNAL_PATH).unwrap()).unwrap().is_empty());
        assert_eq!(fs.rescans(), rescans + 1);
    });
}

#[test]
fn put_orders_intent_sync_transfer_check_rename_sync() {
    block_on(async {
        let fs = device_at(&[], 1 << 30).await;
        let before = fs.calls().len();
        sync_to(&fs, &[(1, "a.opus", b"A")], &TestControl::default())
            .await
            .unwrap();
        let calls: Vec<String> = fs.calls()[before..]
            .iter()
            .filter(|c| !c.starts_with("read") && c.as_str() != "list" && c.as_str() != "free")
            .cloned()
            .collect();
        let pos = |s: &str| {
            calls
                .iter()
                .position(|c| c == s)
                .unwrap_or_else(|| panic!("{s}: {calls:?}"))
        };
        assert!(pos("append .spindle/journal") < pos("put a.opus.spindle-tmp"));
        assert_eq!(calls[pos("append .spindle/journal") + 1], "sync");
        assert!(pos("put a.opus.spindle-tmp") < pos("sha256 a.opus.spindle-tmp"));
        assert!(pos("sha256 a.opus.spindle-tmp") < pos("rename a.opus.spindle-tmp -> a.opus"));
        assert_eq!(
            calls[pos("rename a.opus.spindle-tmp -> a.opus") + 1],
            "sync"
        );
    });
}

#[test]
fn update_and_delete() {
    block_on(async {
        let fs = device_at(&[(1, "a.opus", b"A1"), (2, "b.opus", b"B1")], 1 << 30).await;
        fs.set("手で置いた.mp3", b"x");
        sync_to(
            &fs,
            &[(1, "a.opus", b"A2"), (3, "Dir/c.opus", b"C")],
            &TestControl::default(),
        )
        .await
        .unwrap();
        assert_eq!(fs.get("a.opus"), Some(b"A2".to_vec()));
        assert_eq!(fs.get("b.opus"), None);
        assert_eq!(fs.get("Dir/c.opus"), Some(b"C".to_vec()));
        assert_eq!(fs.get("手で置いた.mp3"), Some(b"x".to_vec()));
        assert_eq!(
            manifest_ids(&fs),
            vec![(1, "a.opus".into()), (3, "Dir/c.opus".into())]
        );
    });
}

#[test]
fn empty_dirs_are_pruned_after_delete() {
    block_on(async {
        let fs = device_at(&[(1, "Artist/Album/a.opus", b"A")], 1 << 30).await;
        sync_to(&fs, &[], &TestControl::default()).await.unwrap();
        assert!(
            fs.dirs()
                .iter()
                .all(|d| d == ".spindle" || d.starts_with(".spindle/")),
            "{:?}",
            fs.dirs()
        );
    });
}

#[test]
fn source_error_is_reported_and_others_continue() {
    block_on(async {
        let fs = device_at(&[], 1 << 30).await;
        let want: &[Want] = &[(1, "a.opus", b"A"), (2, "b.opus", b"B")];
        let rec = recover(&fs, &expect()).await.unwrap();
        let d = diff(&desired(want), &rec.items, &[], &[], Vec::new());
        let plan = StoredPlan::from_diff(1, "tok", &d).unwrap();
        let r = runnable(&plan, &rec.items, &[], &d);
        let mut src = sources(want);
        src.0.remove(&1);
        let report = sync::run(
            &fs,
            &src,
            &TestControl::default(),
            SyncInput {
                generation: 1,
                start: rec.manifest,
                runnable: &r,
                playlist_bodies: &HashMap::new(),
                origins: &origins_from(&r),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            report.errors,
            vec![(EntryKind::Track, 1, SourceError::Missing.reason())]
        );
        assert_eq!(fs.get("b.opus"), Some(b"B".to_vec()));
        assert_eq!(fs.get("a.opus"), None);
    });
}

#[test]
fn content_differing_from_the_plan_is_not_placed() {
    block_on(async {
        let fs = device_at(&[], 1 << 30).await;
        let want: &[Want] = &[(1, "a.opus", b"A")];
        let rec = recover(&fs, &expect()).await.unwrap();
        let d = diff(&desired(want), &rec.items, &[], &[], Vec::new());
        let plan = StoredPlan::from_diff(1, "tok", &d).unwrap();
        let r = runnable(&plan, &rec.items, &[], &d);
        // 送る元が確定後に書き換わった（identity の照合をすり抜けた場合でも送りながらのハッシュで止まる）
        let src = sources(&[(1, "a.opus", b"Z")]);
        let report = sync::run(
            &fs,
            &src,
            &TestControl::default(),
            SyncInput {
                generation: 1,
                start: rec.manifest,
                runnable: &r,
                playlist_bodies: &HashMap::new(),
                origins: &origins_from(&r),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            report.errors,
            vec![(EntryKind::Track, 1, SourceError::Changed.reason())]
        );
        assert_eq!(fs.get("a.opus"), None);
        assert!(fs.paths().iter().all(|p| !p.ends_with(TMP_SUFFIX)));
        assert!(report.manifest.items.is_empty());
    });
}

#[test]
fn not_enough_space_fails_before_writing() {
    block_on(async {
        // 空きは余裕（MARGIN_BYTES）+ 1000 バイト。送る曲は 4000 バイト
        let fs = device_at(&[], MARGIN_BYTES + 1000).await;
        let big = vec![0u8; 4000];
        let before = fs.mutations();
        let err = sync_to(&fs, &[(1, "a.opus", &big)], &TestControl::default())
            .await
            .unwrap_err();
        assert!(err.contains("空き容量"), "{err}");
        // 回復の圧縮は起きない（ジャーナルが空）ので、変更は 0 件
        assert_eq!(fs.mutations(), before);
    });
}

#[test]
fn cancel_and_generation_change_stop_between_ops() {
    block_on(async {
        let fs = device_at(&[], 1 << 30).await;
        let want: &[Want] = &[(1, "a.opus", b"A"), (2, "b.opus", b"B")];
        let c = TestControl {
            cancel_from: Some(2),
            ..Default::default()
        };
        let err = sync_to(&fs, want, &c).await.unwrap_err();
        assert!(err.contains("キャンセル"), "{err}");
        assert_eq!(fs.get("a.opus"), Some(b"A".to_vec()));
        assert_eq!(fs.get("b.opus"), None);

        let fs = device_at(&[], 1 << 30).await;
        let c = TestControl {
            stale_from: Some(1),
            ..Default::default()
        };
        let err = sync_to(&fs, want, &c).await.unwrap_err();
        assert!(err.contains("設定が変わった"), "{err}");
        assert_eq!(fs.get("a.opus"), None);
    });
}

#[test]
fn rescan_failure_is_a_warning() {
    block_on(async {
        let fs = device_at(&[], 1 << 30).await;
        fs.fail_rescan();
        let report = sync_to(&fs, &[(1, "a.opus", b"A")], &TestControl::default())
            .await
            .unwrap();
        assert_eq!(report.warnings.len(), 1);
    });
}

#[test]
fn playlists_are_written_renamed_and_deleted() {
    block_on(async {
        let fs = device_at(&[(1, "a.opus", b"A")], 1 << 30).await;
        let rec = recover(&fs, &expect()).await.unwrap();
        let body = b"#EXTM3U\n../a.opus\n".to_vec();
        let pl = |id: i64, name: &str| DesiredPlaylist {
            playlist_id: id,
            name: name.into(),
            dest_path: format!("Playlists/{name}.m3u8"),
            body: body.clone(),
            token: playlist_token(id, name, &sha256_hex(&body)),
        };
        let run_with =
            |pls: Vec<DesiredPlaylist>, cur_pl: Vec<PlaylistState>, start: DeviceManifest| {
                let fs = &fs;
                async move {
                    let d = diff(
                        &desired(&[(1, "a.opus", b"A")]),
                        &rec_items(fs).await,
                        &pls,
                        &cur_pl,
                        Vec::new(),
                    );
                    let plan = StoredPlan::from_diff(1, "tok", &d).unwrap();
                    let r = runnable(&plan, &rec_items(fs).await, &cur_pl, &d);
                    let bodies: HashMap<i64, Vec<u8>> = pls
                        .iter()
                        .map(|p| (p.playlist_id, p.body.clone()))
                        .collect();
                    sync::run(
                        fs,
                        &sources(&[(1, "a.opus", b"A")]),
                        &TestControl::default(),
                        SyncInput {
                            generation: 1,
                            start,
                            runnable: &r,
                            playlist_bodies: &bodies,
                            origins: &origins_from(&r),
                        },
                    )
                    .await
                    .unwrap()
                }
            };
        let report = run_with(vec![pl(5, "通勤")], vec![], rec.manifest.clone()).await;
        assert_eq!(fs.get("Playlists/通勤.m3u8"), Some(body.clone()));
        // 改名: 旧パスを消してから新パスへ
        let report = run_with(vec![pl(5, "朝")], report.playlists(), report.manifest).await;
        assert_eq!(fs.get("Playlists/通勤.m3u8"), None);
        assert_eq!(fs.get("Playlists/朝.m3u8"), Some(body.clone()));
        // 印を外した
        let report = run_with(vec![], report.playlists(), report.manifest).await;
        assert_eq!(fs.get("Playlists/朝.m3u8"), None);
        assert!(report.manifest.playlists.is_empty());
    });
}

async fn rec_items(fs: &FakeFs) -> Vec<DeviceItem> {
    recover(fs, &expect()).await.unwrap().items
}

#[test]
fn basic_changes_survive_every_crash_point() {
    block_on(async {
        let initial: &[Want] = &[
            (1, "a.opus", b"A1"),
            (2, "b.opus", b"B1"),
            (3, "c.opus", b"C1"),
        ];
        let target: &[Want] = &[
            (1, "a.opus", b"A2"),
            (3, "c.opus", b"C1"),
            (4, "Dir/d.opus", b"D1"),
        ];
        crash_sweep(initial, target, false).await;
    });
}

#[test]
fn stale_copy_is_updated_in_place() {
    block_on(async {
        let fs = device_at(&[(1, "a.opus", b"A1")], 1 << 30).await;
        fs.set("a.opus", b"broken!");
        sync_to(&fs, &[(1, "a.opus", b"A1")], &TestControl::default())
            .await
            .unwrap();
        assert_eq!(fs.get("a.opus"), Some(b"A1".to_vec()));
    });
}

#[test]
fn root_sources_rejects_identity_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.flac"), b"AAAA").unwrap();
    let root = Arc::new(RootDir::open(dir.path()).unwrap());
    let rel = RelPath::parse("a.flac").unwrap();
    let st = fstat(&root.open_file(&rel).unwrap()).unwrap();
    let entry = |inode: u64| SourceEntry {
        kind: SourceKind::Master,
        rel_path: rel.clone(),
        hash: SourceHash {
            semantic: "s".into(),
            inode,
            size: st.size,
            mtime_ns: st.mtime_ns,
            ctime_ns: st.ctime_ns,
            sha256: sha256_hex(b"AAAA"),
        },
    };
    let ok = RootSources::new(
        root.clone(),
        root.clone(),
        HashMap::from([(1, entry(st.inode))]),
    );
    assert!(ok.open(1).is_ok());
    assert!(matches!(ok.open(2), Err(SourceError::Missing)));
    let changed = RootSources::new(
        root.clone(),
        root.clone(),
        HashMap::from([(1, entry(st.inode + 1))]),
    );
    assert!(matches!(changed.open(1), Err(SourceError::Changed)));
    std::fs::remove_file(dir.path().join("a.flac")).unwrap();
    assert!(matches!(ok.open(1), Err(SourceError::Missing)));
    // symlink や通常ファイルでないものに差し替わったら「変わった」
    std::fs::write(dir.path().join("b.flac"), b"AAAA").unwrap();
    std::os::unix::fs::symlink(dir.path().join("b.flac"), dir.path().join("a.flac")).unwrap();
    assert!(matches!(ok.open(1), Err(SourceError::Changed)));
    std::fs::remove_file(dir.path().join("a.flac")).unwrap();
    std::fs::create_dir(dir.path().join("a.flac")).unwrap();
    assert!(matches!(ok.open(1), Err(SourceError::Changed)));
    // FIFO は書き手を待たずに「変わった」になる（回帰したら別スレッドが返らず失敗する）
    std::fs::remove_dir(dir.path().join("a.flac")).unwrap();
    let c = std::ffi::CString::new(dir.path().join("a.flac").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
    let (tx, rx) = std::sync::mpsc::channel();
    let ok2 = ok;
    std::thread::spawn(move || {
        let _ = tx.send(matches!(ok2.open(1), Err(SourceError::Changed)));
    });
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(10)),
        Ok(true)
    );
}

/// 同期の各変更操作の直後で切断し、回復した状態が壊れていないこと、同期し直すと目標になることを
/// すべての切断点で確かめる。`nested` なら回復の途中の切断も総当たりする
pub async fn crash_sweep(initial: &[Want<'_>], target: &[Want<'_>], nested: bool) {
    let base = device_at(initial, 1 << 30).await;
    base.set("手で置いた.mp3", b"keep");
    let clean = base.snapshot();
    sync_to(&clean, target, &TestControl::default())
        .await
        .unwrap();
    let total = clean.mutations();
    assert!(total > 0);
    for k in 0..total {
        let fs = base.snapshot();
        fs.fail_after(k);
        assert!(
            sync_to(&fs, target, &TestControl::default()).await.is_err(),
            "k={k}"
        );
        fs.reconnect();
        if nested {
            let probe = fs.snapshot();
            let _ = recover(&probe, &expect()).await.unwrap();
            for j in 0..probe.mutations() {
                let inner = fs.snapshot();
                inner.fail_after(j);
                assert!(recover(&inner, &expect()).await.is_err());
                inner.reconnect();
                check_recovered(&inner, initial, target, k).await;
                sync_to(&inner, target, &TestControl::default())
                    .await
                    .unwrap();
                check_target(&inner, target);
            }
        }
        check_recovered(&fs, initial, target, k).await;
        sync_to(&fs, target, &TestControl::default()).await.unwrap();
        check_target(&fs, target);
    }
}

async fn check_recovered(fs: &FakeFs, initial: &[Want<'_>], target: &[Want<'_>], k: usize) {
    let rec = recover(fs, &expect())
        .await
        .unwrap_or_else(|e| panic!("k={k}: {e}"));
    for it in &rec.manifest.items {
        let body = fs
            .get(&it.path)
            .unwrap_or_else(|| panic!("k={k}: {} が無い", it.path));
        assert_eq!(sha256_hex(&body), it.sha256, "k={k}: {}", it.path);
    }
    // 残す曲（初めにも目標にもある曲）を失わない
    for (id, _, _) in initial {
        if target.iter().any(|(t, _, _)| t == id) {
            assert!(
                rec.manifest.items.iter().any(|i| i.track_id == *id),
                "k={k}: {id} を失った"
            );
        }
    }
    assert_eq!(fs.get("手で置いた.mp3"), Some(b"keep".to_vec()), "k={k}");
}

fn check_target(fs: &FakeFs, target: &[Want<'_>]) {
    let m = spindle::device::ondevice::parse(&fs.get(MANIFEST_PATH).unwrap()).unwrap();
    let mut got: Vec<(i64, String)> = m
        .items
        .iter()
        .map(|i| (i.track_id, i.path.clone()))
        .collect();
    got.sort();
    let mut want: Vec<(i64, String)> = target
        .iter()
        .map(|(i, p, _)| (*i, (*p).to_owned()))
        .collect();
    want.sort();
    assert_eq!(got, want);
    for (_, p, b) in target {
        assert_eq!(fs.get(p).as_deref(), Some(*b), "{p}");
    }
    assert!(
        fs.paths()
            .iter()
            .all(|p| !p.ends_with(TMP_SUFFIX) && !p.starts_with(".spindle/moving/")),
        "{:?}",
        fs.paths()
    );
    assert_eq!(fs.get("手で置いた.mp3"), Some(b"keep".to_vec()));
    let _: Vec<Record> = parse(&fs.get(JOURNAL_PATH).unwrap()).unwrap();
}

fn pl(id: i64, name: &str, body: &[u8]) -> DesiredPlaylist {
    DesiredPlaylist {
        playlist_id: id,
        name: name.into(),
        dest_path: format!("Playlists/{name}.m3u8"),
        body: body.to_vec(),
        token: playlist_token(id, name, &sha256_hex(body)),
    }
}

#[test]
fn unmanaged_file_at_destination_is_not_overwritten() {
    block_on(async {
        let fs = device_at(&[], 1 << 30).await;
        fs.set("a.opus", b"user's");
        fs.set("Playlists/通勤.m3u8", b"user's list");
        let report = sync_full(
            &fs,
            &[(1, "a.opus", b"A")],
            &[pl(5, "通勤", b"#EXTM3U\n")],
            &[],
            &TestControl::default(),
        )
        .await
        .unwrap();
        assert_eq!(fs.get("a.opus"), Some(b"user's".to_vec()));
        assert_eq!(fs.get("Playlists/通勤.m3u8"), Some(b"user's list".to_vec()));
        assert_eq!(
            report.errors,
            vec![
                (EntryKind::Track, 1, UNMANAGED_COLLISION.to_owned()),
                (EntryKind::Playlist, 5, UNMANAGED_COLLISION.to_owned()),
            ]
        );
        assert!(report.manifest.items.is_empty());
        assert!(report.manifest.playlists.is_empty());
    });
}

fn playlist_paths(m: &DeviceManifest) -> Vec<(i64, String)> {
    let mut v: Vec<(i64, String)> = m
        .playlists
        .iter()
        .map(|p| (p.playlist_id, p.path.clone()))
        .collect();
    v.sort();
    v
}

#[test]
fn rename_onto_unmanaged_file_keeps_the_old_copy() {
    block_on(async {
        let fs = device_at(&[], 1 << 30).await;
        sync_full(
            &fs,
            &[],
            &[pl(5, "old", b"OLD")],
            &[],
            &TestControl::default(),
        )
        .await
        .unwrap();
        fs.set("Playlists/new.m3u8", b"user's list");
        let report = sync_full(
            &fs,
            &[],
            &[pl(5, "new", b"NEW")],
            &[],
            &TestControl::default(),
        )
        .await
        .unwrap();
        assert_eq!(fs.get("Playlists/old.m3u8"), Some(b"OLD".to_vec()));
        assert_eq!(fs.get("Playlists/new.m3u8"), Some(b"user's list".to_vec()));
        assert_eq!(
            report.errors,
            vec![(EntryKind::Playlist, 5, UNMANAGED_COLLISION.to_owned())]
        );
        assert_eq!(
            playlist_paths(&parse_manifest(&fs)),
            vec![(5, "Playlists/old.m3u8".to_owned())]
        );
    });
}

#[test]
fn rename_onto_a_playlist_that_stays_keeps_the_old_copy() {
    block_on(async {
        let fs = device_at(&[], 1 << 30).await;
        sync_full(
            &fs,
            &[],
            &[pl(5, "a", b"P5"), pl(6, "b", b"P6")],
            &[],
            &TestControl::default(),
        )
        .await
        .unwrap();
        // 6 は b → c だが c が手置きなので保留になり、b に居続ける。b へ来る 5 も保留になる
        fs.set("Playlists/c.m3u8", b"user's list");
        let report = sync_full(
            &fs,
            &[],
            &[pl(5, "b", b"P5"), pl(6, "c", b"P6")],
            &[],
            &TestControl::default(),
        )
        .await
        .unwrap();
        assert_eq!(fs.get("Playlists/a.m3u8"), Some(b"P5".to_vec()));
        assert_eq!(fs.get("Playlists/b.m3u8"), Some(b"P6".to_vec()));
        assert_eq!(fs.get("Playlists/c.m3u8"), Some(b"user's list".to_vec()));
        let mut errors = report.errors.clone();
        errors.sort();
        assert_eq!(
            errors,
            vec![
                (EntryKind::Playlist, 5, PATH_OCCUPIED.to_owned()),
                (EntryKind::Playlist, 6, UNMANAGED_COLLISION.to_owned()),
            ]
        );
        assert_eq!(
            playlist_paths(&parse_manifest(&fs)),
            vec![
                (5, "Playlists/a.m3u8".to_owned()),
                (6, "Playlists/b.m3u8".to_owned())
            ]
        );
    });
}

#[test]
fn playlists_can_swap_names() {
    block_on(async {
        let fs = device_at(&[], 1 << 30).await;
        sync_full(
            &fs,
            &[],
            &[pl(1, "x", b"P1"), pl(2, "y", b"P2")],
            &[],
            &TestControl::default(),
        )
        .await
        .unwrap();
        let report = sync_full(
            &fs,
            &[],
            &[pl(1, "y", b"P1"), pl(2, "x", b"P2")],
            &[],
            &TestControl::default(),
        )
        .await
        .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(fs.get("Playlists/y.m3u8"), Some(b"P1".to_vec()));
        assert_eq!(fs.get("Playlists/x.m3u8"), Some(b"P2".to_vec()));
        assert_eq!(
            playlist_paths(&parse_manifest(&fs)),
            vec![
                (1, "Playlists/y.m3u8".to_owned()),
                (2, "Playlists/x.m3u8".to_owned())
            ]
        );
    });
}

#[test]
fn rename_without_body_keeps_the_old_copy() {
    block_on(async {
        let fs = device_at(&[], 1 << 30).await;
        sync_full(
            &fs,
            &[],
            &[pl(5, "old", b"OLD")],
            &[],
            &TestControl::default(),
        )
        .await
        .unwrap();
        let rec = recover(&fs, &expect()).await.unwrap();
        let d = diff(
            &desired(&[]),
            &rec.items,
            &[pl(5, "new", b"NEW")],
            &rec.playlists,
            Vec::new(),
        );
        let plan = StoredPlan::from_diff(1, "tok", &d).unwrap();
        let r = runnable(&plan, &rec.items, &rec.playlists, &d);
        let report = sync::run(
            &fs,
            &sources(&[]),
            &TestControl::default(),
            SyncInput {
                generation: 1,
                start: rec.manifest,
                runnable: &r,
                playlist_bodies: &HashMap::new(),
                origins: &origins_from(&r),
            },
        )
        .await
        .unwrap();
        assert_eq!(fs.get("Playlists/old.m3u8"), Some(b"OLD".to_vec()));
        assert_eq!(fs.get("Playlists/new.m3u8"), None);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(
            playlist_paths(&parse_manifest(&fs)),
            vec![(5, "Playlists/old.m3u8".to_owned())]
        );
    });
}

fn is_compaction_write(c: &str) -> bool {
    c == "write .spindle/manifest.json.spindle-tmp"
}

#[test]
fn compacts_midway_and_survives_crashes() {
    block_on(async {
        let names: Vec<(i64, String, Vec<u8>)> = (1..=60)
            .map(|i| (i, format!("D/t{i}.opus"), format!("body{i}").into_bytes()))
            .collect();
        let target: Vec<Want> = names
            .iter()
            .map(|(i, p, b)| (*i, p.as_str(), b.as_slice()))
            .collect();
        let base = device_at(&[], 1 << 30).await;
        base.set("手で置いた.mp3", b"keep");
        let clean = base.snapshot();
        sync_to(&clean, &target, &TestControl::default())
            .await
            .unwrap();
        let calls = clean.calls();
        let writes = calls.iter().filter(|c| is_compaction_write(c)).count();
        assert!(writes >= 2, "途中の圧縮が無い: {writes}");
        let m = parse_manifest(&clean);
        assert_eq!(m.items.len(), 60);
        assert!(parse(&clean.get(JOURNAL_PATH).unwrap()).unwrap().is_empty());
        // 変更系の呼び出し番号（切断点）で、最初の圧縮の書き込みの位置
        let mutating = |c: &String| {
            ["write ", "append ", "put ", "rename ", "remove ", "prune"]
                .iter()
                .any(|p| c.starts_with(p))
        };
        let first = calls
            .iter()
            .filter(|c| mutating(c))
            .position(|c| is_compaction_write(c))
            .unwrap();
        let total = clean.mutations();
        // 全点だと 60 曲 × 2 回の同期で重いので、7 の倍数と最初の圧縮の前後 6 点に間引く
        let ks: Vec<usize> = (0..total)
            .filter(|k| k % 7 == 0 || (first.saturating_sub(6)..first + 6).contains(k))
            .collect();
        for k in ks {
            let fs = base.snapshot();
            fs.fail_after(k);
            assert!(
                sync_to(&fs, &target, &TestControl::default())
                    .await
                    .is_err(),
                "k={k}"
            );
            fs.reconnect();
            check_recovered(&fs, &[], &target, k).await;
            sync_to(&fs, &target, &TestControl::default())
                .await
                .unwrap();
            check_target(&fs, &target);
        }
    });
}

#[test]
fn playlist_changes_survive_every_crash_point() {
    block_on(async {
        let b = b"#EXTM3U\n../a.opus\n";
        let init_t: &[Want] = &[(1, "a.opus", b"A")];
        let target_t: &[Want] = &[(1, "a.opus", b"A"), (2, "b.opus", b"B")];
        let init_p = [pl(5, "通勤", b), pl(6, "消す", b)];
        let target_p = [pl(5, "朝", b), pl(7, "新", b"#EXTM3U\n")];
        let base = device_at(init_t, 1 << 30).await;
        sync_full(&base, init_t, &init_p, &[], &TestControl::default())
            .await
            .unwrap();
        base.set("手で置いた.mp3", b"keep");
        let clean = base.snapshot();
        sync_full(&clean, target_t, &target_p, &[], &TestControl::default())
            .await
            .unwrap();
        let total = clean.mutations();
        assert!(total > 0);
        for k in 0..total {
            let fs = base.snapshot();
            fs.fail_after(k);
            assert!(
                sync_full(&fs, target_t, &target_p, &[], &TestControl::default())
                    .await
                    .is_err(),
                "k={k}"
            );
            fs.reconnect();
            recover(&fs, &expect())
                .await
                .unwrap_or_else(|e| panic!("k={k}: {e}"));
            sync_full(&fs, target_t, &target_p, &[], &TestControl::default())
                .await
                .unwrap();
            let m = parse_manifest(&fs);
            let mut got: Vec<(i64, String)> = m
                .playlists
                .iter()
                .map(|p| (p.playlist_id, p.path.clone()))
                .collect();
            got.sort();
            assert_eq!(
                got,
                vec![
                    (5, "Playlists/朝.m3u8".to_owned()),
                    (7, "Playlists/新.m3u8".to_owned())
                ],
                "k={k}"
            );
            assert_eq!(fs.get("Playlists/朝.m3u8"), Some(b.to_vec()), "k={k}");
            assert_eq!(
                fs.get("Playlists/新.m3u8"),
                Some(b"#EXTM3U\n".to_vec()),
                "k={k}"
            );
            assert_eq!(fs.get("Playlists/通勤.m3u8"), None, "k={k}");
            assert_eq!(fs.get("Playlists/消す.m3u8"), None, "k={k}");
            check_target(&fs, target_t);
        }
    });
}

/// 回復 → 今の差分 → 保存済みの `plan` の部分集合 → 実行（計画を作り直さない再開）
async fn run_stored_plan(
    fs: &FakeFs,
    plan: &StoredPlan,
    playlists: &[DesiredPlaylist],
) -> Result<SyncReport, String> {
    let rec = recover(fs, &expect()).await.map_err(|e| e.to_string())?;
    let d = diff(
        &desired(&[]),
        &rec.items,
        playlists,
        &rec.playlists,
        Vec::new(),
    );
    let r = runnable(plan, &rec.items, &rec.playlists, &d);
    let bodies: HashMap<i64, Vec<u8>> = playlists
        .iter()
        .map(|p| (p.playlist_id, p.body.clone()))
        .collect();
    sync::run(
        fs,
        &sources(&[]),
        &TestControl::default(),
        SyncInput {
            generation: 1,
            start: rec.manifest,
            runnable: &r,
            playlist_bodies: &bodies,
            origins: &origins_from(&r),
        },
    )
    .await
    .map_err(|e: SyncError| e.to_string())
}

#[test]
fn playlist_rename_resumes_from_the_same_plan_at_every_crash_point() {
    block_on(async {
        let base = device_at(&[], 1 << 30).await;
        sync_full(
            &base,
            &[],
            &[pl(5, "通勤", b"P5"), pl(6, "消す", b"P6")],
            &[],
            &TestControl::default(),
        )
        .await
        .unwrap();
        let target = [pl(5, "朝", b"P5")];
        // 計画は 1 度だけ確定し、切断の後もそれを使って再開する
        let rec = recover(&base, &expect()).await.unwrap();
        let d = diff(
            &desired(&[]),
            &rec.items,
            &target,
            &rec.playlists,
            Vec::new(),
        );
        let plan = StoredPlan::from_diff(1, "tok", &d).unwrap();
        let clean = base.snapshot();
        run_stored_plan(&clean, &plan, &target).await.unwrap();
        let total = clean.mutations();
        assert!(total > 0);
        for k in 0..total {
            let fs = base.snapshot();
            fs.fail_after(k);
            assert!(run_stored_plan(&fs, &plan, &target).await.is_err(), "k={k}");
            fs.reconnect();
            let report = run_stored_plan(&fs, &plan, &target)
                .await
                .unwrap_or_else(|e| panic!("k={k}: {e}"));
            assert!(report.errors.is_empty(), "k={k}: {:?}", report.errors);
            assert_eq!(fs.get("Playlists/朝.m3u8"), Some(b"P5".to_vec()), "k={k}");
            assert_eq!(fs.get("Playlists/通勤.m3u8"), None, "k={k}");
            assert_eq!(fs.get("Playlists/消す.m3u8"), None, "k={k}");
            assert_eq!(
                playlist_paths(&parse_manifest(&fs)),
                vec![(5, "Playlists/朝.m3u8".to_owned())],
                "k={k}"
            );
        }
    });
}

#[test]
fn item_errors_do_not_break_crash_recovery() {
    block_on(async {
        let target: &[Want] = &[
            (1, "a.opus", b"A"),
            (2, "b.opus", b"B"),
            (3, "c.opus", b"C"),
        ];
        let base = device_at(&[], 1 << 30).await;
        base.set("手で置いた.mp3", b"keep");
        let clean = base.snapshot();
        let report = sync_full(&clean, target, &[], &[1], &TestControl::default())
            .await
            .unwrap();
        assert_eq!(
            report.errors,
            vec![(EntryKind::Track, 1, SourceError::Missing.reason())]
        );
        let total = clean.mutations();
        for k in 0..total {
            let fs = base.snapshot();
            fs.fail_after(k);
            assert!(
                sync_full(&fs, target, &[], &[1], &TestControl::default())
                    .await
                    .is_err(),
                "k={k}"
            );
            fs.reconnect();
            check_recovered(&fs, &[], target, k).await;
            let r = sync_full(&fs, target, &[], &[1], &TestControl::default())
                .await
                .unwrap();
            assert_eq!(
                r.errors,
                vec![(EntryKind::Track, 1, SourceError::Missing.reason())]
            );
            assert_eq!(
                manifest_ids(&fs),
                vec![(2, "b.opus".to_owned()), (3, "c.opus".to_owned())],
                "k={k}"
            );
            assert_eq!(fs.get("a.opus"), None);
            assert_eq!(fs.get("b.opus"), Some(b"B".to_vec()));
            assert_eq!(fs.get("c.opus"), Some(b"C".to_vec()));
        }
    });
}

#[test]
fn swap_moves_without_overwriting() {
    block_on(async {
        let fs = device_at(&[(1, "x.opus", b"X"), (2, "y.opus", b"Y")], 1 << 30).await;
        sync_to(
            &fs,
            &[(1, "y.opus", b"X"), (2, "x.opus", b"Y")],
            &TestControl::default(),
        )
        .await
        .unwrap();
        assert_eq!(fs.get("y.opus"), Some(b"X".to_vec()));
        assert_eq!(fs.get("x.opus"), Some(b"Y".to_vec()));
        assert!(fs
            .paths()
            .iter()
            .all(|p| !p.starts_with(".spindle/moving/")));
    });
}

#[test]
fn update_move_prepares_new_content_before_vacating() {
    block_on(async {
        let fs = device_at(&[(1, "a.opus", b"OLD")], 1 << 30).await;
        let before = fs.calls().len();
        sync_to(&fs, &[(1, "Dir/b.opus", b"NEW")], &TestControl::default())
            .await
            .unwrap();
        let calls = fs.calls()[before..].to_vec();
        let put_new = calls
            .iter()
            .position(|c| c.starts_with("put .spindle/moving/") && c.ends_with(".new"))
            .unwrap();
        let rm_old = calls.iter().position(|c| c == "remove a.opus").unwrap();
        assert!(put_new < rm_old, "{calls:?}");
        assert_eq!(fs.get("a.opus"), None);
        assert_eq!(fs.get("Dir/b.opus"), Some(b"NEW".to_vec()));
    });
}

#[test]
fn case_only_rename_on_a_case_insensitive_card() {
    block_on(async {
        let fs = FakeFs::new(1 << 30).case_insensitive();
        spindle::device::store::initialize(&fs, "u1", "emulated")
            .await
            .unwrap();
        sync_to(&fs, &[(1, "Dir/Song.opus", b"S")], &TestControl::default())
            .await
            .unwrap();
        sync_to(&fs, &[(1, "Dir/song.opus", b"S")], &TestControl::default())
            .await
            .unwrap();
        assert_eq!(
            fs.paths()
                .iter()
                .filter(|p| !p.starts_with(".spindle"))
                .cloned()
                .collect::<Vec<_>>(),
            vec!["Dir/song.opus".to_string()]
        );
        assert_eq!(fs.get("Dir/song.opus"), Some(b"S".to_vec()));
    });
}

#[test]
fn failed_prepare_shrinks_the_batch_by_component() {
    block_on(async {
        // 1 ↔ 2 の入れ替え（2 は更新 + 移動で、送る元が無い）と、無関係な移動 3
        let fs = device_at(
            &[
                (1, "x.opus", b"X"),
                (2, "y.opus", b"Y"),
                (3, "p.opus", b"P"),
            ],
            1 << 30,
        )
        .await;
        let want: &[Want] = &[
            (1, "y.opus", b"X"),
            (2, "x.opus", b"Y2"),
            (3, "q.opus", b"P"),
        ];
        let rec = recover(&fs, &expect()).await.unwrap();
        let d = diff(&desired(want), &rec.items, &[], &[], Vec::new());
        let plan = StoredPlan::from_diff(1, "tok", &d).unwrap();
        let r = runnable(&plan, &rec.items, &[], &d);
        let mut src = sources(want);
        src.0.remove(&2);
        let report = sync::run(
            &fs,
            &src,
            &TestControl::default(),
            SyncInput {
                generation: 1,
                start: rec.manifest,
                runnable: &r,
                playlist_bodies: &HashMap::new(),
                origins: &origins_from(&r),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            report.errors,
            vec![(EntryKind::Track, 2, SourceError::Missing.reason())]
        );
        // 入れ替えは両方とも動かない。3 は動く
        assert_eq!(fs.get("x.opus"), Some(b"X".to_vec()));
        assert_eq!(fs.get("y.opus"), Some(b"Y".to_vec()));
        assert_eq!(fs.get("q.opus"), Some(b"P".to_vec()));
        assert!(fs
            .paths()
            .iter()
            .all(|p| !p.starts_with(".spindle/moving/")));
    });
}

/// 送る元 `missing` を欠いた状態で `want` へ同期する（縮めたバッチの試験用）
async fn run_without_source(
    fs: &FakeFs,
    want: &[Want<'_>],
    missing: i64,
) -> Result<SyncReport, String> {
    let rec = recover(fs, &expect()).await.map_err(|e| e.to_string())?;
    let d = diff(&desired(want), &rec.items, &[], &[], Vec::new());
    let plan = StoredPlan::from_diff(1, "tok", &d).unwrap();
    let r = runnable(&plan, &rec.items, &[], &d);
    let mut src = sources(want);
    src.0.remove(&missing);
    sync::run(
        fs,
        &src,
        &TestControl::default(),
        SyncInput {
            generation: 1,
            start: rec.manifest,
            runnable: &r,
            playlist_bodies: &HashMap::new(),
            origins: &origins_from(&r),
        },
    )
    .await
    .map_err(|e| e.to_string())
}

#[test]
fn shrunk_batch_survives_every_crash_point() {
    block_on(async {
        // 1 ↔ 2 の入れ替え（2 の送る元が無いので成分ごと外れる）と、無関係な更新 + 移動 3。
        // 縮めた後の新バッチ（3 だけ）が途中で落ちても、回復で 3 を失わない
        let initial: &[Want] = &[
            (1, "x.opus", b"X"),
            (2, "y.opus", b"Y"),
            (3, "p.opus", b"P"),
        ];
        let want: &[Want] = &[
            (1, "y.opus", b"X"),
            (2, "x.opus", b"Y2"),
            (3, "q.opus", b"P2"),
        ];
        let base = device_at(initial, 1 << 30).await;
        let clean = base.snapshot();
        run_without_source(&clean, want, 2).await.unwrap();
        assert_eq!(clean.get("q.opus"), Some(b"P2".to_vec()));
        for k in 0..clean.mutations() {
            let fs = base.snapshot();
            fs.fail_after(k);
            assert!(run_without_source(&fs, want, 2).await.is_err(), "k={k}");
            fs.reconnect();
            let rec = recover(&fs, &expect())
                .await
                .unwrap_or_else(|e| panic!("k={k}: {e}"));
            for id in [1, 2, 3] {
                let it = rec.manifest.items.iter().find(|i| i.track_id == id);
                let it = it.unwrap_or_else(|| panic!("k={k}: {id} を失った"));
                assert!(fs.get(&it.path).is_some(), "k={k}: {} が無い", it.path);
            }
        }
    });
}

#[test]
fn cancel_after_vacating_completes_the_batch() {
    block_on(async {
        let fs = device_at(&[(1, "x.opus", b"X"), (2, "y.opus", b"Y")], 1 << 30).await;
        // guard の呼び出し: バッチの開始(1)・vacating の前(2)・追加の前(3)
        let c = TestControl {
            cancel_from: Some(3),
            ..Default::default()
        };
        let want: &[Want] = &[
            (1, "y.opus", b"X"),
            (2, "x.opus", b"Y"),
            (3, "z.opus", b"Z"),
        ];
        let err = sync_to(&fs, want, &c).await.unwrap_err();
        assert!(err.contains("キャンセル"), "{err}");
        assert_eq!(fs.get("y.opus"), Some(b"X".to_vec()));
        assert_eq!(fs.get("x.opus"), Some(b"Y".to_vec()));
        assert_eq!(fs.get("z.opus"), None);
        let rec = recover(&fs, &expect()).await.unwrap();
        let paths: Vec<(i64, String)> = rec
            .items
            .iter()
            .map(|i| (i.track_id, i.dest_path.clone()))
            .collect();
        assert_eq!(paths, vec![(1, "y.opus".into()), (2, "x.opus".into())]);
    });
}

#[test]
fn generation_change_before_vacating_aborts_nothing_placed() {
    block_on(async {
        let fs = device_at(&[(1, "x.opus", b"X"), (2, "y.opus", b"Y")], 1 << 30).await;
        let c = TestControl {
            stale_from: Some(2),
            ..Default::default()
        };
        let err = sync_to(&fs, &[(1, "y.opus", b"X"), (2, "x.opus", b"Y")], &c)
            .await
            .unwrap_err();
        assert!(err.contains("設定が変わった"), "{err}");
        let rec = recover(&fs, &expect()).await.unwrap();
        let paths: Vec<(i64, String)> = rec
            .items
            .iter()
            .map(|i| (i.track_id, i.dest_path.clone()))
            .collect();
        assert_eq!(paths, vec![(1, "x.opus".into()), (2, "y.opus".into())]);
    });
}

#[test]
fn swap_cycle_and_case_rename_survive_every_crash_point() {
    block_on(async {
        let initial: &[Want] = &[
            (1, "x.opus", b"X"),
            (2, "y.opus", b"Y"),
            (3, "p.opus", b"P"),
            (4, "q.opus", b"Q"),
            (5, "r.opus", b"R"),
            (6, "Dir/Song.opus", b"S"),
            (7, "del.opus", b"D"),
        ];
        let target: &[Want] = &[
            (1, "y.opus", b"X"),  // 入れ替え
            (2, "x.opus", b"Y2"), // 入れ替え + 更新
            (3, "q.opus", b"P"),  // 循環 p → q → r → p
            (4, "r.opus", b"Q2"),
            (5, "p.opus", b"R"),
            (6, "Dir/song.opus", b"S"), // 大小文字だけ
            (8, "new.opus", b"N"),      // 追加
        ];
        crash_sweep(initial, target, true).await;
    });
}

#[test]
fn path_change_onto_unmanaged_file_is_not_overwritten() {
    block_on(async {
        // 1 ↔ 2 の入れ替えと、手で置いた z.opus へ向かう無関係な移動 3
        let fs = device_at(
            &[
                (1, "x.opus", b"X"),
                (2, "y.opus", b"Y"),
                (3, "p.opus", b"P"),
            ],
            1 << 30,
        )
        .await;
        fs.set("z.opus", b"user's");
        let report = sync_to(
            &fs,
            &[
                (1, "y.opus", b"X"),
                (2, "x.opus", b"Y"),
                (3, "z.opus", b"P"),
            ],
            &TestControl::default(),
        )
        .await
        .unwrap();
        assert_eq!(fs.get("z.opus"), Some(b"user's".to_vec()));
        assert_eq!(fs.get("p.opus"), Some(b"P".to_vec()));
        assert_eq!(
            report.errors,
            vec![(EntryKind::Track, 3, UNMANAGED_COLLISION.to_owned())]
        );
        // 入れ替えは動く
        assert_eq!(fs.get("y.opus"), Some(b"X".to_vec()));
        assert_eq!(fs.get("x.opus"), Some(b"Y".to_vec()));
        assert!(fs
            .paths()
            .iter()
            .all(|p| !p.starts_with(".spindle/moving/")));
    });
}

/// 移動しなかった曲が空けるはずだったパスへの追加は、上書きせず保留になる（manifest のパスが重複しない）
async fn assert_held_then_converges(fs: &FakeFs, want: &[Want<'_>]) {
    assert_eq!(fs.get("a.opus"), Some(b"A".to_vec()));
    let m = parse_manifest(fs);
    assert_eq!(
        m.items
            .iter()
            .map(|i| (i.track_id, i.path.clone()))
            .collect::<Vec<_>>(),
        vec![(1, "a.opus".to_string())]
    );
    recover(fs, &expect()).await.unwrap();
    let report = sync_to(fs, want, &TestControl::default()).await.unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    check_target(fs, want);
}

#[test]
fn add_onto_a_move_source_blocked_by_an_unmanaged_file_is_held() {
    block_on(async {
        let fs = device_at(&[(1, "a.opus", b"A")], 1 << 30).await;
        fs.set("手で置いた.mp3", b"keep");
        fs.set("b.opus", b"user's");
        let want: &[Want] = &[(1, "b.opus", b"A"), (2, "a.opus", b"B")];
        let report = sync_to(&fs, want, &TestControl::default()).await.unwrap();
        assert_eq!(fs.get("b.opus"), Some(b"user's".to_vec()));
        assert_eq!(
            report.errors,
            vec![
                (EntryKind::Track, 1, UNMANAGED_COLLISION.to_owned()),
                (EntryKind::Track, 2, PATH_OCCUPIED.to_owned()),
            ]
        );
        // 管理外のファイルがどけば移動も追加もできる
        fs.remove_for_test("b.opus");
        assert_held_then_converges(&fs, want).await;
    });
}

#[test]
fn add_onto_a_move_source_dropped_by_a_failed_prepare_is_held() {
    block_on(async {
        let fs = device_at(&[(1, "a.opus", b"A")], 1 << 30).await;
        fs.set("手で置いた.mp3", b"keep");
        // 1 は更新 + 移動で送る元が無い（準備で失敗して成分ごと外れる）
        let want: &[Want] = &[(1, "b.opus", b"A2"), (2, "a.opus", b"B")];
        let report = sync_full(&fs, want, &[], &[1], &TestControl::default())
            .await
            .unwrap();
        assert_eq!(
            report.errors,
            vec![
                (EntryKind::Track, 1, SourceError::Missing.reason()),
                (EntryKind::Track, 2, PATH_OCCUPIED.to_owned()),
            ]
        );
        assert_held_then_converges(&fs, want).await;
    });
}

#[test]
fn add_onto_a_move_source_dropped_on_resume_is_held() {
    block_on(async {
        let fs = device_at(&[(1, "a.opus", b"A")], 1 << 30).await;
        fs.set("手で置いた.mp3", b"keep");
        // 計画を確定した後で曲 1 の中身（トークン）が変わった: 再開時に移動だけ Drop になる
        let planned: &[Want] = &[(1, "b.opus", b"A"), (2, "a.opus", b"B")];
        let want: &[Want] = &[(1, "b.opus", b"A2"), (2, "a.opus", b"B")];
        let rec = recover(&fs, &expect()).await.unwrap();
        let d0 = diff(&desired(planned), &rec.items, &[], &[], Vec::new());
        let plan = StoredPlan::from_diff(1, "tok", &d0).unwrap();
        let d = diff(&desired(want), &rec.items, &[], &[], Vec::new());
        let r = runnable(&plan, &rec.items, &[], &d);
        assert_eq!(r.dropped, 1);
        assert_eq!(r.items.len(), 1);
        let report = sync::run(
            &fs,
            &sources(want),
            &TestControl::default(),
            SyncInput {
                generation: 1,
                start: rec.manifest,
                runnable: &r,
                playlist_bodies: &HashMap::new(),
                origins: &origins_from(&r),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            report.errors,
            vec![(EntryKind::Track, 2, PATH_OCCUPIED.to_owned())]
        );
        assert_held_then_converges(&fs, want).await;
    });
}

#[test]
fn stale_token_from_recovery_survives_a_sync_that_does_not_touch_it() {
    block_on(async {
        let fs = device_at(&[(1, "a.opus", b"A"), (2, "b.opus", b"B")], 1 << 30).await;
        sync_full(
            &fs,
            &[(1, "a.opus", b"A"), (2, "b.opus", b"B")],
            &[pl(5, "朝", b"#EXTM3U\n")],
            &[],
            &TestControl::default(),
        )
        .await
        .unwrap();
        fs.remove_for_test("a.opus");
        fs.remove_for_test("Playlists/朝.m3u8");
        let rec = recover(&fs, &expect()).await.unwrap();
        // 曲 1 もプレイリストも今回の計画に入らない（hold）
        let report = sync::run(
            &fs,
            &MemSources::default(),
            &TestControl::default(),
            SyncInput {
                generation: 1,
                start: rec.manifest,
                runnable: &Runnable::default(),
                playlist_bodies: &HashMap::new(),
                origins: &origins_from(&Runnable::default()),
            },
        )
        .await
        .unwrap();
        let token = |items: &[DeviceItem], id: i64| {
            items
                .iter()
                .find(|i| i.track_id == id)
                .map(|i| i.token.clone())
        };
        assert_eq!(token(&report.items(), 1).as_deref(), Some(STALE_TOKEN));
        assert_ne!(token(&report.items(), 2).as_deref(), Some(STALE_TOKEN));
        assert_eq!(report.playlists()[0].token, STALE_TOKEN);
        let again = recover(&fs, &expect()).await.unwrap();
        assert_eq!(token(&again.items, 1).as_deref(), Some(STALE_TOKEN));
        assert_eq!(again.manifest.items[0].token, STALE_TOKEN);
        assert_eq!(again.playlists[0].token, STALE_TOKEN);
        assert_eq!(again.manifest.playlists[0].token, STALE_TOKEN);
    });
}

#[test]
fn sync_error_tells_a_disconnect() {
    use spindle::device::remote::RemoteError;
    use spindle::device::store::StoreError;
    assert!(SyncError::from(RemoteError::NotConnected).is_not_connected());
    assert!(!SyncError::from(RemoteError::Failed("x".into())).is_not_connected());
    assert!(!SyncError::Cancelled.is_not_connected());
    assert!(!SyncError::Store(StoreError::Manifest(ManifestError::TooMany(1))).is_not_connected());
}

/// `crash_sweep` の追記の部分書き版: 各切断点で、切断する操作が追記なら先頭だけ書いて切る。
/// 回復の後の同期し直しも各点で切り、回復 → 同期し直しで目標になること
async fn torn_append_sweep(initial: &[Want<'_>], target: &[Want<'_>]) {
    let base = device_at(initial, 1 << 30).await;
    base.set("手で置いた.mp3", b"keep");
    let clean = base.snapshot();
    sync_to(&clean, target, &TestControl::default())
        .await
        .unwrap();
    let total = clean.mutations();
    for k in 0..total {
        let fs = base.snapshot();
        fs.fail_append_partially_after(k, 7);
        assert!(
            sync_to(&fs, target, &TestControl::default()).await.is_err(),
            "k={k}"
        );
        fs.reconnect();
        let probe = fs.snapshot();
        sync_to(&probe, target, &TestControl::default())
            .await
            .unwrap_or_else(|e| panic!("k={k}: {e}"));
        check_target(&probe, target);
        for j in 0..probe.mutations() {
            let inner = fs.snapshot();
            inner.fail_after(j);
            let _ = sync_to(&inner, target, &TestControl::default()).await;
            inner.reconnect();
            check_recovered(&inner, initial, target, k * 1000 + j).await;
            sync_to(&inner, target, &TestControl::default())
                .await
                .unwrap_or_else(|e| panic!("k={k} j={j}: {e}"));
            check_target(&inner, target);
        }
    }
}

#[test]
fn torn_appends_survive_every_crash_point() {
    block_on(async {
        let initial: &[Want] = &[
            (1, "a.opus", b"A1"),
            (2, "b.opus", b"B1"),
            (3, "c.opus", b"C1"),
        ];
        let target: &[Want] = &[
            (1, "a.opus", b"A2"),
            (3, "c.opus", b"C1"),
            (4, "Dir/d.opus", b"D1"),
        ];
        torn_append_sweep(initial, target).await;
    });
}

#[test]
fn torn_appends_in_a_swap_survive_every_crash_point() {
    block_on(async {
        let initial: &[Want] = &[
            (1, "x.opus", b"X"),
            (2, "y.opus", b"Y"),
            (3, "p.opus", b"P"),
        ];
        let target: &[Want] = &[
            (1, "y.opus", b"X"),
            (2, "x.opus", b"Y2"),
            (4, "n.opus", b"N"),
        ];
        torn_append_sweep(initial, target).await;
    });
}

/// 送る元の identity が `source_hashes` と違った曲は、ハッシュを取り直す対象として返す
#[test]
fn changed_source_is_reported_for_rehash() {
    block_on(async {
        struct ChangedSources;
        impl spindle::device::sync::Sources for ChangedSources {
            fn open(&self, _: i64) -> Result<std::fs::File, spindle::device::sync::SourceError> {
                Err(spindle::device::sync::SourceError::Changed)
            }
        }
        let fs = FakeFs::new(1 << 30);
        spindle::device::store::initialize(&fs, "u1", "emulated")
            .await
            .unwrap();
        let rec = spindle::device::recover::recover(&fs, &expect())
            .await
            .unwrap();
        let want: &[Want] = &[(7, "a.opus", b"A"), (3, "b.opus", b"B")];
        let d = spindle::domain::device::diff(&desired(want), &rec.items, &[], &[], Vec::new());
        let plan = spindle::device::plan::StoredPlan::from_diff(1, "tok", &d).unwrap();
        let r = spindle::device::plan::runnable(&plan, &rec.items, &rec.playlists, &d);
        let report = spindle::device::sync::run(
            &fs,
            &ChangedSources,
            &TestControl::default(),
            spindle::device::sync::SyncInput {
                generation: 1,
                start: rec.manifest,
                runnable: &r,
                playlist_bodies: &std::collections::HashMap::new(),
                origins: &origins_from(&r),
            },
        )
        .await
        .unwrap();
        assert_eq!(report.rehash, vec![3, 7]);
        assert_eq!(report.errors.len(), 2);
    });
}

#[tokio::test]
async fn transferred_items_record_rel_path_and_source() {
    let fs = device_at(&[(1, "A/a.flac", b"AAAA")], 1 << 30).await;
    let m = store::read_manifest(&fs).await.unwrap().unwrap();
    assert_eq!(m.items[0].rel_path, "A/a.flac");
    assert_eq!(m.items[0].source, "master");
}

#[tokio::test]
async fn move_and_update_move_record_rel_path_and_source() {
    let fs = device_at(
        &[(1, "A/a.flac", b"AAAA"), (2, "B/b.flac", b"BBBB")],
        1 << 30,
    )
    .await;
    // 1 は移動だけ、2 は内容も変えて移動（更新 + 移動）
    let report = sync_to(
        &fs,
        &[(1, "C/a.flac", b"AAAA"), (2, "D/b.flac", b"BBB2")],
        &TestControl::default(),
    )
    .await
    .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let m = store::read_manifest(&fs).await.unwrap().unwrap();
    let by_id = |id| m.items.iter().find(|i| i.track_id == id).unwrap();
    assert_eq!(by_id(1).path, "C/a.flac");
    assert_eq!(by_id(1).rel_path, "C/a.flac");
    assert_eq!(by_id(1).source, "master");
    assert_eq!(by_id(2).path, "D/b.flac");
    assert_eq!(by_id(2).rel_path, "D/b.flac");
    assert_eq!(by_id(2).source, "master");
}

#[tokio::test]
async fn sync_refreshes_stale_rel_path_of_unchanged_items() {
    let fs = device_at(&[(1, "A/a.flac", b"AAAA")], 1 << 30).await;
    // 端末の rel_path を古い値にしておく（WAV → FLAC の正規化で Library 側だけ変わった状況）
    let mut m = store::read_manifest(&fs).await.unwrap().unwrap();
    m.items[0].rel_path = "A/a.wav".into();
    store::write_manifest(&fs, &m).await.unwrap();
    // 差分は無い（パス・トークン同じ）が、同期の終わりに今の値へ直る
    sync_to(&fs, &[(1, "A/a.flac", b"AAAA")], &TestControl::default())
        .await
        .unwrap();
    let m = store::read_manifest(&fs).await.unwrap().unwrap();
    assert_eq!(m.items[0].rel_path, "A/a.flac");
}

/// 端末の manifest の曲のトークンを引き継ぎの印にする（引き継ぎ直後の状態）
async fn mark_adopted(fs: &FakeFs) {
    let mut m = store::read_manifest(fs).await.unwrap().unwrap();
    for i in &mut m.items {
        i.token = ADOPTED_TOKEN.to_owned();
    }
    store::write_manifest(fs, &m).await.unwrap();
}

fn put_count(fs: &FakeFs) -> usize {
    fs.calls().iter().filter(|c| c.starts_with("put")).count()
}

#[tokio::test]
async fn adopted_with_same_content_is_not_transferred() {
    let want: &[Want] = &[(1, "A/a.flac", b"AAAA")];
    let fs = device_at(want, 1 << 30).await;
    mark_adopted(&fs).await;
    let before = put_count(&fs);
    let r = sync_to(&fs, want, &TestControl::default()).await.unwrap();
    assert_eq!(put_count(&fs), before);
    assert_eq!(r.manifest.items[0].token, desired(want).desired[0].token);
}

#[tokio::test]
async fn adopted_with_wrong_size_is_transferred() {
    let fs = device_at(&[(1, "A/a.flac", b"AAAA")], 1 << 30).await;
    mark_adopted(&fs).await;
    fs.set("A/a.flac", b"AAAAAA"); // 端末上の写しが変わった
    let want: &[Want] = &[(1, "A/a.flac", b"AAAA")];
    let r = sync_to(&fs, want, &TestControl::default()).await.unwrap();
    assert_eq!(fs.get("A/a.flac").unwrap(), b"AAAA");
    assert_eq!(r.manifest.items[0].token, desired(want).desired[0].token);
}

#[tokio::test]
async fn adopted_updates_do_not_count_toward_peak_bytes() {
    // 空きが転送量に足りなくても、全曲が引き継ぎの照合なら NoSpace にならない
    let want: &[Want] = &[(1, "A/a.flac", &[1u8; 4096]), (2, "A/b.flac", &[2u8; 4096])];
    let big = device_at(want, 1 << 30).await;
    mark_adopted(&big).await;
    let used: u64 = big
        .paths()
        .iter()
        .map(|p| big.get(p).unwrap().len() as u64)
        .sum();
    let small = FakeFs::with_root(used + sync::MARGIN_BYTES + 2048);
    for p in big.paths() {
        small.set(&p, &big.get(&p).unwrap());
    }
    sync_to(&small, want, &TestControl::default())
        .await
        .unwrap();
    assert_eq!(put_count(&small), 0);
}

/// 引き継いだ端末を、指定の送る元で `want` へ同期する
async fn sync_adopted_with<S: Sources>(fs: &FakeFs, want: &[Want<'_>], src: &S) -> SyncReport {
    let rec = recover(fs, &expect()).await.unwrap();
    let d = diff(&desired(want), &rec.items, &[], &[], Vec::new());
    let plan = StoredPlan::from_diff(1, "tok", &d).unwrap();
    let r = runnable(&plan, &rec.items, &rec.playlists, &d);
    sync::run(
        fs,
        src,
        &TestControl::default(),
        SyncInput {
            generation: 1,
            start: rec.manifest,
            runnable: &r,
            playlist_bodies: &HashMap::new(),
            origins: &origins_from(&r),
        },
    )
    .await
    .unwrap()
}

/// 計画の後に送る元が差し替わった引き継ぎの照合は、トークンを進めずハッシュを取り直させる
#[tokio::test]
async fn adopted_with_changed_source_keeps_the_mark_and_is_rehashed() {
    struct ChangedSources;
    impl Sources for ChangedSources {
        fn open(&self, _: i64) -> Result<std::fs::File, SourceError> {
            Err(SourceError::Changed)
        }
    }
    let want: &[Want] = &[(1, "A/a.flac", b"AAAA")];
    let fs = device_at(want, 1 << 30).await;
    mark_adopted(&fs).await;
    let r = sync_adopted_with(&fs, want, &ChangedSources).await;
    assert_eq!(r.manifest.items[0].token, ADOPTED_TOKEN);
    assert_eq!(r.rehash, vec![1]);
    assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
    let m = store::read_manifest(&fs).await.unwrap().unwrap();
    assert_eq!(m.items[0].token, ADOPTED_TOKEN);
}

/// 送る元が無い引き継ぎの照合は、トークンを進めず項目エラーにする
#[tokio::test]
async fn adopted_with_missing_source_keeps_the_mark() {
    let want: &[Want] = &[(1, "A/a.flac", b"AAAA")];
    let fs = device_at(want, 1 << 30).await;
    mark_adopted(&fs).await;
    let r = sync_adopted_with(&fs, want, &MemSources::default()).await;
    assert_eq!(r.manifest.items[0].token, ADOPTED_TOKEN);
    assert!(r.rehash.is_empty());
    assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
}

fn seen(c: &TestControl) -> Vec<SyncProgress> {
    c.seen.lock().unwrap().clone()
}

/// 件数もバイト数も減らず、最後に件数もバイト数も揃う
fn assert_finishes(s: &[SyncProgress]) {
    let last = s.last().expect("進捗が出ていない");
    assert!(s.windows(2).all(|w| w[0].done <= w[1].done), "{s:?}");
    assert_eq!(last.done, last.total, "{s:?}");
    assert_eq!(last.bytes_done, last.bytes_total, "{s:?}");
    assert!(
        s.windows(2).all(|w| w[0].bytes_done <= w[1].bytes_done),
        "{s:?}"
    );
    assert!(s.iter().all(|p| p.bytes_done <= p.bytes_total), "{s:?}");
    assert!(s.iter().all(|p| p.bytes_total == last.bytes_total), "{s:?}");
}

#[tokio::test]
async fn progress_counts_bytes_of_sent_tracks() {
    let fs = device_at(&[], 1 << 30).await;
    let want: &[Want] = &[(1, "A/a.opus", &[1u8; 100]), (2, "B/b.opus", &[2u8; 250])];
    let c = TestControl::default();
    sync_to(&fs, want, &c).await.unwrap();
    let s = seen(&c);
    assert_finishes(&s);
    assert_eq!(s.last().unwrap().bytes_total, 350);
    for name in ["a.opus", "b.opus"] {
        assert!(
            s.iter()
                .any(|p| p.phase == SyncPhase::Send && p.current.as_deref() == Some(name)),
            "{name}: {s:?}"
        );
    }
}

#[tokio::test]
async fn progress_does_not_count_adopted_updates() {
    let fs = device_at(&[(1, "A/a.flac", b"AAAA")], 1 << 30).await;
    mark_adopted(&fs).await;
    let c = TestControl::default();
    sync_to(&fs, &[(1, "A/a.flac", b"AAAA"), (2, "B/b.flac", b"BB")], &c)
        .await
        .unwrap();
    let s = seen(&c);
    assert_finishes(&s);
    assert_eq!(s.last().unwrap().bytes_total, 2);
    assert_eq!(s.last().unwrap().total, 2);
}

#[tokio::test]
async fn progress_counts_update_moves_in_the_batch() {
    let fs = device_at(&[(1, "a.opus", b"OLD"), (2, "x.opus", b"X")], 1 << 30).await;
    let c = TestControl::default();
    sync_to(&fs, &[(1, "Dir/b.opus", b"NEW!"), (2, "y.opus", b"X")], &c)
        .await
        .unwrap();
    let s = seen(&c);
    assert_finishes(&s);
    // 更新 + 移動は送る（4 バイト）。純粋な移動は送らない
    assert_eq!(s.last().unwrap().bytes_total, 4);
    // 送った曲の size 分（4）が、バッチを終える前に Move の段で数えられている（begin の 0 では満たされない）
    assert!(
        s.iter()
            .any(|p| p.phase == SyncPhase::Move && p.bytes_done == 4 && p.done < p.total),
        "{s:?}"
    );
}

#[tokio::test]
async fn progress_finishes_when_a_shrunk_batch_drops_a_track() {
    let fs = device_at(&[(1, "a.opus", b"OLD1"), (2, "b.opus", b"OLD2")], 1 << 30).await;
    let want: &[Want] = &[(1, "A/a.opus", b"NEW11"), (2, "B/b.opus", b"NEW222")];
    let c = TestControl::default();
    let r = sync_full(&fs, want, &[], &[1], &c).await.unwrap();
    // 1 を外してやり直した（報告に 1 件のエラーが残る）
    assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
    let s = seen(&c);
    assert_finishes(&s);
    assert_eq!(s.last().unwrap().bytes_total, 11);
}

#[tokio::test]
async fn progress_finishes_when_a_source_cannot_be_opened() {
    let fs = device_at(&[], 1 << 30).await;
    let want: &[Want] = &[(1, "a.opus", b"AAA"), (2, "b.opus", b"BBBBB")];
    let c = TestControl::default();
    let r = sync_full(&fs, want, &[], &[1], &c).await.unwrap();
    assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
    let s = seen(&c);
    assert_finishes(&s);
    assert_eq!(s.last().unwrap().bytes_total, 8);
}

#[tokio::test]
async fn progress_of_deletes_only_has_no_bytes() {
    let fs = device_at(&[(1, "A/a.opus", b"A")], 1 << 30).await;
    let c = TestControl::default();
    sync_to(&fs, &[], &c).await.unwrap();
    let s = seen(&c);
    assert_finishes(&s);
    assert_eq!(s.last().unwrap().bytes_total, 0);
    assert!(
        s.iter()
            .any(|p| p.phase == SyncPhase::Delete && p.current.as_deref() == Some("a.opus")),
        "{s:?}"
    );
}

#[tokio::test]
async fn progress_of_pure_moves_shows_each_track() {
    let fs = device_at(&[(1, "a.opus", b"A"), (2, "b.opus", b"B")], 1 << 30).await;
    let c = TestControl::default();
    sync_to(&fs, &[(1, "X/c.opus", b"A"), (2, "Y/d.opus", b"B")], &c)
        .await
        .unwrap();
    let s = seen(&c);
    assert_finishes(&s);
    assert_eq!(s.last().unwrap().total, 2);
    // 送り先のファイル名が Move の相で出る
    for name in ["c.opus", "d.opus"] {
        assert!(
            s.iter()
                .any(|p| p.phase == SyncPhase::Move && p.current.as_deref() == Some(name)),
            "{name}: {s:?}"
        );
    }
    // 置くたびに 1 件ずつ進む（バッチの終わりにまとめて増えるのではない）
    assert!(s.iter().any(|p| p.done == 1), "{s:?}");
}

#[tokio::test]
async fn progress_of_a_mixed_batch_shows_the_move_track() {
    let fs = device_at(&[(1, "a.opus", b"OLD"), (2, "x.opus", b"X")], 1 << 30).await;
    let c = TestControl::default();
    sync_to(&fs, &[(1, "Dir/b.opus", b"NEW!"), (2, "y.opus", b"X")], &c)
        .await
        .unwrap();
    let s = seen(&c);
    assert_finishes(&s);
    // 純粋な移動の曲の名前が出る（更新 + 移動の名前が残り続けない）
    assert!(
        s.iter()
            .any(|p| p.phase == SyncPhase::Move && p.current.as_deref() == Some("y.opus")),
        "{s:?}"
    );
}

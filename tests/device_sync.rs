//! 保存した計画の実行（仕様 ⑤「同期」）

mod device_support;

use std::collections::HashMap;
use std::sync::Arc;

use device_support::*;
use spindle::device::journal::{parse, Record};
use spindle::device::ondevice::*;
use spindle::device::plan::{runnable, StoredPlan};
use spindle::device::recover::recover;
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

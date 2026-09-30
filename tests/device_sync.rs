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

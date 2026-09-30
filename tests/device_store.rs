//! 耐久化の順序と登録時の初期化（仕様 ⑤「耐久化の順序」「登録」）

mod device_support;

use device_support::FakeFs;
use spindle::device::journal::Record;
use spindle::device::ondevice::*;
use spindle::device::store::*;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(f)
}

#[test]
fn initialize_writes_manifest_and_empty_journal() {
    block_on(async {
        let fs = FakeFs::new(1 << 30);
        let m = initialize(&fs, "u1", "emulated").await.unwrap();
        assert_eq!(read_manifest(&fs).await.unwrap(), Some(m));
        assert_eq!(fs.get(JOURNAL_PATH), Some(Vec::new()));
        assert!(fs.paths().iter().all(|p| !p.ends_with(TMP_SUFFIX)));
    });
}

#[test]
fn initialize_refuses_a_non_empty_root() {
    block_on(async {
        let fs = FakeFs::with_root(1 << 30);
        fs.set("someone.mp3", b"x");
        assert!(matches!(
            initialize(&fs, "u1", "emulated").await,
            Err(InitError::NotEmpty)
        ));
        assert_eq!(fs.get(MANIFEST_PATH), None);
    });
}

#[test]
fn write_file_durable_orders_tmp_sync_rename_sync() {
    block_on(async {
        let fs = FakeFs::with_root(1 << 30);
        write_file_durable(&fs, "Playlists/a.m3u8", b"#EXTM3U\n")
            .await
            .unwrap();
        assert_eq!(
            fs.calls(),
            vec![
                "write Playlists/a.m3u8.spindle-tmp",
                "sync",
                "rename Playlists/a.m3u8.spindle-tmp -> Playlists/a.m3u8",
                "sync"
            ]
        );
    });
}

#[test]
fn append_durable_syncs_after_append() {
    block_on(async {
        let fs = FakeFs::with_root(1 << 30);
        append_durable(&fs, &[Record::Done { op_id: "a".into() }])
            .await
            .unwrap();
        assert_eq!(fs.calls(), vec!["append .spindle/journal", "sync"]);
        assert_eq!(read_journal(&fs).await.unwrap().len(), 1);
    });
}

#[test]
fn compact_writes_manifest_before_truncating_journal() {
    block_on(async {
        let fs = FakeFs::with_root(1 << 30);
        append_durable(&fs, &[Record::Done { op_id: "a".into() }])
            .await
            .unwrap();
        let before = fs.calls().len();
        compact(&fs, &DeviceManifest::new("u1", "emulated"))
            .await
            .unwrap();
        let calls = fs.calls()[before..].to_vec();
        let rename = calls.iter().position(|c| c.starts_with("rename")).unwrap();
        let truncate = calls
            .iter()
            .position(|c| c == "write .spindle/journal")
            .unwrap();
        assert!(rename < truncate, "{calls:?}");
        assert!(read_journal(&fs).await.unwrap().is_empty());
    });
}

#[test]
fn corrupt_manifest_is_an_error() {
    block_on(async {
        let fs = FakeFs::with_root(1 << 30);
        fs.set(MANIFEST_PATH, b"{not json");
        assert!(matches!(
            read_manifest(&fs).await,
            Err(StoreError::Manifest(_))
        ));
    });
}

use spindle::device::remote::{DeviceFs, RemoteError};

#[test]
fn fail_after_injects_a_disconnect_at_the_operation_boundary() {
    block_on(async {
        let fs = FakeFs::with_root(1 << 30);
        fs.fail_after(1);
        fs.write("a", b"1").await.unwrap();
        assert_eq!(fs.write("b", b"2").await, Err(RemoteError::NotConnected));
        assert_eq!(fs.mutations(), 1);
        assert_eq!(fs.read("a").await, Err(RemoteError::NotConnected));
        assert_eq!(fs.sync().await, Err(RemoteError::NotConnected));
        fs.reconnect();
        assert_eq!(fs.read("a").await, Ok(Some(b"1".to_vec())));
        assert_eq!(fs.read("b").await, Ok(None));
        fs.write("b", b"2").await.unwrap();
    });
}

#[test]
fn snapshot_copies_state_and_resets_injection() {
    block_on(async {
        let fs = FakeFs::with_root(1 << 30);
        fs.write("a", b"1").await.unwrap();
        fs.rescan().await.unwrap();
        fs.fail_after(0);
        let snap = fs.snapshot();
        assert_eq!(snap.get("a"), Some(b"1".to_vec()));
        assert_eq!(snap.mutations(), 0);
        assert!(snap.calls().is_empty());
        assert_eq!(snap.rescans(), 0);
        snap.write("b", b"2").await.unwrap();
        assert_eq!(fs.get("b"), None);
        assert_eq!(fs.write("c", b"3").await, Err(RemoteError::NotConnected));
        assert_eq!(snap.get("c"), None);
    });
}

#[test]
fn compact_is_never_seen_as_old_manifest_with_empty_journal() {
    block_on(async {
        let mut old = DeviceManifest::new("u1", "emulated");
        old.items.push(ManifestItem {
            track_id: 1,
            path: "a.opus".into(),
            token: "t".into(),
            size: 1,
            sha256: "0".repeat(64),
        });
        let new = DeviceManifest::new("u1", "emulated");
        let base = FakeFs::with_root(1 << 30);
        write_manifest(&base, &old).await.unwrap();
        append_durable(&base, &[Record::Done { op_id: "a".into() }])
            .await
            .unwrap();

        let clean = base.snapshot();
        compact(&clean, &new).await.unwrap();
        let total = clean.mutations();
        assert!(total > 0);

        for k in 0..=total {
            let fs = base.snapshot();
            fs.fail_after(k);
            let _ = compact(&fs, &new).await;
            fs.reconnect();
            let m = read_manifest(&fs).await.unwrap().unwrap();
            let j = read_journal(&fs).await.unwrap();
            let is_new = m == new;
            assert!(is_new || m == old, "k={k}");
            let empty = j.is_empty();
            assert!(!(empty && !is_new), "旧 manifest と空のジャーナル (k={k})");
        }
    });
}

#[test]
fn case_insensitive_fs_folds_names_and_keeps_one_file_on_case_rename() {
    block_on(async {
        let fs = FakeFs::with_root(1 << 30).case_insensitive();
        fs.write("A.opus", b"x").await.unwrap();
        assert_eq!(fs.read("a.opus").await, Ok(Some(b"x".to_vec())));
        fs.rename("A.opus", "a.opus").await.unwrap();
        assert_eq!(fs.paths(), vec!["a.opus".to_string()]);
    });
}

#[test]
fn write_over_capacity_fails_with_no_space_and_writes_nothing() {
    block_on(async {
        let fs = FakeFs::with_root(3);
        assert_eq!(fs.write("a", b"1234").await, Err(RemoteError::NoSpace));
        assert!(fs.paths().is_empty());
    });
}

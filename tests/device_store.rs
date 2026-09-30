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

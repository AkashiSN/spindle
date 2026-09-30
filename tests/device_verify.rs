//! 内容の検証（仕様 ⑤「内容の検証」）

mod device_support;

use device_support::*;
use spindle::device::ondevice::MANIFEST_PATH;
use spindle::device::recover::{recover, STALE_TOKEN};
use spindle::device::verify::verify;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(f)
}

#[test]
fn same_size_corruption_is_found_and_dropped_from_manifest() {
    block_on(async {
        let fs = device_at(&[(1, "a.opus", b"AAAA"), (2, "b.opus", b"BBBB")], 1 << 30).await;
        fs.set("a.opus", b"AAAZ"); // 同じサイズの破損（日常の差分では見えない）
        let rec = recover(&fs, &expect()).await.unwrap();
        assert!(rec.items.iter().all(|i| !i.token.is_empty()));
        let report = verify(&fs, &TestControl::default(), rec.manifest)
            .await
            .unwrap();
        assert_eq!(report.mismatched, vec![1]);
        assert!(report.missing.is_empty());
        let m = spindle::device::ondevice::parse(&fs.get(MANIFEST_PATH).unwrap()).unwrap();
        assert_eq!(
            m.items.iter().map(|i| i.track_id).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(m.items[0].token, STALE_TOKEN);
        assert_ne!(m.items[1].token, STALE_TOKEN);
        assert_eq!(report.items().len(), 2);
        // ファイルは消さない
        assert_eq!(fs.get("a.opus"), Some(b"AAAZ".to_vec()));
        // 回復し直しても STALE のまま（サイズが合えば manifest の token をそのまま返す）
        let again = recover(&fs, &expect()).await.unwrap();
        assert_eq!(again.manifest.items[0].token, STALE_TOKEN);
        // 次の同期で直る
        sync_to(
            &fs,
            &[(1, "a.opus", b"AAAA"), (2, "b.opus", b"BBBB")],
            &TestControl::default(),
        )
        .await
        .unwrap();
        assert_eq!(fs.get("a.opus"), Some(b"AAAA".to_vec()));
        let m = spindle::device::ondevice::parse(&fs.get(MANIFEST_PATH).unwrap()).unwrap();
        assert_ne!(m.items[0].token, STALE_TOKEN);
    });
}

#[test]
fn missing_file_is_marked_stale_and_replaced_by_next_sync() {
    block_on(async {
        let fs = device_at(&[(1, "a.opus", b"AAAA"), (2, "b.opus", b"BBBB")], 1 << 30).await;
        fs.remove_for_test("b.opus");
        let rec = recover(&fs, &expect()).await.unwrap();
        let report = verify(&fs, &TestControl::default(), rec.manifest)
            .await
            .unwrap();
        assert_eq!(report.missing, vec![2]);
        assert!(report.mismatched.is_empty());
        let m = spindle::device::ondevice::parse(&fs.get(MANIFEST_PATH).unwrap()).unwrap();
        assert_eq!(m.items[1].token, STALE_TOKEN);
        sync_to(
            &fs,
            &[(1, "a.opus", b"AAAA"), (2, "b.opus", b"BBBB")],
            &TestControl::default(),
        )
        .await
        .unwrap();
        assert_eq!(fs.get("b.opus"), Some(b"BBBB".to_vec()));
        let m = spindle::device::ondevice::parse(&fs.get(MANIFEST_PATH).unwrap()).unwrap();
        assert_ne!(m.items[1].token, STALE_TOKEN);
    });
}

#[test]
fn clean_device_is_not_rewritten() {
    block_on(async {
        let fs = device_at(&[(1, "a.opus", b"A")], 1 << 30).await;
        let rec = recover(&fs, &expect()).await.unwrap();
        let before = fs.mutations();
        let report = verify(&fs, &TestControl::default(), rec.manifest)
            .await
            .unwrap();
        assert!(report.mismatched.is_empty() && report.missing.is_empty());
        assert_eq!(fs.mutations(), before);
    });
}

#[test]
fn verify_can_be_cancelled() {
    block_on(async {
        let fs = device_at(&[(1, "a.opus", b"A"), (2, "b.opus", b"B")], 1 << 30).await;
        let rec = recover(&fs, &expect()).await.unwrap();
        let c = TestControl {
            cancel_from: Some(2),
            ..Default::default()
        };
        assert!(verify(&fs, &c, rec.manifest).await.is_err());
    });
}

#[test]
fn verify_refuses_an_unrecovered_journal() {
    block_on(async {
        let fs = device_at(&[(1, "a.opus", b"AAAA")], 1 << 30).await;
        let rec = recover(&fs, &expect()).await.unwrap();
        fs.set(
            spindle::device::ondevice::JOURNAL_PATH,
            b"{\"t\":\"done\",\"op_id\":\"x\"}\n",
        );
        assert!(verify(&fs, &TestControl::default(), rec.manifest)
            .await
            .is_err());
        // 未完了の意図を圧縮で捨てない
        assert!(!fs
            .get(spindle::device::ondevice::JOURNAL_PATH)
            .unwrap()
            .is_empty());
    });
}

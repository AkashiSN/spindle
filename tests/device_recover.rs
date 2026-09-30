//! 接続のたびの回復（仕様 ⑤「ジャーナルと中断からの回復」の表）

mod device_support;

use device_support::FakeFs;
use spindle::device::journal::*;
use spindle::device::ondevice::*;
use spindle::device::recover::*;
use spindle::device::store::*;
use spindle::domain::device::{sha256_hex, EntryKind};

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(f)
}

fn expect() -> Expect {
    Expect {
        device_uuid: "u1".into(),
        volume: "emulated".into(),
    }
}

fn item(id: i64, path: &str, body: &[u8]) -> ManifestItem {
    ManifestItem {
        track_id: id,
        path: path.into(),
        token: format!("t-{}", sha256_hex(body)),
        size: body.len() as u64,
        sha256: sha256_hex(body),
    }
}

/// manifest に `items` を書き、実ファイルも置いた端末
async fn device_with(items: &[(i64, &str, &[u8])]) -> FakeFs {
    let fs = FakeFs::new(1 << 30);
    let mut m = initialize(&fs, "u1", "emulated").await.unwrap();
    for (id, path, body) in items {
        fs.set(path, body);
        m.items.push(item(*id, path, body));
    }
    write_manifest(&fs, &m).await.unwrap();
    fs
}

fn put_intent(op_id: &str, id: i64, to: &str, body: &[u8]) -> Intent {
    let it = item(id, to, body);
    Intent {
        op_id: op_id.into(),
        generation: 1,
        op: IntentOp::Put,
        kind: EntryKind::Track,
        ref_id: id,
        from: None,
        to: Some(to.into()),
        token: Some(it.token),
        size: Some(it.size),
        sha256: Some(it.sha256),
    }
}

fn rm_intent(op_id: &str, id: i64, from: &str) -> Intent {
    Intent {
        op_id: op_id.into(),
        generation: 1,
        op: IntentOp::Rm,
        kind: EntryKind::Track,
        ref_id: id,
        from: Some(from.into()),
        to: None,
        token: None,
        size: None,
        sha256: None,
    }
}

#[test]
fn clean_device_recovers_as_is() {
    block_on(async {
        let fs = device_with(&[(1, "a.opus", b"A")]).await;
        let r = recover(&fs, &expect()).await.unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].token, item(1, "a.opus", b"A").token);
        assert_eq!(r.unmanaged, 0);
    });
}

#[test]
fn identity_mismatch_stops() {
    block_on(async {
        let fs = device_with(&[]).await;
        let other = Expect {
            device_uuid: "u2".into(),
            volume: "emulated".into(),
        };
        assert!(matches!(
            recover(&fs, &other).await,
            Err(RecoverError::UuidMismatch)
        ));
        let vol = Expect {
            device_uuid: "u1".into(),
            volume: "1A2B-3C4D".into(),
        };
        assert!(matches!(
            recover(&fs, &vol).await,
            Err(RecoverError::VolumeMismatch)
        ));
        let empty = FakeFs::with_root(1 << 30);
        assert!(matches!(
            recover(&empty, &expect()).await,
            Err(RecoverError::NoManifest)
        ));
    });
}

#[test]
fn put_without_done_is_taken_when_sha_matches() {
    block_on(async {
        let fs = device_with(&[]).await;
        append_durable(&fs, &[Record::Intent(put_intent("o1", 1, "a.opus", b"A"))])
            .await
            .unwrap();
        fs.set("a.opus", b"A");
        let r = recover(&fs, &expect()).await.unwrap();
        assert_eq!(
            r.items.iter().map(|i| i.track_id).collect::<Vec<_>>(),
            vec![1]
        );
        // 圧縮済み
        assert!(read_journal(&fs).await.unwrap().is_empty());
        assert_eq!(read_manifest(&fs).await.unwrap().unwrap().items.len(), 1);
    });
}

#[test]
fn put_without_done_and_wrong_content_is_not_taken_and_tmp_is_removed() {
    block_on(async {
        let fs = device_with(&[(1, "a.opus", b"OLD")]).await;
        append_durable(
            &fs,
            &[Record::Intent(put_intent("o1", 1, "a.opus", b"NEW"))],
        )
        .await
        .unwrap();
        fs.set("a.opus.spindle-tmp", b"NE");
        let r = recover(&fs, &expect()).await.unwrap();
        assert_eq!(r.items[0].sha256, sha256_hex(b"OLD"));
        assert_eq!(fs.get("a.opus.spindle-tmp"), None);
    });
}

#[test]
fn rm_without_done_is_redone() {
    block_on(async {
        let fs = device_with(&[(1, "a.opus", b"A")]).await;
        append_durable(&fs, &[Record::Intent(rm_intent("o1", 1, "a.opus"))])
            .await
            .unwrap();
        let r = recover(&fs, &expect()).await.unwrap();
        assert!(r.items.is_empty());
        assert_eq!(fs.get("a.opus"), None);
    });
}

#[test]
fn done_intents_are_applied_even_before_compaction() {
    block_on(async {
        let fs = device_with(&[(1, "a.opus", b"A")]).await;
        fs.set("b.opus", b"B");
        fs.remove_for_test("a.opus");
        append_durable(
            &fs,
            &[
                Record::Intent(rm_intent("o1", 1, "a.opus")),
                Record::Intent(put_intent("o2", 2, "b.opus", b"B")),
                Record::Done { op_id: "o1".into() },
                Record::Done { op_id: "o2".into() },
            ],
        )
        .await
        .unwrap();
        let r = recover(&fs, &expect()).await.unwrap();
        assert_eq!(
            r.items.iter().map(|i| i.track_id).collect::<Vec<_>>(),
            vec![2]
        );
    });
}

#[test]
fn missing_or_resized_files_get_a_stale_token() {
    block_on(async {
        let fs = device_with(&[(1, "a.opus", b"A"), (2, "b.opus", b"B")]).await;
        fs.remove_for_test("a.opus");
        fs.set("b.opus", b"BBBB");
        let r = recover(&fs, &expect()).await.unwrap();
        assert!(r.items.iter().all(|i| i.token == STALE_TOKEN));
        // manifest からは外さない（管理外にしない）
        assert_eq!(r.manifest.items.len(), 2);
    });
}

#[test]
fn unmanaged_files_are_kept_and_counted() {
    block_on(async {
        let fs = device_with(&[(1, "a.opus", b"A")]).await;
        fs.set("手で置いた.mp3", b"x");
        fs.set("Sub/other.flac", b"y");
        fs.set("x.opus.spindle-tmp", b"z");
        let r = recover(&fs, &expect()).await.unwrap();
        assert_eq!(r.unmanaged, 2);
        assert_eq!(fs.get("手で置いた.mp3"), Some(b"x".to_vec()));
        assert_eq!(fs.get("Sub/other.flac"), Some(b"y".to_vec()));
        assert_eq!(fs.get("x.opus.spindle-tmp"), None);
    });
}

#[test]
fn moving_leftover_is_restored_when_it_matches_a_missing_item() {
    block_on(async {
        let fs = device_with(&[(1, "a.opus", b"A")]).await;
        fs.remove_for_test("a.opus");
        fs.set(".spindle/moving/zzz", b"A");
        let r = recover(&fs, &expect()).await.unwrap();
        assert_eq!(fs.get("a.opus"), Some(b"A".to_vec()));
        assert_eq!(fs.get(".spindle/moving/zzz"), None);
        assert_eq!(r.items[0].token, item(1, "a.opus", b"A").token);
    });
}

#[test]
fn moving_leftover_without_match_is_kept() {
    block_on(async {
        let fs = device_with(&[(1, "a.opus", b"A")]).await;
        fs.set(".spindle/moving/zzz", b"???");
        let r = recover(&fs, &expect()).await.unwrap();
        assert_eq!(fs.get(".spindle/moving/zzz"), Some(b"???".to_vec()));
        assert_eq!(r.unmanaged, 1);
    });
}

fn mv(batch: &str, op_id: &str, id: i64, from: &str, to: &str, body: &[u8]) -> BatchMember {
    let it = item(id, to, body);
    BatchMember {
        batch_id: batch.into(),
        op_id: op_id.into(),
        op: MemberOp::Move,
        track_id: id,
        from: from.into(),
        staging: format!("{MOVING_DIR}/{op_id}"),
        new: None,
        to: to.into(),
        token: it.token,
        size: it.size,
        sha256: it.sha256,
    }
}

fn seal(batch: &str, members: &[BatchMember]) -> Vec<Record> {
    let mut v = vec![Record::BatchBegin {
        batch_id: batch.into(),
    }];
    v.extend(members.iter().cloned().map(Record::BatchMember));
    v.push(Record::BatchSealed {
        batch_id: batch.into(),
        count: members.len(),
        digest: member_digest(members).unwrap(),
    });
    v
}

#[test]
fn unsealed_batch_is_ignored() {
    block_on(async {
        let fs = device_with(&[(1, "x.opus", b"X"), (2, "y.opus", b"Y")]).await;
        let ms = [
            mv("b", "m1", 1, "x.opus", "y.opus", b"X"),
            mv("b", "m2", 2, "y.opus", "x.opus", b"Y"),
        ];
        let mut recs = seal("b", &ms);
        recs.pop(); // sealed が耐久化される前に落ちた
        append_durable(&fs, &recs).await.unwrap();
        let r = recover(&fs, &expect()).await.unwrap();
        assert_eq!(fs.get("x.opus"), Some(b"X".to_vec()));
        assert_eq!(
            r.items.iter().find(|i| i.track_id == 1).unwrap().dest_path,
            "x.opus"
        );
    });
}

#[test]
fn swap_interrupted_while_vacating_is_completed() {
    block_on(async {
        let fs = device_with(&[(1, "x.opus", b"X"), (2, "y.opus", b"Y")]).await;
        let ms = [
            mv("b", "m1", 1, "x.opus", "y.opus", b"X"),
            mv("b", "m2", 2, "y.opus", "x.opus", b"Y"),
        ];
        let mut recs = seal("b", &ms);
        recs.push(Record::BatchPhase {
            batch_id: "b".into(),
            phase: Phase::Prepared,
        });
        recs.push(Record::BatchPhase {
            batch_id: "b".into(),
            phase: Phase::Vacating,
        });
        append_durable(&fs, &recs).await.unwrap();
        // 1 件目だけ空けたところで落ちた
        fs.rename_for_test("x.opus", ".spindle/moving/m1");
        let r = recover(&fs, &expect()).await.unwrap();
        assert_eq!(fs.get("y.opus"), Some(b"X".to_vec()));
        assert_eq!(fs.get("x.opus"), Some(b"Y".to_vec()));
        assert!(fs
            .paths()
            .iter()
            .all(|p| !p.starts_with(".spindle/moving/")));
        let paths: Vec<(i64, String)> = r
            .items
            .iter()
            .map(|i| (i.track_id, i.dest_path.clone()))
            .collect();
        assert_eq!(paths, vec![(1, "y.opus".into()), (2, "x.opus".into())]);
        assert!(read_journal(&fs).await.unwrap().is_empty());
    });
}

#[test]
fn prepared_batch_is_aborted_before_new_files_are_removed() {
    block_on(async {
        let fs = device_with(&[(1, "x.opus", b"X")]).await;
        let mut m = mv("b", "m1", 1, "x.opus", "z.opus", b"X2");
        m.op = MemberOp::UpdateMove;
        m.new = Some(format!("{MOVING_DIR}/m1.new"));
        let mut recs = seal("b", std::slice::from_ref(&m));
        recs.push(Record::BatchPhase {
            batch_id: "b".into(),
            phase: Phase::Prepared,
        });
        append_durable(&fs, &recs).await.unwrap();
        fs.set(".spindle/moving/m1.new", b"X2");
        let before = fs.calls().len();
        let r = recover(&fs, &expect()).await.unwrap();
        let calls = fs.calls()[before..].to_vec();
        let abort = calls
            .iter()
            .position(|c| c == "append .spindle/journal")
            .unwrap();
        let rm_new = calls
            .iter()
            .position(|c| c == "remove .spindle/moving/m1.new")
            .unwrap();
        assert!(abort < rm_new, "{calls:?}");
        assert_eq!(fs.get("x.opus"), Some(b"X".to_vec()));
        assert_eq!(r.items[0].dest_path, "x.opus");
    });
}

#[test]
fn aborted_batch_is_never_advanced() {
    block_on(async {
        let fs = device_with(&[(1, "x.opus", b"X")]).await;
        let ms = [mv("b", "m1", 1, "x.opus", "z.opus", b"X")];
        let mut recs = seal("b", &ms);
        recs.push(Record::BatchPhase {
            batch_id: "b".into(),
            phase: Phase::Prepared,
        });
        recs.push(Record::BatchAbort {
            batch_id: "b".into(),
            superseded_by: None,
        });
        append_durable(&fs, &recs).await.unwrap();
        recover(&fs, &expect()).await.unwrap();
        assert_eq!(fs.get("x.opus"), Some(b"X".to_vec()));
        assert_eq!(fs.get("z.opus"), None);
    });
}

#[test]
fn update_move_in_placing_is_finished() {
    block_on(async {
        let fs = device_with(&[(1, "x.opus", b"OLD")]).await;
        let mut m = mv("b", "m1", 1, "x.opus", "z.opus", b"NEW");
        m.op = MemberOp::UpdateMove;
        m.new = Some(format!("{MOVING_DIR}/m1.new"));
        let mut recs = seal("b", std::slice::from_ref(&m));
        for p in [
            Phase::Prepared,
            Phase::Vacating,
            Phase::Vacated,
            Phase::Placing,
        ] {
            recs.push(Record::BatchPhase {
                batch_id: "b".into(),
                phase: p,
            });
        }
        append_durable(&fs, &recs).await.unwrap();
        fs.remove_for_test("x.opus");
        fs.set(".spindle/moving/m1.new", b"NEW");
        let r = recover(&fs, &expect()).await.unwrap();
        assert_eq!(fs.get("z.opus"), Some(b"NEW".to_vec()));
        assert_eq!(fs.get("x.opus"), None);
        assert_eq!(r.items[0].dest_path, "z.opus");
        assert_eq!(r.items[0].sha256, sha256_hex(b"NEW"));
    });
}

#[test]
fn corrupt_journal_stops() {
    block_on(async {
        let fs = device_with(&[]).await;
        fs.set(JOURNAL_PATH, b"garbage\n{\"t\":\"done\",\"op_id\":\"a\"}\n");
        assert!(matches!(
            recover(&fs, &expect()).await,
            Err(RecoverError::Store(StoreError::Journal(_)))
        ));
    });
}

fn swap_records(upto: &[Phase]) -> Vec<Record> {
    let ms = [
        mv("b", "m1", 1, "x.opus", "y.opus", b"X"),
        mv("b", "m2", 2, "y.opus", "x.opus", b"Y"),
    ];
    let mut recs = seal("b", &ms);
    for p in upto {
        recs.push(Record::BatchPhase {
            batch_id: "b".into(),
            phase: *p,
        });
    }
    recs
}

#[test]
fn position_is_judged_by_presence_not_content() {
    block_on(async {
        // x.opus は同サイズで外部から書き換えられている
        let fs = device_with(&[(1, "x.opus", b"X"), (2, "y.opus", b"Y")]).await;
        append_durable(&fs, &swap_records(&[Phase::Prepared, Phase::Vacating]))
            .await
            .unwrap();
        fs.set("x.opus", b"Z");
        recover(&fs, &expect()).await.unwrap();
        assert_eq!(fs.get("y.opus"), Some(b"Z".to_vec()));
        assert_eq!(fs.get("x.opus"), Some(b"Y".to_vec()));
        assert!(fs
            .paths()
            .iter()
            .all(|p| !p.starts_with(".spindle/moving/")));
    });
}

#[test]
fn move_resumes_from_vacated_and_placing() {
    block_on(async {
        for phases in [
            vec![Phase::Prepared, Phase::Vacating, Phase::Vacated],
            vec![
                Phase::Prepared,
                Phase::Vacating,
                Phase::Vacated,
                Phase::Placing,
            ],
        ] {
            let fs = device_with(&[(1, "x.opus", b"X"), (2, "y.opus", b"Y")]).await;
            append_durable(&fs, &swap_records(&phases)).await.unwrap();
            fs.rename_for_test("x.opus", ".spindle/moving/m1");
            fs.rename_for_test("y.opus", ".spindle/moving/m2");
            let r = recover(&fs, &expect()).await.unwrap();
            assert_eq!(fs.get("y.opus"), Some(b"X".to_vec()));
            assert_eq!(fs.get("x.opus"), Some(b"Y".to_vec()));
            assert_eq!(
                r.items.iter().find(|i| i.track_id == 1).unwrap().dest_path,
                "y.opus"
            );
        }
    });
}

#[test]
fn done_batch_is_taken_into_the_book() {
    block_on(async {
        let fs = device_with(&[(1, "x.opus", b"X"), (2, "y.opus", b"Y")]).await;
        append_durable(
            &fs,
            &swap_records(&[
                Phase::Prepared,
                Phase::Vacating,
                Phase::Vacated,
                Phase::Placing,
                Phase::Done,
            ]),
        )
        .await
        .unwrap();
        fs.set("y.opus", b"X");
        fs.set("x.opus", b"Y");
        let r = recover(&fs, &expect()).await.unwrap();
        let paths: Vec<(i64, String)> = r
            .items
            .iter()
            .map(|i| (i.track_id, i.dest_path.clone()))
            .collect();
        assert_eq!(paths, vec![(1, "y.opus".into()), (2, "x.opus".into())]);
        assert!(r.items.iter().all(|i| i.token != STALE_TOKEN));
    });
}

async fn interrupted_swap() -> FakeFs {
    let fs = device_with(&[(1, "x.opus", b"X"), (2, "y.opus", b"Y")]).await;
    append_durable(&fs, &swap_records(&[Phase::Prepared, Phase::Vacating]))
        .await
        .unwrap();
    fs.rename_for_test("x.opus", ".spindle/moving/m1");
    fs
}

type Outcome = (Vec<(i64, String)>, Option<Vec<u8>>, Option<Vec<u8>>);

fn outcome(fs: &FakeFs, r: &Recovered) -> Outcome {
    (
        r.items
            .iter()
            .map(|i| (i.track_id, i.dest_path.clone()))
            .collect(),
        fs.get("x.opus"),
        fs.get("y.opus"),
    )
}

#[test]
fn recovery_is_idempotent_under_disconnects() {
    block_on(async {
        let clean = interrupted_swap().await;
        clean.fail_after(usize::MAX);
        let r = recover(&clean, &expect()).await.unwrap();
        let want = outcome(&clean, &r);
        let n = clean.mutations();
        assert!(n > 0);
        for j in 0..=n {
            let fs = interrupted_swap().await;
            fs.fail_after(j);
            let _ = recover(&fs, &expect()).await;
            fs.reconnect();
            let r = recover(&fs, &expect()).await.unwrap();
            assert_eq!(outcome(&fs, &r), want, "j={j}");
        }
    });
}

#[test]
fn playlist_put_without_done_is_taken_when_sha_matches() {
    block_on(async {
        let fs = device_with(&[]).await;
        let body = b"#EXTM3U\n";
        let it = item(7, "p.m3u8", body);
        let intent = Intent {
            op_id: "o1".into(),
            generation: 1,
            op: IntentOp::Put,
            kind: EntryKind::Playlist,
            ref_id: 7,
            from: None,
            to: Some("p.m3u8".into()),
            token: Some(it.token.clone()),
            size: Some(it.size),
            sha256: Some(it.sha256),
        };
        append_durable(&fs, &[Record::Intent(intent)])
            .await
            .unwrap();
        fs.set("p.m3u8", body);
        let r = recover(&fs, &expect()).await.unwrap();
        assert_eq!(r.playlists.len(), 1);
        assert_eq!(r.playlists[0].playlist_id, 7);
        assert_eq!(r.playlists[0].token, it.token);
    });
}

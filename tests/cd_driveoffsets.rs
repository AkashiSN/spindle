//! AccurateRip のドライブ別オフセット表（`cd::driveoffsets`、`cd::rip::choose_offset`。D-83 追記）

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::routing::get;
use axum::Router;

use spindle::cd::driveoffsets::{lookup, normalize, parse, DriveOffsetTable, DriveTableError};
use spindle::cd::rip::choose_offset;
use spindle::cd::riplog::OffsetSource;
use spindle::config::DriveOffset;

/// 表の 1 レコード（69 バイト。実物の `DriveOffsets.bin` と同じ並び）
fn record(offset: i16, name: &str, submissions: u32) -> Vec<u8> {
    let mut r = offset.to_le_bytes().to_vec();
    let mut n = name.as_bytes().to_vec();
    n.resize(33, 0);
    r.extend(n);
    r.extend(submissions.to_le_bytes());
    r.resize(69, 0);
    r
}

fn table_bytes() -> Vec<u8> {
    [
        record(91, "- 16X12 DVD DUAL", 10),
        // 実物の BDR-209M の項（+667、提出 255）
        record(667, "PIONEER  - BD-RW   BDR-209M", 255),
        record(667, "PIONEER  - BD-RW   BDR-209MIO", 3),
        // 同じ型番の少数派
        record(6, "PIONEER  - BD-RW   BDR-209M", 2),
        record(-30, "HL-DT-ST - DVDRAM GH24NSD1", 40),
    ]
    .concat()
}

#[test]
fn parse_reads_records_and_rejects_bad_length() {
    let entries = parse(&table_bytes()).unwrap();
    assert_eq!(entries.len(), 5);
    assert_eq!(entries[1].name, "PIONEER  - BD-RW   BDR-209M");
    assert_eq!((entries[1].offset, entries[1].submissions), (667, 255));
    assert_eq!(entries[4].offset, -30);
    assert_eq!(parse(&[0u8; 70]), Err(DriveTableError::Length(70)));
    // 空（HTTP 200 + 空 body、空の保存）は表ではない
    assert_eq!(parse(&[]), Err(DriveTableError::Empty));
    // 69 バイト境界でも中身が違う: 名前が空・NUL で終わらない・予約が 0 でない
    assert!(matches!(
        parse(&[0u8; 69]),
        Err(DriveTableError::Record { index: 0, .. })
    ));
    let mut long = record(1, "A", 1);
    long[2..35].fill(b'A');
    assert!(matches!(parse(&long), Err(DriveTableError::Record { .. })));
    let mut junk = table_bytes();
    let last = junk.len() - 1;
    junk[last] = 1;
    assert!(matches!(
        parse(&junk),
        Err(DriveTableError::Record { index: 4, .. })
    ));
}

/// INQUIRY の型番（vendor と product を空白 1 つで結合）と表の名前（`"vendor  - product"`）を同じ鍵にする
#[test]
fn model_from_inquiry_matches_table_names() {
    assert_eq!(
        normalize("PIONEER BD-RW   BDR-209M"),
        normalize("PIONEER  - BD-RW   BDR-209M")
    );
    assert_eq!(normalize("16X12 DVD DUAL"), normalize("- 16X12 DVD DUAL"));
    let entries = parse(&table_bytes()).unwrap();
    // 同じ型番は提出数の多い方。前方一致（BDR-209MIO）は別の型番
    let e = lookup(&entries, "PIONEER BD-RW   BDR-209M").unwrap();
    assert_eq!((e.offset, e.submissions), (667, 255));
    assert_eq!(
        lookup(&entries, "pioneer bd-rw bdr-209m").unwrap().offset,
        667
    );
    assert_eq!(lookup(&entries, "16X12 DVD DUAL").unwrap().offset, 91);
    assert!(lookup(&entries, "PIONEER BD-RW BDR-209").is_none());
}

#[test]
fn choose_offset_prefers_manual_then_learned_then_table() {
    assert_eq!(
        choose_offset(DriveOffset::Samples(12), Some(667), Some(6)),
        (12, OffsetSource::Manual)
    );
    assert_eq!(
        choose_offset(DriveOffset::Auto, Some(667), Some(6)),
        (667, OffsetSource::Learned)
    );
    assert_eq!(
        choose_offset(DriveOffset::Auto, None, Some(667)),
        (667, OffsetSource::Table)
    );
    assert_eq!(
        choose_offset(DriveOffset::Auto, None, None),
        (0, OffsetSource::Unknown)
    );
    // 探索範囲の外（壊れた DB / 表）は使わない
    assert_eq!(
        choose_offset(DriveOffset::Auto, Some(5000), Some(-4000)),
        (0, OffsetSource::Unknown)
    );
    assert_eq!(
        choose_offset(DriveOffset::Auto, Some(5000), Some(667)),
        (667, OffsetSource::Table)
    );
}

/// 取ったら data に保存し、次からは保存したものを使う（取れなくても保存済みで引ける）
#[tokio::test]
async fn table_is_fetched_once_saved_and_reused() {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = Arc::clone(&hits);
    let bytes = table_bytes();
    let app = Router::new().route(
        "/accuraterip/DriveOffsets.bin",
        get(move || {
            let (h, bytes) = (Arc::clone(&h), bytes.clone());
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                bytes
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cd").join("DriveOffsets.bin");
    let base = format!("http://{addr}/accuraterip/");

    let t = DriveOffsetTable::new(&base, "spindle-test/0", cache.clone()).unwrap();
    // 読み込む前は peek では引けない（status はネットワークを待たない）
    assert!(t.peek("PIONEER BD-RW   BDR-209M").is_none());
    assert_eq!(
        t.lookup("PIONEER BD-RW   BDR-209M").await.unwrap().offset,
        667
    );
    assert_eq!(t.peek("PIONEER BD-RW   BDR-209M").unwrap().offset, 667);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(&cache).unwrap(), table_bytes());
    // 2 回目以降はメモリ
    t.lookup("16X12 DVD DUAL").await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // 別のプロセス（再起動）: 保存が新しければ取りに行かない
    let t = DriveOffsetTable::new(&base, "spindle-test/0", cache.clone()).unwrap();
    assert_eq!(
        t.lookup("PIONEER BD-RW   BDR-209M").await.unwrap().offset,
        667
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // 保存が古く、取りに行けない: 古い保存で引く
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 86_400);
    std::fs::File::options()
        .write(true)
        .open(&cache)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let t =
        DriveOffsetTable::new("http://127.0.0.1:9/accuraterip/", "spindle-test/0", cache).unwrap();
    assert_eq!(
        t.lookup("PIONEER BD-RW   BDR-209M").await.unwrap().offset,
        667
    );

    // 保存も無く取れない: 表なし
    let t = DriveOffsetTable::new(
        "http://127.0.0.1:9/accuraterip/",
        "spindle-test/0",
        dir.path().join("none.bin"),
    )
    .unwrap();
    assert!(t.lookup("PIONEER BD-RW   BDR-209M").await.is_none());
}

/// 本物の表: 実ドライブの型番（INQUIRY の形）で BDR-209M の +667 が引ける（ネットワークが要る）
#[tokio::test]
#[ignore]
async fn real_table_resolves_the_drive_in_hand() {
    let dir = tempfile::tempdir().unwrap();
    let t = DriveOffsetTable::new(
        "http://www.accuraterip.com/accuraterip/",
        "spindle-test/0 ( https://github.com/AkashiSN/spindle )",
        dir.path().join("DriveOffsets.bin"),
    )
    .unwrap();
    let e = t.lookup("PIONEER BD-RW   BDR-209M").await.unwrap();
    eprintln!("{e:?}");
    assert_eq!(e.offset, 667);
}

/// 壊れた保存は取り直し、壊れた応答（空）は前の正しい保存を上書きしない。メモリの表も期限が来たら
/// 取り直す（常駐し続けても）。取り直しに失敗したら前の表で動く（codex 指摘）
#[tokio::test]
async fn broken_cache_or_response_never_replaces_a_good_table() {
    let body = Arc::new(std::sync::Mutex::new(table_bytes()));
    let hits = Arc::new(AtomicUsize::new(0));
    let (b, h) = (Arc::clone(&body), Arc::clone(&hits));
    let app = Router::new().route(
        "/accuraterip/DriveOffsets.bin",
        get(move || {
            let (b, h) = (Arc::clone(&b), Arc::clone(&h));
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                b.lock().unwrap().clone()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://{addr}/accuraterip/");
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("DriveOffsets.bin");

    // 新しいが壊れた保存 → 取り直して正しい表を保存
    std::fs::write(&cache, [0u8; 69]).unwrap();
    let t = DriveOffsetTable::new(&base, "spindle-test/0", cache.clone()).unwrap();
    assert_eq!(
        t.lookup("PIONEER BD-RW   BDR-209M").await.unwrap().offset,
        667
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(&cache).unwrap(), table_bytes());

    // 古い保存 + 空の応答 → 古い保存で引き、保存は上書きしない
    *body.lock().unwrap() = Vec::new();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 86_400);
    std::fs::File::options()
        .write(true)
        .open(&cache)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let t = DriveOffsetTable::new(&base, "spindle-test/0", cache.clone()).unwrap();
    assert_eq!(
        t.lookup("PIONEER BD-RW   BDR-209M").await.unwrap().offset,
        667
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(std::fs::read(&cache).unwrap(), table_bytes());
    // 失敗の直後は叩き直さない（RETRY_AFTER）
    t.lookup("PIONEER BD-RW   BDR-209M").await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    // 常駐のまま期限が来た: 取り直して新しい登録を拾う（プロセスを再起動しなくても）
    let mut next = table_bytes();
    next.extend(record(12, "NEWVEND  - NEW DRIVE", 9));
    *body.lock().unwrap() = next;
    assert!(t.lookup("NEWVEND NEW DRIVE").await.is_none());
    t.expire_for_test().await;
    assert_eq!(t.lookup("NEWVEND NEW DRIVE").await.unwrap().offset, 12);
    assert_eq!(t.peek("NEWVEND NEW DRIVE").unwrap().offset, 12);
    assert_eq!(hits.load(Ordering::SeqCst), 3);
}

/// 数えるだけのサーバ（`body` を返す。`delay` だけ待ってから）
async fn counting_server(
    body: Arc<std::sync::Mutex<Vec<u8>>>,
    status: axum::http::StatusCode,
    delay: std::time::Duration,
) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = Arc::clone(&hits);
    let app = Router::new().route(
        "/accuraterip/DriveOffsets.bin",
        get(move || {
            let (b, h) = (Arc::clone(&body), Arc::clone(&h));
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(delay).await;
                (status, b.lock().unwrap().clone())
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/accuraterip/"), hits)
}

/// 表が一度も無いまま取得に失敗しても、直後の吸い出しは取りに行かずすぐ表なしで進む（codex 指摘）
#[tokio::test]
async fn failure_without_any_table_is_remembered() {
    let body = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (base, hits) = counting_server(
        body,
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        std::time::Duration::ZERO,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let t = DriveOffsetTable::new(&base, "spindle-test/0", dir.path().join("t.bin")).unwrap();
    assert!(t.lookup("PIONEER BD-RW   BDR-209M").await.is_none());
    assert!(t.lookup("PIONEER BD-RW   BDR-209M").await.is_none());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// 常駐タスクだけで（吸い出しが無くても）期限切れの表を取り直し、status の peek に新しい登録が出る
#[tokio::test]
async fn refresher_updates_the_table_while_resident() {
    use spindle::cd::driveoffsets::spawn_refresher;
    let body = Arc::new(std::sync::Mutex::new(table_bytes()));
    let (base, _hits) = counting_server(
        Arc::clone(&body),
        axum::http::StatusCode::OK,
        std::time::Duration::ZERO,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let t =
        Arc::new(DriveOffsetTable::new(&base, "spindle-test/0", dir.path().join("t.bin")).unwrap());
    let shutdown = tokio_util::sync::CancellationToken::new();
    let task = spawn_refresher(
        Arc::clone(&t),
        std::time::Duration::from_millis(20),
        shutdown.clone(),
    );
    let wait = |model: &'static str, t: Arc<DriveOffsetTable>| async move {
        for _ in 0..200 {
            if let Some(e) = t.peek(model) {
                return Some(e.offset);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        None
    };
    assert_eq!(
        wait("PIONEER BD-RW   BDR-209M", Arc::clone(&t)).await,
        Some(667)
    );
    let mut next = table_bytes();
    next.extend(record(12, "NEWVEND  - NEW DRIVE", 9));
    *body.lock().unwrap() = next;
    t.expire_for_test().await;
    assert_eq!(wait("NEWVEND NEW DRIVE", Arc::clone(&t)).await, Some(12));
    shutdown.cancel();
    task.await.unwrap();
}

/// 古い保存は、取り直しが終わる前から status に出る（stale-while-revalidate。codex 指摘）
#[tokio::test]
async fn stale_cache_is_visible_while_refreshing() {
    let body = Arc::new(std::sync::Mutex::new(table_bytes()));
    let (base, _hits) = counting_server(
        body,
        axum::http::StatusCode::OK,
        std::time::Duration::from_secs(2),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("t.bin");
    std::fs::write(&cache, table_bytes()).unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 86_400);
    std::fs::File::options()
        .write(true)
        .open(&cache)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let t = Arc::new(DriveOffsetTable::new(&base, "spindle-test/0", cache).unwrap());
    let bg = {
        let t = Arc::clone(&t);
        tokio::spawn(async move { t.entries().await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(t.peek("PIONEER BD-RW   BDR-209M").unwrap().offset, 667);
    assert!(!bg.is_finished(), "取り直しはまだ終わっていないはず");
    bg.await.unwrap();
}

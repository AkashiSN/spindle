#![cfg(feature = "fake")]
//! sync の進捗（描画と spindle への送信）。時計は呼ぶたびに 0.5 秒進め、間引きに掛からないようにする
mod support;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use agent_proto::ProgressRequest;
use spindle_agent::progress::{Progress, ProgressSink, ProgressView};
use spindle_agent::sync::SyncOutcome;
use support::Env;

#[derive(Default)]
struct Rec {
    views: Vec<ProgressView>,
    clears: usize,
    summaries: Vec<String>,
}

struct RecSink(Rc<RefCell<Rec>>);

impl ProgressSink for RecSink {
    fn draw(&mut self, v: &ProgressView) {
        self.0.borrow_mut().views.push(v.clone());
    }
    fn clear(&mut self) {
        self.0.borrow_mut().clears += 1;
    }
    fn summary(&mut self, line: &str) {
        self.0.borrow_mut().summaries.push(line.to_owned());
    }
}

fn recording() -> (Progress, Rc<RefCell<Rec>>) {
    let rec = Rc::new(RefCell::new(Rec::default()));
    let t = Rc::new(Cell::new(Duration::ZERO));
    let p = Progress::new(
        Box::new(RecSink(rec.clone())),
        Box::new(move || {
            let now = t.get() + Duration::from_millis(500);
            t.set(now);
            now
        }),
    );
    (p, rec)
}

const A: &[u8] = b"aaaaaaaaaaaaaaaaaaaa";
const B: &[u8] = b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const C: &[u8] = b"ccccccccccccccc";

/// 追加 3 曲・削除 1 曲・プレイリスト 1 件（計 5 件）
fn five_items(env: &Env) -> u64 {
    env.synced_track(9, "Z/z.m4a", b"zzz");
    env.server.remove_track(9);
    env.server.put_track(1, "A/a.m4a", A);
    env.server.put_track(2, "A/b.m4a", B);
    env.server.put_track(3, "A/c.m4a", C);
    env.server.put_playlist(10, "Favs", &[1, 2, 3]);
    (A.len() + B.len() + C.len()) as u64
}

fn assert_monotonic(sent: &[ProgressRequest]) {
    for w in sent.windows(2) {
        assert!(w[0].done <= w[1].done, "{sent:?}");
        assert!(w[0].bytes_done <= w[1].bytes_done, "{sent:?}");
    }
}

#[test]
fn sync_draws_and_sends_progress_to_the_end() {
    let mut env = Env::new();
    env.paired();
    let bytes = five_items(&env);
    env.server.cut_fetch(2, 1);
    let (p, rec) = recording();
    let out = env.sync_with(p).unwrap();
    assert!(
        matches!(out, SyncOutcome::Applied { errors: 0, .. }),
        "{out:?}"
    );

    let rec = rec.borrow();
    let last = rec.views.last().unwrap();
    assert_eq!((last.done, last.total), (5, 5));
    assert_eq!((last.bytes_done, last.bytes_total), (bytes, bytes));
    assert_eq!(rec.summaries.len(), 1, "{:?}", rec.summaries);
    assert!(
        rec.summaries[0].starts_with("完了: 5 件"),
        "{:?}",
        rec.summaries
    );
    assert!(rec.clears >= 1);

    let sent = env.server.progress_log();
    assert!(sent.len() >= 2, "{sent:?}");
    assert_monotonic(&sent);
    let fin = sent.last().unwrap();
    assert_eq!((fin.done, fin.total), (5, 5));
    assert_eq!((fin.bytes_done, fin.bytes_total), (bytes, bytes));
    // 最後の送信は報告より前（報告で計画が閉じれば受け付けられない）
    assert_eq!(env.server.reports().len(), 1);
    assert_eq!(env.server.progress_accepted(), sent.len());
}

#[test]
fn skipped_fetch_still_fills_bytes() {
    let mut env = Env::new();
    env.paired();
    let bytes = five_items(&env);
    env.server.fail_fetch_changed(2, 1);
    let (p, rec) = recording();
    let out = env.sync_with(p).unwrap();
    assert!(matches!(out, SyncOutcome::Applied { .. }), "{out:?}");
    let rec = rec.borrow();
    let last = rec.views.last().unwrap();
    assert_eq!((last.done, last.total), (5, 5));
    assert_eq!((last.bytes_done, last.bytes_total), (bytes, bytes));
    let fin = env.server.progress_log().pop().unwrap();
    assert_eq!((fin.done, fin.bytes_done), (5, bytes));
}

#[test]
fn not_found_stops_sending_but_sync_completes() {
    let mut env = Env::new();
    env.paired();
    five_items(&env);
    env.server.set_progress_not_found(true);
    let (p, _rec) = recording();
    let out = env.sync_with(p).unwrap();
    assert!(matches!(out, SyncOutcome::Applied { .. }), "{out:?}");
    assert_eq!(env.server.progress_log().len(), 1);
    assert_eq!(env.server.reports().len(), 1);
    assert!(env.server.open().is_none());
}

#[test]
fn send_errors_are_ignored_and_retried() {
    let mut env = Env::new();
    env.paired();
    five_items(&env);
    env.server.fail_progress(u32::MAX);
    let (p, rec) = recording();
    let out = env.sync_with(p).unwrap();
    assert!(matches!(out, SyncOutcome::Applied { .. }), "{out:?}");
    assert!(env.server.progress_log().len() >= 2);
    assert_eq!(env.server.progress_accepted(), 0);
    assert_eq!(env.server.reports().len(), 1);
    assert_eq!(rec.borrow().summaries.len(), 1);
}

#[test]
fn disabled_progress_sends_nothing() {
    let mut env = Env::new();
    env.paired();
    five_items(&env);
    let out = env.sync().unwrap();
    assert!(matches!(out, SyncOutcome::Applied { .. }), "{out:?}");
    assert!(env.server.progress_log().is_empty());
}

#[test]
fn path_changes_count_each_item_and_received_bytes() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "A/a.m4a", b"old");
    env.synced_track(2, "A/b.m4a", b"keep");
    // 1 は更新 + 移動（受け取る）、2 は移動だけ
    env.server.put_track(1, "B/a.m4a", A);
    env.server.put_track(2, "C/b.m4a", b"keep");
    let (p, rec) = recording();
    let out = env.sync_with(p).unwrap();
    assert!(
        matches!(out, SyncOutcome::Applied { errors: 0, .. }),
        "{out:?}"
    );
    let rec = rec.borrow();
    let last = rec.views.last().unwrap();
    assert_eq!((last.done, last.total), (2, 2));
    let n = A.len() as u64;
    assert_eq!((last.bytes_done, last.bytes_total), (n, n));
    // 受け取った分は数え直さない（要約のバイト数は受信量）
    assert!(rec.summaries[0].contains("20 B"), "{:?}", rec.summaries);
}

#[test]
fn progress_status_mapping() {
    use reqwest::StatusCode;
    use spindle_agent::progress::ProgressSent;
    use spindle_agent::server::progress_sent;
    assert_eq!(
        progress_sent(StatusCode::NO_CONTENT),
        Some(ProgressSent::Ok)
    );
    assert_eq!(
        progress_sent(StatusCode::NOT_FOUND),
        Some(ProgressSent::Stop)
    );
    assert_eq!(
        progress_sent(StatusCode::CONFLICT),
        Some(ProgressSent::Stop)
    );
    assert_eq!(progress_sent(StatusCode::BAD_GATEWAY), None);
}

/// 送信が失敗し続けても、書き込みのたびには送らない。3 回続けて失敗したらこの計画では送らない
#[test]
fn failing_progress_backs_off_and_latches() {
    let mut env = Env::new();
    env.paired();
    // 曲を多くして tick を何十回も回す（時計は呼ぶたびに 0.5 秒進む）
    for id in 1..=30 {
        env.server.put_track(id, &format!("A/{id}.m4a"), A);
    }
    env.server.fail_progress(u32::MAX);
    let (p, rec) = recording();
    let out = env.sync_with(p).unwrap();
    assert!(matches!(out, SyncOutcome::Applied { .. }), "{out:?}");
    // 2 秒・4 秒と空けて 3 回で打ち切る（最後の flush も送らない）
    assert_eq!(env.server.progress_log().len(), 3);
    assert_eq!(env.server.reports().len(), 1);
    assert_eq!(rec.borrow().summaries.len(), 1);
}

/// 途中で止めたときは「完了」の要約を出さず、行を消すだけ
#[test]
fn stop_clears_line_without_summary() {
    let mut env = Env::new();
    env.paired();
    env.music.set_copy_on(true);
    env.server.put_track(1, "A/a.m4a", A);
    env.server.put_track(2, "A/b.m4a", B);
    let (p, rec) = recording();
    let res = env.sync_with(p);
    assert!(
        matches!(res, Err(spindle_agent::Error::Stop(ref m)) if m.contains("コピー")),
        "{res:?}"
    );
    let rec = rec.borrow();
    assert!(rec.summaries.is_empty(), "{:?}", rec.summaries);
    assert!(rec.clears >= 1);
    // 最後の進捗は送ってから報告する
    assert!(!env.server.progress_log().is_empty());
    assert_eq!(env.server.reports().len(), 1);
}

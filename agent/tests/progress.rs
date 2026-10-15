use agent_proto::{ProgressPhase, PROGRESS_CURRENT_MAX};
use spindle_agent::progress::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

const MIB: u64 = 1024 * 1024;

#[derive(Default)]
struct Rec {
    draws: usize,
    clears: usize,
    summaries: Vec<String>,
}
struct RecSink(Rc<RefCell<Rec>>);
impl ProgressSink for RecSink {
    fn draw(&mut self, _v: &ProgressView) {
        self.0.borrow_mut().draws += 1;
    }
    fn clear(&mut self) {
        self.0.borrow_mut().clears += 1;
    }
    fn summary(&mut self, line: &str) {
        self.0.borrow_mut().summaries.push(line.to_string());
    }
}

fn make() -> (Progress, Rc<Cell<Duration>>, Rc<RefCell<Rec>>) {
    let t = Rc::new(Cell::new(Duration::ZERO));
    let rec = Rc::new(RefCell::new(Rec::default()));
    let t2 = t.clone();
    let p = Progress::new(Box::new(RecSink(rec.clone())), Box::new(move || t2.get()));
    (p, t, rec)
}
fn secs(t: &Cell<Duration>, s: f64) {
    t.set(Duration::from_secs_f64(s));
}

#[test]
fn format_helpers() {
    assert_eq!(format_bytes(0), "0 B");
    assert_eq!(format_bytes(512), "512 B");
    assert_eq!(format_bytes(1536), "1.5 KB");
    assert_eq!(format_bytes(89_443_123), "85.3 MB");
    assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.0 GB");
    assert_eq!(format_rate(13_002_342), "12.4 MB/s");
    assert_eq!(format_duration(42), "0:42");
    assert_eq!(format_duration(723), "12:03");
    assert_eq!(format_duration(3723), "1:02:03");
}

#[test]
fn rate_uses_five_second_window() {
    let (p, t, _) = make();
    p.start(1, 1, 100, 100 * MIB);
    p.begin_item(ProgressPhase::Fetch, Some("a"), 100 * MIB);
    secs(&t, 1.0);
    p.add_bytes(MIB);
    secs(&t, 2.0);
    p.add_bytes(MIB);
    // 5 秒未満は経過時間で割る
    assert_eq!(p.view().rate, MIB);
    for s in 3..=5 {
        secs(&t, s as f64);
        p.add_bytes(MIB);
    }
    assert_eq!(p.view().rate, MIB);
    for s in 6..=10 {
        secs(&t, s as f64);
        p.add_bytes(MIB);
    }
    assert_eq!(p.view().rate, MIB);
    secs(&t, 15.0);
    assert_eq!(p.view().rate, 0);
}

#[test]
fn eta_cases() {
    let (p, t, _) = make();
    p.start(1, 1, 1, 10 * MIB);
    p.begin_item(ProgressPhase::Fetch, None, 10 * MIB);
    for s in 1..=4 {
        secs(&t, s as f64);
        p.add_bytes(MIB);
    }
    let v = p.view();
    assert_eq!((v.rate, v.bytes_done), (MIB, 4 * MIB));
    assert_eq!(v.eta_secs, Some(6));
    secs(&t, 30.0);
    assert_eq!(p.view().eta_secs, None);

    let (p, t, _) = make();
    p.start(1, 1, 1, 0);
    p.begin_item(ProgressPhase::Delete, None, 0);
    secs(&t, 1.0);
    assert_eq!(p.view().eta_secs, None);
}

#[test]
fn end_item_fills_and_caps() {
    let (p, _, _) = make();
    p.start(1, 1, 3, 300);
    p.begin_item(ProgressPhase::Fetch, Some("a"), 100);
    p.add_bytes(30);
    p.end_item();
    let v = p.view();
    assert_eq!((v.done, v.bytes_done), (1, 100));
    p.begin_item(ProgressPhase::Fetch, Some("b"), 100);
    p.add_bytes(250);
    assert_eq!(p.view().bytes_done, 200);
    p.end_item();
    assert_eq!((p.view().done, p.view().bytes_done), (2, 200));
    p.end_items(5, 7);
    assert_eq!((p.view().done, p.view().bytes_done), (7, 207));
}

#[test]
fn draw_and_send_are_throttled() {
    let (p, t, rec) = make();
    p.start(9, 4, 10, 100);
    p.begin_item(ProgressPhase::Fetch, Some("a"), 100);
    for i in 0..10 {
        secs(&t, i as f64 * 0.05);
        p.add_bytes(1);
    }
    // 0.0〜0.45 秒: 0.2 秒に 1 回 → 0.0, 0.2, 0.4 の 3 回
    assert_eq!(rec.borrow().draws, 3);

    secs(&t, 0.0);
    let (id, r) = p.take_send(false).expect("初回は送る");
    assert_eq!((id, r.generation, r.total), (9, 4, 10));
    secs(&t, 1.9);
    assert!(p.take_send(false).is_none());
    assert!(p.take_send(true).is_some());
    secs(&t, 4.0);
    assert!(p.take_send(false).is_some());
    secs(&t, 4.1);
    assert!(p.take_send(false).is_none());
}

#[test]
fn send_truncates_current_on_char_boundary() {
    let (p, _, _) = make();
    p.start(1, 1, 1, 0);
    let long = "あ".repeat(1000); // 3000 バイト
    p.begin_item(ProgressPhase::Add, Some(&long), 0);
    let (_, r) = p.take_send(true).unwrap();
    let c = r.current.unwrap();
    assert!(c.len() <= PROGRESS_CURRENT_MAX);
    assert_eq!(c.len(), 1023);
    assert!(long.starts_with(&c));
}

fn view(total_bytes: u64) -> ProgressView {
    ProgressView {
        done: 12,
        total: 92,
        bytes_done: 89_443_123,
        bytes_total: total_bytes,
        rate: 13_002_342,
        eta_secs: Some(42),
        phase: ProgressPhase::Fetch,
        current: Some("A/B/c.m4a".into()),
    }
}

#[test]
fn render_line_shapes() {
    let l = render_line(&view(665_625_600), 200);
    assert_eq!(
        l,
        "[12/92] 85.3 MB / 634.8 MB  12.4 MB/s  残り 0:42  受信中: A/B/c.m4a"
    );
    let mut v = view(0);
    v.eta_secs = None;
    assert_eq!(render_line(&v, 200), "[12/92] 受信中: A/B/c.m4a");
}

#[test]
fn render_line_truncates_current_from_front() {
    let mut v = view(665_625_600);
    v.current = Some("アーティスト/アルバム/01 曲名はとても長い題名.m4a".into());
    let l = render_line(&v, 70);
    assert!(l.contains("受信中: …"), "{l}");
    assert!(l.ends_with("m4a"));
    assert!(display_width(&l) <= 70, "{} {l}", display_width(&l));
    let l = render_line(&v, 40);
    assert!(display_width(&l) <= 40, "{} {l}", display_width(&l));
}

#[test]
fn summary_lines() {
    assert_eq!(
        summary_line(92, 665_625_600, 51),
        "完了: 92 件（634.8 MB）を 0:51 で（平均 12.4 MB/s）"
    );
    assert_eq!(summary_line(3, 0, 2), "完了: 3 件を 0:02 で");
}

#[test]
fn finish_and_disabled() {
    let (p, t, rec) = make();
    assert!(p.finish().is_none());
    p.start(1, 1, 2, 0);
    p.begin_item(ProgressPhase::Delete, Some("x"), 0);
    p.end_item();
    secs(&t, 2.0);
    p.begin_item(ProgressPhase::Delete, Some("y"), 0);
    p.end_item();
    assert_eq!(p.finish().unwrap(), "完了: 2 件を 0:02 で");
    assert_eq!(rec.borrow().summaries.len(), 1);
    p.clear();
    assert!(rec.borrow().clears >= 1);

    let d = Progress::disabled();
    d.start(1, 1, 1, 1);
    d.begin_item(ProgressPhase::Fetch, Some("a"), 1);
    d.add_bytes(1);
    d.end_item();
    d.end_items(1, 1);
    assert!(d.take_send(true).is_none());
    assert!(d.finish().is_none());
    d.clear();
}

/// 送信が詰まる（5 秒でタイムアウトする）と、間隔は送信が終わってから数え、失敗が続くと延ばし、
/// 3 回続けて失敗したらこの計画では送らない。書き込みのたびには送らない
#[test]
fn hung_send_backs_off_from_end_and_latches() {
    let (p, t, _) = make();
    p.start(1, 1, 1, 1_000_000);
    p.begin_item(ProgressPhase::Fetch, Some("a"), 1_000_000);
    let sends: Rc<RefCell<Vec<Duration>>> = Rc::default();
    let hang = {
        let t = t.clone();
        let sends = sends.clone();
        move |_: i64, _: &agent_proto::ProgressRequest| {
            sends.borrow_mut().push(t.get());
            t.set(t.get() + Duration::from_secs(5));
            ProgressSent::Failed
        }
    };
    // 64 KiB 相当の書き込みが 0.1 秒ごとに続く
    for _ in 0..600 {
        p.add_bytes(1);
        p.tick(&hang);
        t.set(t.get() + Duration::from_millis(100));
    }
    let sends = sends.borrow();
    // 0 秒で送って 5 秒に失敗 → 2 秒空けて 7 秒 → 12 秒に失敗 → 4 秒空けて 16 秒 → 3 回目で打ち切り
    assert_eq!(
        *sends,
        vec![
            Duration::ZERO,
            Duration::from_secs(7),
            Duration::from_secs(16)
        ]
    );
    // 打ち切った後は最後の 1 回（flush）も送らない
    p.flush(&hang);
    assert_eq!(sends.len(), 3);
}

/// 成功すれば失敗の数え直し。間隔も送信の終わりから 2 秒に戻る
#[test]
fn send_interval_counts_from_end_and_resets_on_success() {
    let (p, t, _) = make();
    p.start(1, 1, 1, 0);
    let n = Rc::new(Cell::new(0u32));
    let slow_ok = {
        let t = t.clone();
        let n = n.clone();
        move |_: i64, _: &agent_proto::ProgressRequest| {
            n.set(n.get() + 1);
            t.set(t.get() + Duration::from_secs(3));
            ProgressSent::Ok
        }
    };
    p.tick(&slow_ok); // 0 秒に送り 3 秒に終わる
    assert_eq!(n.get(), 1);
    secs(&t, 4.9);
    p.tick(&slow_ok);
    assert_eq!(n.get(), 1, "終わりから 2 秒経っていない");
    secs(&t, 5.0);
    p.tick(&slow_ok);
    assert_eq!(n.get(), 2);
}

/// Stop で打ち切っても、次の計画（start）ではまた送る
#[test]
fn start_resets_send_latch() {
    let (p, t, _) = make();
    let n = Rc::new(Cell::new(0u32));
    let stop = {
        let n = n.clone();
        move |_: i64, _: &agent_proto::ProgressRequest| {
            n.set(n.get() + 1);
            ProgressSent::Stop
        }
    };
    p.start(1, 1, 1, 0);
    p.tick(&stop);
    secs(&t, 10.0);
    p.tick(&stop);
    p.flush(&stop);
    assert_eq!(n.get(), 1);
    // 再開した計画の後に新しい計画
    p.start(2, 1, 1, 0);
    let ok = |_: i64, _: &agent_proto::ProgressRequest| ProgressSent::Ok;
    assert!(p.take_send(true).is_some());
    secs(&t, 20.0);
    let m = Rc::new(Cell::new(0u32));
    let m2 = m.clone();
    p.tick(&move |a, r| {
        m2.set(m2.get() + 1);
        ok(a, r)
    });
    assert_eq!(m.get(), 1);
}

fn heartbeat_of(p: &Progress) -> Heartbeat {
    p.heartbeat().expect("有効な進捗には心拍の口がある")
}

/// 項目が長く掛かって（エンジンが何も呼ばなくても）、心拍は 2 秒ごとに最新の値を送る
#[test]
fn heartbeat_sends_latest_while_engine_blocks() {
    let (p, t, _) = make();
    let hb = heartbeat_of(&p);
    let clock = {
        let t = t.clone();
        move || t.get()
    };
    let sent: Rc<RefCell<Vec<(Duration, agent_proto::ProgressRequest)>>> = Rc::default();
    let rec = {
        let t = t.clone();
        let sent = sent.clone();
        move |_: i64, r: &agent_proto::ProgressRequest| {
            sent.borrow_mut().push((t.get(), r.clone()));
            ProgressSent::Ok
        }
    };
    // 始める前は送らない
    assert!(!hb.beat(&clock, &rec));
    p.start(7, 3, 2, 0);
    p.begin_item(ProgressPhase::Delete, Some("a"), 0);
    p.tick(&rec); // 0 秒: 項目の始めにエンジンが送る
    p.end_item();
    secs(&t, 0.5);
    p.begin_item(ProgressPhase::Add, Some("b"), 0);
    // ここからエンジンは止まっている（osascript 待ちなど）
    secs(&t, 1.9);
    assert!(!hb.beat(&clock, &rec), "前回の送信から 2 秒経っていない");
    secs(&t, 2.0);
    assert!(hb.beat(&clock, &rec));
    secs(&t, 3.0);
    assert!(!hb.beat(&clock, &rec));
    secs(&t, 4.0);
    assert!(hb.beat(&clock, &rec), "値が変わらなくても 2 秒ごとに送る");
    let sent = sent.borrow();
    assert_eq!(sent.len(), 3);
    let (at, r) = &sent[1];
    assert_eq!(*at, Duration::from_secs(2));
    assert_eq!((r.done, r.total, r.generation), (1, 2, 3));
    assert_eq!(r.phase, ProgressPhase::Add);
    assert_eq!(r.current.as_deref(), Some("b"));
    // 心拍が送った後は、エンジンも間隔を空ける
    secs(&t, 5.0);
    assert!(p.take_send(false).is_none());
}

/// 心拍とエンジンは打ち切り・失敗の数を共有し、終えた計画（finish / clear）では送らない
#[test]
fn heartbeat_shares_latch_and_stops_after_finish() {
    let (p, t, _) = make();
    let hb = heartbeat_of(&p);
    let clock = {
        let t = t.clone();
        move || t.get()
    };
    let n = Rc::new(Cell::new(0u32));
    let fail = {
        let n = n.clone();
        move |_: i64, _: &agent_proto::ProgressRequest| {
            n.set(n.get() + 1);
            ProgressSent::Failed
        }
    };
    p.start(1, 1, 1, 0);
    p.tick(&fail); // 0 秒: エンジンが 1 回目に失敗 → 2 秒空ける
    secs(&t, 2.0);
    assert!(hb.beat(&clock, &fail)); // 2 回目に失敗 → 4 秒空ける
    secs(&t, 5.0);
    assert!(!hb.beat(&clock, &fail));
    secs(&t, 6.0);
    assert!(hb.beat(&clock, &fail)); // 3 回目で打ち切り
    secs(&t, 100.0);
    assert!(!hb.beat(&clock, &fail));
    p.flush(&fail);
    assert_eq!(n.get(), 3);

    // 次の計画では送り直し、終えたら心拍は送らない
    let ok = |_: i64, _: &agent_proto::ProgressRequest| ProgressSent::Ok;
    p.start(2, 1, 1, 0);
    assert!(hb.beat(&clock, &ok));
    p.finish();
    secs(&t, 200.0);
    assert!(!hb.beat(&clock, &ok));
    p.start(3, 1, 1, 0);
    p.clear();
    assert!(!hb.beat(&clock, &ok));
}

/// 心拍が送っている最中は、エンジンの間引いた送信（tick）は重ねて送らない
#[test]
fn engine_tick_skips_while_heartbeat_in_flight() {
    let (p, t, _) = make();
    let hb = heartbeat_of(&p);
    let clock = {
        let t = t.clone();
        move || t.get()
    };
    p.start(1, 1, 1, 0);
    let inner = Rc::new(Cell::new(None::<bool>));
    let inner2 = inner.clone();
    let pr = &p;
    let during = move |_: i64, _: &agent_proto::ProgressRequest| {
        let n = Cell::new(0u32);
        pr.tick(&|_, _| {
            n.set(n.get() + 1);
            ProgressSent::Ok
        });
        inner2.set(Some(pr.take_send(true).is_some() || n.get() > 0));
        ProgressSent::Ok
    };
    assert!(hb.beat(&clock, &during));
    assert_eq!(inner.get(), Some(false));
    // 終われば送れる
    assert!(p.take_send(true).is_some());
}

type Held = (
    std::sync::mpsc::Receiver<i64>,
    std::sync::mpsc::Sender<ProgressSent>,
    std::thread::JoinHandle<bool>,
);

/// 心拍の送信を別スレッドで始め、`release` に結果を送るまで止めておく
fn hold_heartbeat(hb: Heartbeat, at: Duration) -> Held {
    use std::sync::mpsc;
    use std::sync::Mutex;
    let (started_tx, started_rx) = mpsc::channel::<i64>();
    let (release_tx, release_rx) = mpsc::channel::<ProgressSent>();
    let release_rx = Mutex::new(release_rx);
    let th = std::thread::spawn(move || {
        hb.beat(&move || at, &|id, _| {
            let _ = started_tx.send(id);
            release_rx
                .lock()
                .ok()
                .and_then(|r| r.recv().ok())
                .unwrap_or(ProgressSent::Failed)
        })
    });
    (started_rx, release_tx, th)
}

/// 最後の flush は、心拍の送信中なら終わるのを待ってから最後の値を送る（落とさない）
#[test]
fn flush_waits_for_heartbeat_and_sends_final() {
    let (p, t, _) = make();
    p.start(1, 1, 2, 0);
    p.begin_item(ProgressPhase::Delete, Some("a"), 0);
    let (started, release, th) = hold_heartbeat(heartbeat_of(&p), Duration::ZERO);
    assert_eq!(started.recv_timeout(Duration::from_secs(5)), Ok(1));
    p.end_item();
    p.begin_item(ProgressPhase::Delete, Some("b"), 0);
    p.end_item();
    secs(&t, 0.5);
    // flush が待っている間に心拍の送信を終わらせる
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        let _ = release.send(ProgressSent::Ok);
    });
    let sent: RefCell<Vec<agent_proto::ProgressRequest>> = RefCell::default();
    p.flush(&|_, r| {
        sent.borrow_mut().push(r.clone());
        ProgressSent::Ok
    });
    assert!(th.join().unwrap());
    releaser.join().unwrap();
    let sent = sent.into_inner();
    assert_eq!(sent.len(), 1, "最後の値を送る");
    assert_eq!((sent[0].done, sent[0].total), (2, 2));
}

/// flush が心拍の送信を待つ間に時計が進んだら、最後の値の速度は待った後の時刻で計る（受信が止まれば 0）
#[test]
fn flush_measures_rate_after_waiting() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    let ms = Arc::new(AtomicU64::new(0));
    let clock = {
        let ms = ms.clone();
        move || Duration::from_millis(ms.load(Ordering::SeqCst))
    };
    let p = Progress::new(Box::new(RecSink(Rc::default())), Box::new(clock.clone()));
    p.start(1, 1, 1, 10 * MIB);
    p.begin_item(ProgressPhase::Fetch, Some("a"), 10 * MIB);
    for i in 1..=4u64 {
        ms.store(i * 1000, Ordering::SeqCst);
        p.add_bytes(MIB);
    }
    p.set_phase(ProgressPhase::Add);
    assert_eq!(p.view().rate, MIB);
    // 心拍の送信が詰まり、終わるまでに時計が 10 秒まで進む
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let hb = heartbeat_of(&p);
    let ms2 = ms.clone();
    let th = std::thread::spawn(move || {
        hb.beat(&clock, &|_, _| {
            let _ = started_tx.send(());
            std::thread::sleep(Duration::from_millis(200));
            ms2.store(10_000, Ordering::SeqCst);
            ProgressSent::Ok
        })
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let rates: RefCell<Vec<u64>> = RefCell::default();
    p.flush(&|_, r| {
        rates.borrow_mut().push(r.rate);
        ProgressSent::Ok
    });
    assert!(th.join().unwrap());
    assert_eq!(
        rates.into_inner(),
        vec![0],
        "待った後の時刻（最後の受信から 6 秒）で計る"
    );
}

/// 前の計画の心拍の送信が、次の計画を始めた後に終わっても（404 でも）、その結果は新しい計画の
/// 間隔・失敗の数・打ち切りに触れない
#[test]
fn stale_heartbeat_result_does_not_touch_next_plan() {
    let (p, t, _) = make();
    p.start(1, 1, 1, 0);
    let (started, release, th) = hold_heartbeat(heartbeat_of(&p), Duration::ZERO);
    assert_eq!(started.recv_timeout(Duration::from_secs(5)), Ok(1));
    // 計画 1 を終え（エラーで抜けた形。flush はしない）、同じ実行で計画 2 を始める
    p.finish();
    secs(&t, 1.0);
    p.start(2, 1, 1, 0);
    let n = Rc::new(Cell::new(0u32));
    let count = {
        let n = n.clone();
        move |_: i64, _: &agent_proto::ProgressRequest| {
            n.set(n.get() + 1);
            ProgressSent::Ok
        }
    };
    // 古い送信を 404（Stop）で終わらせる
    release.send(ProgressSent::Stop).unwrap();
    assert!(th.join().unwrap());
    // 新しい計画は打ち切られず、間隔も古い送信の終わりから数えない（初回はすぐ送る）
    p.tick(&count);
    assert_eq!(n.get(), 1);
    secs(&t, 3.0);
    p.tick(&count);
    assert_eq!(n.get(), 2);
    let (id, _) = p.take_send(true).expect("新しい計画は送れる");
    assert_eq!(id, 2);
}

/// 受信の止まった段（追加・プレイリスト）では、心拍が送る速度は送る時点で計り直され、5 秒で 0 に落ちる
#[test]
fn heartbeat_recomputes_rate_after_receiving_stops() {
    let (p, t, _) = make();
    let hb = heartbeat_of(&p);
    let clock = {
        let t = t.clone();
        move || t.get()
    };
    let rates: Rc<RefCell<Vec<u64>>> = Rc::default();
    let rec = {
        let rates = rates.clone();
        move |_: i64, r: &agent_proto::ProgressRequest| {
            rates.borrow_mut().push(r.rate);
            ProgressSent::Ok
        }
    };
    p.start(1, 1, 1, 10 * MIB);
    p.begin_item(ProgressPhase::Fetch, Some("a"), 10 * MIB);
    for i in 1..=4 {
        secs(&t, i as f64);
        p.add_bytes(MIB);
    }
    p.tick(&rec); // 4 秒: 毎秒 1 MiB
    assert_eq!(rates.borrow()[0], MIB);
    p.set_phase(ProgressPhase::Add);
    // ここから受信は無い（osascript 待ち）
    secs(&t, 6.0);
    assert!(hb.beat(&clock, &rec));
    assert!(rates.borrow()[1] > 0, "窓の中にはまだ受信がある");
    assert!(rates.borrow()[1] < MIB);
    secs(&t, 9.0);
    assert!(hb.beat(&clock, &rec));
    assert_eq!(rates.borrow()[2], 0, "最後の受信から 5 秒で 0");
    // 端末の表示も同じ速度
    assert_eq!(p.view().rate, 0);
}

/// 本物のスレッドで回すと、エンジンが何も呼ばなくても送られる。落とすと止まる
#[test]
fn heartbeat_thread_sends_and_stops_on_drop() {
    use std::sync::mpsc;
    use std::time::Instant;
    let t0 = Instant::now();
    let p = Progress::new(
        Box::new(RecSink(Rc::default())),
        Box::new(move || t0.elapsed()),
    );
    p.start(5, 1, 1, 0);
    p.begin_item(ProgressPhase::Add, Some("x"), 0);
    let (tx, rx) = mpsc::channel();
    let th = heartbeat_of(&p).spawn(
        move || t0.elapsed(),
        move |id, r| {
            let _ = tx.send((id, r.phase));
            ProgressSent::Ok
        },
    );
    let got = rx.recv_timeout(Duration::from_secs(5)).expect("心拍が送る");
    assert_eq!(got, (5, ProgressPhase::Add));
    drop(th);
    // 止めた後は、送信の間隔（2 秒）を過ぎても届かない
    assert!(rx.recv_timeout(Duration::from_millis(2600)).is_err());
}

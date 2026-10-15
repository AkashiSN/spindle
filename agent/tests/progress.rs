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

//! sync の進捗の数え方と書式（docs/superpowers/specs/2026-10-15-agent-sync-progress-design.md）。
//! 外界（標準エラー・時計）は `ProgressSink` と `Clock` に閉じ込め、ここは純粋に数えて整形するだけ

use agent_proto::{ProgressPhase, ProgressRequest, PROGRESS_CURRENT_MAX};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 開始からの単調な経過
pub type Clock = Box<dyn Fn() -> Duration>;

/// 再描画の最短間隔
const DRAW_INTERVAL: Duration = Duration::from_millis(200);
/// サーバへ送る最短間隔（前回の送信が終わってから数える）
const SEND_INTERVAL: Duration = Duration::from_secs(2);
/// 送信の失敗が続いたときの間隔の上限
const SEND_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// 送信がこの回数続けて失敗したら、この計画では以後送らない
const SEND_FAIL_LIMIT: u32 = 3;
/// 速度を平均する窓
const RATE_WINDOW: Duration = Duration::from_secs(5);
/// 心拍のスレッドが送るべきか見に行く間隔
const HEARTBEAT_POLL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq)]
pub struct ProgressView {
    pub done: u64,
    pub total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub rate: u64,
    pub eta_secs: Option<u64>,
    pub phase: ProgressPhase,
    pub current: Option<String>,
}

pub trait ProgressSink {
    /// 0.2 秒に 1 回まで呼ばれる
    fn draw(&mut self, v: &ProgressView);
    /// 進捗の行を消す
    fn clear(&mut self);
    /// 終わりの 1 行
    fn summary(&mut self, line: &str);
}

/// 進捗の送信の結果（`Server::progress`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressSent {
    Ok,
    /// 404（計画が無い・古いサーバでルートが無い）か 409。この計画では以後送らない
    Stop,
    /// 通信エラー・タイムアウト・5xx。同期は止めず、間隔を空けて送り直す
    Failed,
}

struct Inner {
    sink: Box<dyn ProgressSink>,
    clock: Clock,
    started: Option<Started>,
    /// 送信の間隔・打ち切りと最新の値。心拍のスレッドと共有する
    gate: Arc<Mutex<Gate>>,
}

/// 送信の門。エンジンと心拍のスレッドのどちらが送っても、間隔・失敗の数・打ち切りは
/// ここで 1 つに数える。時刻はエンジンの時計（`Clock`）の値で持つ
#[derive(Default)]
struct Gate {
    /// 送る値（計画 id と本文）。エンジンが値を変えるたびに書き直す
    snapshot: Option<(i64, ProgressRequest)>,
    /// 計画を始めてから、終わる（`finish` / `clear`）まで
    open: bool,
    /// 前回の送信（の終わり）の時刻
    last_send: Option<Duration>,
    /// 次の送信までに空ける間隔。失敗が続くと 2 秒・4 秒・8 秒…（上限 60 秒）と延ばす
    send_gap: Duration,
    /// 続けて失敗した回数
    send_failures: u32,
    /// `ProgressSent::Stop` を受けたか、失敗が続いた。この計画では以後送らない
    stopped: bool,
    /// どちらかが送っている最中。重ねて送らない（古い値が後から届かないように）
    in_flight: bool,
}

impl Gate {
    fn reset(&mut self, snapshot: (i64, ProgressRequest)) {
        *self = Gate {
            snapshot: Some(snapshot),
            open: true,
            last_send: None,
            send_gap: SEND_INTERVAL,
            send_failures: 0,
            stopped: false,
            in_flight: false,
        };
    }

    /// 送るべき時刻なら送る値を返し、送った時刻を記す。`mark` なら送信中の印を付ける
    fn claim(&mut self, now: Duration, force: bool, mark: bool) -> Option<(i64, ProgressRequest)> {
        if !self.open || self.stopped || self.in_flight {
            return None;
        }
        if !force {
            if let Some(last) = self.last_send {
                if now >= last && now - last < self.send_gap {
                    return None;
                }
            }
        }
        let snap = self.snapshot.clone()?;
        self.last_send = Some(now);
        self.in_flight = mark;
        Some(snap)
    }

    /// 送信の結果を記す。間隔は送信が終わった時刻から数える
    fn record(&mut self, now: Duration, sent: ProgressSent) {
        self.in_flight = false;
        self.last_send = Some(now);
        match sent {
            ProgressSent::Ok => {
                self.send_failures = 0;
                self.send_gap = SEND_INTERVAL;
            }
            ProgressSent::Stop => self.stopped = true,
            ProgressSent::Failed => {
                self.send_failures += 1;
                if self.send_failures >= SEND_FAIL_LIMIT {
                    self.stopped = true;
                }
                self.send_gap = SEND_INTERVAL
                    .saturating_mul(1 << self.send_failures.saturating_sub(1).min(5))
                    .min(SEND_BACKOFF_MAX);
            }
        }
    }
}

/// 送信を別スレッドから補う口（`Progress::heartbeat`）。値が変わらなくても、
/// 項目が長く掛かっても（最初の削除・ミュージック.app への追加・大きなプレイリストの置き換え）、
/// 前回の送信から間隔が空けば最新の値を送り直す。打ち切り・間隔の延長はエンジンと共有する
#[derive(Clone)]
pub struct Heartbeat {
    gate: Arc<Mutex<Gate>>,
}

impl Heartbeat {
    /// 送る時刻なら最新の値を `send` で送り、送ったら true。`clock` はエンジンと同じ基準の時計
    pub fn beat(
        &self,
        clock: &dyn Fn() -> Duration,
        send: &dyn Fn(i64, &ProgressRequest) -> ProgressSent,
    ) -> bool {
        let claimed = match self.gate.lock() {
            Ok(mut g) => g.claim(clock(), false, true),
            Err(_) => None,
        };
        // 送っている間は門を握らない（エンジンを止めない）
        let Some((plan_id, req)) = claimed else {
            return false;
        };
        let sent = send(plan_id, &req);
        if let Ok(mut g) = self.gate.lock() {
            g.record(clock(), sent);
        }
        true
    }

    /// 別スレッドで 0.25 秒ごとに `beat` を回す。返り値を落とすと止まる（送信中なら終わるのを待たない）
    pub fn spawn(
        self,
        clock: impl Fn() -> Duration + Send + 'static,
        send: impl Fn(i64, &ProgressRequest) -> ProgressSent + Send + 'static,
    ) -> HeartbeatThread {
        let quit = Arc::new(AtomicBool::new(false));
        let q = quit.clone();
        // スレッドを作れなければ心拍なしで続ける（同期は止めない）
        let _ = std::thread::Builder::new()
            .name("progress-heartbeat".to_owned())
            .spawn(move || {
                while !q.load(Ordering::Relaxed) {
                    std::thread::sleep(HEARTBEAT_POLL);
                    if q.load(Ordering::Relaxed) {
                        break;
                    }
                    self.beat(&clock, &send);
                }
            });
        HeartbeatThread { quit }
    }
}

/// 心拍のスレッド。落とすと止まる
pub struct HeartbeatThread {
    quit: Arc<AtomicBool>,
}

impl Drop for HeartbeatThread {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
    }
}

struct Started {
    plan_id: i64,
    generation: i64,
    t0: Duration,
    total: u64,
    bytes_total: u64,
    done: u64,
    /// 項目の開始時の `bytes_done`
    item_base: u64,
    item_expected: u64,
    item_got: u64,
    /// 完了済みの項目で確定したバイト数
    bytes_done: u64,
    /// 受信量の標本（開始からの時刻, 累計）。直近の窓の外へ出る最後の 1 点まで残す
    samples: VecDeque<(Duration, u64)>,
    received: u64,
    phase: ProgressPhase,
    current: Option<String>,
    last_draw: Option<Duration>,
}

/// 内部可変。`&self` だけで呼べる（`disabled()` は何もしない）
pub struct Progress {
    inner: Option<RefCell<Inner>>,
}

impl Progress {
    pub fn disabled() -> Self {
        Progress { inner: None }
    }

    pub fn new(sink: Box<dyn ProgressSink>, clock: Clock) -> Self {
        Progress {
            inner: Some(RefCell::new(Inner {
                sink,
                clock,
                started: None,
                gate: Arc::default(),
            })),
        }
    }

    /// 別スレッドから送信を補う口（`disabled()` では None）
    pub fn heartbeat(&self) -> Option<Heartbeat> {
        self.with(|i| {
            Some(Heartbeat {
                gate: i.gate.clone(),
            })
        })
    }

    fn with<R>(&self, f: impl FnOnce(&mut Inner) -> Option<R>) -> Option<R> {
        let cell = self.inner.as_ref()?;
        let mut g = cell.try_borrow_mut().ok()?;
        f(&mut g)
    }

    pub fn start(&self, plan_id: i64, generation: i64, total: u64, bytes_total: u64) {
        self.with(|i| {
            let t0 = (i.clock)();
            i.started = Some(Started {
                plan_id,
                generation,
                t0,
                total,
                bytes_total,
                done: 0,
                item_base: 0,
                item_expected: 0,
                item_got: 0,
                bytes_done: 0,
                samples: VecDeque::new(),
                received: 0,
                phase: ProgressPhase::Delete,
                current: None,
                last_draw: None,
            });
            let snap = i.started.as_ref()?.request(t0);
            if let Ok(mut g) = i.gate.lock() {
                g.reset(snap);
            }
            Some(())
        });
    }

    pub fn begin_item(&self, phase: ProgressPhase, current: Option<&str>, expected_bytes: u64) {
        self.with(|i| {
            let s = i.started.as_mut()?;
            s.phase = phase;
            s.current = current.map(str::to_string);
            s.item_base = s.bytes_done;
            s.item_expected = expected_bytes;
            s.item_got = 0;
            i.touch(true);
            Some(())
        });
    }

    /// 今の項目の段だけを変える（受信を終えてミュージック.app へ追加する、など）
    pub fn set_phase(&self, phase: ProgressPhase) {
        self.with(|i| {
            i.started.as_mut()?.phase = phase;
            i.touch(true);
            Some(())
        });
    }

    pub fn add_bytes(&self, n: u64) {
        self.with(|i| {
            let now = (i.clock)();
            let s = i.started.as_mut()?;
            let n = n.min(s.item_expected - s.item_got);
            if n > 0 {
                s.item_got += n;
                s.bytes_done += n;
                s.received += n;
                let t = now.saturating_sub(s.t0);
                s.samples.push_back((t, s.received));
                // 窓の外にある標本は、窓の端の基準になる 1 点だけ残す
                let edge = t.saturating_sub(RATE_WINDOW);
                while s.samples.len() >= 2 && s.samples[1].0 <= edge {
                    s.samples.pop_front();
                }
            }
            i.touch(false);
            Some(())
        });
    }

    pub fn end_item(&self) {
        self.with(|i| {
            let s = i.started.as_mut()?;
            s.done += 1;
            s.bytes_done = s.item_base + s.item_expected;
            s.item_base = s.bytes_done;
            s.item_expected = 0;
            s.item_got = 0;
            i.touch(true);
            Some(())
        });
    }

    pub fn end_items(&self, n: u64, bytes: u64) {
        self.with(|i| {
            let s = i.started.as_mut()?;
            s.done += n;
            s.bytes_done += bytes;
            s.item_base = s.bytes_done;
            s.item_expected = 0;
            s.item_got = 0;
            i.touch(true);
            Some(())
        });
    }

    pub fn view(&self) -> ProgressView {
        self.with(|i| {
            let now = (i.clock)();
            i.started.as_ref().map(|s| s.view(now))
        })
        .unwrap_or(ProgressView {
            done: 0,
            total: 0,
            bytes_done: 0,
            bytes_total: 0,
            rate: 0,
            eta_secs: None,
            phase: ProgressPhase::Delete,
            current: None,
        })
    }

    /// 送るべき時刻なら Some（前回の送信から 2 秒、失敗が続けばそれ以上空ける。最後の 1 回は `force`）。
    /// 送らなくなった計画・終えた計画・心拍が送っている最中は常に None
    pub fn take_send(&self, force: bool) -> Option<(i64, ProgressRequest)> {
        self.claim(force, false)
    }

    /// 今の値を門へ書いてから、送る時刻か門に問う
    fn claim(&self, force: bool, mark: bool) -> Option<(i64, ProgressRequest)> {
        self.with(|i| {
            let now = (i.clock)();
            let snap = i.started.as_ref()?.request(now);
            let mut g = i.gate.lock().ok()?;
            g.snapshot = Some(snap);
            g.claim(now, force, mark)
        })
    }

    /// 間引いて描き、送る時刻なら `send` で送る。送信の失敗は `send` の側で `Ok` として捨てる
    pub fn tick(&self, send: &dyn Fn(i64, &ProgressRequest) -> ProgressSent) {
        self.with(|i| {
            i.maybe_draw(false);
            Some(())
        });
        self.send_with(false, send);
    }

    /// 間引かずに今の値を送る（報告の前の最後の 1 回）
    pub fn flush(&self, send: &dyn Fn(i64, &ProgressRequest) -> ProgressSent) {
        self.send_with(true, send);
    }

    fn send_with(&self, force: bool, send: &dyn Fn(i64, &ProgressRequest) -> ProgressSent) {
        // 送っている間は借用も門も持たない
        let Some((plan_id, req)) = self.claim(force, true) else {
            return;
        };
        let sent = send(plan_id, &req);
        self.with(|i| {
            // 間隔は送信が終わった時刻から数える。送信が詰まっても、次の書き込みで即座に送り直さない
            let now = (i.clock)();
            i.gate.lock().ok()?.record(now, sent);
            Some(())
        });
    }

    /// 終わりの 1 行を sink へ出し、その行を返す（開始していなければ None）
    pub fn finish(&self) -> Option<String> {
        self.with(|i| {
            let now = (i.clock)();
            let s = i.started.as_ref()?;
            let elapsed = now.saturating_sub(s.t0).as_secs();
            let line = summary_line(s.done, s.received.max(s.bytes_done), elapsed);
            i.close();
            i.sink.clear();
            i.sink.summary(&line);
            Some(line)
        })
    }

    pub fn clear(&self) {
        self.with(|i| {
            i.close();
            i.sink.clear();
            Some(())
        });
    }
}

impl Inner {
    /// 間引いて描く。描いたら true
    fn maybe_draw(&mut self, force: bool) -> bool {
        let now = (self.clock)();
        let Some(s) = self.started.as_mut() else {
            return false;
        };
        let t = now.saturating_sub(s.t0);
        if !force {
            if let Some(last) = s.last_draw {
                if t >= last && t - last < DRAW_INTERVAL {
                    return false;
                }
            }
        }
        s.last_draw = Some(t);
        let v = s.view(now);
        self.sink.draw(&v);
        true
    }

    /// 値が変わったとき。間引いて描き、描いたとき（か `publish`）は心拍が送る値も書き直す
    fn touch(&mut self, publish: bool) {
        if self.maybe_draw(false) || publish {
            let now = (self.clock)();
            let Some(snap) = self.started.as_ref().map(|s| s.request(now)) else {
                return;
            };
            if let Ok(mut g) = self.gate.lock() {
                g.snapshot = Some(snap);
            }
        }
    }

    /// 計画を終えた。心拍はもう送らない
    fn close(&mut self) {
        if let Ok(mut g) = self.gate.lock() {
            g.open = false;
        }
    }
}

impl Started {
    fn request(&self, now: Duration) -> (i64, ProgressRequest) {
        let v = self.view(now);
        (
            self.plan_id,
            ProgressRequest {
                generation: self.generation,
                done: v.done,
                total: v.total,
                bytes_done: v.bytes_done,
                bytes_total: v.bytes_total,
                rate: v.rate,
                phase: v.phase,
                current: v.current.map(|c| truncate_bytes(&c, PROGRESS_CURRENT_MAX)),
            },
        )
    }

    fn rate(&self, now: Duration) -> u64 {
        let t = now.saturating_sub(self.t0);
        if t.is_zero() {
            return 0;
        }
        let edge = t.saturating_sub(RATE_WINDOW);
        let (bt, bb) = if t < RATE_WINDOW {
            (Duration::ZERO, 0)
        } else {
            // 窓の端以前で最後の標本。無ければ開始
            self.samples
                .iter()
                .rev()
                .find(|(st, _)| *st <= edge)
                .copied()
                .unwrap_or((Duration::ZERO, 0))
        };
        let dt = t.saturating_sub(bt).as_secs_f64();
        if dt <= 0.0 {
            return 0;
        }
        (self.received.saturating_sub(bb) as f64 / dt).round() as u64
    }

    fn view(&self, now: Duration) -> ProgressView {
        let rate = self.rate(now);
        let eta_secs = if self.bytes_total > 0 && rate > 0 {
            Some(self.bytes_total.saturating_sub(self.bytes_done) / rate)
        } else {
            None
        };
        ProgressView {
            done: self.done,
            total: self.total,
            bytes_done: self.bytes_done,
            bytes_total: self.bytes_total,
            rate,
            eta_secs,
            phase: self.phase,
            current: self.current.clone(),
        }
    }
}

fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

pub fn format_rate(bps: u64) -> String {
    format!("{}/s", format_bytes(bps))
}

pub fn format_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn phase_verb(p: ProgressPhase) -> &'static str {
    match p {
        ProgressPhase::Delete => "削除中",
        ProgressPhase::Move => "移動中",
        ProgressPhase::Fetch => "受信中",
        ProgressPhase::Add => "追加中",
        ProgressPhase::Playlist => "プレイリスト",
    }
}

fn char_width(c: char) -> usize {
    let u = c as u32;
    let wide = matches!(u,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F | 0x1F900..=0x1F9FF | 0x20000..=0x3FFFD);
    if wide {
        2
    } else {
        1
    }
}

/// 端末での表示幅（全角・かなを 2 として数える）
pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// 項目を前から省くときに最低限残したい表示幅
const MIN_CURRENT_WIDTH: usize = 8;

fn head_line(v: &ProgressView, level: usize) -> String {
    // level 0: 全部 / 1: 残り時間を省く / 2: 速度も省く / 3: バイト数も省く
    let mut rest: Vec<String> = Vec::new();
    if v.bytes_total > 0 && level < 3 {
        rest.push(format!(
            "{} / {}",
            format_bytes(v.bytes_done),
            format_bytes(v.bytes_total)
        ));
        if level < 2 {
            rest.push(format_rate(v.rate));
        }
        if let (Some(e), 0) = (v.eta_secs, level) {
            rest.push(format!("残り {}", format_duration(e)));
        }
    }
    rest.push(phase_verb(v.phase).to_string());
    format!("[{}/{}] {}", v.done, v.total, rest.join("  "))
}

/// 1 行に整形する。`width` に収まるよう今の項目を前から省く（`…` を前置）。
/// それでも足りないときは、残り時間・速度・バイト数の順に省く
pub fn render_line(v: &ProgressView, width: usize) -> String {
    let Some(cur) = v.current.as_deref() else {
        return head_line(v, 0);
    };
    let cur_w = display_width(cur);
    for level in 0..=3 {
        let head = format!("{}: ", head_line(v, level));
        let hw = display_width(&head);
        if hw + cur_w <= width {
            return format!("{head}{cur}");
        }
        let avail = width.saturating_sub(hw + 1);
        if avail >= MIN_CURRENT_WIDTH || level == 3 {
            if avail == 0 {
                return head_line(v, level);
            }
            let mut tail: Vec<char> = Vec::new();
            let mut w = 0;
            for c in cur.chars().rev() {
                let cw = char_width(c);
                if w + cw > avail {
                    break;
                }
                w += cw;
                tail.push(c);
            }
            tail.reverse();
            return format!("{head}…{}", tail.into_iter().collect::<String>());
        }
    }
    head_line(v, 3)
}

pub fn summary_line(done: u64, bytes: u64, elapsed_secs: u64) -> String {
    if bytes == 0 {
        return format!("完了: {done} 件を {} で", format_duration(elapsed_secs));
    }
    let avg = bytes / elapsed_secs.max(1);
    format!(
        "完了: {done} 件（{}）を {} で（平均 {}）",
        format_bytes(bytes),
        format_duration(elapsed_secs),
        format_rate(avg)
    )
}

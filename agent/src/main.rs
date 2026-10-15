//! spindle-agent の CLI。引数の誤りは終了コード 2、`Error` は「エラー: …」を stderr に出して 1。
//! macOS ではミュージック.app を JXA（`JxaMusic`）で操作し、トークンは Keychain に置く。
//! それ以外の OS では `UnsupportedMusic`（全操作が失敗する）とファイルのトークン。
//! `SPINDLE_AGENT_SECRETS=file` なら macOS でもトークンをファイルに置く（試験と、Keychain が使えないときの逃げ道）

use std::cell::RefCell;
use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_proto::ManifestResponse;
use spindle_agent::ctx::Ui;
use spindle_agent::failpoint::Failpoints;
#[cfg(target_os = "macos")]
use spindle_agent::music::jxa::{JxaMusic, ProcessOsascript};
#[cfg(not(target_os = "macos"))]
use spindle_agent::music::UnsupportedMusic;
use spindle_agent::progress::{render_line, Progress, ProgressSent, ProgressSink, ProgressView};
use spindle_agent::recover::Resolution;
#[cfg(target_os = "macos")]
use spindle_agent::secrets::keychain::{KeychainSecrets, ACCOUNT, SERVICE};
use spindle_agent::secrets::{FileSecrets, Secrets};
use spindle_agent::server::{HttpServer, Server};
use spindle_agent::state::StateFile;
use spindle_agent::sync::{self, Paths, SyncOutcome};
use spindle_agent::{Error, Result};

const USAGE: &str = "使い方:
  spindle-agent pair <URL> <コード> [--insecure-http] [--adopt]
  spindle-agent sync
  spindle-agent status
  spindle-agent resolve <op_id> (--delete-track <persistent_id> | --no-copy-created)
  spindle-agent abandon";

enum Command {
    Pair {
        url: String,
        code: String,
        insecure_http: bool,
        adopt: bool,
    },
    Sync,
    Status,
    Resolve {
        op_id: String,
        choice: Resolution,
    },
    Abandon,
}

fn parse(args: &[String]) -> Option<Command> {
    let rest: Vec<&str> = args.iter().map(String::as_str).collect();
    match rest.as_slice() {
        ["pair", tail @ ..] => {
            let insecure_http = tail.contains(&"--insecure-http");
            let adopt = tail.contains(&"--adopt");
            let pos: Vec<&str> = tail
                .iter()
                .copied()
                .filter(|a| *a != "--insecure-http" && *a != "--adopt")
                .collect();
            if pos.iter().any(|a| a.starts_with("--")) {
                return None;
            }
            match pos.as_slice() {
                [url, code] => Some(Command::Pair {
                    url: (*url).to_owned(),
                    code: (*code).to_owned(),
                    insecure_http,
                    adopt,
                }),
                _ => None,
            }
        }
        ["sync"] => Some(Command::Sync),
        ["status"] => Some(Command::Status),
        ["abandon"] => Some(Command::Abandon),
        ["resolve", op_id, "--delete-track", pid] => Some(Command::Resolve {
            op_id: (*op_id).to_owned(),
            choice: Resolution::DeleteTrack((*pid).to_owned()),
        }),
        ["resolve", op_id, "--no-copy-created"] => Some(Command::Resolve {
            op_id: (*op_id).to_owned(),
            choice: Resolution::NoCopyCreated,
        }),
        _ => None,
    }
}

/// TTY でないときに 1 行出す間隔（時間）
const PLAIN_EVERY: Duration = Duration::from_secs(10);
/// TTY でないときに 1 行出す間隔（件数）
const PLAIN_EVERY_ITEMS: u64 = 10;
/// 端末の幅が取れないときの桁数
const DEFAULT_WIDTH: usize = 80;
/// TTY でないときの行は省かない
const PLAIN_WIDTH: usize = 4096;

/// 標準エラーの進捗の行。`StdUi` と進捗の sink が共有し、メッセージの前に行を消す
struct TermLine {
    tty: bool,
    /// 上書きする行が今出ている
    shown: bool,
    /// TTY でないときに最後に出した（時刻, 件数）
    last_plain: Option<(Instant, u64)>,
}

impl TermLine {
    fn new() -> Self {
        Self {
            tty: std::io::stderr().is_terminal(),
            shown: false,
            last_plain: None,
        }
    }

    fn clear(&mut self) {
        if self.shown {
            let mut e = std::io::stderr();
            let _ = write!(e, "\r\x1b[2K");
            let _ = e.flush();
            self.shown = false;
        }
    }

    fn draw(&mut self, v: &ProgressView) {
        let mut e = std::io::stderr();
        if self.tty {
            // 最後の桁まで書くと折り返す端末があるので 1 桁残す
            let line = render_line(v, term_width().saturating_sub(1));
            let _ = write!(e, "\r\x1b[2K{line}");
            let _ = e.flush();
            self.shown = true;
        } else {
            let now = Instant::now();
            if plain_due(self.last_plain, now, v.done) {
                self.last_plain = Some((now, v.done));
                let _ = writeln!(e, "{}", render_line(v, PLAIN_WIDTH));
            }
        }
    }
}

/// TTY でないとき、前に出してから 10 秒経ったか 10 件進んだら出す（最初の 1 回は出す）
fn plain_due(last: Option<(Instant, u64)>, now: Instant, done: u64) -> bool {
    match last {
        None => true,
        Some((t, d)) => {
            now.saturating_duration_since(t) >= PLAIN_EVERY
                || done >= d.saturating_add(PLAIN_EVERY_ITEMS)
        }
    }
}

/// 標準エラーの端末の桁数。取れなければ `COLUMNS`、それも無ければ 80
fn term_width() -> usize {
    if let Ok(ws) = rustix::termios::tcgetwinsize(std::io::stderr()) {
        if ws.ws_col > 0 {
            return usize::from(ws.ws_col);
        }
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.trim().parse::<usize>().ok())
        .filter(|w| *w > 0)
        .unwrap_or(DEFAULT_WIDTH)
}

/// 進捗を標準エラーへ描く sink
struct CliSink(Rc<RefCell<TermLine>>);

impl ProgressSink for CliSink {
    fn draw(&mut self, v: &ProgressView) {
        if let Ok(mut t) = self.0.try_borrow_mut() {
            t.draw(v);
        }
    }

    fn clear(&mut self) {
        if let Ok(mut t) = self.0.try_borrow_mut() {
            t.clear();
        }
    }

    fn summary(&mut self, line: &str) {
        self.clear();
        println!("{line}");
    }
}

/// 標準入出力の Ui。出す前に進捗の行を消す（次の更新で描き直される）
struct StdUi {
    term: Rc<RefCell<TermLine>>,
}

impl StdUi {
    fn clear_line(&self) {
        if let Ok(mut t) = self.term.try_borrow_mut() {
            t.clear();
        }
    }
}

impl Ui for StdUi {
    fn info(&mut self, msg: &str) {
        self.clear_line();
        println!("{msg}");
    }

    fn confirm(&mut self, m: &ManifestResponse) -> bool {
        self.clear_line();
        println!("{}", sync::render_diff(m));
        print!("反映しますか？ [y/N] ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim(), "y" | "Y" | "yes")
    }
}

/// sync の進捗。時計は起動からの単調な経過。
/// 心拍のスレッドを添え、項目が長く掛かっても 2 秒ごとに spindle へ送り続ける（D-108）。
/// 心拍は同じ基準の時計を使い、返り値のスレッドを落とすと止まる
fn cli_progress(
    term: &Rc<RefCell<TermLine>>,
    server: &Arc<HttpServer>,
) -> (Progress, Option<spindle_agent::progress::HeartbeatThread>) {
    let t0 = Instant::now();
    let p = Progress::new(
        Box::new(CliSink(term.clone())),
        Box::new(move || t0.elapsed()),
    );
    let server = server.clone();
    let hb = p.heartbeat().map(|h| {
        h.spawn(
            move || t0.elapsed(),
            // 送信の失敗は同期を止めない（エンジンの送信と同じく `Failed` として数える）
            move |id, r| server.progress(id, r).unwrap_or(ProgressSent::Failed),
        )
    });
    (p, hb)
}

#[cfg(target_os = "macos")]
fn open_music() -> JxaMusic<ProcessOsascript> {
    JxaMusic::new(ProcessOsascript::new())
}

#[cfg(not(target_os = "macos"))]
fn open_music() -> UnsupportedMusic {
    UnsupportedMusic
}

/// トークンの置き場。macOS は Keychain、それ以外（と `SPINDLE_AGENT_SECRETS=file`）はファイル
fn open_secrets(paths: &Paths) -> Box<dyn Secrets> {
    #[cfg(target_os = "macos")]
    if std::env::var("SPINDLE_AGENT_SECRETS").as_deref() != Ok("file") {
        return Box::new(KeychainSecrets::new(SERVICE, ACCOUNT));
    }
    Box::new(FileSecrets::new(&paths.state_dir))
}

/// pair 済みの state の URL とトークンで spindle に繋ぐ。読むのは URL・insecure_http・トークンだけなので
/// ロックは取らない（state を扱う段取りは各コマンドがロックを取ってから読み直す）
fn paired_server(paths: &Paths) -> Result<HttpServer> {
    let state = StateFile::new(&paths.state_dir).load()?;
    let Some(info) = state.server else {
        return Err(Error::Stop(
            "pair されていません。spindle-agent pair <URL> <コード> を実行してください".to_owned(),
        ));
    };
    let token = open_secrets(paths).get()?;
    HttpServer::new(&info.url, info.insecure_http, token)
}

fn run(cmd: Command) -> Result<()> {
    let paths = Paths::from_env()?;
    let music = open_music();
    let fp = Failpoints::none();
    let term = Rc::new(RefCell::new(TermLine::new()));
    let mut ui = StdUi { term: term.clone() };
    match cmd {
        Command::Pair {
            url,
            code,
            insecure_http,
            adopt,
        } => {
            let server = HttpServer::new(&url, insecure_http, None)?;
            let secrets = open_secrets(&paths);
            sync::pair_cmd(
                &music,
                &server,
                secrets.as_ref(),
                &paths,
                &fp,
                &mut ui,
                &url,
                insecure_http,
                &code,
                adopt,
            )
        }
        Command::Sync => {
            let server = Arc::new(paired_server(&paths)?);
            let (progress, _heartbeat) = cli_progress(&term, &server);
            match sync::sync(&music, &*server, &paths, &fp, &mut ui, progress)? {
                SyncOutcome::NothingToDo | SyncOutcome::PendingReevaluation => {}
                SyncOutcome::Declined => ui.info("反映しませんでした"),
                SyncOutcome::Applied {
                    executed,
                    dropped,
                    errors,
                } => ui.info(&format!(
                    "反映しました（実行 {executed} 件、見送り {dropped} 件、エラー {errors} 件）"
                )),
            }
            Ok(())
        }
        Command::Status => {
            ui.info(&sync::status(&paths)?);
            Ok(())
        }
        Command::Resolve { op_id, choice } => {
            let server = paired_server(&paths)?;
            sync::resolve_cmd(&music, &server, &paths, &fp, &mut ui, &op_id, choice)
        }
        Command::Abandon => {
            let server = paired_server(&paths)?;
            sync::abandon(&music, &server, &paths, &fp, &mut ui)
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = parse(&args) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    match run(cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("エラー: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_lines_every_ten_seconds_or_items() {
        let t0 = Instant::now();
        assert!(plain_due(None, t0, 0));
        let last = Some((t0, 3));
        assert!(!plain_due(last, t0 + Duration::from_secs(9), 12));
        assert!(plain_due(last, t0 + Duration::from_secs(10), 3));
        assert!(plain_due(last, t0 + Duration::from_secs(1), 13));
    }
}

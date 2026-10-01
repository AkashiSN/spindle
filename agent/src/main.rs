//! spindle-agent の CLI。引数の誤りは終了コード 2、`Error` は「エラー: …」を stderr に出して 1。
//! ミュージック.app は P5-4c まで `UnsupportedMusic`（全操作が失敗する）

use std::io::{BufRead, Write};
use std::process::ExitCode;

use agent_proto::ManifestResponse;
use spindle_agent::ctx::Ui;
use spindle_agent::failpoint::Failpoints;
use spindle_agent::music::UnsupportedMusic;
use spindle_agent::recover::Resolution;
use spindle_agent::secrets::{FileSecrets, Secrets};
use spindle_agent::server::HttpServer;
use spindle_agent::state::StateFile;
use spindle_agent::sync::{self, Paths, SyncOutcome};
use spindle_agent::{Error, Result};

const USAGE: &str = "使い方:
  spindle-agent pair <URL> <コード> [--insecure-http]
  spindle-agent sync
  spindle-agent status
  spindle-agent resolve <op_id> (--delete-track <persistent_id> | --no-copy-created)
  spindle-agent abandon";

enum Command {
    Pair {
        url: String,
        code: String,
        insecure_http: bool,
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
            let pos: Vec<&str> = tail
                .iter()
                .copied()
                .filter(|a| *a != "--insecure-http")
                .collect();
            if pos.iter().any(|a| a.starts_with("--")) {
                return None;
            }
            match pos.as_slice() {
                [url, code] => Some(Command::Pair {
                    url: (*url).to_owned(),
                    code: (*code).to_owned(),
                    insecure_http,
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

/// 標準入出力の Ui
struct StdUi;

impl Ui for StdUi {
    fn info(&mut self, msg: &str) {
        println!("{msg}");
    }

    fn confirm(&mut self, m: &ManifestResponse) -> bool {
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

/// pair 済みの state の URL とトークンで spindle に繋ぐ。読むのは URL・insecure_http・トークンだけなので
/// ロックは取らない（state を扱う段取りは各コマンドがロックを取ってから読み直す）
fn paired_server(paths: &Paths) -> Result<HttpServer> {
    let state = StateFile::new(&paths.state_dir).load()?;
    let Some(info) = state.server else {
        return Err(Error::Stop(
            "pair されていません。spindle-agent pair <URL> <コード> を実行してください".to_owned(),
        ));
    };
    let token = FileSecrets::new(&paths.state_dir).get()?;
    HttpServer::new(&info.url, info.insecure_http, token)
}

fn run(cmd: Command) -> Result<()> {
    let paths = Paths::from_env()?;
    let music = UnsupportedMusic;
    let fp = Failpoints::none();
    let mut ui = StdUi;
    match cmd {
        Command::Pair {
            url,
            code,
            insecure_http,
        } => {
            let server = HttpServer::new(&url, insecure_http, None)?;
            let secrets = FileSecrets::new(&paths.state_dir);
            sync::pair_cmd(
                &music,
                &server,
                &secrets,
                &paths,
                &fp,
                &mut ui,
                &url,
                insecure_http,
                &code,
            )
        }
        Command::Sync => {
            let server = paired_server(&paths)?;
            match sync::sync(&music, &server, &paths, &fp, &mut ui)? {
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

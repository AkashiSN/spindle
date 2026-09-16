//! 実プロセスでの結合テスト（P0-4 受け入れ条件の起動側）:
//! 「実行中に kill された」状態（`running` ジョブ + `track_locks`、cancel 要求済みの行）を
//! DB に用意してから**実バイナリを起動**し、起動時リカバリの結果を確認する。SSE を接続した
//! まま SIGTERM を送っても所定時間内に終了することも見る（SSE がサーバの停止を塞がない）。
//!
//! 「ワーカーが実行中のプロセスを kill → 再起動でハンドラが再開する」ところまでは
//! ここでは検証しない（本体バイナリにはまだハンドラが無い）。再キューされた queued を
//! ワーカーが拾って完走する経路は tests/jobs.rs の
//! `recovery_requeues_running_jobs_and_clears_locks` が同一プロセス内で検証する。
//!
//! HTTP は生の TCP で喋る（依存を増やさない。/health と SSE のヘッダだけ読めればよい）

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use spindle::db::Db;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");

struct Server {
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// 一時ディレクトリに設定を書き、DB を先に作って返す
fn prepare(root: &Path, port: u16) -> std::path::PathBuf {
    let mut cfg: toml::Table = toml::from_str(EXAMPLE).unwrap();
    let paths = cfg.get_mut("paths").unwrap().as_table_mut().unwrap();
    for key in [
        "library",
        "derived",
        "archive",
        "inbox",
        "playlists",
        "data",
    ] {
        let dir = root.join(key);
        std::fs::create_dir_all(&dir).unwrap();
        paths.insert(key.into(), toml::Value::String(dir.display().to_string()));
    }
    cfg.get_mut("server")
        .unwrap()
        .as_table_mut()
        .unwrap()
        .insert(
            "listen".into(),
            toml::Value::String(format!("127.0.0.1:{port}")),
        );
    let config_path = root.join("data/config.toml");
    std::fs::write(&config_path, toml::to_string(&cfg).unwrap()).unwrap();
    config_path
}

fn spawn(config_path: &Path) -> Server {
    let child = Command::new(env!("CARGO_BIN_EXE_spindle"))
        .env("SPINDLE_CONFIG", config_path)
        .env("SPINDLE_INITIAL_PASSWORD", "pw")
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("バイナリを起動できること");
    Server { child }
}

fn http_get(port: u16, path: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

fn wait_healthy(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = write!(
                s,
                "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
            );
            let mut out = String::new();
            if s.read_to_string(&mut out).is_ok() && out.contains("\"status\":\"ok\"") {
                return;
            }
        }
        assert!(Instant::now() < deadline, "/health が ok にならない");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// ログインして SSE を開き、レスポンスヘッダを読み終えた接続を返す
fn open_sse(port: u16) -> TcpStream {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let body = r#"{"password":"pw"}"#;
    write!(
        s,
        "POST /api/auth/login HTTP/1.1\r\nHost: 127.0.0.1\r\nSec-Fetch-Site: none\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    let cookie = out
        .lines()
        .find_map(|l| l.strip_prefix("set-cookie: "))
        .expect("Set-Cookie")
        .split(';')
        .next()
        .unwrap()
        .to_string();

    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        s,
        "GET /api/events HTTP/1.1\r\nHost: 127.0.0.1\r\nCookie: {cookie}\r\nAccept: text/event-stream\r\n\r\n"
    )
    .unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut reader = BufReader::new(s.try_clone().unwrap());
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    let mut content_type = None;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some(v) = line.strip_prefix("content-type: ") {
            content_type = Some(v.trim().to_string());
        }
    }
    assert!(
        content_type
            .as_deref()
            .is_some_and(|v| v.starts_with("text/event-stream")),
        "{content_type:?}"
    );
    s
}

fn wait_exit(child: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
fn startup_recovers_interrupted_state_in_db_and_sigterm_finishes_with_open_sse() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config_path = prepare(dir.path(), port);
    let db_path = dir.path().join("data/spindle.db");

    // 「実行中に kill された」状態を作る: running ジョブ + ロック、cancel 要求済みの running、
    // バックオフ中に cancel された queued
    {
        let db = Db::open(&db_path).unwrap();
        drop(db);
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                                 title, artist_display, album, albumartist, seen_at)
             VALUES (1, 'A/1.flac', 'A/1.flac', 0, 0, 0, 'flac', 1, 't', 'a', 'al', 'aa', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO jobs (id, type, dedup_key, payload, state, created_at, started_at)
             VALUES (1, 'scan', 'scan', '{}', 'running', 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO track_locks (track_id, job_id, acquired_at) VALUES (1, 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO jobs (id, type, dedup_key, payload, state, created_at, started_at, cancel_requested_at)
             VALUES (2, 'gc', 'gc', '{}', 'running', 1, 1, 2)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO jobs (id, type, dedup_key, payload, state, created_at, run_after, attempts, cancel_requested_at)
             VALUES (3, 'backup', 'backup', '{}', 'queued', 1, 0, 1, 2)",
            [],
        )
        .unwrap();
    }

    let mut server = spawn(&config_path);
    wait_healthy(port);

    // 起動時リカバリの結果
    let conn =
        Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let state = |id: i64| -> String {
        conn.query_row("SELECT state FROM jobs WHERE id = ?1", [id], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(state(1), "queued", "中断ジョブは queued に戻る");
    assert_eq!(
        state(2),
        "cancelled",
        "cancel 要求済みの running は cancelled"
    );
    let locks: i64 = conn
        .query_row("SELECT count(*) FROM track_locks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(locks, 0, "ロック表は空");
    // queued + cancel 要求は、ワーカーが claim する前に cancelled へ送られる
    // （ハンドラ未登録の種別でも、ワーカーの掃除が走れば cancelled になる）
    let deadline = Instant::now() + Duration::from_secs(5);
    while state(3) != "cancelled" && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(state(3), "cancelled");

    // 未認証の /api/jobs は 401
    assert!(http_get(port, "/api/jobs").starts_with("HTTP/1.1 401"));

    // SSE を開いたまま SIGTERM → 所定時間内に終了する
    let _sse = open_sse(port);
    let pid = rustix::process::Pid::from_raw(server.child.id() as i32).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    let status = wait_exit(&mut server.child, Duration::from_secs(8))
        .expect("SSE 接続中でも SIGTERM から 8 秒以内に終了すること");
    assert!(status.success(), "{status}");
}

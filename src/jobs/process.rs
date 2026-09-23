//! 外部プロセスを自分のプロセスグループで起動し、キャンセル時にグループごと kill する。
//! [`ExternalCommand`] が引数配列・`--` / `./` 前置・タイムアウト・終了コード検査・stderr の
//! ログを担い、[`ChildGroup`] の上に載る（SPEC §5「パスの表現と境界」、コーディング規約）。
//!
//! 「グループが消えたか」は leader の終了ではなく `killpg(pgid, 0)` で判定する。leader が
//! TERM で死んでも、TERM を無視する孫が残っていればグループはまだ存在する（D-36）。

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rustix::process::{kill_process_group, test_kill_process_group, Pid, Signal};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::JobError;

/// 外部コマンドの既定タイムアウト。エンコード等の長い処理は呼び出し側で伸ばす
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
/// エラーとログに残す stderr の末尾の上限（バイト）
const STDERR_KEEP: usize = 16 * 1024;
/// `stdout_channel` で流す 1 チャンクの大きさ
const STDOUT_CHUNK: usize = 64 * 1024;

/// パス引数の無害化方式。先頭 `-` のファイル名がオプションと解釈されるのを防ぐ
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathStyle {
    /// `--` 対応ツール（flac / opusenc / cat …）。最初のパスの前に `--` を 1 度だけ置く。
    /// **以降の引数はすべてオペランド**になるのでオプションはパスより前に並べる
    DoubleDash,
    /// `--` 非対応ツール（ffmpeg …）。先頭 `-` の相対パスに `./` を前置する
    DotSlash,
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("{program} を起動できない: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{program} が {after:?} 以内に終わらないので kill した")]
    Timeout { program: String, after: Duration },
    #[error("キャンセルされた")]
    Cancelled,
    /// `stdout_channel` の受け手が消えたので子を止めた。受け手側の失敗が本当の原因なので、
    /// 呼び出し側はそちらを優先して報告する
    #[error("{program} の stdout の受け手が消えたので kill した")]
    OutputAbandoned { program: String },
    #[error("{program} が {status} で終了: {stderr}")]
    Failed {
        program: String,
        status: ExitStatus,
        /// stderr の末尾（[`STDERR_KEEP`] バイトまで）
        stderr: String,
    },
    #[error("{program} の入出力エラー: {source}")]
    Io {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

impl From<ProcessError> for JobError {
    fn from(e: ProcessError) -> Self {
        match e {
            ProcessError::Cancelled => JobError::Cancelled,
            other => JobError::Failed(other.into()),
        }
    }
}

/// 正常終了したコマンドの結果
#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    /// `stdout_file` を指定した場合は空
    pub stdout: Vec<u8>,
    /// stderr の末尾（ログにも出している）
    pub stderr: String,
}

/// 引数配列で組み立てる外部コマンド。`sh -c` は使わない
#[derive(Debug)]
pub struct ExternalCommand {
    program: PathBuf,
    args: Vec<OsString>,
    path_style: PathStyle,
    double_dash_emitted: bool,
    current_dir: Option<PathBuf>,
    timeout: Duration,
    stdin: Option<File>,
    /// stdin に書き込むバイト列（`stdin` より優先）。書き終えたら閉じる
    stdin_bytes: Option<Vec<u8>>,
    stdout_file: Option<File>,
    stdout_channel: Option<mpsc::Sender<Vec<u8>>>,
    stderr_lines: Option<mpsc::UnboundedSender<String>>,
}

impl ExternalCommand {
    pub fn new(program: impl AsRef<Path>) -> Self {
        Self {
            program: program.as_ref().to_path_buf(),
            args: Vec::new(),
            path_style: PathStyle::DoubleDash,
            double_dash_emitted: false,
            current_dir: None,
            timeout: DEFAULT_TIMEOUT,
            stdin: None,
            stdin_bytes: None,
            stdout_file: None,
            stdout_channel: None,
            stderr_lines: None,
        }
    }

    pub fn path_style(mut self, style: PathStyle) -> Self {
        self.path_style = style;
        self
    }

    /// オプション等のパスでない引数
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for a in args {
            self.args.push(a.as_ref().to_os_string());
        }
        self
    }

    /// ファイルパスの引数。[`PathStyle`] に従って先頭 `-` を無害化する
    pub fn path_arg(mut self, path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        match self.path_style {
            PathStyle::DoubleDash => {
                if !self.double_dash_emitted {
                    self.args.push(OsString::from("--"));
                    self.double_dash_emitted = true;
                }
                self.args.push(path.as_os_str().to_os_string());
            }
            PathStyle::DotSlash => {
                let starts_with_dash = path
                    .as_os_str()
                    .as_encoded_bytes()
                    .first()
                    .is_some_and(|b| *b == b'-');
                if path.is_relative() && starts_with_dash {
                    self.args.push(Path::new(".").join(path).into_os_string());
                } else {
                    self.args.push(path.as_os_str().to_os_string());
                }
            }
        }
        self
    }

    pub fn current_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.current_dir = Some(dir.as_ref().to_path_buf());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 開いたファイルを stdin に繋ぐ（パスを渡さずに済む場合はこちらを優先する）
    pub fn stdin_file(mut self, file: File) -> Self {
        self.stdin = Some(file);
        self
    }

    /// stdin にバイト列を書いて閉じる（小さな入力用。JSON のリクエストなど）
    pub fn stdin_bytes(mut self, bytes: Vec<u8>) -> Self {
        self.stdin_bytes = Some(bytes);
        self
    }

    /// stdout を開いたファイルへ書く。指定しなければメモリに取り込む
    pub fn stdout_file(mut self, file: File) -> Self {
        self.stdout_file = Some(file);
        self
    }

    /// stdout をチャンクごとにチャネルへ流す（メモリに溜めない。ffmpeg の PCM 出力など
    /// 大きな出力用）。受け手が消えたら子をグループごと止め [`ProcessError::OutputAbandoned`]
    /// （SIGPIPE を無視する子でもタイムアウトまで待たない）
    pub fn stdout_channel(mut self, tx: mpsc::Sender<Vec<u8>>) -> Self {
        self.stdout_channel = Some(tx);
        self
    }

    /// stderr を 1 行ずつ（改行・CR で区切り、区切りは落とす）チャネルへも流す（進捗を出すツール用。
    /// 例: `cd-paranoia -e`）。末尾の保持とログはそのまま。受け手が消えても子は止めない
    pub fn stderr_lines(mut self, tx: mpsc::UnboundedSender<String>) -> Self {
        self.stderr_lines = Some(tx);
        self
    }

    /// 組み立てた引数（テストと診断用）
    pub fn arg_list(&self) -> Vec<String> {
        self.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn program_name(&self) -> String {
        self.program
            .file_name()
            .unwrap_or(self.program.as_os_str())
            .to_string_lossy()
            .into_owned()
    }

    /// 起動して終了まで待つ。終了コードが 0 でなければ [`ProcessError::Failed`]。
    /// `token` が倒れるかタイムアウトしたらプロセスグループごと kill する
    pub async fn run(self, token: &CancellationToken) -> Result<Output, ProcessError> {
        let program = self.program_name();
        let timeout = self.timeout;
        tracing::debug!(program = %self.program.display(), args = ?self.arg_list(), "外部コマンドを起動");

        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args).stderr(Stdio::piped());
        cmd.stdin(match (&self.stdin_bytes, self.stdin) {
            (Some(_), _) => Stdio::piped(),
            (None, Some(f)) => Stdio::from(f),
            (None, None) => Stdio::null(),
        });
        let capture_stdout = self.stdout_file.is_none();
        cmd.stdout(match self.stdout_file {
            Some(f) => Stdio::from(f),
            None => Stdio::piped(),
        });
        if let Some(dir) = &self.current_dir {
            cmd.current_dir(dir);
        }
        let mut child = ChildGroup::spawn(cmd).map_err(|source| ProcessError::Spawn {
            program: program.clone(),
            source,
        })?;
        let io_err = |source| ProcessError::Io {
            program: program.clone(),
            source,
        };
        let mut stderr_pipe = child
            .child_mut()
            .stderr
            .take()
            .ok_or_else(|| io_err(std::io::Error::other("stderr を取れない")))?;
        let mut stdout_pipe = if capture_stdout {
            child.child_mut().stdout.take()
        } else {
            None
        };
        let stdin_pipe = self
            .stdin_bytes
            .map(|bytes| (child.child_mut().stdin.take(), bytes));

        // タイムアウトは子 token で表現し、親 token の cancel と区別する
        let local = token.child_token();
        let timer = {
            let local = local.clone();
            tokio::spawn(async move {
                tokio::time::sleep(timeout).await;
                local.cancel();
            })
        };
        let stderr_task = read_tail(&mut stderr_pipe, STDERR_KEEP, self.stderr_lines);
        let stdout_channel = self.stdout_channel;
        // 受け手が消えて子を止めたか（タイムアウトの cancel と区別する）
        let abandoned = std::sync::atomic::AtomicBool::new(false);
        let stdout_task = async {
            let mut buf = Vec::new();
            match (stdout_pipe.as_mut(), stdout_channel) {
                (Some(pipe), Some(tx)) => {
                    let mut chunk = vec![0u8; STDOUT_CHUNK];
                    loop {
                        let n = pipe.read(&mut chunk).await?;
                        if n == 0 {
                            break;
                        }
                        if tx.send(chunk[..n].to_vec()).await.is_err() {
                            // 受け手が消えた。子をグループごと止める（EPIPE 任せにしない）
                            abandoned.store(true, std::sync::atomic::Ordering::SeqCst);
                            local.cancel();
                            break;
                        }
                    }
                    drop(stdout_pipe.take());
                }
                (Some(pipe), None) => {
                    pipe.read_to_end(&mut buf).await?;
                }
                (None, _) => {}
            }
            Ok::<_, std::io::Error>(buf)
        };
        // stdin の書き込みは子が読まずに終わると EPIPE になる（Failed / stdout の判定に任せる）
        let stdin_task = async {
            if let Some((Some(mut pipe), bytes)) = stdin_pipe {
                use tokio::io::AsyncWriteExt as _;
                if let Err(e) = pipe.write_all(&bytes).await {
                    tracing::debug!(error = %e, "stdin へ書き切れなかった");
                }
                drop(pipe);
            }
        };
        let (waited, stderr_buf, stdout_buf, ()) = tokio::join!(
            child.wait(local.clone()),
            stderr_task,
            stdout_task,
            stdin_task
        );
        timer.abort();

        let stderr = String::from_utf8_lossy(&stderr_buf.map_err(io_err)?)
            .trim_end()
            .to_owned();
        let status = match waited {
            Ok(status) => status,
            Err(JobError::Cancelled) if token.is_cancelled() => {
                tracing::info!(program, "キャンセルで外部コマンドを止めた");
                return Err(ProcessError::Cancelled);
            }
            Err(JobError::Cancelled) if abandoned.load(std::sync::atomic::Ordering::SeqCst) => {
                tracing::warn!(
                    program,
                    stderr,
                    "stdout の受け手が消えたので外部コマンドを止めた"
                );
                return Err(ProcessError::OutputAbandoned { program });
            }
            Err(JobError::Cancelled) => {
                tracing::warn!(program, ?timeout, stderr, "外部コマンドがタイムアウト");
                return Err(ProcessError::Timeout {
                    program,
                    after: timeout,
                });
            }
            Err(JobError::Failed(e) | JobError::Fatal(e)) => {
                return Err(ProcessError::Io {
                    program,
                    source: std::io::Error::other(e),
                })
            }
        };
        if !status.success() {
            tracing::warn!(program, %status, stderr, "外部コマンドが失敗");
            return Err(ProcessError::Failed {
                program,
                status,
                stderr,
            });
        }
        if !stderr.is_empty() {
            tracing::debug!(program, stderr, "外部コマンドの stderr");
        }
        Ok(Output {
            status,
            stdout: stdout_buf.map_err(io_err)?,
            stderr,
        })
    }
}

/// 読みながら末尾 `keep` バイトだけを保持する（全量を溜めない。長時間の ffmpeg が大量の
/// stderr を出してもメモリは有界）。EOF まで読む。`lines` があれば行ごとにも流す
async fn read_tail<R: tokio::io::AsyncRead + Unpin>(
    pipe: &mut R,
    keep: usize,
    lines: Option<mpsc::UnboundedSender<String>>,
) -> std::io::Result<Vec<u8>> {
    let mut tail: Vec<u8> = Vec::with_capacity(keep.min(64 * 1024));
    let mut chunk = vec![0u8; 8 * 1024];
    let mut partial: Vec<u8> = Vec::new();
    loop {
        let n = pipe.read(&mut chunk).await?;
        if n == 0 {
            if let Some(tx) = &lines {
                if !partial.is_empty() {
                    let _ = tx.send(String::from_utf8_lossy(&partial).into_owned());
                }
            }
            return Ok(tail);
        }
        if let Some(tx) = &lines {
            for &b in &chunk[..n] {
                if b == b'\n' || b == b'\r' {
                    if !partial.is_empty() {
                        let _ = tx.send(String::from_utf8_lossy(&partial).into_owned());
                        partial.clear();
                    }
                } else if partial.len() < STDERR_KEEP {
                    // 改行の無い長大な出力でもメモリは有界（超えた分は捨てる）
                    partial.push(b);
                }
            }
        }
        tail.extend_from_slice(&chunk[..n]);
        if tail.len() > keep {
            let excess = tail.len() - keep;
            tail.drain(..excess);
        }
    }
}

/// SIGTERM 後にこれだけ待ってグループが残っていれば SIGKILL
const KILL_GRACE: Duration = Duration::from_secs(2);
/// SIGKILL 後にグループが消えるのを待つ上限（消えなければ諦めてログに残す）
const KILL_SETTLE: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(20);

pub struct ChildGroup {
    child: Child,
    pid: u32,
    /// leader を wait で回収済みか。回収後もグループにプロセスが残っていれば pgid は
    /// 再利用されない（参照中の pid は割り当てられない）ので `killpg` は安全だが、
    /// 回収済みなら Drop での無条件 SIGKILL はしない（送る相手が残っているとは限らない）
    reaped: bool,
}

impl ChildGroup {
    /// 新しいプロセスグループのリーダーとして起動する
    pub fn spawn(mut cmd: Command) -> std::io::Result<Self> {
        // kill_on_drop は leader だけが対象。グループは Drop で自前で kill する
        cmd.process_group(0).kill_on_drop(true);
        let child = cmd.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| std::io::Error::other("起動直後に pid が取れない"))?;
        Ok(Self {
            child,
            pid,
            reaped: false,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    fn pgid(&self) -> Option<Pid> {
        Pid::from_raw(self.pid as i32)
    }

    /// 終了を待つ。`token` が倒れたらプロセスグループを kill して `Err(Cancelled)`
    ///
    /// leader が終了しても同じグループに子孫が残っていれば（leader が fork して先に exit した）
    /// グループごと掃除してから返す。放置すると継承した stdout / stderr パイプの EOF が来ず
    /// 呼び出し側が固まるか、孫プロセスが漏れる
    pub async fn wait(mut self, token: CancellationToken) -> Result<ExitStatus, JobError> {
        let status = tokio::select! {
            status = self.child.wait() => {
                self.reaped = true;
                status?
            }
            _ = token.cancelled() => {
                self.kill_group().await;
                return Err(JobError::Cancelled);
            }
        };
        if let Some(pgid) = self.pgid() {
            if test_kill_process_group(pgid).is_ok() {
                tracing::warn!(
                    pid = self.pid,
                    "leader が終了したがプロセスグループに残りがある。掃除する"
                );
                self.kill_group().await;
            }
        }
        Ok(status)
    }

    /// プロセスグループへ SIGTERM、猶予後にグループが残っていれば SIGKILL。
    /// leader は zombie のままだと `killpg(pgid, 0)` で「存在」と数えられるので、待っている間も
    /// `try_wait` で回収し続ける。回収後にグループが残っていれば孫なので、そこへ SIGKILL を送る
    pub async fn kill_group(&mut self) {
        let Some(pgid) = self.pgid() else {
            return;
        };
        if let Err(e) = kill_process_group(pgid, Signal::TERM) {
            tracing::debug!(pid = self.pid, error = %e, "SIGTERM を送れない（既に終了か）");
        }
        if !self.wait_group_gone(pgid, KILL_GRACE).await {
            tracing::info!(pid = self.pid, "SIGTERM で終わらないのでグループへ SIGKILL");
            if let Err(e) = kill_process_group(pgid, Signal::KILL) {
                tracing::debug!(pid = self.pid, error = %e, "SIGKILL を送れない");
            }
            if !self.wait_group_gone(pgid, KILL_SETTLE).await {
                tracing::warn!(pid = self.pid, "SIGKILL 後もプロセスグループが残っている");
            }
        }
        // leader をまだ回収していなければここで必ず回収する
        if !self.reaped {
            let _ = self.child.wait().await;
            self.reaped = true;
        }
    }

    /// グループの全プロセスが消えるまで待つ。`within` 内に消えたら `true`。
    /// 待つ間に leader が終わっていれば回収する（zombie が残るとグループが消えない）
    async fn wait_group_gone(&mut self, pgid: Pid, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if !self.reaped {
                if let Ok(Some(_)) = self.child.try_wait() {
                    self.reaped = true;
                }
            }
            // ESRCH ならグループにプロセスがいない
            if test_kill_process_group(pgid).is_err() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

impl Drop for ChildGroup {
    /// wait を経ずに drop された（ハンドラの panic / 停止時の abort）ときはグループごと SIGKILL。
    /// leader を reap 済み（wait / kill_group を通った）なら後始末は済んでいるので何もしない
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        if let Some(pgid) = self.pgid() {
            if let Err(e) = kill_process_group(pgid, Signal::KILL) {
                tracing::debug!(pid = self.pid, error = %e, "drop 時の SIGKILL を送れない");
            }
        }
    }
}

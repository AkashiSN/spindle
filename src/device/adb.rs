//! adb による [`DeviceFs`]（仕様 ⑤「接続の構成」）。`adb -s <serial> shell <script>` を引数配列で
//! 呼ぶ（ローカルでは `sh -c` を使わない）。端末側のスクリプトに埋め込むパスは `sh_quote` だけを通す。
//! サブコマンドは shell v2 の `shell` だけ: pty を割り当てない限り stdin / stdout がバイナリ安全で、
//! 終了コードも返る（`exec-in` / `exec-out` は終了コードを返さない。D-95 の P5-3a 追記）。
//! ジャーナルの行・転送する中身は stdin で流し、コマンド長の上限に当たらないようにする

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::device::quote::{sh_quote, valid_serial};
use crate::device::remote::{DeviceFs, DirState, PutResult, RemoteError, RemoteFile, RemoteResult};
use crate::domain::relpath::RelPath;
use crate::jobs::process::{ExternalCommand, Output, ProcessError};

/// stdin を流すサブコマンド。実機で shell v2 の stdin が使えなければここを差し替える（P5-3b の実機確認）
pub const STDIN_SUBCOMMAND: &str = "shell";
pub const POWERAMP_PACKAGE: &str = "com.maxmpz.audioplayer";
pub const POWERAMP_SCAN_ACTION: &str = "com.maxmpz.audioplayer.ACTION_SCAN_DIRS";
/// Poweramp の API レシーバ（P5-3b の実機確認で固定する）
pub const POWERAMP_RECEIVER: &str = "com.maxmpz.audioplayer/.player.PowerampAPIReceiver";
/// 「ファイルが無い」を表す端末側スクリプトの終了コード
const MISSING_EXIT: i32 = 3;

#[derive(Debug, Clone)]
pub struct AdbConfig {
    /// adb クライアントのパス（`[bin].adb`）
    pub program: PathBuf,
    /// `ADB_SERVER_SOCKET` の値（`[devices].adb_server`。例 `tcp:adb:5037`）
    pub server: String,
    /// 1 回で終わるコマンドのタイムアウト
    pub timeout: Duration,
    /// 転送（`put`）のタイムアウト
    pub transfer_timeout: Duration,
}

pub struct AdbFs {
    cfg: AdbConfig,
    serial: String,
    root: String,
    token: CancellationToken,
}

impl AdbFs {
    /// `root_abs` は `quote::root_abs` で作った端末上の絶対パス。
    /// `token` はプロセスの停止（shutdown）用で、ジョブのキャンセルトークンを渡してはならない。
    /// これが引かれると実行中の adb を含むすべての操作が止まり、封印済みバッチの `vacating` 以降の
    /// `mv` まで止めてしまう（次の接続の回復で前進はするが、同期の中では完遂できない）。
    /// ジョブのキャンセルは `sync::Control::cancelled` だけで効かせる（P5-3b）
    pub fn new(
        cfg: AdbConfig,
        serial: &str,
        root_abs: &str,
        token: CancellationToken,
    ) -> Result<Self, RemoteError> {
        if !valid_serial(serial) {
            return Err(RemoteError::Failed(format!(
                "adb のシリアルが不正: {serial:?}"
            )));
        }
        let root_ok = root_abs
            .strip_prefix('/')
            .is_some_and(|r| RelPath::parse(r.trim_end_matches('/')).is_ok());
        if !root_ok {
            return Err(RemoteError::Failed(format!(
                "端末の root が不正: {root_abs:?}"
            )));
        }
        sh_quote(root_abs).map_err(|e| RemoteError::Failed(e.to_string()))?;
        Ok(Self {
            cfg,
            serial: serial.to_owned(),
            root: root_abs.trim_end_matches('/').to_owned(),
            token,
        })
    }

    fn quoted_root(&self) -> RemoteResult<String> {
        sh_quote(&self.root).map_err(|e| RemoteError::Failed(e.to_string()))
    }

    /// root 相対のパスを検証して、クォート済みの絶対パスにする
    fn abs(&self, rel: &str) -> RemoteResult<String> {
        RelPath::parse(rel)
            .map_err(|e| RemoteError::Failed(format!("端末上のパスが不正（{e}）: {rel:?}")))?;
        sh_quote(&format!("{}/{rel}", self.root)).map_err(|e| RemoteError::Failed(e.to_string()))
    }

    /// 親ディレクトリ（クォート済み）
    fn parent(&self, rel: &str) -> RemoteResult<String> {
        match rel.rsplit_once('/') {
            Some((dir, _)) => self.abs(dir),
            None => self.quoted_root(),
        }
    }

    fn command(&self, sub: &str, script: String, timeout: Duration) -> ExternalCommand {
        ExternalCommand::new(&self.cfg.program)
            .env("ADB_SERVER_SOCKET", &self.cfg.server)
            .arg("-s")
            .arg(&self.serial)
            .arg(sub)
            .arg(script)
            .timeout(timeout)
    }

    async fn shell(&self, script: String) -> RemoteResult<Output> {
        self.command("shell", script, self.cfg.timeout)
            .run(&self.token)
            .await
            .map_err(map_err)
    }

    /// 終了コード [`MISSING_EXIT`] を None にする
    async fn shell_opt(&self, script: String) -> RemoteResult<Option<Output>> {
        match self
            .command("shell", script, self.cfg.timeout)
            .run(&self.token)
            .await
        {
            Ok(o) => Ok(Some(o)),
            Err(ProcessError::Failed { status, .. }) if status.code() == Some(MISSING_EXIT) => {
                Ok(None)
            }
            Err(e) => Err(map_err(e)),
        }
    }

    async fn shell_stdin(&self, script: String, bytes: Vec<u8>) -> RemoteResult<()> {
        self.command(STDIN_SUBCOMMAND, script, self.cfg.timeout)
            .stdin_bytes(bytes)
            .run(&self.token)
            .await
            .map_err(map_err)?;
        Ok(())
    }
}

fn map_err(e: ProcessError) -> RemoteError {
    match e {
        ProcessError::Cancelled => RemoteError::Cancelled,
        ProcessError::Failed { stderr, .. } if stderr.contains("No space left") => {
            RemoteError::NoSpace
        }
        ProcessError::Failed { stderr, .. } if is_disconnected(&stderr) => {
            RemoteError::NotConnected
        }
        other => RemoteError::Failed(other.to_string()),
    }
}

fn is_disconnected(stderr: &str) -> bool {
    stderr.lines().any(|l| {
        let l = l.trim_start_matches("* ").trim();
        (l.starts_with("adb: device '") && l.contains("' not found"))
            || l.starts_with("error: no devices/emulators found")
            || l.starts_with("error: device offline")
            || l.starts_with("error: device unauthorized")
            || l.starts_with("error: closed")
            || l.starts_with("adb: error: failed to get feature set")
            || l.contains("cannot connect to daemon")
    })
}

/// ハッシュのスレッド（送った長さと 16 進の sha256）
type HashHandle = std::thread::JoinHandle<std::io::Result<(u64, String)>>;

/// FD を読みながら SHA-256 を取り、パイプへ流す。返すのは子の stdin に繋ぐ読み口と、
/// (送った長さ, 16 進の sha256) を返すスレッド
fn hashing_pipe(mut src: std::fs::File) -> std::io::Result<(std::fs::File, HashHandle)> {
    let (reader, mut writer) = std::io::pipe()?;
    let handle = std::thread::spawn(move || {
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0u64;
        loop {
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            writer.write_all(&buf[..n])?;
            total += n as u64;
        }
        drop(writer);
        let hex: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok((total, hex))
    });
    Ok((std::fs::File::from(OwnedFd::from(reader)), handle))
}

/// `size\npath\0` の並びを読む（パスは NUL で終わるので改行を含んでも曖昧にならない）
fn parse_listing(bytes: &[u8]) -> RemoteResult<Vec<RemoteFile>> {
    let bad = |what: &str| RemoteError::Failed(format!("一覧の出力を読めない（{what}）"));
    let mut out = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let nl = rest
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| bad("途中で切れた"))?;
        let size = std::str::from_utf8(&rest[..nl])
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .ok_or_else(|| bad("サイズ"))?;
        rest = &rest[nl + 1..];
        let nul = rest
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| bad("途中で切れた"))?;
        let path = String::from_utf8(rest[..nul].to_vec()).map_err(|_| bad("UTF-8 でないパス"))?;
        rest = &rest[nul + 1..];
        out.push(RemoteFile { path, size });
    }
    Ok(out)
}

impl DeviceFs for AdbFs {
    async fn read(&self, path: &str) -> RemoteResult<Option<Vec<u8>>> {
        let p = self.abs(path)?;
        let script = format!("if [ -f {p} ]; then cat {p}; else exit {MISSING_EXIT}; fi");
        Ok(self.shell_opt(script).await?.map(|o| o.stdout))
    }

    async fn write(&self, path: &str, bytes: &[u8]) -> RemoteResult<()> {
        let (p, dir) = (self.abs(path)?, self.parent(path)?);
        self.shell_stdin(format!("mkdir -p {dir} && cat > {p}"), bytes.to_vec())
            .await
    }

    async fn append(&self, path: &str, bytes: &[u8]) -> RemoteResult<()> {
        let (p, dir) = (self.abs(path)?, self.parent(path)?);
        self.shell_stdin(format!("mkdir -p {dir} && cat >> {p}"), bytes.to_vec())
            .await
    }

    async fn put(&self, path: &str, src: std::fs::File) -> RemoteResult<PutResult> {
        let (p, dir) = (self.abs(path)?, self.parent(path)?);
        let (stdin, hasher) = hashing_pipe(src).map_err(|e| RemoteError::Failed(e.to_string()))?;
        let run = self
            .command(
                STDIN_SUBCOMMAND,
                format!("mkdir -p {dir} && cat > {p}"),
                self.cfg.transfer_timeout,
            )
            .stdin_file(stdin)
            .run(&self.token)
            .await;
        let hashed = tokio::task::spawn_blocking(move || hasher.join())
            .await
            .map_err(|e| RemoteError::Failed(e.to_string()))?
            .map_err(|_| RemoteError::Failed("ハッシュのスレッドが落ちた".into()))?;
        run.map_err(map_err)?;
        let (size, sha256) =
            hashed.map_err(|e| RemoteError::Failed(format!("送る元を読めない: {e}")))?;
        Ok(PutResult { size, sha256 })
    }

    async fn sha256(&self, path: &str) -> RemoteResult<Option<String>> {
        let p = self.abs(path)?;
        let script = format!("if [ -f {p} ]; then sha256sum {p}; else exit {MISSING_EXIT}; fi");
        let Some(out) = self.shell_opt(script).await? else {
            return Ok(None);
        };
        let hex: String = String::from_utf8_lossy(&out.stdout)
            .chars()
            .take(64)
            .collect();
        if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            Ok(Some(hex.to_ascii_lowercase()))
        } else {
            Err(RemoteError::Failed(format!(
                "sha256sum の出力を読めない: {hex:?}"
            )))
        }
    }

    async fn list_files(&self) -> RemoteResult<Vec<RemoteFile>> {
        let r = self.quoted_root()?;
        let script = format!(
            "[ -e {r} ] || exit 0; cd {r} || exit 1; find . -type f -exec sh -c 'for f; do s=$(stat -c %s \"$f\") || continue; printf \"%s\\n%s\\0\" \"$s\" \"${{f#./}}\"; done' sh {{}} +"
        );
        parse_listing(&self.shell(script).await?.stdout)
    }

    async fn rename(&self, from: &str, to: &str) -> RemoteResult<()> {
        let (f, t, dir) = (self.abs(from)?, self.abs(to)?, self.parent(to)?);
        self.shell(format!("mkdir -p {dir} && mv -f {f} {t}"))
            .await?;
        Ok(())
    }

    async fn remove(&self, path: &str) -> RemoteResult<()> {
        let p = self.abs(path)?;
        self.shell(format!("rm -f {p}")).await?;
        Ok(())
    }

    async fn prune_empty_dirs(&self) -> RemoteResult<()> {
        let r = self.quoted_root()?;
        self.shell(format!(
            "[ -d {r} ] || exit 0; cd {r} || exit 1; find . -mindepth 1 -depth -type d -empty ! -path ./.spindle ! -path './.spindle/*' -delete"
        ))
        .await?;
        Ok(())
    }

    async fn sync(&self) -> RemoteResult<()> {
        self.shell("sync".to_owned()).await?;
        Ok(())
    }

    async fn free_bytes(&self) -> RemoteResult<u64> {
        let r = self.quoted_root()?;
        let script = format!(
            "d={r}; while [ -n \"$d\" ] && [ ! -d \"$d\" ]; do d=${{d%/*}}; done; stat -f -c '%a %S' \"${{d:-/}}\""
        );
        let out = self.shell(script).await?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut it = text.split_whitespace().map(str::parse::<u64>);
        match (it.next(), it.next()) {
            (Some(Ok(blocks)), Some(Ok(bsize))) => Ok(blocks.saturating_mul(bsize)),
            _ => Err(RemoteError::Failed(format!("空き容量を読めない: {text:?}"))),
        }
    }

    async fn root_state(&self) -> RemoteResult<DirState> {
        let r = self.quoted_root()?;
        let out = self
            .shell(format!(
                "if [ ! -e {r} ]; then echo missing; else c=$(ls -A {r}) || exit 1; if [ -n \"$c\" ]; then echo nonempty; else echo empty; fi; fi"
            ))
            .await?;
        match String::from_utf8_lossy(&out.stdout).trim() {
            "missing" => Ok(DirState::Missing),
            "empty" => Ok(DirState::Empty),
            "nonempty" => Ok(DirState::NonEmpty),
            other => Err(RemoteError::Failed(format!(
                "保存先の状態を読めない: {other:?}"
            ))),
        }
    }

    async fn rescan(&self) -> RemoteResult<()> {
        self.shell(format!(
            "pm path {POWERAMP_PACKAGE} >/dev/null 2>&1 || exit 0; am broadcast -a {POWERAMP_SCAN_ACTION} -n {POWERAMP_RECEIVER}"
        ))
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn failed(stderr: &str) -> ProcessError {
        ProcessError::Failed {
            program: "adb".into(),
            status: std::process::ExitStatus::from_raw(1 << 8),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn only_adb_own_messages_mean_disconnected() {
        for m in [
            "adb: device 'X' not found",
            "error: no devices/emulators found",
            "error: device offline",
            "error: closed",
            "* cannot connect to daemon at tcp:adb:5037",
        ] {
            assert_eq!(map_err(failed(m)), RemoteError::NotConnected, "{m}");
        }
        for m in [
            "sh: sha256sum: not found",
            "mv: /a/closed/x: No such file",
            "find: not found",
        ] {
            assert!(matches!(map_err(failed(m)), RemoteError::Failed(_)), "{m}");
        }
    }

    #[test]
    fn no_space_is_mapped() {
        let e = map_err(failed("cat: write error: No space left on device"));
        assert_eq!(e, RemoteError::NoSpace);
    }

    #[test]
    fn broken_listing_is_an_error() {
        assert!(parse_listing(b"5\na\0").is_ok());
        assert!(parse_listing(b"5\na").is_err());
        assert!(parse_listing(b"x\na\0").is_err());
        assert!(parse_listing(b"5\n\xff\0").is_err());
    }
}

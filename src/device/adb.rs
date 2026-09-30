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

use crate::device::quote::{sh_quote, valid_root, valid_serial, valid_volume};
use crate::device::remote::{DeviceFs, DirState, PutResult, RemoteError, RemoteFile, RemoteResult};
use crate::domain::relpath::RelPath;
use crate::jobs::process::{ExternalCommand, Output, ProcessError};

/// stdin を流すサブコマンド。実機で shell v2 の stdin が使えなければここを差し替える（P5-3b の実機確認）
pub const STDIN_SUBCOMMAND: &str = "shell";
pub const POWERAMP_PACKAGE: &str = "com.maxmpz.audioplayer";
pub const POWERAMP_SCAN_ACTION: &str = "com.maxmpz.audioplayer.ACTION_SCAN_DIRS";
/// Poweramp の API レシーバ（Poweramp build-1025 で確認。D-98）
pub const POWERAMP_RECEIVER: &str = "com.maxmpz.audioplayer/.player.PowerampAPIReceiver";
/// 「ファイルが無い」を表す端末側スクリプトの終了コード
const MISSING_EXIT: i32 = 3;

#[derive(Debug, Clone)]
pub struct AdbConfig {
    /// adb クライアントのパス（`[bin].adb`）
    pub program: PathBuf,
    /// `ADB_SERVER_SOCKET` の値（`[devices].adb_server`。例 `localfilesystem:/run/adb/adb.sock`）。
    /// 必ず渡す: 無いとクライアントが TCP 5037 に自前のサーバを起こし、サイドカーと USB を奪い合う
    pub server: String,
    /// adb の子に渡す `HOME`（`$HOME/.android` を作れないとクライアントが abort する。呼び出し側が作っておく）
    pub home: PathBuf,
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

    fn base(&self, timeout: Duration) -> ExternalCommand {
        base_command(&self.cfg, &self.serial, timeout)
    }

    fn command(&self, sub: &str, script: String, timeout: Duration) -> ExternalCommand {
        self.base(timeout).arg(sub).arg(script)
    }

    async fn shell(&self, script: String) -> RemoteResult<Output> {
        match self
            .command("shell", script, self.cfg.timeout)
            .run(&self.token)
            .await
        {
            Ok(o) => Ok(o),
            Err(e) => Err(self.classify(e).await),
        }
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
            Err(e) => Err(self.classify(e).await),
        }
    }

    async fn shell_stdin(&self, script: String, bytes: Vec<u8>) -> RemoteResult<()> {
        match self
            .command(STDIN_SUBCOMMAND, script, self.cfg.timeout)
            .stdin_bytes(bytes)
            .run(&self.token)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => Err(self.classify(e).await),
        }
    }

    /// adb の文言で決まらない失敗（転送中に抜かれると stderr 空・rc=255 になる。spike 1）は、
    /// `get-state` で端末がまだ `device` かを確かめて分類する。`device` 以外（offline / unauthorized /
    /// authorizing / 見つからない）はすべて未接続
    async fn classify(&self, e: ProcessError) -> RemoteError {
        match map_err(e) {
            RemoteError::Failed(msg) if !self.token.is_cancelled() => {
                match self.get_state().await {
                    Ok(s) if s == "device" => RemoteError::Failed(msg),
                    Ok(s) => {
                        tracing::info!(serial = %self.serial, state = %s, "端末が device でないので未接続として扱う");
                        RemoteError::NotConnected
                    }
                    Err(_) => RemoteError::NotConnected,
                }
            }
            other => other,
        }
    }

    /// `adb -s <serial> get-state` の出力（`device` / `offline` / `unauthorized` …）
    pub async fn get_state(&self) -> RemoteResult<String> {
        let out = self
            .base(self.cfg.timeout)
            .arg("get-state")
            .run(&self.token)
            .await
            .map_err(map_err)?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// 登録の途中で失敗したときの片付け: `<root>/.spindle` を消し、root が空になれば root も消す。
    /// root 直下の他のファイル（手置き）には触らない
    pub async fn discard_init(&self) -> RemoteResult<()> {
        let r = self.quoted_root()?;
        let s = self.abs(".spindle")?;
        self.shell(format!(
            "[ -e {r} ] || exit 0; rm -rf {s} || exit 1; rmdir {r} 2>/dev/null; exit 0"
        ))
        .await?;
        Ok(())
    }
}

/// `adb version` の版（`Version 37.0.1-15733141` の後ろ）。起動時診断と /health に使う
pub async fn probe_version(cfg: &AdbConfig) -> Option<String> {
    let out = ExternalCommand::new(&cfg.program)
        .env("ADB_SERVER_SOCKET", &cfg.server)
        .env("HOME", &cfg.home)
        .arg("version")
        .timeout(Duration::from_secs(5))
        .run(&CancellationToken::new())
        .await
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("Version ")
                .map(|v| v.trim().to_owned())
        })
        .filter(|v| !v.is_empty())
}

/// `adb -s <serial>` までを組み立てる（サーバの指定と HOME を必ず付ける）
fn base_command(cfg: &AdbConfig, serial: &str, timeout: Duration) -> ExternalCommand {
    ExternalCommand::new(&cfg.program)
        .env("ADB_SERVER_SOCKET", &cfg.server)
        .env("HOME", &cfg.home)
        .arg("-s")
        .arg(serial)
        .timeout(timeout)
}

/// 登録の候補になるボリューム（仕様 ⑤「登録」2）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    /// `emulated` か SD の UUID
    pub volume: String,
    /// root の絶対パス
    pub path: String,
    /// ボリュームの空き（バイト）
    pub free: u64,
    /// root の状態
    pub state: DirState,
}

/// `/storage` の下の内部共有ストレージと SD カードを調べる
pub async fn probe_volumes(
    cfg: &AdbConfig,
    serial: &str,
    root_rel: &str,
    token: &CancellationToken,
) -> RemoteResult<Vec<VolumeInfo>> {
    probe_volumes_under(cfg, serial, "/storage", root_rel, token).await
}

/// [`probe_volumes`] の `/storage` を差し替えられる版（試験用）
pub async fn probe_volumes_under(
    cfg: &AdbConfig,
    serial: &str,
    storage: &str,
    root_rel: &str,
    token: &CancellationToken,
) -> RemoteResult<Vec<VolumeInfo>> {
    if !valid_serial(serial) || !valid_root(root_rel) {
        return Err(RemoteError::Failed("シリアルか root が不正".into()));
    }
    let base = sh_quote(storage).map_err(|e| RemoteError::Failed(e.to_string()))?;
    let rel = sh_quote(root_rel).map_err(|e| RemoteError::Failed(e.to_string()))?;
    // 1 行 1 ボリューム: volume \t state \t 空きブロック数 ブロック長
    let script = format!(
        "b={base}; for d in \"$b\"/emulated/0 \"$b\"/????-????; do [ -d \"$d\" ] || continue; \
         v=${{d#\"$b\"/}}; [ \"$v\" = emulated/0 ] && v=emulated; r=\"$d\"/{rel}; \
         if [ ! -e \"$r\" ]; then s=missing; else c=$(ls -A \"$r\") || continue; \
         if [ -n \"$c\" ]; then s=nonempty; else s=empty; fi; fi; \
         f=$(stat -f -c '%a %S' \"$d\") || continue; printf '%s\\t%s\\t%s\\n' \"$v\" \"$s\" \"$f\"; done"
    );
    let out = base_command(cfg, serial, cfg.timeout)
        .arg("shell")
        .arg(script)
        .run(token)
        .await
        .map_err(map_err)?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut v = Vec::new();
    for line in text.lines() {
        let mut it = line.split('\t');
        let (Some(volume), Some(state), Some(free)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if !valid_volume(volume) {
            continue;
        }
        let state = match state {
            "missing" => DirState::Missing,
            "empty" => DirState::Empty,
            "nonempty" => DirState::NonEmpty,
            _ => continue,
        };
        let mut nums = free.split_whitespace().map(str::parse::<u64>);
        let (Some(Ok(blocks)), Some(Ok(bsize))) = (nums.next(), nums.next()) else {
            continue;
        };
        let dir = if volume == "emulated" {
            format!("{storage}/emulated/0")
        } else {
            format!("{storage}/{volume}")
        };
        v.push(VolumeInfo {
            volume: volume.to_owned(),
            path: format!("{dir}/{root_rel}"),
            free: blocks.saturating_mul(bsize),
            state,
        });
    }
    Ok(v)
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

/// adb 自身の文言だけで未接続を判定する。端末側コマンドの stderr（パスや `sh: ...` の行）に
/// 同じ語が含まれても未接続とは読まないよう、trim した行の行頭が adb の書式のときだけにする
fn is_disconnected(stderr: &str) -> bool {
    stderr.lines().any(|l| {
        let l = l.trim();
        let daemon = l.starts_with("* cannot connect to daemon")
            || l.starts_with("error: cannot connect to daemon")
            || l.starts_with("adb: cannot connect to daemon");
        let l = l.trim_start_matches("* ");
        daemon
            || (l.starts_with("adb: device '") && l.contains("' not found"))
            || l.starts_with("error: no devices/emulators found")
            || l.starts_with("error: device offline")
            || l.starts_with("error: device unauthorized")
            || l.starts_with("error: closed")
            || l.starts_with("adb: error: failed to get feature set")
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

/// 一覧の終端の印（サイズ行が `END` でパスが空のレコード）。これが無い出力は途中で切れたか find が失敗した
const LISTING_END: &[u8] = b"END";
/// stat に失敗したファイルの印（サイズ行の代わり）。サイズ行は数字だけなので区別できる
const LISTING_ERROR: &[u8] = b"E";

/// `size\npath\0` の並びを読む（パスは NUL で終わるので改行を含んでも曖昧にならない）。
/// 最後は必ず終端の印 `END\n\0` で、その後には何も無いこと。`E\npath\0`（stat の失敗）が 1 件でもあれば失敗。
/// 終了コードに頼らず出力だけで一覧の完全さを確かめる（toybox の `find -exec … +` が子の非 0 を伝えなくても塞ぐ）
fn parse_listing(bytes: &[u8]) -> RemoteResult<Vec<RemoteFile>> {
    let bad = |what: &str| RemoteError::Failed(format!("一覧の出力を読めない（{what}）"));
    let mut out = Vec::new();
    let mut rest = bytes;
    loop {
        if rest.is_empty() {
            return Err(bad("終端の印が無い"));
        }
        let nl = rest
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| bad("途中で切れた"))?;
        let head = &rest[..nl];
        rest = &rest[nl + 1..];
        let nul = rest
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| bad("途中で切れた"))?;
        let path = &rest[..nul];
        rest = &rest[nul + 1..];
        if head == LISTING_END {
            if !path.is_empty() || !rest.is_empty() {
                return Err(bad("終端の印の後に続きがある"));
            }
            return Ok(out);
        }
        if head == LISTING_ERROR {
            return Err(RemoteError::Failed(format!(
                "一覧でサイズを取れないファイルがある: {}",
                String::from_utf8_lossy(path)
            )));
        }
        let size = std::str::from_utf8(head)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .ok_or_else(|| bad("サイズ"))?;
        let path = String::from_utf8(path.to_vec()).map_err(|_| bad("UTF-8 でないパス"))?;
        out.push(RemoteFile { path, size });
    }
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
        if let Err(e) = run {
            return Err(self.classify(e).await);
        }
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

    /// 1 件でもサイズを取れなければ一覧全体を失敗させる（握り潰すと手置きのファイルが一覧から消え、
    /// 管理外と分からずに上書きしうる）。失敗は 2 重に伝える: 内側の sh の非 0（`find -exec … +` の終了コード）と、
    /// 出力の `E` レコード。終端の印は find が 0 で終わったときだけ書き、root が無い経路でも書く。
    /// 列挙の後に消えたファイルでも失敗するが、同期が失敗するだけで次回やり直せる
    async fn list_files(&self) -> RemoteResult<Vec<RemoteFile>> {
        let r = self.quoted_root()?;
        let script = format!(
            "[ -e {r} ] || {{ printf 'END\\n\\0'; exit 0; }}; cd {r} || exit 1; find . -type f -exec sh -c 'for f; do if s=$(stat -c %s \"$f\"); then printf \"%s\\n%s\\0\" \"$s\" \"${{f#./}}\"; else printf \"E\\n%s\\0\" \"${{f#./}}\"; exit 1; fi; done' sh {{}} + && printf 'END\\n\\0'"
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

    /// extras は付けない。Poweramp API に対象パスの extra は無く、プレイリストの再解析は `eraseTags`
    /// （全タグ消去・CUE がユーザのプレイリストから消える）しか無いため（実機 spike 4、D-98）。
    /// スキャン範囲は Poweramp のフォルダ設定
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
    fn cannot_connect_is_disconnected_only_in_adb_own_format() {
        for m in [
            "* cannot connect to daemon at tcp:adb:5037: Connection refused",
            "  * cannot connect to daemon",
            "error: cannot connect to daemon",
            "adb: cannot connect to daemon at tcp:adb:5037",
        ] {
            assert_eq!(map_err(failed(m)), RemoteError::NotConnected, "{m}");
        }
        for m in [
            "sh: foo: cannot connect to daemon",
            "mv: /a/cannot connect to daemon/x: No such file",
            "cannot connect to daemon",
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
        assert!(parse_listing(b"5\na\0END\n\0").is_ok());
        assert!(parse_listing(b"5\na").is_err());
        assert!(parse_listing(b"x\na\0END\n\0").is_err());
        assert!(parse_listing(b"5\n\xff\0END\n\0").is_err());
    }

    #[test]
    fn listing_needs_the_end_marker_and_no_failure_records() {
        let ok = parse_listing(b"5\na\x002\nb c\0END\n\0").unwrap();
        assert_eq!(
            ok,
            vec![
                RemoteFile {
                    path: "a".into(),
                    size: 5
                },
                RemoteFile {
                    path: "b c".into(),
                    size: 2
                },
            ]
        );
        // root が無い: 終端の印だけで空の一覧
        assert_eq!(parse_listing(b"END\n\0").unwrap(), vec![]);
        // stat に失敗した印（find が子の非 0 を伝えなくても気づく）
        assert!(parse_listing(b"5\na\0E\nb\0END\n\0").is_err());
        // 終端の印が無い（途中で切れた・find が失敗した）。レコードの境界で切れても失敗
        assert!(parse_listing(b"5\na\0").is_err());
        assert!(parse_listing(b"").is_err());
        // 終端の後に何かある
        assert!(parse_listing(b"END\n\x005\na\0").is_err());
        assert!(parse_listing(b"END\n\0x").is_err());
        // 終端の印にパスが付いている
        assert!(parse_listing(b"END\nx\0").is_err());
    }
}

//! トークンの保管。4b はファイル（`<state_dir>/token`、0600）。macOS では `keychain::KeychainSecrets`（P5-4c、D-100）

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::Result;

#[cfg(target_os = "macos")]
pub mod keychain;

pub trait Secrets {
    fn get(&self) -> Result<Option<String>>;
    fn set(&self, token: &str) -> Result<()>;
    /// トークンを保存できるかを、トークンの値を変えずに確かめる。pair はワンタイムコードを使う前に呼ぶ
    /// （コードを使った後で保存に失敗すると、コードを発行し直すしかなくなる）
    fn check_writable(&self) -> Result<()>;
}

const TOKEN: &str = "token";
const TOKEN_TMP: &str = ".token.tmp";
/// `check_writable` の試しの名前（`set` と同じ手順を本番の名前に触らずに行う）
const PROBE_TMP: &str = ".token.probe.tmp";
const PROBE: &str = ".token.probe";

/// `tmp` に書いて fsync し、`to` へ rename してから `dir` を fsync する（0600）
fn write_replace(dir: &Path, tmp: &Path, to: &Path, bytes: &[u8]) -> std::io::Result<()> {
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, to)?;
    std::fs::File::open(dir)?.sync_all()
}

pub struct FileSecrets {
    dir: PathBuf,
}

impl FileSecrets {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.to_path_buf(),
        }
    }
}

impl Secrets for FileSecrets {
    fn get(&self) -> Result<Option<String>> {
        match std::fs::read_to_string(self.dir.join(TOKEN)) {
            Ok(s) => Ok(Some(s.trim().to_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn set(&self, token: &str) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        write_replace(
            &self.dir,
            &self.dir.join(TOKEN_TMP),
            &self.dir.join(TOKEN),
            token.as_bytes(),
        )?;
        Ok(())
    }

    /// `set` と同じ手順（tmp に書く → fsync → rename → ディレクトリの fsync）を試しの名前で行って消す。
    /// 本番の `token` / `.token.tmp` が在れば通常ファイルであることも確かめる（ディレクトリなどなら
    /// `set` の truncate や rename が失敗する）。トークンには触らない
    fn check_writable(&self) -> Result<()> {
        let fail = |what: &str, e: std::io::Error| {
            crate::Error::Stop(format!(
                "トークンを保存できません（{}、{what}）: {e}",
                self.dir.display()
            ))
        };
        std::fs::create_dir_all(&self.dir).map_err(|e| fail("フォルダの作成", e))?;
        for name in [TOKEN, TOKEN_TMP] {
            match std::fs::symlink_metadata(self.dir.join(name)) {
                Ok(m) if m.is_file() => {}
                Ok(_) => {
                    return Err(crate::Error::Stop(format!(
                        "トークンを保存できません: {} が通常のファイルではありません。片付けてから pair し直してください",
                        self.dir.join(name).display()
                    )))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(fail(name, e)),
            }
        }
        let tmp = self.dir.join(PROBE_TMP);
        let probe = self.dir.join(PROBE);
        if let Err(e) = write_replace(&self.dir, &tmp, &probe, b"probe") {
            // 失敗しても試しのファイルは残さない（無ければ何もしない）
            let _ = std::fs::remove_file(&tmp);
            let _ = std::fs::remove_file(&probe);
            return Err(fail("試しの書き込み", e));
        }
        std::fs::remove_file(&probe).map_err(|e| fail("試しのファイルの削除", e))?;
        Ok(())
    }
}

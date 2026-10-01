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
    /// トークンを保存できるかを、トークンに触らずに確かめる。pair はワンタイムコードを使う前に呼ぶ
    /// （コードを使った後で保存に失敗すると、コードを発行し直すしかなくなる）
    fn check_writable(&self) -> Result<()>;
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
        match std::fs::read_to_string(self.dir.join("token")) {
            Ok(s) => Ok(Some(s.trim().to_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn set(&self, token: &str) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(".token.tmp");
        {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(token.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, self.dir.join("token"))?;
        std::fs::File::open(&self.dir)?.sync_all()?;
        Ok(())
    }

    /// `set` と同じディレクトリに試しのファイルを作って消す
    fn check_writable(&self) -> Result<()> {
        let fail = |e: std::io::Error| {
            crate::Error::Stop(format!(
                "トークンを保存できません（{}）: {e}",
                self.dir.display()
            ))
        };
        std::fs::create_dir_all(&self.dir).map_err(fail)?;
        let probe = self.dir.join(".token.probe");
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&probe)
            .map_err(fail)?;
        std::fs::remove_file(&probe).map_err(fail)?;
        Ok(())
    }
}

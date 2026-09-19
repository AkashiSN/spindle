//! サイドカー `spindle-inbox.json`（SPEC §7.7 / §7.8、D-70）。ダウンローダがタグに載らない情報
//! （件の category、ファイルごとの判定・メッセージ・URL・チャンネル）を Inbox へ渡す入れ物。
//! Inbox の行はキャッシュで DB に直接書かない（D-68）ので、ファイルで渡す。
//!
//! - 走査は音声でないので無視する。`GET /api/inbox` が読んで件に付け、配置の成功時に消す
//! - 書き込みは読んで足して tmp + rename（同じディレクトリへ複数回ダウンロードしても項が残る）。
//!   壊れていれば黙って上書きせず Err（人が書いた内容を失わない）

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};

use serde::{Deserialize, Serialize};

use crate::domain::relpath::RelPath;
use crate::fsroot::{FsError, RootDir};

pub const SIDECAR_NAME: &str = "spindle-inbox.json";
const VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum SidecarError {
    #[error("spindle-inbox.json の version が {0}（対応は 1）")]
    Version(u32),
    #[error("spindle-inbox.json が JSON として読めない: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Fs(#[from] FsError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// ファイル 1 本の項
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// 提供元（`youtube`）
    pub source: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    /// `ok` か、`ok: false` の reason（`unmatched` / `unknown_channel` / 未知の値）
    pub verdict: String,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sidecar {
    #[serde(default = "version_default")]
    version: u32,
    /// 件の category（統制語彙の名前。最後に書いたものが勝つ）
    #[serde(default)]
    pub category: Option<String>,
    /// ファイル名（ディレクトリ内の名前）→ 項
    #[serde(default)]
    pub files: BTreeMap<String, FileEntry>,
}

fn version_default() -> u32 {
    VERSION
}

impl Sidecar {
    pub fn parse(bytes: &[u8]) -> Result<Sidecar, SidecarError> {
        let s: Sidecar = serde_json::from_slice(bytes)?;
        if s.version != VERSION {
            return Err(SidecarError::Version(s.version));
        }
        Ok(s)
    }

    pub fn to_json(&self) -> Vec<u8> {
        let out = Sidecar {
            version: VERSION,
            ..self.clone()
        };
        // serde_json の整形出力は失敗しない（Map / String / Option だけ）
        serde_json::to_vec_pretty(&out).unwrap_or_default()
    }

    /// `dir` の `spindle-inbox.json` を読む。無ければ `None`、壊れていれば Err
    pub fn read(root: &RootDir, dir: &RelPath) -> Result<Option<Sidecar>, SidecarError> {
        let rel = dir
            .join(SIDECAR_NAME)
            .map_err(|e| FsError::Io(std::io::Error::other(e)))?;
        let mut file = match root.open_file(&rel) {
            Ok(f) => f,
            Err(FsError::NotFound) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(Some(Sidecar::parse(&bytes)?))
    }

    /// `dir` の `spindle-inbox.json` に `name` の項を足して（同名は置き換え）書く。`category` が
    /// `Some` なら件の category も置き換える。tmp + rename
    pub fn upsert(
        root: &RootDir,
        dir: &RelPath,
        category: Option<&str>,
        name: &str,
        entry: FileEntry,
    ) -> Result<(), SidecarError> {
        let mut s = Sidecar::read(root, dir)?.unwrap_or_default();
        if let Some(c) = category {
            s.category = Some(c.to_owned());
        }
        s.files.insert(name.to_owned(), entry);
        let rel = dir
            .join(SIDECAR_NAME)
            .map_err(|e| FsError::Io(std::io::Error::other(e)))?;
        let (tmp_rel, mut tmp) = root.create_tmp(Some(dir))?;
        let written = (|| -> Result<(), SidecarError> {
            tmp.write_all(&s.to_json())?;
            tmp.sync_all()?;
            Ok(())
        })();
        if let Err(e) = written {
            let _ = root.unlink(&tmp_rel);
            return Err(e);
        }
        if let Err(e) = root.replace_file(&tmp_rel, &rel) {
            let _ = root.unlink(&tmp_rel);
            return Err(e.into());
        }
        Ok(())
    }
}

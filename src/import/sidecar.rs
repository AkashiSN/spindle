//! サイドカー `spindle-inbox.json`（SPEC §7.7 / §7.8、D-70）。取り込み経路（ダウンローダ、CD の
//! 吸い出し）がタグに載らない情報（件の category、ファイルごとの判定・メッセージ・URL・チャンネル、
//! CD の吸い出しの記録）を Inbox へ渡す入れ物。Inbox の行はキャッシュで DB に直接書かない（D-68）ので、
//! ファイルで渡す。
//!
//! - CD（D-67 追記、P2-5）: `rip` に吸い出しの記録（`RipReport`）と、その配列の順をファイル名
//!   （basename）へ結びつける `files` を持つ。Inbox の配置はこれを読み、名前でトラックへ対応させて
//!   検証記録を入れる（承認で番号やタイトルが直っても、名前は変わらない）
//! - 走査は音声でないので無視する。`GET /api/inbox` が読んで件に付け、配置の成功時に消す
//! - 書き込みは読んで足して tmp + rename（同じディレクトリへ複数回ダウンロードしても項が残る）。
//!   壊れていれば黙って上書きせず Err（人が書いた内容を失わない）

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};

use serde::{Deserialize, Serialize};

use crate::cd::metadata::DiscMetadata;
use crate::cd::riplog::RipReport;
use crate::cd::toc::{Toc, TocError};
use crate::domain::relpath::{canonical_key, RelPath};
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
    /// 購読由来なら購読 id と再生リストの位置（P4-16。配置の後続で同期を投入する）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
}

/// CD の吸い出しの記録（件のディレクトリ = 1 枚。P2-5、D-67 追記）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RipEntry {
    /// CTDB 形式の TOC（`Toc::ctdb_toc`）
    pub toc: String,
    /// 吸い出しを始めたときの内容（MusicBrainz から写した値。名前は空でもよい。承認画面の値は
    /// タグから作るので、これは記録）
    pub metadata: DiscMetadata,
    /// 音声トラック順のファイル名（件のディレクトリ内の basename）。`report` の配列と同じ順
    pub files: Vec<String>,
    /// 件のディレクトリに置いた rip.log の名前（`rip.log` / `rip<N>.log`）
    pub log: String,
    pub report: RipReport,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RipEntryError {
    #[error("TOC を読めない: {0}")]
    Toc(#[from] TocError),
    #[error("{what} の件数が音声トラック数と合わない: 期待 {expected}、実際 {got}")]
    Count {
        what: &'static str,
        expected: usize,
        got: usize,
    },
    #[error("ファイル名が不正: {0}")]
    BadFileName(String),
    #[error("ファイル名が重複: {0}")]
    DuplicateFile(String),
}

impl RipEntry {
    /// 形の検証: TOC が読め、ファイル名・読み取り統計・CRC・照合結果・メタデータのトラックが
    /// すべて音声トラック数と同じ件数で、ファイル名は basename で重複が無い（大小文字・正規化の
    /// 違いも同じ名前とみなす。ZFS の insensitive + formD）
    pub fn check(&self) -> Result<Toc, RipEntryError> {
        let toc = Toc::parse(&self.toc)?;
        let expected = toc.audio_tracks().count();
        let r = &self.report;
        let counts = [
            ("files", self.files.len()),
            ("reads", r.reads.len()),
            ("crcs", r.crcs.len()),
            ("metadata.tracks", self.metadata.tracks.len()),
        ]
        .into_iter()
        .chain(r.ctdb.as_ref().map(|m| ("ctdb.tracks", m.tracks.len())))
        .chain(
            r.accuraterip
                .as_ref()
                .map(|m| ("accuraterip.tracks", m.tracks.len())),
        );
        for (what, got) in counts {
            if got != expected {
                return Err(RipEntryError::Count {
                    what,
                    expected,
                    got,
                });
            }
        }
        let mut seen = std::collections::HashSet::new();
        for name in self.files.iter().chain(std::iter::once(&self.log)) {
            if name.is_empty() || name.contains('/') || name == "." || name == ".." {
                return Err(RipEntryError::BadFileName(name.clone()));
            }
            if !seen.insert(canonical_key(name)) {
                return Err(RipEntryError::DuplicateFile(name.clone()));
            }
        }
        Ok(toc)
    }

    /// ファイル名（basename）の `report` 上の位置
    pub fn index_of(&self, name: &str) -> Option<usize> {
        let key = canonical_key(name);
        self.files.iter().position(|f| canonical_key(f) == key)
    }
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
    /// CD の吸い出しの記録（CD の件だけ）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rip: Option<RipEntry>,
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
        // serde_json の整形出力は失敗しない（キーが文字列の Map / 構造体 / 列挙だけ）
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
        s.write(root, dir)
    }

    /// `dir` の `spindle-inbox.json` をこの内容で置き換える。tmp + rename
    pub fn write(&self, root: &RootDir, dir: &RelPath) -> Result<(), SidecarError> {
        let rel = dir
            .join(SIDECAR_NAME)
            .map_err(|e| FsError::Io(std::io::Error::other(e)))?;
        let (tmp_rel, mut tmp) = root.create_tmp(Some(dir))?;
        let written = (|| -> Result<(), SidecarError> {
            tmp.write_all(&self.to_json())?;
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

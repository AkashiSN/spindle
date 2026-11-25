//! アップロードのマニフェスト `upload.json` と、アップロードごとの鍵（設計 §5.1）。
//!
//! - ステージングは Inbox 直下の `.spindle-upload-<16hex>/`。Inbox の走査は `.` で始まる名前を
//!   飛ばすので件に見えない。中身は `upload.json`・`files/`・`zips/`・`x/`（・公開時の `publish/`）
//! - マニフェストの読み書きは必ず [`with_manifest`] の鍵の中で行う（同時の送信・展開・公開・GC が
//!   更新を失わない）。これはライブラリのファイルの排他ロックではない（spindle だけのステージング）
//! - 書き込みは tmp + fsync + rename（[`RootDir::create_tmp`] → [`RootDir::replace_file`]）
//! - 読めないマニフェストは「壊れたアップロード」として一覧に出し、上書きしない

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::domain::relpath::{RelPath, RelPathError};
use crate::fsroot::{FileKind, FsError, RootDir};

/// ステージングのディレクトリ名の前置き（`.spindle-upload-<16hex>`）
pub const STAGING_PREFIX: &str = ".spindle-upload-";
/// ステージング内のマニフェストの名前
pub const MANIFEST_NAME: &str = "upload.json";
/// マニフェストの形式の版
pub const MANIFEST_VERSION: u32 = 1;
/// 受ける音声の拡張子（アップロード専用の許可リスト。画面の表示と同じ）
pub const AUDIO_EXTS: [&str; 7] = ["flac", "wav", "m4a", "aif", "aiff", "mp3", "opus"];
/// zip の拡張子
const ZIP_EXT: &str = "zip";
/// アップロード ID の hex の桁数
const ID_HEX_LEN: usize = 16;
/// ステージングの中の固定のディレクトリ
const FILES_DIR: &str = "files";
const ZIPS_DIR: &str = "zips";
const UNPACK_DIR: &str = "x";
/// ID を引き直す回数（16 hex の衝突はまず起きないが、既存のディレクトリには書かない）
const ID_RETRIES: usize = 8;

#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("パスが不正: {0}")]
    BadPath(String),
    #[error("既にある: {0}")]
    Exists(String),
    #[error("アップロードが見つからない")]
    NotFound,
    #[error("上限を超える: {0}")]
    Limit(String),
    #[error("空き容量が足りない")]
    NoSpace,
    #[error("送信位置が合わない（受け取り済み {received} バイト）")]
    Offset { received: u64 },
    #[error("公開の要求後か破棄済みで変更できない")]
    Busy,
    #[error("状態が合わない: {0}")]
    Conflict(String),
    #[error(transparent)]
    Fs(#[from] FsError),
    #[error("マニフェストが読めない: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O エラー: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Db(#[from] crate::db::DbError),
    #[error(transparent)]
    Inbox(#[from] Box<crate::import::inbox::InboxError>),
    #[error(transparent)]
    Sidecar(#[from] crate::import::sidecar::SidecarError),
}

impl From<RelPathError> for UploadError {
    fn from(e: RelPathError) -> Self {
        UploadError::BadPath(e.to_string())
    }
}

/// 送ったものの種類
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileClass {
    /// 許可リストの音声
    Audio,
    /// zip（展開して中身を分類する）
    Zip,
    /// 件に付けて運ぶ同梱ファイル（`is_companion_name`）
    Companion,
    /// 取り込まない
    Ignored,
}

/// 名前（相対パス可。最後の要素を見る）から種類を決める。拡張子は大小文字を区別しない
pub fn classify(name: &str) -> FileClass {
    let base = name.rsplit('/').next().unwrap_or(name);
    let ext = base
        .rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map(|(_, ext)| ext);
    if let Some(ext) = ext {
        if AUDIO_EXTS.iter().any(|a| ext.eq_ignore_ascii_case(a)) {
            return FileClass::Audio;
        }
        if ext.eq_ignore_ascii_case(ZIP_EXT) {
            return FileClass::Zip;
        }
    }
    if crate::cd::riplog::is_companion_name(base) {
        FileClass::Companion
    } else {
        FileClass::Ignored
    }
}

/// アップロード ID（16 桁の小文字 hex）。検証済みの ID だけが作られる
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UploadId {
    hex: String,
    /// `.spindle-upload-<hex>`（作るときに検証しておく）
    dir: RelPath,
}

impl UploadId {
    /// 乱数で新しい ID を作る
    pub fn new() -> Result<Self, UploadError> {
        let mut raw = [0u8; ID_HEX_LEN / 2];
        getrandom::fill(&mut raw).map_err(|e| UploadError::Io(std::io::Error::other(e)))?;
        let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        Self::parse(&hex)
    }

    /// 外から来た ID を検証する（16 桁の小文字 hex 以外は [`UploadError::BadPath`]）
    pub fn parse(s: &str) -> Result<Self, UploadError> {
        if s.len() != ID_HEX_LEN || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(UploadError::BadPath(format!(
                "アップロード ID が不正: {s:?}"
            )));
        }
        Ok(Self {
            hex: s.to_string(),
            dir: RelPath::parse(&format!("{STAGING_PREFIX}{s}"))?,
        })
    }

    /// ステージングのディレクトリ名から ID を取り出す（前置きが無い・ID が不正なら `None`）
    pub fn from_dir_name(name: &str) -> Option<Self> {
        name.strip_prefix(STAGING_PREFIX)
            .and_then(|hex| Self::parse(hex).ok())
    }

    pub fn as_str(&self) -> &str {
        &self.hex
    }

    /// Inbox 相対のステージングのディレクトリ（`.spindle-upload-<id>`）
    pub fn staging_dir(&self) -> RelPath {
        self.dir.clone()
    }

    /// ステージング内の相対パス（`files/o1` など）を Inbox 相対にする
    pub fn staged(&self, rel: &RelPath) -> Result<RelPath, UploadError> {
        Ok(RelPath::parse(&format!(
            "{}/{}",
            self.dir.as_str(),
            rel.as_str()
        ))?)
    }

    fn manifest_path(&self) -> Result<RelPath, UploadError> {
        Ok(self.dir.join(MANIFEST_NAME)?)
    }
}

/// アップロードのマニフェスト（`upload.json`）。状態の正（設計 §5.1）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// 形式の版。[`MANIFEST_VERSION`] 以外は読まない
    pub version: u32,
    /// アップロード ID（ディレクトリ名の hex と同じ）
    pub id: String,
    pub created_at: i64,
    /// 最後の利用者の操作の時刻（放置の判定に使う。[`with_manifest`] は進めない）
    pub updated_at: i64,
    #[serde(default)]
    pub origins: Vec<Origin>,
    /// 最後に割り当てた origin の番号（次は `o<next_origin + 1>`）
    #[serde(default)]
    pub next_origin: u32,
    /// ③ でまとめた件のキーの組
    #[serde(default)]
    pub merges: Vec<Vec<String>>,
    /// ③ で外したファイル（キーは `Member::staged` = ステージング内の相対パス）
    #[serde(default)]
    pub excluded_files: Vec<String>,
    /// ③ で外した件（キーは `Group::key`）
    #[serde(default)]
    pub excluded_groups: Vec<String>,
    /// 初期で外れるものを入れ直したファイル（キーは `Member::staged`）
    #[serde(default)]
    pub included_files: Vec<String>,
    /// 初期で外れるものを入れ直した件（キーは `Group::key`）
    #[serde(default)]
    pub included_groups: Vec<String>,
    /// 凍結した公開計画（§5.5）
    #[serde(default)]
    pub publish: Option<PublishPlan>,
    #[serde(default)]
    pub publish_requested_at: Option<i64>,
    /// 全件公開の後に Inbox の走査を投入した時刻
    #[serde(default)]
    pub inbox_enqueued_at: Option<i64>,
    #[serde(default)]
    pub discarded_at: Option<i64>,
    /// GC に消させるステージング内のディレクトリ（消えるまで予算に数える）
    #[serde(default)]
    pub garbage: Vec<Garbage>,
}

/// GC 待ちのステージング内のパス
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Garbage {
    /// ステージング内の相対パス（`x/o2/g1` など）
    pub path: String,
    pub bytes: u64,
    pub added_at: i64,
}

/// 送ったもの 1 つ（ファイルか zip）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    /// `o<N>`。アップロード内で一意な安定 ID
    pub id: String,
    /// ブラウザ上の元の相対パス（`Album/01.flac`、`mora.zip`）
    pub client_path: String,
    pub class: FileClass,
    /// 宣言サイズ
    pub total: u64,
    /// 受け取ったバイト数（表示用の写し。正は `.part` の大きさ）
    #[serde(default)]
    pub received: u64,
    #[serde(default)]
    pub complete: bool,
    /// 人が外した（件には入れないが、実体が GC で消えるまで予算に数える）
    #[serde(default)]
    pub removed: bool,
    /// 実体が食い違って壊れた
    #[serde(default)]
    pub damaged: bool,
    #[serde(default)]
    pub zip: Option<ZipState>,
}

impl Origin {
    /// 受け取り終えたばらのファイル（`files/<id>`）
    pub fn file_path(&self) -> Result<RelPath, UploadError> {
        self.under(FILES_DIR, &self.id)
    }

    /// 受信中のファイル（`files/<id>.part`）
    pub fn part_path(&self) -> Result<RelPath, UploadError> {
        self.under(FILES_DIR, &format!("{}.part", self.id))
    }

    /// zip 本体（`zips/<id>.zip`）
    pub fn zip_path(&self) -> Result<RelPath, UploadError> {
        self.under(ZIPS_DIR, &format!("{}.{ZIP_EXT}", self.id))
    }

    /// zip の展開先の世代（`x/<id>/g<generation>`）
    pub fn unpack_dir(&self, generation: u32) -> Result<RelPath, UploadError> {
        Ok(self
            .under(UNPACK_DIR, &self.id)?
            .join(&format!("g{generation}"))?)
    }

    fn under(&self, dir: &str, name: &str) -> Result<RelPath, UploadError> {
        if !is_origin_id(&self.id) {
            return Err(UploadError::BadPath(format!(
                "origin ID が不正: {:?}",
                self.id
            )));
        }
        Ok(RelPath::parse(dir)?.join(name)?)
    }
}

/// `o` + 1 桁以上の数字
fn is_origin_id(s: &str) -> bool {
    s.strip_prefix('o')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// zip の展開の状態（§5.3）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZipState {
    /// 成功した世代。0 = 未展開
    #[serde(default)]
    pub generation: u32,
    /// 次に使う世代の番号（予約のたびに進めて永続化する。失敗した番号を使い回さない）
    #[serde(default)]
    pub next_generation: u32,
    /// 成功した世代で書いたバイト数
    #[serde(default)]
    pub written_bytes: u64,
    /// 展開中の世代（物理バイトはその世代の [`Garbage::bytes`] で数える）
    #[serde(default)]
    pub unpacking: Option<u32>,
    /// 拒否したエントリ
    #[serde(default)]
    pub rejected: Vec<Rejected>,
    /// zip 全体の失敗
    #[serde(default)]
    pub error: Option<ZipError>,
}

/// zip 全体の失敗
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZipError {
    pub reason: String,
    /// やり直せる失敗か（空き不足・I/O・取り消し）。やり直せないのは壊れた・暗号化・上限超過
    pub retryable: bool,
}

/// 展開で拒否したエントリ
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rejected {
    pub entry: String,
    pub reason: String,
}

/// 凍結した公開計画（§5.5。振る舞いは公開の実装が持つ）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishPlan {
    pub frozen_at: i64,
    pub groups: Vec<PlannedGroup>,
}

/// 公開する件 1 つ
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedGroup {
    /// 組み立て先 `publish/<n>/` の番号
    pub n: u32,
    /// 件のキー
    pub key: String,
    pub albumartist: String,
    /// アルバム名。無ければ `upload/_untitled/<origin_label>/`
    pub album: Option<String>,
    /// zip 名かフォルダ名
    pub origin_label: String,
    pub origins: Vec<String>,
    pub members: Vec<PlannedMember>,
    pub stage: Stage,
}

/// 件のメンバー 1 つ
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedMember {
    /// ステージング内の元の場所
    pub from: String,
    /// 件のディレクトリでの名前
    pub name: String,
}

/// 件の公開の段階
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum Stage {
    Planned,
    Assembled,
    Publishing { dest: String },
    Published { dest: String },
}

/// [`list_uploads`] の 1 項
/// 一覧は数件で、`Ok` を箱に入れると呼び出し側の照合が煩雑になるだけなので大きさの差は許す
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Listed {
    Ok(Manifest),
    /// マニフェストが無い・読めないステージング（捨てるだけできる。上書きしない）
    Broken {
        /// Inbox 相対のステージングのディレクトリ
        dir: RelPath,
        /// ディレクトリの mtime（ナノ秒）
        mtime_ns: i64,
        reason: String,
    },
}

/// アップロードごとの鍵。`AppState`・ジョブハンドラ・GC で 1 つを共有する（clone は同じ表を指す）
#[derive(Debug, Clone, Default)]
pub struct UploadLocks(Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>);

impl UploadLocks {
    fn entry(&self, id: &UploadId) -> Arc<Mutex<()>> {
        let mut map = lock(&self.0);
        map.entry(id.as_str().to_string()).or_default().clone()
    }
}

/// 毒された mutex も中身は `()` か表なので、そのまま使う（鍵の目的は直列化だけ）
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 鍵だけを取って `f` を走らせる（マニフェストは読み書きしない。壊れたステージングの処理用）
pub fn with_upload_lock<T>(
    locks: &UploadLocks,
    id: &UploadId,
    f: impl FnOnce() -> Result<T, UploadError>,
) -> Result<T, UploadError> {
    let entry = locks.entry(id);
    let _guard = lock(&entry);
    f()
}

/// 鍵の中でマニフェストを読み、`f` で変え、変わっていれば書く。`f` が Err なら書かない。
/// `updated_at` は進めない（利用者の操作として進めるのは `f` の役目）
pub fn with_manifest<T>(
    inbox: &RootDir,
    locks: &UploadLocks,
    id: &UploadId,
    f: impl FnOnce(&mut Manifest) -> Result<T, UploadError>,
) -> Result<T, UploadError> {
    with_upload_lock(locks, id, || {
        let mut m = read_manifest(inbox, id)?;
        let before = m.clone();
        let out = f(&mut m)?;
        if m != before {
            write_manifest(inbox, id, &m)?;
        }
        Ok(out)
    })
}

/// マニフェストを読むだけ（鍵の外で使うのは表示だけ）
pub fn read_manifest(inbox: &RootDir, id: &UploadId) -> Result<Manifest, UploadError> {
    match inbox.stat(&id.staging_dir()) {
        Ok(st) if st.kind == FileKind::Dir => {}
        Ok(_) | Err(FsError::NotFound) => return Err(UploadError::NotFound),
        Err(e) => return Err(e.into()),
    }
    let mut buf = Vec::new();
    inbox
        .open_file(&id.manifest_path()?)?
        .read_to_end(&mut buf)?;
    let m = parse_manifest(&buf)?;
    if m.id != id.as_str() {
        return Err(json_error(format!(
            "マニフェストの id {:?} がディレクトリと合わない",
            m.id
        )));
    }
    Ok(m)
}

/// マニフェストの JSON を読み、形式を確かめる（版、origin ID の形と重複）
pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest, UploadError> {
    let m: Manifest = serde_json::from_slice(bytes)?;
    if m.version != MANIFEST_VERSION {
        return Err(json_error(format!("対応しない版 {}", m.version)));
    }
    let mut seen = std::collections::HashSet::new();
    for o in &m.origins {
        if !is_origin_id(&o.id) || !seen.insert(o.id.as_str()) {
            return Err(json_error(format!("origin ID が不正: {:?}", o.id)));
        }
    }
    Ok(m)
}

fn json_error(msg: String) -> UploadError {
    UploadError::Json(<serde_json::Error as serde::de::Error>::custom(msg))
}

/// tmp + fsync + rename で書く（呼び出し側が鍵を持つ）
fn write_manifest(inbox: &RootDir, id: &UploadId, m: &Manifest) -> Result<(), UploadError> {
    let bytes = serde_json::to_vec_pretty(m)?;
    let dir = id.staging_dir();
    let (tmp, mut file) = inbox.create_tmp(Some(&dir))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    inbox.replace_file(&tmp, &id.manifest_path()?)?;
    Ok(())
}

/// 新しいアップロードを作る: ステージングと `files/` `zips/` `x/` を作り、空のマニフェストを書く
pub fn create_upload(inbox: &RootDir, now: i64) -> Result<Manifest, UploadError> {
    for _ in 0..ID_RETRIES {
        let id = UploadId::new()?;
        let dir = id.staging_dir();
        match inbox.stat(&dir) {
            Err(FsError::NotFound) => {}
            Ok(_) => continue,
            Err(e) => return Err(e.into()),
        }
        for sub in [FILES_DIR, ZIPS_DIR, UNPACK_DIR] {
            inbox.create_dir_all(&dir.join(sub)?)?;
        }
        let m = Manifest {
            version: MANIFEST_VERSION,
            id: id.as_str().to_string(),
            created_at: now,
            updated_at: now,
            origins: Vec::new(),
            next_origin: 0,
            merges: Vec::new(),
            excluded_files: Vec::new(),
            excluded_groups: Vec::new(),
            included_files: Vec::new(),
            included_groups: Vec::new(),
            publish: None,
            publish_requested_at: None,
            inbox_enqueued_at: None,
            discarded_at: None,
            garbage: Vec::new(),
        };
        write_manifest(inbox, &id, &m)?;
        inbox.fsync_dir(None)?;
        return Ok(m);
    }
    Err(UploadError::Exists("アップロード ID".into()))
}

/// Inbox 直下のステージングをすべて読む。読めないものは [`Listed::Broken`] で返す（消さない）
pub fn list_uploads(inbox: &RootDir) -> Result<Vec<Listed>, UploadError> {
    let mut out = Vec::new();
    for entry in inbox.read_dir(None)? {
        if entry.kind != FileKind::Dir {
            continue;
        }
        let Some(id) = entry.name.to_str().and_then(UploadId::from_dir_name) else {
            continue;
        };
        match read_manifest(inbox, &id) {
            Ok(m) => out.push(Listed::Ok(m)),
            // 一覧の間に消えた（GC が消し終えた）ものは出さない
            Err(UploadError::NotFound) => {}
            Err(e) => {
                let dir = id.staging_dir();
                let mtime_ns = match inbox.stat(&dir) {
                    Ok(st) => st.mtime_ns,
                    Err(FsError::NotFound) => continue,
                    Err(e) => return Err(e.into()),
                };
                out.push(Listed::Broken {
                    dir,
                    mtime_ns,
                    reason: e.to_string(),
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::thread;

    fn inbox() -> (tempfile::TempDir, RootDir) {
        let dir = tempfile::tempdir().unwrap();
        let root = RootDir::open(dir.path()).unwrap();
        (dir, root)
    }

    fn origin(id: &str) -> Origin {
        Origin {
            id: id.to_string(),
            client_path: format!("Album/{id}.flac"),
            class: FileClass::Audio,
            total: 10,
            received: 0,
            complete: false,
            removed: false,
            damaged: false,
            zip: None,
        }
    }

    #[test]
    fn manifest_roundtrip() {
        let (dir, inbox) = inbox();
        let m = create_upload(&inbox, 100).unwrap();
        assert_eq!(m.version, 1);
        assert_eq!(m.created_at, 100);
        assert_eq!(m.updated_at, 100);
        let id = UploadId::parse(&m.id).unwrap();
        let staging = dir.path().join(id.staging_dir().as_str());
        assert!(staging.join(MANIFEST_NAME).is_file());
        for sub in ["files", "zips", "x"] {
            assert!(staging.join(sub).is_dir(), "{sub}");
        }
        assert!(id.staging_dir().as_str().starts_with(STAGING_PREFIX));

        // 全フィールドを埋めて書き、読み直して一致
        let out = with_manifest(&inbox, &UploadLocks::default(), &id, |m| {
            let mut zip = origin("o2");
            zip.class = FileClass::Zip;
            zip.client_path = "mora.zip".into();
            zip.complete = true;
            zip.zip = Some(ZipState {
                generation: 1,
                next_generation: 3,
                written_bytes: 42,
                unpacking: Some(2),
                rejected: vec![Rejected {
                    entry: "../evil".into(),
                    reason: "パス".into(),
                }],
                error: Some(ZipError {
                    reason: "空き不足".into(),
                    retryable: true,
                }),
            });
            let mut o1 = origin("o1");
            o1.removed = true;
            o1.damaged = true;
            m.origins = vec![o1, zip];
            m.next_origin = 3;
            m.merges = vec![vec!["a".into(), "b".into()]];
            m.excluded_files = vec!["o1".into()];
            m.excluded_groups = vec!["g".into()];
            m.included_files = vec!["o3".into()];
            m.included_groups = vec!["h".into()];
            m.publish = Some(PublishPlan {
                frozen_at: 5,
                groups: vec![PlannedGroup {
                    n: 1,
                    key: "k".into(),
                    albumartist: "A".into(),
                    album: Some("X".into()),
                    origin_label: "Album".into(),
                    origins: vec!["o1".into()],
                    members: vec![PlannedMember {
                        from: "files/o1".into(),
                        name: "01.flac".into(),
                    }],
                    stage: Stage::Publishing {
                        dest: "upload/A/X".into(),
                    },
                }],
            });
            m.publish_requested_at = Some(6);
            m.inbox_enqueued_at = Some(7);
            m.discarded_at = Some(8);
            m.garbage = vec![Garbage {
                path: "x/o2/g2".into(),
                bytes: 9,
                added_at: 10,
            }];
            Ok(m.clone())
        })
        .unwrap();
        let back = read_manifest(&inbox, &id).unwrap();
        assert_eq!(back, out);
        assert_eq!(
            back.updated_at, 100,
            "with_manifest は updated_at を進めない"
        );

        // 段階のタグ
        let v = serde_json::to_value(&back).unwrap();
        assert_eq!(
            v["publish"]["groups"][0]["stage"],
            serde_json::json!({"stage": "publishing", "dest": "upload/A/X"})
        );
        assert_eq!(v["origins"][1]["class"], "zip");

        // 新しい任意フィールドが無い古い形でも読める
        let minimal = serde_json::json!({
            "version": 1, "id": m.id, "created_at": 1, "updated_at": 2,
            "origins": [{"id": "o1", "client_path": "a.flac", "class": "audio", "total": 3}]
        });
        let parsed = parse_manifest(&serde_json::to_vec(&minimal).unwrap()).unwrap();
        assert_eq!(parsed.origins[0].received, 0);
        assert!(parsed.garbage.is_empty());
        assert!(parsed.inbox_enqueued_at.is_none());

        // version != 1 は拒否
        let mut v2 = minimal.clone();
        v2["version"] = 2.into();
        assert!(parse_manifest(&serde_json::to_vec(&v2).unwrap()).is_err());
    }

    #[test]
    fn with_manifest_serializes_writers() {
        let (_dir, inbox) = inbox();
        let inbox = Arc::new(inbox);
        let locks = UploadLocks::default();
        let m = create_upload(&inbox, 1).unwrap();
        let id = UploadId::parse(&m.id).unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (inbox, locks, id, barrier) =
                    (inbox.clone(), locks.clone(), id.clone(), barrier.clone());
                thread::spawn(move || {
                    barrier.wait();
                    with_manifest(&inbox, &locks, &id, |m| {
                        m.next_origin += 1;
                        let oid = format!("o{}", m.next_origin);
                        // 読みと書きの間を広げて、鍵が無ければ更新を失うようにする
                        thread::sleep(std::time::Duration::from_millis(5));
                        m.origins.push(origin(&oid));
                        Ok(())
                    })
                    .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let m = read_manifest(&inbox, &id).unwrap();
        assert_eq!(m.origins.len(), 8);
        assert_eq!(m.next_origin, 8);
    }

    #[test]
    fn with_manifest_err_does_not_write() {
        let (dir, inbox) = inbox();
        let locks = UploadLocks::default();
        let m = create_upload(&inbox, 1).unwrap();
        let id = UploadId::parse(&m.id).unwrap();
        let path = dir
            .path()
            .join(id.staging_dir().as_str())
            .join(MANIFEST_NAME);
        let before = std::fs::read(&path).unwrap();
        let r: Result<(), _> = with_manifest(&inbox, &locks, &id, |m| {
            m.origins.push(origin("o1"));
            Err(UploadError::Busy)
        });
        assert!(matches!(r, Err(UploadError::Busy)));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(read_manifest(&inbox, &id).unwrap().origins.is_empty());
    }

    #[test]
    fn upload_lock_without_manifest() {
        let (dir, inbox) = inbox();
        let locks = UploadLocks::default();
        let id = UploadId::parse("0123456789abcdef").unwrap();
        std::fs::create_dir(dir.path().join(id.staging_dir().as_str())).unwrap();
        assert!(read_manifest(&inbox, &id).is_err());
        let got = with_upload_lock(&locks, &id, || Ok(7)).unwrap();
        assert_eq!(got, 7);
        // 鍵は返されている（もう一度取れる）
        assert!(with_upload_lock(&locks, &id, || Ok(())).is_ok());
    }

    #[test]
    fn list_reports_broken_staging() {
        let (dir, inbox) = inbox();
        let good = create_upload(&inbox, 1).unwrap();
        // マニフェストの無いステージング
        let missing = UploadId::parse("00000000000000aa").unwrap();
        std::fs::create_dir(dir.path().join(missing.staging_dir().as_str())).unwrap();
        // 読めないマニフェスト
        let garbled = UploadId::parse("00000000000000bb").unwrap();
        let gdir = dir.path().join(garbled.staging_dir().as_str());
        std::fs::create_dir(&gdir).unwrap();
        std::fs::write(gdir.join(MANIFEST_NAME), b"{not json").unwrap();
        // 関係ないもの（件や他の隠しディレクトリ）は出さない
        std::fs::create_dir(dir.path().join("Album")).unwrap();
        std::fs::create_dir(dir.path().join(".spindle-rip-x")).unwrap();

        let mut listed = list_uploads(&inbox).unwrap();
        assert_eq!(listed.len(), 3);
        listed.sort_by_key(|l| match l {
            Listed::Ok(m) => m.id.clone(),
            Listed::Broken { dir, .. } => dir.as_str().to_string(),
        });
        let mut oks = 0;
        let mut broken = Vec::new();
        for l in &listed {
            match l {
                Listed::Ok(m) => {
                    assert_eq!(m.id, good.id);
                    oks += 1;
                }
                Listed::Broken {
                    dir,
                    mtime_ns,
                    reason,
                } => {
                    assert!(*mtime_ns > 0);
                    assert!(!reason.is_empty());
                    broken.push(dir.clone());
                }
            }
        }
        assert_eq!(oks, 1);
        broken.sort();
        assert_eq!(broken, vec![missing.staging_dir(), garbled.staging_dir()]);
    }

    #[test]
    fn broken_manifest_not_overwritten() {
        let (dir, inbox) = inbox();
        let locks = UploadLocks::default();
        let id = UploadId::parse("00000000000000cc").unwrap();
        let sdir = dir.path().join(id.staging_dir().as_str());
        std::fs::create_dir(&sdir).unwrap();
        std::fs::write(sdir.join(MANIFEST_NAME), b"{\"version\": 1").unwrap();
        let mut called = false;
        let r = with_manifest(&inbox, &locks, &id, |_| {
            called = true;
            Ok(())
        });
        assert!(r.is_err());
        assert!(!called);
        assert_eq!(
            std::fs::read(sdir.join(MANIFEST_NAME)).unwrap(),
            b"{\"version\": 1"
        );
        // 無いアップロードは NotFound
        let none = UploadId::parse("00000000000000dd").unwrap();
        assert!(matches!(
            with_manifest(&inbox, &locks, &none, |_| Ok(())),
            Err(UploadError::NotFound)
        ));
    }

    #[test]
    fn upload_id_rejects_non_hex() {
        for bad in [
            "",
            "0123456789abcde",
            "0123456789abcdef0",
            "0123456789abcdeg",
            "0123456789ABCDEF",
            "../../../../etc/",
            "0123456789abcde/",
        ] {
            assert!(UploadId::parse(bad).is_err(), "{bad:?}");
        }
        let id = UploadId::parse("0123456789abcdef").unwrap();
        assert_eq!(id.as_str(), "0123456789abcdef");
        assert_eq!(
            id.staging_dir().as_str(),
            ".spindle-upload-0123456789abcdef"
        );
        let fresh = UploadId::new().unwrap();
        assert!(UploadId::parse(fresh.as_str()).is_ok());
        assert_ne!(fresh, UploadId::new().unwrap());

        // 実体のパス
        let o = origin("o3");
        assert_eq!(o.file_path().unwrap().as_str(), "files/o3");
        assert_eq!(o.part_path().unwrap().as_str(), "files/o3.part");
        assert_eq!(o.zip_path().unwrap().as_str(), "zips/o3.zip");
        assert_eq!(o.unpack_dir(2).unwrap().as_str(), "x/o3/g2");
        assert_eq!(
            id.staged(&o.file_path().unwrap()).unwrap().as_str(),
            ".spindle-upload-0123456789abcdef/files/o3"
        );
        // origin の ID が規則に外れるマニフェストは読まない
        let bad = serde_json::json!({
            "version": 1, "id": "0123456789abcdef", "created_at": 1, "updated_at": 2,
            "origins": [{"id": "../x", "client_path": "a.flac", "class": "audio", "total": 3}]
        });
        assert!(parse_manifest(&serde_json::to_vec(&bad).unwrap()).is_err());
    }

    #[test]
    fn classify_uses_upload_allowlist() {
        assert_eq!(classify("a.ogg"), FileClass::Ignored);
        assert_eq!(classify("a.wv"), FileClass::Ignored);
        assert_eq!(classify("A.FLAC"), FileClass::Audio);
        assert_eq!(classify("b.pdf"), FileClass::Companion);
        assert_eq!(classify("Album/Disc 1/01.m4a"), FileClass::Audio);
        assert_eq!(classify("mora.ZIP"), FileClass::Zip);
        assert_eq!(classify("Scans/back.jpg"), FileClass::Companion);
        assert_eq!(classify("notes.txt"), FileClass::Ignored);
        assert_eq!(classify("flac"), FileClass::Ignored);
        for ext in AUDIO_EXTS {
            assert_eq!(classify(&format!("x.{ext}")), FileClass::Audio, "{ext}");
        }
    }
}

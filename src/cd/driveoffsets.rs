//! AccurateRip のドライブ別読み取りオフセット表（`DriveOffsets.bin`。D-83 追記）。EAC / foobar2000 /
//! dBpoweramp と同じく、ディスクが無くてもドライブの型番（INQUIRY）からオフセットの初期値を引く。
//! 表は同梱せず、実行時に AccurateRip から取って `data/` に保存し使い回す（照会と同じ扱い。D-13）。
//! 取れなければ保存済みのもの、それも無ければ表なし（オフセット 0 で始め、照合で見つけて学習する）
//!
//! 形式: 69 バイトのレコードの並び。`offset: i16 LE`、名前 33 バイト（NUL 詰め。
//! `"<vendor 8 桁>  - <product 16 桁>"` を右端の空白を落とした形。vendor が空なら `"- <product>"`）、
//! 提出数 `u32 LE`、予約 30 バイト

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use super::LookupError;

pub const TABLE_NAME: &str = "DriveOffsets.bin";
const RECORD: usize = 69;
const NAME_LEN: usize = 33;
/// 保存した表を取り直すまでの期間（新しいドライブの登録を拾う程度でよい）
pub const MAX_AGE: Duration = Duration::from_secs(30 * 86_400);
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveEntry {
    /// 表の名前（`"PIONEER  - BD-RW   BDR-209M"`）
    pub name: String,
    pub offset: i32,
    /// そのオフセットを提出した数（同じ型番が複数あれば多い方を採る）
    pub submissions: u32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DriveTableError {
    #[error("長さが 69 バイトの倍数でない: {0}")]
    Length(usize),
    #[error("表が空")]
    Empty,
    #[error("{index} 番目のレコードが壊れている: {reason}")]
    Record { index: usize, reason: &'static str },
}

/// 表を読む。実物の全レコードで成り立つ形（名前は空でなく 33 バイト内で NUL 終端、予約は 0）から
/// 1 件でも外れれば表ごと拒む（途中で切れた・別物を掴んだ応答で、正しい保存を上書きしない）
pub fn parse(bytes: &[u8]) -> Result<Vec<DriveEntry>, DriveTableError> {
    if !bytes.len().is_multiple_of(RECORD) {
        return Err(DriveTableError::Length(bytes.len()));
    }
    if bytes.is_empty() {
        return Err(DriveTableError::Empty);
    }
    let at = 2 + NAME_LEN;
    bytes
        .as_chunks::<RECORD>()
        .0
        .iter()
        .enumerate()
        .map(|(index, r)| {
            let bad = |reason| DriveTableError::Record { index, reason };
            let raw = &r[2..at];
            let end = raw
                .iter()
                .position(|&b| b == 0)
                .ok_or(bad("名前が NUL で終わっていない"))?;
            let name = String::from_utf8_lossy(&raw[..end]).trim().to_owned();
            if name.is_empty() {
                return Err(bad("名前が空"));
            }
            if r[at + 4..].iter().any(|&b| b != 0) {
                return Err(bad("予約領域が 0 でない"));
            }
            Ok(DriveEntry {
                name,
                offset: i32::from(i16::from_le_bytes([r[0], r[1]])),
                submissions: u32::from_le_bytes([r[at], r[at + 1], r[at + 2], r[at + 3]]),
            })
        })
        .collect()
}

/// 照合の鍵: vendor と product の区切り（`" - "`、vendor が空なら先頭の `"- "`）を空白にし、空白の
/// 連なりを 1 つに、大文字に。`Drive::model`（vendor と product を空白 1 つで結合）と同じ形になる
pub fn normalize(name: &str) -> String {
    let s = name.trim();
    let s = s.strip_prefix("- ").unwrap_or(s);
    s.replacen(" - ", " ", 1)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
}

/// 型番の項（同じ鍵が複数あれば提出数の多いもの）
pub fn lookup<'a>(entries: &'a [DriveEntry], model: &str) -> Option<&'a DriveEntry> {
    let key = normalize(model);
    entries
        .iter()
        .filter(|e| normalize(&e.name) == key)
        .max_by_key(|e| e.submissions)
}

/// 取り直しに失敗してから次に試すまで（古い表で動き続ける。吸い出しのたびに叩かない）
pub const RETRY_AFTER: Duration = Duration::from_secs(3600);

/// メモリの表と、その取得時刻（保存から読んだならファイルの更新時刻）
struct Loaded {
    entries: Arc<Vec<DriveEntry>>,
    fetched_at: SystemTime,
    /// 最後に取り直しに失敗した時刻
    failed_at: Option<SystemTime>,
}

/// 表の取得と保存。読み込んだ表はメモリにも持ち、`MAX_AGE` を過ぎたら次に使うときに取り直す
/// （常駐し続けても新しいドライブの登録を拾う）
pub struct DriveOffsetTable {
    url: String,
    cache: PathBuf,
    http: reqwest::Client,
    mem: tokio::sync::Mutex<Option<Loaded>>,
    /// 同期で覗く用（`GET /api/cd/status` はネットワークを待たない）
    snapshot: std::sync::RwLock<Option<Arc<Vec<DriveEntry>>>>,
}

impl DriveOffsetTable {
    /// `base` は AccurateRip の `dBAR-*.bin` と同じ置き場（`[verify].accuraterip_url`。末尾 `/`）、
    /// `cache` は保存先（`data/cd/DriveOffsets.bin`）
    pub fn new(base: &str, user_agent: &str, cache: PathBuf) -> Result<Self, LookupError> {
        let base = if base.ends_with('/') {
            base.to_owned()
        } else {
            format!("{base}/")
        };
        Ok(Self {
            url: format!("{base}{TABLE_NAME}"),
            cache,
            http: super::http_client(user_agent)?,
            mem: tokio::sync::Mutex::new(None),
            snapshot: std::sync::RwLock::new(None),
        })
    }

    /// 型番の項。表が無ければ（取れない・保存も無い）None
    pub async fn lookup(&self, model: &str) -> Option<DriveEntry> {
        let entries = self.entries().await?;
        lookup(&entries, model).cloned()
    }

    /// 読み込み済みの表だけで引く（取得を待たない）
    pub fn peek(&self, model: &str) -> Option<DriveEntry> {
        let snap = self
            .snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?;
        lookup(&snap, model).cloned()
    }

    /// 表を用意する（起動時に 1 回呼んでおくと、status が最初から出せる）
    pub async fn entries(&self) -> Option<Arc<Vec<DriveEntry>>> {
        let mut mem = self.mem.lock().await;
        let now = SystemTime::now();
        let age = |t: SystemTime| now.duration_since(t).unwrap_or_default();
        // メモリの表が新しい、または取り直しに失敗したばかりならそれを使う
        if let Some(l) = mem.as_ref() {
            let fresh = age(l.fetched_at) < MAX_AGE;
            let backing_off = l.failed_at.is_some_and(|t| age(t) < RETRY_AFTER);
            if fresh || backing_off {
                return Some(Arc::clone(&l.entries));
            }
        } else if let Some((entries, modified)) = self.read_cache().await {
            // 起動して最初: 保存が新しければそれ（壊れた保存は None で取り直しへ）
            if age(modified) < MAX_AGE {
                let entries = Arc::new(entries);
                self.install(&mut mem, Arc::clone(&entries), modified);
                return Some(entries);
            }
            // 古い保存は取り直しに失敗したときの予備としてメモリに置く
            *mem = Some(Loaded {
                entries: Arc::new(entries),
                fetched_at: modified,
                failed_at: None,
            });
        }
        match self.fetch().await {
            Ok(e) => {
                let entries = Arc::new(e);
                self.install(&mut mem, Arc::clone(&entries), now);
                Some(entries)
            }
            Err(err) => {
                tracing::warn!(error = %err, url = %self.url, "AccurateRip のドライブ表を取れない（前の表を使う）");
                let l = mem.as_mut()?;
                l.failed_at = Some(now);
                let entries = Arc::clone(&l.entries);
                *self.snapshot.write().unwrap_or_else(|e| e.into_inner()) =
                    Some(Arc::clone(&entries));
                Some(entries)
            }
        }
    }

    /// テスト用: メモリの表を期限切れにする（常駐が `MAX_AGE` を越えた状況を作る）
    #[doc(hidden)]
    pub async fn expire_for_test(&self) {
        if let Some(l) = self.mem.lock().await.as_mut() {
            l.fetched_at = SystemTime::UNIX_EPOCH;
            l.failed_at = None;
        }
    }

    fn install(&self, mem: &mut Option<Loaded>, entries: Arc<Vec<DriveEntry>>, at: SystemTime) {
        *mem = Some(Loaded {
            entries: Arc::clone(&entries),
            fetched_at: at,
            failed_at: None,
        });
        *self.snapshot.write().unwrap_or_else(|e| e.into_inner()) = Some(entries);
    }

    /// 保存した表と、その更新時刻（無い・壊れていれば None）
    async fn read_cache(&self) -> Option<(Vec<DriveEntry>, SystemTime)> {
        let path = self.cache.clone();
        tokio::task::spawn_blocking(move || {
            let meta = std::fs::metadata(&path).ok()?;
            let modified = meta.modified().ok()?;
            let bytes = std::fs::read(&path).ok()?;
            match parse(&bytes) {
                Ok(e) => Some((e, modified)),
                Err(err) => {
                    tracing::warn!(error = %err, path = %path.display(), "保存したドライブ表が壊れている");
                    None
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    async fn fetch(&self) -> Result<Vec<DriveEntry>, String> {
        let resp = self
            .http
            .get(&self.url)
            .timeout(FETCH_TIMEOUT)
            .send()
            .await
            .map_err(|e| super::error_chain(&e))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let bytes = resp.bytes().await.map_err(|e| super::error_chain(&e))?;
        let entries = parse(&bytes).map_err(|e| e.to_string())?;
        // 保存（tmp + rename）。失敗しても今回はメモリの表で使う
        let (path, bytes) = (self.cache.clone(), bytes.to_vec());
        let saved = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("bin.tmp");
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &path)
        })
        .await;
        if !matches!(saved, Ok(Ok(()))) {
            tracing::warn!(path = %self.cache.display(), "ドライブ表を保存できない");
        }
        tracing::info!(drives = entries.len(), "AccurateRip のドライブ表を取った");
        Ok(entries)
    }
}

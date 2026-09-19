//! メタデータプラグインのプロトコル v1（SPEC §7.7、D-69）。
//!
//! アイテム 1 件ごとに `[ytmusic].metadata_command` を起動し、stdin に Request を書いて stdout の
//! Response を読む。判定できたかに関わらずプラグインは終了コード 0 で返す約束で、非ゼロ・不正な JSON・
//! プロトコル違い・必須の値の欠落はプラグインの故障（`ProviderError`）、`ok: false` は `Outcome::Declined`

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::domain::pathgen::{sanitize_component, TrackFields};
use crate::domain::replaygain::RG_KEYS;
use crate::edit::PICTURE_KEY;
use crate::jobs::process::{ExternalCommand, ProcessError};

pub const PROTOCOL: u32 = 1;

/// 取り込み対象 1 件（提供元に依らない形。無いものは null）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// 提供元（"youtube" 等）
    pub source: String,
    /// 設定にあるダウンロード元の識別子（プラグインのチャンネル定義のキー）
    pub channel: String,
    #[serde(default)]
    pub channel_title: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    pub title: String,
    /// YYYY-MM-DD
    #[serde(default)]
    pub uploaded_at: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct Request<'a> {
    protocol: u32,
    op: &'static str,
    item: &'a Item,
}

/// プラグインが判定したトラック
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    pub title: String,
    pub artists: Vec<String>,
    pub albumartist: String,
    pub album: String,
    /// 配置先の category（統制語彙の名前）。None は未分類（`_Unsorted`）
    #[serde(default)]
    pub category: Option<String>,
    /// YYYY[-MM[-DD]]
    #[serde(default)]
    pub date: Option<String>,
    /// 追加のタグ（キー, 値）
    #[serde(default)]
    pub tags: Vec<(String, String)>,
}

impl Track {
    /// ファイルに書くタグ（Vorbis Comment 流のキー。多値は反復）。`track_no` が None なら
    /// TRACKNUMBER を書かない（採番は Inbox。D-70）
    pub fn tags(&self, track_no: Option<i64>) -> Vec<(String, String)> {
        let mut out = vec![("TITLE".to_owned(), self.title.clone())];
        for a in &self.artists {
            out.push(("ARTIST".to_owned(), a.clone()));
        }
        out.push(("ALBUM".to_owned(), self.album.clone()));
        out.push(("ALBUMARTIST".to_owned(), self.albumartist.clone()));
        if let Some(d) = &self.date {
            out.push(("DATE".to_owned(), d.clone()));
        }
        if let Some(n) = track_no {
            out.push(("TRACKNUMBER".to_owned(), n.to_string()));
        }
        for (k, v) in &self.tags {
            out.push((k.to_ascii_uppercase(), v.clone()));
        }
        out
    }

    /// 配置計画（`pathgen::plan`）に渡す形
    pub fn track_fields(&self, track_no: i64, ext: &str, stem: &str) -> TrackFields {
        TrackFields {
            category: self.category.clone(),
            albumartist: Some(self.albumartist.clone()),
            artist: self.artists.first().cloned(),
            album: Some(self.album.clone()),
            title: Some(self.title.clone()),
            disc_no: Some(1),
            track_no: Some(track_no),
            year: self
                .date
                .as_deref()
                .filter(|d| d.len() >= 4 && d.as_bytes()[..4].iter().all(u8::is_ascii_digit))
                .map(|d| d[..4].to_owned()),
            edition: None,
            ext: ext.to_owned(),
            stem: stem.to_owned(),
        }
    }
}

/// プラグインの応答（`ok` で分岐。未知のフィールドは無視）
#[derive(Debug, Deserialize)]
struct Response {
    protocol: u32,
    ok: bool,
    #[serde(default)]
    track: Option<Track>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// プラグインの判定
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Track(Track),
    /// 判定しなかった。`reason` は unmatched / unknown_channel / skip / unsupported（それ以外も通す）
    Declined {
        reason: String,
        message: String,
    },
}

impl Outcome {
    /// 意図的に取り込まない（要対応ではない）
    pub fn is_skip(&self) -> bool {
        matches!(self, Outcome::Declined { reason, .. } if reason == "skip")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("メタデータプラグインの実行に失敗: {0}")]
    Process(#[from] ProcessError),
    #[error("メタデータプラグインの応答が JSON でない: {0}")]
    Json(String),
    #[error("メタデータプラグインのプロトコルが違う: {0}（対応: {PROTOCOL}）")]
    Protocol(u32),
    #[error("メタデータプラグインの応答が不正: {0}")]
    Invalid(String),
}

/// `[ytmusic].metadata_command` の呼び出し
#[derive(Debug, Clone)]
pub struct MetadataProvider {
    program: PathBuf,
    args: Vec<String>,
    timeout: Duration,
}

impl MetadataProvider {
    /// `command` は引数配列（先頭がプログラム）。空なら None
    pub fn new(command: &[String], timeout: Duration) -> Option<MetadataProvider> {
        let (program, args) = command.split_first()?;
        Some(MetadataProvider {
            program: PathBuf::from(program),
            args: args.to_vec(),
            timeout,
        })
    }

    pub async fn resolve(
        &self,
        item: &Item,
        token: &CancellationToken,
    ) -> Result<Outcome, ProviderError> {
        let request = serde_json::to_vec(&Request {
            protocol: PROTOCOL,
            op: "metadata",
            item,
        })
        .map_err(|e| ProviderError::Invalid(format!("Request を JSON にできない: {e}")))?;
        let out = ExternalCommand::new(&self.program)
            .args(&self.args)
            .timeout(self.timeout)
            .stdin_bytes(request)
            .run(token)
            .await?;
        // 厳密な UTF-8 で読む（不正なバイトを U+FFFD に置換して受理しない）。抜粋の表示だけ lossy
        let excerpt = || {
            String::from_utf8_lossy(&out.stdout)
                .chars()
                .take(200)
                .collect::<String>()
        };
        let text = std::str::from_utf8(&out.stdout)
            .map_err(|e| ProviderError::Json(format!("UTF-8 でない: {e}: {}", excerpt())))?;
        let res: Response = serde_json::from_str(text.trim())
            .map_err(|e| ProviderError::Json(format!("{e}: {}", excerpt())))?;
        if res.protocol != PROTOCOL {
            return Err(ProviderError::Protocol(res.protocol));
        }
        if !res.ok {
            // reason / message の欠落と空はプロトコル不正（未知の reason は前方互換で通す）
            let reason = res
                .reason
                .filter(|r| !r.trim().is_empty())
                .ok_or_else(|| ProviderError::Invalid("ok: false なのに reason が無い".into()))?;
            let message = res
                .message
                .filter(|m| !m.trim().is_empty())
                .ok_or_else(|| ProviderError::Invalid("ok: false なのに message が無い".into()))?;
            return Ok(Outcome::Declined { reason, message });
        }
        let track = res
            .track
            .ok_or_else(|| ProviderError::Invalid("ok なのに track が無い".into()))?;
        validate(&track)?;
        Ok(Outcome::Track(track))
    }
}

fn validate(t: &Track) -> Result<(), ProviderError> {
    let empty = |name: &str| ProviderError::Invalid(format!("{name} が空"));
    if t.title.trim().is_empty() {
        return Err(empty("title"));
    }
    if t.albumartist.trim().is_empty() {
        return Err(empty("albumartist"));
    }
    if t.album.trim().is_empty() {
        return Err(empty("album"));
    }
    if t.artists.is_empty() || t.artists.iter().any(|a| a.trim().is_empty()) {
        return Err(empty("artists"));
    }
    // category は統制語彙 = パスの 1 要素。API（POST /api/categories）と同じ規則で、
    // 置換・切り詰めが要る名前は受け付けない（DB の語彙と実パス名がずれて衝突する）
    if let Some(c) = &t.category {
        if c.trim().is_empty() {
            return Err(empty("category"));
        }
        if c.trim() != c || sanitize_component(c) != *c {
            return Err(ProviderError::Invalid(format!(
                "category がディレクトリ名として不正: {c:?}（前後の空白、`/` 等の禁止文字、末尾のドット、予約名は不可）"
            )));
        }
    }
    if let Some(d) = &t.date {
        if !is_valid_date(d) {
            return Err(ProviderError::Invalid(format!(
                "date の形が不正: {d}（YYYY / YYYY-MM / YYYY-MM-DD）"
            )));
        }
    }
    for (k, v) in &t.tags {
        let key = k.trim().to_uppercase();
        if key.is_empty() {
            return Err(empty("tags のキー"));
        }
        // Vorbis Comment で使えない文字（`=`、制御文字、非 ASCII）
        if !key.chars().all(|c| (' '..='}').contains(&c) && c != '=') {
            return Err(ProviderError::Invalid(format!(
                "tags のキーに使えない文字がある: {k:?}"
            )));
        }
        if RESERVED_TAG_KEYS.contains(&key.as_str())
            || key == PICTURE_KEY
            || RG_KEYS.contains(&key.as_str())
        {
            return Err(ProviderError::Invalid(format!(
                "tags に spindle が決めるキーがある: {key}（track の各フィールド / 画像 / ReplayGain は spindle が書く）"
            )));
        }
        if v.trim().is_empty() || v.chars().any(char::is_control) {
            return Err(ProviderError::Invalid(format!(
                "tags の値が空か制御文字を含む: {key}"
            )));
        }
    }
    Ok(())
}

/// `Track` の各フィールドから spindle が書くタグ。追加タグでは渡せない（画像の疑似キー `PICTURE` と
/// ReplayGain のキーも spindle の責務なので予約）
const RESERVED_TAG_KEYS: &[&str] = &[
    "TITLE",
    "ARTIST",
    "ALBUM",
    "ALBUMARTIST",
    "DATE",
    "TRACKNUMBER",
    "DISCNUMBER",
    "METADATA_BLOCK_PICTURE",
    SOURCE_URL_KEY,
];

/// ダウンローダが書く提供元の URL（yt-dlp の `webpage_url`）。重複取り込みの判定に使う（D-70）
pub const SOURCE_URL_KEY: &str = "SOURCE_URL";

/// `YYYY[-MM[-DD]]` か
fn is_valid_date(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.is_empty() || parts.len() > 3 {
        return false;
    }
    let digits = |p: &str, n: usize| p.len() == n && p.bytes().all(|b| b.is_ascii_digit());
    if !digits(parts[0], 4) {
        return false;
    }
    let in_range = |p: &str, lo: u32, hi: u32| {
        digits(p, 2) && p.parse::<u32>().is_ok_and(|v| (lo..=hi).contains(&v))
    };
    parts.get(1).is_none_or(|m| in_range(m, 1, 12))
        && parts.get(2).is_none_or(|d| in_range(d, 1, 31))
}

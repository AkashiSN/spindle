//! `jobs` / `track_locks` 表への同期アクセス（SPEC §8、D-23）。
//!
//! ここは `&Connection` を受け取る純粋な DB 操作だけを置く。スケジューリングや
//! 進捗配信は `crate::jobs` が担う。状態遷移はすべて `WHERE state = ...` 付きの
//! UPDATE で行い、`changes()` で遷移が成立したかを返す（同じジョブを二重に終端へ
//! 進めない）。

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::str::FromStr;

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use super::{DbError, Result};

/// ジョブ種別。並列度と冪等キーの構成は SPEC §8 の表に従う
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobType {
    Scan,
    Rip,
    Verify,
    Rg,
    Transcode,
    Tagwrite,
    Rename,
    Normalize,
    Thumbnail,
    Flaccheck,
    Inbox,
    Ytdl,
    Gc,
    Backup,
    Hirescheck,
    /// 再生リストの購読の同期（P4-16、D-78）
    PlaylistSync,
}

impl JobType {
    pub const ALL: [JobType; 16] = [
        JobType::Scan,
        JobType::Rip,
        JobType::Verify,
        JobType::Rg,
        JobType::Transcode,
        JobType::Tagwrite,
        JobType::Rename,
        JobType::Normalize,
        JobType::Thumbnail,
        JobType::Flaccheck,
        JobType::Inbox,
        JobType::Ytdl,
        JobType::Gc,
        JobType::Backup,
        JobType::Hirescheck,
        JobType::PlaylistSync,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            JobType::Scan => "scan",
            JobType::Rip => "rip",
            JobType::Verify => "verify",
            JobType::Rg => "rg",
            JobType::Transcode => "transcode",
            JobType::Tagwrite => "tagwrite",
            JobType::Rename => "rename",
            JobType::Normalize => "normalize",
            JobType::Thumbnail => "thumbnail",
            JobType::Flaccheck => "flaccheck",
            JobType::Inbox => "inbox",
            JobType::Ytdl => "ytdl",
            JobType::Gc => "gc",
            JobType::Backup => "backup",
            JobType::Hirescheck => "hirescheck",
            JobType::PlaylistSync => "playlist_sync",
        }
    }

    /// デコード・エンコード・解析で CPU を使い切る種別（D-73）。種別ごとの上限に加えて、
    /// 共通の並列予算（`Jobs::cpu_budget` = コア数）を取ってから走る
    pub fn cpu_bound(self) -> bool {
        matches!(
            self,
            JobType::Rg | JobType::Transcode | JobType::Flaccheck | JobType::Hirescheck
        )
    }

    /// 種別ごとの並列度（SPEC §8）。`cpus` は論理コア数
    pub fn concurrency(self, cpus: usize) -> usize {
        let cpus = cpus.max(1);
        match self {
            JobType::Scan => 1,
            JobType::Rip => 1,
            JobType::Verify => 2,
            JobType::Rg => cpus,
            JobType::Transcode => (cpus - 1).max(1),
            JobType::Tagwrite => 4,
            JobType::Rename => 1,
            JobType::Normalize => 2,
            JobType::Thumbnail => 4,
            JobType::Flaccheck => cpus,
            JobType::Inbox => 1,
            JobType::Ytdl => 1,
            JobType::Gc => 1,
            JobType::Backup => 1,
            // デコード + FFT。rg / transcode / flaccheck と同時に走る分を抑える（D-71）
            JobType::Hirescheck => (cpus / 2).max(1),
            // 購読ごとに揃えとリネームを順に行う。yt-dlp の列挙も 1 本ずつ
            JobType::PlaylistSync => 1,
        }
    }

    /// 版を持つジョブ（開始直前に payload の版と現在値を比較する。SPEC §8 / §7.5）。
    /// 返り値は `(payload のキー, tracks の列名)`
    pub fn version_field(self) -> Option<(&'static str, &'static str)> {
        match self {
            JobType::Tagwrite => Some(("tag_version", "tag_version")),
            JobType::Transcode | JobType::Flaccheck | JobType::Hirescheck => {
                Some(("audio_version", "audio_version"))
            }
            _ => None,
        }
    }
}

impl fmt::Display for JobType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for JobType {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        JobType::ALL
            .into_iter()
            .find(|t| t.as_str() == s)
            .ok_or_else(|| format!("不明なジョブ種別: {s:?}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Done | JobState::Failed | JobState::Cancelled
        )
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for JobState {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "queued" => JobState::Queued,
            "running" => JobState::Running,
            "done" => JobState::Done,
            "failed" => JobState::Failed,
            "cancelled" => JobState::Cancelled,
            other => return Err(format!("不明なジョブ状態: {other:?}")),
        })
    }
}

/// `jobs` の 1 行。`GET /api/jobs` の item もこの形で返す（SPEC §9）
#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: i64,
    #[serde(rename = "type")]
    pub job_type: JobType,
    pub dedup_key: Option<String>,
    pub payload: serde_json::Value,
    pub state: JobState,
    pub edit_batch_id: Option<i64>,
    pub priority: i64,
    pub run_after: Option<i64>,
    pub progress: Option<f64>,
    pub total: Option<i64>,
    pub done: Option<i64>,
    pub attempts: i64,
    pub max_attempts: i64,
    pub last_error: Option<String>,
    pub cancel_requested_at: Option<i64>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    /// 完了時の結果 1 行（ハンドラが `Outcome::DoneWith` で返す。ytdl の「Inbox に置いた: …」等。P4-13）。
    /// 失敗の理由は `last_error`
    pub note: Option<String>,
    /// 対象の表示用文字列（`GET /api/jobs` の一覧だけが埋める。payload の track_id / album_id /
    /// batch_id を解決したもの。[`subject_of`]）
    pub subject: Option<String>,
}

const JOB_COLUMNS: &str = "id, type, dedup_key, payload, state, edit_batch_id, priority, run_after,
     progress, total, done, attempts, max_attempts, last_error, cancel_requested_at,
     created_at, started_at, finished_at, note";

fn parse_col<T: FromStr<Err = String>>(row: &Row<'_>, idx: usize) -> rusqlite::Result<T> {
    let s: String = row.get(idx)?;
    s.parse().map_err(|e: String| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(e)),
        )
    })
}

fn job_from_row(row: &Row<'_>) -> rusqlite::Result<Job> {
    let payload: String = row.get(3)?;
    let payload = serde_json::from_str(&payload).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(Job {
        id: row.get(0)?,
        job_type: parse_col(row, 1)?,
        dedup_key: row.get(2)?,
        payload,
        state: parse_col(row, 4)?,
        edit_batch_id: row.get(5)?,
        priority: row.get(6)?,
        run_after: row.get(7)?,
        progress: row.get(8)?,
        total: row.get(9)?,
        done: row.get(10)?,
        attempts: row.get(11)?,
        max_attempts: row.get(12)?,
        last_error: row.get(13)?,
        cancel_requested_at: row.get(14)?,
        created_at: row.get(15)?,
        started_at: row.get(16)?,
        finished_at: row.get(17)?,
        note: row.get(18)?,
        subject: None,
    })
}

/// ジョブの対象（ジョブ画面の「対象」列）。payload の id を解決した値を優先し、行が消えていれば id で
/// 示す。種別に対象が無ければ None
pub fn subject_of(
    job_type: JobType,
    payload: &serde_json::Value,
    track_rel_path: Option<&str>,
    album_rel_dir: Option<&str>,
    batch_description: Option<&str>,
) -> Option<String> {
    let id_of = |key: &str| payload.get(key).and_then(|v| v.as_i64());
    let text_of = |key: &str| payload.get(key).and_then(|v| v.as_str()).map(str::to_owned);
    let track = || {
        track_rel_path
            .map(str::to_owned)
            .or_else(|| id_of("track_id").map(|id| format!("track #{id}")))
    };
    let album = || {
        album_rel_dir
            .map(|d| format!("{d}/"))
            .or_else(|| id_of("album_id").map(|id| format!("album #{id}")))
    };
    let batch = || {
        batch_description
            .map(str::to_owned)
            .or_else(|| id_of("batch_id").map(|id| format!("batch #{id}")))
    };
    match job_type {
        JobType::Transcode => {
            let variant = text_of("variant").unwrap_or_else(|| "opus".to_owned());
            track().map(|t| format!("{t} [{variant}]"))
        }
        JobType::Rg => track().or_else(album),
        JobType::Flaccheck | JobType::Hirescheck | JobType::Tagwrite | JobType::Normalize => {
            track()
        }
        JobType::Verify => album(),
        JobType::Rename => batch(),
        JobType::Scan => text_of("kind"),
        JobType::Ytdl => text_of("url"),
        JobType::Thumbnail => id_of("artwork_id").map(|id| format!("artwork #{id}")),
        JobType::PlaylistSync => id_of("subscription_id").map(|id| format!("subscription #{id}")),
        JobType::Rip | JobType::Inbox | JobType::Gc | JobType::Backup => None,
    }
}

/// 投入するジョブ。`NewJob::new(..).dedup_key(..)` のように組み立てる
#[derive(Debug, Clone)]
pub struct NewJob {
    pub job_type: JobType,
    pub dedup_key: Option<String>,
    pub payload: serde_json::Value,
    pub edit_batch_id: Option<i64>,
    pub priority: i64,
    pub max_attempts: i64,
    pub run_after: Option<i64>,
}

/// 既定の最大試行回数（スキーマの DEFAULT と合わせる）
pub const DEFAULT_MAX_ATTEMPTS: i64 = 5;

impl NewJob {
    pub fn new(job_type: JobType, payload: serde_json::Value) -> Self {
        Self {
            job_type,
            dedup_key: None,
            payload,
            edit_batch_id: None,
            priority: 0,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            run_after: None,
        }
    }

    pub fn dedup_key(mut self, key: impl Into<String>) -> Self {
        self.dedup_key = Some(key.into());
        self
    }

    pub fn edit_batch_id(mut self, id: i64) -> Self {
        self.edit_batch_id = Some(id);
        self
    }

    pub fn priority(mut self, priority: i64) -> Self {
        self.priority = priority;
        self
    }

    pub fn max_attempts(mut self, n: i64) -> Self {
        self.max_attempts = n.max(1);
        self
    }

    pub fn run_after(mut self, epoch: i64) -> Self {
        self.run_after = Some(epoch);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueResult {
    /// 新規に投入した
    Inserted(i64),
    /// 同じ `dedup_key` の未完了ジョブが既にある
    Duplicate(i64),
}

impl EnqueueResult {
    pub fn id(self) -> i64 {
        match self {
            EnqueueResult::Inserted(id) | EnqueueResult::Duplicate(id) => id,
        }
    }
}

/// 版付きジョブの payload から `(track_id, 版)` を取り出す。欠けていれば理由を返す。
/// `unversioned: true` のジョブ（md5 op の tagwrite。D-59）は版を持たないので `track_id` だけを
/// 要求し、版は `None`（stale 判定の対象外。`track_locks` は取る）。版を持たない種別は `None`
pub fn versioned_payload(
    job_type: JobType,
    payload: &serde_json::Value,
) -> Result<Option<(i64, Option<i64>)>> {
    let Some((payload_key, _)) = job_type.version_field() else {
        return Ok(None);
    };
    let track_id = payload.get("track_id").and_then(|v| v.as_i64());
    if is_unversioned(payload) {
        return match track_id {
            Some(t) => Ok(Some((t, None))),
            None => Err(DbError::Internal(format!(
                "{job_type} の payload に整数の track_id が必要: {payload}"
            ))),
        };
    }
    let version = payload.get(payload_key).and_then(|v| v.as_i64());
    match (track_id, version) {
        (Some(t), Some(v)) => Ok(Some((t, Some(v)))),
        _ => Err(DbError::Internal(format!(
            "{job_type} の payload に整数の track_id と {payload_key} が必要: {payload}"
        ))),
    }
}

/// 未完了の同キーがあれば `Duplicate`。dedup の判定は部分 UNIQUE インデックスに任せる
/// （書き手は単一コネクションなので、事前 SELECT との間に競合は起きない）。
/// 版付き種別は payload に `track_id` と版が無ければ拒否する
pub fn enqueue(conn: &Connection, job: &NewJob, now: i64) -> Result<EnqueueResult> {
    versioned_payload(job.job_type, &job.payload)?;
    if let Some(key) = &job.dedup_key {
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM jobs WHERE dedup_key = ?1 AND state IN ('queued','running')",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return Ok(EnqueueResult::Duplicate(id));
        }
    }
    conn.execute(
        "INSERT INTO jobs (type, dedup_key, payload, state, edit_batch_id, priority,
                           run_after, max_attempts, created_at)
         VALUES (?1, ?2, ?3, 'queued', ?4, ?5, ?6, ?7, ?8)",
        params![
            job.job_type.as_str(),
            job.dedup_key,
            job.payload.to_string(),
            job.edit_batch_id,
            job.priority,
            job.run_after,
            job.max_attempts,
            now,
        ],
    )?;
    Ok(EnqueueResult::Inserted(conn.last_insert_rowid()))
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Job>> {
    Ok(conn
        .query_row(
            &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
            [id],
            job_from_row,
        )
        .optional()?)
}

/// バッチに紐づく未完了（`queued` / `running`）のジョブ
pub fn active_jobs_of_batch(conn: &Connection, batch_id: i64) -> Result<Vec<Job>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {JOB_COLUMNS} FROM jobs
         WHERE edit_batch_id = ?1 AND state IN ('queued','running') ORDER BY id"
    ))?;
    let rows = stmt.query_map([batch_id], job_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// `GET /api/jobs` の一覧の上限（状態ごと。ジョブ画面のタブに対応）。待ちが数千件あっても完了・失敗が
/// 届くよう、1 本の並びで切らずに状態ごとに切り出す
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ListLimits {
    /// 実行中 + 待ち（実行中が先頭、待ちはキューから取られる順）
    pub active: usize,
    /// 完了（新しい順）
    pub done: usize,
    /// 失敗 + 取り消し（新しい順）
    pub failed: usize,
}

impl Default for ListLimits {
    fn default() -> Self {
        Self {
            active: 1000,
            done: 300,
            failed: 300,
        }
    }
}

/// 状態の集合と並び（定数。値は含まない）
enum Slice {
    Active,
    Done,
    Failed,
}

impl Slice {
    fn sql(self) -> (&'static str, &'static str) {
        match self {
            // キューから取られる順（[`claim`] と同じ priority 降順 → 作成順 → id 昇順）。同じ秒に数千件を
            // 一括投入したとき id 降順だと、先頭から取られた実行中が上限の外に落ちる
            Slice::Active => (
                "j.state IN ('queued', 'running')",
                "(j.state = 'running') DESC, j.priority DESC, j.created_at ASC, j.id ASC",
            ),
            Slice::Done => (
                "j.state = 'done'",
                "COALESCE(j.finished_at, j.created_at) DESC, j.id DESC",
            ),
            Slice::Failed => (
                "j.state IN ('failed', 'cancelled')",
                "COALESCE(j.finished_at, j.created_at) DESC, j.id DESC",
            ),
        }
    }
}

fn list_slice(
    conn: &Connection,
    slice: Slice,
    limit: usize,
    job_type: Option<JobType>,
) -> Result<Vec<Job>> {
    // 対象の解決（track / album / batch）。payload の id を JOIN するだけで、行が無ければ NULL
    let columns: Vec<String> = JOB_COLUMNS
        .split(',')
        .map(|c| format!("j.{}", c.trim()))
        .collect();
    let (filter, order) = slice.sql();
    // 種別の絞り込み（`?type=`）。種別名は enum の固定文字列で、値はバインドする
    let type_filter = if job_type.is_some() {
        " AND j.type = ?2"
    } else {
        " AND ?2 IS NULL"
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT {}, t.rel_path, a.rel_dir, b.description
         FROM jobs j
         LEFT JOIN tracks t ON t.id = json_extract(j.payload, '$.track_id')
         LEFT JOIN albums a ON a.id = json_extract(j.payload, '$.album_id')
         LEFT JOIN edit_batches b ON b.id = json_extract(j.payload, '$.batch_id')
         WHERE {filter}{type_filter}
         ORDER BY {order}
         LIMIT ?1",
        columns.join(", ")
    ))?;
    let ty: Option<&str> = job_type.map(JobType::as_str);
    let rows = stmt.query_map(params![limit as i64, ty], |row| {
        let mut job = job_from_row(row)?;
        let track: Option<String> = row.get(19)?;
        let album: Option<String> = row.get(20)?;
        let batch: Option<String> = row.get(21)?;
        job.subject = subject_of(
            job.job_type,
            &job.payload,
            track.as_deref(),
            album.as_deref(),
            batch.as_deref(),
        );
        Ok(job)
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 一覧。実行中・待ち（上限まで）→ 完了（新しい順に上限まで）→ 失敗・取り消し（同）の順に並べる
pub fn list(conn: &Connection, limits: ListLimits, job_type: Option<JobType>) -> Result<Vec<Job>> {
    let mut items = list_slice(conn, Slice::Active, limits.active, job_type)?;
    items.extend(list_slice(conn, Slice::Done, limits.done, job_type)?);
    items.extend(list_slice(conn, Slice::Failed, limits.failed, job_type)?);
    Ok(items)
}

/// 一覧・summary・種別ごとの件数を同じ読み取りスナップショットで取る（WAL では別 SELECT が別
/// スナップになり得るため、明示トランザクションで囲う）。`job_type` は一覧だけを絞る（上限も
/// 種別内で数える）。summary / 件数は全件
pub fn list_with_summary(
    conn: &Connection,
    limits: ListLimits,
    job_type: Option<JobType>,
) -> Result<(Vec<Job>, Summary, BTreeMap<String, TypeCounts>)> {
    let tx = conn.unchecked_transaction()?;
    let items = list(&tx, limits, job_type)?;
    let summary = summary(&tx)?;
    let by_type = counts_by_type(&tx)?;
    tx.finish()?;
    Ok((items, summary, by_type))
}

/// 種別ごとの件数（`GET /api/jobs` の `by_type`。一覧は上限付きなので web で数えると欠ける）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct TypeCounts {
    pub queued: i64,
    pub running: i64,
    pub done: i64,
    pub failed: i64,
    pub cancelled: i64,
}

/// 種別ごとの状態別件数を全件から集計する。件数 0 の種別は入らない
pub fn counts_by_type(conn: &Connection) -> Result<BTreeMap<String, TypeCounts>> {
    let mut stmt = conn.prepare_cached(
        "SELECT type,
                count(*) FILTER (WHERE state = 'queued'),
                count(*) FILTER (WHERE state = 'running'),
                count(*) FILTER (WHERE state = 'done'),
                count(*) FILTER (WHERE state = 'failed'),
                count(*) FILTER (WHERE state = 'cancelled')
           FROM jobs
          GROUP BY type",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            TypeCounts {
                queued: r.get(1)?,
                running: r.get(2)?,
                done: r.get(3)?,
                failed: r.get(4)?,
                cancelled: r.get(5)?,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()?)
}

/// `GET /api/jobs` の `summary`（SPEC §9）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub running: i64,
    pub queued: i64,
    pub pending_ops: i64,
    pub failed: i64,
    /// 終端（ジョブ画面の「完了」タブ。GC までの全件）
    pub done: i64,
    pub cancelled: i64,
}

pub fn summary(conn: &Connection) -> Result<Summary> {
    let (running, queued, failed, done, cancelled) = conn.query_row(
        "SELECT
            count(*) FILTER (WHERE state = 'running'),
            count(*) FILTER (WHERE state = 'queued'),
            count(*) FILTER (WHERE state = 'failed'),
            count(*) FILTER (WHERE state = 'done'),
            count(*) FILTER (WHERE state = 'cancelled')
         FROM jobs",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let pending_ops = conn.query_row(
        "SELECT count(*) FROM edit_ops WHERE result = 'pending'",
        [],
        |r| r.get(0),
    )?;
    Ok(Summary {
        running,
        queued,
        pending_ops,
        failed,
        done,
        cancelled,
    })
}

/// 起動時リカバリの結果
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecoveryReport {
    /// `running` → `queued` に戻した件数
    pub requeued: usize,
    /// cancel 要求が立っていたので `cancelled` に送った件数（running / queued とも）
    pub cancelled: usize,
    pub locks_cleared: usize,
}

/// `running` を `queued` へ戻し、`track_locks` を全件消す（SPEC §8）。
/// cancel 要求が立っている行は再実行せず `cancelled` にする（D-36）。
/// `run_after` / `attempts` は触らない（バックオフは再起動を跨いで保つ）
pub fn recover(conn: &Connection, now: i64) -> Result<RecoveryReport> {
    let cancelled = conn.execute(
        "UPDATE jobs SET state = 'cancelled', finished_at = ?1, started_at = NULL
         WHERE state IN ('running','queued') AND cancel_requested_at IS NOT NULL",
        [now],
    )?;
    let requeued = conn.execute(
        "UPDATE jobs SET state = 'queued', started_at = NULL WHERE state = 'running'",
        [],
    )?;
    let locks_cleared = conn.execute("DELETE FROM track_locks", [])?
        + conn.execute("DELETE FROM derived_path_locks", [])?
        + conn.execute("DELETE FROM job_mutexes", [])?;
    Ok(RecoveryReport {
        requeued,
        cancelled,
        locks_cleared,
    })
}

/// cancel 要求が立ったまま `queued` にある行（バックオフ中に cancel された等）を `cancelled` へ
/// 送る。ワーカーが claim の前に毎回呼ぶ。送った id を返す（イベント配信用）
pub fn sweep_cancel_requested(conn: &Connection, now: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare_cached(
        "UPDATE jobs SET state = 'cancelled', finished_at = ?1
         WHERE state = 'queued' AND cancel_requested_at IS NOT NULL
         RETURNING id",
    )?;
    let ids = stmt.query_map([now], |r| r.get(0))?;
    Ok(ids.collect::<rusqlite::Result<Vec<i64>>>()?)
}

/// 稼働中の回収（D-76）で `queued` へ戻した行の `last_error`
pub const ORPHAN_REQUEUED_ERROR: &str =
    "終端を記録できないまま running に残っていたので再キューした（稼働中の回収）";

/// 稼働中の回収の結果（[`sweep_orphaned_running`]）
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OrphanSweep {
    /// `queued` へ戻した id
    pub requeued: Vec<i64>,
    /// cancel 要求が立っていたので `cancelled` に送った id
    pub cancelled: Vec<i64>,
    pub locks_cleared: usize,
}

impl OrphanSweep {
    pub fn is_empty(&self) -> bool {
        self.requeued.is_empty() && self.cancelled.is_empty()
    }
}

/// 稼働中の回収（起動時リカバリのループ版。SPEC §8、D-76）。DB で `running` だが `tracked`
/// （プロセス内の実行中表）に無い行は、終端を書けずに残った行なので `queued` へ戻す（cancel 要求が
/// 立っていれば `cancelled`）。その行の `track_locks` / `derived_path_locks` / `job_mutexes` も消す。
/// `attempts` / `run_after` は触らない（基盤の障害であってジョブの失敗ではない）
pub fn sweep_orphaned_running(
    conn: &Connection,
    tracked: &HashSet<i64>,
    now: i64,
) -> Result<OrphanSweep> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, cancel_requested_at IS NOT NULL FROM jobs WHERE state = 'running'",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, bool>(1)?)))?;
    let orphans: Vec<(i64, bool)> = rows
        .filter_map(|r| match r {
            Ok((id, _)) if tracked.contains(&id) => None,
            other => Some(other),
        })
        .collect::<rusqlite::Result<_>>()?;
    let mut sweep = OrphanSweep::default();
    for (id, cancel_requested) in orphans {
        sweep.locks_cleared += release_track_locks(conn, id)?
            + conn.execute("DELETE FROM derived_path_locks WHERE job_id = ?1", [id])?
            + release_mutexes(conn, id)?;
        if cancel_requested {
            conn.execute(
                "UPDATE jobs SET state = 'cancelled', finished_at = ?2, started_at = NULL
                 WHERE id = ?1 AND state = 'running'",
                params![id, now],
            )?;
            sweep.cancelled.push(id);
        } else {
            conn.execute(
                "UPDATE jobs SET state = 'queued', started_at = NULL, last_error = ?2
                 WHERE id = ?1 AND state = 'running'",
                params![id, ORPHAN_REQUEUED_ERROR],
            )?;
            sweep.requeued.push(id);
        }
    }
    Ok(sweep)
}

/// 種別 `ty` の実行可能なジョブを 1 件 `running` にして返す。
/// cancel 要求が立っている行は候補にしない（[`sweep_cancel_requested`] が `cancelled` へ送る）
pub fn claim_next(conn: &Connection, ty: JobType, now: i64) -> Result<Option<Job>> {
    let candidate: Option<i64> = conn
        .query_row(
            "SELECT id FROM jobs
             WHERE state = 'queued' AND type = ?1
               AND cancel_requested_at IS NULL
               AND (run_after IS NULL OR run_after <= ?2)
             ORDER BY priority DESC, created_at ASC, id ASC
             LIMIT 1",
            params![ty.as_str(), now],
            |r| r.get(0),
        )
        .optional()?;
    let Some(id) = candidate else {
        return Ok(None);
    };
    let changed = conn.execute(
        "UPDATE jobs SET state = 'running', started_at = ?2
         WHERE id = ?1 AND state = 'queued' AND cancel_requested_at IS NULL",
        params![id, now],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    get(conn, id)
}

/// 次に `run_after` が来る時刻（待機中のジョブがあれば）。ワーカーの sleep 長に使う
pub fn next_run_after(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn.query_row(
        "SELECT min(run_after) FROM jobs WHERE state = 'queued' AND run_after IS NOT NULL",
        [],
        |r| r.get(0),
    )?)
}

/// 種別 `ty` の終端になったジョブのうち最新の `finished_at`（周期ジョブの due 判定に使う）
pub fn last_finished_at(conn: &Connection, ty: JobType) -> Result<Option<i64>> {
    Ok(conn.query_row(
        "SELECT max(finished_at) FROM jobs
         WHERE type = ?1 AND state IN ('done', 'failed', 'cancelled')",
        [ty.as_str()],
        |r| r.get(0),
    )?)
}

/// 指数バックオフの待ち秒数。`attempts` は失敗回数（1 始まり）。10 秒から倍々で 1 時間まで
pub fn backoff_secs(attempts: i64) -> i64 {
    const BASE: i64 = 10;
    const MAX: i64 = 3600;
    let exp = (attempts.max(1) - 1).min(30) as u32;
    BASE.saturating_mul(1i64 << exp).min(MAX)
}

/// 正常終了。`running` からのみ遷移する。`note` は完了時の結果 1 行（無ければ触らない）
pub fn mark_done(conn: &Connection, id: i64, now: i64, note: Option<&str>) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE jobs SET state = 'done', finished_at = ?2, progress = 1.0,
                         done = COALESCE(total, done), note = COALESCE(?3, note)
         WHERE id = ?1 AND state = 'running'",
        params![id, now, note],
    )?;
    Ok(changed > 0)
}

/// 失敗の結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureOutcome {
    /// `run_after` を書いて `queued` に戻した
    Retrying { run_after: i64, attempts: i64 },
    /// `max_attempts` に達して `failed`
    Failed { attempts: i64 },
    /// cancel 要求が立っていたので再試行せず `cancelled` にした（試行回数は数えない）
    Cancelled,
    /// `running` でなかったので何もしていない
    NotRunning,
}

/// 失敗: `attempts` を進め、上限未満なら `run_after` を書いて `queued` へ、達したら `failed`。
/// cancel 要求が立っていれば `cancelled`（キャンセルしたジョブをバックオフで再実行しない。D-36）
pub fn mark_failed(conn: &Connection, id: i64, error: &str, now: i64) -> Result<FailureOutcome> {
    let row: Option<(i64, i64, Option<i64>)> = conn
        .query_row(
            "SELECT attempts, max_attempts, cancel_requested_at FROM jobs
             WHERE id = ?1 AND state = 'running'",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((attempts, max_attempts, cancel_requested)) = row else {
        return Ok(FailureOutcome::NotRunning);
    };
    if cancel_requested.is_some() {
        conn.execute(
            "UPDATE jobs SET state = 'cancelled', last_error = ?2, finished_at = ?3
             WHERE id = ?1 AND state = 'running'",
            params![id, error, now],
        )?;
        return Ok(FailureOutcome::Cancelled);
    }
    let attempts = attempts + 1;
    if attempts >= max_attempts {
        conn.execute(
            "UPDATE jobs SET state = 'failed', attempts = ?2, last_error = ?3, finished_at = ?4
             WHERE id = ?1 AND state = 'running'",
            params![id, attempts, error, now],
        )?;
        return Ok(FailureOutcome::Failed { attempts });
    }
    let run_after = now + backoff_secs(attempts);
    conn.execute(
        "UPDATE jobs SET state = 'queued', attempts = ?2, last_error = ?3, run_after = ?4,
                         started_at = NULL
         WHERE id = ?1 AND state = 'running'",
        params![id, attempts, error, run_after],
    )?;
    Ok(FailureOutcome::Retrying {
        run_after,
        attempts,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequeueOutcome {
    Requeued {
        run_after: i64,
    },
    /// cancel 要求が立っていたので戻さず `cancelled` にした
    Cancelled,
    NotRunning,
}

/// 再試行しても直らない失敗（不正な payload 等）。試行回数に関わらず即 `failed`
pub fn mark_failed_permanently(conn: &Connection, id: i64, error: &str, now: i64) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE jobs SET state = 'failed', last_error = ?2, finished_at = ?3
         WHERE id = ?1 AND state = 'running'",
        params![id, error, now],
    )?;
    Ok(changed > 0)
}

/// ハンドラが `JobError::Fatal` で終わった: 試行回数を 1 進めて即 `failed`（バックオフを予約しない。
/// D-70）。`mark_failed_permanently` と違い、走った 1 回を `attempts` に数える
pub fn mark_failed_fatally(conn: &Connection, id: i64, error: &str, now: i64) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE jobs SET state = 'failed', attempts = attempts + 1, last_error = ?2,
                         run_after = NULL, finished_at = ?3
         WHERE id = ?1 AND state = 'running'",
        params![id, error, now],
    )?;
    Ok(changed > 0)
}

/// ロックが取れない等で実行前に戻す。試行回数は数えない。`delay_secs` 後に再度対象になる。
/// cancel 要求が立っていれば `cancelled`（D-36）
pub fn requeue(conn: &Connection, id: i64, now: i64, delay_secs: i64) -> Result<RequeueOutcome> {
    let cancelled = conn.execute(
        "UPDATE jobs SET state = 'cancelled', finished_at = ?2
         WHERE id = ?1 AND state = 'running' AND cancel_requested_at IS NOT NULL",
        params![id, now],
    )?;
    if cancelled > 0 {
        return Ok(RequeueOutcome::Cancelled);
    }
    let run_after = now + delay_secs;
    let changed = conn.execute(
        "UPDATE jobs SET state = 'queued', started_at = NULL, run_after = ?2
         WHERE id = ?1 AND state = 'running'",
        params![id, run_after],
    )?;
    Ok(if changed > 0 {
        RequeueOutcome::Requeued { run_after }
    } else {
        RequeueOutcome::NotRunning
    })
}

/// 協調キャンセルの完了。`running` からのみ遷移する
pub fn mark_cancelled(conn: &Connection, id: i64, now: i64) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE jobs SET state = 'cancelled', finished_at = ?2,
                         cancel_requested_at = COALESCE(cancel_requested_at, ?2)
         WHERE id = ?1 AND state = 'running'",
        params![id, now],
    )?;
    Ok(changed > 0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    NotFound,
    /// 未着手だったので即座に `cancelled` にした
    Cancelled,
    /// 実行中。`cancel_requested_at` を立てた。ハンドラが止まると `cancelled` になる
    Requested,
    /// 既に終端
    NotCancellable,
}

pub fn request_cancel(conn: &Connection, id: i64, now: i64) -> Result<CancelOutcome> {
    let state: Option<JobState> = conn
        .query_row("SELECT state FROM jobs WHERE id = ?1", [id], |r| {
            parse_col(r, 0)
        })
        .optional()?;
    match state {
        None => Ok(CancelOutcome::NotFound),
        Some(JobState::Queued) => {
            conn.execute(
                "UPDATE jobs SET state = 'cancelled', cancel_requested_at = ?2, finished_at = ?2
                 WHERE id = ?1 AND state = 'queued'",
                params![id, now],
            )?;
            Ok(CancelOutcome::Cancelled)
        }
        Some(JobState::Running) => {
            conn.execute(
                "UPDATE jobs SET cancel_requested_at = COALESCE(cancel_requested_at, ?2)
                 WHERE id = ?1 AND state = 'running'",
                params![id, now],
            )?;
            Ok(CancelOutcome::Requested)
        }
        Some(_) => Ok(CancelOutcome::NotCancellable),
    }
}

pub fn is_cancel_requested(conn: &Connection, id: i64) -> Result<bool> {
    let requested: Option<Option<i64>> = conn
        .query_row(
            "SELECT cancel_requested_at FROM jobs WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(matches!(requested, Some(Some(_))))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOutcome {
    NotFound,
    /// `queued` に戻した
    Requeued,
    /// `failed` / `cancelled` 以外は再試行できない
    NotRetryable,
    /// 同じ `dedup_key` の未完了ジョブがある
    Duplicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    NotFound,
    Deleted,
    /// `queued` / `running` は消せない（取り消してから）
    NotTerminal,
}

/// 終端（failed / done / cancelled）のジョブ行を 1 件消す（P4-18。確認済みの失敗の片付け）。
/// ロック表は CASCADE、`edit_ops` / `album_verifications` の `job_id` は SET NULL（履歴は残る）
pub fn delete_terminal(conn: &Connection, id: i64) -> Result<DeleteOutcome> {
    let state: Option<JobState> = conn
        .query_row("SELECT state FROM jobs WHERE id = ?1", [id], |r| {
            parse_col(r, 0)
        })
        .optional()?;
    let Some(state) = state else {
        return Ok(DeleteOutcome::NotFound);
    };
    if !matches!(
        state,
        JobState::Failed | JobState::Done | JobState::Cancelled
    ) {
        return Ok(DeleteOutcome::NotTerminal);
    }
    let n = conn.execute(
        "DELETE FROM jobs WHERE id = ?1 AND state IN ('failed','done','cancelled')",
        [id],
    )?;
    Ok(if n == 1 {
        DeleteOutcome::Deleted
    } else {
        DeleteOutcome::NotFound
    })
}

/// 保持期間を過ぎた終端の行を消す（GC。P4-18）: done / cancelled は `finished_at < done_before`、failed は
/// `finished_at < failed_before`。None は消さない。返り値は (done + cancelled, failed) の消した数
pub fn prune_terminal(
    conn: &Connection,
    done_before: Option<i64>,
    failed_before: Option<i64>,
) -> Result<(usize, usize)> {
    let done = match done_before {
        Some(t) => conn.execute(
            "DELETE FROM jobs WHERE state IN ('done','cancelled') AND finished_at IS NOT NULL AND finished_at < ?1",
            [t],
        )?,
        None => 0,
    };
    let failed = match failed_before {
        Some(t) => conn.execute(
            "DELETE FROM jobs WHERE state = 'failed' AND finished_at IS NOT NULL AND finished_at < ?1",
            [t],
        )?,
        None => 0,
    };
    Ok((done, failed))
}

/// [`prune_terminal`] が消す数（dry-run）
pub fn count_prunable(
    conn: &Connection,
    done_before: Option<i64>,
    failed_before: Option<i64>,
) -> Result<(usize, usize)> {
    let count = |sql: &str, t: i64| -> Result<usize> {
        let n: i64 = conn.query_row(sql, [t], |r| r.get(0))?;
        Ok(n.max(0) as usize)
    };
    let done = match done_before {
        Some(t) => count(
            "SELECT count(*) FROM jobs WHERE state IN ('done','cancelled') AND finished_at IS NOT NULL AND finished_at < ?1",
            t,
        )?,
        None => 0,
    };
    let failed = match failed_before {
        Some(t) => count(
            "SELECT count(*) FROM jobs WHERE state = 'failed' AND finished_at IS NOT NULL AND finished_at < ?1",
            t,
        )?,
        None => 0,
    };
    Ok((done, failed))
}

/// `failed` をすべて消す。返り値は消した数
pub fn delete_failed(conn: &Connection) -> Result<usize> {
    Ok(conn.execute("DELETE FROM jobs WHERE state = 'failed'", [])?)
}

/// 手動再試行。`failed` / `cancelled` を試行回数 0 で `queued` に戻す。
/// `last_error` は前回の理由として残す
pub fn retry(conn: &Connection, id: i64) -> Result<RetryOutcome> {
    let row: Option<(JobState, Option<String>)> = conn
        .query_row(
            "SELECT state, dedup_key FROM jobs WHERE id = ?1",
            [id],
            |r| Ok((parse_col(r, 0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((state, dedup_key)) = row else {
        return Ok(RetryOutcome::NotFound);
    };
    if !matches!(state, JobState::Failed | JobState::Cancelled) {
        return Ok(RetryOutcome::NotRetryable);
    }
    if let Some(key) = dedup_key {
        let active: Option<i64> = conn
            .query_row(
                "SELECT id FROM jobs WHERE dedup_key = ?1 AND state IN ('queued','running')",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        if active.is_some() {
            return Ok(RetryOutcome::Duplicate);
        }
    }
    conn.execute(
        "UPDATE jobs SET state = 'queued', attempts = 0, run_after = NULL,
                         cancel_requested_at = NULL, started_at = NULL, finished_at = NULL,
                         progress = NULL, done = NULL, total = NULL
         WHERE id = ?1 AND state IN ('failed','cancelled')",
        [id],
    )?;
    Ok(RetryOutcome::Requeued)
}

/// 進捗の永続化。`total` が 0 のときは progress を NULL にする
pub fn update_progress(conn: &Connection, id: i64, done: i64, total: i64) -> Result<()> {
    let progress = if total > 0 {
        Some((done as f64 / total as f64).clamp(0.0, 1.0))
    } else {
        None
    };
    conn.execute(
        "UPDATE jobs SET progress = ?2, done = ?3, total = ?4 WHERE id = ?1 AND state = 'running'",
        params![id, progress, done.max(0), total.max(0)],
    )?;
    Ok(())
}

/// `track_ids` を昇順に全件ロックする。1 つでも取れなければ何も残さず `false`
/// （SPEC §8 デッドロック回避）。同じジョブが既に持っているロックは取得済みとみなす
pub fn acquire_track_locks(
    conn: &Connection,
    job_id: i64,
    track_ids: &[i64],
    now: i64,
) -> Result<bool> {
    let mut ids: Vec<i64> = track_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    conn.execute("SAVEPOINT track_locks", [])?;
    let result = (|| -> Result<bool> {
        let mut stmt = conn.prepare_cached(
            "INSERT INTO track_locks (track_id, job_id, acquired_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(track_id) DO NOTHING",
        )?;
        let mut owned =
            conn.prepare_cached("SELECT 1 FROM track_locks WHERE track_id = ?1 AND job_id = ?2")?;
        for id in &ids {
            let inserted = stmt.execute(params![id, job_id, now])?;
            if inserted == 0 && !owned.exists(params![id, job_id])? {
                return Ok(false);
            }
        }
        Ok(true)
    })();
    match result {
        Ok(true) => {
            conn.execute("RELEASE track_locks", [])?;
            Ok(true)
        }
        Ok(false) => {
            conn.execute("ROLLBACK TO track_locks", [])?;
            conn.execute("RELEASE track_locks", [])?;
            Ok(false)
        }
        Err(e) => {
            let _ = conn.execute("ROLLBACK TO track_locks", []);
            let _ = conn.execute("RELEASE track_locks", []);
            Err(e)
        }
    }
}

pub fn release_track_locks(conn: &Connection, job_id: i64) -> Result<usize> {
    Ok(conn.execute("DELETE FROM track_locks WHERE job_id = ?1", [job_id])?)
}

/// 名前付き排他（`job_mutexes`）を `job_id` のために取る。別の running なジョブが持っていれば
/// false。持ち主が running でなくなっていれば無効として奪う。同じジョブの再取得は true
pub fn acquire_mutex(conn: &Connection, name: &str, job_id: i64, now: i64) -> Result<bool> {
    conn.execute(
        "DELETE FROM job_mutexes
          WHERE name = ?1 AND job_id NOT IN (SELECT id FROM jobs WHERE state = 'running')",
        [name],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO job_mutexes (name, job_id, acquired_at) VALUES (?1, ?2, ?3)",
        params![name, job_id, now],
    )?;
    let holder: i64 = conn.query_row(
        "SELECT job_id FROM job_mutexes WHERE name = ?1",
        [name],
        |r| r.get(0),
    )?;
    Ok(holder == job_id)
}

/// ジョブが持つ名前付き排他を全部解放する
pub fn release_mutexes(conn: &Connection, job_id: i64) -> Result<usize> {
    Ok(conn.execute("DELETE FROM job_mutexes WHERE job_id = ?1", [job_id])?)
}

pub fn track_exists(conn: &Connection, track_id: i64) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM tracks WHERE id = ?1", [track_id], |_| Ok(()))
        .optional()?
        .is_some())
}

/// 版付きジョブが stale か（payload の版 < 現在値、またはトラックが無い）。
/// 版を持たない種別、payload に版が無いものは stale ではない
/// 版付きジョブの payload に `"unversioned": true` があれば stale 判定をしない（同じ job type を
/// 版に関係ない op に使うとき。md5 op の tagwrite がこれ。D-59）。`track_locks` は通常どおり取る
pub const UNVERSIONED_KEY: &str = "unversioned";

pub fn is_unversioned(payload: &serde_json::Value) -> bool {
    payload.get(UNVERSIONED_KEY).and_then(|v| v.as_bool()) == Some(true)
}

pub fn is_stale(conn: &Connection, job: &Job) -> Result<bool> {
    let Some((payload_key, column)) = job.job_type.version_field() else {
        return Ok(false);
    };
    if is_unversioned(&job.payload) {
        return Ok(false);
    }
    let (Some(track_id), Some(version)) = (
        job.payload.get("track_id").and_then(|v| v.as_i64()),
        job.payload.get(payload_key).and_then(|v| v.as_i64()),
    ) else {
        return Ok(false);
    };
    // 列名は version_field() の固定値からのみ来る（生 SQL 組み立て禁止の例外ではなくホワイトリスト）
    let current: Option<i64> = conn
        .query_row(
            &format!("SELECT {column} FROM tracks WHERE id = ?1"),
            [track_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(match current {
        None => true,
        Some(current) => version < current,
    })
}

//! `backup` ジョブ（SPEC §8 / §14「バックアップ」、P0-13）。
//!
//! `VACUUM INTO` で `[paths].data/backup/` へ一貫したスナップショットを書く。WAL 中でも
//! 安全に取れるコピー手段で、読み取りコネクションで走らせるので書き手を止めない。
//! 手順は tmp に書く → `quick_check` → tmp を fsync → rename（`RENAME_NOREPLACE`。同名の
//! 確定済みを決して上書きしない）→ `backup/` を fsync → その親（`data`）も fsync → 世代 GC。
//! 途中で落ちても tmp は次回の実行が片付ける。
//!
//! ファイル名は `spindle-<YYYYMMDDTHHMMSSZ>.db`（UTC）。固定幅なので名前順 = 時刻順で、
//! 世代 GC はこの順で新しいものから `[backup].retention_generations` 件残す。
//!
//! 周期は [`spawn_scheduler`] が担う。due 判定は `jobs` 表の最後の終端 `backup` からの経過で、
//! ファイルの mtime には依らない（復元直後は古い DB の記録しか無いので、すぐ 1 世代取れる）

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use tokio_util::sync::CancellationToken;

use crate::db::jobs as dbjobs;
use crate::db::{now_epoch, Result as DbResult};
use crate::jobs::{
    BoxFuture, EnqueueResult, Handler, HandlerResult, JobContext, JobError, JobType, Jobs, NewJob,
    Outcome,
};

pub const DEDUP_KEY: &str = "backup";
/// `[paths].data` 直下のバックアップ置き場（SPEC §5）
pub const BACKUP_DIR_NAME: &str = "backup";

const FILE_PREFIX: &str = "spindle-";
const FILE_SUFFIX: &str = ".db";
const TMP_SUFFIX: &str = ".tmp";
/// `YYYYMMDDTHHMMSSZ`
const STAMP_LEN: usize = 16;

/// DB 本体のサイズに加えて空けておく余裕。VACUUM INTO の出力は元と同程度だが、
/// 同じデータセットに他のファイルも書かれるので少し余らせる
const DEFAULT_MIN_FREE_BYTES: u64 = 64 * 1024 * 1024;

/// スケジューラが due を見直す最長間隔。due になったら投入し、次はこの間隔の後に見直す
const SCHEDULER_TICK: Duration = Duration::from_secs(10 * 60);

pub fn new_backup_job() -> NewJob {
    NewJob::new(JobType::Backup, serde_json::json!({})).dedup_key(DEDUP_KEY)
}

pub async fn enqueue_backup(jobs: &Jobs) -> DbResult<EnqueueResult> {
    jobs.enqueue(new_backup_job()).await
}

// ---------------------------------------------------------------- 名前と時刻

/// UNIX epoch 秒を `YYYYMMDDTHHMMSSZ`（UTC、固定幅）にする。負の値は 1970-01-01 に丸める
pub fn format_utc(epoch: i64) -> String {
    let epoch = epoch.max(0);
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

/// `YYYYMMDDTHHMMSSZ` を UNIX epoch 秒に戻す。形式が違えば None
fn parse_utc(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != STAMP_LEN || b[8] != b'T' || b[15] != b'Z' {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> {
        let part = &s[from..to];
        if !part.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let (y, m, d) = (num(0, 4)?, num(4, 6)?, num(6, 8)?);
    let (hh, mm, ss) = (num(9, 11)?, num(11, 13)?, num(13, 15)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 59 {
        return None;
    }
    let days = days_from_civil(y, m, d);
    // 存在しない日付（2 月 30 日など）は往復で一致しない
    if civil_from_days(days) != (y, m, d) {
        return None;
    }
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// 1970-01-01 からの日数 → (年, 月, 日)。Howard Hinnant の civil_from_days
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (年, 月, 日) → 1970-01-01 からの日数。`civil_from_days` の逆
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 確定済みバックアップのファイル名
pub fn backup_file_name(epoch: i64) -> String {
    format!("{FILE_PREFIX}{}{FILE_SUFFIX}", format_utc(epoch))
}

/// 書き込み途中の一時ファイル名。先頭の `.` で確定済みと区別する
fn tmp_file_name(epoch: i64) -> String {
    format!(".{}{TMP_SUFFIX}", backup_file_name(epoch))
}

/// 名前がこのジョブの確定済みバックアップの形か
pub fn is_backup_file_name(name: &str) -> bool {
    parse_backup_file_name(name).is_some()
}

/// 確定済みバックアップの名前から時刻を取り出す
pub fn parse_backup_file_name(name: &str) -> Option<i64> {
    let stamp = name.strip_prefix(FILE_PREFIX)?.strip_suffix(FILE_SUFFIX)?;
    parse_utc(stamp)
}

fn is_tmp_file_name(name: &str) -> bool {
    name.strip_prefix('.')
        .and_then(|n| n.strip_suffix(TMP_SUFFIX))
        .is_some_and(is_backup_file_name)
}

// ---------------------------------------------------------------- スケジューラ

/// 最後の終端 `backup` から `interval_secs` 経ったか。一度も無ければ true。
/// 時計が戻って `last` が未来にあるときは due にしない（次の実機時刻で自然に解消する）
pub fn is_due(last: Option<i64>, now: i64, interval_secs: i64) -> bool {
    match last {
        None => true,
        Some(last) => now >= last && now - last >= interval_secs,
    }
}

/// 周期投入のタスクを起動する。起動直後に一度判定し、以後は due までの残りか
/// [`SCHEDULER_TICK`] の短い方だけ待って見直す。投入の重複は dedup key が防ぐ
pub fn spawn_scheduler(
    jobs: Arc<Jobs>,
    interval_hours: u32,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    spawn_scheduler_with(
        jobs,
        i64::from(interval_hours) * 3600,
        SCHEDULER_TICK,
        shutdown,
    )
}

/// [`spawn_scheduler`] の本体。間隔と見直しの最長間隔を直接指定する（テスト用）
pub fn spawn_scheduler_with(
    jobs: Arc<Jobs>,
    interval_secs: i64,
    tick: Duration,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let wait = match jobs
                .db()
                .read(|c| dbjobs::last_finished_at(c, JobType::Backup))
                .await
            {
                Ok(last) => {
                    let now = now_epoch();
                    if is_due(last, now, interval_secs) {
                        match enqueue_backup(&jobs).await {
                            Ok(EnqueueResult::Inserted(id)) => {
                                tracing::info!(job_id = id, "定期バックアップを投入した")
                            }
                            Ok(EnqueueResult::Duplicate(_)) => {}
                            Err(e) => tracing::warn!(error = %e, "定期バックアップを投入できない"),
                        }
                        tick
                    } else {
                        let remaining = last.map_or(0, |l| l + interval_secs - now).max(1);
                        tick.min(Duration::from_secs(remaining as u64))
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "バックアップの due 判定に失敗");
                    tick
                }
            };
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(wait) => {}
            }
        }
    })
}

// ---------------------------------------------------------------- ハンドラ

pub struct BackupHandler {
    dir: PathBuf,
    retention: usize,
    min_free_bytes: u64,
}

impl BackupHandler {
    /// `dir` はバックアップ置き場（通常 `[paths].data/backup`）、`retention` は残す世代数
    pub fn new(dir: impl Into<PathBuf>, retention_generations: u32) -> Self {
        Self {
            dir: dir.into(),
            retention: (retention_generations as usize).max(1),
            min_free_bytes: DEFAULT_MIN_FREE_BYTES,
        }
    }

    /// DB サイズに加えて要求する空き容量（既定 64 MiB）。テストで容量不足を再現する用
    pub fn with_min_free_bytes(mut self, bytes: u64) -> Self {
        self.min_free_bytes = bytes;
        self
    }

    async fn run_inner(&self, ctx: &JobContext) -> HandlerResult {
        const STEPS: i64 = 4;
        let dir = self.dir.clone();
        ctx.check_cancel().await?;

        // 1. 置き場の用意と前回の取り残し掃除
        {
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || prepare_dir(&dir))
                .await
                .map_err(|e| JobError::Failed(e.into()))??;
        }
        ctx.progress(0, STEPS).await?;

        // 2. 容量確認。VACUUM INTO の出力は元の DB とほぼ同じ大きさ
        let db_size = ctx
            .db()
            .read(|c| {
                let pages: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0))?;
                let page_size: i64 = c.query_row("PRAGMA page_size", [], |r| r.get(0))?;
                Ok((pages.max(0) as u64).saturating_mul(page_size.max(0) as u64))
            })
            .await?;
        let need = db_size.saturating_add(self.min_free_bytes);
        let free = free_bytes(&dir)?;
        if free < need {
            return Err(JobError::Failed(anyhow!(
                "バックアップ先の容量不足: 空き {free} バイト、必要 {need} バイト（{}）",
                dir.display()
            )));
        }
        ctx.progress(1, STEPS).await?;

        // 3. tmp へ VACUUM INTO。読み取りコネクションで走らせ、書き手を止めない
        let stamp = now_epoch();
        let tmp = ctx.temp_file(dir.join(tmp_file_name(stamp)));
        // 同じ秒に 2 回は走らない想定だが、走ったら commit の RENAME_NOREPLACE が
        // 既存を守って失敗し、再試行に回る
        let final_path = dir.join(backup_file_name(stamp));
        let tmp_str = tmp
            .path()
            .to_str()
            .ok_or_else(|| anyhow!("バックアップ先のパスが UTF-8 でない: {}", dir.display()))?
            .to_owned();
        ctx.db()
            .read(move |c| {
                c.execute("VACUUM INTO ?1", [tmp_str.as_str()])?;
                Ok(())
            })
            .await
            .context("VACUUM INTO に失敗")?;
        ctx.progress(2, STEPS).await?;

        // 4. 検証 → fsync → rename（no-replace）→ backup/ と data の fsync
        {
            let tmp_path = tmp.path().to_path_buf();
            let final_path = final_path.clone();
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || commit(&tmp_path, &final_path, &dir))
                .await
                .map_err(|e| JobError::Failed(e.into()))??;
        }
        // rename 済みなので tmp のガードは外す（消す対象がもう無い）
        let _ = tmp.keep();
        tracing::info!(path = %final_path.display(), bytes = db_size, "バックアップを書いた");
        ctx.progress(3, STEPS).await?;

        // 5. 世代 GC。新しい順に retention 件残す
        let retention = self.retention;
        let removed = tokio::task::spawn_blocking(move || gc_generations(&dir, retention))
            .await
            .map_err(|e| JobError::Failed(e.into()))??;
        if !removed.is_empty() {
            tracing::info!(removed = ?removed, retention, "古いバックアップ世代を削除した");
        }
        ctx.progress(STEPS, STEPS).await?;
        Ok(Outcome::Done)
    }
}

impl Handler for BackupHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let this = BackupHandler {
            dir: self.dir.clone(),
            retention: self.retention,
            min_free_bytes: self.min_free_bytes,
        };
        Box::pin(async move { this.run_inner(&ctx).await })
    }
}

/// 置き場を作り、前回の実行が残した tmp を消す
fn prepare_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("バックアップ先を作れない: {}", dir.display()))?;
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("バックアップ先を読めない: {}", dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if is_tmp_file_name(name) {
            let path = entry.path();
            match std::fs::remove_file(&path) {
                Ok(()) => tracing::warn!(path = %path.display(), "前回の一時ファイルを消した"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("一時ファイルを消せない: {}", path.display()))
                }
            }
        }
    }
    Ok(())
}

/// 非特権ユーザが使える空き容量（バイト）
fn free_bytes(dir: &Path) -> anyhow::Result<u64> {
    let st = rustix::fs::statvfs(dir)
        .with_context(|| format!("空き容量を取得できない: {}", dir.display()))?;
    Ok(st.f_bavail.saturating_mul(st.f_frsize))
}

/// tmp を検証して確定する: quick_check → tmp fsync → rename（`RENAME_NOREPLACE`）→
/// `dir` の fsync → `dir` の親の fsync。
/// 同名の確定済みがあれば rename が `EEXIST` で失敗し、既存は触らない（tmp は呼び出し側の
/// ガードが消す）。親まで fsync するのは、`dir` を今回初めて作った場合にそのエントリの
/// 永続化が親ディレクトリ側にあるため（判定せず常に行う。1 回の fsync で済む）
fn commit(tmp: &Path, final_path: &Path, dir: &Path) -> anyhow::Result<()> {
    {
        let conn = rusqlite::Connection::open_with_flags(
            tmp,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("書いたバックアップを開けない: {}", tmp.display()))?;
        let verdict: String = conn
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .context("quick_check に失敗")?;
        if verdict != "ok" {
            return Err(anyhow!("書いたバックアップが壊れている: {verdict}"));
        }
    }
    std::fs::File::open(tmp)
        .and_then(|f| f.sync_all())
        .with_context(|| format!("fsync に失敗: {}", tmp.display()))?;
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        tmp,
        rustix::fs::CWD,
        final_path,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .with_context(|| {
        format!(
            "rename に失敗（同名のバックアップが既にあるなら上書きしない）: {} → {}",
            tmp.display(),
            final_path.display()
        )
    })?;
    // rename が永続化されるのはディレクトリの fsync 後。dir 自体の作成は親の fsync が要る
    fsync_dir(dir)?;
    if let Some(parent) = dir.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

fn fsync_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("ディレクトリの fsync に失敗: {}", dir.display()))
}

/// 確定済みバックアップを名前の降順（= 新しい順）に並べ、`retention` 件より古いものを消す。
/// 消した名前を返す
fn gc_generations(dir: &Path, retention: usize) -> anyhow::Result<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .with_context(|| format!("バックアップ先を読めない: {}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| is_backup_file_name(n))
        .collect();
    names.sort_unstable_by(|a, b| b.cmp(a));
    let mut removed = Vec::new();
    for name in names.into_iter().skip(retention) {
        let path = dir.join(&name);
        match std::fs::remove_file(&path) {
            Ok(()) => removed.push(name),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("古いバックアップを消せない: {}", path.display()))
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_round_trips() {
        for epoch in [
            0,
            86_399,
            951_782_400,
            1_709_210_096,
            1_789_516_800,
            4_102_444_799,
        ] {
            let s = format_utc(epoch);
            assert_eq!(parse_utc(&s), Some(epoch), "{s}");
        }
        assert_eq!(parse_utc("20240230T000000Z"), None);
        assert_eq!(parse_utc("2024022T0000000Z"), None);
        assert_eq!(parse_utc("20240229T123456"), None);
    }

    /// 確定先に同名がある競合では rename 自体が失敗し、既存ファイルは変わらない
    #[test]
    fn commit_never_replaces_an_existing_final() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("backup");
        std::fs::create_dir(&dir).unwrap();
        // 既存の確定済み（中身は本物の SQLite でなくてよい。触られないことだけ見る）
        let final_path = dir.join(backup_file_name(1_709_210_096));
        std::fs::write(&final_path, b"existing").unwrap();
        // tmp は quick_check を通る本物の DB にする
        let tmp = dir.join(tmp_file_name(1_709_210_096));
        rusqlite::Connection::open(&tmp)
            .unwrap()
            .execute_batch("CREATE TABLE t (x)")
            .unwrap();

        let err = commit(&tmp, &final_path, &dir).unwrap_err();
        assert!(err.to_string().contains("rename"), "{err:#}");
        assert_eq!(std::fs::read(&final_path).unwrap(), b"existing");
        assert!(tmp.exists());
    }

    #[test]
    fn tmp_names_are_distinct_from_final_names() {
        let tmp = tmp_file_name(1_709_210_096);
        assert!(is_tmp_file_name(&tmp));
        assert!(!is_backup_file_name(&tmp));
        assert!(!is_tmp_file_name(&backup_file_name(1_709_210_096)));
    }
}

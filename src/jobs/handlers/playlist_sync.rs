//! `playlist_sync` ジョブ（SPEC §7.7「再生リストの購読」、D-78、P4-16）。並列 1、dedup
//! `playlist_sync:<subscription_id>`、payload `{ "subscription_id" }`。
//!
//! ```text
//! begin     latch（sync_requested_at）を消し last_attempted_at を書く
//! list      yt-dlp --flat-playlist --dump-single-json（entries < playlist_count なら取りこぼしとして失敗）
//! resolve   追記先の album（album_id が束ねてあればそれ、無ければ albumartist / album / category から
//!           引いて CAS で束ねる。まだ無ければ揃えは無し）
//! match     entry ごとに Library の active 行（全件）と Inbox の有無
//! align     位置 ↔ TRACKNUMBER の差分 → tags バッチ → 終端待ち → 番号が合った行の rename → 終端待ち
//! enqueue   Library にも Inbox にも無くて取れる entry を位置順に max_enqueue まで ytdl 投入
//! finish    last_result / last_synced_at。走行中に latch が立っていれば Requeue
//! ```
//!
//! 順序は align → enqueue（購読由来の ytdl は TRACKNUMBER = 位置を書くので、先に既存行を揃えて隙間を
//! 空ける）。phase は永続化しない: 各段の前に追記先 album の active 行に pending の op が無くなるまで
//! 待ち、差分は待った後の DB から計算し直す。落ちて再実行しても、失敗した op は overlay がファイルの
//! 値に戻るので次回また差分になり、適用済みなら差分ゼロで通る。rename の候補は「現在の TRACKNUMBER が
//! 目標位置と一致する対象行の全部」（今回のバッチの applied 集合には依らない。tags 適用後・rename 前に
//! 落ちた境界を再実行で拾う）

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::config::LayoutConfig;
use crate::db::history::{self, BatchCounts};
use crate::db::inbox::{album_rows, inbox_has_source_url, library_rows_by_source_url};
use crate::db::subscriptions::{self, BindOutcome, Subscription};
use crate::db::{now_epoch, Db};
use crate::domain::pathgen::Planned;
use crate::domain::relpath::RelPath;
use crate::domain::tags::TagChange;
use crate::edit::{EditError, Editor, NewTagOp, RenameTarget};
use crate::import::inbox::destination_of;
use crate::import::ytmusic::downloader::new_subscription_ytdl_job;
use crate::import::ytmusic::playlist::{
    parse_playlist_dump, plan_align, AlignBlocked, Availability, PlaylistDump, PlaylistEntry,
    UnavailableKind,
};
use crate::jobs::process::{ExternalCommand, ProcessError};
use crate::jobs::{
    BoxFuture, EnqueueResult, Handler, HandlerResult, JobContext, JobError, JobType, Jobs, NewJob,
    Outcome,
};

pub const DEDUP_PREFIX: &str = "playlist_sync:";
/// 列挙（yt-dlp）の上限
const LIST_TIMEOUT: Duration = Duration::from_secs(180);

pub fn new_sync_job(subscription_id: i64) -> NewJob {
    NewJob::new(
        JobType::PlaylistSync,
        serde_json::json!({ "subscription_id": subscription_id }),
    )
    .dedup_key(format!("{DEDUP_PREFIX}{subscription_id}"))
}

pub struct SyncEnv {
    pub db: Arc<Db>,
    pub jobs: Arc<Jobs>,
    pub editor: Arc<Editor>,
    pub layout: LayoutConfig,
    /// yt-dlp（引数配列。先頭がプログラム）
    pub ytdlp: Vec<String>,
    /// pending の op が無くなるまで待つ上限と間隔
    pub pending_wait: Duration,
    pub pending_poll: Duration,
}

impl SyncEnv {
    pub fn default_waits() -> (Duration, Duration) {
        (Duration::from_secs(600), Duration::from_secs(2))
    }
}

// ---------------------------------------------------------------- 結果（last_result）

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unavailable {
    pub position: u32,
    pub id: String,
    pub kind: UnavailableKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Elsewhere {
    pub position: u32,
    pub id: String,
    pub track_id: i64,
    pub rel_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatchOutcome {
    pub batch_id: i64,
    pub total: i64,
    pub pending: i64,
    pub applied: i64,
    pub conflict: i64,
    pub failed: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AlignResult {
    /// 動かした（tags バッチに載せた）行数
    pub moved: usize,
    pub unchanged: usize,
    pub blocked: Vec<AlignBlocked>,
    pub outsiders: usize,
    pub unnumbered: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<BatchOutcome>,
    /// 名前を変えた行数（rename バッチに載せた）
    pub renamed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rename: Option<BatchOutcome>,
    /// 計画の時点で名前が衝突して改名できない行（同名のファイルがある等。触らない）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rename_conflicts: Vec<RenameConflict>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RenameConflict {
    pub track_id: i64,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SyncResult {
    /// `done` / `failed` / `cancelled`
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub synced_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub entries: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_id: Option<i64>,
    /// 追記先 album にある entry の数
    pub in_library: usize,
    /// Inbox にある（取り込み中）entry の数
    pub in_inbox: usize,
    /// 別の album にある（触らない）
    pub elsewhere: Vec<Elsewhere>,
    /// 投入した entry の位置
    pub enqueued: Vec<u32>,
    /// 別の投入が走行中で投入しなかった entry の位置（次回に Library / 再投入で解決）
    pub running: Vec<u32>,
    /// 上限で次回に持ち越した数
    pub deferred: usize,
    pub unavailable: Vec<Unavailable>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub align: Option<AlignResult>,
}

impl SyncResult {
    /// `jobs.note` に残す 1 行
    pub fn summary(&self) -> String {
        let mut parts = vec![format!(
            "{} 件中 Library に {}",
            self.entries, self.in_library
        )];
        if !self.enqueued.is_empty() {
            parts.push(format!("{} 件を投入", self.enqueued.len()));
        }
        if self.in_inbox > 0 {
            parts.push(format!("{} 件は Inbox で取り込み中", self.in_inbox));
        }
        if !self.running.is_empty() {
            parts.push(format!("{} 件は別の投入が走行中", self.running.len()));
        }
        if self.deferred > 0 {
            parts.push(format!("{} 件は次回", self.deferred));
        }
        if !self.unavailable.is_empty() {
            parts.push(format!("{} 件は取れない", self.unavailable.len()));
        }
        if !self.elsewhere.is_empty() {
            parts.push(format!("{} 件は別の album", self.elsewhere.len()));
        }
        if let Some(a) = &self.align {
            if a.moved > 0 || a.renamed > 0 {
                parts.push(format!("番号を {} 件揃え {} 件を改名", a.moved, a.renamed));
            }
            if !a.blocked.is_empty() {
                parts.push(format!("{} 件は揃えられない", a.blocked.len()));
            }
            if !a.rename_conflicts.is_empty() {
                parts.push(format!("{} 件は改名できない", a.rename_conflicts.len()));
            }
        }
        parts.join("、")
    }
}

// ---------------------------------------------------------------- 本体

pub struct PlaylistSyncHandler {
    env: Arc<SyncEnv>,
}

impl PlaylistSyncHandler {
    pub fn new(env: SyncEnv) -> Self {
        Self { env: Arc::new(env) }
    }
}

#[derive(Debug, thiserror::Error)]
enum SyncError {
    /// 再試行しても変わらない
    #[error("{0}")]
    Fatal(String),
    #[error(transparent)]
    Failed(anyhow::Error),
    #[error("キャンセルされた")]
    Cancelled,
}

impl From<crate::db::DbError> for SyncError {
    fn from(e: crate::db::DbError) -> Self {
        SyncError::Failed(e.into())
    }
}

impl From<ProcessError> for SyncError {
    fn from(e: ProcessError) -> Self {
        match e {
            ProcessError::Cancelled => SyncError::Cancelled,
            other => SyncError::Failed(other.into()),
        }
    }
}

impl From<EditError> for SyncError {
    fn from(e: EditError) -> Self {
        match e {
            EditError::Cancelled => SyncError::Cancelled,
            other => SyncError::Failed(other.into()),
        }
    }
}

impl Handler for PlaylistSyncHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let env = Arc::clone(&self.env);
        Box::pin(async move {
            let Some(id) = ctx
                .job
                .payload
                .get("subscription_id")
                .and_then(|v| v.as_i64())
            else {
                return Err(JobError::Fatal(anyhow::anyhow!(
                    "payload に subscription_id が無い"
                )));
            };
            let now = now_epoch();
            let Some(sub) = env
                .db
                .write(move |c| subscriptions::begin_attempt(c, id, now))
                .await?
            else {
                return Err(JobError::Fatal(anyhow::anyhow!("購読 #{id} は消えている")));
            };
            let token = ctx.cancel_token();
            let mut result = SyncResult {
                synced_at: now,
                ..SyncResult::default()
            };
            let outcome = sync_one(&env, &ctx, &sub, &token, &mut result).await;
            let (state, error) = match &outcome {
                Ok(()) => ("done", None),
                Err(SyncError::Cancelled) => ("cancelled", None),
                Err(e) => ("failed", Some(format!("{e:#}"))),
            };
            result.state = state.to_owned();
            result.error = error;
            let value = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::json!({}));
            let ok = outcome.is_ok();
            let finished = now_epoch();
            env.db
                .write(move |c| {
                    if ok {
                        subscriptions::finish_attempt(c, id, finished, &value)
                    } else {
                        subscriptions::set_result(c, id, &value)
                    }
                })
                .await?;
            match outcome {
                Ok(()) => {
                    // 走行中に要求（承認の後続・手動）が来ていればもう一度（試行回数は数えない）。
                    // ここから終端までの窓に来た分は dispatcher が拾う
                    if env
                        .db
                        .read(move |c| subscriptions::is_requested(c, id))
                        .await?
                    {
                        tracing::info!(
                            subscription_id = id,
                            "走行中に要求が来たのでもう一度同期する"
                        );
                        return Ok(Outcome::Requeue);
                    }
                    Ok(Outcome::DoneWith(result.summary()))
                }
                Err(SyncError::Cancelled) => Err(JobError::Cancelled),
                Err(SyncError::Fatal(m)) => Err(JobError::Fatal(anyhow::anyhow!(m))),
                Err(SyncError::Failed(e)) => Err(JobError::Failed(e)),
            }
        })
    }
}

/// 追記先 album の解決結果
struct Target {
    album_id: i64,
}

async fn sync_one(
    env: &SyncEnv,
    ctx: &JobContext,
    sub: &Subscription,
    token: &CancellationToken,
    result: &mut SyncResult,
) -> Result<(), SyncError> {
    let id = sub.id;
    // 1. 列挙
    let dump = list_playlist(env, &sub.url, token).await?;
    result.title = dump.title.clone();
    result.entries = dump.entries.len();
    result.playlist_count = dump.playlist_count;
    if dump.truncated() {
        return Err(SyncError::Failed(anyhow::anyhow!(
            "yt-dlp が再生リストを取りこぼした（{} 件中 {} 件しか列挙できない）。yt-dlp を更新してください",
            dump.playlist_count.unwrap_or(0),
            dump.entries.len()
        )));
    }
    ctx.check_cancel().await.map_err(|_| SyncError::Cancelled)?;
    // 2. 追記先（列挙の間に PATCH で変わっていたらやり直す）
    check_unchanged(env, sub).await?;
    let target = resolve_target(env, sub).await?;
    result.album_id = target.as_ref().map(|t| t.album_id);
    // 3. 照合
    let entries = Arc::new(dump.entries.clone());
    let matched = {
        let entries = Arc::clone(&entries);
        let album_id = target.as_ref().map(|t| t.album_id);
        env.db
            .read(move |c| {
                let mut out = Vec::with_capacity(entries.len());
                for e in entries.iter() {
                    let rows = library_rows_by_source_url(c, &e.url)?;
                    let in_inbox = rows.is_empty() && inbox_has_source_url(c, &e.url)?;
                    out.push(Matched {
                        in_target: rows
                            .iter()
                            .any(|r| album_id.is_some() && r.album_id == album_id),
                        elsewhere: rows
                            .iter()
                            .filter(|r| album_id.is_none() || r.album_id != album_id)
                            .map(|r| (r.track_id, r.rel_path.clone()))
                            .collect(),
                        in_inbox,
                    });
                }
                Ok(out)
            })
            .await?
    };
    for (e, m) in entries.iter().zip(&matched) {
        if m.in_target {
            result.in_library += 1;
        } else if m.in_inbox {
            result.in_inbox += 1;
        }
        for (track_id, rel_path) in &m.elsewhere {
            result.elsewhere.push(Elsewhere {
                position: e.position,
                id: e.id.clone(),
                track_id: *track_id,
                rel_path: rel_path.clone(),
            });
        }
        if let Availability::Unavailable(kind) = e.availability {
            if !m.in_target && m.elsewhere.is_empty() {
                result.unavailable.push(Unavailable {
                    position: e.position,
                    id: e.id.clone(),
                    kind,
                });
            }
        }
    }
    // 4. 揃え
    if let Some(t) = &target {
        if sub.align {
            check_unchanged(env, sub).await?;
            // 途中で失敗しても、そこまでの結果（バッチ id と件数）は残す
            let mut a = AlignResult::default();
            let r = align(env, ctx, sub, t.album_id, &entries, &mut a).await;
            result.align = Some(a);
            r?;
        }
    }
    // 5. 投入（Library / Inbox に無く、取れるものを位置順に上限まで。揃えの間に購読が変わっていたら投入しない）
    check_unchanged(env, sub).await?;
    let mut budget = usize::try_from(sub.max_enqueue).unwrap_or(usize::MAX);
    let mut seen_urls: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (e, m) in entries.iter().zip(&matched) {
        if m.in_target || m.in_inbox || !m.elsewhere.is_empty() {
            continue;
        }
        if e.availability != Availability::Available {
            continue;
        }
        // 同じ動画が複数回あれば最初の位置でだけ投入する
        if !seen_urls.insert(e.url.as_str()) {
            continue;
        }
        if budget == 0 {
            result.deferred += 1;
            continue;
        }
        ctx.check_cancel().await.map_err(|_| SyncError::Cancelled)?;
        match env
            .jobs
            .enqueue(new_subscription_ytdl_job(&e.url, id, e.position))
            .await?
        {
            EnqueueResult::Inserted(_) => {
                budget -= 1;
                result.enqueued.push(e.position);
            }
            EnqueueResult::Duplicate(_) => result.running.push(e.position),
        }
    }
    tracing::info!(
        subscription_id = id,
        entries = result.entries,
        in_library = result.in_library,
        enqueued = result.enqueued.len(),
        "再生リストを同期した"
    );
    Ok(())
}

struct Matched {
    in_target: bool,
    elsewhere: Vec<(i64, String)>,
    in_inbox: bool,
}

async fn list_playlist(
    env: &SyncEnv,
    url: &str,
    token: &CancellationToken,
) -> Result<PlaylistDump, SyncError> {
    let program = env.ytdlp.first().map(String::as_str).unwrap_or("yt-dlp");
    let out = ExternalCommand::new(program)
        .args(env.ytdlp.iter().skip(1))
        .args([
            "--dump-single-json",
            "--flat-playlist",
            "--no-download",
            "--no-warnings",
        ])
        .timeout(LIST_TIMEOUT)
        .arg("--")
        .arg(url)
        .run(token)
        .await?;
    parse_playlist_dump(&out.stdout)
        .map_err(|e| SyncError::Fatal(format!("yt-dlp の出力を読めない: {e}")))
}

/// 購読が消えていれば Fatal、PATCH で変わっていれば（`updated_at` が進んだ）Failed（再試行で新しい行から
/// やり直す。走行中の同期が古い追記先で揃えたり投入したりしない）
async fn check_unchanged(env: &SyncEnv, sub: &Subscription) -> Result<(), SyncError> {
    let (id, updated_at) = (sub.id, sub.updated_at);
    let now = env.db.read(move |c| subscriptions::get(c, id)).await?;
    match now {
        None => Err(SyncError::Fatal(format!("購読 #{id} は消えている"))),
        Some(s) if s.updated_at != updated_at => Err(SyncError::Failed(anyhow::anyhow!(
            "同期の間に購読が変更されたのでやり直す"
        ))),
        Some(_) => Ok(()),
    }
}

/// 追記先 album。束ねてあればその album（missing なら失敗）、無ければ引いて CAS で束ねる。
/// まだ無ければ None（揃えは無し。最初の配置で束ねる）
async fn resolve_target(env: &SyncEnv, sub: &Subscription) -> Result<Option<Target>, SyncError> {
    let (id, layout, sub) = (sub.id, env.layout.clone(), sub.clone());
    let found: Result<Option<i64>, String> = env
        .db
        .write(move |c| {
            if let Some(album_id) = sub.album_id {
                let active: i64 = c.query_row(
                    "SELECT count(*) FROM albums WHERE id = ?1 AND missing_since IS NULL",
                    [album_id],
                    |r| r.get(0),
                )?;
                return Ok(if active > 0 {
                    Ok(Some(album_id))
                } else {
                    Err(format!(
                        "追記先の album #{album_id} が無い（消えたか、まだ走査で拾われていない）"
                    ))
                });
            }
            let dest = destination_of(
                c,
                &layout,
                sub.category.as_deref(),
                &sub.albumartist,
                &sub.album,
            )
            .map_err(|e| crate::db::DbError::Internal(e.to_string()))?;
            let Some(dest) = dest else {
                return Ok(Ok(None));
            };
            Ok(match subscriptions::bind_album(c, id, dest.album_id)? {
                BindOutcome::Bound => Ok(Some(dest.album_id)),
                // 読んでから束ねるまでに配置が束ねた: そちらが正
                BindOutcome::AlreadyBound(bound) => Ok(Some(bound)),
                BindOutcome::TakenBy(other) => Err(format!(
                    "追記先の album #{} は購読 #{other} が使っている",
                    dest.album_id
                )),
                BindOutcome::NotFound => Err(format!("購読 #{id} は消えている")),
            })
        })
        .await?;
    match found {
        Ok(album_id) => Ok(album_id.map(|album_id| Target { album_id })),
        Err(m) => Err(SyncError::Fatal(m)),
    }
}

/// 追記先 album の active 行に pending の op が無くなるまで待つ（上限で Failed）
async fn wait_pending(env: &SyncEnv, ctx: &JobContext, album_id: i64) -> Result<(), SyncError> {
    let started = Instant::now();
    loop {
        let pending = env
            .db
            .read(move |c| {
                let ids: Vec<i64> = album_rows(c, album_id)?
                    .into_iter()
                    .map(|r| r.track_id)
                    .collect();
                history::pending_track_ids(c, &ids)
            })
            .await?;
        if pending.is_empty() {
            return Ok(());
        }
        if started.elapsed() >= env.pending_wait {
            return Err(SyncError::Failed(anyhow::anyhow!(
                "album #{album_id} に反映待ちの編集が残っている（{} 件）ので揃えを見送った",
                pending.len()
            )));
        }
        ctx.check_cancel().await.map_err(|_| SyncError::Cancelled)?;
        tokio::time::sleep(env.pending_poll).await;
    }
}

async fn batch_outcome(env: &SyncEnv, batch_id: i64) -> Result<BatchOutcome, SyncError> {
    let c: BatchCounts = env
        .db
        .read(move |c| history::batch_counts(c, batch_id))
        .await?;
    Ok(BatchOutcome {
        batch_id,
        total: c.total(),
        pending: c.pending,
        applied: c.applied,
        conflict: c.conflict,
        failed: c.failed,
    })
}

/// 子バッチの op が全部終端になるまで待つ（album の行でなく batch_id で見る: 対象が走査で missing に
/// なっても pending の op を見落とさない）。上限で Failed
async fn wait_batch(env: &SyncEnv, ctx: &JobContext, batch_id: i64) -> Result<(), SyncError> {
    let started = Instant::now();
    loop {
        let c: BatchCounts = env
            .db
            .read(move |c| history::batch_counts(c, batch_id))
            .await?;
        if c.pending == 0 {
            return Ok(());
        }
        if started.elapsed() >= env.pending_wait {
            return Err(SyncError::Failed(anyhow::anyhow!(
                "バッチ #{batch_id} の反映が終わらない（{} 件が反映待ち）ので揃えを見送った",
                c.pending
            )));
        }
        ctx.check_cancel().await.map_err(|_| SyncError::Cancelled)?;
        tokio::time::sleep(env.pending_poll).await;
    }
}

async fn align(
    env: &SyncEnv,
    ctx: &JobContext,
    sub: &Subscription,
    album_id: i64,
    entries: &Arc<Vec<PlaylistEntry>>,
    out: &mut AlignResult,
) -> Result<(), SyncError> {
    let description = format!("再生リスト『{}』に番号を揃える", sub.album.trim());
    // a. tags（差分は pending が無くなった後の DB から）
    wait_pending(env, ctx, album_id).await?;
    let rows = env.db.read(move |c| album_rows(c, album_id)).await?;
    let plan = plan_align(entries, &rows);
    out.unchanged = plan.unchanged;
    out.blocked = plan.blocked.clone();
    out.outsiders = plan.outsiders;
    out.unnumbered = plan.unnumbered;
    if !plan.moves.is_empty() {
        let ops: Vec<NewTagOp> = plan
            .moves
            .iter()
            .map(|m| NewTagOp {
                track_id: m.track_id,
                changes: vec![TagChange {
                    key: "TRACKNUMBER".to_owned(),
                    values: Some(vec![m.target_no.to_string()]),
                }],
            })
            .collect();
        out.moved = ops.len();
        match env.editor.prepare_tags(Some(&description), ops).await {
            Ok(p) => {
                tracing::info!(
                    subscription_id = sub.id,
                    batch_id = p.batch_id,
                    moved = out.moved,
                    "番号を揃える"
                );
                wait_batch(env, ctx, p.batch_id).await?;
                let b = batch_outcome(env, p.batch_id).await?;
                out.tags = Some(b.clone());
                require_all_applied("番号", &b)?;
            }
            // 直前に別の編集が入った: 待ってやり直す代わりに今回は失敗（再試行で差分から取り直す）
            Err(EditError::Pending { track_ids }) => {
                return Err(SyncError::Failed(anyhow::anyhow!(
                    "反映待ちの編集が入った（{} 件）ので揃えをやり直す",
                    track_ids.len()
                )));
            }
            Err(EditError::NoChanges) => {}
            Err(e) => return Err(e.into()),
        }
    }
    // b. rename: 現在の TRACKNUMBER が目標位置と一致する対象行のうち、**ファイル名の先頭の番号が
    //    合っていない行**（今回の applied には依らない = 番号だけ直して落ちた境界も拾う）。変えるのは
    //    ファイル名だけで、ディレクトリは今のまま（テンプレートの dir は category 未推定の album を
    //    _Unsorted へ動かしてしまう。名前の書式だけ違う行も触らない）
    let rows = env.db.read(move |c| album_rows(c, album_id)).await?;
    let position_of: HashMap<&str, u32> = entries
        .iter()
        .map(|e| (e.url.as_str(), e.position))
        .collect();
    let mut url_count: HashMap<&str, usize> = HashMap::new();
    for r in &rows {
        if let Some(u) = r.source_url.as_deref() {
            *url_count.entry(u).or_default() += 1;
        }
    }
    // 同じ動画が複数回ある URL は対象外（plan_align と同じ）
    let mut entry_count: HashMap<&str, usize> = HashMap::new();
    for e in entries.iter() {
        *entry_count.entry(e.url.as_str()).or_default() += 1;
    }
    let numbered: Vec<i64> = rows
        .iter()
        .filter(|r| r.disc_no.unwrap_or(1) == 1)
        .filter(|r| {
            r.source_url.as_deref().is_some_and(|u| {
                url_count.get(u) == Some(&1)
                    && entry_count.get(u) == Some(&1)
                    && position_of
                        .get(u)
                        .is_some_and(|p| r.track_no == Some(i64::from(*p)))
            })
        })
        .map(|r| r.track_id)
        .collect();
    let track_no_of: HashMap<i64, i64> = rows
        .iter()
        .filter_map(|r| r.track_no.map(|n| (r.track_id, n)))
        .collect();
    if numbered.is_empty() {
        return Ok(());
    }
    let planned = env.editor.plan_rename(&numbered, &env.layout).await?;
    let mut targets = Vec::new();
    for p in planned {
        let Ok(current) = RelPath::parse(&p.current_rel_path) else {
            continue;
        };
        // 名前の先頭の番号が track_no と同じなら触らない
        if leading_number(current.file_name()) == track_no_of.get(&p.track_id).copied() {
            continue;
        }
        match p.planned {
            Planned::Path(new) => {
                // ディレクトリは今のまま、ファイル名だけテンプレートのもの
                let Some(dir) = current.parent() else {
                    continue;
                };
                let Ok(new_rel) = dir.join(new.file_name()) else {
                    continue;
                };
                if new_rel.key() == current.key() {
                    continue;
                }
                targets.push(RenameTarget {
                    track_id: p.track_id,
                    new_rel_path: new_rel.as_str().to_owned(),
                    expected: None,
                    planned_conflict: None,
                });
            }
            Planned::Unchanged => {}
            // 計画の時点の衝突（同名のファイルがある等）は続く状態なので、失敗にせず報告だけ
            Planned::Conflict(reason) => out.rename_conflicts.push(RenameConflict {
                track_id: p.track_id,
                reason,
            }),
        }
    }
    if targets.is_empty() {
        return Ok(());
    }
    out.renamed = targets.len();
    match env.editor.prepare_rename(Some(&description), targets).await {
        Ok(p) => {
            tracing::info!(
                subscription_id = sub.id,
                batch_id = p.batch_id,
                renamed = out.renamed,
                "ファイル名を揃える"
            );
            wait_batch(env, ctx, p.batch_id).await?;
            let b = batch_outcome(env, p.batch_id).await?;
            out.rename = Some(b.clone());
            require_all_applied("改名", &b)?;
        }
        Err(EditError::Pending { track_ids }) => {
            return Err(SyncError::Failed(anyhow::anyhow!(
                "反映待ちの編集が入った（{} 件）ので改名をやり直す",
                track_ids.len()
            )));
        }
        Err(EditError::NoChanges) => out.renamed = 0,
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

// ---------------------------------------------------------------- dispatcher

/// 常駐の dispatcher。`tick` ごとに (a) latch（`sync_requested_at`）の立った購読を投入し（enabled に
/// 関わらず。active なジョブがあれば Duplicate で何もせず、終端になった次の tick で投入される =
/// handler の最終確認から終端までの窓に立った latch も必ず回収される）、(b) `interval_hours > 0` なら
/// enabled で `last_attempted_at` から interval 経った購読を投入する。`interval_hours = 0` なら (a) だけ
pub fn spawn_dispatcher(
    jobs: Arc<Jobs>,
    interval_hours: u32,
    tick: Duration,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(tick) => {}
            }
            let interval_secs = i64::from(interval_hours) * 3600;
            let ids = jobs
                .db()
                .read(move |c| {
                    let mut ids = subscriptions::requested(c)?;
                    if interval_secs > 0 {
                        ids.extend(subscriptions::due(c, now_epoch(), interval_secs)?);
                    }
                    ids.sort_unstable();
                    ids.dedup();
                    Ok(ids)
                })
                .await;
            let ids = match ids {
                Ok(ids) => ids,
                Err(e) => {
                    tracing::warn!(error = %e, "購読の同期の due 判定に失敗");
                    continue;
                }
            };
            for id in ids {
                match jobs.enqueue(new_sync_job(id)).await {
                    Ok(EnqueueResult::Inserted(job_id)) => {
                        tracing::info!(subscription_id = id, job_id, "購読の同期を投入した")
                    }
                    Ok(EnqueueResult::Duplicate(_)) => {}
                    Err(e) => {
                        tracing::warn!(subscription_id = id, error = %e, "購読の同期を投入できない")
                    }
                }
            }
        }
    })
}

/// 子バッチが全件 applied でなければ（衝突・失敗・キャンセル）揃えは終わっていない: Failed（再試行で差分から
/// 取り直す。投入はしない）
fn require_all_applied(what: &str, b: &BatchOutcome) -> Result<(), SyncError> {
    if b.pending > 0 || b.conflict > 0 || b.failed > 0 || b.applied != b.total {
        return Err(SyncError::Failed(anyhow::anyhow!(
            "{what}のバッチ #{} が全件反映されていない（適用 {} / {}、衝突 {} / 失敗 {} / 反映待ち {}）ので揃えをやり直す",
            b.batch_id,
            b.applied,
            b.total,
            b.conflict,
            b.failed,
            b.pending
        )));
    }
    Ok(())
}

/// ファイル名の先頭の番号（`03 title.opus` / `03. title.opus` / `3-title.opus` の 3）。無ければ None
fn leading_number(file_name: &str) -> Option<i64> {
    let digits: String = file_name
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

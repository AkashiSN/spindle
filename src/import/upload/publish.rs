//! 公開計画の凍結と、Inbox への段階的な公開（SPEC §7.12「Inbox へ置く（公開）」）。
//!
//! - [`freeze`] はその時点の分け方（[`decide`] の included な件と曲・同梱ファイル）から計画を作って
//!   マニフェストに凍結する。以後の ③ の変更は計画に効かない。二度目は何もしない（冪等）
//! - [`publish`] は件ごとに段階を進める。段階ごとにマニフェストを書き、再実行は計画と実体だけで
//!   続きから進む。
//!   1. `Planned`: `publish/<n>/` を作ってメンバーを `RENAME_NOREPLACE` で移し（元に無く組み立て先に
//!      あれば移し済み）、サイドカーに `upload` を書き、fsync → `Assembled`
//!   2. `Assembled`: 宛先（[`super::view::base_dest`] と同じ名前。埋まっていれば ` (2)`…）を選んで
//!      `Publishing { dest }` を**書いてから** rename する
//!   3. `Publishing`: `publish/<n>/` が残っていれば宛先へ `RENAME_NOREPLACE`（宛先が他に取られて
//!      いれば `Assembled` に戻して選び直す）。`publish/<n>/` が無ければ rename は済んでいる（宛先が
//!      Inbox の配置で既に消えていてもよい）→ `Published`
//! - 鍵: 1 段階の実体の操作（移動・サイドカー・rename・fsync）は、その段階のマニフェストの書き込みと
//!   同じ [`with_manifest`] の中で行う。段階の途中で落ちても（クロージャが Err でも）マニフェストは
//!   前の段階のままで、実体の側から続きを判定できる。rename は速いので鍵を持つ時間は短い
//! - 消さない: unlink / rmdir はしない。移動は `rename_noreplace` だけ。外したファイル・取り込まない
//!   ファイルはステージングに残り、GC が後で消す
//! - `discarded_at` の記録と inbox ジョブの投入はここではしない（呼び出し側）

use tokio_util::sync::CancellationToken;

use crate::config::{LayoutConfig, UploadConfig};
use crate::domain::relpath::{canonical_key, RelPath};
use crate::fsroot::{FileKind, FsError, RootDir};
use crate::import::sidecar::{Sidecar, UploadEntry};

use super::group::group;
use super::manifest::{
    with_manifest, FileClass, Manifest, Origin, PlannedGroup, PlannedMember, PublishPlan, Stage,
    UploadError, UploadId, UploadLocks,
};
use super::receive::sync_with_disk;
use super::view::{
    decide, dest_of, library_matches, read_staged, read_staged_cached, StagedReadCache,
};

/// ステージング内の組み立て用のディレクトリ（`publish/<n>/`）
const PUBLISH_DIR: &str = "publish";
/// ` (N)` を試す上限（それでも空きが無ければ Conflict）
const MAX_SUFFIX: u32 = 10_000;

/// [`freeze`] の結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreezeOutcome {
    /// 計画を作って凍結した
    Frozen,
    /// 既に凍結してあった（何も変えない）
    AlreadyFrozen,
}

/// 公開の途中の地点（テストの故障注入。本番は `&|_| false`）。`fault` が true を返すと、その地点で
/// `UploadError::Io("fault")` を返して止まる（以後の変更をしない = 電源断の模擬）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishStep {
    /// メンバー 1 つを移す前
    BeforeMove,
    /// メンバー 1 つを移した後（マニフェストは `Planned` のまま）
    AfterMove,
    /// サイドカーを書いた後（fsync とマニフェストの前）
    AfterSidecar,
    /// `Assembled` を書いた後
    AfterAssembledWrite,
    /// `Publishing { dest }` を書いた後（rename の前）
    AfterDestWrite,
    /// 宛先へ rename した後（マニフェストは `Publishing` のまま）
    AfterRename,
    /// `Published` を書いた後
    AfterPublishedWrite,
}

/// [`publish`] の結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishReport {
    /// 公開済みの件の宛先（Inbox 相対。計画の順）
    pub placed: Vec<String>,
    /// 全件が公開済み
    pub finished: bool,
}

fn fault_error() -> UploadError {
    UploadError::Io(std::io::Error::other("fault"))
}

/// 送信・展開が終わっていない origin（removed と damaged は数えない。damaged は読まれず、
/// 送り直しは別の origin になる）
fn incomplete(o: &Origin) -> bool {
    if o.removed || o.damaged {
        return false;
    }
    if !o.complete {
        return true;
    }
    if o.class != FileClass::Zip {
        return false;
    }
    match &o.zip {
        None => true,
        Some(z) => z.generation == 0 || z.unpacking.is_some() || z.error.is_some(),
    }
}

/// 公開計画を作ってマニフェストに凍結し、`publish_requested_at` を入れる（鍵の中で 1 回だけ）。
///
/// - 既に計画があれば [`FreezeOutcome::AlreadyFrozen`]（冪等。計画は変えない）。ただし捨てた
///   アップロードで計画の全件を置き終えていなければ [`UploadError::Busy`]（止まった公開を捨てた後に
///   押し直しで動かさない）
/// - 計画が無く破棄済みなら [`UploadError::Busy`]
/// - removed でない origin に送信未完・未展開・失敗した zip・展開中があれば `Conflict("incomplete")`
/// - included な件が 0 なら `Conflict("empty")`（曲が 1 曲もない件は計画に入れない）
///
/// ステージングのタグは読み直す（読み取りの覚えを使うのは [`freeze_cached`]）。
/// `layout` と `cfg` は表示（`build_view`）と同じ入力を揃えるために受ける（今の計画には使わない）
#[allow(clippy::too_many_arguments)]
pub fn freeze(
    inbox: &RootDir,
    locks: &UploadLocks,
    id: &UploadId,
    conn: &rusqlite::Connection,
    layout: &LayoutConfig,
    cfg: &UploadConfig,
    now: i64,
) -> Result<FreezeOutcome, UploadError> {
    freeze_with(inbox, locks, id, conn, layout, cfg, now, None)
}

/// [`freeze`] と同じで、変わっていないファイルは `cache`（API の表示と共有する覚え）を使う
#[allow(clippy::too_many_arguments)]
pub fn freeze_cached(
    inbox: &RootDir,
    locks: &UploadLocks,
    id: &UploadId,
    conn: &rusqlite::Connection,
    layout: &LayoutConfig,
    cfg: &UploadConfig,
    now: i64,
    cache: &StagedReadCache,
) -> Result<FreezeOutcome, UploadError> {
    freeze_with(inbox, locks, id, conn, layout, cfg, now, Some(cache))
}

#[allow(clippy::too_many_arguments)]
fn freeze_with(
    inbox: &RootDir,
    locks: &UploadLocks,
    id: &UploadId,
    conn: &rusqlite::Connection,
    _layout: &LayoutConfig,
    _cfg: &UploadConfig,
    now: i64,
    cache: Option<&StagedReadCache>,
) -> Result<FreezeOutcome, UploadError> {
    with_manifest(inbox, locks, id, |m| {
        if let Some(plan) = &m.publish {
            let all_published = plan
                .groups
                .iter()
                .all(|g| matches!(g.stage, Stage::Published { .. }));
            if m.discarded_at.is_some() && !all_published {
                return Err(UploadError::Busy);
            }
            return Ok(FreezeOutcome::AlreadyFrozen);
        }
        if m.discarded_at.is_some() {
            return Err(UploadError::Busy);
        }
        // 最後のチャンクの後に落ちた origin などを実体に合わせてから判定する
        sync_with_disk(inbox, m)?;
        if m.origins.iter().any(incomplete) {
            return Err(UploadError::Conflict("incomplete".into()));
        }
        let groups = plan_groups(inbox, conn, m, cache)?;
        if groups.is_empty() {
            return Err(UploadError::Conflict("empty".into()));
        }
        m.publish = Some(PublishPlan {
            frozen_at: now,
            groups,
        });
        m.publish_requested_at = Some(now);
        Ok(FreezeOutcome::Frozen)
    })
}

/// included な件と、その included な曲・同梱ファイルだけで計画の件を作る（`n` は 1 から）
fn plan_groups(
    inbox: &RootDir,
    conn: &rusqlite::Connection,
    m: &Manifest,
    cache: Option<&StagedReadCache>,
) -> Result<Vec<PlannedGroup>, UploadError> {
    let (files, fps) = match cache {
        Some(cache) => read_staged_cached(inbox, m, cache)?,
        None => read_staged(inbox, m)?,
    };
    let grouping = group(&files, m);
    let matches = library_matches(conn, &grouping, &fps)?;
    let decided = decide(m, &grouping, &matches);
    let mut out = Vec::new();
    for (g, d) in grouping.groups.iter().zip(&decided.groups) {
        if !d.included {
            continue;
        }
        let tracks = g.tracks.iter().zip(&d.tracks);
        let companions = g.companions.iter().zip(&d.companions);
        let members: Vec<PlannedMember> = tracks
            .chain(companions)
            .filter(|(_, &inc)| inc)
            .map(|(mem, _)| PlannedMember {
                from: mem.staged.clone(),
                name: mem.publish_name.clone(),
            })
            .collect();
        let n = u32::try_from(out.len() + 1)
            .map_err(|_| UploadError::Limit("件の数が多すぎる".into()))?;
        out.push(PlannedGroup {
            n,
            key: g.key.clone(),
            albumartist: g.albumartist.clone(),
            album: g.album.clone(),
            origin_label: g.origin_label.clone(),
            origins: g.origins.clone(),
            members,
            stage: Stage::Planned,
        });
    }
    Ok(out)
}

/// 1 回の [`advance`] で起きたこと
enum Advance {
    /// 破棄済みで止まった
    Stopped,
    /// 段階を進めた（マニフェストに書いた）
    To(Stage),
    /// 既に公開済み
    Done,
}

/// 凍結した計画で件を Inbox の `upload/…` へ置く（同期関数。ジョブから spawn_blocking で呼ぶ）。
///
/// 各段階の前に `discarded_at` と取り消しを確かめ、あれば `finished: false` で止まる。計画が無ければ
/// `Conflict("not_frozen")`。`fault` はテストの故障注入（本番は `&|_| false`）
pub fn publish(
    inbox: &RootDir,
    locks: &UploadLocks,
    id: &UploadId,
    _now: i64,
    cancel: &CancellationToken,
    fault: &dyn Fn(PublishStep) -> bool,
) -> Result<PublishReport, UploadError> {
    let count = with_manifest(inbox, locks, id, |m| {
        Ok(m.publish.as_ref().map(|p| p.groups.len()))
    })?
    .ok_or_else(|| UploadError::Conflict("not_frozen".into()))?;

    'groups: for i in 0..count {
        loop {
            if cancel.is_cancelled() {
                break 'groups;
            }
            let step = with_manifest(inbox, locks, id, |m| advance(inbox, id, m, i, fault))?;
            let after = match step {
                Advance::Stopped => break 'groups,
                Advance::Done => break,
                Advance::To(Stage::Planned) => None,
                Advance::To(Stage::Assembled) => Some(PublishStep::AfterAssembledWrite),
                Advance::To(Stage::Publishing { .. }) => Some(PublishStep::AfterDestWrite),
                Advance::To(Stage::Published { .. }) => Some(PublishStep::AfterPublishedWrite),
            };
            if after.is_some_and(fault) {
                return Err(fault_error());
            }
        }
    }

    with_manifest(inbox, locks, id, |m| Ok(report(m)))
}

fn report(m: &Manifest) -> PublishReport {
    let groups = m
        .publish
        .as_ref()
        .map(|p| p.groups.as_slice())
        .unwrap_or(&[]);
    let placed: Vec<String> = groups
        .iter()
        .filter_map(|g| match &g.stage {
            Stage::Published { dest } => Some(dest.clone()),
            _ => None,
        })
        .collect();
    PublishReport {
        finished: !groups.is_empty() && placed.len() == groups.len(),
        placed,
    }
}

/// 件 `i` を 1 段階進める（[`with_manifest`] の中。Err ならマニフェストは書かれない）
fn advance(
    inbox: &RootDir,
    id: &UploadId,
    m: &mut Manifest,
    i: usize,
    fault: &dyn Fn(PublishStep) -> bool,
) -> Result<Advance, UploadError> {
    if m.discarded_at.is_some() {
        return Ok(Advance::Stopped);
    }
    let Some(plan) = m.publish.as_mut() else {
        return Err(UploadError::Conflict("not_frozen".into()));
    };
    let frozen_at = plan.frozen_at;
    // 他の件が使う・使った宛先は選ばない。置き終えた件も入れる: Inbox の取り込みが先にその
    // ディレクトリを片付けていると、ファイルシステムの上では空いて見えるため
    let taken: Vec<String> = plan
        .groups
        .iter()
        .enumerate()
        .filter(|(j, _)| *j != i)
        .filter_map(|(_, g)| match &g.stage {
            Stage::Publishing { dest } | Stage::Published { dest } => Some(canonical_key(dest)),
            _ => None,
        })
        .collect();
    let Some(g) = plan.groups.get_mut(i) else {
        return Err(UploadError::Conflict("計画に無い件".into()));
    };
    let next = match &g.stage {
        Stage::Published { .. } => return Ok(Advance::Done),
        Stage::Planned => {
            assemble(inbox, id, &m.id, g, frozen_at, fault)?;
            Stage::Assembled
        }
        Stage::Assembled => Stage::Publishing {
            dest: pick_dest(inbox, g, &taken)?,
        },
        Stage::Publishing { dest } => {
            if move_to_dest(inbox, id, g.n, dest, fault)? {
                Stage::Published { dest: dest.clone() }
            } else {
                // 宛先が他に取られた: 組み立て済みに戻して選び直す
                tracing::info!(
                    upload = id.as_str(),
                    dest = dest.as_str(),
                    "公開の宛先が埋まったので選び直す"
                );
                Stage::Assembled
            }
        }
    };
    g.stage = next.clone();
    Ok(Advance::To(next))
}

/// `publish/<n>`（ステージング内）
fn work_dir(id: &UploadId, n: u32) -> Result<RelPath, UploadError> {
    id.staged(&RelPath::parse(PUBLISH_DIR)?.join(&n.to_string())?)
}

/// あれば true（種類は問わない）
fn exists(inbox: &RootDir, rel: &RelPath) -> Result<bool, UploadError> {
    match inbox.stat(rel) {
        Ok(_) => Ok(true),
        Err(FsError::NotFound) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// 組み立て: メンバーを `publish/<n>/` へ移し、サイドカーに `upload` を書いて fsync する
fn assemble(
    inbox: &RootDir,
    id: &UploadId,
    upload_id: &str,
    g: &PlannedGroup,
    frozen_at: i64,
    fault: &dyn Fn(PublishStep) -> bool,
) -> Result<(), UploadError> {
    let work = work_dir(id, g.n)?;
    inbox.create_dir_all(&work)?;
    for mem in &g.members {
        let from = id.staged(&RelPath::parse(&mem.from)?)?;
        let to = work.join(&mem.name)?;
        match (exists(inbox, &from)?, exists(inbox, &to)?) {
            (true, false) => {
                if fault(PublishStep::BeforeMove) {
                    return Err(fault_error());
                }
                inbox.rename_noreplace(&from, &to)?;
                if fault(PublishStep::AfterMove) {
                    return Err(fault_error());
                }
            }
            // 移し済み
            (false, true) => {}
            (true, true) => {
                return Err(UploadError::Conflict(format!(
                    "組み立て先に同じ名前が既にある: {}",
                    mem.name
                )))
            }
            (false, false) => {
                return Err(UploadError::Conflict(format!(
                    "公開するファイルが無い: {}",
                    mem.from
                )))
            }
        }
    }

    let entry = UploadEntry {
        upload_id: upload_id.to_string(),
        group_key: g.key.clone(),
        uploaded_at: frozen_at,
        origins: g.origins.clone(),
    };
    let mut sidecar = Sidecar::read(inbox, &work)?.unwrap_or_default();
    if sidecar.upload.as_ref() != Some(&entry) {
        sidecar.upload = Some(entry);
        sidecar.write(inbox, &work)?;
    }
    if fault(PublishStep::AfterSidecar) {
        return Err(fault_error());
    }
    inbox.fsync_dir(Some(&work))?;
    if let Some(parent) = work.parent() {
        inbox.fsync_dir(Some(&parent))?;
    }
    Ok(())
}

/// 宛先を選ぶ: 基本の名前、埋まっていれば最後の要素に ` (2)`, ` (3)` … の最初の空き
fn pick_dest(inbox: &RootDir, g: &PlannedGroup, taken: &[String]) -> Result<String, UploadError> {
    let base = dest_of(&g.albumartist, g.album.as_deref(), &g.origin_label)?;
    let parent = base
        .parent()
        .ok_or_else(|| UploadError::BadPath(format!("宛先に親が無い: {base}")))?;
    for k in 1..=MAX_SUFFIX {
        let cand = if k == 1 {
            base.clone()
        } else {
            parent.join(&format!("{} ({k})", base.file_name()))?
        };
        if taken.contains(&canonical_key(cand.as_str())) {
            continue;
        }
        if !exists(inbox, &cand)? {
            return Ok(cand.as_str().to_string());
        }
    }
    Err(UploadError::Conflict(format!("空いた宛先が無い: {base}")))
}

/// `publish/<n>/` を宛先へ rename する。済んでいた（`publish/<n>/` が無い）か rename できたら true、
/// 宛先が埋まっていれば false
fn move_to_dest(
    inbox: &RootDir,
    id: &UploadId,
    n: u32,
    dest: &str,
    fault: &dyn Fn(PublishStep) -> bool,
) -> Result<bool, UploadError> {
    let work = work_dir(id, n)?;
    match inbox.stat(&work) {
        Ok(st) if st.kind == FileKind::Dir => {}
        Ok(_) => {
            return Err(UploadError::Conflict(format!(
                "組み立て先がディレクトリでない: {work}"
            )))
        }
        // rename は済んでいる（宛先が Inbox の配置で既に消えていてもよい）。rename の後・fsync の前に
        // 落ちたかもしれないので、残っている宛先の親の連なりを fsync してから公開済みにする
        Err(FsError::NotFound) => {
            fsync_dest_parents(inbox, dest)?;
            if let Some(p) = work.parent() {
                fsync_existing(inbox, Some(&p))?;
            }
            return Ok(true);
        }
        Err(e) => return Err(e.into()),
    }
    let dest = RelPath::parse(dest)?;
    let parent = dest
        .parent()
        .ok_or_else(|| UploadError::BadPath(format!("宛先に親が無い: {dest}")))?;
    inbox.create_dir_all(&parent)?;
    match inbox.rename_noreplace(&work, &dest) {
        Ok(()) => {}
        Err(FsError::Exists) => return Ok(false),
        Err(e) => return Err(e.into()),
    }
    if fault(PublishStep::AfterRename) {
        return Err(fault_error());
    }
    // 宛先の親から root までと、組み立て元の親を fsync（作ったディレクトリと rename を永続化）
    let mut dir = Some(parent);
    while let Some(d) = dir {
        inbox.fsync_dir(Some(&d))?;
        dir = d.parent();
    }
    inbox.fsync_dir(None)?;
    if let Some(p) = work.parent() {
        inbox.fsync_dir(Some(&p))?;
    }
    Ok(true)
}

/// 宛先の親から root までのうち、残っているディレクトリを fsync する（Inbox の配置で宛先や親が既に
/// 消えていてもよい）
fn fsync_dest_parents(inbox: &RootDir, dest: &str) -> Result<(), UploadError> {
    let dest = RelPath::parse(dest)?;
    let mut dir = dest.parent();
    while let Some(d) = dir {
        fsync_existing(inbox, Some(&d))?;
        dir = d.parent();
    }
    inbox.fsync_dir(None)?;
    Ok(())
}

/// ディレクトリを fsync する。無ければ何もしない
fn fsync_existing(inbox: &RootDir, dir: Option<&RelPath>) -> Result<(), UploadError> {
    match inbox.fsync_dir(dir) {
        Ok(()) | Err(FsError::NotFound) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

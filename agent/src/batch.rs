//! パス変更（移動・更新 + 移動）のバッチ（仕様 ⑥「パス変更は ADB と同じバッチ」、③「例外: 始まった
//! パス変更のバッチ」）。本体の `device::sync` の `path_batch` / `abort_batch` / `batch_members` と
//! `device::recover` の `complete_batch` / `vacate` / `place` の移植。ジャーナルの代わりに state.json の
//! `pending_batches`（1 回の書き込みに全メンバーと相）を、端末の `mv` の代わりにローカルの rename と
//! `Music::set_location` を使う。
//!
//! 相: sealed（記録）→ prepared（更新 + 移動の新しい内容を `new` へ）→ vacating → vacated → placing →
//! done（バッチを外す）。vacating 以降はキャンセルも差分の変化も見ずに完遂する（巻き戻さない）。
//! 旧ファイルは `staging` へ動かし、track の `location` も付け替えるので track（再生回数など）は残る

use std::collections::HashSet;

use agent_proto::{OpKind, PlanItem};

use crate::ctx::Ctx;
use crate::exec::{
    download, entry_for, Downloaded, CONTENT_MISMATCH, PATH_OCCUPIED, UNMANAGED_COLLISION,
};
use crate::local::MOVING_DIR;
use crate::music::Music;
use crate::pathkey::{canonical_key, random_id};
use crate::plan::drop_components;
use crate::server::Server;
use crate::state::{member_digest, BatchMember, BatchPhase, MemberOp, PendingBatch};
use crate::{Error, Result};

/// state.json のバッチの digest が合わない（vacating 以降は前進も破棄もできない）
pub const BROKEN_BATCH: &str = "state.json のバッチが壊れています";
/// パス変更の行き先に、空けた後で管理外のファイルが現れた
pub const FOREIGN_AT_DESTINATION: &str = "移動の行き先に管理外のファイルがあります";

/// パス変更をまとめて 1 つのバッチで実行する。返すのは（実行した数, 実行しなかった数）。
/// 項目のエラー（管理外と衝突・パス衝突・内容の不一致）にしたものはどちらにも数えない。
/// 行き先を管理外のファイル・track か、動かない管理下の曲が占めるメンバーは、封印の前にその成分ごと外す
/// （上書きしない）。
/// 準備で失敗したメンバーがいれば、その曲を含む成分を全員外し、旧バッチを破棄してから残りで記録し直す
pub fn run<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    ops: Vec<PlanItem>,
    unmanaged: &HashSet<String>,
) -> Result<(usize, usize)> {
    let total = ops.len();
    let mut errored = 0;
    let mut skip: HashSet<i64> = HashSet::new();
    // 今回動く曲。その移動元はバッチの中で空くので、行き先にあっても占有ではない
    // （行き先が別の操作の移動元なら同じ成分に入るので、外すときは一緒に外れる）
    let moving: HashSet<i64> = ops.iter().map(|o| o.track_id).collect();
    for o in &ops {
        let to_key = o.to.as_deref().map(canonical_key);
        let occupied = to_key.as_ref().is_some_and(|k| {
            cx.state
                .tracks
                .iter()
                .any(|(id, e)| !moving.contains(id) && canonical_key(&e.path) == *k)
        });
        if to_key.as_ref().is_some_and(|k| unmanaged.contains(k)) {
            cx.error_track(o.track_id, UNMANAGED_COLLISION);
            errored += 1;
            skip.insert(o.track_id);
        } else if occupied {
            // 動かない管理下の曲が行き先にいる。上書きしない
            cx.error_track(o.track_id, PATH_OCCUPIED);
            errored += 1;
            skip.insert(o.track_id);
        } else if !runnable_member(cx, o) {
            // state と食い違う（行が無い・移動元が違う）か、計画の形が欠けている。数えるだけ
            skip.insert(o.track_id);
        }
    }
    let mut ops = drop_components(ops, &skip);
    let mut superseded: Option<PendingBatch> = None;
    loop {
        if ops.is_empty() {
            if let Some(old) = superseded.take() {
                abort(cx, &old)?;
            }
            return Ok((0, total - errored));
        }
        let batch_id = random_id()?;
        if let Some(old) = superseded.take() {
            abort(cx, &old)?;
        }
        let members = batch_members(cx, &batch_id, &ops);
        let batch = PendingBatch {
            batch_id: batch_id.clone(),
            phase: BatchPhase::Sealed,
            digest: member_digest(&members)?,
            members,
        };
        cx.state.pending_batches.push(batch.clone());
        cx.save()?;

        let mut failed: HashSet<i64> = HashSet::new();
        for m in &batch.members {
            let Some(new) = &m.new else { continue };
            match download(cx, m.track_id, &m.token, m.size, &m.sha256, new)? {
                Downloaded::Ok => {}
                Downloaded::Changed | Downloaded::Gone => {
                    failed.insert(m.track_id);
                }
                Downloaded::Mismatch => {
                    cx.error_track(m.track_id, CONTENT_MISMATCH);
                    errored += 1;
                    failed.insert(m.track_id);
                }
            }
        }
        cx.fp.hit("batch.prepared")?;
        if failed.is_empty() {
            // 管理外の鍵は実行の始めに取ったもの。空ける前（ここまでは破棄できる）に行き先を見直し、
            // ファイルが現れていればその曲の成分を外して縮める（上書きしない）。他のメンバーの移動元は
            // バッチの中で空くので占有ではない
            let from_keys: HashSet<String> = batch
                .members
                .iter()
                .map(|m| canonical_key(&m.from))
                .collect();
            for m in &batch.members {
                if !from_keys.contains(&canonical_key(&m.to)) && cx.local.exists(&m.to)? {
                    cx.error_track(m.track_id, UNMANAGED_COLLISION);
                    errored += 1;
                    failed.insert(m.track_id);
                }
            }
        }
        if failed.is_empty() {
            set_phase(cx, &batch_id, BatchPhase::Prepared)?;
            set_phase(cx, &batch_id, BatchPhase::Vacating)?;
            // ここから先はキャンセルも差分の変化も見ずに完遂する
            complete(cx, &batch_id, false)?;
            let n = batch.members.len();
            return Ok((n, total - n - errored));
        }
        ops = drop_components(ops, &failed);
        superseded = Some(batch);
    }
}

/// state の行があり、その `path` が計画の移動元と同じ鍵で、計画の形が揃っているか
fn runnable_member<M: Music, S: Server>(cx: &Ctx<'_, M, S>, o: &PlanItem) -> bool {
    let shaped = matches!(o.op, OpKind::Move | OpKind::UpdateMove)
        && o.to.is_some()
        && o.token.is_some()
        && o.sha256.is_some();
    let from_key = o.from.as_deref().map(canonical_key);
    shaped
        && cx
            .state
            .tracks
            .get(&o.track_id)
            .is_some_and(|e| Some(canonical_key(&e.path)) == from_key)
}

fn batch_members<M: Music, S: Server>(
    cx: &Ctx<'_, M, S>,
    batch_id: &str,
    ops: &[PlanItem],
) -> Vec<BatchMember> {
    ops.iter()
        .filter_map(|o| {
            let op = match o.op {
                OpKind::Move => MemberOp::Move,
                OpKind::UpdateMove => MemberOp::UpdateMove,
                _ => return None,
            };
            let e = cx.state.tracks.get(&o.track_id)?;
            // 名前に batch_id を含める: 縮めた後の新バッチが旧バッチ（破棄済み）と一時ファイルを共有すると、
            // 旧バッチの片付けが新バッチの new を消してしまう（D-95 の P5-3a 追記）
            let staging = format!("{MOVING_DIR}/{batch_id}-{}", o.op_id);
            Some(BatchMember {
                op_id: o.op_id.clone(),
                op,
                track_id: o.track_id,
                persistent_id: e.persistent_id.clone(),
                // 実際にあるファイルの名前（鍵は計画の from と同じ）
                from: e.path.clone(),
                new: (op == MemberOp::UpdateMove).then(|| format!("{staging}.new")),
                staging,
                to: o.to.clone()?,
                token: o.token.clone()?,
                size: o.size,
                sha256: o.sha256.clone()?,
            })
        })
        .collect()
}

fn set_phase<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    batch_id: &str,
    phase: BatchPhase,
) -> Result<()> {
    if let Some(b) = cx
        .state
        .pending_batches
        .iter_mut()
        .find(|b| b.batch_id == batch_id)
    {
        b.phase = phase;
    }
    cx.save()
}

/// 破棄: 先にバッチを `pending_batches` から外して保存し、その後で `new` を消す
/// （state にあるバッチの new を消さない）。消すのは置き場（`.moving/`）の下だけ
fn abort<M: Music, S: Server>(cx: &mut Ctx<'_, M, S>, b: &PendingBatch) -> Result<()> {
    cx.state
        .pending_batches
        .retain(|x| x.batch_id != b.batch_id);
    cx.save()?;
    for m in &b.members {
        if let Some(new) = m.new.as_deref().filter(|n| in_moving_dir(n)) {
            cx.local.remove(new)?;
        }
    }
    Ok(())
}

/// 置き場（`.moving/`）の下か。`..` を含むものは外へ出られるので認めない
fn in_moving_dir(rel: &str) -> bool {
    rel.starts_with(&format!("{MOVING_DIR}/")) && !rel.split('/').any(|c| c == "..")
}

/// `vacating` 以降のバッチを done まで前進させる（巻き戻さない）。`verify` は回復のとき真:
/// メンバーごとにファイルの在る場所から進み具合を判定する。実行中（偽）は記録どおりに動かすだけ。
/// 全メンバーが空くまで行き先を埋めない
fn complete<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    batch_id: &str,
    verify: bool,
) -> Result<()> {
    let Some(b) = cx
        .state
        .pending_batches
        .iter()
        .find(|b| b.batch_id == batch_id)
        .cloned()
    else {
        return Err(Error::State(format!("バッチが無い（{batch_id}）")));
    };
    if b.phase < BatchPhase::Vacated {
        for m in &b.members {
            vacate(cx, m, verify)?;
            cx.fp.hit("batch.vacated_one")?;
        }
        set_phase(cx, batch_id, BatchPhase::Vacated)?;
    }
    if b.phase < BatchPhase::Placing {
        set_phase(cx, batch_id, BatchPhase::Placing)?;
    }
    let mut placed = Vec::with_capacity(b.members.len());
    for m in &b.members {
        placed.push(place(cx, m, verify)?);
        cx.fp.hit("batch.placed_one")?;
    }
    // done の前に置き場（移動は空、更新 + 移動は旧版）と残った new を片付ける
    for m in &b.members {
        cx.local.remove(&m.staging)?;
        if let Some(new) = &m.new {
            cx.local.remove(new)?;
        }
    }
    for m in &b.members {
        for p in std::iter::once(&m.staging).chain(m.new.as_ref()) {
            if cx.local.exists(p)? {
                // バッチは未完了のまま残し、次回の回復で消し直す
                return Err(Error::Io(std::io::Error::other(format!(
                    "置き場のファイルを消せなかった（{p}）"
                ))));
            }
        }
    }
    cx.fp.hit("batch.cleaned")?;
    // done: バッチを外すのと tracks の更新を 1 回の書き込みで
    for (m, ok) in b.members.iter().zip(placed) {
        if ok {
            let entry = entry_for(cx, &m.persistent_id, &m.token, &m.to, m.size, &m.sha256)?;
            cx.state.tracks.insert(m.track_id, entry);
        } else {
            cx.state.tracks.remove(&m.track_id);
        }
    }
    cx.state.pending_batches.retain(|x| x.batch_id != batch_id);
    cx.save()
}

/// 旧パスを空ける: 旧ファイルを `staging` へ動かし、track の `location` を付け替える（track は残す）。
/// 回復では在る場所で判定する: `staging` にあれば空け済み（location の付け替えだけやり直す）、
/// `from` にあれば未着手、どちらにも無ければ手で消された（`place` で見つからず state から外れる）
fn vacate<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    m: &BatchMember,
    verify: bool,
) -> Result<()> {
    if verify {
        if cx.local.exists(&m.staging)? {
            return relocate(cx, m, &m.staging, false);
        }
        if !cx.local.exists(&m.from)? {
            return Ok(());
        }
    }
    cx.local.rename(&m.from, &m.staging)?;
    relocate(cx, m, &m.staging, false)
}

/// track の location を `rel` へ付け替える（`refresh` が真なら読み直させる）。track が手で消されていれば
/// 何もしない: ファイルの移動だけ進めて置き場所を揃え、done で書いた行は次の再発見が「手で消された」として
/// 外し、ファイルを片付ける
fn relocate<M: Music, S: Server>(
    cx: &Ctx<'_, M, S>,
    m: &BatchMember,
    rel: &str,
    refresh: bool,
) -> Result<()> {
    if cx.music.track(&m.persistent_id)?.is_none() {
        return Ok(());
    }
    cx.music
        .set_location(&m.persistent_id, &cx.local.abs(rel)?)?;
    if refresh {
        cx.music.refresh(&m.persistent_id)?;
    }
    Ok(())
}

/// 行き先を埋める: 移動は `staging → to`、更新 + 移動は `new → to`（と refresh）。置けたら true。
/// 回復では src が在れば置き、無くて `to` が期待どおりの中身（大きさと sha256）で在れば置き済みとみなして
/// location を付け替える（違う中身なら管理外として止める）。どちらにも無ければ false（track は消さない。
/// state から外れる）
fn place<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    m: &BatchMember,
    verify: bool,
) -> Result<bool> {
    let src = match m.op {
        MemberOp::Move => m.staging.as_str(),
        MemberOp::UpdateMove => m.new.as_deref().ok_or_else(|| {
            Error::State(format!(
                "pending_batches の更新 + 移動に new が無い（op_id {}）",
                m.op_id
            ))
        })?,
    };
    // 行き先に管理外のファイルがあれば上書きも採用もしない。vacating 以降は巻き戻せないので、バッチを
    // 残して止め、利用者が退けた後の回復で続きを置く（置き済みのメンバーは回復が在る場所で判定する）
    let foreign = || {
        Error::Stop(format!(
            "{FOREIGN_AT_DESTINATION}（{}）。そのファイルを別の場所へ移してから sync し直してください",
            m.to
        ))
    };
    if verify && !cx.local.exists(src)? {
        if !cx.local.exists(&m.to)? {
            return Ok(false);
        }
        // 置いた後で落ちた。落ちている間に行き先が差し替えられていれば自分の置いたものではない。
        // 中身で確かめてから採用する（done で記録する sha256 は計画の値なので、確かめずに採ると
        // 管理外の内容を正しい写しとして確定してしまう）
        let size_ok = cx.local.stat(&m.to)?.is_some_and(|st| st.size == m.size);
        if !size_ok || cx.local.sha256(&m.to)?.as_deref() != Some(m.sha256.as_str()) {
            return Err(foreign());
        }
    } else {
        // 全員を空けた後なので、行き先にあってよい自分のファイルは無い。空ける前の見直しの後に
        // 管理外のファイルが現れていれば上書きしない
        if cx.local.exists(&m.to)? {
            return Err(foreign());
        }
        cx.fp.hit("batch.place.checked")?;
        // 確かめてから動かすまでの間に現れたものも上書きしない（rename が断る）
        match cx.local.rename_new(src, &m.to) {
            Ok(()) => {}
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(foreign())
            }
            Err(e) => return Err(e),
        }
    }
    relocate(cx, m, &m.to, m.op == MemberOp::UpdateMove)?;
    Ok(true)
}

/// 前回の実行が途中で止まったバッチを先頭から片付ける（`recover::recover` が `pending_ops` より先に呼ぶ。
/// バッチの途中は track の location が `.moving/` を指すので、仕様 ⑥「回復の順序」）。
/// sealed / prepared は破棄（縮めない）、vacating 以降は完遂する
pub fn recover_batches<M: Music, S: Server>(cx: &mut Ctx<'_, M, S>) -> Result<()> {
    let batches = cx.state.pending_batches.clone();
    for b in &batches {
        let intact = member_digest(&b.members)? == b.digest;
        match b.phase {
            BatchPhase::Sealed | BatchPhase::Prepared => abort(cx, b)?,
            _ if !intact => return Err(Error::Stop(BROKEN_BATCH.to_owned())),
            _ => {
                // done の保存に含める
                cx.state.needs_report = true;
                complete(cx, &b.batch_id, true)?;
            }
        }
    }
    Ok(())
}

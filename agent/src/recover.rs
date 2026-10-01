//! 回復（仕様 ⑥「回復」、D-100）。前回の実行が途中で止まった `pending_ops` を先頭から 1 件ずつ片付ける。
//! 片付けと pending からの除去は 1 回の保存で行う。ここで扱うのは曲の追加・更新・削除
//! （パス変更のバッチは Task 9、プレイリストは Task 10 が足す）

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::ctx::Ctx;
use crate::exec::{entry_for, remove_pending};
use crate::local::TMP_SUFFIX;
use crate::music::{Music, MusicTrack};
use crate::pathkey::{canonical_key, to_rel};
use crate::server::Server;
use crate::state::{Candidate, OpPhase, PendingKind, PendingOp};
use crate::{Error, Result};

pub fn recover<M: Music, S: Server>(cx: &mut Ctx<'_, M, S>) -> Result<()> {
    let ops = cx.state.pending_ops.clone();
    for op in &ops {
        match (op.op, op.phase) {
            (PendingKind::Delete, OpPhase::Deleting) => recover_delete(cx, op)?,
            (PendingKind::Update, OpPhase::Fetching) => recover_update(cx, op)?,
            (PendingKind::Add, OpPhase::Fetching) => recover_add_fetching(cx, op)?,
            (PendingKind::Add, OpPhase::Adding) => recover_add_adding(cx, op)?,
            // プレイリスト（Task 10）はまだ扱わない
            _ => {}
        }
    }
    Ok(())
}

fn need<'a>(v: &'a Option<String>, what: &str, op: &PendingOp) -> Result<&'a str> {
    v.as_deref().ok_or_else(|| {
        Error::State(format!(
            "pending_ops の {} に {what} が無い（op_id {}）",
            kind_name(op.op),
            op.op_id
        ))
    })
}

fn kind_name(k: PendingKind) -> &'static str {
    match k {
        PendingKind::Add => "add",
        PendingKind::Update => "update",
        PendingKind::Delete => "delete",
        PendingKind::Playlist => "playlist",
        PendingKind::PlaylistDelete => "playlist_delete",
    }
}

/// track を消した → ファイルを消した → state から外した、のどこで止まっても前へ進める
fn recover_delete<M: Music, S: Server>(cx: &mut Ctx<'_, M, S>, op: &PendingOp) -> Result<()> {
    let pid = need(&op.persistent_id, "persistent_id", op)?;
    let from = need(&op.from, "from", op)?;
    if cx.music.track(pid)?.is_some() {
        cx.music.delete_track(pid)?;
    }
    cx.local.remove(from)?;
    let key = canonical_key(from);
    if cx
        .state
        .tracks
        .get(&op.ref_id)
        .is_some_and(|e| canonical_key(&e.path) == key)
    {
        cx.state.tracks.remove(&op.ref_id);
    }
    remove_pending(cx, &op.op_id);
    cx.state.needs_report = true;
    cx.save()
}

/// 置き終えていれば（`to` が新しい中身）refresh して state を新しい値にする。置く前なら捨てる
fn recover_update<M: Music, S: Server>(cx: &mut Ctx<'_, M, S>, op: &PendingOp) -> Result<()> {
    let to = need(&op.to, "to", op)?;
    cx.local.remove(&format!("{to}{TMP_SUFFIX}"))?;
    let sha = need(&op.sha256, "sha256", op)?;
    if cx.local.sha256(to)?.as_deref() == Some(sha) {
        let pid = need(&op.persistent_id, "persistent_id", op)?;
        let token = need(&op.token, "token", op)?;
        cx.music.refresh(pid)?;
        let entry = entry_for(cx, pid, token, to, op.size, sha)?;
        cx.state.tracks.insert(op.ref_id, entry);
        cx.state.needs_report = true;
    }
    remove_pending(cx, &op.op_id);
    cx.save()
}

/// `to` を指す track（root の下で鍵が一致するもの）
fn tracks_at<M: Music, S: Server>(cx: &Ctx<'_, M, S>, to: &str) -> Result<Vec<MusicTrack>> {
    let key = canonical_key(to);
    Ok(cx
        .music
        .tracks_under(cx.local.path())?
        .into_iter()
        .filter(|t| {
            t.location
                .as_deref()
                .and_then(|l| to_rel(cx.local.path(), l))
                .is_some_and(|r| canonical_key(&r) == key)
        })
        .collect())
}

/// 取得中・置いた直後（`add` の前）に止まった。置いたファイルは pending にあるので自分のもの
fn recover_add_fetching<M: Music, S: Server>(cx: &mut Ctx<'_, M, S>, op: &PendingOp) -> Result<()> {
    let to = need(&op.to, "to", op)?;
    cx.local.remove(&format!("{to}{TMP_SUFFIX}"))?;
    if cx.local.exists(to)? && tracks_at(cx, to)?.is_empty() {
        cx.local.remove(to)?;
    }
    remove_pending(cx, &op.op_id);
    cx.save()
}

/// `add` の前後で止まった
fn recover_add_adding<M: Music, S: Server>(cx: &mut Ctx<'_, M, S>, op: &PendingOp) -> Result<()> {
    let to = need(&op.to, "to", op)?;
    let sha = need(&op.sha256, "sha256", op)?;
    let token = need(&op.token, "token", op)?;
    let at = tracks_at(cx, to)?;
    match at.as_slice() {
        [t] => {
            if cx.local.sha256(to)?.as_deref() == Some(sha) {
                let entry = entry_for(cx, &t.persistent_id, token, to, op.size, sha)?;
                cx.state.tracks.insert(op.ref_id, entry);
                cx.state.needs_report = true;
            }
            // 中身が違えば取り込まない（track もファイルも触らない。管理外として残る）
            remove_pending(cx, &op.op_id);
            cx.save()
        }
        [] => {
            let cands = copy_candidates(cx, op)?;
            if cands.is_empty() {
                // add は起きていない
                cx.local.remove(to)?;
                remove_pending(cx, &op.op_id);
                return cx.save();
            }
            record_candidates(cx, &op.op_id, &cands)?;
            Err(Error::Stop(candidates_message(op, to, &cands)))
        }
        _ => Err(Error::Stop(format!(
            "{}: {to}。片方を手で消してから sync し直してください",
            crate::rediscover::DUPLICATE_LOCATION
        ))),
    }
}

/// 見つけた候補を pending の add に記録して保存する
fn record_candidates<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    op_id: &str,
    cands: &[(MusicTrack, String)],
) -> Result<()> {
    if let Some(o) = cx.state.pending_ops.iter_mut().find(|o| o.op_id == op_id) {
        o.candidates = Some(
            cands
                .iter()
                .map(|(t, sha)| Candidate {
                    persistent_id: t.persistent_id.clone(),
                    sha256: sha.clone(),
                })
                .collect(),
        );
    }
    cx.save()
}

/// `resolve` でユーザが選ぶ結論
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// この persistent ID の曲が spindle の複製なので消す
    DeleteTrack(String),
    /// 複製は作られなかった
    NoCopyCreated,
}

/// コピー設定 ON の `add` の候補についての、ユーザの明示的な選択を適用する。
/// 候補は取り直して、表示した時と違えば何も消さずに止める
pub fn resolve<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    op_id: &str,
    choice: Resolution,
) -> Result<()> {
    let op = cx
        .state
        .pending_ops
        .iter()
        .find(|o| o.op_id == op_id && o.op == PendingKind::Add && o.phase == OpPhase::Adding)
        .cloned();
    let (Some(op), true) = (
        op.as_ref(),
        op.as_ref().is_some_and(|o| o.candidates.is_some()),
    ) else {
        return Err(Error::Stop("その op_id の保留はありません".into()));
    };
    let to = need(&op.to, "to", op)?;
    let recorded: BTreeSet<(String, String)> = op
        .candidates
        .iter()
        .flatten()
        .map(|c| (c.persistent_id.clone(), c.sha256.clone()))
        .collect();
    let cands = copy_candidates(cx, op)?;
    let now: BTreeSet<(String, String)> = cands
        .iter()
        .map(|(t, sha)| (t.persistent_id.clone(), sha.clone()))
        .collect();
    if now != recorded {
        record_candidates(cx, op_id, &cands)?;
        return Err(Error::Stop(format!(
            "候補が変わりました。もう一度確かめてください。\n{}",
            candidates_message(op, to, &cands)
        )));
    }
    if let Resolution::DeleteTrack(pid) = &choice {
        if !cands.iter().any(|(t, _)| &t.persistent_id == pid) {
            return Err(Error::Stop(format!(
                "{pid} は候補にありません。候補以外の曲は消せません"
            )));
        }
        cx.music.delete_track(pid)?;
    }
    cx.local.remove(to)?;
    remove_pending(cx, op_id);
    cx.save()
}

fn candidates_message(op: &PendingOp, to: &str, cands: &[(MusicTrack, String)]) -> String {
    let mut s = format!(
        "add の途中で止まった曲（op_id {}、{to}）の複製の候補がミュージック.app にあります。\n",
        op.op_id
    );
    for (t, _) in cands {
        let loc = t
            .location
            .as_deref()
            .map(|l| l.display().to_string())
            .unwrap_or_default();
        s.push_str(&format!(
            "  - {}  {loc}  追加 {}  {} バイト\n",
            t.persistent_id,
            t.date_added,
            t.size.unwrap_or(0)
        ));
    }
    s.push_str("次のどちらかを実行してください:\n");
    // 候補が 1 つならその persistent ID を、複数なら選んでもらう
    let pid = match cands {
        [(t, _)] => t.persistent_id.as_str(),
        _ => "<persistent_id>",
    };
    s.push_str(&format!(
        "  spindle-agent resolve {} --delete-track {pid}   （その曲を spindle の複製として消す）\n",
        op.op_id
    ));
    s.push_str(&format!(
        "  spindle-agent resolve {} --no-copy-created                （複製は作られなかった。add の意図を捨てる）",
        op.op_id
    ));
    s
}

/// コピー設定 ON で `add` が作ったかもしれない複製の候補: `max_database_id` より後に足された track の
/// うち、サイズが同じで `location` のファイルの sha256 が一致するもの（track と sha256）
pub fn copy_candidates<M: Music, S: Server>(
    cx: &Ctx<'_, M, S>,
    op: &PendingOp,
) -> Result<Vec<(MusicTrack, String)>> {
    let Some(after) = op.max_database_id else {
        return Err(Error::State(format!(
            "pending_ops の add に max_database_id が無い（op_id {}）",
            op.op_id
        )));
    };
    let sha = need(&op.sha256, "sha256", op)?;
    let mut out = Vec::new();
    for t in cx.music.tracks_added_after(after)? {
        if t.size != Some(op.size) {
            continue;
        }
        let Some(loc) = t.location.as_deref() else {
            continue;
        };
        if let Some(got) = file_sha256(loc)? {
            if got == sha {
                out.push((t, got));
            }
        }
    }
    Ok(out)
}

/// root の外のファイル（ミュージック.app のメディアフォルダ）の sha256。無ければ None
fn file_sha256(p: &Path) -> Result<Option<String>> {
    let mut f = match File::open(p) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(Some(
        h.finalize().iter().map(|b| format!("{b:02x}")).collect(),
    ))
}

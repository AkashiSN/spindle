//! 確定した計画の実行（仕様 ⑥「反映」、D-100）。順序は 削除 → パス変更 → 更新 → 追加 → プレイリスト。
//! 各操作は副作用の前に `pending_ops` へ記録し、終われば state の行の更新と pending からの除去を
//! 1 回の保存で行う。途中で落ちたものは `recover::recover` が片付ける

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Write};

use agent_proto::{ManifestResponse, OpKind, PlanItem};

use crate::ctx::Ctx;
use crate::local::{is_reserved, TMP_SUFFIX};
use crate::music::Music;
use crate::pathkey::{canonical_key, to_rel};
use crate::plan::Runnable;
use crate::rediscover::pending_pids;
use crate::server::{Fetch, Server};
use crate::state::{OpPhase, PendingKind, PendingOp, TrackEntry};
use crate::{Error, Result};

pub const UNMANAGED_COLLISION: &str = "管理外と衝突";
pub const PATH_OCCUPIED: &str = "パス衝突（移動しなかった曲が置き先にいる）";
pub const CONTENT_MISMATCH: &str = "受け取った内容がトークンと一致しない";
/// 空き容量の余裕
pub const MARGIN_BYTES: u64 = 64 * 1024 * 1024;
/// 受信が切れたときに同じ `If-Match` の続きでやり直す回数
const FETCH_RETRIES: u32 = 3;
/// 「［ミュージック］フォルダにコピー」が ON のときの停止の文言
pub const COPY_ON_STOP: &str = "ミュージック.app の「ファイルを［ミュージック］フォルダにコピー」が ON です。設定 > ファイル で切ってから sync し直してください";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outcome {
    /// 実行し終えた操作の数
    pub executed: usize,
    /// 実行しなかった操作の数（送る元が変わった・消えた、state と食い違う、未対応の種類）
    pub dropped: usize,
    /// 残りを止めた理由。呼び出し側は報告してから `Error::Stop` で終える
    pub stop: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Downloaded {
    Ok,
    /// 412（版が変わった）
    Changed,
    /// 404（もう desired に無い）
    Gone,
    /// 最後まで受けたがサイズか sha256 が違う
    Mismatch,
}

/// 1 操作の結果
enum Step {
    Done,
    /// 既に満たされていた（数えない）
    Satisfied,
    Dropped,
    /// 項目のエラー（`cx.errors` に記録済み）
    Errored,
    Stop(String),
}

pub fn run<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    r: &Runnable,
    // プレイリストの実行（Task 10）が中身を引く
    _m: &ManifestResponse,
) -> Result<Outcome> {
    // 空き容量（副作用の前）
    let need = r
        .items
        .iter()
        .filter(|i| matches!(i.op, OpKind::Add | OpKind::Update | OpKind::UpdateMove))
        .fold(MARGIN_BYTES, |a, i| a.saturating_add(i.size));
    let free = cx.local.free_bytes()?;
    if free < need {
        return Err(Error::Stop(format!(
            "空き容量が足りません（必要 {need} バイト、空き {free} バイト）"
        )));
    }
    let unmanaged = unmanaged_keys(cx)?;

    let mut out = Outcome {
        dropped: r.dropped,
        ..Outcome::default()
    };
    let order = [
        OpKind::Delete,
        OpKind::Move,
        OpKind::UpdateMove,
        OpKind::Update,
        OpKind::Add,
    ];
    for kind in order {
        for item in r.items.iter().filter(|i| i.op == kind) {
            let step = match kind {
                OpKind::Delete => delete(cx, item)?,
                // パス変更はバッチ（Task 9）で実行する。この時点では数えるだけ
                OpKind::Move | OpKind::UpdateMove => Step::Dropped,
                OpKind::Update => update(cx, item)?,
                OpKind::Add => add(cx, item, &unmanaged)?,
            };
            match step {
                Step::Done => out.executed += 1,
                Step::Satisfied => {}
                Step::Dropped => out.dropped += 1,
                Step::Errored => {}
                Step::Stop(msg) => {
                    out.stop = Some(msg);
                    return Ok(out);
                }
            }
        }
    }
    // プレイリスト（Task 10）。この時点では数えるだけ
    out.dropped += r.playlists.len();
    Ok(out)
}

/// 管理外のパスの鍵: root のファイルのうち予約でなく、state の曲のパスでも pending の `to` / `from` でも
/// ないもの、と root の下の track のうち persistent ID が state にも pending にも無いものの location
pub fn unmanaged_keys<M: Music, S: Server>(cx: &Ctx<'_, M, S>) -> Result<HashSet<String>> {
    let mut managed_paths: HashSet<String> = cx
        .state
        .tracks
        .values()
        .map(|e| canonical_key(&e.path))
        .collect();
    for o in &cx.state.pending_ops {
        for p in [&o.to, &o.from].into_iter().flatten() {
            managed_paths.insert(canonical_key(p));
        }
    }
    let mut out = HashSet::new();
    for f in cx.local.list_files()? {
        if is_reserved(&f) {
            continue;
        }
        let key = canonical_key(&f);
        if !managed_paths.contains(&key) {
            out.insert(key);
        }
    }
    let mut managed_pids = pending_pids(&cx.state);
    managed_pids.extend(cx.state.tracks.values().map(|e| e.persistent_id.clone()));
    for t in cx.music.tracks_under(cx.local.path())? {
        if managed_pids.contains(&t.persistent_id) {
            continue;
        }
        if let Some(rel) = t
            .location
            .as_deref()
            .and_then(|l| to_rel(cx.local.path(), l))
        {
            out.insert(canonical_key(&rel));
        }
    }
    Ok(out)
}

/// `track_id` の中身を `dest_rel`（root 相対の一時ファイル）へ受け取る。切れたら 3 回まで続きから
/// やり直す。`Ok` 以外とエラーでは一時ファイルを消す
pub fn download<M: Music, S: Server>(
    cx: &Ctx<'_, M, S>,
    track_id: i64,
    token: &str,
    size: u64,
    sha256: &str,
    dest_rel: &str,
) -> Result<Downloaded> {
    let res = download_inner(cx, track_id, token, size, sha256, dest_rel);
    if !matches!(res, Ok(Downloaded::Ok)) {
        cx.local.remove(dest_rel)?;
    }
    res
}

fn download_inner<M: Music, S: Server>(
    cx: &Ctx<'_, M, S>,
    track_id: i64,
    token: &str,
    size: u64,
    sha256: &str,
    dest_rel: &str,
) -> Result<Downloaded> {
    let abs = cx.local.abs(dest_rel)?;
    if let Some(p) = abs.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut w = BufWriter::new(File::create(&abs)?);
    let mut retries = 0;
    loop {
        let offset = w.get_ref().metadata()?.len();
        let res = cx.server.fetch(track_id, token, offset, &mut w);
        // 切れた場合も、書けた分を続きの起点にする
        w.flush()?;
        match res {
            Ok(Fetch::Complete) => break,
            Ok(Fetch::Changed) => return Ok(Downloaded::Changed),
            Ok(Fetch::Gone) => return Ok(Downloaded::Gone),
            Err(Error::Server(_)) if retries < FETCH_RETRIES => retries += 1,
            Err(e) => return Err(e),
        }
    }
    let f = w.into_inner().map_err(|e| Error::Io(e.into_error()))?;
    f.sync_all()?;
    let len = f.metadata()?.len();
    drop(f);
    if len != size || cx.local.sha256(dest_rel)?.as_deref() != Some(sha256) {
        return Ok(Downloaded::Mismatch);
    }
    Ok(Downloaded::Ok)
}

/// 置いたファイルの stat を取って state の行を作る
pub fn entry_for<M: Music, S: Server>(
    cx: &Ctx<'_, M, S>,
    persistent_id: &str,
    token: &str,
    path: &str,
    size: u64,
    sha256: &str,
) -> Result<TrackEntry> {
    let st = cx.local.stat(path)?.ok_or_else(|| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("置いたはずのファイルが無い（{path}）"),
        ))
    })?;
    Ok(TrackEntry {
        persistent_id: persistent_id.to_owned(),
        token: token.to_owned(),
        path: path.to_owned(),
        size,
        sha256: sha256.to_owned(),
        inode: st.inode,
        mtime_ns: st.mtime_ns,
    })
}

pub(crate) fn remove_pending<M: Music, S: Server>(cx: &mut Ctx<'_, M, S>, op_id: &str) {
    cx.state.pending_ops.retain(|o| o.op_id != op_id);
}

fn delete<M: Music, S: Server>(cx: &mut Ctx<'_, M, S>, item: &PlanItem) -> Result<Step> {
    let Some(e) = cx.state.tracks.get(&item.track_id).cloned() else {
        return Ok(Step::Satisfied);
    };
    if item.from.as_deref().map(canonical_key) != Some(canonical_key(&e.path)) {
        return Ok(Step::Dropped);
    }
    cx.state.pending_ops.push(PendingOp {
        op_id: item.op_id.clone(),
        op: PendingKind::Delete,
        ref_id: item.track_id,
        persistent_id: Some(e.persistent_id.clone()),
        from: Some(e.path.clone()),
        to: None,
        token: None,
        size: item.size,
        sha256: None,
        phase: OpPhase::Deleting,
        started_at: (cx.now)(),
        max_database_id: None,
        candidates: None,
    });
    cx.save()?;
    if cx.music.track(&e.persistent_id)?.is_some() {
        cx.music.delete_track(&e.persistent_id)?;
    }
    cx.fp.hit("exec.delete.track_deleted")?;
    cx.local.remove(&e.path)?;
    cx.state.tracks.remove(&item.track_id);
    remove_pending(cx, &item.op_id);
    cx.save()?;
    Ok(Step::Done)
}

/// 取得の結果が `Ok` 以外なら pending を外して結果の Step を返す
fn after_download<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    item: &PlanItem,
    d: Downloaded,
) -> Result<Option<Step>> {
    let step = match d {
        Downloaded::Ok => return Ok(None),
        Downloaded::Changed | Downloaded::Gone => Step::Dropped,
        Downloaded::Mismatch => {
            cx.error_track(item.track_id, CONTENT_MISMATCH);
            Step::Errored
        }
    };
    remove_pending(cx, &item.op_id);
    cx.save()?;
    Ok(Some(step))
}

fn update<M: Music, S: Server>(cx: &mut Ctx<'_, M, S>, item: &PlanItem) -> Result<Step> {
    let (Some(to), Some(token), Some(sha)) = (&item.to, &item.token, &item.sha256) else {
        return Ok(Step::Dropped);
    };
    let Some(e) = cx.state.tracks.get(&item.track_id).cloned() else {
        return Ok(Step::Dropped);
    };
    if canonical_key(&e.path) != canonical_key(to) {
        return Ok(Step::Dropped);
    }
    let path = e.path.clone();
    let pid = e.persistent_id.clone();
    cx.state.pending_ops.push(PendingOp {
        op_id: item.op_id.clone(),
        op: PendingKind::Update,
        ref_id: item.track_id,
        persistent_id: Some(pid.clone()),
        from: None,
        to: Some(path.clone()),
        token: Some(token.clone()),
        size: item.size,
        sha256: Some(sha.clone()),
        phase: OpPhase::Fetching,
        started_at: (cx.now)(),
        max_database_id: None,
        candidates: None,
    });
    cx.save()?;
    let tmp = format!("{path}{TMP_SUFFIX}");
    let d = download(cx, item.track_id, token, item.size, sha, &tmp)?;
    if let Some(step) = after_download(cx, item, d)? {
        return Ok(step);
    }
    cx.local.place(&tmp, &path)?;
    cx.fp.hit("exec.update.placed")?;
    cx.music.refresh(&pid)?;
    let entry = entry_for(cx, &pid, token, &path, item.size, sha)?;
    cx.state.tracks.insert(item.track_id, entry);
    remove_pending(cx, &item.op_id);
    cx.save()?;
    Ok(Step::Done)
}

fn add<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    item: &PlanItem,
    unmanaged: &HashSet<String>,
) -> Result<Step> {
    let (Some(to), Some(token), Some(sha)) = (&item.to, &item.token, &item.sha256) else {
        return Ok(Step::Dropped);
    };
    let key = canonical_key(to);
    if unmanaged.contains(&key) {
        cx.error_track(item.track_id, UNMANAGED_COLLISION);
        return Ok(Step::Errored);
    }
    if cx
        .state
        .tracks
        .iter()
        .any(|(id, e)| *id != item.track_id && canonical_key(&e.path) == key)
    {
        cx.error_track(item.track_id, PATH_OCCUPIED);
        return Ok(Step::Errored);
    }
    if cx.state.tracks.contains_key(&item.track_id) {
        // 満たされていない追加はここに来ない。来たら計画と食い違っている
        return Ok(Step::Dropped);
    }
    cx.state.pending_ops.push(PendingOp {
        op_id: item.op_id.clone(),
        op: PendingKind::Add,
        ref_id: item.track_id,
        persistent_id: None,
        from: None,
        to: Some(to.clone()),
        token: Some(token.clone()),
        size: item.size,
        sha256: Some(sha.clone()),
        phase: OpPhase::Fetching,
        started_at: (cx.now)(),
        max_database_id: None,
        candidates: None,
    });
    cx.save()?;
    let tmp = format!("{to}{TMP_SUFFIX}");
    let d = download(cx, item.track_id, token, item.size, sha, &tmp)?;
    if let Some(step) = after_download(cx, item, d)? {
        return Ok(step);
    }
    cx.local.place(&tmp, to)?;
    cx.fp.hit("exec.add.placed")?;
    let max_db = cx.music.max_database_id()?;
    if let Some(o) = cx
        .state
        .pending_ops
        .iter_mut()
        .find(|o| o.op_id == item.op_id)
    {
        o.max_database_id = Some(max_db);
        o.phase = OpPhase::Adding;
    }
    cx.save()?;
    let t = cx.music.add(&cx.local.abs(to)?)?;
    let placed_key = t
        .location
        .as_deref()
        .and_then(|l| to_rel(cx.local.path(), l))
        .map(|r| canonical_key(&r));
    if placed_key.as_deref() != Some(key.as_str()) {
        // 「［ミュージック］フォルダにコピー」が ON で複製された。この add が返した track なので
        // 通常経路で消してよい
        cx.music.delete_track(&t.persistent_id)?;
        cx.local.remove(to)?;
        remove_pending(cx, &item.op_id);
        cx.save()?;
        return Ok(Step::Stop(COPY_ON_STOP.to_owned()));
    }
    let entry = entry_for(cx, &t.persistent_id, token, to, item.size, sha)?;
    cx.state.tracks.insert(item.track_id, entry);
    remove_pending(cx, &item.op_id);
    cx.save()?;
    Ok(Step::Done)
}

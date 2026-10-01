//! プレイリストの反映（仕様 ⑥「プレイリスト」、D-100 判断 11・15）。spindle フォルダの直下だけを扱う。
//! 追加・更新は一時プレイリスト `.tmp-<op_id>` を作って中身を入れ、旧を消してから改名する
//! （入れ替え）。削除を全部先に、それから追加・更新。途中で落ちたものは `recover` が片付ける

use std::collections::HashSet;

use agent_proto::{ManifestResponse, PlanPlaylist, PlaylistOpKind};

use crate::ctx::Ctx;
use crate::exec::{remove_pending, UNMANAGED_COLLISION};
use crate::music::{Music, MusicPlaylist};
use crate::pathkey::canonical_key;
use crate::rediscover::pending_pids;
use crate::server::Server;
use crate::state::{OpPhase, PendingKind, PendingOp, PlaylistEntry};
use crate::{Error, Result};

/// 一時プレイリストの名前の接頭辞（`.tmp-<op_id>`）
pub const TMP_PREFIX: &str = ".tmp-";

/// 1 操作の結果
enum Step {
    Done,
    /// 既に満たされていた（数えない）
    Satisfied,
    Dropped,
    /// 項目のエラー（`cx.errors` に記録済み）
    Errored,
}

/// 開始時に見た spindle フォルダの様子
struct Scene {
    folder: String,
    /// フォルダの直下のプレイリスト
    existing: Vec<MusicPlaylist>,
    /// 管理下の persistent ID（state の `playlists` と pending）
    managed: HashSet<String>,
}

impl Scene {
    fn has(&self, pid: &str) -> bool {
        self.existing.iter().any(|p| p.persistent_id == pid)
    }
}

/// spindle フォルダの persistent ID
pub(crate) fn folder_pid<M: Music, S: Server>(cx: &Ctx<'_, M, S>) -> Result<String> {
    cx.state
        .setup
        .as_ref()
        .and_then(|s| s.folder_pid.clone())
        .ok_or_else(|| Error::State("spindle フォルダの persistent ID が記録されていない".into()))
}

/// 確定した計画のプレイリストの操作を実行する。（実行した数, 実行しなかった数）を返す
pub fn run<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    ops: &[PlanPlaylist],
    m: &ManifestResponse,
) -> Result<(usize, usize)> {
    if ops.is_empty() {
        return Ok((0, 0));
    }
    let folder = folder_pid(cx)?;
    let existing = cx.music.playlists_in(&folder)?;
    let mut managed = pending_pids(&cx.state);
    managed.extend(cx.state.playlists.values().map(|p| p.persistent_id.clone()));
    let scene = Scene {
        folder,
        existing,
        managed,
    };

    let (mut executed, mut dropped) = (0, 0);
    let deletes = ops.iter().filter(|o| o.op == PlaylistOpKind::Delete);
    let writes = ops.iter().filter(|o| o.op != PlaylistOpKind::Delete);
    for op in deletes.chain(writes) {
        let step = match op.op {
            PlaylistOpKind::Delete => delete(cx, &scene, op)?,
            PlaylistOpKind::Add | PlaylistOpKind::Update => write(cx, &scene, op, m)?,
        };
        match step {
            Step::Done => executed += 1,
            Step::Dropped => dropped += 1,
            Step::Satisfied | Step::Errored => {}
        }
    }
    Ok((executed, dropped))
}

fn delete<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    scene: &Scene,
    op: &PlanPlaylist,
) -> Result<Step> {
    let Some(e) = cx.state.playlists.get(&op.playlist_id).cloned() else {
        return Ok(Step::Satisfied);
    };
    if op.from.as_deref().map(canonical_key) != Some(canonical_key(&e.name)) {
        return Ok(Step::Dropped);
    }
    cx.state.pending_ops.push(PendingOp {
        op_id: op.op_id.clone(),
        op: PendingKind::PlaylistDelete,
        ref_id: op.playlist_id,
        persistent_id: Some(e.persistent_id.clone()),
        from: Some(e.name.clone()),
        to: None,
        token: None,
        size: 0,
        sha256: None,
        phase: OpPhase::Deleting,
        started_at: (cx.now)(),
        max_database_id: None,
        candidates: None,
    });
    cx.save()?;
    if scene.has(&e.persistent_id) {
        cx.music.delete_playlist(&e.persistent_id)?;
    }
    cx.state.playlists.remove(&op.playlist_id);
    remove_pending(cx, &op.op_id);
    cx.save()?;
    Ok(Step::Done)
}

fn write<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    scene: &Scene,
    op: &PlanPlaylist,
    m: &ManifestResponse,
) -> Result<Step> {
    let (Some(name), Some(token)) = (&op.to, &op.token) else {
        return Ok(Step::Dropped);
    };
    let Some(mp) = m
        .playlists
        .iter()
        .find(|p| p.playlist_id == op.playlist_id && &p.token == token)
    else {
        return Ok(Step::Dropped);
    };
    let key = canonical_key(name);
    if scene
        .existing
        .iter()
        .any(|p| canonical_key(&p.name) == key && !scene.managed.contains(&p.persistent_id))
    {
        cx.error_playlist(op.playlist_id, UNMANAGED_COLLISION);
        return Ok(Step::Errored);
    }
    // state に無い曲（hold で写しが無い・この実行で取れなかった）は飛ばす
    let pids: Vec<String> = mp
        .tracks
        .iter()
        .filter_map(|id| cx.state.tracks.get(id).map(|e| e.persistent_id.clone()))
        .collect();

    cx.state.pending_ops.push(PendingOp {
        op_id: op.op_id.clone(),
        op: PendingKind::Playlist,
        ref_id: op.playlist_id,
        persistent_id: None,
        from: None,
        to: Some(name.clone()),
        token: Some(token.clone()),
        size: 0,
        sha256: None,
        phase: OpPhase::Creating,
        started_at: (cx.now)(),
        max_database_id: None,
        candidates: None,
    });
    cx.save()?;
    let tmp = cx
        .music
        .create_playlist(&scene.folder, &format!("{TMP_PREFIX}{}", op.op_id))?;
    if let Some(o) = cx
        .state
        .pending_ops
        .iter_mut()
        .find(|o| o.op_id == op.op_id)
    {
        o.persistent_id = Some(tmp.persistent_id.clone());
        o.phase = OpPhase::Filling;
    }
    cx.save()?;
    cx.music.set_playlist_tracks(&tmp.persistent_id, &pids)?;
    if let Some(old) = cx.state.playlists.get(&op.playlist_id) {
        if scene.has(&old.persistent_id) {
            cx.music.delete_playlist(&old.persistent_id)?;
        }
    }
    cx.fp.hit("playlist.old_deleted")?;
    cx.music.rename_playlist(&tmp.persistent_id, name)?;
    cx.state.playlists.insert(
        op.playlist_id,
        PlaylistEntry {
            persistent_id: tmp.persistent_id,
            name: name.clone(),
            token: token.clone(),
        },
    );
    remove_pending(cx, &op.op_id);
    cx.save()?;
    Ok(Step::Done)
}

/// 削除の途中で止まった: 在れば消し、state の行と pending を外す
pub(crate) fn recover_delete<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    op: &PendingOp,
    pid: &str,
) -> Result<()> {
    let folder = folder_pid(cx)?;
    if cx
        .music
        .playlists_in(&folder)?
        .iter()
        .any(|p| p.persistent_id == pid)
    {
        cx.music.delete_playlist(pid)?;
    }
    if cx
        .state
        .playlists
        .get(&op.ref_id)
        .is_some_and(|e| e.persistent_id == pid)
    {
        cx.state.playlists.remove(&op.ref_id);
    }
    remove_pending(cx, &op.op_id);
    cx.state.needs_report = true;
    cx.save()
}

/// 一時プレイリストを作る前後で止まった（persistent ID は未記録）。フォルダの直下で名前が
/// ちょうど `.tmp-<op_id>` のものが 1 つだけなら消す（op_id はこの操作だけの 128 bit 乱数。判断 15）
pub(crate) fn recover_creating<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    op: &PendingOp,
) -> Result<()> {
    let folder = folder_pid(cx)?;
    let tmp_name = format!("{TMP_PREFIX}{}", op.op_id);
    let found: Vec<MusicPlaylist> = cx
        .music
        .playlists_in(&folder)?
        .into_iter()
        .filter(|p| p.name == tmp_name)
        .collect();
    if let [p] = found.as_slice() {
        cx.music.delete_playlist(&p.persistent_id)?;
    }
    remove_pending(cx, &op.op_id);
    cx.save()
}

/// 一時プレイリストの persistent ID を記録した後で止まった。記録した ID のものだけを消す
/// （改名の後でも今の名前は問わない）。旧が既に消えていれば、state の行は再発見が外し、再開が作り直す
pub(crate) fn recover_filling<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    op: &PendingOp,
    pid: &str,
) -> Result<()> {
    let folder = folder_pid(cx)?;
    if cx
        .music
        .playlists_in(&folder)?
        .iter()
        .any(|p| p.persistent_id == pid)
    {
        cx.music.delete_playlist(pid)?;
    }
    remove_pending(cx, &op.op_id);
    cx.save()
}

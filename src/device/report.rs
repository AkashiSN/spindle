//! エージェントの報告の検証と正準ダイジェスト（純粋関数）。
//! 報告は端末の実状態の自己申告なので、サーバが知っている材料（希望・現状・確定した計画）の
//! どれかに 5 項目すべて（プレイリストは id・名前の鍵・トークン）一致するものだけを受け入れる。
//! 例外として、トークンが空の曲（古い写し）は (track_id, パスの鍵) の一致だけで受け入れる（D-100）

use std::collections::HashSet;

use agent_proto::{ErrorKind, ReportRequest};
use serde_json::json;

use crate::device::ondevice::is_reserved;
use crate::device::plan::StoredPlan;
use crate::device::recover::STALE_TOKEN;
use crate::domain::device::{
    canonical_sha256, DesiredItem, DesiredPlaylist, DeviceItem, EntryKind, OpKind, PlaylistOpKind,
    PlaylistState,
};
use crate::domain::relpath::{canonical_key, RelPath};

pub const MAX_TRACKS: usize = 100_000;
pub const MAX_PLAYLISTS: usize = 100_000;
pub const MAX_ERRORS: usize = 200_000;
pub const MAX_REASON_CHARS: usize = 1_000;
const MAX_REASONS: usize = 20;

/// 照合の材料（書き込みトランザクションの中で読んだもの）
#[derive(Clone, Copy)]
pub struct Basis<'a> {
    pub desired: &'a [DesiredItem],
    pub desired_playlists: &'a [DesiredPlaylist],
    pub current: &'a [DeviceItem],
    pub current_playlists: &'a [PlaylistState],
    pub plan: &'a StoredPlan,
}

/// 検証を通った報告（`apply_device_state` にそのまま渡せる形）
pub struct Accepted {
    pub items: Vec<DeviceItem>,
    pub playlists: Vec<PlaylistState>,
    pub errors: Vec<(EntryKind, i64, String)>,
}

type TrackKey<'a> = (i64, &'a str, &'a str, u64, &'a str);

/// 全項目を検証する。1 件でも外れれば Err（理由の一覧。先頭 20 件まで）
pub fn validate(r: &ReportRequest, b: &Basis<'_>) -> Result<Accepted, Vec<String>> {
    let mut reasons: Vec<String> = Vec::new();
    if r.state.tracks.len() > MAX_TRACKS {
        reasons.push(format!("曲の件数が上限（{MAX_TRACKS}）を超えています"));
    }
    if r.state.playlists.len() > MAX_PLAYLISTS {
        reasons.push(format!(
            "プレイリストの件数が上限（{MAX_PLAYLISTS}）を超えています"
        ));
    }
    if r.errors.len() > MAX_ERRORS {
        reasons.push(format!("エラーの件数が上限（{MAX_ERRORS}）を超えています"));
    }
    if !reasons.is_empty() {
        reasons.truncate(MAX_REASONS);
        return Err(reasons);
    }

    // 受け入れてよい曲の集合
    let mut ok_tracks: HashSet<TrackKey<'_>> = HashSet::new();
    for d in b.desired {
        ok_tracks.insert((d.track_id, &d.dest_path, &d.token, d.size, &d.sha256));
    }
    for c in b.current {
        ok_tracks.insert((c.track_id, &c.dest_path, &c.token, c.size, &c.sha256));
    }
    for p in &b.plan.items {
        if !matches!(
            p.op,
            OpKind::Add | OpKind::Update | OpKind::Move | OpKind::UpdateMove
        ) {
            continue;
        }
        if let (Some(to), Some(token), Some(sha)) = (&p.to, &p.token, &p.sha256) {
            ok_tracks.insert((p.track_id, to, token, p.size, sha));
        }
    }
    // トークンが空（古い写し）の曲は size・sha256 を問わず、(track_id, パスの鍵) だけで照合する
    let mut ok_stale: HashSet<(i64, String)> = HashSet::new();
    for d in b.desired {
        ok_stale.insert((d.track_id, canonical_key(&d.dest_path)));
    }
    for c in b.current {
        ok_stale.insert((c.track_id, canonical_key(&c.dest_path)));
    }
    for p in &b.plan.items {
        if matches!(
            p.op,
            OpKind::Add | OpKind::Update | OpKind::Move | OpKind::UpdateMove
        ) {
            if let Some(to) = &p.to {
                ok_stale.insert((p.track_id, canonical_key(to)));
            }
        }
    }
    // 受け入れてよいプレイリストの集合
    let mut ok_pls: HashSet<(i64, String, &str)> = HashSet::new();
    for d in b.desired_playlists {
        ok_pls.insert((d.playlist_id, canonical_key(&d.dest_path), &d.token));
    }
    for c in b.current_playlists {
        ok_pls.insert((c.playlist_id, canonical_key(&c.dest_path), &c.token));
    }
    for p in &b.plan.playlists {
        if !matches!(p.op, PlaylistOpKind::Add | PlaylistOpKind::Update) {
            continue;
        }
        if let (Some(to), Some(token)) = (&p.to, &p.token) {
            ok_pls.insert((p.playlist_id, canonical_key(to), token));
        }
    }

    let mut ids = HashSet::new();
    let mut keys = HashSet::new();
    for t in &r.state.tracks {
        if RelPath::parse(&t.dest_path).is_err() || is_reserved(&t.dest_path) {
            reasons.push(format!(
                "曲 {} のパスが不正です（{}）",
                t.track_id, t.dest_path
            ));
            continue;
        }
        if !ids.insert(t.track_id) {
            reasons.push(format!("曲 {} が重複しています", t.track_id));
        }
        if !keys.insert(canonical_key(&t.dest_path)) {
            reasons.push(format!(
                "曲 {} のパスが他の曲と衝突しています（{}）",
                t.track_id, t.dest_path
            ));
        }
        let k: TrackKey<'_> = (t.track_id, &t.dest_path, &t.token, t.size, &t.sha256);
        let known = if t.token == STALE_TOKEN {
            ok_stale.contains(&(t.track_id, canonical_key(&t.dest_path)))
        } else {
            ok_tracks.contains(&k)
        };
        if !known {
            reasons.push(format!(
                "曲 {} が希望・現状・計画のどれとも一致しません",
                t.track_id
            ));
        }
    }

    let mut pl_ids = HashSet::new();
    let mut pl_keys = HashSet::new();
    for p in &r.state.playlists {
        if p.name.is_empty() {
            reasons.push(format!("プレイリスト {} の名前が空です", p.playlist_id));
            continue;
        }
        if !pl_ids.insert(p.playlist_id) {
            reasons.push(format!("プレイリスト {} が重複しています", p.playlist_id));
        }
        let key = canonical_key(&p.name);
        if !pl_keys.insert(key.clone()) {
            reasons.push(format!(
                "プレイリスト {} の名前が他と衝突しています",
                p.playlist_id
            ));
        }
        if !ok_pls.contains(&(p.playlist_id, key, p.token.as_str())) {
            reasons.push(format!(
                "プレイリスト {} が希望・現状・計画のどれとも一致しません",
                p.playlist_id
            ));
        }
    }

    let mut err_keys = HashSet::new();
    for e in &r.errors {
        if e.reason.is_empty() || e.reason.chars().count() > MAX_REASON_CHARS {
            reasons.push(format!("エラー {} の理由が不正です", e.ref_id));
        }
        if !err_keys.insert((e.kind, e.ref_id)) {
            reasons.push(format!("エラー {} が重複しています", e.ref_id));
        }
    }

    if !reasons.is_empty() {
        reasons.truncate(MAX_REASONS);
        return Err(reasons);
    }
    Ok(Accepted {
        items: r
            .state
            .tracks
            .iter()
            .map(|t| DeviceItem {
                track_id: t.track_id,
                dest_path: t.dest_path.clone(),
                token: t.token.clone(),
                size: t.size,
                sha256: t.sha256.clone(),
            })
            .collect(),
        playlists: r
            .state
            .playlists
            .iter()
            .map(|p| PlaylistState {
                playlist_id: p.playlist_id,
                dest_path: p.name.clone(),
                token: p.token.clone(),
            })
            .collect(),
        errors: r
            .errors
            .iter()
            .map(|e| {
                let kind = match e.kind {
                    ErrorKind::Track => EntryKind::Track,
                    ErrorKind::Playlist => EntryKind::Playlist,
                };
                (kind, e.ref_id, e.reason.clone())
            })
            .collect(),
    })
}

/// `{v:1, plan_id, generation, state, errors}` の正準 JSON の SHA-256。
/// tracks は track_id、playlists は playlist_id、errors は (kind, ref_id) の昇順に並べてから
pub fn digest(r: &ReportRequest) -> String {
    let mut tracks: Vec<_> = r.state.tracks.iter().collect();
    tracks.sort_by_key(|t| t.track_id);
    let mut pls: Vec<_> = r.state.playlists.iter().collect();
    pls.sort_by_key(|p| p.playlist_id);
    let mut errs: Vec<_> = r.errors.iter().collect();
    errs.sort_by_key(|e| (e.kind, e.ref_id));
    let v = json!({
        "v": 1,
        "plan_id": r.plan_id,
        "generation": r.generation,
        "state": {
            "tracks": tracks.iter().map(|t| json!({
                "track_id": t.track_id, "dest_path": t.dest_path, "token": t.token,
                "size": t.size, "sha256": t.sha256,
            })).collect::<Vec<_>>(),
            "playlists": pls.iter().map(|p| json!({
                "playlist_id": p.playlist_id, "name": p.name, "token": p.token,
            })).collect::<Vec<_>>(),
        },
        "errors": errs.iter().map(|e| json!({
            "kind": e.kind, "ref_id": e.ref_id, "reason": e.reason,
        })).collect::<Vec<_>>(),
    });
    canonical_sha256(&v)
}

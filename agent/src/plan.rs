//! 確定した計画の再開の照合（仕様 ③「開始済みの計画の再開」）。本体の `device::plan` の移植。
//! 直すときは両方を直す。
//! 実行するのは計画の部分集合だけ: 今の状態で満たされている操作は飛ばし、今の差分にも同じ操作・
//! 同じトークン・同じパスで残っているものだけを実行する。計画に無い操作（とくに新しい削除）は
//! 実行しない。パス変更は入れ替え・循環の成分ごとに全員残すか全員外す。
//! 例外として、プレイリストの改名が旧パスの削除の後で切れた中間状態は、残りを追加として再開する

use std::collections::{BTreeMap, HashMap, HashSet};

use agent_proto::{
    DiffView, ItemOp, OpKind, Plan, PlanItem, PlanPlaylist, PlaylistOp, PlaylistOpKind,
};

use crate::pathkey::canonical_key;
use crate::state::State;

/// 今の写し（state の `tracks`）の 1 曲
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Current {
    pub track_id: i64,
    pub path: String,
    pub token: String,
}

/// 今の写し（state の `playlists`）の 1 件。agent のプレイリストの dest_path は名前（D-99）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentPlaylist {
    pub playlist_id: i64,
    pub name: String,
    pub token: String,
}

pub fn current_of(s: &State) -> (Vec<Current>, Vec<CurrentPlaylist>) {
    let tracks = s
        .tracks
        .iter()
        .map(|(id, e)| Current {
            track_id: *id,
            path: e.path.clone(),
            token: e.token.clone(),
        })
        .collect();
    let playlists = s
        .playlists
        .iter()
        .map(|(id, e)| CurrentPlaylist {
            playlist_id: *id,
            name: e.name.clone(),
            token: e.token.clone(),
        })
        .collect();
    (tracks, playlists)
}

/// 計画のうち、今回実行する部分
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Runnable {
    pub items: Vec<PlanItem>,
    pub playlists: Vec<PlanPlaylist>,
    /// 既に満たされていて飛ばした操作の数
    pub satisfied: usize,
    /// 今の差分と食い違うので実行しない操作の数（次の差分に回る）
    pub dropped: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    Satisfied,
    Run,
    Drop,
}

fn same_item(p: &PlanItem, o: &ItemOp) -> bool {
    p.op == o.op
        && p.track_id == o.track_id
        && p.from == o.from
        && p.to == o.to
        && p.token == o.token
}

fn same_playlist(p: &PlanPlaylist, o: &PlaylistOp) -> bool {
    p.op == o.op
        && p.playlist_id == o.playlist_id
        && p.from == o.from
        && p.to == o.to
        && p.token == o.token
}

fn item_satisfied(p: &PlanItem, cur: &HashMap<i64, &Current>) -> bool {
    match p.op {
        OpKind::Delete => !cur.contains_key(&p.track_id),
        _ => cur
            .get(&p.track_id)
            .is_some_and(|c| Some(&c.path) == p.to.as_ref() && Some(&c.token) == p.token.as_ref()),
    }
}

fn playlist_satisfied(p: &PlanPlaylist, cur: &HashMap<i64, &CurrentPlaylist>) -> bool {
    match p.op {
        PlaylistOpKind::Delete => !cur.contains_key(&p.playlist_id),
        _ => cur
            .get(&p.playlist_id)
            .is_some_and(|c| Some(&c.name) == p.to.as_ref() && Some(&c.token) == p.token.as_ref()),
    }
}

pub fn runnable(
    plan: &Plan,
    current: &[Current],
    current_playlists: &[CurrentPlaylist],
    now: &DiffView,
) -> Runnable {
    let cur: HashMap<i64, &Current> = current.iter().map(|c| (c.track_id, c)).collect();
    let now_items: HashMap<i64, &ItemOp> = now.items.iter().map(|o| (o.track_id, o)).collect();
    let mut fates: Vec<Fate> = plan
        .items
        .iter()
        .map(|p| {
            if item_satisfied(p, &cur) {
                Fate::Satisfied
            } else if now_items.get(&p.track_id).is_some_and(|o| same_item(p, o)) {
                Fate::Run
            } else {
                Fate::Drop
            }
        })
        .collect();
    for comp in path_components(&plan.items) {
        if comp.iter().any(|&i| fates[i] == Fate::Drop) {
            for &i in &comp {
                if fates[i] == Fate::Run {
                    fates[i] = Fate::Drop;
                }
            }
        }
    }
    let mut out = Runnable::default();
    for (p, f) in plan.items.iter().zip(fates) {
        match f {
            Fate::Satisfied => out.satisfied += 1,
            Fate::Run => out.items.push(p.clone()),
            Fate::Drop => out.dropped += 1,
        }
    }
    let cur_pl: HashMap<i64, &CurrentPlaylist> = current_playlists
        .iter()
        .map(|c| (c.playlist_id, c))
        .collect();
    let now_pl: HashMap<i64, &PlaylistOp> =
        now.playlists.iter().map(|o| (o.playlist_id, o)).collect();
    for p in &plan.playlists {
        let now_op = now_pl.get(&p.playlist_id);
        if playlist_satisfied(p, &cur_pl) {
            out.satisfied += 1;
        } else if now_op.is_some_and(|o| same_playlist(p, o)) {
            out.playlists.push(p.clone());
        } else if now_op.is_some_and(|o| rename_old_path_removed(p, o, &cur_pl)) {
            // 改名の旧パスの削除が済んだ中間状態: 残りの新パスへの書き込みを追加として実行する
            out.playlists.push(PlanPlaylist {
                op: PlaylistOpKind::Add,
                from: None,
                ..p.clone()
            });
        } else {
            out.dropped += 1;
        }
    }
    out
}

/// プレイリストの改名（旧パスの削除 → 新パスへの書き込み）が、旧パスを消した後・新パスを書く前に
/// 切れた中間状態か。今の状態にその行が無く、今の差分が同じ行き先・同じトークンの追加なら正当とみなす。
/// 曲のパス変更はバッチで原子的に完遂される（回復が前進か破棄に決める）ので、同様の中間状態は無い
fn rename_old_path_removed(
    p: &PlanPlaylist,
    o: &PlaylistOp,
    cur: &HashMap<i64, &CurrentPlaylist>,
) -> bool {
    p.op == PlaylistOpKind::Update
        && p.from.is_some()
        && p.from != p.to
        && !cur.contains_key(&p.playlist_id)
        && o.op == PlaylistOpKind::Add
        && o.playlist_id == p.playlist_id
        && o.from.is_none()
        && o.to == p.to
        && o.token == p.token
}

fn find(parent: &mut [usize], i: usize) -> usize {
    let mut r = i;
    while parent[r] != r {
        r = parent[r];
    }
    let mut j = i;
    while parent[j] != r {
        let next = parent[j];
        parent[j] = r;
        j = next;
    }
    r
}

/// パス変更（移動・更新 + 移動）を、ある操作の行き先が別の操作の移動元と同じパス
/// （`canonical_key`）になるものどうしで成分に分ける（入れ替え・循環・連鎖）。
/// 返すのは `items` の添字の集合（各成分は昇順、成分は先頭の添字の昇順）
pub fn path_components(items: &[PlanItem]) -> Vec<Vec<usize>> {
    let idx: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, p)| matches!(p.op, OpKind::Move | OpKind::UpdateMove))
        .map(|(i, _)| i)
        .collect();
    let mut parent: Vec<usize> = (0..items.len()).collect();
    let by_from: HashMap<String, usize> = idx
        .iter()
        .filter_map(|&i| items[i].from.as_deref().map(|f| (canonical_key(f), i)))
        .collect();
    for &i in &idx {
        if let Some(to) = items[i].to.as_deref() {
            if let Some(&j) = by_from.get(&canonical_key(to)) {
                let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                if a != b {
                    parent[a.max(b)] = a.min(b);
                }
            }
        }
    }
    let mut groups: BTreeMap<usize, Vec<usize>> = Default::default();
    for &i in &idx {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    let mut out: Vec<Vec<usize>> = groups.into_values().collect();
    out.sort_by_key(|c| c[0]);
    out
}

/// `tracks` の曲を含む成分（入れ替え・循環・連鎖）を全員外す
pub fn drop_components(ops: Vec<PlanItem>, tracks: &HashSet<i64>) -> Vec<PlanItem> {
    if tracks.is_empty() {
        return ops;
    }
    let drop: HashSet<usize> = path_components(&ops)
        .into_iter()
        .filter(|c| c.iter().any(|&i| tracks.contains(&ops[i].track_id)))
        .flatten()
        .collect();
    ops.into_iter()
        .enumerate()
        .filter(|(i, _)| !drop.contains(i))
        .map(|(_, o)| o)
        .collect()
}

//! 確定した計画（`device_sync_plans.plan`）と、開始済みの計画の再開（仕様 ③）。
//! 実行するのは計画の部分集合だけ: 今の状態で満たされている操作は飛ばし、今の差分にも同じ操作・
//! 同じトークン・同じパスで残っているものだけを実行する。計画に無い操作（とくに新しい削除）は
//! 実行しない。パス変更は入れ替え・循環の成分ごとに全員残すか全員外す。
//! 例外として、プレイリストの改名が旧パスの削除の後で切れた中間状態は、残りを追加として再開する

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::device::journal::random_id;
use crate::domain::device::{
    DeviceItem, Diff, ItemOp, OpKind, PlaylistOp, PlaylistOpKind, PlaylistState,
};
use crate::domain::relpath::canonical_key;

pub const PLAN_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    pub op_id: String,
    pub op: OpKind,
    pub track_id: i64,
    pub from: Option<String>,
    pub to: Option<String>,
    pub token: Option<String>,
    pub size: u64,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanPlaylist {
    pub op_id: String,
    pub op: PlaylistOpKind,
    pub playlist_id: i64,
    pub from: Option<String>,
    pub to: Option<String>,
    pub token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredPlan {
    pub v: u32,
    pub generation: i64,
    pub plan_token: String,
    pub items: Vec<PlanItem>,
    pub playlists: Vec<PlanPlaylist>,
}

impl StoredPlan {
    /// 差分から計画を作る。操作ごとに `op_id`（128 bit 乱数）を振る
    pub fn from_diff(generation: i64, plan_token: &str, d: &Diff) -> std::io::Result<StoredPlan> {
        let mut items = Vec::with_capacity(d.items.len());
        for o in &d.items {
            items.push(PlanItem {
                op_id: random_id()?,
                op: o.kind,
                track_id: o.track_id,
                from: o.from.clone(),
                to: o.to.clone(),
                token: o.token.clone(),
                size: o.size,
                sha256: o.sha256.clone(),
            });
        }
        let mut playlists = Vec::with_capacity(d.playlists.len());
        for p in &d.playlists {
            playlists.push(PlanPlaylist {
                op_id: random_id()?,
                op: p.kind,
                playlist_id: p.playlist_id,
                from: p.from.clone(),
                to: p.to.clone(),
                token: p.token.clone(),
            });
        }
        Ok(StoredPlan {
            v: PLAN_VERSION,
            generation,
            plan_token: plan_token.to_owned(),
            items,
            playlists,
        })
    }
}

/// 計画のうち、今回実行する部分
#[derive(Debug, Clone, PartialEq, Eq, Default)]
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
    p.op == o.kind
        && p.track_id == o.track_id
        && p.from == o.from
        && p.to == o.to
        && p.token == o.token
}

fn same_playlist(p: &PlanPlaylist, o: &PlaylistOp) -> bool {
    p.op == o.kind
        && p.playlist_id == o.playlist_id
        && p.from == o.from
        && p.to == o.to
        && p.token == o.token
}

fn item_satisfied(p: &PlanItem, cur: &HashMap<i64, &DeviceItem>) -> bool {
    match p.op {
        OpKind::Delete => !cur.contains_key(&p.track_id),
        _ => cur.get(&p.track_id).is_some_and(|c| {
            Some(&c.dest_path) == p.to.as_ref() && Some(&c.token) == p.token.as_ref()
        }),
    }
}

fn playlist_satisfied(p: &PlanPlaylist, cur: &HashMap<i64, &PlaylistState>) -> bool {
    match p.op {
        PlaylistOpKind::Delete => !cur.contains_key(&p.playlist_id),
        _ => cur.get(&p.playlist_id).is_some_and(|c| {
            Some(&c.dest_path) == p.to.as_ref() && Some(&c.token) == p.token.as_ref()
        }),
    }
}

pub fn runnable(
    plan: &StoredPlan,
    current: &[DeviceItem],
    current_playlists: &[PlaylistState],
    now: &Diff,
) -> Runnable {
    let cur: HashMap<i64, &DeviceItem> = current.iter().map(|c| (c.track_id, c)).collect();
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
    let cur_pl: HashMap<i64, &PlaylistState> = current_playlists
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
    cur: &HashMap<i64, &PlaylistState>,
) -> bool {
    p.op == PlaylistOpKind::Update
        && p.from.is_some()
        && p.from != p.to
        && !cur.contains_key(&p.playlist_id)
        && o.kind == PlaylistOpKind::Add
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
    let mut groups: std::collections::BTreeMap<usize, Vec<usize>> = Default::default();
    for &i in &idx {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    let mut out: Vec<Vec<usize>> = groups.into_values().collect();
    out.sort_by_key(|c| c[0]);
    out
}

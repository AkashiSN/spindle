//! 偽の spindle サーバ（試験用）。差分は「desired」と「最後に受け入れた報告（current）」から本体と同じ
//! 規則で作る: 追加 / 更新（同じパス）/ 更新 + 移動 / 移動 / 削除（desired にも hold にも無い current）。
//! hold は `held` に入れた track_id（既存の写しに触らない）。プレイリストは名前（= dest_path）で同じ規則

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Write;
use std::rc::Rc;

use agent_proto::*;
use sha2::{Digest, Sha256};

use crate::server::{Confirmed, Fetch, Reported, Server};
use crate::{Error, Result};

#[derive(Default)]
struct World {
    device_uuid: String,
    generation: i64,
    pending_reevaluation: bool,
    desired: BTreeMap<i64, (ManifestItem, Vec<u8>)>,
    held: BTreeMap<i64, String>,
    playlists: BTreeMap<i64, ManifestPlaylist>,
    current: Vec<ReportTrack>,
    current_playlists: Vec<ReportPlaylist>,
    open: Option<Plan>,
    next_plan: i64,
    next_op: u64,
    reports: Vec<ReportRequest>,
    abandons: Vec<AbandonRequest>,
    /// track_id → 412 を返す回数
    fetch_changed: BTreeMap<i64, u32>,
    /// track_id → この回数だけ、半分まで書いて切る
    fetch_cut: BTreeMap<i64, u32>,
    fetch_log: Vec<(i64, u64)>,
}

#[derive(Clone, Default)]
pub struct FakeServer(Rc<RefCell<World>>);

pub fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

impl FakeServer {
    pub fn new(device_uuid: &str) -> Self {
        let s = Self::default();
        {
            let mut w = s.0.borrow_mut();
            w.device_uuid = device_uuid.to_owned();
            w.generation = 1;
            w.next_plan = 1;
        }
        s
    }

    /// 曲を desired に入れる（トークンは中身とパスから作る）
    pub fn put_track(&self, track_id: i64, dest_path: &str, bytes: &[u8]) -> ManifestItem {
        let sha = sha256_hex(bytes);
        let item = ManifestItem {
            track_id,
            dest_path: dest_path.to_owned(),
            token: sha256_hex(format!("tok:{track_id}:{sha}").as_bytes()),
            size: bytes.len() as u64,
            sha256: sha,
        };
        let mut w = self.0.borrow_mut();
        w.held.remove(&track_id);
        w.desired.insert(track_id, (item.clone(), bytes.to_vec()));
        item
    }

    pub fn remove_track(&self, track_id: i64) {
        self.0.borrow_mut().desired.remove(&track_id);
    }

    pub fn hold(&self, track_id: i64, reason: &str) {
        let mut w = self.0.borrow_mut();
        w.desired.remove(&track_id);
        w.held.insert(track_id, reason.to_owned());
    }

    pub fn put_playlist(&self, playlist_id: i64, name: &str, tracks: &[i64]) {
        let token = sha256_hex(format!("pl:{playlist_id}:{name}:{tracks:?}").as_bytes());
        self.0.borrow_mut().playlists.insert(
            playlist_id,
            ManifestPlaylist {
                playlist_id,
                name: name.to_owned(),
                token,
                tracks: tracks.to_vec(),
            },
        );
    }

    pub fn remove_playlist(&self, playlist_id: i64) {
        self.0.borrow_mut().playlists.remove(&playlist_id);
    }

    pub fn set_pending_reevaluation(&self, v: bool) {
        self.0.borrow_mut().pending_reevaluation = v;
    }

    pub fn bump_generation(&self) {
        self.0.borrow_mut().generation += 1;
    }

    pub fn fail_fetch_changed(&self, track_id: i64, times: u32) {
        self.0.borrow_mut().fetch_changed.insert(track_id, times);
    }

    pub fn cut_fetch(&self, track_id: i64, times: u32) {
        self.0.borrow_mut().fetch_cut.insert(track_id, times);
    }

    pub fn fetch_log(&self) -> Vec<(i64, u64)> {
        self.0.borrow().fetch_log.clone()
    }

    pub fn reports(&self) -> Vec<ReportRequest> {
        self.0.borrow().reports.clone()
    }

    pub fn abandons(&self) -> Vec<AbandonRequest> {
        self.0.borrow().abandons.clone()
    }

    pub fn current(&self) -> Vec<ReportTrack> {
        self.0.borrow().current.clone()
    }

    pub fn open(&self) -> Option<Plan> {
        self.0.borrow().open.clone()
    }

    /// 現在の差分（本体の差分と同じ分類）
    pub fn diff(&self) -> DiffView {
        let w = self.0.borrow();
        let mut items = Vec::new();
        let cur: BTreeMap<i64, &ReportTrack> = w.current.iter().map(|c| (c.track_id, c)).collect();
        for (id, (d, _)) in &w.desired {
            let base = ItemOp {
                op: OpKind::Add,
                track_id: *id,
                from: None,
                to: Some(d.dest_path.clone()),
                token: Some(d.token.clone()),
                size: d.size,
                sha256: Some(d.sha256.clone()),
            };
            match cur.get(id) {
                None => items.push(base),
                Some(c) if c.token == d.token && c.dest_path == d.dest_path => {}
                Some(c) => {
                    let op = match (c.token == d.token, c.dest_path == d.dest_path) {
                        (false, true) => OpKind::Update,
                        (true, false) => OpKind::Move,
                        _ => OpKind::UpdateMove,
                    };
                    items.push(ItemOp {
                        op,
                        from: Some(c.dest_path.clone()),
                        ..base
                    });
                }
            }
        }
        for c in &w.current {
            if !w.desired.contains_key(&c.track_id) && !w.held.contains_key(&c.track_id) {
                items.push(ItemOp {
                    op: OpKind::Delete,
                    track_id: c.track_id,
                    from: Some(c.dest_path.clone()),
                    to: None,
                    token: None,
                    size: c.size,
                    sha256: None,
                });
            }
        }
        items.sort_by_key(|o| (o.op, o.track_id));
        let held = w
            .held
            .iter()
            .map(|(id, r)| Held {
                track_id: *id,
                reason: r.clone(),
                waiting: true,
                has_copy: cur.contains_key(id),
            })
            .collect();
        let curp: BTreeMap<i64, &ReportPlaylist> = w
            .current_playlists
            .iter()
            .map(|c| (c.playlist_id, c))
            .collect();
        let mut playlists = Vec::new();
        for (id, p) in &w.playlists {
            match curp.get(id) {
                None => playlists.push(PlaylistOp {
                    op: PlaylistOpKind::Add,
                    playlist_id: *id,
                    from: None,
                    to: Some(p.name.clone()),
                    token: Some(p.token.clone()),
                }),
                Some(c) if c.token == p.token && c.name == p.name => {}
                Some(c) => playlists.push(PlaylistOp {
                    op: PlaylistOpKind::Update,
                    playlist_id: *id,
                    from: Some(c.name.clone()),
                    to: Some(p.name.clone()),
                    token: Some(p.token.clone()),
                }),
            }
        }
        for c in &w.current_playlists {
            if !w.playlists.contains_key(&c.playlist_id) {
                playlists.push(PlaylistOp {
                    op: PlaylistOpKind::Delete,
                    playlist_id: c.playlist_id,
                    from: Some(c.name.clone()),
                    to: None,
                    token: None,
                });
            }
        }
        DiffView {
            items,
            held,
            playlists,
            playlist_errors: vec![],
        }
    }

    fn plan_token(&self, diff: &DiffView) -> String {
        let w = self.0.borrow();
        let body = format!("{}:{diff:?}", w.generation);
        sha256_hex(body.as_bytes())
    }

    /// 報告で current を置き換えたのと同じことを、試験の前提として直接行う
    pub fn set_current(&self, tracks: Vec<ReportTrack>, playlists: Vec<ReportPlaylist>) {
        let mut w = self.0.borrow_mut();
        w.current = tracks;
        w.current_playlists = playlists;
    }
}

impl Server for FakeServer {
    fn pair(&self, _code: &str) -> Result<PairResponse> {
        let w = self.0.borrow();
        Ok(PairResponse {
            device_uuid: w.device_uuid.clone(),
            device_name: "iPhone".to_owned(),
            token: "fake.token".to_owned(),
        })
    }

    fn manifest(&self) -> Result<ManifestResponse> {
        let diff = self.diff();
        let plan_token = self.plan_token(&diff);
        let w = self.0.borrow();
        Ok(ManifestResponse {
            device_uuid: w.device_uuid.clone(),
            device_name: "iPhone".to_owned(),
            generation: w.generation,
            plan_token,
            pending_reevaluation: w.pending_reevaluation,
            items: w.desired.values().map(|(i, _)| i.clone()).collect(),
            playlists: w.playlists.values().cloned().collect(),
            diff,
        })
    }

    fn fetch(&self, track_id: i64, token: &str, offset: u64, out: &mut dyn Write) -> Result<Fetch> {
        let mut w = self.0.borrow_mut();
        w.fetch_log.push((track_id, offset));
        if let Some(n) = w.fetch_changed.get_mut(&track_id) {
            if *n > 0 {
                *n -= 1;
                return Ok(Fetch::Changed);
            }
        }
        let Some((item, bytes)) = w.desired.get(&track_id).cloned() else {
            return Ok(Fetch::Gone);
        };
        if item.token != token {
            return Ok(Fetch::Changed);
        }
        let rest = bytes.get(offset as usize..).unwrap_or_default();
        if let Some(n) = w.fetch_cut.get_mut(&track_id) {
            if *n > 0 {
                *n -= 1;
                out.write_all(&rest[..rest.len() / 2])?;
                return Err(Error::Server("受信が切れた（偽）".to_owned()));
            }
        }
        out.write_all(rest)?;
        Ok(Fetch::Complete)
    }

    fn confirm(&self, plan_token: &str) -> Result<Confirmed> {
        if let Some(open) = self.open() {
            return Ok(if open.plan_token == plan_token {
                Confirmed::Plan(open)
            } else {
                Confirmed::OpenPlanExists
            });
        }
        if self.0.borrow().pending_reevaluation {
            return Ok(Confirmed::PendingReevaluation);
        }
        let diff = self.diff();
        let now = self.plan_token(&diff);
        if now != plan_token {
            return Ok(Confirmed::Changed { plan_token: now });
        }
        let mut w = self.0.borrow_mut();
        let mut next = || {
            w.next_op += 1;
            format!("{:032x}", w.next_op)
        };
        let items: Vec<PlanItem> = diff
            .items
            .iter()
            .map(|o| PlanItem {
                op_id: next(),
                op: o.op,
                track_id: o.track_id,
                from: o.from.clone(),
                to: o.to.clone(),
                token: o.token.clone(),
                size: o.size,
                sha256: o.sha256.clone(),
            })
            .collect();
        let playlists: Vec<PlanPlaylist> = diff
            .playlists
            .iter()
            .map(|p| PlanPlaylist {
                op_id: next(),
                op: p.op,
                playlist_id: p.playlist_id,
                from: p.from.clone(),
                to: p.to.clone(),
                token: p.token.clone(),
            })
            .collect();
        let plan = Plan {
            plan_id: w.next_plan,
            generation: w.generation,
            plan_token: now,
            items,
            playlists,
        };
        w.next_plan += 1;
        w.open = Some(plan.clone());
        Ok(Confirmed::Plan(plan))
    }

    fn open_plan(&self) -> Result<Option<Plan>> {
        Ok(self.open())
    }

    fn report(&self, r: &ReportRequest) -> Result<Reported> {
        let mut w = self.0.borrow_mut();
        w.reports.push(r.clone());
        if w.open.as_ref().map(|p| p.plan_id) != Some(r.plan_id) {
            return Ok(Reported::Closed);
        }
        if r.generation != w.generation {
            return Ok(Reported::GenerationMismatch);
        }
        w.current = r.state.tracks.clone();
        w.current_playlists = r.state.playlists.clone();
        w.open = None;
        Ok(Reported::Ok)
    }

    fn abandon(&self, plan_id: i64, a: &AbandonRequest) -> Result<Reported> {
        let mut w = self.0.borrow_mut();
        w.abandons.push(a.clone());
        if w.open.as_ref().map(|p| p.plan_id) != Some(plan_id) {
            return Ok(Reported::Closed);
        }
        w.current = a.report.state.tracks.clone();
        w.current_playlists = a.report.state.playlists.clone();
        w.open = None;
        Ok(Reported::Ok)
    }
}

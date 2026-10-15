//! 偽の spindle サーバ（試験用）。差分は「desired」と「最後に受け入れた報告（current）」から本体と同じ
//! 規則で作る: 追加 / 更新（同じパス）/ 更新 + 移動 / 移動 / 削除（desired にも hold にも無い current）。
//! hold は `held` に入れた track_id（既存の写しに触らない）。プレイリストは名前（= dest_path）で同じ規則

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Write;
use std::rc::Rc;

use agent_proto::*;
use sha2::{Digest, Sha256};

use crate::progress::ProgressSent;
use crate::server::{Confirmed, Fetch, Reported, Server, NOT_ADOPTABLE_PAIR_STOP};
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
    /// report をこの回数だけ通信エラーにする（受け付けない）
    report_fail: u32,
    /// pair が呼ばれた回数（ワンタイムコードを使った回数）
    pair_calls: u32,
    /// 直近の pair に渡された adopt_uuid
    last_pair_adopt_uuid: Option<String>,
    /// adopt_uuid 付きの pair を 409 not_adoptable 相当で断る
    adopt_refused: bool,
    adopt_calls: Vec<AdoptRequest>,
    /// adopt をこの回数だけ通信エラーにする
    adopt_fail: u32,
    /// progress に届いた要求（受け付けた・断った・失敗させたものすべて）
    progress_log: Vec<ProgressRequest>,
    /// 受け付けた（`Ok` を返した）progress の数
    progress_accepted: usize,
    /// progress を 404（古いサーバ）で断る
    progress_not_found: bool,
    /// progress をこの回数だけ通信エラーにする
    progress_fail: u32,
    /// トークンを持たない（CLI の pair が作るサーバ）。`use_token` まで、トークンの要る呼び出しを断る
    anonymous: bool,
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
            rel_path: dest_path.to_owned(),
            source: "master".to_owned(),
        };
        let mut w = self.0.borrow_mut();
        w.held.remove(&track_id);
        w.desired.insert(track_id, (item.clone(), bytes.to_vec()));
        item
    }

    /// `put_track` の後で、Library の `rel_path` と送る元の種類を指定する
    pub fn set_origin(&self, track_id: i64, rel_path: &str, source: &str) {
        if let Some((item, _)) = self.0.borrow_mut().desired.get_mut(&track_id) {
            item.rel_path = rel_path.to_owned();
            item.source = source.to_owned();
        }
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

    pub fn fail_report(&self, times: u32) {
        self.0.borrow_mut().report_fail = times;
    }

    pub fn fetch_log(&self) -> Vec<(i64, u64)> {
        self.0.borrow().fetch_log.clone()
    }

    /// pair（ワンタイムコードの消費）が呼ばれた回数
    pub fn pair_calls(&self) -> u32 {
        self.0.borrow().pair_calls
    }

    /// 直近の pair に渡された adopt_uuid
    pub fn last_pair_adopt_uuid(&self) -> Option<String> {
        self.0.borrow().last_pair_adopt_uuid.clone()
    }

    /// adopt_uuid 付きの pair を断る（409 `not_adoptable` 相当）
    pub fn set_adopt_refused(&self, v: bool) {
        self.0.borrow_mut().adopt_refused = v;
    }

    /// トークン無しのサーバにする（CLI の pair と同じ）。adopt は `use_token` の後でないと断る
    pub fn set_anonymous(&self) {
        self.0.borrow_mut().anonymous = true;
    }

    /// adopt をこの回数だけ通信エラーにする
    pub fn fail_adopt(&self, times: u32) {
        self.0.borrow_mut().adopt_fail = times;
    }

    /// adopt に届いた要求（成功・失敗とも記録する）
    pub fn adopt_calls(&self) -> Vec<AdoptRequest> {
        self.0.borrow().adopt_calls.clone()
    }

    /// progress に届いた要求（断った・失敗させたものも含む）
    pub fn progress_log(&self) -> Vec<ProgressRequest> {
        self.0.borrow().progress_log.clone()
    }

    /// 受け付けた progress の数
    pub fn progress_accepted(&self) -> usize {
        self.0.borrow().progress_accepted
    }

    /// progress を 404（ルートが無い古いサーバ）で断る
    pub fn set_progress_not_found(&self, v: bool) {
        self.0.borrow_mut().progress_not_found = v;
    }

    /// progress をこの回数だけ通信エラーにする
    pub fn fail_progress(&self, times: u32) {
        self.0.borrow_mut().progress_fail = times;
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
    fn pair(&self, _code: &str, adopt_uuid: Option<&str>) -> Result<PairResponse> {
        let mut w = self.0.borrow_mut();
        w.pair_calls += 1;
        w.last_pair_adopt_uuid = adopt_uuid.map(str::to_owned);
        if let Some(u) = adopt_uuid {
            if w.adopt_refused {
                return Err(Error::Stop(NOT_ADOPTABLE_PAIR_STOP.to_owned()));
            }
            w.device_uuid = u.to_owned();
        }
        Ok(PairResponse {
            device_uuid: w.device_uuid.clone(),
            device_name: "iPhone".to_owned(),
            token: "fake.token".to_owned(),
        })
    }

    /// 試験用の簡易規則: 曲は送られた `rel_path` と desired の `rel_path` の完全一致で付け替え、
    /// プレイリストは名前の完全一致。一致が無ければ `to` は `None`
    fn adopt(&self, r: &AdoptRequest) -> Result<AdoptResponse> {
        let mut w = self.0.borrow_mut();
        if w.anonymous {
            // 本物の HttpServer はトークンが無ければ送らずに止める
            return Err(Error::Stop("トークンがありません（偽）".to_owned()));
        }
        w.adopt_calls.push(r.clone());
        if w.adopt_fail > 0 {
            w.adopt_fail -= 1;
            return Err(Error::Server("引き継ぎの送信に失敗（偽）".to_owned()));
        }
        let tracks = r
            .tracks
            .iter()
            .map(|t| IdMap {
                from: t.track_id,
                to: w
                    .desired
                    .values()
                    .find(|(i, _)| i.rel_path == t.rel_path)
                    .map(|(i, _)| i.track_id),
            })
            .collect();
        let playlists = r
            .playlists
            .iter()
            .map(|p| IdMap {
                from: p.playlist_id,
                to: w
                    .playlists
                    .values()
                    .find(|q| q.name == p.name)
                    .map(|q| q.playlist_id),
            })
            .collect();
        Ok(AdoptResponse { tracks, playlists })
    }

    fn use_token(&self, _token: &str) -> Result<()> {
        self.0.borrow_mut().anonymous = false;
        Ok(())
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

    fn fetch(
        &self,
        track_id: i64,
        token: &str,
        _size: u64,
        offset: u64,
        out: &mut dyn Write,
    ) -> Result<Fetch> {
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
        let cut = match w.fetch_cut.get_mut(&track_id) {
            Some(n) if *n > 0 => {
                *n -= 1;
                true
            }
            _ => false,
        };
        // 書いている間に（進捗の送信などで）呼び戻されても借用が重ならないよう、書く前に放す
        drop(w);
        if cut {
            out.write_all(&rest[..rest.len() / 2])?;
            return Err(Error::Server("受信が切れた（偽）".to_owned()));
        }
        out.write_all(rest)?;
        Ok(Fetch::Complete)
    }

    fn confirm(&self, plan_token: &str) -> Result<Confirmed> {
        if self.0.borrow().pending_reevaluation {
            return Ok(Confirmed::PendingReevaluation);
        }
        if let Some(open) = self.open() {
            return Ok(if open.plan_token == plan_token {
                Confirmed::Plan(open)
            } else {
                Confirmed::OpenPlanExists
            });
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
        if w.report_fail > 0 {
            w.report_fail -= 1;
            return Err(Error::Server("報告の送信に失敗（偽）".to_owned()));
        }
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

    /// 本体と同じく、open な計画でなければ 404、generation が違えば 409（どちらも `Stop`）
    fn progress(&self, plan_id: i64, r: &ProgressRequest) -> Result<ProgressSent> {
        let mut w = self.0.borrow_mut();
        w.progress_log.push(r.clone());
        if w.progress_fail > 0 {
            w.progress_fail -= 1;
            return Err(Error::Server("進捗の送信に失敗（偽）".to_owned()));
        }
        if w.progress_not_found || w.open.as_ref().map(|p| p.plan_id) != Some(plan_id) {
            return Ok(ProgressSent::Stop);
        }
        if r.generation != w.generation {
            return Ok(ProgressSent::Stop);
        }
        w.progress_accepted += 1;
        Ok(ProgressSent::Ok)
    }
}

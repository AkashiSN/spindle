//! エンジンの試験の準備。tempdir の下に state のディレクトリ・root・偽のメディアフォルダを作り、
//! 偽のミュージック.app と偽のサーバと中断点を共有する
#![allow(dead_code)]

use std::path::PathBuf;

use agent_proto::{
    ManifestResponse, Plan, ReportPlaylist, ReportRequest, ReportState, ReportTrack,
};
use spindle_agent::ctx::{Ctx, Ui};
use spindle_agent::failpoint::Failpoints;
use spindle_agent::local::LocalRoot;
use spindle_agent::music::fake::FakeMusic;
use spindle_agent::plan::{current_of, runnable, Runnable};
use spindle_agent::server::fake::FakeServer;
use spindle_agent::server::{Confirmed, Reported, Server};
use spindle_agent::state::{ServerInfo, Setup, SetupPhase, State, StateFile, TrackEntry};

pub const UUID: &str = "11111111-2222-3333-4444-555555555555";

pub struct Env {
    pub dir: tempfile::TempDir,
    pub state_dir: PathBuf,
    pub root: LocalRoot,
    pub music: FakeMusic,
    pub server: FakeServer,
    pub fp: Failpoints,
    pub ui: ScriptUi,
}

/// 台本どおりに y/N を返し、表示を記録する
#[derive(Default)]
pub struct ScriptUi {
    pub answers: std::collections::VecDeque<bool>,
    pub log: Vec<String>,
    pub shown: Vec<ManifestResponse>,
}

impl Ui for ScriptUi {
    fn info(&mut self, msg: &str) {
        self.log.push(msg.to_owned());
    }
    fn confirm(&mut self, m: &ManifestResponse) -> bool {
        self.shown.push(m.clone());
        self.answers.pop_front().unwrap_or(true)
    }
}

impl Env {
    pub fn new() -> Self {
        Self::new_at(|base| base.join("Music/spindle"))
    }

    /// root を tempdir から `root_of` で決める（ディレクトリの用意も `root_of` で行える）
    pub fn new_at(root_of: impl FnOnce(&std::path::Path) -> PathBuf) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fp = Failpoints::armed();
        let state_dir = dir.path().join("state");
        let root = LocalRoot::new(root_of(dir.path()));
        let music = FakeMusic::new(dir.path().join("Media"), fp.clone());
        Self {
            state_dir,
            root,
            music,
            server: FakeServer::new(UUID),
            fp,
            ui: ScriptUi::default(),
            dir,
        }
    }

    pub fn file(&self) -> StateFile {
        StateFile::new(&self.state_dir)
    }

    pub fn state(&self) -> State {
        self.file().load().unwrap()
    }

    /// state を読んで Ctx を作り、`f` を走らせる（`Ctx` を作る試験用）
    pub fn with_ctx<T>(&mut self, f: impl FnOnce(&mut Ctx<'_, FakeMusic, FakeServer>) -> T) -> T {
        let file = self.file();
        let state = file.load().unwrap();
        let mut cx = Ctx {
            music: &self.music,
            server: &self.server,
            local: &self.root,
            file: &file,
            fp: &self.fp,
            ui: &mut self.ui,
            state,
            errors: Vec::new(),
            now: || 1_800_000_000,
        };
        f(&mut cx)
    }
}

impl Env {
    /// pair 済みの state（spindle フォルダ・root・marker を作る）を用意し、フォルダの persistent ID を返す
    pub fn paired(&self) -> String {
        let folder = self.music.insert_folder("spindle");
        let s = State {
            server: Some(ServerInfo {
                url: "https://music.example".into(),
                insecure_http: false,
                device_uuid: UUID.into(),
                device_name: "iPhone".into(),
            }),
            setup: Some(Setup {
                phase: SetupPhase::Done,
                nonce: "0".repeat(32),
                folder_pid: Some(folder.clone()),
            }),
            ..State::default()
        };
        self.root
            .write_marker(&spindle_agent::local::Marker {
                device_uuid: UUID.into(),
                nonce: "0".repeat(32),
            })
            .unwrap();
        self.file().save(&s).unwrap();
        folder
    }

    /// root にファイルを置く（root 相対）
    pub fn write_local(&self, rel: &str, bytes: &[u8]) {
        let p = self.root.abs(rel).unwrap();
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    /// エージェントが反映済みの曲を 1 つ作る（ファイル・track・state の行・偽サーバの current）
    pub fn synced_track(&self, track_id: i64, rel: &str, bytes: &[u8]) -> TrackEntry {
        let item = self.server.put_track(track_id, rel, bytes);
        self.write_local(rel, bytes);
        let pid = self.music.insert_track(&self.root.abs(rel).unwrap());
        let st = self.root.stat(rel).unwrap().unwrap();
        let e = TrackEntry {
            persistent_id: pid,
            token: item.token.clone(),
            path: rel.into(),
            size: st.size,
            sha256: item.sha256.clone(),
            inode: st.inode,
            mtime_ns: st.mtime_ns,
        };
        let mut s = self.state();
        s.tracks.insert(track_id, e.clone());
        self.file().save(&s).unwrap();
        let mut cur = self.server.current();
        cur.retain(|c| c.track_id != track_id);
        cur.push(agent_proto::ReportTrack {
            track_id,
            dest_path: rel.into(),
            token: item.token,
            size: st.size,
            sha256: item.sha256,
        });
        self.server.set_current(cur, vec![]);
        e
    }
}

impl Env {
    /// 今の差分で計画を確定し、runnable を作る
    pub fn confirm_all(&self) -> (Plan, Runnable, ManifestResponse) {
        let m = self.server.manifest().unwrap();
        let Confirmed::Plan(p) = self.server.confirm(&m.plan_token).unwrap() else {
            panic!("確定できない")
        };
        let (c, cp) = current_of(&self.state());
        let r = runnable(&p, &c, &cp, &m.diff);
        (p, r, m)
    }

    /// 偽のミュージック.app の track の (location の root 相対, persistent ID)
    pub fn music_paths(&self) -> Vec<(String, String)> {
        let mut v: Vec<_> = self
            .music
            .tracks()
            .into_iter()
            .filter_map(|t| {
                let rel = spindle_agent::pathkey::to_rel(self.root.path(), t.location.as_deref()?)?;
                Some((rel, t.persistent_id))
            })
            .collect();
        v.sort();
        v
    }
}

impl Env {
    /// 偽サーバの open な計画を今の state の報告で閉じる（以後の `confirm_all` は新しい差分で確定する）
    pub fn accept_state(&self) {
        let s = self.state();
        let m = self.server.manifest().unwrap();
        let plan_id = self.server.open().unwrap().plan_id;
        let r = ReportRequest {
            generation: m.generation,
            plan_id,
            state: ReportState {
                tracks: s
                    .tracks
                    .iter()
                    .map(|(id, e)| ReportTrack {
                        track_id: *id,
                        dest_path: e.path.clone(),
                        token: e.token.clone(),
                        size: e.size,
                        sha256: e.sha256.clone(),
                    })
                    .collect(),
                playlists: s
                    .playlists
                    .iter()
                    .map(|(id, e)| ReportPlaylist {
                        playlist_id: *id,
                        name: e.name.clone(),
                        token: e.token.clone(),
                    })
                    .collect(),
            },
            errors: vec![],
        };
        assert_eq!(self.server.report(&r).unwrap(), Reported::Ok);
    }
}

impl Env {
    pub fn paths(&self) -> spindle_agent::sync::Paths {
        spindle_agent::sync::Paths {
            state_dir: self.state_dir.clone(),
            root: self.root.path().to_path_buf(),
        }
    }

    /// `spindle-agent sync` と同じ段取りを偽のミュージック.app・偽のサーバで走らせる
    pub fn sync(&mut self) -> spindle_agent::Result<spindle_agent::sync::SyncOutcome> {
        let paths = self.paths();
        spindle_agent::sync::sync(&self.music, &self.server, &paths, &self.fp, &mut self.ui)
    }
}

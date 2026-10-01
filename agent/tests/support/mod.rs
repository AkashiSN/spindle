//! エンジンの試験の準備。tempdir の下に state のディレクトリ・root・偽のメディアフォルダを作り、
//! 偽のミュージック.app と偽のサーバと中断点を共有する
#![allow(dead_code)]

use std::path::PathBuf;

use agent_proto::ManifestResponse;
use spindle_agent::ctx::{Ctx, Ui};
use spindle_agent::failpoint::Failpoints;
use spindle_agent::local::LocalRoot;
use spindle_agent::music::fake::FakeMusic;
use spindle_agent::server::fake::FakeServer;
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
        let dir = tempfile::tempdir().unwrap();
        let fp = Failpoints::armed();
        let state_dir = dir.path().join("state");
        let root = LocalRoot::new(dir.path().join("Music/spindle"));
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

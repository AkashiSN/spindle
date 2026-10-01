//! 偽のミュージック.app（試験用）。track・フォルダ・プレイリストをメモリに持つ。
//! 変更する操作は副作用の前に `music.<op>`、後に `music.<op>:after` の中断点を通る。
//! コピー設定 ON（`set_copy_on(true)`）なら `add` はファイルを `media_dir` へ複製して、その場所の track を作る

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::failpoint::Failpoints;
use crate::music::{Music, MusicPlaylist, MusicTrack};
use crate::pathkey::canonical_key;
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeTrack {
    pub persistent_id: String,
    pub database_id: i64,
    pub location: Option<PathBuf>,
    pub date_added: i64,
    /// 再生回数（location の付け替え・refresh で保たれることを試験で確かめる）
    pub play_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakePlaylist {
    pub persistent_id: String,
    pub name: String,
    pub parent: Option<String>,
    pub is_folder: bool,
    pub tracks: Vec<String>,
}

struct World {
    next_db: i64,
    next_pid: u64,
    tracks: Vec<FakeTrack>,
    playlists: Vec<FakePlaylist>,
    copy_setting: Option<bool>,
    copy_on: bool,
    media_dir: PathBuf,
    calls: Vec<String>,
    /// 本物のように、場所をシンボリックリンクを解いたパスで持つ
    resolve_links: bool,
}

#[derive(Clone)]
pub struct FakeMusic {
    w: Rc<RefCell<World>>,
    fp: Failpoints,
}

impl FakeMusic {
    pub fn new(media_dir: PathBuf, fp: Failpoints) -> Self {
        Self {
            w: Rc::new(RefCell::new(World {
                next_db: 100,
                next_pid: 1,
                tracks: Vec::new(),
                playlists: Vec::new(),
                copy_setting: Some(false),
                copy_on: false,
                media_dir,
                calls: Vec::new(),
                resolve_links: false,
            })),
            fp,
        }
    }

    pub fn set_copy_setting(&self, v: Option<bool>) {
        self.w.borrow_mut().copy_setting = v;
    }

    pub fn set_copy_on(&self, v: bool) {
        self.w.borrow_mut().copy_on = v;
    }

    /// 本物のミュージック.app のように、`add` / `set_location` の場所をシンボリックリンクを解いて持つ
    pub fn set_resolve_symlinks(&self, v: bool) {
        self.w.borrow_mut().resolve_links = v;
    }

    /// `resolve_links` なら解いたパス（無ければそのまま）
    fn stored(w: &World, path: &Path) -> PathBuf {
        if w.resolve_links {
            if let Ok(p) = std::fs::canonicalize(path) {
                return p;
            }
        }
        path.to_path_buf()
    }

    fn new_pid(w: &mut World) -> String {
        let p = format!("{:016X}", 0xA000_0000_0000_0000u64 + w.next_pid);
        w.next_pid += 1;
        p
    }

    /// ユーザが手で足した track（コピー設定は見ない）
    pub fn insert_track(&self, location: &Path) -> String {
        let mut w = self.w.borrow_mut();
        let pid = Self::new_pid(&mut w);
        w.next_db += 1;
        let db = w.next_db;
        w.tracks.push(FakeTrack {
            persistent_id: pid.clone(),
            database_id: db,
            location: Some(location.to_path_buf()),
            date_added: 1_700_000_000 + db,
            play_count: 0,
        });
        pid
    }

    /// ユーザが手で消した
    pub fn remove_track(&self, pid: &str) {
        self.w
            .borrow_mut()
            .tracks
            .retain(|t| t.persistent_id != pid);
    }

    /// ユーザが手で場所を変えた（中断点を通らない）
    pub fn relocate(&self, pid: &str, location: &Path) {
        if let Some(t) = self
            .w
            .borrow_mut()
            .tracks
            .iter_mut()
            .find(|t| t.persistent_id == pid)
        {
            t.location = Some(location.to_path_buf());
        }
    }

    pub fn set_play_count(&self, pid: &str, n: u32) {
        if let Some(t) = self
            .w
            .borrow_mut()
            .tracks
            .iter_mut()
            .find(|t| t.persistent_id == pid)
        {
            t.play_count = n;
        }
    }

    pub fn tracks(&self) -> Vec<FakeTrack> {
        self.w.borrow().tracks.clone()
    }

    pub fn track_at(&self, location: &Path) -> Vec<FakeTrack> {
        self.w
            .borrow()
            .tracks
            .iter()
            .filter(|t| t.location.as_deref() == Some(location))
            .cloned()
            .collect()
    }

    pub fn insert_folder(&self, name: &str) -> String {
        let mut w = self.w.borrow_mut();
        let pid = Self::new_pid(&mut w);
        w.playlists.push(FakePlaylist {
            persistent_id: pid.clone(),
            name: name.to_owned(),
            parent: None,
            is_folder: true,
            tracks: Vec::new(),
        });
        pid
    }

    pub fn insert_playlist(&self, parent: Option<&str>, name: &str) -> String {
        let mut w = self.w.borrow_mut();
        let pid = Self::new_pid(&mut w);
        w.playlists.push(FakePlaylist {
            persistent_id: pid.clone(),
            name: name.to_owned(),
            parent: parent.map(str::to_owned),
            is_folder: false,
            tracks: Vec::new(),
        });
        pid
    }

    /// フォルダ・プレイリストの現在の名前（試験の補助）
    pub fn folder_name(&self, pid: &str) -> Option<String> {
        self.w
            .borrow()
            .playlists
            .iter()
            .find(|p| p.persistent_id == pid)
            .map(|p| p.name.clone())
    }

    pub fn playlists(&self) -> Vec<FakePlaylist> {
        self.w.borrow().playlists.clone()
    }

    /// 呼ばれた変更操作の記録（`add` など）
    pub fn calls(&self) -> Vec<String> {
        self.w.borrow().calls.clone()
    }

    fn mutate<T>(&self, op: &str, f: impl FnOnce(&mut World) -> Result<T>) -> Result<T> {
        self.fp.hit(&format!("music.{op}"))?;
        let v = {
            let mut w = self.w.borrow_mut();
            w.calls.push(op.to_owned());
            f(&mut w)?
        };
        self.fp.hit(&format!("music.{op}:after"))?;
        Ok(v)
    }

    fn to_music(t: &FakeTrack) -> MusicTrack {
        MusicTrack {
            persistent_id: t.persistent_id.clone(),
            database_id: t.database_id,
            location: t.location.clone(),
            date_added: t.date_added,
            size: t
                .location
                .as_deref()
                .and_then(|p| std::fs::metadata(p).ok())
                .map(|m| m.len()),
        }
    }
}

fn no_track(pid: &str) -> Error {
    Error::Music(format!("track が無い（{pid}）"))
}

fn no_playlist(pid: &str) -> Error {
    Error::Music(format!("プレイリストが無い（{pid}）"))
}

impl Music for FakeMusic {
    fn probe(&self) -> Result<()> {
        Ok(())
    }

    fn copy_to_library(&self) -> Result<Option<bool>> {
        Ok(self.w.borrow().copy_setting)
    }

    fn tracks_under(&self, root: &Path) -> Result<Vec<MusicTrack>> {
        Ok(self
            .w
            .borrow()
            .tracks
            .iter()
            .filter(|t| t.location.as_deref().is_some_and(|l| l.starts_with(root)))
            .map(Self::to_music)
            .collect())
    }

    fn track(&self, pid: &str) -> Result<Option<MusicTrack>> {
        Ok(self
            .w
            .borrow()
            .tracks
            .iter()
            .find(|t| t.persistent_id == pid)
            .map(Self::to_music))
    }

    fn tracks_added_after(&self, after: i64) -> Result<Vec<MusicTrack>> {
        Ok(self
            .w
            .borrow()
            .tracks
            .iter()
            .filter(|t| t.database_id > after)
            .map(Self::to_music)
            .collect())
    }

    fn max_database_id(&self) -> Result<i64> {
        Ok(self
            .w
            .borrow()
            .tracks
            .iter()
            .map(|t| t.database_id)
            .max()
            .unwrap_or(0))
    }

    fn add(&self, path: &Path) -> Result<MusicTrack> {
        if !path.is_file() {
            return Err(Error::Music(format!(
                "ファイルが無い（{}）",
                path.display()
            )));
        }
        self.mutate("add", |w| {
            let pid = Self::new_pid(w);
            w.next_db += 1;
            let db = w.next_db;
            let location = if w.copy_on {
                std::fs::create_dir_all(&w.media_dir)?;
                let dst = w.media_dir.join(format!("{pid}.m4a"));
                std::fs::copy(path, &dst)?;
                dst
            } else {
                Self::stored(w, path)
            };
            let t = FakeTrack {
                persistent_id: pid,
                database_id: db,
                location: Some(location),
                date_added: 1_700_000_000 + db,
                play_count: 0,
            };
            w.tracks.push(t.clone());
            Ok(Self::to_music(&t))
        })
    }

    fn delete_track(&self, pid: &str) -> Result<()> {
        self.mutate("delete_track", |w| {
            let n = w.tracks.len();
            w.tracks.retain(|t| t.persistent_id != pid);
            if w.tracks.len() == n {
                return Err(no_track(pid));
            }
            for p in &mut w.playlists {
                p.tracks.retain(|t| t != pid);
            }
            Ok(())
        })
    }

    fn set_location(&self, pid: &str, path: &Path) -> Result<()> {
        self.mutate("set_location", |w| {
            let location = Self::stored(w, path);
            let t = w
                .tracks
                .iter_mut()
                .find(|t| t.persistent_id == pid)
                .ok_or_else(|| no_track(pid))?;
            t.location = Some(location);
            Ok(())
        })
    }

    fn refresh(&self, pid: &str) -> Result<()> {
        self.mutate("refresh", |w| {
            w.tracks
                .iter()
                .find(|t| t.persistent_id == pid)
                .ok_or_else(|| no_track(pid))?;
            Ok(())
        })
    }

    fn folders_named(&self, name: &str) -> Result<Vec<MusicPlaylist>> {
        let key = canonical_key(name);
        Ok(self
            .w
            .borrow()
            .playlists
            .iter()
            .filter(|p| p.is_folder && p.parent.is_none() && canonical_key(&p.name) == key)
            .map(|p| MusicPlaylist {
                persistent_id: p.persistent_id.clone(),
                name: p.name.clone(),
            })
            .collect())
    }

    fn folder(&self, pid: &str) -> Result<Option<MusicPlaylist>> {
        Ok(self
            .w
            .borrow()
            .playlists
            .iter()
            .find(|p| p.is_folder && p.persistent_id == pid)
            .map(|p| MusicPlaylist {
                persistent_id: p.persistent_id.clone(),
                name: p.name.clone(),
            }))
    }

    fn create_folder(&self, name: &str) -> Result<MusicPlaylist> {
        self.mutate("create_folder", |w| {
            let pid = Self::new_pid(w);
            w.playlists.push(FakePlaylist {
                persistent_id: pid.clone(),
                name: name.to_owned(),
                parent: None,
                is_folder: true,
                tracks: Vec::new(),
            });
            Ok(MusicPlaylist {
                persistent_id: pid,
                name: name.to_owned(),
            })
        })
    }

    fn rename_playlist(&self, pid: &str, name: &str) -> Result<()> {
        self.mutate("rename_playlist", |w| {
            let p = w
                .playlists
                .iter_mut()
                .find(|p| p.persistent_id == pid)
                .ok_or_else(|| no_playlist(pid))?;
            p.name = name.to_owned();
            Ok(())
        })
    }

    fn playlists_in(&self, folder_id: &str) -> Result<Vec<MusicPlaylist>> {
        Ok(self
            .w
            .borrow()
            .playlists
            .iter()
            .filter(|p| !p.is_folder && p.parent.as_deref() == Some(folder_id))
            .map(|p| MusicPlaylist {
                persistent_id: p.persistent_id.clone(),
                name: p.name.clone(),
            })
            .collect())
    }

    fn create_playlist(&self, folder_id: &str, name: &str) -> Result<MusicPlaylist> {
        self.mutate("create_playlist", |w| {
            if !w
                .playlists
                .iter()
                .any(|p| p.is_folder && p.persistent_id == folder_id)
            {
                return Err(no_playlist(folder_id));
            }
            let pid = Self::new_pid(w);
            w.playlists.push(FakePlaylist {
                persistent_id: pid.clone(),
                name: name.to_owned(),
                parent: Some(folder_id.to_owned()),
                is_folder: false,
                tracks: Vec::new(),
            });
            Ok(MusicPlaylist {
                persistent_id: pid,
                name: name.to_owned(),
            })
        })
    }

    fn set_playlist_tracks(&self, pid: &str, tracks: &[String]) -> Result<()> {
        self.mutate("set_playlist_tracks", |w| {
            for t in tracks {
                if !w.tracks.iter().any(|x| &x.persistent_id == t) {
                    return Err(no_track(t));
                }
            }
            let p = w
                .playlists
                .iter_mut()
                .find(|p| p.persistent_id == pid)
                .ok_or_else(|| no_playlist(pid))?;
            p.tracks = tracks.to_vec();
            Ok(())
        })
    }

    fn delete_playlist(&self, pid: &str) -> Result<()> {
        self.mutate("delete_playlist", |w| {
            let n = w.playlists.len();
            w.playlists.retain(|p| p.persistent_id != pid);
            if w.playlists.len() == n {
                return Err(no_playlist(pid));
            }
            Ok(())
        })
    }
}

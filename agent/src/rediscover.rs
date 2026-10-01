//! 再発見（仕様 ⑥「再発見」、D-100 判断 2〜4）。反映の前に、ミュージック.app の file track のうち
//! `location` が root の下を指すものを列挙して state と突き合わせる。
//! - 同じファイルを指す track が複数ある → どれも採らずに止める
//! - state の曲の track が手で消された → state から外し、自分のファイルを片付ける（判断 4）
//! - 外で書き換えられたファイル → 古い写し（`STALE_TOKEN`）にして track は残す（判断 3）
//! - `add` の後、state を書く前に落ちた track → manifest の項目と中身が一致するときだけ取り込む（判断 2）

use std::collections::{HashMap, HashSet};

use agent_proto::{ManifestItem, ManifestResponse};

use crate::ctx::Ctx;
use crate::local::{Check, FileStat};
use crate::music::{Music, MusicTrack};
use crate::pathkey::{canonical_key, to_rel};
use crate::server::Server;
use crate::state::{State, TrackEntry, STALE_TOKEN};
use crate::{Error, Result};

/// 重複の停止の文言の先頭
pub const DUPLICATE_LOCATION: &str = "ミュージック.app に同じファイルを指す曲が複数あります";

/// state の pending_ops / pending_batches が持つ persistent ID（管理下）
pub fn pending_pids(s: &State) -> HashSet<String> {
    let mut out: HashSet<String> = s
        .pending_ops
        .iter()
        .filter_map(|o| o.persistent_id.clone())
        .collect();
    for b in &s.pending_batches {
        for m in &b.members {
            out.insert(m.persistent_id.clone());
        }
    }
    out
}

/// root の下を指す track（root 相対パス付き）
struct Found {
    rel: String,
    track: MusicTrack,
}

/// 変化の記録（`report` は報告が要る変化、`dirty` は保存が要る変化）
#[derive(Default)]
struct Changes {
    report: bool,
    dirty: bool,
}

impl Changes {
    fn reported(&mut self) {
        self.report = true;
        self.dirty = true;
    }
}

pub fn rediscover<M: Music, S: Server>(cx: &mut Ctx<'_, M, S>, m: &ManifestResponse) -> Result<()> {
    // 1. root の下の track を鍵ごとに集める。同じ鍵が 2 つ以上なら何も採らずに止める
    let mut by_key: HashMap<String, Found> = HashMap::new();
    for t in cx.music.tracks_under(cx.local.path())? {
        let Some(rel) = t
            .location
            .as_deref()
            .and_then(|l| to_rel(cx.local.path(), l))
        else {
            continue;
        };
        let key = canonical_key(&rel);
        if by_key.contains_key(&key) {
            return Err(Error::Stop(format!(
                "{DUPLICATE_LOCATION}: {rel}。片方を手で消してから sync し直してください"
            )));
        }
        by_key.insert(key, Found { rel, track: t });
    }

    // 2. spindle フォルダ
    let folder = match cx.state.setup.as_ref().and_then(|s| s.folder_pid.clone()) {
        Some(pid) => cx.music.folder(&pid)?,
        None => None,
    };
    let Some(folder) = folder else {
        return Err(Error::Stop(
            "ミュージック.app の「spindle」フォルダが見つかりません。手で消した場合は pair し直してください"
                .to_owned(),
        ));
    };

    let mut ch = Changes::default();
    let by_pid: HashMap<&str, &Found> = by_key
        .values()
        .map(|f| (f.track.persistent_id.as_str(), f))
        .collect();

    // 3. state の各曲
    let ids: Vec<i64> = cx.state.tracks.keys().copied().collect();
    for id in ids {
        let Some(e) = cx.state.tracks.get(&id).cloned() else {
            continue;
        };
        let key = canonical_key(&e.path);
        let location_key = match by_pid.get(e.persistent_id.as_str()) {
            Some(f) => Some(canonical_key(&f.rel)),
            None => match cx.music.track(&e.persistent_id)? {
                // root の外へ動かされた（または場所を失った）track
                Some(t) => Some(
                    t.location
                        .as_deref()
                        .and_then(|l| to_rel(cx.local.path(), l))
                        .map(|r| canonical_key(&r))
                        .unwrap_or_default(),
                ),
                None => None,
            },
        };
        let Some(location_key) = location_key else {
            // 手で消された。同じ鍵の track が他に無ければ自分のファイルを片付ける（判断 4）
            cx.state.tracks.remove(&id);
            if !by_key.contains_key(&key) {
                cx.local.remove(&e.path)?;
            }
            ch.reported();
            continue;
        };
        let recorded = FileStat {
            size: e.size,
            inode: e.inode,
            mtime_ns: e.mtime_ns,
        };
        let check = cx.local.check(&e.path, &recorded)?;
        if location_key != key {
            // 場所を変えられた。ファイルが記録どおりで、その場所を他の track が指していなければ戻す
            let intact = match &check {
                Check::Same => Some(None),
                Check::Changed(st, sha) if *sha == e.sha256 && st.size == e.size => Some(Some(*st)),
                _ => None,
            };
            match intact {
                Some(st) if !by_key.contains_key(&key) => {
                    cx.music
                        .set_location(&e.persistent_id, &cx.local.abs(&e.path)?)?;
                    if let Some(st) = st {
                        set_stat(&mut cx.state, id, &st, &mut ch);
                    }
                }
                _ => {
                    // track もファイルも触らず、管理から外す
                    cx.state.tracks.remove(&id);
                    ch.reported();
                }
            }
            continue;
        }
        match check {
            Check::Same => {}
            Check::Changed(st, sha) if sha == e.sha256 && st.size == e.size => {
                set_stat(&mut cx.state, id, &st, &mut ch);
            }
            Check::Changed(st, sha) => {
                let stale = TrackEntry {
                    token: STALE_TOKEN.to_owned(),
                    size: st.size,
                    sha256: sha,
                    inode: st.inode,
                    mtime_ns: st.mtime_ns,
                    ..e.clone()
                };
                replace(&mut cx.state, id, &e, stale, &mut ch);
            }
            Check::Missing => {
                // 中身を失った。track は残し、次の「更新」で置き直す
                let stale = TrackEntry {
                    token: STALE_TOKEN.to_owned(),
                    size: 0,
                    sha256: String::new(),
                    inode: 0,
                    mtime_ns: 0,
                    ..e.clone()
                };
                replace(&mut cx.state, id, &e, stale, &mut ch);
            }
        }
    }

    // 4. 取り込み（add の後、state を書く前に落ちた track）
    let items: HashMap<String, &ManifestItem> = m
        .items
        .iter()
        .map(|i| (canonical_key(&i.dest_path), i))
        .collect();
    let mut managed = pending_pids(&cx.state);
    managed.extend(cx.state.tracks.values().map(|e| e.persistent_id.clone()));
    let mut used: HashSet<String> = cx
        .state
        .tracks
        .values()
        .map(|e| canonical_key(&e.path))
        .collect();
    let mut keys: Vec<&String> = by_key.keys().collect();
    keys.sort();
    for key in keys {
        let Some(f) = by_key.get(key) else {
            continue;
        };
        if managed.contains(&f.track.persistent_id) || used.contains(key) {
            continue;
        }
        let Some(item) = items.get(key) else {
            continue;
        };
        if cx.state.tracks.contains_key(&item.track_id) {
            continue;
        }
        let Some(st) = cx.local.stat(&item.dest_path)? else {
            continue;
        };
        if st.size != item.size {
            continue;
        }
        if cx.local.sha256(&item.dest_path)?.as_deref() != Some(item.sha256.as_str()) {
            continue;
        }
        cx.state.tracks.insert(
            item.track_id,
            TrackEntry {
                persistent_id: f.track.persistent_id.clone(),
                token: item.token.clone(),
                path: item.dest_path.clone(),
                size: st.size,
                sha256: item.sha256.clone(),
                inode: st.inode,
                mtime_ns: st.mtime_ns,
            },
        );
        used.insert(key.clone());
        ch.reported();
    }

    // 5. プレイリスト: spindle フォルダから消えたものを外す
    let present: HashSet<String> = cx
        .music
        .playlists_in(&folder.persistent_id)?
        .into_iter()
        .map(|p| p.persistent_id)
        .collect();
    let before = cx.state.playlists.len();
    cx.state
        .playlists
        .retain(|_, p| present.contains(&p.persistent_id));
    if cx.state.playlists.len() != before {
        ch.reported();
    }

    // 6. 保存
    if ch.report {
        cx.state.needs_report = true;
    }
    if ch.dirty {
        cx.save()?;
    }
    Ok(())
}

/// stat のキャッシュ（inode・mtime）だけ更新する（報告は要らない）
fn set_stat(s: &mut State, id: i64, st: &FileStat, ch: &mut Changes) {
    if let Some(e) = s.tracks.get_mut(&id) {
        if e.inode != st.inode || e.mtime_ns != st.mtime_ns {
            e.inode = st.inode;
            e.mtime_ns = st.mtime_ns;
            ch.dirty = true;
        }
    }
}

/// 古い写しに置き換える（変わらなければ何もしない）
fn replace(s: &mut State, id: i64, old: &TrackEntry, new: TrackEntry, ch: &mut Changes) {
    if *old != new {
        s.tracks.insert(id, new);
        ch.reported();
    }
}

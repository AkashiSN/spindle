// spindle-agent がミュージック.app を操作する JXA。要求は argv[0] の JSON、応答は 1 行の JSON。
// パス・名前は必ず JSON の値として受け取り、スクリプトへ埋め込まない
function run(argv) {
  try {
    return JSON.stringify({ ok: dispatch(JSON.parse(argv[0])) });
  } catch (e) {
    return JSON.stringify({ err: String(e) });
  }
}

function dispatch(q) {
  const M = Application('Music');
  const lib = M.libraryPlaylists[0];
  const trackOf = (pid) => {
    const xs = lib.fileTracks.whose({ persistentID: pid })();
    return xs.length ? xs[0] : null;
  };
  const mustTrack = (pid) => {
    const t = trackOf(pid);
    if (!t) throw new Error('track が無い（' + pid + '）');
    return t;
  };
  const playlistOf = (pid) => {
    const xs = M.playlists.whose({ persistentID: pid })();
    if (!xs.length) throw new Error('プレイリストが無い（' + pid + '）');
    return xs[0];
  };
  // 追加日時（epoch 秒）。念のため null は 0 にする
  const epoch = (d) => (d ? Math.floor(d.getTime() / 1000) : 0);
  const info = (t) => {
    const l = t.location();
    return {
      pid: t.persistentID(),
      db: t.databaseID(),
      loc: l ? l.toString() : null,
      added: epoch(t.dateAdded()),
      size: t.size(),
    };
  };
  const kind = (p) => {
    const c = p.class();
    return c === 'folderPlaylist' ? 'folder' : c === 'userPlaylist' ? 'user' : 'other';
  };
  const parentOf = (p) => {
    try { return p.parent().persistentID(); } catch (e) { return null; }
  };
  switch (q.op) {
    case 'probe': return M.version();
    case 'tracks': {
      const ts = lib.fileTracks;
      const pids = ts.persistentID(), dbs = ts.databaseID(), locs = ts.location();
      const added = ts.dateAdded(), sizes = ts.size();
      return pids.map((pid, i) => ({
        pid, db: dbs[i], loc: locs[i] ? locs[i].toString() : null,
        added: epoch(added[i]), size: sizes[i],
      }));
    }
    case 'track': { const t = trackOf(q.pid); return t ? info(t) : null; }
    case 'add': return info(M.add(q.path));
    case 'delete_track': M.delete(mustTrack(q.pid)); return null;
    case 'refresh': M.refresh(mustTrack(q.pid)); return null;
    case 'set_location': mustTrack(q.pid).location = q.path; return null;
    case 'playlists':
      return M.playlists().map((p) => ({ pid: p.persistentID(), name: p.name(), kind: kind(p), parent: parentOf(p) }));
    case 'create_folder': {
      const f = M.make({ new: 'folderPlaylist', withProperties: { name: q.name } });
      return { pid: f.persistentID(), name: f.name() };
    }
    case 'create_playlist': {
      const p = M.make({ new: 'userPlaylist', at: playlistOf(q.folder), withProperties: { name: q.name } });
      return { pid: p.persistentID(), name: p.name() };
    }
    case 'rename_playlist': playlistOf(q.pid).name = q.name; return null;
    case 'set_playlist_tracks': {
      const p = playlistOf(q.pid);
      M.delete(p.tracks);
      for (const id of q.tracks) M.duplicate(mustTrack(id), { to: p });
      return null;
    }
    case 'delete_playlist': M.delete(playlistOf(q.pid)); return null;
  }
  throw new Error('知らない op（' + q.op + '）');
}

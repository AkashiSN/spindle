//! ミュージック.app の JXA 実装（D-101）。`music.js` を埋め込み、`osascript -l JavaScript -` に標準入力で渡す。
//! 要求は JSON 1 つ（argv）、応答は `{"ok": 値}` か `{"err": 文字列}`。trait のメソッド 1 回 = osascript 1 回。
//! macOS 以外でも組み立てられる（試験は偽の実行器・偽の実行ファイルで走らせる）

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use super::{Music, MusicPlaylist, MusicTrack};
use crate::pathkey::canonical_key;
use crate::{Error, Result};

/// ミュージック.app を操作する JXA（バイナリに埋め込む。実行時にファイルを読まない）
pub const SCRIPT: &str = include_str!("music.js");
/// osascript 1 回の上限
pub const TIMEOUT: Duration = Duration::from_secs(120);

/// `set_playlist_tracks` で 1 曲ごとに足す時間。実機で 1 曲あたり約 35 ms（200 曲で 7 秒）かかったので余裕を見る
pub const PER_PLAYLIST_TRACK: Duration = Duration::from_millis(100);

/// 終了を確かめる間隔
const POLL: Duration = Duration::from_millis(50);

/// JXA の実行器。`script` を標準入力、`request` を引数で渡し、標準出力を返す。
/// 終了コード 0 以外・タイムアウトは `Error::Music`
pub trait Osascript {
    fn run(&self, script: &str, request: &str, timeout: Duration) -> Result<String>;
}

/// 本物の `osascript` を起動する実行器
pub struct ProcessOsascript {
    program: PathBuf,
}

impl Default for ProcessOsascript {
    fn default() -> Self {
        Self {
            program: PathBuf::from("osascript"),
        }
    }
}

impl ProcessOsascript {
    pub fn new() -> Self {
        Self::default()
    }

    /// 試験用: `osascript` の代わりに起動する実行ファイルを差し替える
    #[doc(hidden)]
    pub fn with_program(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

/// パイプを別スレッドで読み切る（子の出力でパイプが詰まらないように）
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            // 読み取りの失敗はそこまでの内容で扱う（終了コードの検査で失敗は拾う）
            let _ = p.read_to_end(&mut buf);
        }
        buf
    })
}

fn join_output(h: JoinHandle<Vec<u8>>) -> String {
    let buf = h.join().unwrap_or_default();
    String::from_utf8_lossy(&buf).into_owned()
}

/// タイムアウトまで終了を待つ。超えたら None
fn wait_until(child: &mut Child, timeout: Duration) -> Result<Option<ExitStatus>> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if start.elapsed() >= timeout {
            return Ok(None);
        }
        thread::sleep(POLL);
    }
}

impl Osascript for ProcessOsascript {
    fn run(&self, script: &str, request: &str, timeout: Duration) -> Result<String> {
        let mut child = Command::new(&self.program)
            .args(["-l", "JavaScript", "-", request])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                Error::Music(format!(
                    "osascript を起動できない（{}）: {e}",
                    self.program.display()
                ))
            })?;
        // 標準入力も別スレッドで書く（子が読まないまま止まってもこちらが詰まらないように）
        let stdin = child.stdin.take();
        let body = script.to_owned();
        let writer = thread::spawn(move || {
            if let Some(mut w) = stdin {
                // 書き込みの失敗（子が先に終わった等）は終了コードの検査に任せる
                let _ = w.write_all(body.as_bytes());
            }
        });
        let out = drain(child.stdout.take());
        let err = drain(child.stderr.take());

        let status = match wait_until(&mut child, timeout) {
            Ok(Some(status)) => status,
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                // 孫プロセスがパイプを握ったままだと読み取りが終わらないので、スレッドは待たずに手放す
                return Err(Error::Music(format!(
                    "ミュージック.app が応答しない（{} 秒）",
                    timeout.as_secs()
                )));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };
        let _ = writer.join();
        let stdout = join_output(out);
        let stderr = join_output(err);
        if !status.success() {
            let code = status
                .code()
                .map_or_else(|| status.to_string(), |c| c.to_string());
            return Err(Error::Music(format!(
                "osascript が失敗（{code}）: {}",
                stderr.trim_end()
            )));
        }
        Ok(stdout.trim_end_matches(['\n', '\r']).to_owned())
    }
}

/// `tracks` / `track` / `add` の応答の 1 件
#[derive(Deserialize)]
struct TrackInfo {
    pid: String,
    db: i64,
    loc: Option<String>,
    added: i64,
    size: Option<u64>,
}

impl From<TrackInfo> for MusicTrack {
    fn from(t: TrackInfo) -> Self {
        MusicTrack {
            persistent_id: t.pid,
            database_id: t.db,
            location: t.loc.map(PathBuf::from),
            date_added: t.added,
            size: t.size,
        }
    }
}

/// `playlists` の応答の 1 件
#[derive(Deserialize)]
struct PlaylistInfo {
    pid: String,
    name: String,
    kind: String,
    parent: Option<String>,
}

impl From<PlaylistInfo> for MusicPlaylist {
    fn from(p: PlaylistInfo) -> Self {
        MusicPlaylist {
            persistent_id: p.pid,
            name: p.name,
        }
    }
}

/// `create_folder` / `create_playlist` の応答
#[derive(Deserialize)]
struct Created {
    pid: String,
    name: String,
}

/// JXA でミュージック.app を操作する `Music`
pub struct JxaMusic<R: Osascript> {
    runner: R,
}

fn utf8(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| Error::Music(format!("パスが UTF-8 でない（{}）", path.display())))
}

fn decode<T: serde::de::DeserializeOwned>(v: Value) -> Result<T> {
    serde_json::from_value(v).map_err(|e| Error::Music(format!("JXA の応答の形が違う: {e}")))
}

impl<R: Osascript> JxaMusic<R> {
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    /// 要求を投げて ok の値を返す。err は Error::Music
    fn call(&self, request: Value) -> Result<Value> {
        self.call_with(request, TIMEOUT)
    }

    /// `call` の上限を指定する版
    fn call_with(&self, request: Value, timeout: Duration) -> Result<Value> {
        let out = self.runner.run(SCRIPT, &request.to_string(), timeout)?;
        let reply: Value = serde_json::from_str(&out)
            .map_err(|e| Error::Music(format!("JXA の応答を読めない（{e}）: {out}")))?;
        if let Some(err) = reply.get("err") {
            let msg = err.as_str().map_or_else(|| err.to_string(), str::to_owned);
            return Err(Error::Music(msg));
        }
        match reply {
            Value::Object(mut m) => m
                .remove("ok")
                .ok_or_else(|| Error::Music(format!("JXA の応答に ok が無い: {out}"))),
            _ => Err(Error::Music(format!("JXA の応答の形が違う: {out}"))),
        }
    }

    /// 応答が null であること（値を返さない op）
    fn call_unit(&self, request: Value) -> Result<()> {
        self.call(request).map(|_| ())
    }

    fn all_tracks(&self) -> Result<Vec<MusicTrack>> {
        let ts: Vec<TrackInfo> = decode(self.call(json!({"op": "tracks"}))?)?;
        Ok(ts.into_iter().map(MusicTrack::from).collect())
    }

    fn all_playlists(&self) -> Result<Vec<PlaylistInfo>> {
        decode(self.call(json!({"op": "playlists"}))?)
    }
}

impl<R: Osascript> Music for JxaMusic<R> {
    fn probe(&self) -> Result<()> {
        self.call_unit(json!({"op": "probe"}))
    }

    fn copy_to_library(&self) -> Result<Option<bool>> {
        // スクリプトからは読めない（spike 6）
        Ok(None)
    }

    fn tracks_under(&self, root: &Path) -> Result<Vec<MusicTrack>> {
        Ok(self
            .all_tracks()?
            .into_iter()
            .filter(|t| t.location.as_deref().is_some_and(|l| l.starts_with(root)))
            .collect())
    }

    fn track(&self, persistent_id: &str) -> Result<Option<MusicTrack>> {
        let t: Option<TrackInfo> =
            decode(self.call(json!({"op": "track", "pid": persistent_id}))?)?;
        Ok(t.map(MusicTrack::from))
    }

    fn tracks_added_after(&self, after: i64) -> Result<Vec<MusicTrack>> {
        Ok(self
            .all_tracks()?
            .into_iter()
            .filter(|t| t.database_id > after)
            .collect())
    }

    fn max_database_id(&self) -> Result<i64> {
        decode(self.call(json!({"op": "max_db"}))?)
    }

    fn add(&self, path: &Path) -> Result<MusicTrack> {
        let t: TrackInfo = decode(self.call(json!({"op": "add", "path": utf8(path)?}))?)?;
        Ok(t.into())
    }

    fn delete_track(&self, persistent_id: &str) -> Result<()> {
        self.call_unit(json!({"op": "delete_track", "pid": persistent_id}))
    }

    fn set_location(&self, persistent_id: &str, path: &Path) -> Result<()> {
        self.call_unit(json!({"op": "set_location", "pid": persistent_id, "path": utf8(path)?}))
    }

    fn refresh(&self, persistent_id: &str) -> Result<()> {
        self.call_unit(json!({"op": "refresh", "pid": persistent_id}))
    }

    fn folders_named(&self, name: &str) -> Result<Vec<MusicPlaylist>> {
        let key = canonical_key(name);
        Ok(self
            .all_playlists()?
            .into_iter()
            .filter(|p| p.kind == "folder" && p.parent.is_none() && canonical_key(&p.name) == key)
            .map(MusicPlaylist::from)
            .collect())
    }

    fn folder(&self, persistent_id: &str) -> Result<Option<MusicPlaylist>> {
        Ok(self
            .all_playlists()?
            .into_iter()
            .find(|p| p.kind == "folder" && p.pid == persistent_id)
            .map(MusicPlaylist::from))
    }

    fn create_folder(&self, name: &str) -> Result<MusicPlaylist> {
        let c: Created = decode(self.call(json!({"op": "create_folder", "name": name}))?)?;
        Ok(MusicPlaylist {
            persistent_id: c.pid,
            name: c.name,
        })
    }

    fn rename_playlist(&self, persistent_id: &str, name: &str) -> Result<()> {
        self.call_unit(json!({"op": "rename_playlist", "pid": persistent_id, "name": name}))
    }

    fn playlists_in(&self, folder_id: &str) -> Result<Vec<MusicPlaylist>> {
        Ok(self
            .all_playlists()?
            .into_iter()
            .filter(|p| p.kind == "user" && p.parent.as_deref() == Some(folder_id))
            .map(MusicPlaylist::from)
            .collect())
    }

    fn create_playlist(&self, folder_id: &str, name: &str) -> Result<MusicPlaylist> {
        let c: Created = decode(
            self.call(json!({"op": "create_playlist", "folder": folder_id, "name": name}))?,
        )?;
        Ok(MusicPlaylist {
            persistent_id: c.pid,
            name: c.name,
        })
    }

    fn set_playlist_tracks(&self, persistent_id: &str, tracks: &[String]) -> Result<()> {
        // 1 曲ずつ複製するので、曲数に比例して上限を延ばす（固定の上限だと大きいプレイリストが毎回失敗する）
        let n = u32::try_from(tracks.len()).unwrap_or(u32::MAX);
        let timeout = TIMEOUT.saturating_add(PER_PLAYLIST_TRACK.saturating_mul(n));
        self.call_with(
            json!({"op": "set_playlist_tracks", "pid": persistent_id, "tracks": tracks}),
            timeout,
        )
        .map(|_| ())
    }

    fn delete_playlist(&self, persistent_id: &str) -> Result<()> {
        self.call_unit(json!({"op": "delete_playlist", "pid": persistent_id}))
    }
}

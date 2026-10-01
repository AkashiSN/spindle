//! JXA の `Music` 実装の試験。osascript は偽の実行器・偽の実行ファイルで置き換え、Linux でも走る

use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{json, Value};
use spindle_agent::music::jxa::{JxaMusic, Osascript, ProcessOsascript, SCRIPT};
use spindle_agent::music::Music;
use spindle_agent::{Error, Result};

#[derive(Default)]
struct Scripted {
    replies: RefCell<Vec<Result<String>>>,
    seen: RefCell<Vec<Value>>,
}

impl Osascript for &Scripted {
    fn run(&self, script: &str, request: &str, _t: Duration) -> Result<String> {
        assert_eq!(script, SCRIPT);
        self.seen
            .borrow_mut()
            .push(serde_json::from_str(request).unwrap());
        self.replies.borrow_mut().remove(0)
    }
}

fn ok(v: Value) -> Result<String> {
    Ok(json!({ "ok": v }).to_string())
}

#[test]
fn request_escapes_paths_and_names() {
    let s = Scripted::default();
    s.replies.borrow_mut().push(ok(
        json!({"pid":"AB","db":1,"loc":"/r/it's \"x\"\\y\n.m4a","added":5,"size":3}),
    ));
    let m = JxaMusic::new(&s);
    let t = m.add(Path::new("/r/it's \"x\"\\y\n.m4a")).unwrap();
    assert_eq!(t.persistent_id, "AB");
    assert_eq!(
        s.seen.borrow()[0],
        json!({"op":"add","path":"/r/it's \"x\"\\y\n.m4a"})
    );
}

#[test]
fn tracks_under_filters_by_root_and_keeps_fields() {
    let s = Scripted::default();
    s.replies.borrow_mut().push(ok(json!([
        {"pid":"A","db":10,"loc":"/Users/me/Music/spindle/a.m4a","added":100,"size":7},
        {"pid":"B","db":11,"loc":"/Users/me/Music/other/b.m4a","added":101,"size":8},
        {"pid":"C","db":12,"loc":null,"added":102,"size":9}
    ])));
    let m = JxaMusic::new(&s);
    let ts = m
        .tracks_under(Path::new("/Users/me/Music/spindle"))
        .unwrap();
    assert_eq!(ts.len(), 1);
    assert_eq!(ts[0].persistent_id, "A");
    assert_eq!(ts[0].database_id, 10);
    assert_eq!(ts[0].date_added, 100);
    assert_eq!(ts[0].size, Some(7));
    assert_eq!(
        ts[0].location,
        Some(PathBuf::from("/Users/me/Music/spindle/a.m4a"))
    );
}

#[test]
fn added_after_and_max_database_id() {
    let s = Scripted::default();
    let all = json!([
        {"pid":"A","db":10,"loc":"/x/a","added":1,"size":1},
        {"pid":"B","db":30,"loc":null,"added":2,"size":1}
    ]);
    s.replies.borrow_mut().push(ok(all.clone()));
    s.replies.borrow_mut().push(ok(all));
    s.replies.borrow_mut().push(ok(json!([])));
    let m = JxaMusic::new(&s);
    assert_eq!(m.tracks_added_after(10).unwrap().len(), 1);
    assert_eq!(m.max_database_id().unwrap(), 30);
    assert_eq!(m.max_database_id().unwrap(), 0);
}

#[test]
fn missing_track_maps_to_none_or_error() {
    let s = Scripted::default();
    s.replies.borrow_mut().push(ok(Value::Null));
    s.replies
        .borrow_mut()
        .push(Ok(json!({"err":"Error: track が無い（ZZ）"}).to_string()));
    let m = JxaMusic::new(&s);
    assert_eq!(m.track("ZZ").unwrap(), None);
    assert!(matches!(m.refresh("ZZ"), Err(Error::Music(msg)) if msg.contains("track が無い")));
}

#[test]
fn copy_setting_is_unknown_without_calling_osascript() {
    let s = Scripted::default();
    let m = JxaMusic::new(&s);
    assert_eq!(m.copy_to_library().unwrap(), None);
    assert!(s.seen.borrow().is_empty());
}

#[test]
fn folders_and_playlists_are_filtered_in_rust() {
    let s = Scripted::default();
    let pls = json!([
        {"pid":"L","name":"ライブラリ","kind":"other","parent":null},
        {"pid":"F","name":"Spindle","kind":"folder","parent":null},
        {"pid":"G","name":"spindle","kind":"folder","parent":"F"},
        {"pid":"P","name":"Favs","kind":"user","parent":"F"},
        {"pid":"Q","name":"Other","kind":"user","parent":null}
    ]);
    for _ in 0..3 {
        s.replies.borrow_mut().push(ok(pls.clone()));
    }
    let m = JxaMusic::new(&s);
    let f = m.folders_named("spindle").unwrap();
    assert_eq!(
        f.iter()
            .map(|x| x.persistent_id.as_str())
            .collect::<Vec<_>>(),
        vec!["F"]
    );
    assert_eq!(m.folder("F").unwrap().unwrap().name, "Spindle");
    let inside = m.playlists_in("F").unwrap();
    assert_eq!(
        inside
            .iter()
            .map(|x| x.persistent_id.as_str())
            .collect::<Vec<_>>(),
        vec!["P"]
    );
}

#[test]
fn set_playlist_tracks_sends_order() {
    let s = Scripted::default();
    s.replies.borrow_mut().push(ok(Value::Null));
    let m = JxaMusic::new(&s);
    m.set_playlist_tracks("P", &["B".into(), "A".into()])
        .unwrap();
    assert_eq!(
        s.seen.borrow()[0],
        json!({"op":"set_playlist_tracks","pid":"P","tracks":["B","A"]})
    );
}

#[test]
fn runner_error_propagates() {
    let s = Scripted::default();
    s.replies.borrow_mut().push(Err(Error::Music(
        "osascript が失敗（1）: execution error".into(),
    )));
    let m = JxaMusic::new(&s);
    assert!(matches!(m.probe(), Err(Error::Music(_))));
}

/// 偽のプロセスを起動する試験を直列にする。macOS の `pipe()` は CLOEXEC の付与が原子的でなく、
/// 並行して起動した別の試験の子（`sleep 30` など）がこちらのパイプの書き端を継いで読み取りが終わらないことがある
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 偽の osascript（シェルスクリプト）を置き、実行ビットを付ける
fn fake_program(dir: &Path, body: &str) -> PathBuf {
    let prog = dir.join("fake-osascript");
    std::fs::write(&prog, body).unwrap();
    std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();
    prog
}

#[test]
fn timeout_kills_and_errors() {
    let _serial = serial();
    let d = tempfile::tempdir().unwrap();
    let prog = fake_program(d.path(), "#!/bin/sh\ncat >/dev/null\nexec sleep 30\n");
    let r = ProcessOsascript::with_program(prog);
    let t0 = std::time::Instant::now();
    let res = r.run("x", "{}", Duration::from_millis(300));
    assert!(matches!(res, Err(Error::Music(m)) if m.contains("応答しない")));
    assert!(t0.elapsed() < Duration::from_secs(5));
}

#[test]
fn nonzero_exit_includes_stderr() {
    let _serial = serial();
    let d = tempfile::tempdir().unwrap();
    let prog = fake_program(
        d.path(),
        "#!/bin/sh\ncat >/dev/null\necho 'execution error: boom' >&2\nexit 1\n",
    );
    let r = ProcessOsascript::with_program(prog);
    let res = r.run("x", "{}", Duration::from_secs(5));
    assert!(matches!(res, Err(Error::Music(m)) if m.contains("boom")));
}

#[test]
fn stdout_and_stdin_are_wired() {
    let _serial = serial();
    let d = tempfile::tempdir().unwrap();
    // 引数の最後（要求）と標準入力（スクリプト）の長さを返す。要求の `"` は JSON の文字列用に `\"` へ直す
    let prog = fake_program(
        d.path(),
        "#!/bin/sh\nfor a; do last=$a; done\nn=$(wc -c)\nesc=$(printf '%s' \"$last\" | sed 's/\"/\\\\\"/g')\nprintf '{\"ok\":[\"%s\",%s]}\\n' \"$esc\" $n\n",
    );
    let r = ProcessOsascript::with_program(prog);
    let out = r
        .run("abcd", "{\"op\":\"probe\"}", Duration::from_secs(5))
        .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"][0], "{\"op\":\"probe\"}");
    assert_eq!(v["ok"][1], 4);
}

#[test]
fn add_refused_maps_to_music_error() {
    let s = Scripted::default();
    s.replies.borrow_mut().push(Ok(json!({
        "err": "Error: ミュージック.app が追加しなかった（音声ファイルでない？）: /r/x.m4a"
    })
    .to_string()));
    let m = JxaMusic::new(&s);
    assert!(
        matches!(m.add(Path::new("/r/x.m4a")), Err(Error::Music(msg)) if msg.contains("追加しなかった"))
    );
}

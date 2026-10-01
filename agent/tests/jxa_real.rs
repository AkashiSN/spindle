//! 本物のミュージック.app で JXA の `Music` 実装を確かめる（macOS だけ、すべて `#[ignore]`）。
//!
//! **試験用のライブラリでだけ実行すること。** ミュージック.app のライブラリへ実際に曲・フォルダ・
//! プレイリストを追加し、最後に消す（途中で落ちても後始末はするが、普段使いのライブラリでは走らせない）。
//! 実行: `cargo test -p spindle-agent --test jxa_real -- --ignored --test-threads=1`
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;

use spindle_agent::music::jxa::{JxaMusic, ProcessOsascript};
use spindle_agent::music::Music;

fn random_hex() -> String {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).unwrap();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `secs` 秒・`freq` Hz の 16bit モノラル WAV を書く
fn write_wav(path: &Path, secs: u32, freq: f64) {
    let rate = 44_100u32;
    let n = rate * secs;
    let data_len = n * 2;
    let mut v = Vec::with_capacity(44 + data_len as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * 2).to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..n {
        let s = (f64::from(i) / f64::from(rate) * freq * std::f64::consts::TAU).sin();
        v.extend_from_slice(&((s * 8000.0) as i16).to_le_bytes());
    }
    std::fs::write(path, v).unwrap();
}

/// `afconvert` で AAC の m4a を作る
fn make_m4a(dir: &Path, out: &Path, secs: u32, freq: f64) {
    let wav = dir.join(format!(".src-{}.wav", random_hex()));
    write_wav(&wav, secs, freq);
    let st = Command::new("afconvert")
        .args(["-f", "m4af", "-d", "aac"])
        .arg(&wav)
        .arg(out)
        .status()
        .unwrap();
    assert!(st.success(), "afconvert が失敗: {st}");
    std::fs::remove_file(&wav).unwrap();
}

/// 途中で落ちても作ったものを消す
struct Cleanup {
    music: JxaMusic<ProcessOsascript>,
    root: PathBuf,
    tracks: Vec<String>,
    playlists: Vec<String>,
    folders: Vec<String>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for p in &self.playlists {
            let _ = self.music.delete_playlist(p);
        }
        for f in &self.folders {
            let _ = self.music.delete_playlist(f);
        }
        for t in &self.tracks {
            let _ = self.music.delete_track(t);
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
#[ignore = "本物のミュージック.app を操作する（試験用ライブラリでだけ）"]
fn real_music_round_trip() {
    let tag = random_hex();
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let root = home.join("Music").join(format!("spindle-jxa-test-{tag}"));
    std::fs::create_dir_all(root.join(".moving")).unwrap();
    let mut c = Cleanup {
        music: JxaMusic::new(ProcessOsascript::new()),
        root: root.clone(),
        tracks: Vec::new(),
        playlists: Vec::new(),
        folders: Vec::new(),
    };

    c.music.probe().unwrap();

    // add 2 曲
    let a_path = root.join("a.m4a");
    let b_path = root.join("b.m4a");
    make_m4a(&root, &a_path, 1, 440.0);
    make_m4a(&root, &b_path, 1, 660.0);
    let a = c.music.add(&a_path).unwrap();
    c.tracks.push(a.persistent_id.clone());
    let b = c.music.add(&b_path).unwrap();
    c.tracks.push(b.persistent_id.clone());
    assert_eq!(a.location.as_deref(), Some(a_path.as_path()));
    assert_eq!(b.location.as_deref(), Some(b_path.as_path()));
    let under = c.music.tracks_under(&root).unwrap();
    assert_eq!(under.len(), 2);
    assert!(c.music.max_database_id().unwrap() >= a.database_id.max(b.database_id));

    // 同じファイルの add は同じ pid
    let a2 = c.music.add(&a_path).unwrap();
    assert_eq!(a2.persistent_id, a.persistent_id);

    // 拡張子なしの一時名へ動かして set_location → 戻して set_location
    let moving = root.join(".moving").join("b-o");
    std::fs::rename(&b_path, &moving).unwrap();
    c.music.set_location(&b.persistent_id, &moving).unwrap();
    let t = c.music.track(&b.persistent_id).unwrap().unwrap();
    assert_eq!(t.location.as_deref(), Some(moving.as_path()));
    std::fs::rename(&moving, &b_path).unwrap();
    c.music.set_location(&b.persistent_id, &b_path).unwrap();
    let t = c.music.track(&b.persistent_id).unwrap().unwrap();
    assert_eq!(t.location.as_deref(), Some(b_path.as_path()));

    // 中身を差し替えて refresh（size が変わる）
    let before = c.music.track(&a.persistent_id).unwrap().unwrap().size;
    let tmp = root.join(".moving").join("a-new.m4a");
    make_m4a(&root, &tmp, 3, 330.0);
    std::fs::rename(&tmp, &a_path).unwrap();
    c.music.refresh(&a.persistent_id).unwrap();
    let after = c.music.track(&a.persistent_id).unwrap().unwrap().size;
    assert_ne!(before, after);

    // フォルダとプレイリスト
    let folder_name = format!("spindle-jxa-{tag}");
    let folder = c.music.create_folder(&folder_name).unwrap();
    c.folders.push(folder.persistent_id.clone());
    assert_eq!(folder.name, folder_name);
    let found = c.music.folders_named(&folder_name).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].persistent_id, folder.persistent_id);
    let pl = c
        .music
        .create_playlist(&folder.persistent_id, ".tmp-x")
        .unwrap();
    c.playlists.push(pl.persistent_id.clone());
    c.music
        .set_playlist_tracks(
            &pl.persistent_id,
            &[b.persistent_id.clone(), a.persistent_id.clone()],
        )
        .unwrap();
    let order = Command::new("osascript")
        .args(["-l", "JavaScript", "-e"])
        .arg(format!(
            "Application('Music').playlists.whose({{persistentID: '{}'}})()[0].tracks.persistentID().join(',')",
            pl.persistent_id
        ))
        .output()
        .unwrap();
    assert!(order.status.success());
    assert_eq!(
        String::from_utf8_lossy(&order.stdout).trim(),
        format!("{},{}", b.persistent_id, a.persistent_id)
    );
    let new_name = "お気に入り 'テスト' \"x\"";
    c.music
        .rename_playlist(&pl.persistent_id, new_name)
        .unwrap();
    let inside = c.music.playlists_in(&folder.persistent_id).unwrap();
    assert_eq!(inside.len(), 1);
    assert_eq!(inside[0].persistent_id, pl.persistent_id);
    assert_eq!(inside[0].name, new_name);
    c.music.delete_playlist(&pl.persistent_id).unwrap();
    c.playlists.clear();
    assert!(c
        .music
        .playlists_in(&folder.persistent_id)
        .unwrap()
        .is_empty());

    // track を消してもファイルは残る
    for t in [&a, &b] {
        c.music.delete_track(&t.persistent_id).unwrap();
        assert_eq!(c.music.track(&t.persistent_id).unwrap(), None);
    }
    c.tracks.clear();
    assert!(a_path.exists());
    assert!(b_path.exists());

    c.music.delete_playlist(&folder.persistent_id).unwrap();
    c.folders.clear();
    assert!(c.music.folder(&folder.persistent_id).unwrap().is_none());
}

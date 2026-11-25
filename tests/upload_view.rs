//! アップロードの件の状態の判定と表示用の組み立て（`import::upload::view`。設計 §4.2・§5.4）。
//! 一時ディレクトリの Inbox に本物の送信（`begin_origin` + `receive_chunk`）でファイルを置き、
//! DB は `:memory:` にマイグレーションを流して tracks / albums を直接入れる

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::io::{Cursor, Write as _};
use std::path::Path;

use rusqlite::{params, Connection};

use spindle::config::{LayoutConfig, UploadConfig};
use spindle::db::open_memory_connection;
use spindle::db::scans::Fingerprint;
use spindle::domain::relpath::{canonical_key, RelPath};
use spindle::fsroot::RootDir;
use spindle::import::upload::{
    begin_origin, build_view, create_upload, decide, parse_manifest, read_manifest, read_staged,
    receive_chunk, unpack, with_manifest, FileClass, Garbage, Group, Grouping, Manifest, Member,
    Status, UploadId, UploadLocks, UploadView,
};

const NOW: i64 = 1_700_000_000;

fn layout() -> LayoutConfig {
    LayoutConfig {
        multi_disc: "{category}/{albumartist}/{album}/{disc}-{track:02} {title}".into(),
        single_disc: "{category}/{albumartist}/{album}/{track:02} {title}".into(),
        unsorted: "_Unsorted/{albumartist}/{album}/{track:02} {title}".into(),
    }
}

struct Env {
    dir: tempfile::TempDir,
    inbox: RootDir,
    locks: UploadLocks,
    id: UploadId,
    conn: Connection,
    cfg: UploadConfig,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["Inbox", "src"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let inbox = RootDir::open(&dir.path().join("Inbox")).unwrap();
        let m = create_upload(&inbox, NOW).unwrap();
        let id = UploadId::parse(&m.id).unwrap();
        Self {
            dir,
            inbox,
            locks: UploadLocks::default(),
            id,
            conn: open_memory_connection().unwrap(),
            cfg: UploadConfig::default(),
        }
    }

    /// 送る元のファイルを作る（WAV。`album` が Some ならタグを付ける）
    fn wav(&self, name: &str, seed: u32, album: Option<&str>, track: u32) -> Vec<u8> {
        let p = self.dir.path().join("src").join(name);
        common::write_wav(&p, &common::pcm_samples(seed), 16);
        if let Some(album) = album {
            common::set_basic_tags(&p, name, "Artist", album, "Artist", track, 1);
        }
        std::fs::read(&p).unwrap()
    }

    /// 送る元のファイルのフィンガープリント（ライブラリの行に入れる）
    fn md5_of(&self, name: &str) -> Vec<u8> {
        let root = RootDir::open(&self.dir.path().join("src")).unwrap();
        let rel = RelPath::parse(name).unwrap();
        let af = spindle::domain::tags::read_audio_file(root.open_file(&rel).unwrap(), Some("wav"))
            .unwrap();
        match spindle::import::scanner::read_fingerprint(&root, &rel, &af) {
            Fingerprint::Md5(Some(m)) => m.to_vec(),
            other => panic!("MD5 が取れない: {other:?}"),
        }
    }

    /// 1 つ送り終える
    fn send(&self, client_path: &str, bytes: &[u8]) -> String {
        let o = begin_origin(
            &self.inbox,
            &self.locks,
            &self.id,
            client_path,
            bytes.len() as u64,
            &self.cfg,
            NOW,
        )
        .unwrap();
        let r = receive_chunk(
            &self.inbox,
            &self.locks,
            &self.id,
            &o.id,
            0,
            bytes,
            NOW,
            &|_| false,
        )
        .unwrap();
        assert!(r.complete);
        o.id
    }

    /// 途中まで送る（未完）
    fn send_partial(&self, client_path: &str, bytes: &[u8]) {
        let o = begin_origin(
            &self.inbox,
            &self.locks,
            &self.id,
            client_path,
            bytes.len() as u64,
            &self.cfg,
            NOW,
        )
        .unwrap();
        receive_chunk(
            &self.inbox,
            &self.locks,
            &self.id,
            &o.id,
            0,
            &bytes[..bytes.len() / 2],
            NOW,
            &|_| false,
        )
        .unwrap();
    }

    fn manifest(&self) -> Manifest {
        read_manifest(&self.inbox, &self.id).unwrap()
    }

    fn edit(&self, f: impl FnOnce(&mut Manifest)) {
        with_manifest(&self.inbox, &self.locks, &self.id, |m| {
            f(m);
            Ok(())
        })
        .unwrap();
    }

    fn view(&self) -> UploadView {
        build_view(
            &self.conn,
            &layout(),
            &self.inbox,
            &self.manifest(),
            &self.cfg,
            false,
        )
        .unwrap()
    }

    /// ライブラリに active な行を 1 つ入れる（album も作る）
    fn library_track(&self, album_dir: &str, album: &str, file: &str, md5: Option<&[u8]>) {
        let key = canonical_key(album_dir);
        let album_id: i64 = match self.conn.query_row(
            "SELECT id FROM albums WHERE rel_dir_key = ?1",
            [&key],
            |r| r.get(0),
        ) {
            Ok(id) => id,
            Err(_) => {
                self.conn
                    .execute(
                        "INSERT INTO albums (rel_dir, rel_dir_key, album, albumartist)
                         VALUES (?1, ?2, ?3, 'Artist')",
                        params![album_dir, key, album],
                    )
                    .unwrap();
                self.conn.last_insert_rowid()
            }
        };
        let rel = format!("{album_dir}/{file}");
        self.conn
            .execute(
                "INSERT INTO tracks (album_id, rel_path, rel_path_key, size, mtime_ns, ctime_ns,
                                     codec, lossless, title, artist_display, album, albumartist,
                                     seen_at, audio_md5, track_no)
                 VALUES (?1, ?2, ?3, 0, 0, 0, 'wav', 1, 't', 'Artist', ?4, 'Artist', 0, ?5, 1)",
                params![album_id, rel, canonical_key(&rel), album, md5],
            )
            .unwrap();
    }
}

fn group_by_album<'a>(v: &'a UploadView, album: &str) -> &'a spindle::import::upload::GroupView {
    v.groups
        .iter()
        .find(|g| g.album.as_deref() == Some(album))
        .unwrap_or_else(|| panic!("件 {album} が無い: {:?}", v.groups))
}

// ---------------------------------------------------------------- 既定のチェック（DB 込み）

#[test]
fn library_duplicate_defaults_unchecked() {
    let env = Env::new();
    let a = env.wav("01.wav", 1, Some("Dup"), 1);
    let b = env.wav("02.wav", 2, Some("Dup"), 2);
    env.send("Dup/01.wav", &a);
    env.send("Dup/02.wav", &b);
    env.library_track(
        "Lib/Artist/Dup",
        "Dup",
        "01.wav",
        Some(&env.md5_of("01.wav")),
    );
    env.library_track(
        "Lib/Artist/Dup",
        "Dup",
        "02.wav",
        Some(&env.md5_of("02.wav")),
    );

    let v = env.view();
    let g = group_by_album(&v, "Dup");
    assert!(!g.included);
    assert!(!g.selectable, "曲が 1 曲も入っていない件は選べない");
    assert_eq!(g.track_count, 0);
    match &g.status {
        Status::LibraryDuplicate { paths } => {
            assert_eq!(
                paths,
                &vec![
                    "Lib/Artist/Dup/01.wav".to_string(),
                    "Lib/Artist/Dup/02.wav".to_string()
                ]
            );
        }
        other => panic!("LibraryDuplicate のはず: {other:?}"),
    }
    assert!(g.tracks.iter().all(|t| !t.included));
    assert_eq!(
        g.tracks[0].library_match.as_deref(),
        Some("Lib/Artist/Dup/01.wav")
    );
    // JSON の形（web との契約）
    let json = serde_json::to_value(&v).unwrap();
    assert_eq!(json["groups"][0]["status"]["kind"], "library_duplicate");
    assert_eq!(json["expires_at"], NOW + 30 * 86_400);
    assert_eq!(json["busy"], false);
}

#[test]
fn partial_duplicate_marks_member_only() {
    let env = Env::new();
    let a = env.wav("01.wav", 1, Some("Half"), 1);
    let b = env.wav("02.wav", 2, Some("Half"), 2);
    env.send("Half/01.wav", &a);
    env.send("Half/02.wav", &b);
    env.library_track("Lib/Artist/X", "X", "01.wav", Some(&env.md5_of("01.wav")));

    let v = env.view();
    let g = group_by_album(&v, "Half");
    assert!(g.included);
    assert!(g.selectable);
    assert_eq!(g.track_count, 1);
    assert!(
        !g.statuses
            .iter()
            .any(|s| matches!(s, Status::LibraryDuplicate { .. })),
        "一部だけの一致は件の状態にしない: {:?}",
        g.statuses
    );
    assert_eq!(g.status, Status::New);
    let t1 = g
        .tracks
        .iter()
        .find(|t| t.display == "Half/01.wav")
        .unwrap();
    let t2 = g
        .tracks
        .iter()
        .find(|t| t.display == "Half/02.wav")
        .unwrap();
    assert!(!t1.included);
    assert_eq!(t1.library_match.as_deref(), Some("Lib/Artist/X/01.wav"));
    assert!(t2.included);
    assert_eq!(t2.library_match, None);
}

// ---------------------------------------------------------------- decide（純粋）

fn manifest() -> Manifest {
    parse_manifest(br#"{"version":1,"id":"0123456789abcdef","created_at":0,"updated_at":0}"#)
        .unwrap()
}

fn member(staged: &str) -> Member {
    Member {
        staged: staged.into(),
        display: staged.into(),
        publish_name: staged.rsplit('/').next().unwrap().into(),
    }
}

fn grouping(groups: &[(&str, &[&str])]) -> Grouping {
    Grouping {
        groups: groups
            .iter()
            .map(|(key, tracks)| Group {
                key: (*key).into(),
                albumartist: "A".into(),
                album: Some((*key).into()),
                origin_label: (*key).into(),
                origins: vec![],
                tracks: tracks.iter().map(|s| member(s)).collect(),
                companions: vec![member(&format!("files/{key}-cover.jpg"))],
            })
            .collect(),
        ignored: vec![],
        unassigned_companions: vec![],
    }
}

fn matched(staged: &[&str]) -> HashMap<String, Vec<String>> {
    staged
        .iter()
        .map(|s| ((*s).to_string(), vec![format!("Lib/{s}")]))
        .collect()
}

#[test]
fn user_reincludes_duplicate() {
    let g = grouping(&[("k1", &["files/o1", "files/o2"])]);
    let matches = matched(&["files/o1", "files/o2"]);
    let mut m = manifest();
    let d = decide(&m, &g, &matches);
    assert!(!d.groups[0].included);
    assert_eq!(d.groups[0].tracks, vec![false, false]);

    m.included_files = vec!["files/o1".into()];
    m.included_groups = vec!["k1".into()];
    let d = decide(&m, &g, &matches);
    assert!(d.groups[0].included);
    assert!(d.groups[0].selectable);
    assert_eq!(d.groups[0].tracks, vec![true, false]);
    assert_eq!(d.groups[0].companions, vec![true]);
}

#[test]
fn excluded_wins_over_included() {
    let g = grouping(&[("k1", &["files/o1", "files/o2"])]);
    let matches = matched(&["files/o1"]);
    let mut m = manifest();
    m.included_files = vec!["files/o1".into()];
    m.excluded_files = vec!["files/o1".into(), "files/o2".into()];
    m.included_groups = vec!["k1".into()];
    m.excluded_groups = vec!["k1".into()];
    let d = decide(&m, &g, &matches);
    assert_eq!(d.groups[0].tracks, vec![false, false]);
    assert!(!d.groups[0].included);

    // 件だけを外す: 曲のチェックは残り、件は選べるが入っていない
    let mut m = manifest();
    m.excluded_groups = vec!["k1".into()];
    m.excluded_files = vec!["files/k1-cover.jpg".into()];
    let d = decide(&m, &g, &matches);
    assert!(!d.groups[0].included);
    assert!(d.groups[0].selectable);
    assert_eq!(d.groups[0].tracks, vec![false, true]);
    assert_eq!(d.groups[0].companions, vec![false]);
}

#[test]
fn group_with_zero_tracks_not_selectable() {
    let g = grouping(&[("k1", &["files/o1"]), ("k2", &["files/o2"])]);
    let mut m = manifest();
    m.excluded_files = vec!["files/o1".into()];
    m.included_groups = vec!["k1".into()];
    let d = decide(&m, &g, &HashMap::new());
    assert!(!d.groups[0].included, "曲が 0 の件は入れ直しても入らない");
    assert!(!d.groups[0].selectable);
    assert!(d.groups[1].included);
    assert!(d.groups[1].selectable);
}

// ---------------------------------------------------------------- 状態

#[test]
fn inbox_exists_only_by_dir() {
    let env = Env::new();
    let a = env.wav("a.wav", 1, Some("Here"), 1);
    let b = env.wav("b.wav", 2, Some("FileOnly"), 1);
    let c = env.wav("c.wav", 3, Some("Elsewhere"), 1);
    env.send("Here/a.wav", &a);
    env.send("FileOnly/b.wav", &b);
    env.send("Elsewhere/c.wav", &c);
    let inbox_dir = env.dir.path().join("Inbox");
    // 置き場所にディレクトリがある
    std::fs::create_dir_all(inbox_dir.join("upload/Artist/Here")).unwrap();
    // 置き場所の名前がファイル（ディレクトリではない）
    std::fs::create_dir_all(inbox_dir.join("upload/Artist")).unwrap();
    std::fs::write(inbox_dir.join("upload/Artist/FileOnly"), b"x").unwrap();
    // 同じ音声が Inbox の別の件にある（音声の一致は見ない）
    std::fs::create_dir_all(inbox_dir.join("other")).unwrap();
    std::fs::write(inbox_dir.join("other/c.wav"), &c).unwrap();

    let v = env.view();
    assert_eq!(
        group_by_album(&v, "Here").status,
        Status::InboxExists {
            dir: "upload/Artist/Here".into()
        }
    );
    assert_eq!(group_by_album(&v, "FileOnly").status, Status::New);
    assert_eq!(group_by_album(&v, "Elsewhere").status, Status::New);
}

#[test]
fn append_to_existing_album() {
    let env = Env::new();
    let a = env.wav("05.wav", 5, Some("Album"), 5);
    env.send("Album/05.wav", &a);
    env.library_track("_Unsorted/Artist/Album", "Album", "01 t.wav", None);

    let v = env.view();
    let g = group_by_album(&v, "Album");
    assert_eq!(
        g.status,
        Status::AppendTo {
            album: "Album".into(),
            track_count: 1
        }
    );
    assert!(g.included);
    let json = serde_json::to_value(g).unwrap();
    assert_eq!(json["status"]["kind"], "append_to");
    assert_eq!(json["status"]["track_count"], 1);
}

#[test]
fn needs_name_for_untagged() {
    let env = Env::new();
    let a = env.wav("x.wav", 1, None, 0);
    env.send("Loose/x.wav", &a);
    let v = env.view();
    assert_eq!(v.groups.len(), 1);
    let g = &v.groups[0];
    assert_eq!(g.album, None);
    assert_eq!(g.status, Status::NeedsName);
    assert_eq!(g.format, "WAV 44.1k/16");

    // 置き場所 upload/_untitled/<origin_label> があれば InboxExists が強い
    std::fs::create_dir_all(env.dir.path().join("Inbox/upload/_untitled/Loose")).unwrap();
    let v = env.view();
    assert_eq!(
        v.groups[0].status,
        Status::InboxExists {
            dir: "upload/_untitled/Loose".into()
        }
    );
    assert_eq!(v.groups[0].statuses.len(), 2);
    assert_eq!(v.groups[0].statuses[1], Status::NeedsName);
}

#[test]
fn unreadable_audio_listed_as_ignored() {
    let env = Env::new();
    let a = env.wav("01.wav", 1, Some("Ok"), 1);
    env.send("Ok/01.wav", &a);
    env.send("Ok/broken.flac", b"this is not flac at all");
    env.send("Ok/readme.txt", b"hello");

    let (files, fps) = read_staged(&env.inbox, &env.manifest()).unwrap();
    let broken = files
        .iter()
        .find(|f| f.display.starts_with("Ok/broken.flac"))
        .unwrap();
    assert_eq!(broken.class, FileClass::Ignored);
    assert!(fps.contains_key(
        &files
            .iter()
            .find(|f| f.display == "Ok/01.wav")
            .unwrap()
            .staged
    ));

    let v = env.view();
    assert_eq!(v.groups.len(), 1);
    assert_eq!(v.groups[0].tracks.len(), 1);
    assert!(
        v.ignored
            .iter()
            .any(|s| s.starts_with("Ok/broken.flac") && s.len() > "Ok/broken.flac".len()),
        "読めない音声は理由付きで取り込まないへ: {:?}",
        v.ignored
    );
    assert!(v.ignored.iter().any(|s| s == "Ok/readme.txt"));
}

fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, data) in entries {
        w.start_file(*name, opts).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap().into_inner()
}

fn write_under(root: &Path, rel: &str, data: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

#[test]
fn garbage_generation_not_read() {
    let env = Env::new();
    let a = env.wav("01.wav", 1, Some("Zipped"), 1);
    let stale = env.wav("stale.wav", 7, Some("Stale"), 1);
    let o = env.send("mora.zip", &zip_of(&[("Disc 1/01.wav", &a)]));
    let out = unpack(
        &env.inbox,
        &env.locks,
        &env.id,
        &o,
        false,
        &env.cfg,
        &tokio_util::sync::CancellationToken::new(),
    )
    .unwrap();
    assert!(matches!(
        out,
        spindle::import::upload::UnpackOutcome::Done { generation: 1, .. }
    ));
    // 古い世代（garbage）と展開中の世代に別の音声を置いておく
    let staging = env
        .dir
        .path()
        .join("Inbox")
        .join(env.id.staging_dir().as_str());
    write_under(&staging, &format!("x/{o}/g2/stale.wav"), &stale);
    write_under(&staging, &format!("x/{o}/g3/stale.wav"), &stale);
    env.edit(|m| {
        m.garbage.push(Garbage {
            path: format!("x/{o}/g2"),
            bytes: 1,
            added_at: NOW,
        });
        let z = m.origins[0].zip.as_mut().unwrap();
        z.unpacking = Some(3);
        z.next_generation = 4;
    });
    // 未完のばらのファイルも読まない
    env.send_partial("Partial/p.wav", &stale);

    let (files, _) = read_staged(&env.inbox, &env.manifest()).unwrap();
    let displays: Vec<&str> = files.iter().map(|f| f.display.as_str()).collect();
    assert_eq!(displays, vec!["mora.zip/Disc 1/01.wav"]);
    assert_eq!(files[0].staged, format!("x/{o}/g1/Disc 1/01.wav"));

    let v = env.view();
    assert_eq!(v.groups.len(), 1);
    assert_eq!(v.groups[0].album.as_deref(), Some("Zipped"));
    let zo = v
        .origins
        .iter()
        .find(|x| x.client_path == "mora.zip")
        .unwrap();
    let z = zo.zip.as_ref().unwrap();
    assert_eq!(z.generation, 1);
    assert!(z.unpacking);
    let p = v
        .origins
        .iter()
        .find(|x| x.client_path == "Partial/p.wav")
        .unwrap();
    assert!(!p.complete);
}

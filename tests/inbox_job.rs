//! Inbox の走査と配置（`import::inbox`、`inbox` ジョブ。SPEC §7.8、D-68、P2-10）。
//! 一時ディレクトリに Inbox / Library を作り、ffmpeg で音声を置く（無ければ skip）

#![cfg(target_os = "linux")]

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use rusqlite::Connection;

use spindle::db::inbox::{self, ItemState};
use spindle::db::Db;
use spindle::fsroot::RootDir;
use spindle::import::inbox::scan_inbox;

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    db: Arc<Db>,
    inbox: Arc<RootDir>,
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["Library", "Inbox", "tmp"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let inbox = Arc::new(RootDir::open(&dir.path().join("Inbox")).unwrap());
        Self {
            dir,
            db_path,
            db,
            inbox,
        }
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.db_path).unwrap()
    }

    fn inbox_path(&self, rel: &str) -> PathBuf {
        self.dir.path().join("Inbox").join(rel)
    }

    /// Inbox に音声を置いてタグを付ける
    fn add(&self, rel: &str, seed: u32, title: &str, album: &str, track: u32) -> Option<PathBuf> {
        let p = self.inbox_path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let ext = rel.rsplit_once('.').unwrap().1;
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        let made = if ext == "wav" {
            common::write_wav(&p, &common::pcm_samples(seed), 16);
            p.clone()
        } else {
            common::make_audio(p.parent().unwrap(), &name, ext, seed)?
        };
        common::set_basic_tags(&made, title, "Artist", album, "Artist", track, 1);
        Some(made)
    }

    async fn scan(&self, now: i64) -> spindle::import::inbox::ScanOutcome {
        scan_inbox(&self.db, &self.inbox, now).await.unwrap()
    }

    fn item(&self, rel_dir: &str) -> Option<inbox::Item> {
        inbox::find_by_dir_key(
            &self.conn(),
            &spindle::domain::relpath::canonical_key(rel_dir),
        )
        .unwrap()
    }
}

// ---------------------------------------------------------------- 走査

#[tokio::test]
async fn scan_detects_directories_and_reads_changed_files_only() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    lib.add("AlbumA/02.flac", 2, "Two", "A", 2);
    lib.add("AlbumB/x.wav", 3, "X", "B", 1);
    lib.add("loose.flac", 4, "Loose", "L", 1);
    // 隠しファイル・非音声・.spindle-tmp-* は無視
    std::fs::write(lib.inbox_path("AlbumA/cover.jpg"), b"jpg").unwrap();
    std::fs::write(lib.inbox_path("AlbumA/.DS_Store"), b"x").unwrap();
    std::fs::write(lib.inbox_path("AlbumA/.spindle-tmp-abc"), b"x").unwrap();
    std::fs::create_dir(lib.inbox_path("Empty")).unwrap();

    let out = lib.scan(1000).await;
    assert_eq!((out.items_seen, out.items_new, out.files_read), (3, 3, 4));
    let a = lib.item("AlbumA").unwrap();
    assert_eq!(a.state, ItemState::Pending);
    assert_eq!((a.detected_at, a.seen_at), (1000, 1000));
    let files = inbox::files(&lib.conn(), a.id).unwrap();
    assert_eq!(
        files
            .iter()
            .map(|f| f.rel_path.as_str())
            .collect::<Vec<_>>(),
        ["AlbumA/01.flac", "AlbumA/02.flac"]
    );
    assert_eq!(files[0].codec, "flac");
    assert!(files[0].lossless);
    assert_eq!(files[0].sample_rate, Some(44100));
    assert!(files[0]
        .tags
        .iter()
        .any(|(k, v)| k == "TITLE" && v == "One"));
    let b = lib.item("AlbumB").unwrap();
    let bf = inbox::files(&lib.conn(), b.id).unwrap();
    assert_eq!(bf[0].codec, "wav");
    let root = lib.item("").unwrap();
    assert_eq!(root.rel_dir, "");
    assert_eq!(
        inbox::files(&lib.conn(), root.id).unwrap()[0].rel_path,
        "loose.flac"
    );
    assert!(lib.item("Empty").is_none());

    // 変わっていなければ読み直さない
    let out = lib.scan(1001).await;
    assert_eq!(
        (
            out.items_seen,
            out.items_new,
            out.files_read,
            out.items_removed
        ),
        (3, 0, 0, 0)
    );
    assert_eq!(lib.item("AlbumA").unwrap().seen_at, 1001);

    // 1 本だけ retag → その 1 本だけ読み直す
    common::set_basic_tags(
        &lib.inbox_path("AlbumA/02.flac"),
        "Two!",
        "Artist",
        "A",
        "Artist",
        2,
        1,
    );
    let out = lib.scan(1002).await;
    assert_eq!(out.files_read, 1);
    let files = inbox::files(&lib.conn(), a.id).unwrap();
    assert!(files[1]
        .tags
        .iter()
        .any(|(k, v)| k == "TITLE" && v == "Two!"));
    assert_eq!(lib.item("AlbumA").unwrap().id, a.id); // 件の id は保つ
}

#[tokio::test]
async fn scan_reverts_changed_approved_items_and_removes_vanished_ones() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    lib.add("AlbumB/01.flac", 2, "One", "B", 1);
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    let b = lib.item("AlbumB").unwrap();
    inbox::set_state(&lib.conn(), a.id, ItemState::Approved, None, 1001).unwrap();
    inbox::set_state(&lib.conn(), b.id, ItemState::Approved, None, 1001).unwrap();
    // A にファイルが増えた → pending に戻る。B は変わっていない → approved のまま
    lib.add("AlbumA/02.flac", 3, "Two", "A", 2);
    lib.scan(1002).await;
    let a = lib.item("AlbumA").unwrap();
    assert_eq!(a.state, ItemState::Pending);
    assert!(a.error.as_deref().unwrap_or("").contains("再承認"));
    assert_eq!(lib.item("AlbumB").unwrap().state, ItemState::Approved);
    assert_eq!(inbox::files(&lib.conn(), a.id).unwrap().len(), 2);

    // ディレクトリが消えた → 行も消える（placed は残す）
    std::fs::remove_dir_all(lib.inbox_path("AlbumB")).unwrap();
    {
        let c = lib.conn();
        c.execute(
            "INSERT INTO albums (rel_dir, rel_dir_key, album) VALUES ('L/P', 'l/p', 'P')",
            [],
        )
        .unwrap();
        let album = c.last_insert_rowid();
        let p = inbox::insert_item(&c, "Gone", "gone", 900).unwrap();
        inbox::set_placed(&c, p, album, 900).unwrap();
    }
    let out = lib.scan(1003).await;
    assert_eq!(out.items_removed, 1);
    assert!(lib.item("AlbumB").is_none());
    assert!(lib.item("Gone").is_some());
    // placed は 24 時間で消える
    let out = lib.scan(900 + 86_401).await;
    assert_eq!(out.items_removed, 1);
    assert!(lib.item("Gone").is_none());
    assert!(lib.item("AlbumA").is_some());
}

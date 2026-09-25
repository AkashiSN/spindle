//! 4 相スキャン（SPEC §7.1、§6、D-24 / D-30 / D-32、docs/TASKS.md P0-6）。
//! 合成ツリーは ffmpeg で作る（無ければ skip）。

#![cfg(target_os = "linux")]

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use lofty::tag::Accessor;
use rusqlite::{params, Connection, OptionalExtension};
use tokio_util::sync::CancellationToken;

use spindle::db::Db;
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, ScanPhase, ScanReport, Scanner, SkipReason};

// ---------------------------------------------------------------- ハーネス

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    scanner: Scanner,
}

#[derive(Debug, Clone, PartialEq)]
struct TrackRow {
    id: i64,
    rel_path: String,
    album_id: Option<i64>,
    title: Option<String>,
    artist_display: Option<String>,
    album: Option<String>,
    albumartist: Option<String>,
    track_no: Option<i64>,
    disc_no: Option<i64>,
    codec: String,
    lossless: bool,
    tag_version: i64,
    audio_version: i64,
    seen_run_id: Option<i64>,
    missing_since: Option<i64>,
    size: i64,
    mtime_ns: i64,
    audio_md5: Option<Vec<u8>>,
    audio_fp: Option<Vec<u8>>,
    tag_hash: Option<Vec<u8>>,
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Library");
        std::fs::create_dir(&lib).unwrap();
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let root = Arc::new(RootDir::open(&lib).unwrap());
        let scanner = Scanner::new(db, root, 4);
        Self {
            dir,
            db_path,
            scanner,
        }
    }

    fn lib(&self) -> PathBuf {
        self.dir.path().join("Library")
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.lib().join(rel)
    }

    /// `rel`（root 相対）に `seed` の音声を作り、基本タグを付ける。ffmpeg が無ければ None
    fn add(&self, rel: &str, seed: u32, title: &str, album: &str, track: u32) -> Option<PathBuf> {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let ext = if rel.ends_with(".opus") {
            "opus"
        } else {
            "flac"
        };
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        let made = common::make_audio(p.parent().unwrap(), &name, ext, seed)?;
        common::set_basic_tags(&made, title, "Artist", album, "AlbumArtist", track, 1);
        Some(made)
    }

    async fn scan(&self) -> ScanReport {
        self.scan_kind(ScanKind::Incremental).await
    }

    async fn scan_kind(&self, kind: ScanKind) -> ScanReport {
        self.scanner
            .run(kind, Arc::new(|_, _, _| {}), CancellationToken::new())
            .await
            .unwrap()
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.db_path).unwrap()
    }

    fn track(&self, rel: &str) -> Option<TrackRow> {
        self.conn()
            .query_row(
                "SELECT id, rel_path, album_id, title, artist_display, album, albumartist, track_no,
                        disc_no, codec, lossless, tag_version, audio_version, seen_run_id,
                        missing_since, size, mtime_ns, audio_md5, audio_fp, tag_hash
                 FROM tracks WHERE rel_path = ?1",
                [rel],
                |r| {
                    Ok(TrackRow {
                        id: r.get(0)?,
                        rel_path: r.get(1)?,
                        album_id: r.get(2)?,
                        title: r.get(3)?,
                        artist_display: r.get(4)?,
                        album: r.get(5)?,
                        albumartist: r.get(6)?,
                        track_no: r.get(7)?,
                        disc_no: r.get(8)?,
                        codec: r.get(9)?,
                        lossless: r.get::<_, i64>(10)? == 1,
                        tag_version: r.get(11)?,
                        audio_version: r.get(12)?,
                        seen_run_id: r.get(13)?,
                        missing_since: r.get(14)?,
                        size: r.get(15)?,
                        mtime_ns: r.get(16)?,
                        audio_md5: r.get(17)?,
                        audio_fp: r.get(18)?,
                        tag_hash: r.get(19)?,
                    })
                },
            )
            .optional()
            .unwrap()
    }

    fn track_count(&self) -> i64 {
        self.conn()
            .query_row("SELECT count(*) FROM tracks", [], |r| r.get(0))
            .unwrap()
    }

    fn album_of_dir(&self, rel_dir: &str) -> Option<(i64, Option<String>, Option<i64>)> {
        self.conn()
            .query_row(
                "SELECT id, album, missing_since FROM albums WHERE rel_dir = ?1",
                [rel_dir],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .unwrap()
    }

    fn tag_values(&self, track_id: i64, key: &str) -> Vec<String> {
        self.conn()
            .prepare("SELECT value FROM track_tags WHERE track_id = ?1 AND key = ?2 ORDER BY idx")
            .unwrap()
            .query_map(params![track_id, key], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn album_category(&self, rel_dir: &str) -> Option<i64> {
        self.conn()
            .query_row(
                "SELECT category_id FROM albums WHERE rel_dir = ?1",
                [rel_dir],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn category_names(&self) -> Vec<String> {
        self.conn()
            .prepare("SELECT name FROM categories ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn run_state(&self, run_id: i64) -> String {
        self.conn()
            .query_row("SELECT state FROM scan_runs WHERE id = ?1", [run_id], |r| {
                r.get(0)
            })
            .unwrap()
    }
}

/// mtime を保ったまま上書きする（`touch -r` 相当）。ctime は進む
fn overwrite_preserving_mtime(path: &Path, f: impl FnOnce(&Path)) {
    let mtime = std::fs::metadata(path).unwrap().modified().unwrap();
    f(path);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
}

fn set_mtime(path: &Path, t: SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

// ---------------------------------------------------------------- 初回スキャン

#[tokio::test]
async fn initial_scan_registers_tracks_albums_and_skips_non_targets() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("Pop/AA/Album1/01 One.flac", 1, "One", "Album1", 1));
    lib.add("Pop/AA/Album1/02 Two.flac", 2, "Two", "Album1", 2);
    lib.add("Pop/AA/Album2/01 Uno.opus", 3, "Uno", "Album2", 1);
    std::fs::write(lib.path("Pop/AA/Album1/cover.jpg"), b"jpg").unwrap();
    std::fs::write(lib.path("Pop/AA/Album1/.DS_Store"), b"x").unwrap();
    std::os::unix::fs::symlink("01 One.flac", lib.path("Pop/AA/Album1/link.flac")).unwrap();
    // SMB 禁止名（末尾ドット）
    std::fs::create_dir(lib.path("Bad.")).unwrap();
    std::fs::copy(
        lib.path("Pop/AA/Album1/01 One.flac"),
        lib.path("Bad./x.flac"),
    )
    .unwrap();
    std::fs::write(lib.path("Pop/AA/Album1/what?.flac"), b"not audio").unwrap();

    let r = lib.scan().await;
    assert_eq!(lib.run_state(r.run_id), "completed");
    assert_eq!(r.new, 3);
    assert_eq!(lib.track_count(), 3);

    let one = lib.track("Pop/AA/Album1/01 One.flac").unwrap();
    assert_eq!(one.title.as_deref(), Some("One"));
    assert_eq!(one.artist_display.as_deref(), Some("Artist"));
    assert_eq!(one.album.as_deref(), Some("Album1"));
    assert_eq!(one.albumartist.as_deref(), Some("AlbumArtist"));
    assert_eq!(one.track_no, Some(1));
    assert_eq!(one.disc_no, Some(1));
    assert_eq!(one.codec, "flac");
    assert!(one.lossless);
    assert_eq!(one.tag_version, 1);
    assert_eq!(one.audio_version, 1);
    assert_eq!(one.seen_run_id, Some(r.run_id));
    assert!(one.audio_md5.is_some());
    assert!(one.audio_fp.is_none());
    assert!(one.tag_hash.is_some());
    assert_eq!(lib.tag_values(one.id, "TITLE"), ["One"]);
    assert_eq!(lib.tag_values(one.id, "ALBUMARTIST"), ["AlbumArtist"]);

    let uno = lib.track("Pop/AA/Album2/01 Uno.opus").unwrap();
    assert_eq!(uno.codec, "opus");
    assert!(!uno.lossless);
    assert!(uno.audio_md5.is_none());
    assert!(uno.audio_fp.is_some());

    // album = ディレクトリ
    let (a1, name1, _) = lib.album_of_dir("Pop/AA/Album1").unwrap();
    let (a2, name2, _) = lib.album_of_dir("Pop/AA/Album2").unwrap();
    assert_ne!(a1, a2);
    assert_eq!(name1.as_deref(), Some("Album1"));
    assert_eq!(name2.as_deref(), Some("Album2"));
    assert_eq!(one.album_id, Some(a1));
    assert_eq!(uno.album_id, Some(a2));

    // 対象外の一覧
    let mut skipped: Vec<(String, SkipReason)> = r
        .skipped
        .iter()
        .map(|s| (s.path.clone(), s.reason))
        .collect();
    skipped.sort();
    assert_eq!(
        skipped,
        [
            ("Bad.".to_owned(), SkipReason::InvalidName),
            ("Pop/AA/Album1/link.flac".to_owned(), SkipReason::Symlink),
            (
                "Pop/AA/Album1/what?.flac".to_owned(),
                SkipReason::InvalidName
            ),
        ]
    );
    assert_eq!(r.files_seen, 3);
}

#[tokio::test]
async fn category_is_inferred_from_top_directory_then_genre_map() {
    // D-92 以降、Library 直下のディレクトリは自動で語彙になるので、GENRE の写像が効くのは語彙に
    // しない `_Unsorted` の下だけ
    let lib = Lib::new();
    require_ffmpeg!(lib.add("Pop/AA/A/01.flac", 1, "t", "A", 1));
    let p = lib.add("_Unsorted/BB/B/01.flac", 2, "t", "B", 1).unwrap();
    common::retag(&p, |tag| {
        tag.set_genre("J-Pop".to_owned());
    });
    lib.add("_Unsorted/CC/C/01.flac", 3, "t", "C", 1);
    {
        let c = lib.conn();
        c.execute(
            "INSERT INTO categories (id, name) VALUES (1, 'Pop'), (2, 'Anime')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO genre_category_map (genre, category_id) VALUES ('J-Pop', 2)",
            [],
        )
        .unwrap();
    }
    lib.scan().await;
    assert_eq!(
        lib.album_category("Pop/AA/A"),
        Some(1),
        "先頭ディレクトリ名が語彙に一致"
    );
    assert_eq!(lib.album_category("_Unsorted/BB/B"), Some(2), "GENRE → map");
    assert_eq!(lib.album_category("_Unsorted/CC/C"), None);
}

#[tokio::test]
async fn top_directories_become_categories_on_the_first_scan() {
    // D-92: 空の語彙から、Library 直下のディレクトリ名が語彙になり album に付く
    let lib = Lib::new();
    require_ffmpeg!(lib.add("Anime/AA/A/01.flac", 1, "t", "A", 1));
    lib.add("J-POP/BB/B/01.flac", 2, "t", "B", 1);
    lib.add("東方Project/CC/C/01.flac", 3, "t", "C", 1);
    lib.add("_Unsorted/DD/D/01.flac", 4, "t", "D", 1);
    // 直下のディレクトリそのものに置いた曲と root 直下の曲は category を名乗らない
    lib.add("Loose/01.flac", 5, "t", "L", 1);
    lib.add("root.flac", 6, "t", "R", 1);
    let report = lib.scan().await;
    assert_eq!(report.categories_registered, 3);
    assert_eq!(
        lib.category_names(),
        vec![
            "Anime".to_owned(),
            "J-POP".to_owned(),
            "東方Project".to_owned()
        ]
    );
    let id_of = |name: &str| -> i64 {
        lib.conn()
            .query_row("SELECT id FROM categories WHERE name = ?1", [name], |r| {
                r.get(0)
            })
            .unwrap()
    };
    assert_eq!(lib.album_category("Anime/AA/A"), Some(id_of("Anime")));
    assert_eq!(lib.album_category("J-POP/BB/B"), Some(id_of("J-POP")));
    assert_eq!(
        lib.album_category("東方Project/CC/C"),
        Some(id_of("東方Project"))
    );
    assert_eq!(
        lib.album_category("_Unsorted/DD/D"),
        None,
        "_Unsorted は語彙にしない"
    );
    assert_eq!(lib.album_category("Loose"), None);
    // 2 回目は何も足さない
    let again = lib.scan().await;
    assert_eq!(again.categories_registered, 0);
    assert_eq!(again.albums_categorized, 0);
    assert_eq!(lib.category_names().len(), 3);
}

#[tokio::test]
async fn existing_names_are_not_registered_twice_across_case_and_normalization() {
    // 語彙に大小・NFC/NFD 違いの同じ名前があれば、それを使って重複登録しない
    let lib = Lib::new();
    require_ffmpeg!(lib.add("anime/AA/A/01.flac", 1, "t", "A", 1));
    lib.add("ボカロ/BB/B/01.flac", 2, "t", "B", 1);
    {
        let c = lib.conn();
        // ボ を NFD（ホ + 結合濁点）で書いた語彙
        c.execute(
            "INSERT INTO categories (id, name) VALUES (1, 'Anime'), (2, ?1)",
            ["\u{30db}\u{3099}\u{30ab}\u{30ed}"],
        )
        .unwrap();
    }
    let report = lib.scan().await;
    assert_eq!(report.categories_registered, 0);
    assert_eq!(lib.category_names().len(), 2);
    assert_eq!(lib.album_category("anime/AA/A"), Some(1));
    assert_eq!(lib.album_category("ボカロ/BB/B"), Some(2));
}

#[tokio::test]
async fn null_categories_of_an_existing_db_are_filled_on_the_next_scan_without_overwriting() {
    // 語彙の無い古い DB で category が NULL のまま登録された album も、次の（増分・変化なしの）スキャンで
    // 埋まる。人が付けた値は変えない
    let lib = Lib::new();
    require_ffmpeg!(lib.add("Anime/AA/A/01.flac", 1, "t", "A", 1));
    lib.add("Anime/BB/B/01.flac", 2, "t", "B", 1);
    lib.add("Game/CC/C/01.flac", 3, "t", "C", 1);
    lib.scan().await;
    // D-92 以前の状態を作る: 語彙を消して（ON DELETE SET NULL で album は NULL）、人の値を 1 つ付ける
    {
        let c = lib.conn();
        c.execute("PRAGMA foreign_keys = ON", []).unwrap();
        c.execute("DELETE FROM categories", []).unwrap();
        c.execute("INSERT INTO categories (id, name) VALUES (50, 'Drama')", [])
            .unwrap();
        c.execute(
            "UPDATE albums SET category_id = 50 WHERE rel_dir = 'Anime/BB/B'",
            [],
        )
        .unwrap();
    }
    assert_eq!(lib.album_category("Anime/AA/A"), None);
    let report = lib.scan().await;
    assert_eq!(report.unchanged, 3, "ファイルは変わっていない（最速パス）");
    assert_eq!(report.categories_registered, 2, "Anime と Game");
    assert_eq!(report.albums_categorized, 2);
    let anime: i64 = lib
        .conn()
        .query_row("SELECT id FROM categories WHERE name = 'Anime'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(lib.album_category("Anime/AA/A"), Some(anime));
    assert_eq!(
        lib.album_category("Anime/BB/B"),
        Some(50),
        "人が付けた値は上書きしない"
    );
    assert!(lib.album_category("Game/CC/C").is_some());
    // ファイルは変わっていないが、埋めた album のトラックは `library` イベントで表へ知らせる
    let aa = lib.track("Anime/AA/A/01.flac").unwrap().id;
    let bb = lib.track("Anime/BB/B/01.flac").unwrap().id;
    let cc = lib.track("Game/CC/C/01.flac").unwrap().id;
    assert!(report.changed_ids.contains(&aa) && report.changed_ids.contains(&cc));
    assert!(
        !report.changed_ids.contains(&bb),
        "埋めていない album は知らせない"
    );
}

#[tokio::test]
async fn a_category_added_by_the_api_during_the_scan_is_used_instead_of_the_genre_map() {
    // Phase 2 で語彙を読んだ後・Phase 4 の前に API が同じ名前（大小違い）を足しても、Phase 4 は
    // トランザクションの中で読み直してその語彙を付ける（古い語彙のまま GENRE の写像が付かない）
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("Anime/AA/A/01.flac", 1, "t", "A", 1));
    common::retag(&p, |tag| {
        tag.set_genre("Pop".to_owned());
    });
    {
        let c = lib.conn();
        c.execute("INSERT INTO categories (id, name) VALUES (7, 'Pop')", [])
            .unwrap();
        c.execute(
            "INSERT INTO genre_category_map (genre, category_id) VALUES ('Pop', 7)",
            [],
        )
        .unwrap();
    }
    let db_path = lib.db_path.clone();
    lib.scanner.set_before_artwork_hook(Arc::new(move |point| {
        if point == "before_commit" {
            Connection::open(&db_path)
                .unwrap()
                .execute("INSERT INTO categories (id, name) VALUES (8, 'anime')", [])
                .unwrap();
        }
    }));
    let report = lib.scan().await;
    assert_eq!(report.categories_registered, 0, "API が先に足した");
    assert_eq!(lib.album_category("Anime/AA/A"), Some(8));
    assert_eq!(lib.category_names().len(), 2);
}

#[test]
fn the_null_category_fill_uses_the_partial_index() {
    // D-92: スキャンごとの「category が NULL の album」の問い合わせは部分索引を使い、全 album を走査しない
    let conn = Connection::open_in_memory().unwrap();
    let mut conn = conn;
    spindle::db::migrations::apply(&mut conn).unwrap();
    let plan: Vec<String> = conn
        .prepare(
            "EXPLAIN QUERY PLAN SELECT id, rel_dir FROM albums
              WHERE category_id IS NULL AND missing_since IS NULL",
        )
        .unwrap()
        .query_map([], |r| r.get::<_, String>(3))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(
        plan.iter().any(|p| p.contains("idx_albums_category_null")),
        "{plan:?}"
    );
}

#[tokio::test]
async fn unsorted_layout_prefix_is_not_registered() {
    // `[layout].unsorted` の先頭を別名にしている場合もその名前は語彙にしない
    let dir = tempfile::tempdir().unwrap();
    let root_path = dir.path().join("Library");
    std::fs::create_dir(&root_path).unwrap();
    let db_path = dir.path().join("spindle.db");
    let db = Arc::new(Db::open(&db_path).unwrap());
    let root = Arc::new(RootDir::open(&root_path).unwrap());
    let scanner = Scanner::new(db, root, 2)
        .with_unsorted_layout("未分類/{albumartist}/{album}/{track:02}. {title}");
    let p = root_path.join("未分類/AA/A");
    std::fs::create_dir_all(&p).unwrap();
    require_ffmpeg!(common::make_audio(&p, "01.flac", "flac", 1));
    let q = root_path.join("Anime/BB/B");
    std::fs::create_dir_all(&q).unwrap();
    common::make_audio(&q, "01.flac", "flac", 2).unwrap();
    let report = scanner
        .run(
            ScanKind::Incremental,
            Arc::new(|_, _, _| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(report.categories_registered, 1);
    let names: Vec<String> = Connection::open(&db_path)
        .unwrap()
        .prepare("SELECT name FROM categories ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(names, vec!["Anime".to_owned()]);
}

// ---------------------------------------------------------------- 2 回目（最速パス）

#[tokio::test]
async fn second_scan_takes_the_fast_path_and_only_touches_seen_columns() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.add("A/B/02.opus", 2, "t2", "B", 2);
    let r1 = lib.scan().await;
    let before = lib.track("A/B/01.flac").unwrap();

    let r2 = lib.scan().await;
    assert_eq!(r2.unchanged, 2);
    assert_eq!(r2.new + r2.updated, 0);
    let after = lib.track("A/B/01.flac").unwrap();
    assert_eq!(after.seen_run_id, Some(r2.run_id));
    assert_ne!(r1.run_id, r2.run_id);
    assert_eq!(
        TrackRow {
            seen_run_id: before.seen_run_id,
            ..after.clone()
        },
        before
    );
}

/// ホスト再起動で dev 番号が振り直された（DB の dev だけが古い）。inode / size / mtime / ctime が
/// 同じなら最速パスで済ませ、dev だけ現在値へ直す（D-62）
#[tokio::test]
async fn dev_renumbering_after_reboot_takes_the_fast_path_and_updates_dev() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.add("A/B/02.opus", 2, "t2", "B", 2);
    lib.scan().await;
    let before = lib.track("A/B/01.flac").unwrap();
    let live_dev: i64 = lib
        .conn()
        .query_row("SELECT dev FROM tracks WHERE id = ?1", [before.id], |r| {
            r.get(0)
        })
        .unwrap();
    lib.conn()
        .execute("UPDATE tracks SET dev = dev + 1", [])
        .unwrap();

    let r = lib.scan().await;
    assert_eq!(r.unchanged, 2, "{r:?}");
    assert_eq!(r.updated + r.new + r.moved, 0, "{r:?}");
    let after = lib.track("A/B/01.flac").unwrap();
    assert_eq!(
        TrackRow {
            seen_run_id: before.seen_run_id,
            ..after.clone()
        },
        before,
        "dev 以外の列は触らない"
    );
    let devs: Vec<i64> = lib
        .conn()
        .prepare("SELECT dev FROM tracks ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(devs, [live_dev, live_dev], "dev を現在値へ直す");

    // 直った後はふつうの最速パス
    let r = lib.scan().await;
    assert_eq!(r.unchanged, 2);
}

// ---------------------------------------------------------------- 移動・rename

#[tokio::test]
async fn external_move_keeps_track_id_and_does_not_duplicate() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.add("A/B/02.flac", 2, "t2", "B", 2);
    lib.scan().await;
    let id = lib.track("A/B/01.flac").unwrap().id;

    std::fs::create_dir_all(lib.path("A/C")).unwrap();
    std::fs::rename(lib.path("A/B/01.flac"), lib.path("A/C/01.flac")).unwrap();
    let r = lib.scan().await;
    assert_eq!(r.moved, 1);
    assert_eq!(lib.track_count(), 2, "重複が増えない");
    assert!(lib.track("A/B/01.flac").is_none());
    let moved = lib.track("A/C/01.flac").unwrap();
    assert_eq!(moved.id, id);
    assert_eq!(moved.tag_version, 1);
    assert_eq!(moved.audio_version, 1);
    // 再走査しても安定
    let r = lib.scan().await;
    assert_eq!(r.unchanged, 2);
    assert_eq!(lib.track_count(), 2);
}

#[tokio::test]
async fn copy_then_delete_is_a_move_by_md5_and_copy_with_source_is_a_duplicate() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.scan().await;
    let id = lib.track("A/B/01.flac").unwrap().id;

    // cp + rm（inode が変わる）
    std::fs::copy(lib.path("A/B/01.flac"), lib.path("A/B/01x.flac")).unwrap();
    std::fs::remove_file(lib.path("A/B/01.flac")).unwrap();
    lib.scan().await;
    assert_eq!(lib.track("A/B/01x.flac").unwrap().id, id);
    assert_eq!(lib.track_count(), 1);

    // コピー元が残る → 新規 + duplicate_groups
    std::fs::copy(lib.path("A/B/01x.flac"), lib.path("A/B/01y.flac")).unwrap();
    lib.scan().await;
    assert_eq!(lib.track_count(), 2);
    let n: i64 = lib
        .conn()
        .query_row("SELECT count(*) FROM duplicate_groups", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn swap_of_two_files_follows_inodes() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/B/01.flac", 1, "one", "B", 1));
    lib.add("A/B/02.flac", 2, "two", "B", 2);
    lib.scan().await;
    let one = lib.track("A/B/01.flac").unwrap();
    let two = lib.track("A/B/02.flac").unwrap();

    std::fs::rename(lib.path("A/B/01.flac"), lib.path("A/B/tmp.flac")).unwrap();
    std::fs::rename(lib.path("A/B/02.flac"), lib.path("A/B/01.flac")).unwrap();
    std::fs::rename(lib.path("A/B/tmp.flac"), lib.path("A/B/02.flac")).unwrap();
    lib.scan().await;
    assert_eq!(lib.track_count(), 2);
    let now_at_01 = lib.track("A/B/01.flac").unwrap();
    let now_at_02 = lib.track("A/B/02.flac").unwrap();
    assert_eq!(now_at_01.id, two.id);
    assert_eq!(now_at_01.title.as_deref(), Some("two"));
    assert_eq!(now_at_02.id, one.id);
    assert_eq!(now_at_02.title.as_deref(), Some("one"));
}

#[tokio::test]
async fn album_directory_rename_keeps_album_id_even_when_swapped() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/X/01.flac", 1, "x1", "X", 1));
    lib.add("A/X/02.flac", 2, "x2", "X", 2);
    lib.add("A/Y/01.flac", 3, "y1", "Y", 1);
    lib.add("A/Y/02.flac", 4, "y2", "Y", 2);
    lib.scan().await;
    let (xid, _, _) = lib.album_of_dir("A/X").unwrap();
    let (yid, _, _) = lib.album_of_dir("A/Y").unwrap();

    // 単純 rename
    std::fs::rename(lib.path("A/X"), lib.path("A/X2")).unwrap();
    lib.scan().await;
    assert_eq!(lib.album_of_dir("A/X2").unwrap().0, xid);
    assert!(lib.album_of_dir("A/X").is_none());
    assert_eq!(lib.track("A/X2/01.flac").unwrap().album_id, Some(xid));

    // swap
    std::fs::rename(lib.path("A/X2"), lib.path("A/tmp")).unwrap();
    std::fs::rename(lib.path("A/Y"), lib.path("A/X2")).unwrap();
    std::fs::rename(lib.path("A/tmp"), lib.path("A/Y")).unwrap();
    lib.scan().await;
    assert_eq!(lib.album_of_dir("A/X2").unwrap().0, yid);
    assert_eq!(lib.album_of_dir("A/Y").unwrap().0, xid);
    assert_eq!(lib.track("A/Y/01.flac").unwrap().album_id, Some(xid));
    assert_eq!(lib.track("A/X2/01.flac").unwrap().album_id, Some(yid));
    let albums: i64 = lib
        .conn()
        .query_row(
            "SELECT count(*) FROM albums WHERE missing_since IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(albums, 2);
}

#[tokio::test]
async fn album_split_and_merge() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/X/01.flac", 1, "x1", "X", 1));
    lib.add("A/X/02.flac", 2, "x2", "X", 2);
    lib.add("A/X/03.flac", 3, "x3", "X", 3);
    lib.scan().await;
    let (xid, _, _) = lib.album_of_dir("A/X").unwrap();

    // 分割: 1 曲だけ別ディレクトリへ → 移った側は新規 album、残った側は id 維持
    std::fs::create_dir(lib.path("A/Z")).unwrap();
    std::fs::rename(lib.path("A/X/03.flac"), lib.path("A/Z/03.flac")).unwrap();
    lib.scan().await;
    assert_eq!(lib.album_of_dir("A/X").unwrap().0, xid);
    let (zid, _, _) = lib.album_of_dir("A/Z").unwrap();
    assert_ne!(zid, xid);
    assert_eq!(lib.track("A/Z/03.flac").unwrap().album_id, Some(zid));

    // 統合: Z を X に戻す → X が id を維持、Z は構成 0 で missing
    std::fs::rename(lib.path("A/Z/03.flac"), lib.path("A/X/03.flac")).unwrap();
    std::fs::remove_dir(lib.path("A/Z")).unwrap();
    lib.scan().await;
    assert_eq!(lib.track("A/X/03.flac").unwrap().album_id, Some(xid));
    let (zid2, _, missing) = lib.album_of_dir("A/Z").unwrap();
    assert_eq!(zid2, zid);
    assert!(missing.is_some(), "構成 0 の album は missing_since");
}

// ---------------------------------------------------------------- 変更検出と版

#[tokio::test]
async fn tag_change_bumps_tag_version_only() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.scan().await;
    common::retag(&p, |tag| tag.set_title("changed".to_owned()));
    let r = lib.scan().await;
    assert_eq!(r.updated, 1);
    let t = lib.track("A/B/01.flac").unwrap();
    assert_eq!(t.title.as_deref(), Some("changed"));
    assert_eq!(t.tag_version, 2);
    assert_eq!(t.audio_version, 1);
    assert_eq!(lib.tag_values(t.id, "TITLE"), ["changed"]);
}

#[tokio::test]
async fn resaving_identical_tags_does_not_bump_tag_version() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.scan().await;
    let before = lib.track("A/B/01.flac").unwrap();
    // 同じ値を書き直す（mtime は進む）
    common::retag(&p, |tag| tag.set_title("t".to_owned()));
    set_mtime(&p, SystemTime::now() + Duration::from_secs(5));
    let r = lib.scan().await;
    assert_eq!(r.updated, 1);
    let after = lib.track("A/B/01.flac").unwrap();
    assert_eq!(after.tag_version, before.tag_version);
    assert_eq!(after.audio_version, before.audio_version);
    assert_ne!(after.mtime_ns, before.mtime_ns);
}

#[tokio::test]
async fn mtime_preserving_overwrite_is_detected_via_ctime() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.scan().await;
    let before = lib.track("A/B/01.flac").unwrap();
    // inode の時刻はカーネルの粗い時計（数 ms 刻み）なので、直前の stat と同じ刻みで書き換えると
    // ctime が動かない。刻みをまたいでから書き換える
    std::thread::sleep(Duration::from_millis(20));
    overwrite_preserving_mtime(&p, |p| {
        common::retag(p, |tag| tag.set_title("x".to_owned()))
    });
    let r = lib.scan().await;
    assert_eq!(r.updated, 1, "touch -r 相当でも ctime で検出する");
    let after = lib.track("A/B/01.flac").unwrap();
    assert_eq!(after.mtime_ns, before.mtime_ns);
    assert_eq!(after.title.as_deref(), Some("x"));
    assert_eq!(after.tag_version, 2);
}

#[tokio::test]
async fn audio_change_bumps_audio_version_for_lossless_and_lossy() {
    let lib = Lib::new();
    let f = require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    let o = lib.add("A/B/02.opus", 2, "t2", "B", 2).unwrap();
    lib.scan().await;

    // 同じパスに別の音声を同じタグで置く（inode は同じ = 上書き）
    let dir = tempfile::tempdir().unwrap();
    let nf = common::make_audio(dir.path(), "n.flac", "flac", 9).unwrap();
    common::set_basic_tags(&nf, "t", "Artist", "B", "AlbumArtist", 1, 1);
    std::fs::copy(&nf, &f).unwrap();
    let no = common::make_audio(dir.path(), "n.opus", "opus", 9).unwrap();
    common::set_basic_tags(&no, "t2", "Artist", "B", "AlbumArtist", 2, 1);
    std::fs::copy(&no, &o).unwrap();

    lib.scan().await;
    let t = lib.track("A/B/01.flac").unwrap();
    assert_eq!(t.audio_version, 2);
    assert_eq!(t.tag_version, 1);
    let t2 = lib.track("A/B/02.opus").unwrap();
    assert_eq!(t2.audio_version, 2);
    assert_eq!(t2.tag_version, 1);
}

// ---------------------------------------------------------------- missing / 復活 / 途中失敗

#[tokio::test]
async fn missing_is_set_on_completed_run_and_revived_on_return() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.add("A/B/02.flac", 2, "t2", "B", 2);
    lib.scan().await;
    let id = lib.track("A/B/01.flac").unwrap().id;
    let (album, _, _) = lib.album_of_dir("A/B").unwrap();

    let stash = lib.dir.path().join("stash.flac");
    std::fs::rename(lib.path("A/B/01.flac"), &stash).unwrap();
    let r = lib.scan().await;
    assert_eq!(r.missing_marked, 1);
    let t = lib.track("A/B/01.flac").unwrap();
    assert_eq!(t.id, id);
    assert!(t.missing_since.is_some());
    assert!(
        lib.album_of_dir("A/B").unwrap().2.is_none(),
        "1 曲残るので album は生きている"
    );

    std::fs::rename(&stash, lib.path("A/B/01.flac")).unwrap();
    let r = lib.scan().await;
    assert_eq!(r.revived, 1);
    let t = lib.track("A/B/01.flac").unwrap();
    assert_eq!(t.id, id);
    assert!(t.missing_since.is_none());
    assert_eq!(t.album_id, Some(album));
    assert_eq!(lib.track_count(), 2);
}

#[tokio::test]
async fn cancelled_run_does_not_mark_missing() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.add("A/B/02.flac", 2, "t2", "B", 2);
    lib.scan().await;
    std::fs::remove_file(lib.path("A/B/01.flac")).unwrap();
    // 2 曲目のタグを変えて Phase 3 に仕事を作り、最初の進捗でキャンセルする
    common::retag(&lib.path("A/B/02.flac"), |tag| {
        tag.set_title("x".to_owned())
    });

    let token = CancellationToken::new();
    let t = token.clone();
    let calls = Arc::new(AtomicU64::new(0));
    let c = calls.clone();
    let progress = Arc::new(move |_phase, _done: u64, _total: u64| {
        c.fetch_add(1, Ordering::SeqCst);
        t.cancel();
    });
    let err = lib
        .scanner
        .run(ScanKind::Incremental, progress, token)
        .await
        .unwrap_err();
    assert!(
        matches!(err, spindle::import::scanner::ScanError::Cancelled),
        "{err:?}"
    );
    assert!(calls.load(Ordering::SeqCst) >= 1);

    let t = lib.track("A/B/01.flac").unwrap();
    assert!(
        t.missing_since.is_none(),
        "cancelled では missing を立てない"
    );
    let state: String = lib
        .conn()
        .query_row(
            "SELECT state FROM scan_runs ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "cancelled");
}

#[tokio::test]
async fn failed_run_when_root_walk_errors_does_not_mark_missing() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.scan().await;
    // root を読めなくする（walk がエラー → failed）
    let mode = std::fs::metadata(lib.lib()).unwrap().permissions();
    let mut denied = mode.clone();
    std::os::unix::fs::PermissionsExt::set_mode(&mut denied, 0o000);
    std::fs::set_permissions(lib.lib(), denied).unwrap();
    let res = lib
        .scanner
        .run(
            ScanKind::Incremental,
            Arc::new(|_, _, _| {}),
            CancellationToken::new(),
        )
        .await;
    std::fs::set_permissions(lib.lib(), mode).unwrap();
    if nix_is_root() {
        eprintln!("root では権限で失敗させられないので skip");
        return;
    }
    assert!(res.is_err());
    assert!(lib.track("A/B/01.flac").unwrap().missing_since.is_none());
    let state: String = lib
        .conn()
        .query_row(
            "SELECT state FROM scan_runs ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "failed");
}

fn nix_is_root() -> bool {
    rustix::process::geteuid().is_root()
}

// ---------------------------------------------------------------- pending op（D-24）

fn insert_pending_op(conn: &Connection, track_id: i64, kind: &str, expected_rel_path: &str) -> i64 {
    conn.execute(
        "INSERT INTO edit_batches (id, created_at, state) VALUES (1, 0, 'prepared')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edit_ops (batch_id, ordinal, track_id, kind, expected_rel_path)
         VALUES (1, 1, ?1, ?2, ?3)",
        params![track_id, kind, expected_rel_path],
    )
    .unwrap();
    conn.last_insert_rowid()
}

#[tokio::test]
async fn pending_tags_op_keeps_db_tags_but_follows_physical_state() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.scan().await;
    let before = lib.track("A/B/01.flac").unwrap();
    // 編集バッチが DB を先行更新した状態を再現
    {
        let c = lib.conn();
        insert_pending_op(&c, before.id, "tags", "A/B/01.flac");
        c.execute(
            "UPDATE tracks SET title = 'edited', tag_version = tag_version + 1 WHERE id = ?1",
            [before.id],
        )
        .unwrap();
        c.execute(
            "UPDATE track_tags SET value = 'edited' WHERE track_id = ?1 AND key = 'TITLE'",
            [before.id],
        )
        .unwrap();
    }
    // 外部でファイルを書き換え + 別ディレクトリへ移動
    common::retag(&p, |tag| tag.set_title("external".to_owned()));
    std::fs::create_dir(lib.path("A/C")).unwrap();
    std::fs::rename(&p, lib.path("A/C/01.flac")).unwrap();

    lib.scan().await;
    let t = lib.track("A/C/01.flac").unwrap();
    assert_eq!(t.id, before.id);
    assert_eq!(
        t.title.as_deref(),
        Some("edited"),
        "pending の論理フィールドは巻き戻さない"
    );
    assert_eq!(t.tag_version, before.tag_version + 1);
    assert_eq!(lib.tag_values(t.id, "TITLE"), ["edited"]);
    assert_eq!(t.tag_hash, before.tag_hash);
    assert_ne!(t.size, before.size, "物理属性は追随する");
}

#[tokio::test]
async fn pending_rename_op_conflicts_with_external_rename() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.scan().await;
    let before = lib.track("A/B/01.flac").unwrap();
    let op_id = insert_pending_op(&lib.conn(), before.id, "rename", "A/B/01.flac");
    std::fs::rename(&p, lib.path("A/B/renamed.flac")).unwrap();

    let report = lib.scan().await;
    // op は conflict になり、同じトランザクションで rel_path は実在パスへ追随する（D-43）
    let t = lib.track("A/B/renamed.flac").unwrap();
    assert_eq!(t.id, before.id);
    assert!(lib.track("A/B/01.flac").is_none());
    // pending → conflict でバッジが変わるので、SSE library の変更行に入る
    assert_eq!(report.changed_ids, vec![before.id]);
    let (result, err): (String, Option<String>) = lib
        .conn()
        .query_row(
            "SELECT result, error FROM edit_ops WHERE id = ?1",
            [op_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(result, "skipped_conflict");
    assert!(err.is_some());
}

#[tokio::test]
async fn unreadable_changed_file_updates_physical_attrs_and_is_reported_as_changed() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.scan().await;
    let before = lib.track("A/B/01.flac").unwrap();
    // 壊れたファイルに差し替える（size / mtime が変わるので「変更あり」、タグは読めない）
    std::fs::write(&p, b"not a flac file at all").unwrap();

    let report = lib.scan().await;
    assert_eq!(report.errors, 1);
    let t = lib.track("A/B/01.flac").unwrap();
    assert_eq!(t.id, before.id);
    assert_ne!(t.size, before.size, "物理属性は更新される");
    assert_eq!(
        t.tag_version, before.tag_version,
        "タグは読めなかったので据え置き"
    );
    assert_eq!(
        report.changed_ids,
        vec![before.id],
        "表示値が変わったので通知対象"
    );
}

// ---------------------------------------------------------------- deep scan / tmp 回収

#[tokio::test]
async fn deep_scan_recomputes_hashes_that_incremental_skips() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.scan().await;
    let good = lib.track("A/B/01.flac").unwrap();
    lib.conn()
        .execute(
            "UPDATE tracks SET tag_hash = zeroblob(32), audio_md5 = zeroblob(16) WHERE id = ?1",
            [good.id],
        )
        .unwrap();
    let r = lib.scan().await;
    assert_eq!(r.unchanged, 1);
    assert_eq!(
        lib.track("A/B/01.flac").unwrap().tag_hash,
        Some(vec![0u8; 32])
    );

    let r = lib.scan_kind(ScanKind::Deep).await;
    assert_eq!(r.updated, 1);
    let kind: String = lib
        .conn()
        .query_row(
            "SELECT kind FROM scan_runs WHERE id = ?1",
            [r.run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kind, "deep");
    let fixed = lib.track("A/B/01.flac").unwrap();
    assert_eq!(fixed.tag_hash, good.tag_hash);
    assert_eq!(fixed.audio_md5, good.audio_md5);
    // deep は stat に見えない変更（rollback 等）を拾うためのもの。実差分があれば版を進める
    assert_eq!(fixed.tag_version, good.tag_version + 1);
    assert_eq!(fixed.audio_version, good.audio_version + 1);
}

#[tokio::test]
async fn stale_tmp_files_are_removed_but_fresh_ones_kept() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    let old = lib.path("A/B/.spindle-tmp-old");
    let fresh = lib.path("A/B/.spindle-tmp-fresh");
    std::fs::write(&old, b"x").unwrap();
    std::fs::write(&fresh, b"x").unwrap();
    set_mtime(&old, SystemTime::now() - Duration::from_secs(2 * 3600));
    let r = lib.scan().await;
    assert_eq!(r.tmp_removed, 1);
    assert!(!old.exists());
    assert!(fresh.exists());
    assert_eq!(lib.track_count(), 1);
}

// ---------------------------------------------------------------- 性能（参照機で手動実行）

/// 合成ツリー 1 万件（FLAC 8 割 / Opus 2 割、アルバム 12 曲）。1 回目と 2 回目の時間を出す。
/// `cargo test --release --test scanner -- --ignored --nocapture perf_10k`
#[tokio::test]
#[ignore = "1 万件の合成ツリー。参照機で手動実行して D-38 に記録する"]
async fn perf_10k_synthetic_tree() {
    let lib = Lib::new();
    let seed_dir = tempfile::tempdir().unwrap();
    let flac = require_ffmpeg!(common::make_audio(seed_dir.path(), "s.flac", "flac", 1));
    let opus = common::make_audio(seed_dir.path(), "s.opus", "opus", 2).unwrap();
    let total = 10_000;
    let per_album = 12;
    for i in 0..total {
        let album = i / per_album;
        let ext = if i % 5 == 4 { "opus" } else { "flac" };
        let rel = format!(
            "Cat{}/Artist{}/Album{album}/{:02}.{ext}",
            album % 3,
            album % 50,
            i % per_album + 1
        );
        let p = lib.path(&rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::copy(if ext == "opus" { &opus } else { &flac }, &p).unwrap();
        common::set_basic_tags(
            &p,
            &format!("Track {i}"),
            "Artist",
            &format!("Album {album}"),
            "AA",
            (i % per_album + 1) as u32,
            1,
        );
    }
    let t0 = std::time::Instant::now();
    let r1 = lib.scan().await;
    let first = t0.elapsed();
    let t1 = std::time::Instant::now();
    let r2 = lib.scan().await;
    let second = t1.elapsed();
    eprintln!(
        "1 回目: {first:?} (new={}), 2 回目: {second:?} (unchanged={})",
        r1.new, r2.unchanged
    );
    assert_eq!(r1.new as usize, total);
    assert_eq!(r2.unchanged as usize, total);
    assert!(second < first / 3, "2 回目は 1 回目より大幅に速いこと");
}

// ---------------------------------------------------------------- レビュー指摘（P0-6）

#[tokio::test]
async fn move_onto_a_path_still_held_by_a_missing_row_swaps_keys_in_one_transaction() {
    // B が missing のまま key を占有している所へ A が外部 rename される
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/B/01.flac", 1, "one", "B", 1));
    lib.add("A/B/02.flac", 2, "two", "B", 2);
    lib.scan().await;
    let one = lib.track("A/B/01.flac").unwrap();
    let two = lib.track("A/B/02.flac").unwrap();

    // 02 を消して missing にする
    let stash = lib.dir.path().join("stash.flac");
    std::fs::rename(lib.path("A/B/02.flac"), &stash).unwrap();
    lib.scan().await;
    assert!(lib.track("A/B/02.flac").unwrap().missing_since.is_some());

    // 01 を 02 の名前へ rename（宛先 key は missing 行 two が占有中）
    std::fs::rename(lib.path("A/B/01.flac"), lib.path("A/B/02.flac")).unwrap();
    let r = lib.scan().await;
    assert_eq!(lib.run_state(r.run_id), "completed");
    let now_at_02 = lib.track("A/B/02.flac").unwrap();
    assert_eq!(
        now_at_02.id, one.id,
        "inode で one と判定され、key を two から奪う"
    );
    assert!(now_at_02.missing_since.is_none());
    // two は missing のまま行が残る（rel_path は明け渡している）
    let two_now: (Option<i64>, String) = lib
        .conn()
        .query_row(
            "SELECT missing_since, rel_path FROM tracks WHERE id = ?1",
            [two.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(two_now.0.is_some());
    assert_ne!(two_now.1, "A/B/02.flac");
    assert_eq!(lib.track_count(), 2);

    // active 行が占有している場合（inode で別行に判定される swap の片側だけ）も同じ
    // → 02 を 03 に、stash を 02 に戻す。stash は two（md5 で復活）、01 由来は 03
    std::fs::rename(lib.path("A/B/02.flac"), lib.path("A/B/03.flac")).unwrap();
    std::fs::rename(&stash, lib.path("A/B/02.flac")).unwrap();
    let r = lib.scan().await;
    assert_eq!(lib.run_state(r.run_id), "completed");
    assert_eq!(lib.track("A/B/03.flac").unwrap().id, one.id);
    let revived = lib.track("A/B/02.flac").unwrap();
    assert_eq!(revived.id, two.id);
    assert!(revived.missing_since.is_none());
}

#[tokio::test]
async fn duplicate_keys_in_inventory_are_skipped_not_inserted_twice() {
    // 大小文字だけ違う 2 ファイル（case-sensitive な FS でのみ起きる）。key が同じなので
    // 2 つ目は対象外にする（UNIQUE を踏まない）
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/B/Song.flac", 1, "one", "B", 1));
    lib.add("A/B/song.flac", 2, "two", "B", 2);
    if !lib.path("A/B/song.flac").exists() || lib.track_count() != 0 {
        return;
    }
    let r = lib.scan().await;
    assert_eq!(lib.run_state(r.run_id), "completed");
    assert_eq!(lib.track_count(), 1);
    assert_eq!(r.skipped.len(), 1);
    assert_eq!(r.skipped[0].reason, SkipReason::DuplicateKey);
}

#[tokio::test]
async fn pending_op_created_between_snapshot_and_commit_is_respected() {
    // Phase 2 のスナップショット後、Phase 4 の前に編集バッチが pending op と overlay を作る
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.scan().await;
    let before = lib.track("A/B/01.flac").unwrap();
    // 外部でタグを変えて Phase 3 の仕事を作る
    common::retag(&p, |tag| tag.set_title("external".to_owned()));

    let db_path = lib.db_path.clone();
    let id = before.id;
    let inserted = Arc::new(AtomicU64::new(0));
    let ins = inserted.clone();
    // progress(Read, 0, total) は Phase 2 の後・Phase 3 の前に呼ばれる
    let progress = Arc::new(move |phase, done: u64, _total: u64| {
        if phase == ScanPhase::Read && done == 0 && ins.fetch_add(1, Ordering::SeqCst) == 0 {
            let c = Connection::open(&db_path).unwrap();
            insert_pending_op(&c, id, "tags", "A/B/01.flac");
            c.execute(
                "UPDATE tracks SET title = 'edited', tag_version = tag_version + 1 WHERE id = ?1",
                [id],
            )
            .unwrap();
            c.execute(
                "UPDATE track_tags SET value = 'edited' WHERE track_id = ?1 AND key = 'TITLE'",
                [id],
            )
            .unwrap();
        }
    });
    lib.scanner
        .run(ScanKind::Incremental, progress, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(inserted.load(Ordering::SeqCst), 1);
    let t = lib.track("A/B/01.flac").unwrap();
    assert_eq!(
        t.title.as_deref(),
        Some("edited"),
        "commit 時点の pending を見る"
    );
    assert_eq!(t.tag_version, before.tag_version + 1);
    assert_eq!(lib.tag_values(t.id, "TITLE"), ["edited"]);
    assert_ne!(t.size, before.size);
}

#[tokio::test]
async fn switching_alac_and_aac_at_the_same_path_bumps_audio_version() {
    let lib = Lib::new();
    let dir = tempfile::tempdir().unwrap();
    let alac = require_ffmpeg!(common::make_audio(dir.path(), "a.m4a", "alac.m4a", 1));
    let aac = common::make_audio(dir.path(), "b.m4a", "m4a", 1).unwrap();
    for p in [&alac, &aac] {
        common::set_basic_tags(p, "t", "Artist", "B", "AlbumArtist", 1, 1);
    }
    std::fs::create_dir_all(lib.path("A/B")).unwrap();
    std::fs::copy(&alac, lib.path("A/B/01.m4a")).unwrap();
    lib.scan().await;
    let t = lib.track("A/B/01.m4a").unwrap();
    assert_eq!(t.codec, "alac");
    assert!(t.audio_md5.is_some());

    std::fs::copy(&aac, lib.path("A/B/01.m4a")).unwrap();
    lib.scan().await;
    let t = lib.track("A/B/01.m4a").unwrap();
    assert_eq!(t.codec, "aac");
    assert!(t.audio_md5.is_none() && t.audio_fp.is_some());
    assert_eq!(t.audio_version, 2, "可逆 → 非可逆は音声の変化");

    std::fs::copy(&alac, lib.path("A/B/01.m4a")).unwrap();
    lib.scan().await;
    let t = lib.track("A/B/01.m4a").unwrap();
    assert_eq!(t.codec, "alac");
    assert_eq!(t.audio_version, 3, "非可逆 → 可逆も音声の変化");
}

#[tokio::test]
async fn track_moved_to_root_loses_its_album_and_empty_album_goes_missing() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.scan().await;
    let (album, _, _) = lib.album_of_dir("A/B").unwrap();
    std::fs::rename(lib.path("A/B/01.flac"), lib.path("01.flac")).unwrap();
    lib.scan().await;
    let t = lib.track("01.flac").unwrap();
    assert_eq!(t.album_id, None);
    assert_eq!(t.album, None);
    let (album2, _, missing) = lib.album_of_dir("A/B").unwrap();
    assert_eq!(album2, album);
    assert!(missing.is_some(), "構成 0 になった album は missing");
}

#[tokio::test]
async fn album_without_majority_in_its_directory_cannot_be_taken_by_another_directory() {
    // A/X に X の 1 曲と新規 1 曲（同数・過半数なし）、A/Y に X の 3 曲。
    // Y は X を持ち去れず新規 album、X は A/X に残る
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/X/01.flac", 1, "x1", "X", 1));
    lib.add("A/X/02.flac", 2, "x2", "X", 2);
    lib.add("A/X/03.flac", 3, "x3", "X", 3);
    lib.add("A/X/04.flac", 4, "x4", "X", 4);
    lib.scan().await;
    let (xid, _, _) = lib.album_of_dir("A/X").unwrap();

    std::fs::create_dir(lib.path("A/Y")).unwrap();
    for n in ["02", "03", "04"] {
        std::fs::rename(
            lib.path(&format!("A/X/{n}.flac")),
            lib.path(&format!("A/Y/{n}.flac")),
        )
        .unwrap();
    }
    lib.add("A/X/05.flac", 5, "new", "X", 5);
    lib.scan().await;
    assert_eq!(lib.album_of_dir("A/X").unwrap().0, xid);
    let (yid, _, _) = lib.album_of_dir("A/Y").unwrap();
    assert_ne!(yid, xid);
    assert_eq!(lib.track("A/X/01.flac").unwrap().album_id, Some(xid));
    assert_eq!(lib.track("A/X/05.flac").unwrap().album_id, Some(xid));
    assert_eq!(lib.track("A/Y/02.flac").unwrap().album_id, Some(yid));
}

/// dev の付け替え（D-62）の最速パスは `update_physical` で inode 以下も書く。Phase 2 の
/// スナップショット後に tagwrite（tmp + rename）が完了していたら、古い inventory の属性で
/// 巻き戻してはいけない（overtaken として seen だけにする）
#[tokio::test]
async fn dev_remap_fast_path_does_not_roll_back_a_tagwrite_completed_after_snapshot() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.scan().await;
    use std::os::unix::fs::MetadataExt;
    let id = lib.track("A/B/01.flac").unwrap().id;
    let old_inode = std::fs::metadata(&p).unwrap().ino();
    // 再起動で dev が振り直された状態（DB の dev だけ古い）
    lib.conn()
        .execute("UPDATE tracks SET dev = dev + 1 WHERE id = ?1", [id])
        .unwrap();

    // Phase 3 の仕事は無い（changed = false）ので、Read 相の進捗は (0, 0) が 1 回だけ来る。
    // そこで tagwrite 相当を完了させる: tmp + rename で新 inode、DB は新しい物理属性で更新済み
    let db_path = lib.db_path.clone();
    let file = p.clone();
    let fired = Arc::new(AtomicU64::new(0));
    let f = fired.clone();
    let progress = Arc::new(move |phase, _done: u64, total: u64| {
        if phase == ScanPhase::Read && total == 0 && f.fetch_add(1, Ordering::SeqCst) == 0 {
            let tmp = file.with_file_name(".spindle-tmp-test.flac");
            std::fs::copy(&file, &tmp).unwrap();
            common::retag(&tmp, |tag| tag.set_title("written".to_owned()));
            std::fs::rename(&tmp, &file).unwrap();
            let meta = std::fs::metadata(&file).unwrap();
            use std::os::unix::fs::MetadataExt;
            let c = Connection::open(&db_path).unwrap();
            c.execute(
                "UPDATE tracks SET dev = ?2, inode = ?3, size = ?4, mtime_ns = ?5, ctime_ns = ?6,
                        title = 'written', tag_hash = zeroblob(32), tag_version = tag_version + 1
                 WHERE id = ?1",
                params![
                    id,
                    meta.dev() as i64,
                    meta.ino() as i64,
                    meta.len() as i64,
                    meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
                    meta.ctime() * 1_000_000_000 + meta.ctime_nsec(),
                ],
            )
            .unwrap();
        }
    });
    let r = lib
        .scanner
        .run(ScanKind::Incremental, progress, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(fired.load(Ordering::SeqCst), 1);
    assert_eq!(r.overtaken, 1, "{r:?}");

    let meta = std::fs::metadata(&p).unwrap();
    let new_inode = meta.ino();
    assert_ne!(new_inode, old_inode);
    let (dev, inode, title, seen): (i64, i64, String, Option<i64>) = lib
        .conn()
        .query_row(
            "SELECT dev, inode, title, seen_run_id FROM tracks WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (dev, inode),
        (meta.dev() as i64, new_inode as i64),
        "tagwrite の値を巻き戻さない"
    );
    assert_eq!(title, "written");
    assert_eq!(seen, Some(r.run_id));
}

#[tokio::test]
async fn tagwrite_completed_during_phase3_is_not_rolled_back_by_commit() {
    // Phase 3 の読み取りが終わった後（progress(total, total)）に tagwrite が完了した状態を作る:
    // tmp + rename で新 inode、DB は新しい物理属性・タグ・版で更新済み、pending は applied
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.scan().await;
    let before = lib.track("A/B/01.flac").unwrap();
    // 外部変更で Phase 3 の仕事を作る（古いタグ "external" を読ませる）
    common::retag(&p, |tag| tag.set_title("external".to_owned()));

    let db_path = lib.db_path.clone();
    let file = p.clone();
    let id = before.id;
    let fired = Arc::new(AtomicU64::new(0));
    let f = fired.clone();
    let progress = Arc::new(move |phase, done: u64, total: u64| {
        if phase == ScanPhase::Read
            && total > 0
            && done == total
            && f.fetch_add(1, Ordering::SeqCst) == 0
        {
            // tagwrite 相当: tmp に書いて rename（inode が変わる）
            let tmp = file.with_file_name(".spindle-tmp-test.flac");
            std::fs::copy(&file, &tmp).unwrap();
            common::retag(&tmp, |tag| tag.set_title("written".to_owned()));
            std::fs::rename(&tmp, &file).unwrap();
            let meta = std::fs::metadata(&file).unwrap();
            use std::os::unix::fs::MetadataExt;
            let c = Connection::open(&db_path).unwrap();
            c.execute(
                "UPDATE tracks SET dev = ?2, inode = ?3, size = ?4, mtime_ns = ?5, ctime_ns = ?6,
                        title = 'written', tag_hash = zeroblob(32), tag_version = tag_version + 1
                 WHERE id = ?1",
                params![
                    id,
                    meta.dev() as i64,
                    meta.ino() as i64,
                    meta.len() as i64,
                    meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
                    meta.ctime() * 1_000_000_000 + meta.ctime_nsec(),
                ],
            )
            .unwrap();
            c.execute(
                "UPDATE track_tags SET value = 'written' WHERE track_id = ?1 AND key = 'TITLE'",
                [id],
            )
            .unwrap();
        }
    });
    let r = lib
        .scanner
        .run(ScanKind::Incremental, progress, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(fired.load(Ordering::SeqCst), 1);
    assert_eq!(r.overtaken, 1);

    let meta = std::fs::metadata(&p).unwrap();
    let (inode, title, tag_hash, tag_version, seen, missing): (
        i64,
        String,
        Vec<u8>,
        i64,
        Option<i64>,
        Option<i64>,
    ) = lib
        .conn()
        .query_row(
            "SELECT inode, title, tag_hash, tag_version, seen_run_id, missing_since
             FROM tracks WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(inode as u64, meta.ino(), "tagwrite 後の inode が残る");
    assert_eq!(title, "written", "完了済み編集を巻き戻さない");
    assert_eq!(tag_hash, vec![0u8; 32]);
    assert_eq!(tag_version, before.tag_version + 1);
    assert_eq!(seen, Some(r.run_id), "seen は更新され missing にならない");
    assert!(missing.is_none());
    assert_eq!(lib.tag_values(id, "TITLE"), ["written"]);

    // 次回スキャンは DB とファイルが一致しているので最速パス
    let r2 = lib.scan().await;
    assert_eq!(r2.unchanged, 1);
}

#[tokio::test]
async fn rename_job_completed_during_phase3_keeps_its_album_assignment() {
    // Phase 3 の後に rename ジョブが完了: ファイルは A/C へ、DB の rel_path と album も更新済み
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.add("A/B/02.flac", 2, "t2", "B", 2);
    lib.scan().await;
    let before = lib.track("A/B/01.flac").unwrap();
    let (album_b, _, _) = lib.album_of_dir("A/B").unwrap();
    common::retag(&p, |tag| tag.set_title("external".to_owned())); // Phase 3 の仕事

    let db_path = lib.db_path.clone();
    let lib_dir = lib.lib();
    let id = before.id;
    let progress = Arc::new(move |phase, done: u64, total: u64| {
        if phase == ScanPhase::Read && total > 0 && done == total {
            std::fs::create_dir_all(lib_dir.join("A/C")).unwrap();
            std::fs::rename(lib_dir.join("A/B/01.flac"), lib_dir.join("A/C/01.flac")).unwrap();
            let c = Connection::open(&db_path).unwrap();
            c.execute(
                "INSERT INTO albums (id, rel_dir, rel_dir_key, album) VALUES (99, 'A/C', 'a/c', 'C')",
                [],
            )
            .unwrap();
            c.execute(
                "UPDATE tracks SET rel_path = 'A/C/01.flac', rel_path_key = 'a/c/01.flac',
                        album_id = 99, album = 'C' WHERE id = ?1",
                [id],
            )
            .unwrap();
        }
    });
    let r = lib
        .scanner
        .run(ScanKind::Incremental, progress, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(r.overtaken, 1);
    let t = lib.track("A/C/01.flac").unwrap();
    assert_eq!(t.id, id);
    assert_eq!(
        t.album_id,
        Some(99),
        "完了済み rename の album 所属を戻さない"
    );
    assert_eq!(t.album.as_deref(), Some("C"));
    assert!(t.missing_since.is_none());
    // A/B には 02 が残るので album B は生きている
    assert!(lib.album_of_dir("A/B").unwrap().2.is_none());
    assert_eq!(lib.track("A/B/02.flac").unwrap().album_id, Some(album_b));
}

#[tokio::test]
async fn pending_rename_overlay_keeps_path_but_follows_physical_state_and_tags() {
    // pending の rename op が DB の rel_path を先行更新（overlay）している。ファイルは旧パスのまま
    // 外部でタグが変わった → rel_path は overlay のまま、物理属性とタグは追随、op は pending のまま
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.scan().await;
    let before = lib.track("A/B/01.flac").unwrap();
    let op_id = {
        let c = lib.conn();
        let op = insert_pending_op(&c, before.id, "rename", "A/B/01.flac");
        c.execute(
            "UPDATE tracks SET rel_path = 'A/B/01 Renamed.flac', rel_path_key = 'a/b/01 renamed.flac'
             WHERE id = ?1",
            [before.id],
        )
        .unwrap();
        op
    };
    common::retag(&p, |tag| tag.set_title("external".to_owned()));
    let r = lib.scan().await;
    assert_eq!(r.overtaken, 0);
    let t = lib.track("A/B/01 Renamed.flac").unwrap();
    assert_eq!(t.id, before.id);
    assert_eq!(
        t.title.as_deref(),
        Some("external"),
        "rename op はタグを所有しない"
    );
    assert_ne!(t.size, before.size, "物理属性は追随する");
    assert!(t.missing_since.is_none());
    let result: String = lib
        .conn()
        .query_row("SELECT result FROM edit_ops WHERE id = ?1", [op_id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        result, "pending",
        "ファイルは expected_rel_path にあるので衝突ではない"
    );
}

#[tokio::test]
async fn pending_rename_overlay_conflicts_when_file_moved_away_from_expected_path() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.scan().await;
    let before = lib.track("A/B/01.flac").unwrap();
    let op_id = {
        let c = lib.conn();
        let op = insert_pending_op(&c, before.id, "rename", "A/B/01.flac");
        c.execute(
            "UPDATE tracks SET rel_path = 'A/B/01 Renamed.flac', rel_path_key = 'a/b/01 renamed.flac'
             WHERE id = ?1",
            [before.id],
        )
        .unwrap();
        op
    };
    std::fs::rename(&p, lib.path("A/B/elsewhere.flac")).unwrap();
    lib.scan().await;
    // op は conflict になり、overlay の最終名ではなく実在パスへ追随する（D-43）
    let t = lib.track("A/B/elsewhere.flac").unwrap();
    assert_eq!(t.id, before.id);
    assert!(lib.track("A/B/01 Renamed.flac").is_none());
    let result: String = lib
        .conn()
        .query_row("SELECT result FROM edit_ops WHERE id = ?1", [op_id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(result, "skipped_conflict");
}

#[tokio::test]
async fn pending_rename_created_after_snapshot_is_handled_at_commit() {
    // Phase 2 の後に rename バッチが pending op + overlay を作る（ファイルは旧パスのまま、
    // Phase 3 の仕事はタグ変更）→ overtaken ではなく、物理属性とタグは追随、rel_path は overlay
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/B/01.flac", 1, "t", "B", 1));
    lib.scan().await;
    let before = lib.track("A/B/01.flac").unwrap();
    common::retag(&p, |tag| tag.set_title("external".to_owned()));
    let db_path = lib.db_path.clone();
    let id = before.id;
    let progress = Arc::new(move |phase, done: u64, _total: u64| {
        if phase == ScanPhase::Read && done == 0 {
            let c = Connection::open(&db_path).unwrap();
            if c.query_row("SELECT count(*) FROM edit_ops", [], |r| r.get::<_, i64>(0))
                .unwrap()
                == 0
            {
                insert_pending_op(&c, id, "rename", "A/B/01.flac");
                c.execute(
                    "UPDATE tracks SET rel_path = 'A/B/01 Renamed.flac', rel_path_key = 'a/b/01 renamed.flac'
                     WHERE id = ?1",
                    [id],
                )
                .unwrap();
            }
        }
    });
    let r = lib
        .scanner
        .run(ScanKind::Incremental, progress, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(r.overtaken, 0);
    let t = lib.track("A/B/01 Renamed.flac").unwrap();
    assert_eq!(t.id, id);
    assert_eq!(t.title.as_deref(), Some("external"));
    assert_ne!(t.size, before.size);
    let pending: i64 = lib
        .conn()
        .query_row(
            "SELECT count(*) FROM edit_ops WHERE result = 'pending'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending, 1);
}

// ---------------------------------------------------------------- Phase 2 の md5（P1-0）

/// 進捗の `(0, total)` は Phase 2（md5 の並列計算。要求があるときだけ）と Phase 3 の開始で出る。
/// 記録して、Phase 2 が走ったかを数える
type ProgressLog = Arc<std::sync::Mutex<Vec<(ScanPhase, u64, u64)>>>;

fn recording_progress() -> (ProgressLog, spindle::import::scanner::Progress) {
    let log: ProgressLog = Arc::default();
    let l = log.clone();
    let progress: spindle::import::scanner::Progress = Arc::new(move |phase, done, total| {
        l.lock().unwrap().push((phase, done, total));
    });
    (log, progress)
}

/// 各相の開始 `(phase, total)`
fn phase_starts(log: &[(ScanPhase, u64, u64)]) -> Vec<(ScanPhase, u64)> {
    log.iter()
        .filter(|(_, done, _)| *done == 0)
        .map(|(phase, _, total)| (*phase, *total))
        .collect()
}

#[tokio::test]
async fn initial_scan_computes_no_md5_in_phase2() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a", "A", 1));
    lib.add("A/02.flac", 2, "b", "A", 2).unwrap();
    lib.add("B/01.flac", 3, "c", "B", 1).unwrap();
    let (log, progress) = recording_progress();
    let report = lib
        .scanner
        .run(ScanKind::Incremental, progress, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.new, 3);
    // Phase 3 の開始だけ（Phase 2 の要求は 0 件なので進捗を出さない）
    assert_eq!(
        phase_starts(&log.lock().unwrap()),
        vec![(ScanPhase::Read, 3)]
    );
    // 変更の無い増分スキャンも同じ
    let (log, progress) = recording_progress();
    lib.scanner
        .run(ScanKind::Incremental, progress, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        phase_starts(&log.lock().unwrap()),
        vec![(ScanPhase::Read, 0)]
    );
}

#[tokio::test]
async fn move_candidate_triggers_parallel_md5_with_progress() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/01.flac", 1, "a", "A", 1));
    lib.add("A/02.flac", 2, "b", "A", 2).unwrap();
    lib.scan().await;
    let before = lib.track("A/01.flac").unwrap();
    // コピー + 削除（inode が変わる。旧 key が消えるので md5 の移動候補になる）と新規 1 本
    std::fs::create_dir_all(lib.lib().join("C")).unwrap();
    std::fs::copy(&p, lib.lib().join("C/01.flac")).unwrap();
    std::fs::remove_file(&p).unwrap();
    lib.add("C/03.flac", 3, "d", "C", 3).unwrap();
    let (log, progress) = recording_progress();
    let report = lib
        .scanner
        .run(ScanKind::Incremental, progress, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.moved, 1);
    assert_eq!(report.new, 1);
    // Phase 2 は未決の 2 本（コピー先と新規）の md5 を要求し、Phase 3 はその 2 本を読む
    let starts = phase_starts(&log.lock().unwrap());
    assert_eq!(
        starts,
        vec![(ScanPhase::Md5, 2), (ScanPhase::Read, 2)],
        "{:?}",
        log.lock().unwrap()
    );
    let log = log.lock().unwrap();
    assert!(log.contains(&(ScanPhase::Md5, 2, 2)));
    let moved = lib.track("C/01.flac").unwrap();
    assert_eq!(moved.id, before.id);
}

#[tokio::test]
async fn cancel_during_phase2_drains_running_md5_work_before_returning() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("A/01.flac", 1, "a", "A", 1));
    lib.scan().await;
    // 移動候補を作り、未決の新規を並列度より多く用意する（Md5 相が複数の仕事を持つ）
    std::fs::create_dir_all(lib.lib().join("C")).unwrap();
    std::fs::copy(&p, lib.lib().join("C/01.flac")).unwrap();
    std::fs::remove_file(&p).unwrap();
    for n in 2..=9 {
        lib.add(&format!("C/{n:02}.flac"), n, "x", "C", n).unwrap();
    }
    let (log, progress) = recording_progress();
    let token = CancellationToken::new();
    let t = token.clone();
    let progress: spindle::import::scanner::Progress = Arc::new(move |phase, done, total| {
        progress(phase, done, total);
        // Md5 相の最初の完了で cancel
        if phase == ScanPhase::Md5 && done == 1 {
            t.cancel();
        }
    });
    let err = lib
        .scanner
        .run(ScanKind::Incremental, progress, token)
        .await
        .unwrap_err();
    assert!(
        matches!(err, spindle::import::scanner::ScanError::Cancelled),
        "{err:?}"
    );
    // 返った時点で起動済みの仕事は終わっている: その後に進捗は増えない
    let n = log.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(log.lock().unwrap().len(), n);
    assert!(log
        .lock()
        .unwrap()
        .iter()
        .all(|(phase, _, _)| *phase == ScanPhase::Md5));
    let state: String = lib
        .conn()
        .query_row(
            "SELECT state FROM scan_runs ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "cancelled");
    assert!(lib.track("C/01.flac").is_none(), "commit していない");
}

#[tokio::test]
async fn rip_log_marks_new_rows_as_cd_rip_but_not_existing_rows() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, "a", "A", 1));
    lib.scan().await;
    let src = |rel: &str| -> String {
        lib.conn()
            .query_row(
                "SELECT source_type FROM tracks WHERE rel_path = ?1",
                [rel],
                |r| r.get(0),
            )
            .unwrap()
    };
    // 既存行のあるディレクトリに後から rip.log が置かれても既存行は触らない（D-67）
    std::fs::write(lib.path("A/rip.log"), "spindle rip log v1\nドライブ: x\n").unwrap();
    lib.add("A/02.flac", 2, "b", "A", 2);
    lib.add("B/01.flac", 3, "c", "B", 1);
    std::fs::write(lib.path("B/rip2.log"), "spindle rip log v1\n").unwrap();
    lib.add("C/01.flac", 4, "d", "C", 1);
    // 他ツールのログは spindle の署名が無いので対象外
    std::fs::write(lib.path("C/rip.log"), "Exact Audio Copy V1.6\n").unwrap();
    lib.scan().await;
    assert_eq!(src("A/01.flac"), "unknown");
    assert_eq!(src("A/02.flac"), "cd_rip");
    assert_eq!(src("B/01.flac"), "cd_rip");
    assert_eq!(src("C/01.flac"), "unknown");
    // 再スキャンで変わらない
    lib.scan().await;
    assert_eq!(src("A/01.flac"), "unknown");
    assert_eq!(src("A/02.flac"), "cd_rip");
}

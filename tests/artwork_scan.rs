//! スキャンの Phase 5（アートワークの解決。SPEC §7.1、docs/TASKS.md P1-3、D-49）。
//! 同梱カバー画像 → 最初のトラックの埋め込み画像の順で album のアートワークを決め、原画像を
//! ハッシュアドレスのキャッシュへ置き、thumbnail ジョブを投入する。合成ファイルは ffmpeg で作る
//! （無ければ skip）。画像はヘッダだけの最小 JPEG / PNG（寸法が読めればよい）

#![cfg(target_os = "linux")]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension};
use tokio_util::sync::CancellationToken;

use spindle::db::Db;
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, ScanReport, Scanner};
use spindle::media::artwork::ArtworkStore;

/// 1x1 の JPEG。`tag` で内容を変えられる（末尾のコメントセグメント）
fn jpeg(tag: &[u8]) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8];
    v.extend_from_slice(&[
        0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01, 0x01, 0x01, 0x11, 0x00,
    ]);
    // COM セグメント
    let len = (tag.len() + 2) as u16;
    v.extend_from_slice(&[0xFF, 0xFE]);
    v.extend_from_slice(&len.to_be_bytes());
    v.extend_from_slice(tag);
    v.extend_from_slice(&[0xFF, 0xD9]);
    v
}

/// 2x3 の PNG
fn png() -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    v.extend_from_slice(&[0x00, 0x00, 0x00, 0x0D]);
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&[0, 0, 0, 2, 0, 0, 0, 3, 8, 2, 0, 0, 0]);
    v.extend_from_slice(&[0, 0, 0, 0]);
    v
}

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    db: Arc<Db>,
    root: Arc<RootDir>,
    store: Arc<ArtworkStore>,
    scanner: Scanner,
}

#[derive(Debug, Clone, PartialEq)]
struct AlbumArt {
    artwork_id: Option<i64>,
    cover_size: Option<i64>,
    resolved_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
struct Art {
    id: i64,
    sha256: Vec<u8>,
    mime: String,
    width: Option<i64>,
    height: Option<i64>,
    bytes: i64,
    origin: String,
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Library");
        std::fs::create_dir(&lib).unwrap();
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let root = Arc::new(RootDir::open(&lib).unwrap());
        let store = Arc::new(ArtworkStore::new(dir.path().join("thumbs")));
        let scanner = Scanner::new(db.clone(), root.clone(), 4).with_artwork(store.clone());
        Self {
            dir,
            db_path,
            db,
            root,
            store,
            scanner,
        }
    }

    fn lib(&self) -> PathBuf {
        self.dir.path().join("Library")
    }

    fn add(&self, rel: &str, seed: u32, track: u32, picture: Option<Vec<u8>>) -> Option<PathBuf> {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let ext = rel.rsplit_once('.').unwrap().1;
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        let made = common::make_audio(p.parent().unwrap(), &name, ext, seed)?;
        let album = p
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        common::set_basic_tags(
            &made,
            &format!("t{track}"),
            "Artist",
            &album,
            "Artist",
            track,
            1,
        );
        if let Some(bytes) = picture {
            set_picture(&made, bytes);
        }
        Some(made)
    }

    fn write_cover(&self, rel: &str, bytes: &[u8]) {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
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

    fn album(&self, rel_dir: &str) -> AlbumArt {
        self.conn()
            .query_row(
                "SELECT artwork_id, cover_size, artwork_resolved_at FROM albums WHERE rel_dir = ?1",
                [rel_dir],
                |r| {
                    Ok(AlbumArt {
                        artwork_id: r.get(0)?,
                        cover_size: r.get(1)?,
                        resolved_at: r.get(2)?,
                    })
                },
            )
            .unwrap()
    }

    fn art(&self, id: i64) -> Art {
        self.conn()
            .query_row(
                "SELECT id, sha256, mime, width, height, bytes, origin FROM artwork WHERE id = ?1",
                [id],
                |r| {
                    Ok(Art {
                        id: r.get(0)?,
                        sha256: r.get(1)?,
                        mime: r.get(2)?,
                        width: r.get(3)?,
                        height: r.get(4)?,
                        bytes: r.get(5)?,
                        origin: r.get(6)?,
                    })
                },
            )
            .unwrap()
    }

    fn art_of(&self, rel_dir: &str) -> Option<Art> {
        self.album(rel_dir).artwork_id.map(|id| self.art(id))
    }

    fn artwork_count(&self) -> i64 {
        self.conn()
            .query_row("SELECT count(*) FROM artwork", [], |r| r.get(0))
            .unwrap()
    }

    fn thumbnail_jobs(&self) -> Vec<String> {
        let c = self.conn();
        let mut st = c
            .prepare("SELECT dedup_key FROM jobs WHERE type = 'thumbnail' ORDER BY id")
            .unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }
}

/// lofty で埋め込み画像を 1 枚にする
fn set_picture(path: &Path, bytes: Vec<u8>) {
    use lofty::picture::{MimeType, Picture, PictureType};
    let pic = Picture::unchecked(bytes)
        .pic_type(PictureType::CoverFront)
        .mime_type(MimeType::Jpeg)
        .build();
    common::retag(path, |t| {
        while !t.pictures().is_empty() {
            t.remove_picture(0);
        }
        t.push_picture(pic);
    });
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------- 解決の規則

#[tokio::test]
async fn cover_file_wins_over_embedded_and_is_cached_with_thumbnail_job() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, 1, Some(png())));
    let cover = jpeg(b"A");
    lib.write_cover("A/cover.jpg", &cover);
    let report = lib.scan().await;
    assert_eq!(report.artwork_resolved, 1);
    assert_eq!(report.enqueued_jobs.len(), 1);

    let a = lib.album("A");
    assert_eq!(a.cover_size, Some(cover.len() as i64));
    assert!(a.resolved_at.is_some());
    let art = lib.art_of("A").unwrap();
    assert_eq!(art.origin, "file");
    assert_eq!(art.mime, "image/jpeg");
    assert_eq!(
        (art.width, art.height),
        (Some(1), Some(1)),
        "DB の列は NULL 許容"
    );
    assert_eq!(art.bytes, cover.len() as i64);
    assert_eq!(art.sha256, ArtworkStore::hash_of(&cover).to_vec());
    // 原画像がハッシュアドレスで置かれ、サムネイルはまだ無い
    let orig = lib.store.original_path(&art.sha256, "image/jpeg");
    assert_eq!(
        orig,
        lib.dir
            .path()
            .join("thumbs")
            .join(hex(&art.sha256))
            .join("orig.jpg")
    );
    assert_eq!(std::fs::read(&orig).unwrap(), cover);
    assert_eq!(lib.thumbnail_jobs(), vec![format!("thumbnail:{}", art.id)]);

    // 変化が無ければ次のスキャンでは解決し直さない
    let report = lib.scan().await;
    assert_eq!(report.artwork_resolved, 0);
    assert!(report.enqueued_jobs.is_empty());
    assert_eq!(lib.artwork_count(), 1);
}

#[tokio::test]
async fn embedded_picture_of_first_track_when_no_cover_file() {
    let lib = Lib::new();
    // track 2 のファイル名が先でも track_no 順で 1 → 2。1 には画像が無いので 2 の画像
    require_ffmpeg!(lib.add("A/b.flac", 1, 1, None));
    lib.add("A/a.flac", 2, 2, Some(jpeg(b"two"))).unwrap();
    lib.scan().await;
    let art = lib.art_of("A").unwrap();
    assert_eq!(art.origin, "embedded");
    assert_eq!(art.sha256, ArtworkStore::hash_of(&jpeg(b"two")).to_vec());
    assert_eq!(lib.album("A").cover_size, None);

    // 1 にも画像が付いたら（タグ変更 → 行が変わる）1 の画像に切り替わる
    set_picture(&lib.lib().join("A/b.flac"), jpeg(b"one"));
    let report = lib.scan().await;
    assert_eq!(report.artwork_resolved, 1);
    let art = lib.art_of("A").unwrap();
    assert_eq!(art.sha256, ArtworkStore::hash_of(&jpeg(b"one")).to_vec());
    assert_eq!(lib.artwork_count(), 2, "古い画像の行は残る");
}

#[tokio::test]
async fn album_without_any_picture_is_resolved_to_null_once() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, 1, None));
    let report = lib.scan().await;
    assert_eq!(report.artwork_resolved, 1);
    let a = lib.album("A");
    assert_eq!(a.artwork_id, None);
    assert!(a.resolved_at.is_some());
    assert!(lib.thumbnail_jobs().is_empty());
    let report = lib.scan().await;
    assert_eq!(report.artwork_resolved, 0);
}

#[tokio::test]
async fn cover_name_priority_is_name_then_extension() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, 1, None));
    lib.write_cover("A/folder.jpg", &jpeg(b"folder"));
    lib.write_cover("A/cover.png", &png());
    lib.write_cover("A/Front.jpg", &jpeg(b"front"));
    lib.scan().await;
    let art = lib.art_of("A").unwrap();
    assert_eq!(art.mime, "image/png", "cover.* が folder.* より優先");
    assert_eq!((art.width, art.height), (Some(2), Some(3)));
}

// ---------------------------------------------------------------- 変更検出

#[tokio::test]
async fn cover_change_is_detected_without_track_changes() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, 1, Some(jpeg(b"embedded"))));
    lib.write_cover("A/cover.jpg", &jpeg(b"v1"));
    lib.scan().await;
    let first = lib.art_of("A").unwrap();
    assert_eq!(first.origin, "file");

    // 差し替え（内容が変わるので size / mtime が動く）
    lib.write_cover("A/cover.jpg", &jpeg(b"v2-longer"));
    let report = lib.scan().await;
    assert_eq!(report.artwork_resolved, 1);
    assert_eq!(report.unchanged, 1, "トラックは最速パス");
    let second = lib.art_of("A").unwrap();
    assert_ne!(second.id, first.id);
    assert_eq!(
        second.sha256,
        ArtworkStore::hash_of(&jpeg(b"v2-longer")).to_vec()
    );
    assert_eq!(lib.thumbnail_jobs().len(), 2);

    // 消えたら埋め込みへ戻る
    std::fs::remove_file(lib.lib().join("A/cover.jpg")).unwrap();
    let report = lib.scan().await;
    assert_eq!(report.artwork_resolved, 1);
    let third = lib.art_of("A").unwrap();
    assert_eq!(third.origin, "embedded");
    assert_eq!(lib.album("A").cover_size, None);

    // 何も変わらなければ触らない
    assert_eq!(lib.scan().await.artwork_resolved, 0);
}

#[tokio::test]
async fn unresolved_albums_are_resolved_on_next_scan_even_without_changes() {
    // アートワーク無しのスキャナで作った DB（マイグレーション直後の既存 album と同じ状態）
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, 1, None));
    lib.write_cover("A/cover.jpg", &jpeg(b"A"));
    let plain = Scanner::new(lib.db.clone(), lib.root.clone(), 2);
    plain
        .run(
            ScanKind::Incremental,
            Arc::new(|_, _, _| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(lib.album("A").resolved_at, None);

    let report = lib.scan().await;
    assert_eq!(report.unchanged, 1);
    assert_eq!(report.artwork_resolved, 1);
    assert!(lib.art_of("A").is_some());
}

#[tokio::test]
async fn deep_scan_resolves_every_album() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, 1, None));
    lib.add("B/01.flac", 2, 1, None).unwrap();
    lib.write_cover("A/cover.jpg", &jpeg(b"A"));
    lib.scan().await;
    let report = lib.scan_kind(ScanKind::Deep).await;
    assert_eq!(report.artwork_resolved, 2);
}

#[tokio::test]
async fn same_image_in_two_albums_is_one_artwork_row_and_one_job() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, 1, None));
    lib.add("B/01.flac", 2, 1, None).unwrap();
    lib.write_cover("A/cover.jpg", &jpeg(b"same"));
    lib.write_cover("B/cover.jpg", &jpeg(b"same"));
    let report = lib.scan().await;
    assert_eq!(report.artwork_resolved, 2);
    assert_eq!(lib.artwork_count(), 1);
    assert_eq!(lib.album("A").artwork_id, lib.album("B").artwork_id);
    assert_eq!(lib.thumbnail_jobs().len(), 1);
}

#[tokio::test]
async fn unreadable_cover_falls_back_to_embedded_and_is_not_reread_until_it_changes() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, 1, Some(jpeg(b"embedded"))));
    lib.write_cover("A/cover.jpg", b"this is not an image");
    let report = lib.scan().await;
    assert_eq!(report.artwork_resolved, 1);
    let art = lib.art_of("A").unwrap();
    assert_eq!(art.origin, "embedded");
    // stat は記録するので、変わらない限り読み直さない
    assert_eq!(lib.album("A").cover_size, Some(20));
    assert_eq!(lib.scan().await.artwork_resolved, 0);
}

#[tokio::test]
async fn missing_album_is_skipped() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, 1, None));
    lib.write_cover("A/cover.jpg", &jpeg(b"A"));
    lib.scan().await;
    std::fs::remove_dir_all(lib.lib().join("A")).unwrap();
    let report = lib.scan().await;
    assert_eq!(report.missing_marked, 1);
    // 構成 0 になった album は missing。その run では（行が変わったので）解決に入るが、
    // 次の run では触らない
    let missing: Option<i64> = lib
        .conn()
        .query_row(
            "SELECT missing_since FROM albums WHERE rel_dir = 'A'",
            [],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
        .flatten();
    assert!(missing.is_some());
    assert_eq!(lib.scan().await.artwork_resolved, 0);
}

// ---------------------------------------------------------------- 再解決の予約と再開（D-49）

#[tokio::test]
async fn moving_the_pictured_track_re_resolves_both_albums() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, 1, Some(jpeg(b"one"))));
    lib.add("A/02.flac", 2, 2, Some(jpeg(b"two"))).unwrap();
    lib.scan().await;
    assert_eq!(
        lib.art_of("A").unwrap().sha256,
        ArtworkStore::hash_of(&jpeg(b"one")).to_vec()
    );
    // 画像付きの先頭トラックを別ディレクトリへ（分割）。A は残った 02 の画像、B は 01 の画像
    std::fs::create_dir_all(lib.lib().join("B")).unwrap();
    std::fs::rename(lib.lib().join("A/01.flac"), lib.lib().join("B/01.flac")).unwrap();
    let report = lib.scan().await;
    assert_eq!(report.moved, 1);
    assert_eq!(
        report.artwork_resolved, 2,
        "構成を失った旧 album も解決し直す"
    );
    assert_eq!(
        lib.art_of("A").unwrap().sha256,
        ArtworkStore::hash_of(&jpeg(b"two")).to_vec()
    );
    assert_eq!(
        lib.art_of("B").unwrap().sha256,
        ArtworkStore::hash_of(&jpeg(b"one")).to_vec()
    );
}

#[tokio::test]
async fn cancel_after_commit_keeps_run_completed_and_resumes_next_scan() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, 1, Some(jpeg(b"one"))));
    lib.add("B/01.flac", 2, 1, None).unwrap();
    lib.scan().await;
    // B を消し、A の画像を差し替えてから、Phase 4 の commit 後に cancel が来る状況を作る
    std::fs::remove_dir_all(lib.lib().join("B")).unwrap();
    set_picture(&lib.lib().join("A/01.flac"), jpeg(b"replaced"));
    let token = CancellationToken::new();
    let hook_token = token.clone();
    lib.scanner
        .set_before_artwork_hook(Arc::new(move |_| hook_token.cancel()));
    let report = lib
        .scanner
        .run(ScanKind::Incremental, Arc::new(|_, _, _| {}), token)
        .await
        .expect("Phase 4 は commit 済みなので run は成功で返る");
    assert_eq!(report.missing_marked, 1);
    assert_eq!(report.artwork_resolved, 0);
    let state: String = lib
        .conn()
        .query_row(
            "SELECT state FROM scan_runs WHERE id = ?1",
            [report.run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "completed");
    let missing: Option<i64> = lib
        .conn()
        .query_row(
            "SELECT missing_since FROM tracks WHERE rel_path = 'B/01.flac'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(missing.is_some(), "missing の確定は commit 済み");
    // 予約は DB に残っている。次のスキャンはトラックが unchanged でも解決する
    assert_eq!(lib.album("A").resolved_at, None);
    assert_eq!(
        lib.art_of("A").unwrap().sha256,
        ArtworkStore::hash_of(&jpeg(b"one")).to_vec(),
        "旧画像のまま"
    );
    lib.scanner.set_before_artwork_hook(Arc::new(|_| {}));
    let report = lib.scan().await;
    assert_eq!(report.unchanged, 1);
    assert_eq!(report.artwork_resolved, 1);
    assert_eq!(
        lib.art_of("A").unwrap().sha256,
        ArtworkStore::hash_of(&jpeg(b"replaced")).to_vec()
    );
    assert!(lib.album("A").resolved_at.is_some());
}

#[tokio::test]
async fn cache_write_failure_keeps_reservation_until_it_succeeds() {
    use std::os::unix::fs::PermissionsExt;
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, 1, None));
    lib.write_cover("A/cover.jpg", &jpeg(b"A"));
    // キャッシュ置き場を書けなくする
    let thumbs = lib.dir.path().join("thumbs");
    std::fs::create_dir_all(&thumbs).unwrap();
    std::fs::set_permissions(&thumbs, std::fs::Permissions::from_mode(0o500)).unwrap();
    let report = lib.scan().await;
    std::fs::set_permissions(&thumbs, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(report.artwork_resolved, 0);
    assert_eq!(
        report.artwork_error, None,
        "album 単位の失敗は run の失敗ではない"
    );
    let a = lib.album("A");
    assert_eq!(a.artwork_id, None);
    assert_eq!(a.resolved_at, None, "予約は残る");
    // 何も変えずに再スキャン → 解決される
    let report = lib.scan().await;
    assert_eq!(report.artwork_resolved, 1);
    assert!(lib.art_of("A").is_some());
}

#[tokio::test]
async fn unreadable_track_defers_resolution_and_keeps_old_artwork() {
    use std::os::unix::fs::PermissionsExt;
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, 1, None));
    lib.add("A/02.flac", 2, 2, Some(jpeg(b"two"))).unwrap();
    lib.scan().await;
    let before = lib.art_of("A").unwrap();
    // 02 の画像を差し替えて（行が変わる）、01 を読めなくする
    set_picture(&lib.lib().join("A/02.flac"), jpeg(b"new"));
    let p1 = lib.lib().join("A/01.flac");
    std::fs::set_permissions(&p1, std::fs::Permissions::from_mode(0o000)).unwrap();
    let report = lib.scan().await;
    std::fs::set_permissions(&p1, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(report.artwork_resolved, 0, "先頭が読めなければ決めない");
    assert_eq!(lib.art_of("A").unwrap().id, before.id, "旧画像を外さない");
    assert_eq!(lib.album("A").resolved_at, None);
    // 読めるようになれば解決する
    let report = lib.scan().await;
    assert_eq!(report.artwork_resolved, 1);
    assert_eq!(
        lib.art_of("A").unwrap().sha256,
        ArtworkStore::hash_of(&jpeg(b"new")).to_vec()
    );
}

#[tokio::test]
async fn lost_original_in_cache_is_restored_by_incremental_scan() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, 1, Some(jpeg(b"emb"))));
    lib.scan().await;
    let art = lib.art_of("A").unwrap();
    let orig = lib.store.original_path(&art.sha256, &art.mime);
    std::fs::remove_file(&orig).unwrap();
    let report = lib.scan().await;
    assert_eq!(report.unchanged, 1);
    assert_eq!(
        report.artwork_resolved, 1,
        "参照中の原画像が無ければ解決し直す"
    );
    assert!(orig.is_file());
    assert_eq!(lib.art_of("A").unwrap().id, art.id);
    assert_eq!(lib.scan().await.artwork_resolved, 0);
}

#[tokio::test]
async fn corrupted_original_is_repaired_by_length_on_incremental_and_by_hash_on_deep() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, 1, None));
    let cover = jpeg(b"cover");
    lib.write_cover("A/cover.jpg", &cover);
    lib.scan().await;
    let art = lib.art_of("A").unwrap();
    let orig = lib.store.original_path(&art.sha256, &art.mime);

    // 長さの違う破損 → incremental で置き直す
    std::fs::write(&orig, b"short").unwrap();
    let report = lib.scan().await;
    assert_eq!(report.artwork_resolved, 1);
    assert_eq!(std::fs::read(&orig).unwrap(), cover);
    assert_eq!(lib.art_of("A").unwrap().id, art.id);

    // 同じ長さの破損 → incremental では見えないが deep のハッシュ照合で置き直す
    let mut same_len = cover.clone();
    same_len[cover.len() - 3] ^= 0xFF;
    std::fs::write(&orig, &same_len).unwrap();
    assert_eq!(lib.scan().await.artwork_resolved, 0);
    assert_eq!(std::fs::read(&orig).unwrap(), same_len);
    let report = lib.scan_kind(ScanKind::Deep).await;
    assert_eq!(report.artwork_resolved, 1);
    assert_eq!(std::fs::read(&orig).unwrap(), cover);
}

#[tokio::test]
async fn deferred_album_during_deep_scan_is_retried_by_next_incremental() {
    use std::os::unix::fs::PermissionsExt;
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, 1, Some(jpeg(b"one"))));
    lib.scan().await;
    assert!(lib.album("A").resolved_at.is_some());
    // deep だけを理由に対象になり、構成トラックが読めない → 予約を残す
    let p1 = lib.lib().join("A/01.flac");
    std::fs::set_permissions(&p1, std::fs::Permissions::from_mode(0o000)).unwrap();
    let report = lib.scan_kind(ScanKind::Deep).await;
    std::fs::set_permissions(&p1, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(report.artwork_resolved, 0);
    assert_eq!(
        lib.album("A").resolved_at,
        None,
        "決められなかった album は予約に戻す"
    );
    assert!(lib.art_of("A").is_some(), "旧画像は外さない");
    // 次の incremental（トラックは deep で読めなかったので errors に数えられ、今回は変更あり扱いに
    // なりうる。どちらでも予約により解決される）
    let report = lib.scan().await;
    assert_eq!(report.artwork_resolved, 1);
    assert!(lib.album("A").resolved_at.is_some());
}

/// deep で unchanged の album が、Phase 4 の commit 後のどの位置で止まっても予約されたままになる
async fn deep_cancel_at(point: &'static str) {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/01.flac", 1, 1, Some(jpeg(b"one"))));
    lib.scan().await;
    assert!(lib.album("A").resolved_at.is_some());
    let token = CancellationToken::new();
    let hook_token = token.clone();
    lib.scanner.set_before_artwork_hook(Arc::new(move |p| {
        if p == point {
            hook_token.cancel();
        }
    }));
    let report = lib
        .scanner
        .run(ScanKind::Deep, Arc::new(|_, _, _| {}), token)
        .await
        .unwrap();
    assert_eq!(report.artwork_resolved, 0, "{point}");
    let state: String = lib
        .conn()
        .query_row(
            "SELECT state FROM scan_runs WHERE id = ?1",
            [report.run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "completed", "{point}");
    assert_eq!(
        lib.album("A").resolved_at,
        None,
        "{point}: deep だけを理由の候補も予約が残る"
    );
    // 次の incremental（トラックは unchanged）で解決される
    lib.scanner.set_before_artwork_hook(Arc::new(|_| {}));
    let report = lib.scan().await;
    assert_eq!(report.unchanged, 1, "{point}");
    assert_eq!(report.artwork_resolved, 1, "{point}");
    assert!(lib.album("A").resolved_at.is_some(), "{point}");
}

#[tokio::test]
async fn cancel_between_phase4_commit_and_phase5_reservation_keeps_deep_reservation() {
    deep_cancel_at("before_reserve").await;
}

#[tokio::test]
async fn cancel_after_phase5_reservation_keeps_reservation() {
    deep_cancel_at("after_reserve").await;
}

//! 埋め込み画像の差し替え（P1-3 書き側、D-60）。`PICTURE` キーの tags op として通常の
//! 編集バッチに乗る: 旧画像は書く前に `ArtworkStore` へ退避し、巻き戻しで戻る。
//! 合成ファイルは ffmpeg で作る（無ければ skip）

#![cfg(target_os = "linux")]

mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{params, Connection};
use tokio_util::sync::CancellationToken;

use spindle::db::history::{self, BatchState, OpResult};
use spindle::db::Db;
use spindle::domain::tags::read_transfer_tags;
use spindle::edit::{EditError, Editor};
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, Scanner};
use spindle::jobs::handlers::tagwrite::TagwriteHandler;
use spindle::jobs::{JobType, Jobs, Registry};
use spindle::media::artwork::ArtworkStore;

/// 1x1 の JPEG。`tag` で内容を変えられる（末尾のコメントセグメント）
fn jpeg(tag: &[u8]) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8];
    v.extend_from_slice(&[
        0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01, 0x01, 0x01, 0x11, 0x00,
    ]);
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn picture_value(mime: &str, bytes: &[u8]) -> String {
    format!("{mime}:{}", hex(&ArtworkStore::hash_of(bytes)))
}

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    db: Arc<Db>,
    jobs: Arc<Jobs>,
    store: Arc<ArtworkStore>,
    scanner: Scanner,
    editor: Arc<Editor>,
    shutdown: CancellationToken,
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Library");
        std::fs::create_dir(&lib).unwrap();
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let root = Arc::new(RootDir::open(&lib).unwrap());
        let jobs = Jobs::new(db.clone());
        let store = Arc::new(ArtworkStore::new(dir.path().join("thumbs")));
        let scanner = Scanner::new(db.clone(), root.clone(), 2).with_artwork(store.clone());
        let editor =
            Arc::new(Editor::new(db.clone(), root, jobs.clone()).with_artwork(store.clone()));
        Self {
            dir,
            db_path,
            db,
            jobs,
            store,
            scanner,
            editor,
            shutdown: CancellationToken::new(),
        }
    }

    fn start(&self) {
        let mut reg = Registry::new();
        reg.register(
            JobType::Tagwrite,
            Arc::new(TagwriteHandler::new(self.editor.clone())),
        );
        self.jobs.start(reg, self.shutdown.clone());
    }

    fn lib(&self) -> PathBuf {
        self.dir.path().join("Library")
    }

    fn add(&self, rel: &str, seed: u32, pictures: &[Vec<u8>]) -> Option<PathBuf> {
        let p = self.lib().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let ext = rel.rsplit_once('.').unwrap().1;
        let name = p.file_name().unwrap().to_str().unwrap().to_owned();
        let made = common::make_audio(p.parent().unwrap(), &name, ext, seed)?;
        common::set_basic_tags(&made, "Title", "Artist", "Album", "AlbumArtist", 1, 1);
        if !pictures.is_empty() {
            set_pictures(&made, pictures);
        }
        Some(made)
    }

    async fn scan(&self) {
        self.scanner
            .run(
                ScanKind::Incremental,
                Arc::new(|_, _, _| {}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.db_path).unwrap()
    }

    fn track_id(&self, rel: &str) -> i64 {
        self.conn()
            .query_row("SELECT id FROM tracks WHERE rel_path = ?1", [rel], |r| {
                r.get(0)
            })
            .unwrap()
    }

    fn tag_version(&self, id: i64) -> i64 {
        self.conn()
            .query_row("SELECT tag_version FROM tracks WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
    }

    fn db_tag(&self, track_id: i64, key: &str) -> Vec<String> {
        self.conn()
            .prepare("SELECT value FROM track_tags WHERE track_id = ?1 AND key = ?2 ORDER BY idx")
            .unwrap()
            .query_map(params![track_id, key], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn album_resolved(&self, track_id: i64) -> bool {
        self.conn()
            .query_row(
                "SELECT a.artwork_resolved_at IS NOT NULL FROM albums a
                   JOIN tracks t ON t.album_id = a.id WHERE t.id = ?1",
                [track_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn artwork_row(&self, bytes: &[u8]) -> Option<(String, String)> {
        self.conn()
            .query_row(
                "SELECT mime, origin FROM artwork WHERE sha256 = ?1",
                [ArtworkStore::hash_of(bytes).to_vec()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()
    }

    /// アップロードの模擬: 画像を store と artwork 行に置く
    async fn upload(&self, bytes: &[u8]) -> [u8; 32] {
        let hash = ArtworkStore::hash_of(bytes);
        let info = spindle::media::artwork::sniff(bytes).unwrap();
        self.store.put_original(&hash, info.mime, bytes).unwrap();
        let bytes_len = bytes.len();
        self.db
            .write(move |c| {
                spindle::db::artwork::upsert(
                    c,
                    &hash,
                    info.mime,
                    Some(info.width),
                    Some(info.height),
                    bytes_len,
                    "embedded",
                )
            })
            .await
            .unwrap();
        hash
    }

    fn batch_state(&self, id: i64) -> BatchState {
        history::get_batch(&self.conn(), id).unwrap().unwrap().state
    }

    fn ops(&self, batch_id: i64) -> Vec<history::Op> {
        history::list_ops(&self.conn(), batch_id).unwrap()
    }

    fn edits(&self, op_id: i64) -> Vec<history::Edit> {
        history::list_edits(&self.conn(), op_id).unwrap()
    }

    async fn wait_batch_terminal(&self, id: i64) -> BatchState {
        for _ in 0..1000 {
            let st = self.batch_state(id);
            if !matches!(st, BatchState::Prepared | BatchState::Applying) {
                return st;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("batch {id} が終端にならない: {:?}", self.batch_state(id));
    }
}

impl Drop for Lib {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn set_pictures(path: &Path, pictures: &[Vec<u8>]) {
    use lofty::picture::{MimeType, Picture, PictureType};
    let pics: Vec<Picture> = pictures
        .iter()
        .enumerate()
        .map(|(i, bytes)| {
            let mime = if bytes.starts_with(b"\x89PNG") {
                MimeType::Png
            } else {
                MimeType::Jpeg
            };
            Picture::unchecked(bytes.clone())
                .pic_type(if i == 0 {
                    PictureType::CoverFront
                } else {
                    PictureType::CoverBack
                })
                .mime_type(mime)
                .build()
        })
        .collect();
    common::retag(path, |t| {
        while !t.pictures().is_empty() {
            t.remove_picture(0);
        }
        for p in pics {
            t.push_picture(p);
        }
    });
}

/// ファイルの埋め込み画像（バイト列）
fn file_pictures(path: &Path) -> Vec<Vec<u8>> {
    let ext = path.extension().and_then(|e| e.to_str());
    let t = read_transfer_tags(File::open(path).unwrap(), ext).unwrap();
    t.pictures.into_iter().map(|p| p.data().to_vec()).collect()
}

// ---------------------------------------------------------------- 差し替え

#[tokio::test]
async fn replaces_pictures_in_flac_opus_and_mp4_and_bumps_tag_version() {
    let lib = Lib::new();
    let old = jpeg(b"old");
    let new = png();
    let mut paths = Vec::new();
    for (rel, seed) in [("A/1.flac", 1), ("A/2.opus", 2), ("A/3.m4a", 3)] {
        paths.push((
            rel,
            require_ffmpeg!(lib.add(rel, seed, std::slice::from_ref(&old))),
        ));
    }
    lib.scan().await;
    let ids: Vec<i64> = paths.iter().map(|(rel, _)| lib.track_id(rel)).collect();
    let versions: Vec<i64> = ids.iter().map(|id| lib.tag_version(*id)).collect();
    let hash = lib.upload(&new).await;
    lib.start();

    let p = lib
        .editor
        .prepare_picture(Some("差し替え"), ids.clone(), hash)
        .await
        .unwrap();
    let batch_id = p.batch_id.unwrap();
    assert_eq!(p.affected, 3);
    assert_eq!(p.unchanged, 0);
    // overlay: DB は先に新値
    for id in &ids {
        assert_eq!(
            lib.db_tag(*id, "PICTURE"),
            vec![picture_value("image/png", &new)]
        );
    }
    assert_eq!(lib.wait_batch_terminal(batch_id).await, BatchState::Applied);

    for ((_, path), (id, v)) in paths.iter().zip(ids.iter().zip(versions.iter())) {
        assert_eq!(file_pictures(path), vec![new.clone()], "{}", path.display());
        assert_eq!(lib.tag_version(*id), v + 1);
        assert_eq!(
            lib.db_tag(*id, "PICTURE"),
            vec![picture_value("image/png", &new)]
        );
        // album の再解決を予約する
        assert!(!lib.album_resolved(*id));
    }
    // 旧画像は退避されている（行と実体）
    assert!(lib
        .store
        .has_original(&ArtworkStore::hash_of(&old), "image/jpeg"));
    assert_eq!(
        lib.artwork_row(&old),
        Some(("image/jpeg".to_owned(), "embedded".to_owned()))
    );
    // edits は PICTURE だけ
    let ops = lib.ops(batch_id);
    assert_eq!(ops.len(), 3);
    for op in &ops {
        let edits = lib.edits(op.id);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].key, "PICTURE");
        assert_eq!(
            edits[0].old_value,
            serde_json::json!([picture_value("image/jpeg", &old)])
        );
        assert_eq!(
            edits[0].new_value,
            serde_json::json!([picture_value("image/png", &new)])
        );
    }
}

#[tokio::test]
async fn all_pictures_are_replaced_by_the_single_front_cover_and_each_old_one_is_stashed() {
    let lib = Lib::new();
    let front = jpeg(b"front");
    let back = jpeg(b"back");
    let new = jpeg(b"new");
    let path = require_ffmpeg!(lib.add("A/1.flac", 1, &[front.clone(), back.clone()]));
    lib.scan().await;
    let id = lib.track_id("A/1.flac");
    assert_eq!(lib.db_tag(id, "PICTURE").len(), 2);
    let hash = lib.upload(&new).await;
    lib.start();

    let p = lib
        .editor
        .prepare_picture(None, vec![id], hash)
        .await
        .unwrap();
    assert_eq!(
        lib.wait_batch_terminal(p.batch_id.unwrap()).await,
        BatchState::Applied
    );
    assert_eq!(file_pictures(&path), vec![new.clone()]);
    for old in [&front, &back] {
        assert!(lib
            .store
            .has_original(&ArtworkStore::hash_of(old), "image/jpeg"));
        assert!(lib.artwork_row(old).is_some());
    }
}

#[tokio::test]
async fn track_that_already_has_only_that_picture_is_unchanged_and_track_without_picture_gets_one()
{
    let lib = Lib::new();
    let new = jpeg(b"new");
    let same = require_ffmpeg!(lib.add("A/1.flac", 1, std::slice::from_ref(&new)));
    let none = lib.add("A/2.flac", 2, &[]).unwrap();
    lib.scan().await;
    let id_same = lib.track_id("A/1.flac");
    let id_none = lib.track_id("A/2.flac");
    let v_same = lib.tag_version(id_same);
    let hash = lib.upload(&new).await;
    lib.start();

    let p = lib
        .editor
        .prepare_picture(None, vec![id_same, id_none], hash)
        .await
        .unwrap();
    assert_eq!(p.affected, 1);
    assert_eq!(p.unchanged, 1);
    assert_eq!(
        lib.wait_batch_terminal(p.batch_id.unwrap()).await,
        BatchState::Applied
    );
    assert_eq!(file_pictures(&same), vec![new.clone()]);
    assert_eq!(lib.tag_version(id_same), v_same);
    assert_eq!(file_pictures(&none), vec![new.clone()]);
    let ops = lib.ops(p.batch_id.unwrap());
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].track_id, id_none);
    assert_eq!(
        lib.edits(ops[0].id)[0].old_value,
        serde_json::Value::Null,
        "画像なしの旧値は null"
    );

    // 全件が既に一致なら NoChanges
    let err = lib
        .editor
        .prepare_picture(None, vec![id_same], hash)
        .await
        .unwrap_err();
    assert!(matches!(err, EditError::NoChanges), "{err:?}");
}

#[tokio::test]
async fn missing_tracks_are_excluded() {
    let lib = Lib::new();
    let path = require_ffmpeg!(lib.add("A/1.flac", 1, &[]));
    lib.add("A/2.flac", 2, &[]).unwrap();
    lib.scan().await;
    let id1 = lib.track_id("A/1.flac");
    let id2 = lib.track_id("A/2.flac");
    std::fs::remove_file(&path).unwrap();
    lib.scan().await;
    let hash = lib.upload(&jpeg(b"new")).await;

    let p = lib
        .editor
        .prepare_picture(None, vec![id1, id2], hash)
        .await
        .unwrap();
    assert_eq!(p.affected, 1);
    assert_eq!(p.missing, 1);
    let ops = lib.ops(p.batch_id.unwrap());
    assert_eq!(ops[0].track_id, id2);
}

#[tokio::test]
async fn unknown_hash_is_rejected_before_recording_anything() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("A/1.flac", 1, &[]));
    lib.scan().await;
    let id = lib.track_id("A/1.flac");
    let err = lib
        .editor
        .prepare_picture(None, vec![id], [7u8; 32])
        .await
        .unwrap_err();
    assert!(matches!(err, EditError::ArtworkNotFound), "{err:?}");
    let n: i64 = lib
        .conn()
        .query_row("SELECT count(*) FROM edit_batches", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}

// ---------------------------------------------------------------- 巻き戻し

#[tokio::test]
async fn revert_restores_the_old_pictures_from_the_stash() {
    let lib = Lib::new();
    let front = jpeg(b"front");
    let back = png();
    let new = jpeg(b"new");
    let path = require_ffmpeg!(lib.add("A/1.flac", 1, &[front.clone(), back.clone()]));
    lib.scan().await;
    let id = lib.track_id("A/1.flac");
    let hash = lib.upload(&new).await;
    lib.start();

    let p = lib
        .editor
        .prepare_picture(None, vec![id], hash)
        .await
        .unwrap();
    let batch_id = p.batch_id.unwrap();
    assert_eq!(lib.wait_batch_terminal(batch_id).await, BatchState::Applied);
    assert_eq!(file_pictures(&path), vec![new.clone()]);
    let v = lib.tag_version(id);

    let r = lib.editor.revert_batch(batch_id, None).await.unwrap();
    assert_eq!(
        lib.wait_batch_terminal(r.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(file_pictures(&path), vec![front.clone(), back.clone()]);
    assert_eq!(lib.tag_version(id), v + 1);
    assert_eq!(
        lib.db_tag(id, "PICTURE"),
        vec![
            picture_value("image/jpeg", &front),
            picture_value("image/png", &back)
        ]
    );
    // 逆バッチが applied になったので元バッチは reverted
    assert!(history::get_batch(&lib.conn(), batch_id)
        .unwrap()
        .unwrap()
        .reverted_at
        .is_some());
}

#[tokio::test]
async fn revert_fails_the_op_when_the_old_picture_is_not_in_the_cache() {
    let lib = Lib::new();
    let old = jpeg(b"old");
    let new = jpeg(b"new");
    let path = require_ffmpeg!(lib.add("A/1.flac", 1, std::slice::from_ref(&old)));
    lib.scan().await;
    let id = lib.track_id("A/1.flac");
    let hash = lib.upload(&new).await;
    lib.start();
    let p = lib
        .editor
        .prepare_picture(None, vec![id], hash)
        .await
        .unwrap();
    let batch_id = p.batch_id.unwrap();
    assert_eq!(lib.wait_batch_terminal(batch_id).await, BatchState::Applied);

    // 退避した旧画像を消す（キャッシュの欠損）
    std::fs::remove_dir_all(lib.store.dir().join(hex(&ArtworkStore::hash_of(&old)))).unwrap();
    let r = lib.editor.revert_batch(batch_id, None).await.unwrap();
    assert_eq!(
        lib.wait_batch_terminal(r.batch_id).await,
        BatchState::Failed
    );
    let ops = lib.ops(r.batch_id);
    assert_eq!(ops[0].result, OpResult::Failed);
    assert!(
        ops[0].error.as_deref().unwrap_or("").contains("キャッシュ"),
        "{:?}",
        ops[0].error
    );
    // ファイルは触っていない。overlay は解消されファイルの現在値（新画像）に戻る
    assert_eq!(file_pictures(&path), vec![new.clone()]);
    assert_eq!(
        lib.db_tag(id, "PICTURE"),
        vec![picture_value("image/jpeg", &new)]
    );
}

// ---------------------------------------------------------------- 事前条件

#[tokio::test]
async fn external_picture_change_between_prepare_and_apply_is_a_conflict() {
    let lib = Lib::new();
    let old = jpeg(b"old");
    let external = jpeg(b"external");
    let new = jpeg(b"new");
    let path = require_ffmpeg!(lib.add("A/1.flac", 1, std::slice::from_ref(&old)));
    lib.scan().await;
    let id = lib.track_id("A/1.flac");
    let hash = lib.upload(&new).await;

    let p = lib
        .editor
        .prepare_picture(None, vec![id], hash)
        .await
        .unwrap();
    let batch_id = p.batch_id.unwrap();
    // ワーカーを起こす前に外部が画像を書き換える
    set_pictures(&path, std::slice::from_ref(&external));
    lib.start();
    assert_eq!(lib.wait_batch_terminal(batch_id).await, BatchState::Failed);
    let ops = lib.ops(batch_id);
    assert_eq!(ops[0].result, OpResult::SkippedConflict);
    assert_eq!(file_pictures(&path), vec![external.clone()]);
    // overlay の解消: DB はファイルの現在値
    assert_eq!(
        lib.db_tag(id, "PICTURE"),
        vec![picture_value("image/jpeg", &external)]
    );
    // 何も書いていないので「退避」は無いが、外部の画像はトラック自身の画像として記録される（D-61）
    assert_eq!(
        lib.track_artwork_sha(id),
        Some(ArtworkStore::hash_of(&external).to_vec())
    );
}

#[tokio::test]
async fn external_write_of_the_same_picture_is_applied_without_touching_the_file() {
    let lib = Lib::new();
    let old = jpeg(b"old");
    let new = jpeg(b"new");
    let path = require_ffmpeg!(lib.add("A/1.flac", 1, std::slice::from_ref(&old)));
    lib.scan().await;
    let id = lib.track_id("A/1.flac");
    let hash = lib.upload(&new).await;

    let p = lib
        .editor
        .prepare_picture(None, vec![id], hash)
        .await
        .unwrap();
    let batch_id = p.batch_id.unwrap();
    set_pictures(&path, std::slice::from_ref(&new));
    let st_before = std::fs::metadata(&path).unwrap();
    lib.start();
    assert_eq!(lib.wait_batch_terminal(batch_id).await, BatchState::Applied);
    let st_after = std::fs::metadata(&path).unwrap();
    assert_eq!(
        std::os::unix::fs::MetadataExt::ino(&st_before),
        std::os::unix::fs::MetadataExt::ino(&st_after),
        "ファイルは書き直していない"
    );
    assert_eq!(file_pictures(&path), vec![new.clone()]);
}

#[tokio::test]
async fn missing_new_picture_in_cache_fails_the_op_without_writing() {
    let lib = Lib::new();
    let old = jpeg(b"old");
    let new = jpeg(b"new");
    let path = require_ffmpeg!(lib.add("A/1.flac", 1, std::slice::from_ref(&old)));
    lib.scan().await;
    let id = lib.track_id("A/1.flac");
    let hash = lib.upload(&new).await;
    let p = lib
        .editor
        .prepare_picture(None, vec![id], hash)
        .await
        .unwrap();
    let batch_id = p.batch_id.unwrap();
    // 反映前にキャッシュから消える
    std::fs::remove_dir_all(lib.store.dir().join(hex(&hash))).unwrap();
    lib.start();
    assert_eq!(lib.wait_batch_terminal(batch_id).await, BatchState::Failed);
    let ops = lib.ops(batch_id);
    assert_eq!(ops[0].result, OpResult::Failed);
    assert_eq!(file_pictures(&path), vec![old.clone()]);
    assert_eq!(
        lib.db_tag(id, "PICTURE"),
        vec![picture_value("image/jpeg", &old)]
    );
}

#[tokio::test]
async fn editor_without_artwork_store_cannot_prepare() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Library");
    std::fs::create_dir(&lib).unwrap();
    let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
    let root = Arc::new(RootDir::open(&lib).unwrap());
    let jobs = Jobs::new(db.clone());
    let editor = Editor::new(db, root, jobs);
    let err = editor
        .prepare_picture(None, vec![1], [0u8; 32])
        .await
        .unwrap_err();
    assert!(matches!(err, EditError::ArtworkUnavailable), "{err:?}");
}

// ---------------------------------------------------------------- tracks.artwork_id の追随（D-61）

impl Lib {
    fn track_artwork_sha(&self, id: i64) -> Option<Vec<u8>> {
        self.conn()
            .query_row(
                "SELECT a.sha256 FROM tracks t LEFT JOIN artwork a ON a.id = t.artwork_id WHERE t.id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
    }
}

#[tokio::test]
async fn track_artwork_follows_replace_revert_and_survives_other_tag_edits() {
    let lib = Lib::new();
    let old = jpeg(b"old");
    let new = png();
    let path = require_ffmpeg!(lib.add("A/1.flac", 1, std::slice::from_ref(&old)));
    lib.scan().await;
    let id = lib.track_id("A/1.flac");
    assert_eq!(
        lib.track_artwork_sha(id),
        Some(ArtworkStore::hash_of(&old).to_vec())
    );
    let hash = lib.upload(&new).await;
    lib.start();

    let p = lib
        .editor
        .prepare_picture(None, vec![id], hash)
        .await
        .unwrap();
    let batch_id = p.batch_id.unwrap();
    assert_eq!(lib.wait_batch_terminal(batch_id).await, BatchState::Applied);
    assert_eq!(lib.track_artwork_sha(id), Some(hash.to_vec()));
    // 差し替えた画像のサムネイルは upload 時に投入済み。ここでは行が新画像を指すことだけ

    // 画像に触らないタグ編集では据え置き
    let t = lib
        .editor
        .prepare_tags(
            None,
            vec![spindle::edit::NewTagOp {
                track_id: id,
                changes: vec![spindle::edit::TagChange {
                    key: "TITLE".to_owned(),
                    values: Some(vec!["Renamed".to_owned()]),
                }],
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        lib.wait_batch_terminal(t.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(lib.track_artwork_sha(id), Some(hash.to_vec()));
    assert_eq!(file_pictures(&path), vec![new.clone()]);

    // 巻き戻しで旧画像へ
    let r = lib.editor.revert_batch(batch_id, None).await.unwrap();
    assert_eq!(
        lib.wait_batch_terminal(r.batch_id).await,
        BatchState::Applied
    );
    assert_eq!(
        lib.track_artwork_sha(id),
        Some(ArtworkStore::hash_of(&old).to_vec())
    );
}

#[tokio::test]
async fn external_picture_change_seen_as_conflict_updates_track_artwork() {
    let lib = Lib::new();
    let old = jpeg(b"old");
    let external = jpeg(b"external");
    let path = require_ffmpeg!(lib.add("A/1.flac", 1, std::slice::from_ref(&old)));
    lib.scan().await;
    let id = lib.track_id("A/1.flac");
    let hash = lib.upload(&jpeg(b"new")).await;
    let p = lib
        .editor
        .prepare_picture(None, vec![id], hash)
        .await
        .unwrap();
    set_pictures(&path, std::slice::from_ref(&external));
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(p.batch_id.unwrap()).await,
        BatchState::Failed
    );
    // overlay の解消はファイルの現在値に揃える（artwork_id も外部の画像になる）
    assert_eq!(
        lib.track_artwork_sha(id),
        Some(ArtworkStore::hash_of(&external).to_vec())
    );
}

impl Lib {
    fn album_unresolved(&self, track_id: i64) -> bool {
        !self.album_resolved(track_id)
    }

    fn job_types(&self) -> Vec<String> {
        let c = self.conn();
        let mut st = c
            .prepare("SELECT type FROM jobs WHERE state IN ('queued', 'running') ORDER BY id")
            .unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    /// Phase 5 で album を解決し直したことにする（予約を消す）
    fn mark_album_resolved(&self, track_id: i64) {
        self.conn()
            .execute(
                "UPDATE albums SET artwork_resolved_at = 1 WHERE id = (SELECT album_id FROM tracks WHERE id = ?1)",
                [track_id],
            )
            .unwrap();
    }
}

/// 通常のタグ op の反映中に外部が画像を差し替えていた（conflict）: artwork_id に加えて、album の
/// 再解決の予約と thumbnail ジョブも追随する（codex の指摘: 物理属性を現在値に揃えるので、ここで
/// 予約しないと次の増分スキャンには「変更なし」と見えて album の絵が古いまま固定される）
#[tokio::test]
async fn external_picture_change_during_a_tag_op_reserves_album_and_thumbnail() {
    let lib = Lib::new();
    let old = jpeg(b"old");
    let external = jpeg(b"external");
    let path = require_ffmpeg!(lib.add("A/1.flac", 1, std::slice::from_ref(&old)));
    lib.scan().await;
    let id = lib.track_id("A/1.flac");
    lib.mark_album_resolved(id);
    // 画像に触らないタグ編集を記録してから、外部が画像を差し替える
    let t = lib
        .editor
        .prepare_tags(
            None,
            vec![spindle::edit::NewTagOp {
                track_id: id,
                changes: vec![spindle::edit::TagChange {
                    key: "TITLE".to_owned(),
                    values: Some(vec!["Renamed".to_owned()]),
                }],
            }],
        )
        .await
        .unwrap();
    set_pictures(&path, std::slice::from_ref(&external));
    lib.start();
    assert_eq!(
        lib.wait_batch_terminal(t.batch_id).await,
        BatchState::Failed
    );
    assert_eq!(
        lib.track_artwork_sha(id),
        Some(ArtworkStore::hash_of(&external).to_vec())
    );
    assert!(lib.album_unresolved(id), "album の再解決を予約する");
    let jobs = lib.job_types();
    assert!(jobs.iter().any(|t| t == "thumbnail"), "{jobs:?}");
    assert!(jobs.iter().any(|t| t == "scan"), "{jobs:?}");
}

/// pending の tags op がある間にスキャンが走った（Phase 4 は overlay を守るため update_content を
/// 飛ばす）後、その op がキャンセルで閉じられる: overlay の解消がファイルの現在値（外部の画像）を
/// artwork_id に反映し、album の再解決も予約する
#[tokio::test]
async fn cancel_after_a_scan_skipped_the_pending_row_follows_the_external_picture() {
    let lib = Lib::new();
    let old = jpeg(b"old");
    let external = jpeg(b"external");
    let path = require_ffmpeg!(lib.add("A/1.flac", 1, std::slice::from_ref(&old)));
    lib.scan().await;
    let id = lib.track_id("A/1.flac");
    lib.mark_album_resolved(id);
    let t = lib
        .editor
        .prepare_tags(
            None,
            vec![spindle::edit::NewTagOp {
                track_id: id,
                changes: vec![spindle::edit::TagChange {
                    key: "TITLE".to_owned(),
                    values: Some(vec!["Renamed".to_owned()]),
                }],
            }],
        )
        .await
        .unwrap();
    set_pictures(&path, std::slice::from_ref(&external));
    // ワーカーを起こさずにスキャン: pending 行は overlay を守って content を書かない
    lib.scan().await;
    assert_eq!(
        lib.track_artwork_sha(id),
        Some(ArtworkStore::hash_of(&old).to_vec()),
        "pending の間は据え置き"
    );
    lib.editor.cancel_batch(t.batch_id).await.unwrap();
    assert_eq!(
        lib.wait_batch_terminal(t.batch_id).await,
        BatchState::Cancelled
    );
    assert_eq!(
        lib.track_artwork_sha(id),
        Some(ArtworkStore::hash_of(&external).to_vec())
    );
    assert!(lib.album_unresolved(id));
}

/// 通常のタグ op の反映中に外部が画像を差し替え、しかも store が書けない: 旧 `artwork_id` を保って
/// `artwork_dirty` を立て、障害が直った後の増分スキャンが（物理属性は tagwrite が現在値に揃えて
/// いても）読み直して追随する
#[tokio::test]
async fn store_failure_while_reading_an_external_picture_change_is_retried_by_the_next_scan() {
    use std::os::unix::fs::PermissionsExt;
    if std::fs::metadata("/proc/self").is_ok_and(|m| std::os::unix::fs::MetadataExt::uid(&m) == 0) {
        eprintln!("root では書き込み禁止を作れないので skip");
        return;
    }
    let lib = Lib::new();
    let old = jpeg(b"old");
    let external = jpeg(b"external");
    let path = require_ffmpeg!(lib.add("A/1.flac", 1, std::slice::from_ref(&old)));
    lib.scan().await;
    let id = lib.track_id("A/1.flac");
    let t = lib
        .editor
        .prepare_tags(
            None,
            vec![spindle::edit::NewTagOp {
                track_id: id,
                changes: vec![spindle::edit::TagChange {
                    key: "TITLE".to_owned(),
                    values: Some(vec!["Renamed".to_owned()]),
                }],
            }],
        )
        .await
        .unwrap();
    set_pictures(&path, std::slice::from_ref(&external));
    std::fs::set_permissions(lib.store.dir(), std::fs::Permissions::from_mode(0o500)).unwrap();
    lib.start();
    let state = lib.wait_batch_terminal(t.batch_id).await;
    std::fs::set_permissions(lib.store.dir(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(state, BatchState::Failed);
    assert_eq!(
        lib.track_artwork_sha(id),
        Some(ArtworkStore::hash_of(&old).to_vec()),
        "旧 id を保つ"
    );
    let dirty: i64 = lib
        .conn()
        .query_row(
            "SELECT artwork_dirty FROM tracks WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dirty, 1);

    lib.scan().await;
    assert_eq!(
        lib.track_artwork_sha(id),
        Some(ArtworkStore::hash_of(&external).to_vec())
    );
}

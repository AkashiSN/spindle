//! Inbox の却下した件の破棄（D-90、P4-22）。「削除」で破棄待ちにした件を GC が `[gc].retention_days`
//! 経過後に消すこと、期限前・取り消し後・ファイルが足された後は消さないこと、件のディレクトリの外
//! （別の件・サブディレクトリ）に触らないこと、走査でファイルが変わると破棄待ちが解けることを固定する

#![cfg(target_os = "linux")]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use spindle::db::inbox::{self as dbinbox, ItemState};
use spindle::db::Db;
use spindle::fsroot::RootDir;
use spindle::gc::{execute_inbox, plan, GcRoots};
use spindle::import::inbox::{scan_inbox, DISCARD_CANCELLED_BY_CHANGE};
use spindle::media::artwork::ArtworkStore;

const DAY: i64 = 86_400;
const RETENTION: i64 = 30 * DAY;

struct Env {
    /// 破棄を要求する時刻。同梱ファイルの新旧は ctime と比べる（D-90）ので実時刻にし、準備で置くファイルより
    /// 後（2 秒先）にする
    t0: i64,
    dir: tempfile::TempDir,
    db: Arc<Db>,
    inbox: Arc<RootDir>,
    roots: GcRoots,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["Library", "Archive", "Derived", "Inbox", "thumbs"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let inbox = Arc::new(RootDir::open(&dir.path().join("Inbox")).unwrap());
        let roots = GcRoots {
            library: Arc::new(RootDir::open(&dir.path().join("Library")).unwrap()),
            archive: Arc::new(RootDir::open(&dir.path().join("Archive")).unwrap()),
            derived: Arc::new(RootDir::open(&dir.path().join("Derived")).unwrap()),
            artwork: Arc::new(ArtworkStore::new(dir.path().join("thumbs"))),
            inbox: Some(Arc::clone(&inbox)),
        };
        Self {
            t0: spindle::db::now_epoch() + 2,
            dir,
            db,
            inbox,
            roots,
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join("Inbox").join(rel)
    }

    fn wav(&self, rel: &str, seed: u32) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        common::write_wav(&p, &common::pcm_samples(seed), 16);
    }

    /// 実時刻が破棄の要求（`t0`）に達するまで待つ（要求の後に置いたファイルを作るため）
    async fn wait_past_request(&self) {
        while spindle::db::now_epoch() < self.t0 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    fn file(&self, rel: &str, body: &[u8]) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    async fn scan(&self, now: i64) {
        scan_inbox(&self.db, &self.inbox, now).await.unwrap();
    }

    async fn item_id(&self, rel_dir: &str) -> i64 {
        let key = spindle::domain::relpath::canonical_key(rel_dir);
        self.db
            .read(move |c| Ok(dbinbox::find_by_dir_key(c, &key)?.map(|i| i.id)))
            .await
            .unwrap()
            .unwrap()
    }

    async fn item(&self, id: i64) -> Option<dbinbox::Item> {
        self.db.read(move |c| dbinbox::get(c, id)).await.unwrap()
    }

    /// 却下して `at` に破棄を要求する
    async fn reject_and_discard(&self, id: i64, at: i64) {
        self.db
            .write(move |c| {
                assert!(dbinbox::transition(
                    c,
                    id,
                    &[ItemState::Pending],
                    ItemState::Rejected,
                    None,
                    at
                )?);
                assert!(dbinbox::request_discard(c, id, at)?);
                Ok(())
            })
            .await
            .unwrap();
    }

    /// GC の F 区分だけを `now` で計画して実行する。返り値は（計画した件数、消した件数、残した件数）
    async fn gc(&self, now: i64) -> (usize, usize, usize) {
        let p = plan(&self.db, &self.roots, RETENTION, now).await.unwrap();
        let c = execute_inbox(&self.db, &self.roots, &p, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(c.failed, 0);
        (c.planned, c.deleted, c.skipped)
    }
}

fn exists(p: &Path) -> bool {
    p.symlink_metadata().is_ok()
}

/// 破棄する件（直下に音声 2 本・cover.jpg・サイドカー・知らないファイル、サブディレクトリは別の件）と
/// 隣の件
async fn seeded() -> (Env, i64) {
    let env = Env::new();
    env.wav("youtube/A/Aのお歌/01 x.wav", 1);
    env.wav("youtube/A/Aのお歌/02 y.wav", 2);
    env.file("youtube/A/Aのお歌/cover.jpg", b"jpeg");
    env.file("youtube/A/Aのお歌/spindle-inbox.json", b"{}");
    env.wav("youtube/A/Aのお歌/sub/03 z.wav", 3);
    env.wav("youtube/A/other/04 w.wav", 4);
    env.scan(env.t0 - 10).await;
    let id = env.item_id("youtube/A/Aのお歌").await;
    (env, id)
}

#[tokio::test]
async fn discarded_item_is_deleted_only_after_the_retention() {
    let (env, id) = seeded().await;
    env.reject_and_discard(id, env.t0).await;
    // 期限前は計画にも載らない
    assert_eq!(env.gc(env.t0 + RETENTION - 1).await, (0, 0, 0));
    assert!(exists(&env.path("youtube/A/Aのお歌/01 x.wav")));
    assert!(env.item(id).await.is_some());
    // 期限後: 件の音声・同梱の画像・サイドカーを消し、行も消す
    assert_eq!(env.gc(env.t0 + RETENTION).await, (1, 1, 0));
    for gone in [
        "youtube/A/Aのお歌/01 x.wav",
        "youtube/A/Aのお歌/02 y.wav",
        "youtube/A/Aのお歌/cover.jpg",
        "youtube/A/Aのお歌/spindle-inbox.json",
    ] {
        assert!(!exists(&env.path(gone)), "{gone} が残っている");
    }
    assert!(env.item(id).await.is_none());
    // サブディレクトリ（別の件）と隣の件には触らない。サブディレクトリが残るのでディレクトリも残る
    assert!(exists(&env.path("youtube/A/Aのお歌/sub/03 z.wav")));
    assert!(exists(&env.path("youtube/A/other/04 w.wav")));
    let sub = env.item_id("youtube/A/Aのお歌/sub").await;
    let other = env.item_id("youtube/A/other").await;
    assert_eq!(env.item(sub).await.unwrap().state, ItemState::Pending);
    assert_eq!(env.item(other).await.unwrap().state, ItemState::Pending);
}

#[tokio::test]
async fn empty_directory_is_removed_and_unknown_files_keep_it() {
    let env = Env::new();
    env.wav("youtube/B/Bのお歌/01 x.wav", 1);
    env.wav("youtube/C/Cのお歌/01 x.wav", 2);
    env.file("youtube/C/Cのお歌/memo.txt", b"keep");
    env.scan(env.t0 - 10).await;
    let b = env.item_id("youtube/B/Bのお歌").await;
    let c = env.item_id("youtube/C/Cのお歌").await;
    env.reject_and_discard(b, env.t0).await;
    env.reject_and_discard(c, env.t0).await;
    assert_eq!(env.gc(env.t0 + RETENTION).await, (2, 2, 0));
    // 空になったディレクトリは消える。親（youtube/B）は残す
    assert!(!exists(&env.path("youtube/B/Bのお歌")));
    assert!(exists(&env.path("youtube/B")));
    // 知らないファイルは消さないので、ディレクトリも残る
    assert!(!exists(&env.path("youtube/C/Cのお歌/01 x.wav")));
    assert!(exists(&env.path("youtube/C/Cのお歌/memo.txt")));
}

#[tokio::test]
async fn cancelled_discard_is_kept() {
    let (env, id) = seeded().await;
    env.reject_and_discard(id, env.t0).await;
    env.db
        .write(move |c| {
            assert!(dbinbox::cancel_discard(c, id, None)?);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(env.gc(env.t0 + RETENTION).await, (0, 0, 0));
    let it = env.item(id).await.unwrap();
    assert_eq!(it.state, ItemState::Rejected);
    assert_eq!(it.discard_requested_at, None);
    assert!(exists(&env.path("youtube/A/Aのお歌/01 x.wav")));
}

#[tokio::test]
async fn cancel_after_planning_keeps_the_item() {
    // 計画の後に取り消された（下書きに戻された）件は、実行時の確かめ直しで残す
    let (env, id) = seeded().await;
    env.reject_and_discard(id, env.t0).await;
    let p = plan(&env.db, &env.roots, RETENTION, env.t0 + RETENTION)
        .await
        .unwrap();
    assert_eq!(p.inbox.len(), 1);
    env.db
        .write(move |c| {
            assert!(dbinbox::transition(
                c,
                id,
                &[ItemState::Rejected],
                ItemState::Pending,
                None,
                env.t0 + 1
            )?);
            Ok(())
        })
        .await
        .unwrap();
    let counts = execute_inbox(&env.db, &env.roots, &p, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!((counts.deleted, counts.skipped), (0, 1));
    let it = env.item(id).await.unwrap();
    assert_eq!(it.state, ItemState::Pending);
    assert_eq!(
        it.discard_requested_at, None,
        "rejected から出る遷移で破棄待ちは解ける"
    );
    assert!(exists(&env.path("youtube/A/Aのお歌/01 x.wav")));
}

#[tokio::test]
async fn a_file_added_after_the_scan_stops_the_deletion() {
    // 走査が写した後（次の走査の前）に音声が足された。何も消さず、破棄待ちを解いて理由を残す
    let (env, id) = seeded().await;
    env.reject_and_discard(id, env.t0).await;
    env.wav("youtube/A/Aのお歌/05 new.wav", 5);
    assert_eq!(env.gc(env.t0 + RETENTION).await, (1, 0, 1));
    for kept in [
        "youtube/A/Aのお歌/01 x.wav",
        "youtube/A/Aのお歌/05 new.wav",
        "youtube/A/Aのお歌/cover.jpg",
        "youtube/A/Aのお歌/spindle-inbox.json",
    ] {
        assert!(exists(&env.path(kept)), "{kept} が消えた");
    }
    let it = env.item(id).await.unwrap();
    assert_eq!(it.state, ItemState::Rejected);
    assert_eq!(it.discard_requested_at, None);
    assert_eq!(it.error.as_deref(), Some(DISCARD_CANCELLED_BY_CHANGE));
}

#[tokio::test]
async fn a_changed_file_stops_the_deletion() {
    let (env, id) = seeded().await;
    env.reject_and_discard(id, env.t0).await;
    // 同じ名前で中身（size / mtime）を差し替える
    env.wav("youtube/A/Aのお歌/02 y.wav", 9);
    std::fs::write(env.path("youtube/A/Aのお歌/02 y.wav"), b"RIFF-changed").unwrap();
    assert_eq!(env.gc(env.t0 + RETENTION).await, (1, 0, 1));
    assert!(exists(&env.path("youtube/A/Aのお歌/01 x.wav")));
    assert_eq!(env.item(id).await.unwrap().discard_requested_at, None);
}

#[tokio::test]
async fn scan_that_sees_changed_files_cancels_the_discard() {
    let (env, id) = seeded().await;
    env.reject_and_discard(id, env.t0).await;
    env.wav("youtube/A/Aのお歌/05 new.wav", 5);
    env.scan(env.t0 + 10).await;
    let it = env.item(id).await.unwrap();
    assert_eq!(it.state, ItemState::Rejected, "却下のまま（人が見直す）");
    assert_eq!(it.discard_requested_at, None);
    assert_eq!(it.error.as_deref(), Some(DISCARD_CANCELLED_BY_CHANGE));
    // 変化の無い走査では破棄待ちは解けない
    env.db
        .write(move |c| {
            assert!(dbinbox::request_discard(c, id, env.t0 + 20)?);
            Ok(())
        })
        .await
        .unwrap();
    env.scan(env.t0 + 30).await;
    assert_eq!(
        env.item(id).await.unwrap().discard_requested_at,
        Some(env.t0 + 20)
    );
}

#[tokio::test]
async fn inbox_category_is_skipped_without_an_inbox_root() {
    let (mut env, id) = seeded().await;
    env.reject_and_discard(id, env.t0).await;
    env.roots.inbox = None;
    assert_eq!(env.gc(env.t0 + RETENTION).await, (0, 0, 0));
    assert!(env.item(id).await.is_some());
}

#[tokio::test]
async fn a_companion_placed_after_the_request_stops_the_deletion() {
    // 同梱ファイル（cover.jpg 等）は inbox_files に写らないので、破棄の要求より後に置かれたものは ctime で見分ける。
    // 要求の後に差し替えられた cover を黙って消さない
    let (env, id) = seeded().await;
    env.reject_and_discard(id, env.t0).await;
    env.wait_past_request().await;
    std::fs::remove_file(env.path("youtube/A/Aのお歌/cover.jpg")).unwrap();
    env.file("youtube/A/Aのお歌/cover.jpg", b"new jpeg");
    env.file("youtube/A/Aのお歌/disc1.cue", b"cue");
    assert_eq!(env.gc(env.t0 + RETENTION).await, (1, 0, 1));
    for kept in [
        "youtube/A/Aのお歌/01 x.wav",
        "youtube/A/Aのお歌/cover.jpg",
        "youtube/A/Aのお歌/disc1.cue",
        "youtube/A/Aのお歌/spindle-inbox.json",
    ] {
        assert!(exists(&env.path(kept)), "{kept} が消えた");
    }
    let it = env.item(id).await.unwrap();
    assert_eq!(it.discard_requested_at, None);
    assert_eq!(it.error.as_deref(), Some(DISCARD_CANCELLED_BY_CHANGE));
}

#[tokio::test]
async fn a_failed_unlink_keeps_the_row_and_the_request_for_the_next_gc() {
    // 消せなかった（EACCES）ら成功扱いにせず、行と破棄待ちを残す。直れば次の GC で消える
    use std::os::unix::fs::PermissionsExt as _;
    if is_root() {
        eprintln!("root では権限で unlink を失敗させられないので skip");
        return;
    }
    let (env, id) = seeded().await;
    env.reject_and_discard(id, env.t0).await;
    let dir = env.path("youtube/A/Aのお歌");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let p = plan(&env.db, &env.roots, RETENTION, env.t0 + RETENTION)
        .await
        .unwrap();
    let c = execute_inbox(&env.db, &env.roots, &p, &CancellationToken::new())
        .await
        .unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!((c.deleted, c.failed), (0, 1));
    let it = env.item(id).await.unwrap();
    assert_eq!(it.state, ItemState::Rejected);
    assert_eq!(it.discard_requested_at, Some(env.t0), "破棄待ちは残る");
    assert!(exists(&env.path("youtube/A/Aのお歌/01 x.wav")));
    // 次の GC で続きを行い、行も消す
    assert_eq!(env.gc(env.t0 + RETENTION).await, (1, 1, 0));
    assert!(!exists(&env.path("youtube/A/Aのお歌/01 x.wav")));
    assert!(env.item(id).await.is_none());
}

fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1).map(|u| u == "0"))
        })
        .unwrap_or(false)
}

//! CD の取り込みの表の画像を Cover Art Archive から一度だけ取る（D-91）。CAA はローカルの axum で模す。
//!
//! 見るもの: リリースの決まった CD の取り込みは画像を置いて提案の全曲の picture に入る、画像の無い盤（404）と
//! 上流の失敗は取り込みを止めず画像なし（失敗は次の走査でもう 1 回だけ試す）、リリースの無い取り込み・CD で
//! ない取り込みは取りに行かない、保存した下書きが勝つ、この機能より前からある取り込みにも効く、inbox
//! ジョブの走査の後に取る、取った画像は GC しない

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use spindle::cd::coverart::CoverArtClient;
use spindle::config::LayoutConfig;
use spindle::db::inbox::{self as dbinbox, ItemState, CAA_MAX_TRIES};
use spindle::db::Db;
use spindle::domain::relpath::{canonical_key, RelPath};
use spindle::fsroot::RootDir;
use spindle::import::cover::{fetch_cd_covers, CoverReport};
use spindle::import::inbox::{propose, scan_inbox, PlaceItemEnv};
use spindle::import::sidecar::Sidecar;
use spindle::jobs::handlers::inbox::{new_inbox_job, InboxHandler};
use spindle::jobs::{EnqueueResult, JobState, JobType, Jobs, Registry};
use spindle::media::artwork::ArtworkStore;

/// 画像のある盤
const WITH_ART: &str = "66666666-6666-6666-6666-666666666666";
/// 画像の無い盤（404）
const NO_ART: &str = "00000000-0000-0000-0000-000000000000";
/// 上流が 500 を返す盤
const BROKEN: &str = "77777777-7777-7777-7777-777777777777";

/// 1x1 の PNG（IHDR まである本物。寸法を読める）
const REAL_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5, 0x1c, 0x0c,
    0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0x60, 0x00, 0x00,
    0x00, 0x06, 0x00, 0x02, 0x30, 0x81, 0xd0, 0x2f, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

async fn caa_front(
    State(hits): State<Arc<AtomicUsize>>,
    Path((id, size)): Path<(String, String)>,
) -> axum::response::Response {
    hits.fetch_add(1, Ordering::SeqCst);
    if size != "front-500" {
        return (StatusCode::NOT_FOUND, "wrong size").into_response();
    }
    match id.as_str() {
        WITH_ART => ([(header::CONTENT_TYPE, "image/png")], REAL_PNG.to_vec()).into_response(),
        BROKEN => (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response(),
        _ => (StatusCode::NOT_FOUND, "no art").into_response(),
    }
}

/// CAA の模擬を立て、ベース URL と呼ばれた回数を返す
async fn serve_caa() -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/release/{id}/{size}", get(caa_front))
        .with_state(Arc::clone(&hits));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (format!("http://{addr}/"), hits)
}

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    db: Arc<Db>,
    jobs: Arc<Jobs>,
    inbox: Arc<RootDir>,
    library: Arc<RootDir>,
    store: Arc<ArtworkStore>,
    client: Arc<CoverArtClient>,
    hits: Arc<AtomicUsize>,
    shutdown: CancellationToken,
}

impl Lib {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["Library", "Inbox"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let inbox = Arc::new(RootDir::open(&dir.path().join("Inbox")).unwrap());
        let library = Arc::new(RootDir::open(&dir.path().join("Library")).unwrap());
        let store = Arc::new(ArtworkStore::new(dir.path().join("thumbs")));
        let (base, hits) = serve_caa().await;
        let client = Arc::new(CoverArtClient::new(&base, "spindle-test/0").unwrap());
        Self {
            jobs: Jobs::new(db.clone()),
            dir,
            db_path,
            db,
            inbox,
            library,
            store,
            client,
            hits,
            shutdown: CancellationToken::new(),
        }
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.db_path).unwrap()
    }

    /// Inbox の `dir` に音声を 2 本置く（CD の吸い出しと同じ形）。`release` があればサイドカーの
    /// `rip.metadata.release_id` に入れる。`rip` が false ならサイドカーの `rip` 自体を置かない
    fn add_item(&self, dir: &str, release: Option<&str>, rip: bool) -> Option<()> {
        let root = self.dir.path().join("Inbox").join(dir);
        std::fs::create_dir_all(&root).unwrap();
        for (i, name) in ["01.flac", "02.flac"].iter().enumerate() {
            let made = common::make_audio(&root, name, "flac", i as u32 + 1)?;
            common::set_basic_tags(&made, "", "", "", "", i as u32 + 1, 1);
        }
        let mut s = Sidecar::default();
        if rip {
            let mut e = common::rip_entry(&["01.flac", "02.flac"], &[true, true]);
            e.metadata.release_id = release.map(str::to_owned);
            s.rip = Some(e);
        }
        s.write(&self.inbox, &RelPath::parse(dir).unwrap()).unwrap();
        Some(())
    }

    async fn scan(&self) {
        scan_inbox(&self.db, &self.inbox, 1000).await.unwrap();
    }

    async fn fetch(&self) -> CoverReport {
        fetch_cd_covers(
            &self.db,
            &self.inbox,
            &self.store,
            &self.client,
            &self.jobs,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
    }

    fn item(&self, dir: &str) -> dbinbox::Item {
        dbinbox::find_by_dir_key(&self.conn(), &canonical_key(dir))
            .unwrap()
            .unwrap()
    }

    /// 件の提案の picture（トラック順）
    fn proposed_pictures(&self, dir: &str) -> Vec<Option<String>> {
        let c = self.conn();
        let item = self.item(dir);
        let files = dbinbox::files(&c, item.id).unwrap();
        let p = propose(&c, &self.inbox, &layout(), &item, &files, &[], &[]).unwrap();
        p.draft.tracks.iter().map(|t| t.picture.clone()).collect()
    }
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn layout() -> LayoutConfig {
    LayoutConfig {
        multi_disc: "{category}/{albumartist}/{album}/{disc}-{track:02}. {title}".into(),
        single_disc: "{category}/{albumartist}/{album}/{track:02}. {title}".into(),
        unsorted: "_Unsorted/{albumartist}/{album}/{track:02}. {title}".into(),
    }
}

#[tokio::test]
async fn a_cd_import_with_a_release_gets_the_front_cover_in_its_proposal() {
    let lib = Lib::new().await;
    require_ffmpeg!(lib.add_item("CD/Five", Some(WITH_ART), true));
    lib.scan().await;
    // 取る前は画像なし
    assert_eq!(lib.proposed_pictures("CD/Five"), vec![None, None]);
    let r = lib.fetch().await;
    assert_eq!(r.found, 1, "{r:?}");
    let item = lib.item("CD/Five");
    let picture = item.caa_picture.clone().expect("画像を記録する");
    assert!(picture.starts_with("image/png:"), "{picture}");
    // artwork 行ができ、提案の全曲に入る
    let hash = unhex(picture.split_once(':').unwrap().1);
    assert!(spindle::db::artwork::get_by_sha256(&lib.conn(), &hash)
        .unwrap()
        .is_some());
    assert_eq!(
        lib.proposed_pictures("CD/Five"),
        vec![Some(picture.clone()), Some(picture.clone())]
    );
    // 二度目の走査では取りに行かない（一度だけ）
    let before = lib.hits.load(Ordering::SeqCst);
    assert_eq!(lib.fetch().await, CoverReport::default());
    assert_eq!(lib.hits.load(Ordering::SeqCst), before);
    // GC しない（取り込みが参照している）
    let unref = spindle::db::gc::unreferenced_artwork(&lib.conn()).unwrap();
    assert!(unref.iter().all(|(_, h)| *h != hash), "{unref:?}");
}

#[tokio::test]
async fn a_disc_without_art_stays_without_a_picture_and_is_not_asked_again() {
    let lib = Lib::new().await;
    require_ffmpeg!(lib.add_item("CD/NoArt", Some(NO_ART), true));
    lib.scan().await;
    let r = lib.fetch().await;
    assert_eq!(r.absent, 1, "{r:?}");
    let item = lib.item("CD/NoArt");
    assert_eq!(item.caa_picture, None);
    assert_eq!(item.caa_tries, CAA_MAX_TRIES);
    assert_eq!(item.state, ItemState::Pending, "取り込みは止めない");
    assert_eq!(lib.proposed_pictures("CD/NoArt"), vec![None, None]);
    let before = lib.hits.load(Ordering::SeqCst);
    lib.fetch().await;
    assert_eq!(lib.hits.load(Ordering::SeqCst), before);
}

#[tokio::test]
async fn an_upstream_failure_is_retried_once_on_the_next_scan() {
    let lib = Lib::new().await;
    require_ffmpeg!(lib.add_item("CD/Broken", Some(BROKEN), true));
    lib.scan().await;
    assert_eq!(lib.fetch().await.failed, 1);
    let item = lib.item("CD/Broken");
    assert_eq!((item.caa_picture.as_deref(), item.caa_tries), (None, 1));
    assert_eq!(item.state, ItemState::Pending);
    // 次の走査でもう 1 回だけ
    assert_eq!(lib.fetch().await.failed, 1);
    assert_eq!(lib.item("CD/Broken").caa_tries, CAA_MAX_TRIES);
    let before = lib.hits.load(Ordering::SeqCst);
    assert_eq!(lib.fetch().await, CoverReport::default());
    assert_eq!(
        lib.hits.load(Ordering::SeqCst),
        before,
        "上限の後は取りに行かない"
    );
}

#[tokio::test]
async fn imports_without_a_release_or_without_rip_are_not_fetched() {
    let lib = Lib::new().await;
    // 候補を選ばずに吸い出した CD（release_id なし）と、CD でない取り込み（サイドカーに rip が無い）
    require_ffmpeg!(lib.add_item("CD/Manual", None, true));
    lib.add_item("youtube/A/A のお歌", None, false);
    lib.scan().await;
    let r = lib.fetch().await;
    assert_eq!(r.skipped, 2, "{r:?}");
    assert_eq!(lib.hits.load(Ordering::SeqCst), 0);
    for dir in ["CD/Manual", "youtube/A/A のお歌"] {
        let item = lib.item(dir);
        assert_eq!((item.caa_picture, item.caa_tries), (None, CAA_MAX_TRIES));
    }
}

#[tokio::test]
async fn a_saved_draft_wins_over_the_fetched_cover() {
    let lib = Lib::new().await;
    require_ffmpeg!(lib.add_item("CD/Five", Some(WITH_ART), true));
    lib.scan().await;
    // 人が先に下書きを保存した（画像は外したまま）
    let c = lib.conn();
    let item = lib.item("CD/Five");
    let files = dbinbox::files(&c, item.id).unwrap();
    let draft = propose(&c, &lib.inbox, &layout(), &item, &files, &[], &[])
        .unwrap()
        .draft;
    assert!(draft.tracks.iter().all(|t| t.picture.is_none()));
    dbinbox::set_draft(&c, item.id, &serde_json::to_value(&draft).unwrap()).unwrap();
    // 後から画像が取れても、保存した下書きの「画像なし」が勝つ
    assert_eq!(lib.fetch().await.found, 1);
    assert!(lib.item("CD/Five").caa_picture.is_some());
    assert_eq!(lib.proposed_pictures("CD/Five"), vec![None, None]);
}

/// この機能より前に吸い出して Inbox に置いてある取り込み（走査済み・`caa_tries` 0）にも、inbox ジョブの
/// 走査の後に取りに行く（ジョブの配線）
#[tokio::test]
async fn the_inbox_job_fetches_covers_for_existing_imports() {
    let lib = Lib::new().await;
    require_ffmpeg!(lib.add_item("CD/Five", Some(WITH_ART), true));
    lib.scan().await;
    assert_eq!(lib.item("CD/Five").caa_tries, 0);
    let env = PlaceItemEnv {
        db: lib.db.clone(),
        library: lib.library.clone(),
        inbox: lib.inbox.clone(),
        jobs: lib.jobs.clone(),
        layout: layout(),
        editor: None,
        wav_to_flac: false,
        before_place: None,
        artwork: Some(lib.store.clone()),
        before_artwork: None,
        coverart: Some(lib.client.clone()),
    };
    let mut reg = Registry::new();
    reg.register(JobType::Inbox, Arc::new(InboxHandler::new(env)));
    lib.jobs.start(reg, lib.shutdown.clone());
    let job = match lib.jobs.enqueue(new_inbox_job()).await.unwrap() {
        EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let state = loop {
        let s: String = lib
            .conn()
            .query_row("SELECT state FROM jobs WHERE id = ?1", [job], |r| r.get(0))
            .unwrap();
        let st: JobState = s.parse().unwrap();
        if st.is_terminal() || std::time::Instant::now() > deadline {
            break st;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(state, JobState::Done);
    let item = lib.item("CD/Five");
    assert!(item.caa_picture.is_some(), "{item:?}");
    assert_eq!(item.state, ItemState::Pending);
    lib.shutdown.cancel();
}

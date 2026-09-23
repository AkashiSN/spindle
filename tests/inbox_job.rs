//! Inbox の走査と配置（`import::inbox`、`inbox` ジョブ。SPEC §7.8、D-68、P2-10）。
//! 一時ディレクトリに Inbox / Library を作り、ffmpeg で音声を置く（無ければ skip）

#![cfg(target_os = "linux")]

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use std::time::Duration;

use rusqlite::{Connection, OptionalExtension as _};
use tokio_util::sync::CancellationToken;

use spindle::cd::place::PlaceHook;
use spindle::config::LayoutConfig;
use spindle::db::inbox::{self, ItemState};
use spindle::db::Db;
use spindle::edit::{Editor, NormalizeEnv};
use spindle::fsroot::RootDir;
use spindle::import::inbox::{scan_inbox, DraftTrack, InboxDraft, PlaceItemEnv};
use spindle::jobs::handlers::inbox::{new_inbox_job, InboxHandler};
use spindle::jobs::{EnqueueResult, JobState, JobType, Jobs, Registry};
use spindle::media::artwork::ArtworkStore;
use spindle::media::encode::FlacEncoder;

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    db: Arc<Db>,
    jobs: Arc<Jobs>,
    library: Arc<RootDir>,
    inbox: Arc<RootDir>,
    editor: Arc<Editor>,
    shutdown: CancellationToken,
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["Library", "Inbox", "Archive", "tmp"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        common::enable_opus_variant(&db_path, 128);
        let library = Arc::new(RootDir::open(&dir.path().join("Library")).unwrap());
        let inbox = Arc::new(RootDir::open(&dir.path().join("Inbox")).unwrap());
        let archive = Arc::new(RootDir::open(&dir.path().join("Archive")).unwrap());
        let jobs = Jobs::new(db.clone());
        let ffmpeg = common::ffmpeg().unwrap_or_else(|| PathBuf::from("ffmpeg"));
        let editor = Arc::new(
            Editor::new(db.clone(), library.clone(), jobs.clone()).with_normalize(NormalizeEnv {
                archive,
                encoder: FlacEncoder::new(ffmpeg, "flac", 5, dir.path().join("tmp")),
                retention_days: 30,
            }),
        );
        Self {
            dir,
            db_path,
            db,
            jobs,
            library,
            inbox,
            editor,
            shutdown: CancellationToken::new(),
        }
    }

    fn env(&self, wav_to_flac: bool) -> PlaceItemEnv {
        self.env_with(wav_to_flac, None)
    }

    fn env_with(&self, wav_to_flac: bool, before_place: Option<PlaceHook>) -> PlaceItemEnv {
        self.env_full(wav_to_flac, before_place, None)
    }

    fn env_full(
        &self,
        wav_to_flac: bool,
        before_place: Option<PlaceHook>,
        before_artwork: Option<PlaceHook>,
    ) -> PlaceItemEnv {
        PlaceItemEnv {
            db: self.db.clone(),
            library: self.library.clone(),
            inbox: self.inbox.clone(),
            jobs: self.jobs.clone(),
            layout: LayoutConfig {
                multi_disc: "{category}/{albumartist}/{album}/{disc}-{track:02} {title}".into(),
                single_disc: "{category}/{albumartist}/{album}/{track:02} {title}".into(),
                unsorted: "_Unsorted/{albumartist}/{album}/{track:02} {title}".into(),
            },
            editor: Some(self.editor.clone()),
            wav_to_flac,
            before_place,
            artwork: Some(Arc::new(ArtworkStore::new(self.dir.path().join("thumbs")))),
            before_artwork,
        }
    }

    fn start(&self, wav_to_flac: bool) {
        self.start_with(self.env(wav_to_flac));
    }

    fn start_with(&self, env: PlaceItemEnv) {
        let mut reg = Registry::new();
        reg.register(JobType::Inbox, Arc::new(InboxHandler::new(env)));
        self.jobs.start(reg, self.shutdown.clone());
    }

    fn lib_path(&self, rel: &str) -> PathBuf {
        self.dir.path().join("Library").join(rel)
    }

    /// inbox ジョブを投入して終端まで待つ
    async fn run_job(&self) -> JobState {
        let job = match self.jobs.enqueue(new_inbox_job()).await.unwrap() {
            EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while std::time::Instant::now() < deadline {
            let s: String = self
                .conn()
                .query_row("SELECT state FROM jobs WHERE id = ?1", [job], |r| r.get(0))
                .unwrap();
            let st: JobState = s.parse().unwrap();
            if st.is_terminal() {
                return st;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("inbox ジョブが終わらない");
    }

    fn approve(&self, id: i64, draft: &InboxDraft) {
        let c = self.conn();
        inbox::set_draft(&c, id, &serde_json::to_value(draft).unwrap()).unwrap();
        inbox::set_state(&c, id, ItemState::Approved, None, 1).unwrap();
    }

    fn count(&self, sql: &str) -> i64 {
        self.conn().query_row(sql, [], |r| r.get(0)).unwrap()
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

impl Drop for Lib {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

// ---------------------------------------------------------------- 配置

fn draft_for(files: &[(&str, u32, &str)], category: Option<&str>, album: &str) -> InboxDraft {
    InboxDraft {
        category: category.map(str::to_owned),
        albumartist: "Artist".into(),
        album: album.into(),
        date: Some("2024".into()),
        tracks: files
            .iter()
            .map(|(rel, n, title)| DraftTrack {
                rel_path: rel.to_string(),
                disc_no: 1,
                track_no: *n,
                title: title.to_string(),
                artist: String::new(),
                keep_artists: None,
            })
            .collect(),
        album_gain: false,
        release_id: None,
        release_group_id: None,
    }
}

#[tokio::test]
async fn approved_item_is_placed_registered_and_consumed() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    lib.add("AlbumA/02.flac", 2, "Two", "A", 2);
    std::fs::write(lib.inbox_path("AlbumA/cover.jpg"), b"jpg").unwrap();
    std::fs::write(lib.inbox_path("AlbumA/notes.txt"), b"keep").unwrap();
    lib.conn()
        .execute("INSERT INTO categories (name) VALUES ('Rock')", [])
        .unwrap();
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    // 補正: category、アルバム名、2 曲目のタイトル
    lib.approve(
        a.id,
        &draft_for(
            &[("AlbumA/01.flac", 1, "One"), ("AlbumA/02.flac", 2, "Two!")],
            Some("Rock"),
            "Album",
        ),
    );
    lib.start(true);
    assert_eq!(lib.run_job().await, JobState::Done);

    // Library に置かれ、タグに補正が書かれている
    let p1 = lib.lib_path("Rock/Artist/Album/01 One.flac");
    let p2 = lib.lib_path("Rock/Artist/Album/02 Two!.flac");
    assert!(p1.exists() && p2.exists());
    let af =
        spindle::domain::tags::read_audio_file(std::fs::File::open(&p2).unwrap(), Some("flac"))
            .unwrap();
    assert_eq!(af.tags.first("TITLE"), Some("Two!"));
    assert_eq!(af.tags.first("ALBUM"), Some("Album"));
    assert_eq!(af.tags.first("ALBUMARTIST"), Some("Artist"));
    assert_eq!(af.tags.first("DATE"), Some("2024"));
    assert_eq!(af.tags.first("TRACKNUMBER"), Some("2"));
    assert!(lib.lib_path("Rock/Artist/Album/cover.jpg").exists());
    // DB
    let (src, album_id): (String, i64) = lib
        .conn()
        .query_row(
            "SELECT source_type, album_id FROM tracks WHERE rel_path = 'Rock/Artist/Album/02 Two!.flac'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(src, "download");
    let (rel_dir, cat, aa): (String, Option<i64>, Option<String>) = lib
        .conn()
        .query_row(
            "SELECT rel_dir, category_id, albumartist FROM albums WHERE id = ?1",
            [album_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(rel_dir, "Rock/Artist/Album");
    assert!(cat.is_some());
    assert_eq!(aa.as_deref(), Some("Artist"));
    let it = inbox::get(&lib.conn(), a.id).unwrap().unwrap();
    assert_eq!(it.state, ItemState::Placed);
    assert_eq!(it.placed_album_id, Some(album_id));
    // 後続: rg はトラックごと（album gain は既定 off。D-74）+ transcode 2
    assert_eq!(
        lib.count("SELECT count(*) FROM jobs WHERE type = 'rg' AND state = 'queued'"),
        2
    );
    assert_eq!(
        lib.count("SELECT count(*) FROM jobs WHERE type = 'rg' AND dedup_key LIKE 'rg:track:%'"),
        2
    );
    assert_eq!(
        lib.count(&format!(
            "SELECT album_gain FROM albums WHERE id = {album_id}"
        )),
        0
    );
    assert_eq!(
        lib.count("SELECT count(*) FROM jobs WHERE type = 'transcode' AND state = 'queued'"),
        2
    );
    // Inbox 側: 音声と同梱ファイルは消え、未知のファイルは残る（ディレクトリも残る）
    assert!(!lib.inbox_path("AlbumA/01.flac").exists());
    assert!(!lib.inbox_path("AlbumA/cover.jpg").exists());
    assert!(lib.inbox_path("AlbumA/notes.txt").exists());
    // 排他は解放されている
    assert_eq!(lib.count("SELECT count(*) FROM job_mutexes"), 0);
    // 次の走査で AlbumA は音声が無いので件にならない（placed の行は残る）
    lib.scan(2000).await;
    assert_eq!(
        inbox::get(&lib.conn(), a.id).unwrap().unwrap().state,
        ItemState::Placed
    );
}

#[tokio::test]
async fn wav_item_gets_a_normalize_batch_when_enabled() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("W/01.wav", 1, "One", "W", 1));
    lib.scan(1000).await;
    let w = lib.item("W").unwrap();
    lib.approve(
        w.id,
        &draft_for(&[("W/01.wav", 1, "One")], None, "Wav Album"),
    );
    lib.start(true);
    assert_eq!(lib.run_job().await, JobState::Done);
    assert!(lib
        .lib_path("_Unsorted/Artist/Wav Album/01 One.wav")
        .exists());
    assert_eq!(
        inbox::get(&lib.conn(), w.id).unwrap().unwrap().state,
        ItemState::Placed
    );
    // normalize の編集バッチ（archive op）と normalize ジョブ
    assert_eq!(
        lib.count("SELECT count(*) FROM edit_ops WHERE kind = 'archive'"),
        1
    );
    assert_eq!(
        lib.count("SELECT count(*) FROM jobs WHERE type = 'normalize'"),
        1
    );
}

#[tokio::test]
async fn wav_item_without_wav_to_flac_is_placed_only() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("W/01.wav", 1, "One", "W", 1));
    lib.scan(1000).await;
    let w = lib.item("W").unwrap();
    lib.approve(
        w.id,
        &draft_for(&[("W/01.wav", 1, "One")], None, "Wav Album"),
    );
    lib.start(false);
    assert_eq!(lib.run_job().await, JobState::Done);
    assert_eq!(lib.count("SELECT count(*) FROM edit_ops"), 0);
}

#[tokio::test]
async fn conflict_marks_failed_and_leaves_nothing_in_library() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    // 宛先に別の内容のファイルが既にある（登録されていない外部ファイル）
    std::fs::create_dir_all(lib.lib_path("_Unsorted/Artist/Album")).unwrap();
    std::fs::write(lib.lib_path("_Unsorted/Artist/Album/01 One.flac"), b"other").unwrap();
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    lib.approve(
        a.id,
        &draft_for(&[("AlbumA/01.flac", 1, "One")], None, "Album"),
    );
    lib.start(true);
    assert_eq!(lib.run_job().await, JobState::Done);
    let it = inbox::get(&lib.conn(), a.id).unwrap().unwrap();
    assert_eq!(it.state, ItemState::Failed);
    assert!(
        it.error.as_deref().unwrap_or("").contains("01 One.flac"),
        "{:?}",
        it.error
    );
    // Library には自分の成果物が残らず、外部ファイルはそのまま。Inbox も残る
    assert_eq!(
        std::fs::read(lib.lib_path("_Unsorted/Artist/Album/01 One.flac")).unwrap(),
        b"other"
    );
    assert_eq!(lib.count("SELECT count(*) FROM tracks"), 0);
    assert!(lib.inbox_path("AlbumA/01.flac").exists());
    assert_eq!(lib.count("SELECT count(*) FROM job_mutexes"), 0);
}

#[tokio::test]
async fn busy_library_requeues_and_keeps_item_approved() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    lib.approve(
        a.id,
        &draft_for(&[("AlbumA/01.flac", 1, "One")], None, "Album"),
    );
    // 別の本物の running ジョブ（scan）が library を持ち続ける（行を直接 running にすると
    // 稼働中の回収で queued に戻される。D-76）
    let release = CancellationToken::new();
    let env = lib.env(true);
    let handler = InboxHandler::new(env);
    // ハンドラを直接は呼べないので、ワーカーで 1 回だけ回して Requeue を観測する
    let mut reg = Registry::new();
    reg.register(JobType::Inbox, Arc::new(handler));
    {
        let release = release.clone();
        reg.register_fn(JobType::Scan, move |ctx| {
            let release = release.clone();
            async move {
                if !ctx.lock_mutex(spindle::jobs::LIBRARY_MUTEX).await? {
                    return Ok(spindle::jobs::Outcome::Requeue);
                }
                release.cancelled().await;
                Ok(spindle::jobs::Outcome::Done)
            }
        });
    }
    let other = match lib
        .jobs
        .enqueue(spindle::jobs::NewJob::new(
            JobType::Scan,
            serde_json::json!({}),
        ))
        .await
        .unwrap()
    {
        EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
    };
    lib.jobs.start(reg, lib.shutdown.clone());
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let st: String = lib
            .conn()
            .query_row("SELECT state FROM jobs WHERE id = ?1", [other], |r| {
                r.get(0)
            })
            .unwrap();
        if st == "running" || std::time::Instant::now() >= deadline {
            assert_eq!(st, "running");
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let job = match lib.jobs.enqueue(new_inbox_job()).await.unwrap() {
        EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
    };
    // Requeue は queued に戻る（終端にならない）。少し待って状態と件を見る
    tokio::time::sleep(Duration::from_millis(500)).await;
    let st: String = lib
        .conn()
        .query_row("SELECT state FROM jobs WHERE id = ?1", [job], |r| r.get(0))
        .unwrap();
    assert!(st == "queued" || st == "running", "{st}");
    assert_eq!(
        inbox::get(&lib.conn(), a.id).unwrap().unwrap().state,
        ItemState::Approved
    );
    assert!(!lib.lib_path("_Unsorted").exists());
    release.cancel();
}

#[tokio::test]
async fn changed_inbox_file_after_approval_goes_back_to_pending() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    // 承認の後にファイルが変わった（走査で拾う前に配置へ進む状況を、行の stat を古くして作る）
    lib.approve(
        a.id,
        &draft_for(&[("AlbumA/01.flac", 1, "One")], None, "Album"),
    );
    lib.conn()
        .execute("UPDATE inbox_files SET size = size + 1", [])
        .unwrap();
    let env = lib.env(true);
    let item = inbox::get(&lib.conn(), a.id).unwrap().unwrap();
    let err = spindle::import::inbox::place_item(&env, &item, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(err, spindle::import::inbox::InboxError::Changed(_)),
        "{err}"
    );
    assert!(!lib.lib_path("_Unsorted").exists());
}

#[tokio::test]
async fn rerun_after_partial_placement_reuses_files_and_adopts_rows() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    lib.approve(
        a.id,
        &draft_for(&[("AlbumA/01.flac", 1, "One")], None, "Album"),
    );
    let env = lib.env(false);
    let item = inbox::get(&lib.conn(), a.id).unwrap().unwrap();
    let first = spindle::import::inbox::place_item(&env, &item, &CancellationToken::new())
        .await
        .unwrap();
    // 「登録の後・Inbox の消去の前に落ちて、Inbox に同じファイルが戻った」状況: 行を消さずに
    // Inbox へ同じ内容を再作成して、もう一度配置する
    std::fs::create_dir_all(lib.inbox_path("AlbumA")).unwrap();
    std::fs::copy(
        lib.lib_path("_Unsorted/Artist/Album/01 One.flac"),
        lib.inbox_path("AlbumA/01.flac"),
    )
    .unwrap();
    lib.scan(1001).await;
    let a2 = lib.item("AlbumA").unwrap();
    lib.approve(
        a2.id,
        &draft_for(&[("AlbumA/01.flac", 1, "One")], None, "Album"),
    );
    let item = inbox::get(&lib.conn(), a2.id).unwrap().unwrap();
    let second = spindle::import::inbox::place_item(&env, &item, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(second.album_id, first.album_id);
    assert_eq!(second.track_ids, first.track_ids);
    assert_eq!(lib.count("SELECT count(*) FROM tracks"), 1);
    assert!(!lib.inbox_path("AlbumA/01.flac").exists());
}

// ---------------------------------------------------------------- クラッシュ境界と競合

/// 前のプロセスが placing のまま落ちた件（ファイルはまだ何も置いていない）は、次のジョブが
/// approved に戻してそのまま配置する
#[tokio::test]
async fn placing_left_by_a_crash_is_recovered_and_placed() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    lib.approve(
        a.id,
        &draft_for(&[("AlbumA/01.flac", 1, "One")], None, "Album"),
    );
    inbox::set_state(&lib.conn(), a.id, ItemState::Placing, None, 2).unwrap();
    lib.start(false);
    assert_eq!(lib.run_job().await, JobState::Done);
    let it = inbox::get(&lib.conn(), a.id).unwrap().unwrap();
    assert_eq!(it.state, ItemState::Placed, "{:?}", it.error);
    assert!(lib.lib_path("_Unsorted/Artist/Album/01 One.flac").exists());
    assert!(!lib.inbox_path("AlbumA/01.flac").exists());
}

/// 「ファイルは置いたが登録の前に落ちた」: Library に補正済みのファイルだけがあり、DB に行が無く、
/// Inbox に原本が残っている。再実行は宛先を自分の成果物として採用し、行を作り、Inbox を消す
#[tokio::test]
async fn crash_after_copy_before_register_is_completed_by_the_next_job() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    let draft = draft_for(&[("AlbumA/01.flac", 1, "One!")], None, "Album");
    lib.approve(a.id, &draft);
    let env = lib.env(false);
    let item = inbox::get(&lib.conn(), a.id).unwrap().unwrap();
    let first = spindle::import::inbox::place_item(&env, &item, &CancellationToken::new())
        .await
        .unwrap();
    // 登録を無かったことにし（行は消す）、Inbox に原本を戻し、placing のまま落ちたことにする
    let c = lib.conn();
    c.execute("DELETE FROM tracks WHERE id = ?1", [first.track_ids[0]])
        .unwrap();
    c.execute("DELETE FROM albums WHERE id = ?1", [first.album_id])
        .unwrap();
    c.execute("DELETE FROM jobs WHERE type IN ('rg', 'transcode')", [])
        .unwrap();
    drop(c);
    std::fs::create_dir_all(lib.inbox_path("AlbumA")).unwrap();
    lib.add("AlbumA/01.flac", 1, "One", "A", 1).unwrap();
    lib.scan(1001).await;
    let a2 = lib.item("AlbumA").unwrap();
    lib.approve(a2.id, &draft);
    inbox::set_state(&lib.conn(), a2.id, ItemState::Placing, None, 2).unwrap();
    lib.start(false);
    assert_eq!(lib.run_job().await, JobState::Done);
    let it = inbox::get(&lib.conn(), a2.id).unwrap().unwrap();
    assert_eq!(it.state, ItemState::Placed, "{:?}", it.error);
    assert_eq!(lib.count("SELECT count(*) FROM tracks"), 1);
    assert_eq!(lib.count("SELECT count(*) FROM albums"), 1);
    let p = lib.lib_path("_Unsorted/Artist/Album/01 One!.flac");
    assert!(p.exists());
    let af = spindle::domain::tags::read_audio_file(std::fs::File::open(&p).unwrap(), Some("flac"))
        .unwrap();
    assert_eq!(af.tags.first("TITLE"), Some("One!"));
    assert!(!lib.inbox_path("AlbumA/01.flac").exists());
}

/// Inbox 側を読んだ後・コピーの前に原本が差し替えられたら Changed で、Library には何も残らない
#[tokio::test]
async fn source_replaced_before_copy_is_detected_and_cleaned_up() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    lib.approve(
        a.id,
        &draft_for(&[("AlbumA/01.flac", 1, "One")], None, "Album"),
    );
    let inbox_dir = lib.inbox_path("AlbumA");
    let hook: PlaceHook = Arc::new(move || {
        // 別の音声で差し替える（inode が変わる）
        std::fs::remove_file(inbox_dir.join("01.flac")).unwrap();
        common::make_audio(&inbox_dir, "01.flac", "flac", 9).unwrap();
        common::set_basic_tags(
            &inbox_dir.join("01.flac"),
            "One",
            "Artist",
            "A",
            "Artist",
            1,
            1,
        );
    });
    let env = lib.env_with(false, Some(hook));
    let item = inbox::get(&lib.conn(), a.id).unwrap().unwrap();
    let err = spindle::import::inbox::place_item(&env, &item, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(err, spindle::import::inbox::InboxError::Changed(_)),
        "{err}"
    );
    assert!(!lib.lib_path("_Unsorted").exists());
    assert_eq!(lib.count("SELECT count(*) FROM tracks"), 0);
    assert!(lib.inbox_path("AlbumA/01.flac").exists());
}

/// 配置の後も Inbox に音声が残っていれば（消せなかった / 置き直された）、placed の裏に隠さず
/// 次の走査で pending に戻す
#[tokio::test]
async fn audio_left_in_a_placed_directory_reopens_the_item() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    lib.approve(
        a.id,
        &draft_for(&[("AlbumA/01.flac", 1, "One")], None, "Album"),
    );
    lib.start(false);
    assert_eq!(lib.run_job().await, JobState::Done);
    assert_eq!(
        inbox::get(&lib.conn(), a.id).unwrap().unwrap().state,
        ItemState::Placed
    );
    // 配置の後に同じディレクトリへ別の音声が置かれた
    lib.add("AlbumA/02.flac", 2, "Two", "A", 2).unwrap();
    lib.scan(2000).await;
    let it = inbox::get(&lib.conn(), a.id).unwrap().unwrap();
    assert_eq!(it.state, ItemState::Pending);
    assert!(it.error.is_some());
    let files = inbox::files(&lib.conn(), a.id).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].rel_path, "AlbumA/02.flac");
}

/// 「登録も Inbox の消費も済んだが、ジョブが件を placed にする前に落ちた」境界: 登録トランザクションが
/// 件の placed も確定するので、再起動後も placed のまま（approved に戻されて failed になったり、
/// 走査で消えたりしない）
#[tokio::test]
async fn crash_after_register_keeps_the_item_placed() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    lib.approve(
        a.id,
        &draft_for(&[("AlbumA/01.flac", 1, "One")], None, "Album"),
    );
    inbox::set_state(&lib.conn(), a.id, ItemState::Placing, None, 2).unwrap();
    let env = lib.env(false);
    let item = inbox::get(&lib.conn(), a.id).unwrap().unwrap();
    // place_item は登録 + 消費まで。handler の後処理をせずに落ちたことにする
    let placed = spindle::import::inbox::place_item(&env, &item, &CancellationToken::new())
        .await
        .unwrap();
    let it = inbox::get(&lib.conn(), a.id).unwrap().unwrap();
    assert_eq!(it.state, ItemState::Placed);
    assert_eq!(it.placed_album_id, Some(placed.album_id));
    assert!(!lib.inbox_path("AlbumA/01.flac").exists());
    // 次のジョブ（回復 + 走査）でも placed のまま残り、二重登録もしない
    lib.start(false);
    assert_eq!(lib.run_job().await, JobState::Done);
    let it = inbox::get(&lib.conn(), a.id).unwrap().unwrap();
    assert_eq!(it.state, ItemState::Placed, "{:?}", it.error);
    assert_eq!(lib.count("SELECT count(*) FROM tracks"), 1);
}

/// normalize の投入は登録と同じトランザクション: place_item（handler の後処理なし）が返った時点で
/// 件の placed・トラック・rg / transcode・normalize バッチが揃って確定している（登録の commit と
/// normalize の投入の間にプロセスが落ちる窓が無い）。次のジョブは placed の件を触らず二重投入しない
#[tokio::test]
async fn normalize_batch_is_recorded_atomically_with_registration() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("W/01.wav", 1, "One", "W", 1));
    lib.scan(1000).await;
    let w = lib.item("W").unwrap();
    lib.approve(
        w.id,
        &draft_for(&[("W/01.wav", 1, "One")], None, "Wav Album"),
    );
    inbox::set_state(&lib.conn(), w.id, ItemState::Placing, None, 2).unwrap();
    let env = lib.env(true);
    let item = inbox::get(&lib.conn(), w.id).unwrap().unwrap();
    let placed = spindle::import::inbox::place_item(&env, &item, &CancellationToken::new())
        .await
        .unwrap();
    let batch_id = placed.normalize_batch.expect("normalize バッチ");
    let (state, ops, jobs): (String, i64, i64) = lib
        .conn()
        .query_row(
            "SELECT (SELECT state FROM inbox_items WHERE id = ?1),
                    (SELECT count(*) FROM edit_ops WHERE batch_id = ?2 AND kind = 'archive'),
                    (SELECT count(*) FROM jobs WHERE type = 'normalize')",
            [w.id, batch_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((state.as_str(), ops, jobs), ("placed", 1, 1));
    assert!(
        placed.job_ids.len() >= 2,
        "rg + normalize: {:?}",
        placed.job_ids
    );
    lib.start(true);
    assert_eq!(lib.run_job().await, JobState::Done);
    assert_eq!(lib.count("SELECT count(*) FROM edit_batches"), 1);
    assert_eq!(
        lib.count("SELECT count(*) FROM jobs WHERE type = 'normalize'"),
        1
    );
}

// ---------------------------------------------------------------- 既存 album への追記（D-70）

/// テスト用: ファイルのタグを直接書き換える（`write_tag_changes` を tmp 無しで当てる）
fn set_tags(path: &std::path::Path, ext: &str, tags: &[(&str, &[&str])]) {
    let changes: Vec<spindle::domain::tags::TagChange> = tags
        .iter()
        .map(|(k, vs)| spindle::domain::tags::TagChange {
            key: (*k).to_owned(),
            values: Some(vs.iter().map(|v| (*v).to_owned()).collect()),
        })
        .collect();
    let mut f = std::fs::File::options()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    spindle::domain::tags::write_tag_changes(&mut f, Some(ext), &changes, None).unwrap();
}

fn sidecar_entry() -> spindle::import::sidecar::FileEntry {
    spindle::import::sidecar::FileEntry {
        source: "youtube".into(),
        url: Some("https://www.youtube.com/watch?v=abc".into()),
        channel: Some("CH".into()),
        verdict: "ok".into(),
        message: None,
        subscription_id: None,
        position: None,
    }
}

/// 1 件目を配置した後、同じ category / albumartist / album への 2 件目は既存の album に追記される。
/// サイドカーは Library に持っていかず、配置の成功で消える
#[tokio::test]
async fn second_item_appends_to_the_existing_album_and_removes_the_sidecar() {
    use spindle::import::sidecar::{Sidecar, SIDECAR_NAME};
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    lib.conn()
        .execute("INSERT INTO categories (name) VALUES ('Rock')", [])
        .unwrap();
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    lib.approve(
        a.id,
        &draft_for(&[("AlbumA/01.flac", 1, "One")], Some("Rock"), "Album"),
    );
    lib.start(true);
    assert_eq!(lib.run_job().await, JobState::Done);
    let album_id: i64 = lib
        .conn()
        .query_row(
            "SELECT id FROM albums WHERE rel_dir = 'Rock/Artist/Album'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    // 2 件目（TRACKNUMBER 無し、サイドカー付き）。承認の下書きで #2 を振る
    lib.add(
        "youtube/Artist/Album/20260901 Two [abc].flac",
        2,
        "Two",
        "Album",
        0,
    );
    let dir = spindle::domain::relpath::RelPath::parse("youtube/Artist/Album").unwrap();
    Sidecar::upsert(
        &lib.inbox,
        &dir,
        Some("Rock"),
        "20260901 Two [abc].flac",
        sidecar_entry(),
    )
    .unwrap();
    lib.scan(2000).await;
    let b = lib.item("youtube/Artist/Album").unwrap();
    lib.approve(
        b.id,
        &draft_for(
            &[("youtube/Artist/Album/20260901 Two [abc].flac", 2, "Two")],
            Some("Rock"),
            "Album",
        ),
    );
    assert_eq!(lib.run_job().await, JobState::Done);
    let it = inbox::get(&lib.conn(), b.id).unwrap().unwrap();
    assert_eq!(it.state, ItemState::Placed, "{:?}", it.error);
    assert_eq!(it.placed_album_id, Some(album_id));
    assert!(lib.lib_path("Rock/Artist/Album/02 Two.flac").exists());
    assert_eq!(lib.count("SELECT count(*) FROM albums"), 1);
    assert_eq!(
        lib.count(&format!(
            "SELECT count(*) FROM tracks WHERE album_id = {album_id} AND missing_since IS NULL"
        )),
        2
    );
    // サイドカーは Library に無く、Inbox からも消えてディレクトリごと無くなる
    assert!(!lib
        .lib_path(&format!("Rock/Artist/Album/{SIDECAR_NAME}"))
        .exists());
    assert!(!lib.inbox_path("youtube/Artist/Album").exists());
}

/// 下書きの album_gain=true で配置した album は属性が on になり rg は album 単位。追記の下書きは
/// 追記先の属性を上書きし、off にすると album の値が消える（D-74）
#[tokio::test]
async fn album_gain_in_draft_sets_the_attribute_and_picks_the_rg_scope() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    lib.conn()
        .execute("INSERT INTO categories (name) VALUES ('Rock')", [])
        .unwrap();
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    let mut draft = draft_for(&[("AlbumA/01.flac", 1, "One")], Some("Rock"), "Album");
    draft.album_gain = true;
    lib.approve(a.id, &draft);
    lib.start(true);
    assert_eq!(lib.run_job().await, JobState::Done);
    let album_id: i64 = lib
        .conn()
        .query_row(
            "SELECT id FROM albums WHERE rel_dir = 'Rock/Artist/Album'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        lib.count(&format!(
            "SELECT album_gain FROM albums WHERE id = {album_id}"
        )),
        1
    );
    assert_eq!(
        lib.count("SELECT count(*) FROM jobs WHERE type = 'rg' AND dedup_key LIKE 'rg:album:%'"),
        1
    );
    assert_eq!(
        lib.count("SELECT count(*) FROM jobs WHERE type = 'rg' AND dedup_key LIKE 'rg:track:%'"),
        0
    );
    // album の値を持たせておき、2 件目を off で追記 → 属性 off、値が消え、rg は track 単位
    lib.conn()
        .execute(
            "UPDATE tracks SET rg_track_gain = 0, rg_track_peak = 0.5, rg_album_gain = -1.0,
                    rg_album_peak = 0.9, rg_scanned_at = 10, rg_written_at = 10
              WHERE album_id = ?1",
            [album_id],
        )
        .unwrap();
    lib.conn()
        .execute("DELETE FROM jobs WHERE type IN ('rg', 'transcode')", [])
        .unwrap();
    lib.add("AlbumB/02.flac", 2, "Two", "Album", 1);
    lib.scan(2000).await;
    let b = lib.item("AlbumB").unwrap();
    lib.approve(
        b.id,
        &draft_for(&[("AlbumB/02.flac", 2, "Two")], Some("Rock"), "Album"),
    );
    assert_eq!(lib.run_job().await, JobState::Done);
    let it = inbox::get(&lib.conn(), b.id).unwrap().unwrap();
    assert_eq!(it.placed_album_id, Some(album_id), "{:?}", it.error);
    assert_eq!(
        lib.count(&format!(
            "SELECT album_gain FROM albums WHERE id = {album_id}"
        )),
        0
    );
    assert_eq!(
        lib.count(&format!(
            "SELECT count(*) FROM tracks WHERE album_id = {album_id} AND rg_album_gain IS NOT NULL"
        )),
        0
    );
    assert_eq!(
        lib.count("SELECT count(*) FROM jobs WHERE type = 'rg' AND dedup_key LIKE 'rg:track:%'"),
        1,
        "追記した 1 曲だけ track 単位"
    );
    assert_eq!(
        lib.count("SELECT count(*) FROM jobs WHERE type = 'rg' AND dedup_key LIKE 'rg:album:%'"),
        0
    );
}

/// 承認と配置の間に番号が埋まっていたら、登録で弾いて failed（置いたファイルは片付ける）
#[tokio::test]
async fn append_with_a_taken_number_fails_at_registration() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    lib.approve(
        a.id,
        &draft_for(&[("AlbumA/01.flac", 1, "One")], None, "Album"),
    );
    lib.start(true);
    assert_eq!(lib.run_job().await, JobState::Done);

    // 2 件目が #1 を名乗る（API の検証を通らないが、承認後に 1 件目が入った状況と同じ）
    lib.add("AlbumB/01.flac", 2, "Other", "Album", 1);
    lib.scan(2000).await;
    let b = lib.item("AlbumB").unwrap();
    lib.approve(
        b.id,
        &draft_for(&[("AlbumB/01.flac", 1, "Other")], None, "Album"),
    );
    assert_eq!(lib.run_job().await, JobState::Done);
    let it = inbox::get(&lib.conn(), b.id).unwrap().unwrap();
    assert_eq!(it.state, ItemState::Failed);
    assert!(
        it.error.as_deref().unwrap_or("").contains("track 1"),
        "{:?}",
        it.error
    );
    assert!(!lib
        .lib_path("_Unsorted/Artist/Album/01 Other.flac")
        .exists());
    assert_eq!(lib.count("SELECT count(*) FROM tracks"), 1);
    assert!(lib.inbox_path("AlbumB/01.flac").exists());
}

/// MB リリースの album には追記しない（従来どおり別リリースとして降格）
#[tokio::test]
async fn album_with_a_release_id_is_not_adopted() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    lib.approve(
        a.id,
        &draft_for(&[("AlbumA/01.flac", 1, "One")], None, "Album"),
    );
    lib.start(true);
    assert_eq!(lib.run_job().await, JobState::Done);
    lib.conn()
        .execute("UPDATE albums SET mb_release_id = 'mbid-1'", [])
        .unwrap();

    lib.add("AlbumB/02.flac", 2, "Two", "Album", 2);
    lib.scan(2000).await;
    let b = lib.item("AlbumB").unwrap();
    lib.approve(
        b.id,
        &draft_for(&[("AlbumB/02.flac", 2, "Two")], None, "Album"),
    );
    assert_eq!(lib.run_job().await, JobState::Done);
    let it = inbox::get(&lib.conn(), b.id).unwrap().unwrap();
    assert_eq!(it.state, ItemState::Placed, "{:?}", it.error);
    assert_eq!(lib.count("SELECT count(*) FROM albums"), 2);
    assert!(lib
        .lib_path("_Unsorted/Artist/Album (2024)/02 Two.flac")
        .exists());
}

/// 入ってくる件に MUSICBRAINZ_ALBUMID があれば、宛先の非 MB の album には追記しない（別リリース）
#[tokio::test]
async fn incoming_release_id_is_not_appended_to_a_plain_album() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    lib.approve(
        a.id,
        &draft_for(&[("AlbumA/01.flac", 1, "One")], None, "Album"),
    );
    lib.start(true);
    assert_eq!(lib.run_job().await, JobState::Done);

    let p = lib.add("AlbumB/01.flac", 2, "Other", "Album", 1).unwrap();
    set_tags(&p, "flac", &[("MUSICBRAINZ_ALBUMID", &["mbid-2"])]);
    lib.scan(2000).await;
    let b = lib.item("AlbumB").unwrap();
    // 提案の宛先は無い（追記しない）
    let files = inbox::files(&lib.conn(), b.id).unwrap();
    let dest = spindle::import::inbox::destination(
        &lib.conn(),
        &lib.env(false).layout,
        &draft_for(&[("AlbumB/01.flac", 1, "Other")], None, "Album"),
        &files,
    )
    .unwrap();
    assert!(dest.is_none());
    // #1 が重なっても別リリースとして置ける（年で降格）
    lib.approve(
        b.id,
        &draft_for(&[("AlbumB/01.flac", 1, "Other")], None, "Album"),
    );
    assert_eq!(lib.run_job().await, JobState::Done);
    let it = inbox::get(&lib.conn(), b.id).unwrap().unwrap();
    assert_eq!(it.state, ItemState::Placed, "{:?}", it.error);
    assert!(lib
        .lib_path("_Unsorted/Artist/Album (2024)/01 Other.flac")
        .exists());
    assert_eq!(lib.count("SELECT count(*) FROM albums"), 2);
}

// ---------------------------------------------------------------- CD と CD 以外の区別（D-67 追記 3）

/// `disc_no` を指定した下書き（複数枚組の CD 用）
fn draft_disc(files: &[(&str, u32, &str)], disc_no: u32, album: &str) -> InboxDraft {
    let mut d = draft_for(files, None, album);
    for t in &mut d.tracks {
        t.disc_no = disc_no;
    }
    d
}

/// Inbox に 1 件置いて承認し、配置まで流す。`discid` があれば CD の件（`MUSICBRAINZ_DISCID` 付き）にする。
/// 走査の時刻は `seed` から作る（件ごとに進める）
async fn place_one(
    lib: &Lib,
    dir: &str,
    seed: u32,
    title: &str,
    track: u32,
    disc_no: u32,
    discid: Option<&str>,
) -> Option<inbox::Item> {
    let rel = format!("{dir}/{track:02}.flac");
    let p = lib.add(&rel, seed, title, "Album", track)?;
    if let Some(d) = discid {
        set_tags(&p, "flac", &[("MUSICBRAINZ_DISCID", &[d])]);
    }
    lib.scan(i64::from(seed) * 1000).await;
    let it = lib.item(dir).unwrap();
    lib.approve(
        it.id,
        &draft_disc(&[(&rel, track, title)], disc_no, "Album"),
    );
    assert_eq!(lib.run_job().await, JobState::Done);
    let it = inbox::get(&lib.conn(), it.id).unwrap().unwrap();
    assert_eq!(it.state, ItemState::Placed, "{:?}", it.error);
    Some(it)
}

/// MBID の無い CD の件は、同じ名前の CD でない album（YouTube・既存曲）には追記しない（別リリース。
/// 番号が重ならなくても年で降格する）
#[tokio::test]
async fn cd_item_is_not_appended_to_a_non_cd_album() {
    let lib = Lib::new();
    lib.start(true);
    let a = require_ffmpeg!(place_one(&lib, "AlbumA", 1, "One", 1, 1, None).await);
    let b = place_one(&lib, "AlbumB", 2, "Two", 2, 1, Some("disc-b"))
        .await
        .unwrap();
    assert_ne!(a.placed_album_id, b.placed_album_id);
    assert_eq!(lib.count("SELECT count(*) FROM albums"), 2);
    assert!(lib
        .lib_path("_Unsorted/Artist/Album (2024)/02 Two.flac")
        .exists());
}

/// 逆に、CD でない件は MBID の無い CD の album には追記しない
#[tokio::test]
async fn non_cd_item_is_not_appended_to_a_cd_album() {
    let lib = Lib::new();
    lib.start(true);
    let a = require_ffmpeg!(place_one(&lib, "AlbumA", 1, "One", 1, 1, Some("disc-a")).await);
    // 提案の段階で宛先が無い
    lib.add("AlbumB/02.flac", 2, "Two", "Album", 2).unwrap();
    lib.scan(2000).await;
    let item = lib.item("AlbumB").unwrap();
    let files = inbox::files(&lib.conn(), item.id).unwrap();
    let draft = draft_for(&[("AlbumB/02.flac", 2, "Two")], None, "Album");
    let dest =
        spindle::import::inbox::destination(&lib.conn(), &lib.env(false).layout, &draft, &files)
            .unwrap();
    assert!(dest.is_none(), "{dest:?}");
    lib.approve(item.id, &draft);
    assert_eq!(lib.run_job().await, JobState::Done);
    let b = inbox::get(&lib.conn(), item.id).unwrap().unwrap();
    assert_eq!(b.state, ItemState::Placed, "{:?}", b.error);
    assert_ne!(a.placed_album_id, b.placed_album_id);
    assert!(lib
        .lib_path("_Unsorted/Artist/Album (2024)/02 Two.flac")
        .exists());
}

/// MBID の無い複数枚組の CD は、2 枚目（まだ無い disc_no）が 1 枚目の album に合流する
/// （DiscID は 1 枚ごとに違うので、album の鍵にはしない）
#[tokio::test]
async fn next_disc_of_a_cd_without_release_id_joins_the_first_disc() {
    let lib = Lib::new();
    lib.start(true);
    let a = require_ffmpeg!(place_one(&lib, "Disc1", 1, "One", 1, 1, Some("disc-1")).await);
    let b = place_one(&lib, "Disc2", 2, "Two", 1, 2, Some("disc-2"))
        .await
        .unwrap();
    assert_eq!(a.placed_album_id, b.placed_album_id);
    assert_eq!(lib.count("SELECT count(*) FROM albums"), 1);
    assert_eq!(
        lib.count("SELECT count(*) FROM tracks WHERE missing_since IS NULL AND disc_no = 2"),
        1
    );
}

/// 同じ disc_no の CD（同名の別の盤）は合流しない（年で降格）
#[tokio::test]
async fn cd_with_the_same_disc_number_is_not_joined() {
    let lib = Lib::new();
    lib.start(true);
    let a = require_ffmpeg!(place_one(&lib, "Disc1", 1, "One", 1, 1, Some("disc-1")).await);
    let b = place_one(&lib, "Other", 2, "Two", 2, 1, Some("disc-x"))
        .await
        .unwrap();
    assert_ne!(a.placed_album_id, b.placed_album_id);
    assert!(lib
        .lib_path("_Unsorted/Artist/Album (2024)/02 Two.flac")
        .exists());
}

/// 同じ音声が CD でない album にあっても、それを「自分の成果物」とみなして CD の件をそこへ合流させない
/// （再実行の判定が CD と CD 以外の区別を素通りしない。codex 指摘）
#[tokio::test]
async fn same_audio_in_a_non_cd_album_does_not_pull_in_a_cd_item() {
    let lib = Lib::new();
    lib.start(true);
    let a = require_ffmpeg!(place_one(&lib, "AlbumA", 1, "One", 1, 1, None).await);
    // 同じ音声（seed 1）の CD の件。disc 2 なので番号は重ならない
    let rel = "Disc2/01.flac";
    let p = lib.add(rel, 1, "One", "Album", 1).unwrap();
    set_tags(&p, "flac", &[("MUSICBRAINZ_DISCID", &["disc-2"])]);
    lib.scan(5000).await;
    let it = lib.item("Disc2").unwrap();
    lib.approve(it.id, &draft_disc(&[(rel, 1, "One")], 2, "Album"));
    assert_eq!(lib.run_job().await, JobState::Done);
    let b = inbox::get(&lib.conn(), it.id).unwrap().unwrap();
    assert_eq!(b.state, ItemState::Placed, "{:?}", b.error);
    assert_ne!(a.placed_album_id, b.placed_album_id);
}

/// 同じ音声が MBID 付きの CD でない album にあっても、MBID の無い CD の件をそこへ合流させない
/// （件に MBID が無いのに、音声の一致だけで `mb:` を自分の成果物とみなさない。codex 指摘）
#[tokio::test]
async fn same_audio_in_a_release_album_does_not_pull_in_a_cd_item() {
    let lib = Lib::new();
    lib.start(true);
    let a = require_ffmpeg!(place_one(&lib, "AlbumA", 1, "One", 1, 1, None).await);
    lib.conn()
        .execute("UPDATE albums SET mb_release_id = 'mbid-1'", [])
        .unwrap();
    let rel = "Disc2/01.flac";
    let p = lib.add(rel, 1, "One", "Album", 1).unwrap();
    set_tags(&p, "flac", &[("MUSICBRAINZ_DISCID", &["disc-2"])]);
    lib.scan(5000).await;
    let it = lib.item("Disc2").unwrap();
    lib.approve(it.id, &draft_disc(&[(rel, 1, "One")], 2, "Album"));
    assert_eq!(lib.run_job().await, JobState::Done);
    let b = inbox::get(&lib.conn(), it.id).unwrap().unwrap();
    assert_eq!(b.state, ItemState::Placed, "{:?}", b.error);
    assert_ne!(a.placed_album_id, b.placed_album_id);
}

/// 同じ音声が 2 つの album（CD でない album と、自分が置いた CD の album）にある状態で置き直しても、
/// 自分の album を見つけて冪等に終わる（候補を全部見て、入れられるものが 1 つなら採る。codex 指摘）
#[tokio::test]
async fn rerun_finds_its_own_cd_album_among_same_audio_candidates() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "Album", 1));
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    lib.approve(
        a.id,
        &draft_for(&[("AlbumA/01.flac", 1, "One")], None, "Album"),
    );
    let env = lib.env(false);
    let item = inbox::get(&lib.conn(), a.id).unwrap().unwrap();
    spindle::import::inbox::place_item(&env, &item, &CancellationToken::new())
        .await
        .unwrap();
    // 同じ音声（seed 1）の CD の件（disc 2）を置く → 年で降格した別の album
    let rel = "Disc2/01.flac";
    let add_cd = |lib: &Lib| {
        let p = lib.add(rel, 1, "One", "Album", 1).unwrap();
        set_tags(&p, "flac", &[("MUSICBRAINZ_DISCID", &["disc-2"])]);
    };
    add_cd(&lib);
    lib.scan(2000).await;
    let b = lib.item("Disc2").unwrap();
    let draft = draft_disc(&[(rel, 1, "One")], 2, "Album");
    lib.approve(b.id, &draft);
    let item = inbox::get(&lib.conn(), b.id).unwrap().unwrap();
    let first = spindle::import::inbox::place_item(&env, &item, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(lib.count("SELECT count(*) FROM albums"), 2);
    // Inbox を消費する前に落ちたことにする（行は残し、原本を戻して placing に）
    add_cd(&lib);
    lib.scan(2001).await;
    let b2 = lib.item("Disc2").unwrap();
    lib.approve(b2.id, &draft);
    inbox::set_state(&lib.conn(), b2.id, ItemState::Placing, None, 3).unwrap();
    lib.start(false);
    assert_eq!(lib.run_job().await, JobState::Done);
    let it = inbox::get(&lib.conn(), b2.id).unwrap().unwrap();
    assert_eq!(it.state, ItemState::Placed, "{:?}", it.error);
    assert_eq!(it.placed_album_id, Some(first.album_id));
    assert_eq!(lib.count("SELECT count(*) FROM albums"), 2);
    assert_eq!(
        lib.count("SELECT count(*) FROM tracks WHERE missing_since IS NULL"),
        2
    );
}

/// 計画の後・登録の前に追記先が CD でなくなった（タグから DiscID が消えた）ら、CD の件は登録しない
/// （登録のトランザクションで CD と CD 以外の区別を再検証する。codex 指摘）
#[tokio::test]
async fn cd_item_is_not_registered_when_the_album_stops_being_a_cd_after_planning() {
    let lib = Lib::new();
    lib.start(true);
    let a = require_ffmpeg!(place_one(&lib, "Disc1", 1, "One", 1, 1, Some("disc-1")).await);
    let rel = "Disc2/01.flac";
    let p = lib.add(rel, 2, "Two", "Album", 1).unwrap();
    set_tags(&p, "flac", &[("MUSICBRAINZ_DISCID", &["disc-2"])]);
    lib.scan(5000).await;
    let it = lib.item("Disc2").unwrap();
    lib.approve(it.id, &draft_disc(&[(rel, 1, "Two")], 2, "Album"));
    let db_path = lib.db_path.clone();
    let album_id = a.placed_album_id.unwrap();
    let hook: PlaceHook = Arc::new(move || {
        Connection::open(&db_path)
            .unwrap()
            .execute(
                "DELETE FROM track_tags WHERE key = 'MUSICBRAINZ_DISCID'
                   AND track_id IN (SELECT id FROM tracks WHERE album_id = ?1)",
                [album_id],
            )
            .unwrap();
    });
    let env = lib.env_with(false, Some(hook));
    let item = inbox::get(&lib.conn(), it.id).unwrap().unwrap();
    let err = spindle::import::inbox::place_item(&env, &item, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(err, spindle::import::inbox::InboxError::Conflict(_)),
        "{err}"
    );
    assert_eq!(
        lib.count(&format!(
            "SELECT count(*) FROM tracks WHERE album_id = {album_id} AND missing_since IS NULL"
        )),
        1
    );
}

// ---------------------------------------------------------------- 承認画面で選んだ MusicBrainz のリリース（P4-21）

/// 下書きのリリース ID はタグ（MUSICBRAINZ_ALBUMID / RELEASEGROUPID）に書かれ、album の mb_release_id になる
#[tokio::test]
async fn release_ids_in_draft_are_written_to_tags_and_the_album() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    let mut d = draft_for(&[("AlbumA/01.flac", 1, "One")], None, "Album");
    d.release_id = Some("f1223d63-f359-457d-b935-fc27eb24a6de".into());
    d.release_group_id = Some("0b3a4c5d-1111-2222-3333-444455556666".into());
    lib.approve(a.id, &d);
    lib.start(true);
    assert_eq!(lib.run_job().await, JobState::Done);
    let it = inbox::get(&lib.conn(), a.id).unwrap().unwrap();
    assert_eq!(it.state, ItemState::Placed, "{:?}", it.error);
    let p = lib.lib_path("_Unsorted/Artist/Album/01 One.flac");
    let af = spindle::domain::tags::read_audio_file(std::fs::File::open(&p).unwrap(), Some("flac"))
        .unwrap();
    assert_eq!(
        af.tags.first("MUSICBRAINZ_ALBUMID"),
        Some("f1223d63-f359-457d-b935-fc27eb24a6de")
    );
    assert_eq!(
        af.tags.first("MUSICBRAINZ_RELEASEGROUPID"),
        Some("0b3a4c5d-1111-2222-3333-444455556666")
    );
    let mb: Option<String> = lib
        .conn()
        .query_row("SELECT mb_release_id FROM albums", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mb.as_deref(), Some("f1223d63-f359-457d-b935-fc27eb24a6de"));
}

/// 同じリリースの 2 枚目は、承認画面で同じ MBID を選べば 1 枚目の album に合流する（`mb:` のキー）
#[tokio::test]
async fn second_disc_with_the_same_release_id_joins_the_first() {
    let lib = Lib::new();
    lib.start(true);
    let place =
        |dir: &'static str, seed: u32, title: &'static str, disc: u32, discid: &'static str| {
            let lib = &lib;
            async move {
                let rel = format!("{dir}/01.flac");
                let p = lib.add(&rel, seed, title, "Album", 1)?;
                set_tags(&p, "flac", &[("MUSICBRAINZ_DISCID", &[discid])]);
                lib.scan(i64::from(seed) * 1000).await;
                let it = lib.item(dir).unwrap();
                let mut d = draft_disc(&[(&rel, 1, title)], disc, "Album");
                d.release_id = Some("f1223d63-f359-457d-b935-fc27eb24a6de".into());
                lib.approve(it.id, &d);
                assert_eq!(lib.run_job().await, JobState::Done);
                inbox::get(&lib.conn(), it.id).unwrap()
            }
        };
    let a = require_ffmpeg!(place("Disc1", 1, "One", 1, "disc-1").await);
    let b = place("Disc2", 2, "Two", 2, "disc-2").await.unwrap();
    assert_eq!(a.state, ItemState::Placed, "{:?}", a.error);
    assert_eq!(b.state, ItemState::Placed, "{:?}", b.error);
    assert_eq!(a.placed_album_id, b.placed_album_id);
    assert_eq!(lib.count("SELECT count(*) FROM albums"), 1);
}

/// ARTIST が多値のファイルは、下書きの `keep_artists` が true なら（現在の個数に関係なく）触れず、
/// false なら `artist` の 1 値で上書きする。`keep_artists` の無い旧下書きは先頭の値のままなら保つ
/// （プラグインの artists の写像を Library まで運ぶ。SPEC §7.7 / §7.8、D-70、P4-4）
#[tokio::test]
async fn keep_artists_decides_whether_multi_valued_artist_is_preserved_on_placement() {
    let lib = Lib::new();
    let p1 = require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    let p2 = lib.add("AlbumA/02.flac", 2, "Two", "A", 2).unwrap();
    let p3 = lib.add("AlbumA/03.flac", 3, "Three", "A", 3).unwrap();
    let p4 = lib.add("AlbumA/04.flac", 4, "Four", "A", 4).unwrap();
    let p5 = lib.add("AlbumA/05.flac", 5, "Five", "A", 5).unwrap();
    let p6 = lib.add("AlbumA/06.flac", 6, "Six", "A", 6).unwrap();
    for p in [&p1, &p2, &p3] {
        set_tags(p, "flac", &[("ARTIST", &["A", "B"])]);
    }
    set_tags(&p4, "flac", &[("ARTIST", &["X"])]);
    // 空白だけの値も 1 値として数える（["A", " "] は多値）。旧規則の実効アーティストはアルバムアーティストに倒れる
    set_tags(&p5, "flac", &[("ARTIST", &["A", " "])]);
    set_tags(&p6, "flac", &[("ARTIST", &["Artist", " B "])]);
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    let mut d = draft_for(
        &[
            ("AlbumA/01.flac", 1, "One"),
            ("AlbumA/02.flac", 2, "Two"),
            ("AlbumA/03.flac", 3, "Three"),
            ("AlbumA/04.flac", 4, "Four"),
            ("AlbumA/05.flac", 5, "Five"),
            ("AlbumA/06.flac", 6, "Six"),
        ],
        None,
        "Album",
    );
    // 提案は値をそのまま結合し、空値があっても多値
    let proposal =
        spindle::import::inbox::proposal(&inbox::files(&lib.conn(), a.id).unwrap(), &[], &[]);
    let by_path = |rel: &str| {
        proposal
            .tracks
            .iter()
            .find(|t| t.rel_path == rel)
            .unwrap()
            .clone()
    };
    assert_eq!(by_path("AlbumA/05.flac").artist, "A;  ");
    assert_eq!(by_path("AlbumA/05.flac").keep_artists, Some(true));
    assert_eq!(by_path("AlbumA/06.flac").artist, "Artist;  B ");
    assert_eq!(by_path("AlbumA/04.flac").keep_artists, Some(false));
    // 提案どおり（多値を保つ。artist は表示用）
    d.tracks[0].artist = "A; B".into();
    d.tracks[0].keep_artists = Some(true);
    // チェックを外して 1 値に
    d.tracks[1].artist = "C".into();
    d.tracks[1].keep_artists = Some(false);
    // 旧下書き（欄なし）: 先頭の値のまま = 保つ
    d.tracks[2].artist = "A".into();
    d.tracks[2].keep_artists = None;
    // true はファイルが 1 値でも触れない（承認後に変わった経路の安全側）
    d.tracks[3].artist = "Y".into();
    d.tracks[3].keep_artists = Some(true);
    // 旧下書き: ["A", " "] の先頭 "A" のまま = 保つ（空白の値を落とすと 1 値扱いになって潰れる）
    d.tracks[4].artist = "A".into();
    d.tracks[4].keep_artists = None;
    // 旧下書き: artist 空 → 実効はアルバムアーティスト "Artist" = 先頭値 → 保つ
    d.tracks[5].artist = String::new();
    d.tracks[5].keep_artists = None;
    lib.approve(a.id, &d);
    lib.start(true);
    assert_eq!(lib.run_job().await, JobState::Done);
    let read = |rel: &str| {
        spindle::domain::tags::read_audio_file(
            std::fs::File::open(lib.lib_path(rel)).unwrap(),
            Some("flac"),
        )
        .unwrap()
    };
    let artists = |rel: &str| {
        read(rel)
            .tags
            .values("ARTIST")
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(artists("_Unsorted/Artist/Album/01 One.flac"), ["A", "B"]);
    assert_eq!(artists("_Unsorted/Artist/Album/02 Two.flac"), ["C"]);
    assert_eq!(artists("_Unsorted/Artist/Album/03 Three.flac"), ["A", "B"]);
    assert_eq!(artists("_Unsorted/Artist/Album/04 Four.flac"), ["X"]);
    assert_eq!(artists("_Unsorted/Artist/Album/05 Five.flac"), ["A", " "]);
    assert_eq!(
        artists("_Unsorted/Artist/Album/06 Six.flac"),
        ["Artist", " B "]
    );
    let disp: String = lib
        .conn()
        .query_row(
            "SELECT artist_display FROM tracks WHERE rel_path = '_Unsorted/Artist/Album/01 One.flac'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(disp, "A, B");
}

/// 走査は Library の pick_embedded と同じ規則（front cover 優先）で選んだ画像を `PICTURE` の先頭に
/// 置く（承認画面の代表画像。P4-4）
#[tokio::test]
async fn scan_puts_the_front_cover_first_among_pictures() {
    use lofty::picture::{MimeType, Picture, PictureType};
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    let other = jpeg(b"other");
    let front = jpeg(b"front");
    let pic = |bytes: &[u8], t: PictureType| {
        Picture::unchecked(bytes.to_vec())
            .pic_type(t)
            .mime_type(MimeType::Jpeg)
            .build()
    };
    common::retag(&p, |t| {
        t.push_picture(pic(&other, PictureType::Other));
        t.push_picture(pic(&front, PictureType::CoverFront));
    });
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    let files = inbox::files(&lib.conn(), a.id).unwrap();
    let hashes: Vec<(String, String)> = spindle::import::inbox::picture_hashes(&files[0].tags)
        .into_iter()
        .map(|(m, h)| (m.to_owned(), h.to_owned()))
        .collect();
    let hex = |b: &[u8]| {
        spindle::media::artwork::ArtworkStore::hex(&spindle::media::artwork::ArtworkStore::hash_of(
            b,
        ))
    };
    assert_eq!(
        hashes,
        [
            ("image/jpeg".to_owned(), hex(&front)),
            ("image/jpeg".to_owned(), hex(&other))
        ]
    );
}

// ---------------------------------------------------------------- 配置直後のアートワーク解決（P3-4、D-68）

/// 最小の JPEG（SOF0 1x1 + COM）。内容は `tag` で変える
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

fn set_picture(path: &std::path::Path, bytes: Vec<u8>) {
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

/// 配置の直後に、その album のアートワークを埋め込み画像から解決して thumbnail を投入する
/// （次のスキャンを待たない）
#[tokio::test]
async fn placement_resolves_album_artwork_and_enqueues_thumbnail() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    let pic = jpeg(b"front");
    set_picture(&p, pic.clone());
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    lib.approve(
        a.id,
        &draft_for(&[("AlbumA/01.flac", 1, "One")], None, "Album"),
    );
    lib.start(true);
    assert_eq!(lib.run_job().await, JobState::Done);
    let (artwork_id, resolved_at, sha): (Option<i64>, Option<i64>, Option<Vec<u8>>) = lib
        .conn()
        .query_row(
            "SELECT a.artwork_id, a.artwork_resolved_at, w.sha256
               FROM albums a LEFT JOIN artwork w ON w.id = a.artwork_id
              WHERE a.rel_dir = '_Unsorted/Artist/Album'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert!(artwork_id.is_some(), "配置直後にアートワークが決まる");
    assert!(resolved_at.is_some());
    assert_eq!(sha, Some(ArtworkStore::hash_of(&pic).to_vec()));
    assert_eq!(
        lib.count("SELECT count(*) FROM jobs WHERE type = 'thumbnail' AND state = 'queued'"),
        1
    );
    assert!(
        lib.dir.path().join("thumbs").exists(),
        "原画像がキャッシュに置かれる"
    );

    // 画像の無い件は「画像なし」で解決され、thumbnail は投入されない（次のスキャンで読み直さない）
    lib.add("AlbumB/01.flac", 2, "One", "B", 1);
    lib.scan(2000).await;
    let b = lib.item("AlbumB").unwrap();
    lib.approve(
        b.id,
        &draft_for(&[("AlbumB/01.flac", 1, "One")], None, "Album B"),
    );
    assert_eq!(lib.run_job().await, JobState::Done);
    let (artwork_id, resolved_at): (Option<i64>, Option<i64>) = lib
        .conn()
        .query_row(
            "SELECT artwork_id, artwork_resolved_at FROM albums WHERE rel_dir = '_Unsorted/Artist/Album B'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(artwork_id.is_none());
    assert!(resolved_at.is_some());
    assert_eq!(
        lib.count("SELECT count(*) FROM jobs WHERE type = 'thumbnail'"),
        1
    );
}

/// 同梱カバー画像は埋め込み画像より優先され、名前の優先順位（cover > folder）も Phase 1 と同じ
#[tokio::test]
async fn placement_prefers_the_bundled_cover_over_embedded_pictures() {
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    set_picture(&p, jpeg(b"embedded"));
    let cover = jpeg(b"cover");
    std::fs::write(lib.inbox_path("AlbumA/folder.jpg"), jpeg(b"folder")).unwrap();
    std::fs::write(lib.inbox_path("AlbumA/cover.jpg"), &cover).unwrap();
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    lib.approve(
        a.id,
        &draft_for(&[("AlbumA/01.flac", 1, "One")], None, "Album"),
    );
    lib.start(true);
    assert_eq!(lib.run_job().await, JobState::Done);
    let (sha, origin, cover_size): (Vec<u8>, String, Option<i64>) = lib
        .conn()
        .query_row(
            "SELECT w.sha256, w.origin, a.cover_size FROM albums a JOIN artwork w ON w.id = a.artwork_id
              WHERE a.rel_dir = '_Unsorted/Artist/Album'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(sha, ArtworkStore::hash_of(&cover).to_vec());
    assert_eq!(origin, "file");
    assert_eq!(cover_size, Some(cover.len() as i64));
}

/// 解決は library の排他を持ったまま行う（並行する scan の Phase 4 / 5 と結果が交錯しない）。
/// 探索が I/O で失敗したら「なし」と確定せず、予約（artwork_resolved_at = NULL）を残す
#[tokio::test]
async fn artwork_is_resolved_under_the_library_mutex_and_io_failure_keeps_the_reservation() {
    use std::os::unix::fs::PermissionsExt as _;
    let lib = Lib::new();
    let p = require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    set_picture(&p, jpeg(b"front"));
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    lib.approve(
        a.id,
        &draft_for(&[("AlbumA/01.flac", 1, "One")], None, "Album"),
    );
    // 解決の直前: 排他がまだ取られていることを見て、ディレクトリを読めなくする
    let db_path = lib.db_path.clone();
    let album_dir = lib.lib_path("_Unsorted/Artist/Album");
    let held = Arc::new(std::sync::atomic::AtomicI64::new(-1));
    let held_in = held.clone();
    let hook: PlaceHook = Arc::new(move || {
        let n: i64 = Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT count(*) FROM job_mutexes WHERE name = 'library'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        held_in.store(n, std::sync::atomic::Ordering::SeqCst);
        std::fs::set_permissions(&album_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    });
    lib.start_with(lib.env_full(true, None, Some(hook)));
    let st = lib.run_job().await;
    std::fs::set_permissions(
        lib.lib_path("_Unsorted/Artist/Album"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert_eq!(st, JobState::Done);
    assert_eq!(
        held.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "排他を持ったまま解決する"
    );
    let (artwork_id, resolved_at): (Option<i64>, Option<i64>) = lib
        .conn()
        .query_row(
            "SELECT artwork_id, artwork_resolved_at FROM albums WHERE rel_dir = '_Unsorted/Artist/Album'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(artwork_id.is_none());
    assert!(
        resolved_at.is_none(),
        "I/O で失敗したら予約を残す（次のスキャンが拾う）"
    );
    assert_eq!(
        lib.count("SELECT count(*) FROM jobs WHERE type = 'thumbnail'"),
        0
    );
    // 排他は解放されている
    assert_eq!(lib.count("SELECT count(*) FROM job_mutexes"), 0);
}

/// 既存 album への追記でも解決し直す（画像が無かった album に画像付きの曲が入れば付く）
#[tokio::test]
async fn appending_a_track_with_a_picture_resolves_an_album_without_one() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    lib.approve(
        a.id,
        &draft_for(&[("AlbumA/01.flac", 1, "One")], None, "Album"),
    );
    lib.start(true);
    assert_eq!(lib.run_job().await, JobState::Done);
    let p = lib.add("AlbumB/02.flac", 2, "Two", "Album", 2).unwrap();
    let pic = jpeg(b"second");
    set_picture(&p, pic.clone());
    lib.scan(2000).await;
    let b = lib.item("AlbumB").unwrap();
    lib.approve(
        b.id,
        &draft_for(&[("AlbumB/02.flac", 2, "Two")], None, "Album"),
    );
    assert_eq!(lib.run_job().await, JobState::Done);
    assert_eq!(lib.count("SELECT count(*) FROM albums"), 1);
    let sha: Option<Vec<u8>> = lib
        .conn()
        .query_row(
            "SELECT w.sha256 FROM albums a JOIN artwork w ON w.id = a.artwork_id
              WHERE a.rel_dir = '_Unsorted/Artist/Album'",
            [],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    assert_eq!(sha, Some(ArtworkStore::hash_of(&pic).to_vec()));
}

/// P4-16: 購読由来の件（サイドカーに subscription_id）を配置したら、購読の追記先を束ね、同期を要求して
/// `playlist_sync` を投入する（番号揃えの後続。D-78）
#[tokio::test]
async fn placing_a_subscription_item_binds_the_album_and_requests_a_sync() {
    use spindle::db::subscriptions::{self, NewSubscription, WriteOutcome};
    use spindle::import::sidecar::Sidecar;
    let lib = Lib::new();
    require_ffmpeg!(lib.add(
        "youtube/Artist/Album/20260901 One [abc].flac",
        1,
        "One",
        "Album",
        1,
    ));
    lib.conn()
        .execute("INSERT INTO categories (name) VALUES ('Rock')", [])
        .unwrap();
    let sub = match subscriptions::insert(
        &lib.conn(),
        &NewSubscription {
            list_id: "PL1".into(),
            url: "https://www.youtube.com/playlist?list=PL1".into(),
            albumartist: "Artist".into(),
            album: "Album".into(),
            category: Some("Rock".into()),
            align: true,
            enabled: true,
            max_enqueue: 50,
        },
        1,
    )
    .unwrap()
    {
        WriteOutcome::Ok(id) => id,
        other => panic!("{other:?}"),
    };
    let dir = spindle::domain::relpath::RelPath::parse("youtube/Artist/Album").unwrap();
    let mut entry = sidecar_entry();
    entry.subscription_id = Some(sub);
    entry.position = Some(1);
    Sidecar::upsert(
        &lib.inbox,
        &dir,
        Some("Rock"),
        "20260901 One [abc].flac",
        entry,
    )
    .unwrap();
    lib.scan(1000).await;
    let item = lib.item("youtube/Artist/Album").unwrap();
    lib.approve(
        item.id,
        &draft_for(
            &[("youtube/Artist/Album/20260901 One [abc].flac", 1, "One")],
            Some("Rock"),
            "Album",
        ),
    );
    lib.start(true);
    assert_eq!(lib.run_job().await, JobState::Done);
    let it = inbox::get(&lib.conn(), item.id).unwrap().unwrap();
    assert_eq!(it.state, ItemState::Placed, "{:?}", it.error);
    let s = subscriptions::get(&lib.conn(), sub).unwrap().unwrap();
    assert_eq!(s.album_id, it.placed_album_id, "追記先を束ねた");
    assert!(s.sync_requested_at.is_some(), "同期を要求した（latch）");
    assert_eq!(
        lib.count(&format!(
            "SELECT count(*) FROM jobs WHERE type = 'playlist_sync' AND dedup_key = 'playlist_sync:{sub}' AND state = 'queued'"
        )),
        1
    );
}

// ---------------------------------------------------------------- 変化の検知（P4-18）

/// Inbox の指紋は音声ファイルの集合（パス・inode・size・mtime・ctime）で決まる。非音声・空ディレクトリは
/// 無視。消せば元に戻る
#[tokio::test]
async fn fingerprint_changes_only_with_audio_files() {
    let lib = Lib::new();
    let f0 = spindle::import::inbox::fingerprint(&lib.inbox).unwrap();
    std::fs::write(lib.inbox_path("cover.jpg"), b"jpg").unwrap();
    std::fs::create_dir(lib.inbox_path("Empty")).unwrap();
    assert_eq!(spindle::import::inbox::fingerprint(&lib.inbox).unwrap(), f0);
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    let f1 = spindle::import::inbox::fingerprint(&lib.inbox).unwrap();
    assert_ne!(f1, f0);
    // 内容を書き直せば変わる（size / mtime / inode のどれか）
    lib.add(
        "AlbumA/01.flac",
        1,
        "One (long title to change size)",
        "A",
        1,
    );
    let f2 = spindle::import::inbox::fingerprint(&lib.inbox).unwrap();
    assert_ne!(f2, f1);
    std::fs::remove_file(lib.inbox_path("AlbumA/01.flac")).unwrap();
    assert_eq!(spindle::import::inbox::fingerprint(&lib.inbox).unwrap(), f0);
}

/// 監視は「起動直後」「指紋が変わった」「配置待ち・期限切れの placed がある」ときだけ inbox ジョブを
/// 投入する。変化が無ければ毎分ジョブ行を作らない（一覧を汚さない）
#[tokio::test]
async fn watcher_enqueues_only_when_inbox_changes_or_items_need_attention() {
    use spindle::jobs::handlers::inbox::{spawn_watcher_with, WatchStatus};
    let lib = Lib::new();
    require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    let status = Arc::new(WatchStatus::default());
    let tick = Duration::from_millis(30);
    let handle = spawn_watcher_with(
        lib.jobs.clone(),
        lib.inbox.clone(),
        tick,
        status.clone(),
        lib.shutdown.clone(),
    );
    let count = || -> i64 {
        lib.conn()
            .query_row("SELECT count(*) FROM jobs WHERE type = 'inbox'", [], |r| {
                r.get(0)
            })
            .unwrap()
    };
    let finish_all = || {
        lib.conn()
            .execute(
                "UPDATE jobs SET state = 'done', finished_at = 1 WHERE type = 'inbox'",
                [],
            )
            .unwrap();
    };
    // 回数ベースだと遅いランナーで足りないので、経過時間で待つ
    let wait_count = |n: i64| async move {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            if count() == n {
                return;
            }
            tokio::time::sleep(tick).await;
        }
        panic!("inbox ジョブが {n} 件にならない（{}）", count());
    };
    // 起動直後に 1 回
    wait_count(1).await;
    finish_all();
    tokio::time::sleep(tick * 10).await;
    assert_eq!(count(), 1, "変化が無ければ投入しない");
    assert!(status.checked_at() > 0, "確認した時刻を記録する");
    // 音声を置いたら 1 回だけ
    lib.add("AlbumB/01.flac", 2, "Two", "B", 1);
    wait_count(2).await;
    finish_all();
    tokio::time::sleep(tick * 10).await;
    assert_eq!(count(), 2);
    // 非音声だけなら投入しない
    std::fs::write(lib.inbox_path("AlbumB/cover.jpg"), b"jpg").unwrap();
    tokio::time::sleep(tick * 10).await;
    assert_eq!(count(), 2);
    // 配置待ち（承認済み）が残っていれば投入する（承認 API の投入が Requeue で戻った後の保険）
    lib.conn()
        .execute(
            "INSERT INTO inbox_items (id, rel_dir, rel_dir_key, state, detected_at, seen_at)
             VALUES (900, 'x', 'x', 'approved', 1, 1)",
            [],
        )
        .unwrap();
    wait_count(3).await;
    lib.conn()
        .execute(
            "UPDATE inbox_items SET state = 'pending' WHERE id = 900",
            [],
        )
        .unwrap();
    finish_all();
    tokio::time::sleep(tick * 10).await;
    assert_eq!(count(), 3);
    // 期限切れの placed（片付けが要る）も投入の理由になる
    lib.conn()
        .execute(
            "UPDATE inbox_items SET state = 'placed', placed_at = 1 WHERE id = 900",
            [],
        )
        .unwrap();
    wait_count(4).await;
    lib.conn()
        .execute("DELETE FROM inbox_items WHERE id = 900", [])
        .unwrap();
    finish_all();
    tokio::time::sleep(tick * 10).await;
    assert_eq!(count(), 4);
    // 前のジョブがまだ走っていて Duplicate なら、次の周回で投入し直す（変化を取りこぼさない）
    lib.add("AlbumC/01.flac", 3, "Three", "C", 1);
    wait_count(5).await;
    lib.add("AlbumC/02.flac", 4, "Four", "C", 2);
    tokio::time::sleep(tick * 10).await;
    assert_eq!(count(), 5, "queued のままなら重複投入しない");
    finish_all();
    wait_count(6).await;
    lib.shutdown.cancel();
    handle.await.unwrap();
}

// ---------------------------------------------------------------- CD の吸い出し（P2-5、D-67 追記）

/// CD の件（3 本 + rip.log + サイドカーの `rip`）を Inbox に置く。CTDB は 2 本目だけ不一致
async fn add_cd_item(lib: &Lib) -> Option<inbox::Item> {
    use spindle::import::sidecar::Sidecar;
    lib.add("CD/01.flac", 1, "", "", 1)?;
    lib.add("CD/02.flac", 2, "", "", 2);
    lib.add("CD/03.flac", 3, "", "", 3);
    std::fs::write(lib.inbox_path("CD/rip.log"), "spindle rip log v1\n").unwrap();
    let mut s = Sidecar::default();
    s.rip = Some(common::rip_entry(
        &["01.flac", "02.flac", "03.flac"],
        &[true, false, true],
    ));
    s.write(
        &lib.inbox,
        &spindle::domain::relpath::RelPath::parse("CD").unwrap(),
    )
    .unwrap();
    lib.scan(1000).await;
    lib.item("CD")
}

/// 承認で番号とタイトルを入れ替えても、検証記録はファイル名で結びついた行に付く。出自は cd_rip、
/// 記録は source = rip、log_path は移した rip.log の Library 相対、album gain は on で提案される
#[tokio::test]
async fn cd_item_is_placed_with_verification_bound_by_file_name() {
    let lib = Lib::new();
    let item = require_ffmpeg!(add_cd_item(&lib).await);
    let files = inbox::files(&lib.conn(), item.id).unwrap();
    let proposed = spindle::import::inbox::propose(
        &lib.conn(),
        &lib.inbox,
        &lib.env(false).layout,
        &item,
        &files,
        &[],
        &[],
    )
    .unwrap();
    assert!(proposed.draft.album_gain, "CD は album gain on で提案する");
    assert!(
        !proposed
            .warnings
            .iter()
            .any(|w| w.contains("吸い出しの記録")),
        "{:?}",
        proposed.warnings
    );

    // 01 と 03 の番号を入れ替え、タイトルも付ける
    let mut draft = draft_for(
        &[
            ("CD/01.flac", 3, "Uno"),
            ("CD/02.flac", 2, "Dos"),
            ("CD/03.flac", 1, "Tres"),
        ],
        None,
        "Disc",
    );
    draft.album_gain = true;
    lib.approve(item.id, &draft);
    lib.start(false);
    assert_eq!(lib.run_job().await, JobState::Done);
    let it = inbox::get(&lib.conn(), item.id).unwrap().unwrap();
    assert_eq!(it.state, ItemState::Placed, "{:?}", it.error);

    let c = lib.conn();
    let row = |rel: &str| -> (String, String, i64) {
        c.query_row(
            "SELECT t.source_type, t.verification, tv.ctdb_crc
               FROM tracks t JOIN track_verifications tv ON tv.track_id = t.id
              WHERE t.rel_path = ?1",
            [rel],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    };
    // 01.flac（記録の 0 番、CRC 100、一致）は 03 Uno に、03.flac（2 番、102、一致）は 01 Tres に
    assert_eq!(
        row("_Unsorted/Artist/Disc/03 Uno.flac"),
        ("cd_rip".into(), "verified_ctdb".into(), 100)
    );
    assert_eq!(
        row("_Unsorted/Artist/Disc/02 Dos.flac"),
        ("cd_rip".into(), "mismatch".into(), 101)
    );
    assert_eq!(
        row("_Unsorted/Artist/Disc/01 Tres.flac"),
        ("cd_rip".into(), "verified_ctdb".into(), 102)
    );
    let (source, method, result, disc_no, job_id, log_path): (
        String,
        String,
        String,
        i64,
        Option<i64>,
        Option<String>,
    ) = c
        .query_row(
            "SELECT source, method, result, disc_no, job_id, log_path FROM album_verifications",
            [],
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
    assert_eq!(
        (
            source.as_str(),
            method.as_str(),
            result.as_str(),
            disc_no,
            job_id
        ),
        ("rip", "ctdb", "mismatch", 1, None)
    );
    assert_eq!(log_path.as_deref(), Some("_Unsorted/Artist/Disc/rip.log"));
    // drive_offset は PCM に当てた読み取りオフセット（レポートの read_offset）、detected_offset は
    // そこからの残りのずれ
    let (drive_offset, detected_offset): (Option<i64>, Option<i64>) = c
        .query_row(
            "SELECT drive_offset, detected_offset FROM album_verifications",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((drive_offset, detected_offset), (Some(6), Some(0)));
    assert!(lib.lib_path("_Unsorted/Artist/Disc/rip.log").exists());
    assert_eq!(
        lib.count("SELECT album_gain FROM albums WHERE rel_dir = '_Unsorted/Artist/Disc'"),
        1
    );
    // サイドカーは Library へ持っていかず、Inbox の件ごと消える
    assert!(!lib.inbox_path("CD").exists());
}

/// 記録が件のファイルと 1 対 1 に対応しなければ配置しない（件は failed、Library は空のまま）
#[tokio::test]
async fn cd_item_is_not_placed_when_the_rip_record_does_not_match() {
    let lib = Lib::new();
    let item = require_ffmpeg!(add_cd_item(&lib).await);
    // 件に 4 本目が足された（記録に無いファイル）
    lib.add("CD/04.flac", 4, "", "", 4);
    lib.scan(1001).await;
    let files = inbox::files(&lib.conn(), item.id).unwrap();
    assert_eq!(files.len(), 4);
    let item = lib.item("CD").unwrap();
    let proposed = spindle::import::inbox::propose(
        &lib.conn(),
        &lib.inbox,
        &lib.env(false).layout,
        &item,
        &files,
        &[],
        &[],
    )
    .unwrap();
    assert!(
        proposed
            .warnings
            .iter()
            .any(|w| w.contains("吸い出しの記録と件が合わない")),
        "{:?}",
        proposed.warnings
    );
    lib.approve(
        item.id,
        &draft_for(
            &[
                ("CD/01.flac", 1, "A"),
                ("CD/02.flac", 2, "B"),
                ("CD/03.flac", 3, "C"),
                ("CD/04.flac", 4, "D"),
            ],
            None,
            "Disc",
        ),
    );
    lib.start(false);
    assert_eq!(lib.run_job().await, JobState::Done);
    let it = inbox::get(&lib.conn(), item.id).unwrap().unwrap();
    assert_eq!(it.state, ItemState::Failed);
    assert!(
        it.error
            .as_deref()
            .unwrap_or("")
            .contains("吸い出しの記録と件が合わない"),
        "{:?}",
        it.error
    );
    assert_eq!(lib.count("SELECT count(*) FROM tracks"), 0);
    assert_eq!(lib.count("SELECT count(*) FROM album_verifications"), 0);
    assert!(!lib.lib_path("_Unsorted").exists());
    assert!(lib.inbox_path("CD/01.flac").exists());
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
    }
}

/// 登録の commit の後・Inbox を消す前に落ちた状況: Inbox に音声とサイドカーが残り、走査が件を
/// pending に戻す。再承認で同じファイルと行を採用しても、検証記録は二重にならない
#[tokio::test]
async fn replacing_a_cd_item_after_a_crash_does_not_duplicate_verification_records() {
    let lib = Lib::new();
    let item = require_ffmpeg!(add_cd_item(&lib).await);
    let saved = lib.dir.path().join("saved-cd");
    copy_dir(&lib.inbox_path("CD"), &saved);
    let draft = draft_for(
        &[
            ("CD/01.flac", 1, "Uno"),
            ("CD/02.flac", 2, "Dos"),
            ("CD/03.flac", 3, "Tres"),
        ],
        None,
        "Disc",
    );
    lib.approve(item.id, &draft);
    lib.start(false);
    assert_eq!(lib.run_job().await, JobState::Done);
    assert_eq!(
        inbox::get(&lib.conn(), item.id).unwrap().unwrap().state,
        ItemState::Placed
    );
    let before = (
        lib.count("SELECT count(*) FROM album_verifications"),
        lib.count("SELECT count(*) FROM track_verifications"),
    );
    assert_eq!(before, (1, 3));

    // Inbox を消す前に落ちた（原本とサイドカーが残った）
    copy_dir(&saved, &lib.inbox_path("CD"));
    lib.scan(5000).await;
    let again = lib.item("CD").unwrap();
    assert_eq!(again.id, item.id);
    assert_eq!(again.state, ItemState::Pending);
    lib.approve(again.id, &draft);
    assert_eq!(lib.run_job().await, JobState::Done);
    let it = inbox::get(&lib.conn(), item.id).unwrap().unwrap();
    assert_eq!(it.state, ItemState::Placed, "{:?}", it.error);
    assert_eq!(
        (
            lib.count("SELECT count(*) FROM album_verifications"),
            lib.count("SELECT count(*) FROM track_verifications"),
        ),
        before
    );
    assert_eq!(lib.count("SELECT count(*) FROM tracks"), 3);
    assert_eq!(
        lib.count("SELECT count(*) FROM tracks WHERE source_type = 'cd_rip'"),
        3
    );
}

/// 読んだ後にサイドカーが差し替えられたら、古い記録を登録せず件を pending に戻す。新しいサイドカーは
/// 消さない（ファイルが正）
#[tokio::test]
async fn cd_item_is_not_registered_when_the_sidecar_is_replaced_during_placement() {
    use spindle::import::sidecar::Sidecar;
    let lib = Lib::new();
    let item = require_ffmpeg!(add_cd_item(&lib).await);
    lib.approve(
        item.id,
        &draft_for(
            &[
                ("CD/01.flac", 1, "A"),
                ("CD/02.flac", 2, "B"),
                ("CD/03.flac", 3, "C"),
            ],
            None,
            "Disc",
        ),
    );
    let inbox_root = lib.inbox.clone();
    let hook: PlaceHook = Arc::new(move || {
        let mut s = Sidecar::default();
        s.rip = Some(common::rip_entry(
            &["01.flac", "02.flac", "03.flac"],
            &[true, true, true],
        ));
        s.write(
            &inbox_root,
            &spindle::domain::relpath::RelPath::parse("CD").unwrap(),
        )
        .unwrap();
    });
    lib.start_with(lib.env_with(false, Some(hook)));
    assert_eq!(lib.run_job().await, JobState::Done);
    let it = inbox::get(&lib.conn(), item.id).unwrap().unwrap();
    assert_eq!(it.state, ItemState::Pending, "{:?}", it.error);
    assert!(
        it.error
            .as_deref()
            .unwrap_or("")
            .contains("spindle-inbox.json"),
        "{:?}",
        it.error
    );
    assert_eq!(lib.count("SELECT count(*) FROM tracks"), 0);
    assert_eq!(lib.count("SELECT count(*) FROM album_verifications"), 0);
    assert!(!lib.lib_path("_Unsorted").exists());
    // 差し替えた側（全トラック一致）が残っている
    let s = Sidecar::read(
        &lib.inbox,
        &spindle::domain::relpath::RelPath::parse("CD").unwrap(),
    )
    .unwrap()
    .unwrap();
    assert!(s
        .rip
        .unwrap()
        .report
        .ctdb
        .unwrap()
        .tracks
        .iter()
        .all(|t| t.matched));
}

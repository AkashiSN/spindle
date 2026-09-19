//! Inbox の走査と配置（`import::inbox`、`inbox` ジョブ。SPEC §7.8、D-68、P2-10）。
//! 一時ディレクトリに Inbox / Library を作り、ffmpeg で音声を置く（無ければ skip）

#![cfg(target_os = "linux")]

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use std::time::Duration;

use rusqlite::Connection;
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
        }
    }

    fn start(&self, wav_to_flac: bool) {
        let mut reg = Registry::new();
        reg.register(
            JobType::Inbox,
            Arc::new(InboxHandler::new(self.env(wav_to_flac))),
        );
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
            })
            .collect(),
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
    // 後続: rg 1 + transcode 2
    assert_eq!(
        lib.count("SELECT count(*) FROM jobs WHERE type = 'rg' AND state = 'queued'"),
        1
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
    // 別の running ジョブが library を持っている
    {
        let c = lib.conn();
        c.execute(
            "INSERT INTO jobs (type, state, payload, priority, attempts, max_attempts, created_at)
             VALUES ('scan', 'running', '{}', 0, 0, 1, 0)",
            [],
        )
        .unwrap();
        let other = c.last_insert_rowid();
        c.execute(
            "INSERT INTO job_mutexes (name, job_id, acquired_at) VALUES ('library', ?1, 0)",
            [other],
        )
        .unwrap();
    }
    let env = lib.env(true);
    let handler = InboxHandler::new(env);
    // ハンドラを直接は呼べないので、ワーカーで 1 回だけ回して Requeue を観測する
    let mut reg = Registry::new();
    reg.register(JobType::Inbox, Arc::new(handler));
    lib.jobs.start(reg, lib.shutdown.clone());
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

fn sidecar_entry() -> spindle::import::ytmusic::sidecar::FileEntry {
    spindle::import::ytmusic::sidecar::FileEntry {
        source: "youtube".into(),
        url: Some("https://www.youtube.com/watch?v=abc".into()),
        channel: Some("CH".into()),
        verdict: "ok".into(),
        message: None,
    }
}

/// 1 件目を配置した後、同じ category / albumartist / album への 2 件目は既存の album に追記される。
/// サイドカーは Library に持っていかず、配置の成功で消える
#[tokio::test]
async fn second_item_appends_to_the_existing_album_and_removes_the_sidecar() {
    use spindle::import::ytmusic::sidecar::{Sidecar, SIDECAR_NAME};
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

/// ARTIST が多値のファイルは、下書きのアーティストが先頭の値のまま（未編集）なら多値を保つ。
/// 編集していれば 1 値で上書き（プラグインの artists の写像を Library まで運ぶ。SPEC §7.7）
#[tokio::test]
async fn unedited_multi_valued_artist_is_preserved_on_placement() {
    let lib = Lib::new();
    let p1 = require_ffmpeg!(lib.add("AlbumA/01.flac", 1, "One", "A", 1));
    let p2 = lib.add("AlbumA/02.flac", 2, "Two", "A", 2).unwrap();
    for p in [&p1, &p2] {
        set_tags(p, "flac", &[("ARTIST", &["A", "B"])]);
    }
    lib.scan(1000).await;
    let a = lib.item("AlbumA").unwrap();
    let mut d = draft_for(
        &[("AlbumA/01.flac", 1, "One"), ("AlbumA/02.flac", 2, "Two")],
        None,
        "Album",
    );
    d.tracks[0].artist = "A".into(); // 提案どおり（先頭）= 未編集
    d.tracks[1].artist = "C".into(); // 編集
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
    assert_eq!(
        read("_Unsorted/Artist/Album/01 One.flac")
            .tags
            .values("ARTIST")
            .collect::<Vec<_>>(),
        ["A", "B"]
    );
    assert_eq!(
        read("_Unsorted/Artist/Album/02 Two.flac")
            .tags
            .values("ARTIST")
            .collect::<Vec<_>>(),
        ["C"]
    );
    let disp: String = lib
        .conn()
        .query_row(
            "SELECT artist_display FROM tracks WHERE rel_path = '_Unsorted/Artist/Album/01 One.flac'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(disp.contains('B'), "{disp}");
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

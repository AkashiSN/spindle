//! 再生リストの列挙（`import::ytmusic::playlist::parse_playlist_dump`）と同期ジョブ
//! （`jobs::handlers::playlist_sync`。P4-16、D-78）

use spindle::import::ytmusic::playlist::{parse_playlist_dump, Availability, UnavailableKind};

const DUMP: &str = r#"{
  "id": "PLx", "title": "理芽", "_type": "playlist", "playlist_count": 4,
  "webpage_url": "https://www.youtube.com/playlist?list=PLx",
  "entries": [
    {"id": "aaa", "title": "bad guy", "url": "https://www.youtube.com/watch?v=aaa", "ie_key": "Youtube", "duration": 197},
    {"id": "ppp", "title": null, "url": "https://www.youtube.com/watch?v=ppp", "ie_key": "Youtube", "duration": null},
    {"id": "qqq", "title": "[Private video]", "url": "https://www.youtube.com/watch?v=qqq", "ie_key": "Youtube"},
    {"id": "ddd", "title": "[Deleted video]", "url": "https://youtu.be/ddd", "ie_key": "Youtube"}
  ]
}"#;

#[test]
fn parse_playlist_dump_numbers_entries_and_classifies_unavailable_ones() {
    let d = parse_playlist_dump(DUMP.as_bytes()).unwrap();
    assert_eq!(d.title.as_deref(), Some("理芽"));
    assert_eq!(d.playlist_count, Some(4));
    assert!(!d.truncated());
    let got: Vec<(u32, &str, &str, Option<&str>, Availability)> = d
        .entries
        .iter()
        .map(|e| {
            (
                e.position,
                e.id.as_str(),
                e.url.as_str(),
                e.title.as_deref(),
                e.availability.clone(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (
                1,
                "aaa",
                "https://www.youtube.com/watch?v=aaa",
                Some("bad guy"),
                Availability::Available
            ),
            (
                2,
                "ppp",
                "https://www.youtube.com/watch?v=ppp",
                None,
                Availability::Unavailable(UnavailableKind::Unknown)
            ),
            (
                3,
                "qqq",
                "https://www.youtube.com/watch?v=qqq",
                None,
                Availability::Unavailable(UnavailableKind::Private)
            ),
            (
                4,
                "ddd",
                "https://www.youtube.com/watch?v=ddd",
                None,
                Availability::Unavailable(UnavailableKind::Deleted)
            ),
        ]
    );
}

#[test]
fn parse_playlist_dump_detects_truncation_and_rejects_non_playlists() {
    let d = parse_playlist_dump(
        br#"{"_type":"playlist","playlist_count":150,"entries":[{"id":"a","title":"t","url":"https://www.youtube.com/watch?v=a"}]}"#,
    )
    .unwrap();
    assert!(d.truncated());
    // playlist_count が無ければ判定できない（打ち切りとはみなさない）
    let d = parse_playlist_dump(
        br#"{"_type":"playlist","entries":[{"id":"a","title":"t","url":"https://www.youtube.com/watch?v=a"}]}"#,
    )
    .unwrap();
    assert!(!d.truncated());
    // 動画 1 本の dump は再生リストでない
    let e = parse_playlist_dump(
        br#"{"id":"a","title":"t","webpage_url":"https://www.youtube.com/watch?v=a"}"#,
    )
    .unwrap_err();
    assert!(e.contains("再生リストでない"), "{e}");
    // id の無い entry は位置を占めるが取れない（unknown）
    let d = parse_playlist_dump(
        br#"{"_type":"playlist","entries":[{"title":"t"},{"id":"b","title":"u","url":"https://www.youtube.com/watch?v=b"}]}"#,
    )
    .unwrap();
    assert_eq!(d.entries.len(), 2);
    assert_eq!(
        d.entries[0].availability,
        Availability::Unavailable(UnavailableKind::Unknown)
    );
    assert_eq!(d.entries[1].position, 2);
}

// ---------------------------------------------------------------- 同期ジョブ

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use spindle::config::LayoutConfig;
use spindle::db::subscriptions::{self, NewSubscription, WriteOutcome};
use spindle::db::Db;
use spindle::edit::Editor;
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, Scanner};
use spindle::jobs::handlers::playlist_sync::{new_sync_job, PlaylistSyncHandler, SyncEnv};
use spindle::jobs::handlers::rename::RenameHandler;
use spindle::jobs::handlers::tagwrite::TagwriteHandler;
use spindle::jobs::{EnqueueResult, JobState, JobType, Jobs, Registry};

/// 偽の yt-dlp: `dump/<list_id>.json` を返す。`slow` があれば 1 秒待つ。呼び出しは `calls.log` に残す
const FAKE_YTDLP: &str = r#"
FAKE="$1"; shift
printf '%s\n' "$*" >> "$FAKE/calls.log"
url="${@: -1}"
list="${url##*list=}"
[ -f "$FAKE/slow" ] && sleep 1
if [ -f "$FAKE/dump/$list.json" ]; then cat "$FAKE/dump/$list.json"; exit 0; fi
echo "ERROR: [youtube:tab] $list: The playlist does not exist." >&2; exit 1
"#;

fn layout() -> LayoutConfig {
    LayoutConfig {
        multi_disc: "{category}/{albumartist}/{album}/{disc}-{track:02} {title}".to_owned(),
        single_disc: "{category}/{albumartist}/{album}/{track:02} {title}".to_owned(),
        unsorted: "_Unsorted/{albumartist}/{album}/{track:02} {title}".to_owned(),
    }
}

fn watch(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

/// `SOURCE_URL` をファイルに書く（spindle のタグ書き込みを tmp 無しで当てる）
fn set_source_url(path: &std::path::Path, id: &str) {
    let changes = vec![spindle::domain::tags::TagChange {
        key: "SOURCE_URL".to_owned(),
        values: Some(vec![watch(id)]),
    }];
    let mut f = std::fs::File::options()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    spindle::domain::tags::write_tag_changes(&mut f, Some("opus"), &changes, None).unwrap();
}

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    db: Arc<Db>,
    jobs: Arc<Jobs>,
    editor: Arc<Editor>,
    scanner: Scanner,
    fake: PathBuf,
    shutdown: CancellationToken,
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["Library", "fake", "fake/dump"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let fake = dir.path().join("fake");
        std::fs::write(fake.join("ytdlp.sh"), FAKE_YTDLP).unwrap();
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let root = Arc::new(RootDir::open(&dir.path().join("Library")).unwrap());
        let jobs = Jobs::new(db.clone());
        let scanner = Scanner::new(db.clone(), root.clone(), 2);
        let editor = Arc::new(Editor::new(db.clone(), root, jobs.clone()));
        Self {
            dir,
            db_path,
            db,
            jobs,
            editor,
            scanner,
            fake,
            shutdown: CancellationToken::new(),
        }
    }

    fn env(&self) -> SyncEnv {
        SyncEnv {
            db: self.db.clone(),
            jobs: self.jobs.clone(),
            editor: self.editor.clone(),
            layout: layout(),
            ytdlp: vec![
                "/bin/bash".to_owned(),
                self.fake.join("ytdlp.sh").display().to_string(),
                self.fake.display().to_string(),
            ],
            pending_wait: Duration::from_secs(30),
            pending_poll: Duration::from_millis(20),
        }
    }

    /// 同期・tagwrite・rename を動かす（ytdl は登録しない: 投入された ytdl は queued のまま検分する）
    fn start(&self) {
        self.start_with(true, self.env());
    }

    fn start_with(&self, tagwrite: bool, env: SyncEnv) {
        let mut reg = Registry::new();
        reg.register(
            JobType::PlaylistSync,
            Arc::new(PlaylistSyncHandler::new(env)),
        );
        if tagwrite {
            reg.register(
                JobType::Tagwrite,
                Arc::new(TagwriteHandler::new(self.editor.clone())),
            );
        }
        reg.register(
            JobType::Rename,
            Arc::new(RenameHandler::new(self.editor.clone())),
        );
        self.jobs.start(reg, self.shutdown.clone());
    }

    fn lib(&self) -> PathBuf {
        self.dir.path().join("Library")
    }

    /// `_Unsorted/Art/Alb/<no:02> <title>.opus` に SOURCE_URL 付きで置く。`video_id` が None なら URL 無し
    fn add(&self, no: u32, title: &str, video_id: Option<&str>) -> Option<PathBuf> {
        let dir = self.lib().join("_Unsorted/Art/Alb");
        std::fs::create_dir_all(&dir).unwrap();
        let name = format!("{no:02} {title}.opus");
        let p = common::make_audio(&dir, &name, "opus", no)?;
        common::set_basic_tags(&p, title, "Art", "Alb", "Art", no, 1);
        if let Some(id) = video_id {
            set_source_url(&p, id);
        }
        Some(p)
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

    fn playlist(&self, list_id: &str, ids: &[&str], count: Option<usize>) {
        let entries: Vec<String> = ids
            .iter()
            .map(|id| match *id {
                // `~` 始まりは取れない entry（title: null）
                s if s.starts_with('~') => format!(
                    r#"{{"id":"{}","title":null,"url":"{}","duration":null}}"#,
                    &s[1..],
                    watch(&s[1..])
                ),
                s => format!(
                    r#"{{"id":"{s}","title":"song {s}","url":"{}","ie_key":"Youtube","duration":100}}"#,
                    watch(s)
                ),
            })
            .collect();
        let count = count
            .map(|c| format!(r#""playlist_count":{c},"#))
            .unwrap_or_default();
        let json = format!(
            r#"{{"id":"{list_id}","title":"list {list_id}","_type":"playlist",{count}"entries":[{}]}}"#,
            entries.join(",")
        );
        std::fs::write(self.fake.join("dump").join(format!("{list_id}.json")), json).unwrap();
    }

    async fn subscribe(&self, list_id: &str) -> i64 {
        let s = NewSubscription {
            list_id: list_id.to_owned(),
            url: format!("https://www.youtube.com/playlist?list={list_id}"),
            albumartist: "Art".to_owned(),
            album: "Alb".to_owned(),
            category: None,
            align: true,
            enabled: true,
            max_enqueue: 50,
        };
        match self
            .db
            .write(move |c| subscriptions::insert(c, &s, 1))
            .await
            .unwrap()
        {
            WriteOutcome::Ok(id) => id,
            other => panic!("{other:?}"),
        }
    }

    async fn sync(&self, sub_id: i64) -> (i64, JobState) {
        let id = match self.jobs.enqueue(new_sync_job(sub_id)).await.unwrap() {
            EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
        };
        (id, self.wait(id).await)
    }

    async fn wait(&self, id: i64) -> JobState {
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while std::time::Instant::now() < deadline {
            let (s, attempts): (String, i64) = self
                .conn()
                .query_row(
                    "SELECT state, attempts FROM jobs WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            let st: JobState = s.parse().unwrap();
            if st.is_terminal() || (st == JobState::Queued && attempts > 0) {
                return st;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("同期ジョブが終わらない");
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.db_path).unwrap()
    }

    fn sub(&self, id: i64) -> subscriptions::Subscription {
        subscriptions::get(&self.conn(), id).unwrap().unwrap()
    }

    fn result(&self, id: i64) -> serde_json::Value {
        self.sub(id).last_result.unwrap()
    }

    fn note(&self, job_id: i64) -> Option<String> {
        self.conn()
            .query_row("SELECT note FROM jobs WHERE id = ?1", [job_id], |r| {
                r.get(0)
            })
            .unwrap()
    }

    fn last_error(&self, job_id: i64) -> Option<String> {
        self.conn()
            .query_row("SELECT last_error FROM jobs WHERE id = ?1", [job_id], |r| {
                r.get(0)
            })
            .unwrap()
    }

    /// queued の ytdl ジョブの payload（id 順）
    fn ytdl_payloads(&self) -> Vec<serde_json::Value> {
        let c = self.conn();
        let mut st = c
            .prepare("SELECT payload FROM jobs WHERE type = 'ytdl' ORDER BY id")
            .unwrap();
        st.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| serde_json::from_str(&r.unwrap()).unwrap())
            .collect()
    }

    /// album の active 行を (track_no, rel_path) で番号順に
    fn album_layout(&self) -> Vec<(i64, String)> {
        let c = self.conn();
        let mut st = c
            .prepare(
                "SELECT track_no, rel_path FROM tracks WHERE missing_since IS NULL ORDER BY track_no, rel_path",
            )
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn batches(&self) -> i64 {
        self.conn()
            .query_row("SELECT count(*) FROM edit_batches", [], |r| r.get(0))
            .unwrap()
    }

    fn calls(&self) -> usize {
        std::fs::read_to_string(self.fake.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .count()
    }
}

impl Drop for Lib {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// 無いものだけ投入し、既存の行を再生リストの位置に揃える（番号とファイル名）。再同期は変更なし
#[tokio::test]
async fn sync_enqueues_missing_entries_and_aligns_existing_rows_then_is_idempotent() {
    let lib = Lib::new();
    // Library: a=1, b=2, d=3。再生リスト: a, b, c, d（c が未取り込み → d は 4 番になる）
    require_ffmpeg!(lib.add(1, "aa", Some("a")));
    lib.add(2, "bb", Some("b"));
    lib.add(3, "dd", Some("d"));
    lib.scan().await;
    lib.playlist("PL1", &["a", "b", "c", "d"], Some(4));
    let sub = lib.subscribe("PL1").await;
    lib.start();

    let (job, st) = lib.sync(sub).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.last_error(job));
    let r = lib.result(sub);
    assert_eq!(r["state"], "done");
    assert_eq!(r["entries"], 4);
    assert_eq!(r["in_library"], 3);
    assert_eq!(r["enqueued"], serde_json::json!([3]));
    assert_eq!(r["align"]["moved"], 1);
    assert_eq!(r["align"]["renamed"], 1);
    assert_eq!(r["align"]["unchanged"], 2);
    assert_eq!(r["align"]["tags"]["applied"], 1);
    assert_eq!(r["align"]["rename"]["applied"], 1);
    assert_eq!(
        lib.album_layout(),
        [
            (1, "_Unsorted/Art/Alb/01 aa.opus".to_owned()),
            (2, "_Unsorted/Art/Alb/02 bb.opus".to_owned()),
            (4, "_Unsorted/Art/Alb/04 dd.opus".to_owned()),
        ]
    );
    assert!(lib.lib().join("_Unsorted/Art/Alb/04 dd.opus").is_file());
    // ytdl は購読の情報付き
    assert_eq!(
        lib.ytdl_payloads(),
        [serde_json::json!({ "url": watch("c"), "subscription_id": sub, "position": 3 })]
    );
    // 追記先を束ねた、成功の終端
    let s = lib.sub(sub);
    assert!(s.album_id.is_some());
    assert!(s.last_synced_at.is_some());
    assert!(
        lib.note(job).unwrap().contains("1 件を投入"),
        "{:?}",
        lib.note(job)
    );
    let batches = lib.batches();

    // 再同期: 揃えは変更なし、c は投入済み（走行中）
    let (job2, st) = lib.sync(sub).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.last_error(job2));
    let r = lib.result(sub);
    assert_eq!(r["align"]["moved"], 0);
    assert_eq!(r["align"]["renamed"], 0);
    assert_eq!(r["align"]["unchanged"], 3);
    assert_eq!(r["enqueued"], serde_json::json!([]));
    assert_eq!(r["running"], serde_json::json!([3]));
    assert_eq!(lib.batches(), batches, "新しいバッチは作らない");
    assert_eq!(lib.ytdl_payloads().len(), 1);
}

/// 取れない entry は投入せず一覧に出る。Inbox にあるものと別 album にあるものは投入しない。
/// 上限を超えた分は次回に持ち越す
#[tokio::test]
async fn sync_skips_unavailable_inbox_and_elsewhere_entries_and_defers_over_the_cap() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add(1, "aa", Some("a")));
    // 別 album の行
    let other = lib.lib().join("_Unsorted/Other/Alb2");
    std::fs::create_dir_all(&other).unwrap();
    let p = common::make_audio(&other, "01 ee.opus", "opus", 9).unwrap();
    common::set_basic_tags(&p, "ee", "Other", "Alb2", "Other", 1, 1);
    set_source_url(&p, "e");
    lib.scan().await;
    // Inbox に f がある（取り込み中）
    lib.conn()
        .execute_batch(&format!(
            "INSERT INTO inbox_items (id, rel_dir, rel_dir_key, state, detected_at, seen_at) VALUES (1, 'x', 'x', 'pending', 0, 0);
             INSERT INTO inbox_files (item_id, rel_path, rel_path_key, inode, size, mtime_ns, ctime_ns, codec, lossless, tags)
               VALUES (1, 'x/f.opus', 'x/f.opus', 1, 1, 0, 0, 'opus', 0, '[[\"SOURCE_URL\",\"{}\"]]');",
            watch("f")
        ))
        .unwrap();
    // a, ~p（取れない）, c, e（別 album）, f（Inbox）, g, h
    lib.playlist("PL1", &["a", "~p", "c", "e", "f", "g", "h"], Some(7));
    let sub = lib.subscribe("PL1").await;
    lib.db
        .write(move |c| {
            subscriptions::update(
                c,
                sub,
                &subscriptions::Patch {
                    max_enqueue: Some(2),
                    ..Default::default()
                },
                2,
            )
        })
        .await
        .unwrap();
    lib.start();
    let (job, st) = lib.sync(sub).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.last_error(job));
    let r = lib.result(sub);
    assert_eq!(r["in_library"], 1);
    assert_eq!(r["in_inbox"], 1);
    assert_eq!(r["elsewhere"][0]["position"], 4);
    assert_eq!(r["elsewhere"][0]["id"], "e");
    assert_eq!(
        r["unavailable"],
        serde_json::json!([{ "position": 2, "id": "p", "kind": "unknown" }])
    );
    assert_eq!(r["enqueued"], serde_json::json!([3, 6]));
    assert_eq!(r["deferred"], 1);
    assert_eq!(lib.ytdl_payloads().len(), 2);
    let note = lib.note(job).unwrap();
    assert!(
        note.contains("2 件を投入")
            && note.contains("1 件は次回")
            && note.contains("1 件は取れない"),
        "{note}"
    );
}

/// 取りこぼし（entries < playlist_count）は失敗して何も投入しない
#[tokio::test]
async fn truncated_listing_fails_without_enqueueing() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add(1, "aa", Some("a")));
    lib.scan().await;
    lib.playlist("PL1", &["a", "b"], Some(150));
    let sub = lib.subscribe("PL1").await;
    lib.start();
    let (job, st) = lib.sync(sub).await;
    assert_eq!(st, JobState::Queued, "再試行待ち");
    assert!(lib.last_error(job).unwrap().contains("取りこぼした"));
    assert!(lib.ytdl_payloads().is_empty());
    let s = lib.sub(sub);
    assert_eq!(s.last_synced_at, None);
    assert!(s.last_attempted_at.is_some());
    assert_eq!(s.last_result.unwrap()["state"], "failed");
}

/// 固定行（SOURCE_URL 無し）が塞ぐ番号には動かさず「揃えられない」に出す。塞がれていない行は揃う
#[tokio::test]
async fn fixed_rows_block_their_numbers() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add(1, "aa", Some("a")));
    lib.add(2, "xx", None);
    lib.add(3, "bb", Some("b"));
    lib.add(5, "cc", Some("c"));
    lib.scan().await;
    // b は 2 番になるべきだが xx が塞ぐ。c は 3 番に動く
    lib.playlist("PL1", &["a", "b", "c"], Some(3));
    let sub = lib.subscribe("PL1").await;
    lib.start();
    let (job, st) = lib.sync(sub).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.last_error(job));
    let r = lib.result(sub);
    assert_eq!(r["align"]["moved"], 1);
    assert_eq!(r["align"]["blocked"][0]["position"], 2);
    assert_eq!(r["align"]["blocked"][0]["reason"]["kind"], "number_taken");
    assert_eq!(r["align"]["unnumbered"], 1);
    assert_eq!(
        lib.album_layout(),
        [
            (1, "_Unsorted/Art/Alb/01 aa.opus".to_owned()),
            (2, "_Unsorted/Art/Alb/02 xx.opus".to_owned()),
            (3, "_Unsorted/Art/Alb/03 bb.opus".to_owned()),
            (3, "_Unsorted/Art/Alb/03 cc.opus".to_owned()),
        ]
    );
}

/// tags 適用後・rename 前に落ちた境界: 番号だけ合っている行は再実行で改名される
#[tokio::test]
async fn rerun_renames_rows_whose_numbers_were_already_fixed() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add(1, "aa", Some("a")));
    lib.add(2, "bb", Some("b"));
    lib.scan().await;
    lib.start();
    // 「前の実行」が TRACKNUMBER だけ直して落ちた状態を作る（b を 3 番に）
    let id_b: i64 = lib
        .conn()
        .query_row(
            "SELECT id FROM tracks WHERE rel_path = '_Unsorted/Art/Alb/02 bb.opus'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    lib.editor
        .prepare_tags(
            None,
            vec![spindle::edit::NewTagOp {
                track_id: id_b,
                changes: vec![spindle::domain::tags::TagChange {
                    key: "TRACKNUMBER".into(),
                    values: Some(vec!["3".into()]),
                }],
            }],
        )
        .await
        .unwrap();
    // 反映を待つ
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !spindle::db::history::pending_track_ids(&lib.conn(), &[id_b])
        .unwrap()
        .is_empty()
    {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        lib.album_layout()[1],
        (3, "_Unsorted/Art/Alb/02 bb.opus".to_owned())
    );
    lib.playlist("PL1", &["a", "~p", "b"], Some(3));
    let sub = lib.subscribe("PL1").await;
    let (job, st) = lib.sync(sub).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.last_error(job));
    let r = lib.result(sub);
    assert_eq!(r["align"]["moved"], 0, "番号は既に合っている");
    assert_eq!(r["align"]["renamed"], 1);
    assert_eq!(
        lib.album_layout()[1],
        (3, "_Unsorted/Art/Alb/03 bb.opus".to_owned())
    );
}

/// 走行中に要求（承認の後続・手動）が来たら Requeue でもう一度走る
#[tokio::test]
async fn request_during_run_makes_the_job_run_again() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add(1, "aa", Some("a")));
    lib.scan().await;
    lib.playlist("PL1", &["a"], Some(1));
    std::fs::write(lib.fake.join("slow"), "").unwrap();
    let sub = lib.subscribe("PL1").await;
    lib.start();
    let job = match lib.jobs.enqueue(new_sync_job(sub)).await.unwrap() {
        EnqueueResult::Inserted(j) => j,
        EnqueueResult::Duplicate(j) => j,
    };
    // 走り出す（latch が消える）のを待ってから要求を立てる
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while lib.sub(sub).last_attempted_at.is_none() {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        lib.jobs.enqueue(new_sync_job(sub)).await.unwrap(),
        EnqueueResult::Duplicate(job)
    );
    lib.db
        .write(move |c| subscriptions::request_sync(c, sub, 5))
        .await
        .unwrap();
    // 1 周目の終端で Requeue → 2 周目 → done。yt-dlp は 2 回呼ばれる
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let st = lib.wait(job).await;
        if st == JobState::Done && lib.calls() >= 2 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "state={st:?} calls={}",
            lib.calls()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(lib.calls(), 2);
    assert_eq!(
        lib.sub(sub).sync_requested_at,
        None,
        "2 周目の開始で latch が消えた"
    );
}

/// 改名はファイル名だけ（ディレクトリは今のまま）で、名前の番号が合っていない行だけ。album のディレクトリが
/// テンプレートと違っても（category が未推定の実機）album を動かさず、番号の合った行の名前の書式も触らない
#[tokio::test]
async fn align_renames_only_the_file_name_of_rows_whose_name_number_is_stale() {
    let lib = Lib::new();
    let dir = lib.lib().join("Music/Art/Alb");
    std::fs::create_dir_all(&dir).unwrap();
    // 旧書式の名前。a=1（合っている）、b は番号 2 だが名前は「03.」（前の実行が番号だけ直した状態）、
    // c は 5 番で再生リストでは 3 番
    for (name, no, title, id) in [
        ("01. aa.opus", 1u32, "aa", "a"),
        ("03. bb.opus", 2, "bb", "b"),
        ("05. cc.opus", 5, "cc", "c"),
    ] {
        let p = require_ffmpeg!(common::make_audio(&dir, name, "opus", no));
        common::set_basic_tags(&p, title, "Art", "Alb", "Art", no, 1);
        set_source_url(&p, id);
    }
    lib.scan().await;
    lib.playlist("PL1", &["a", "b", "c"], Some(3));
    let sub = lib.subscribe("PL1").await;
    let album_id: i64 = lib
        .conn()
        .query_row(
            "SELECT id FROM albums WHERE rel_dir = 'Music/Art/Alb'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    lib.db
        .write(move |c| subscriptions::bind_album(c, sub, album_id))
        .await
        .unwrap();
    lib.start();
    let (job, st) = lib.sync(sub).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.last_error(job));
    let r = lib.result(sub);
    assert_eq!(r["align"]["moved"], 1, "c だけ番号が変わる");
    assert_eq!(r["align"]["renamed"], 2, "b（名前の番号が古い）と c");
    assert_eq!(
        lib.album_layout(),
        [
            (1, "Music/Art/Alb/01. aa.opus".to_owned()),
            (2, "Music/Art/Alb/02 bb.opus".to_owned()),
            (3, "Music/Art/Alb/03 cc.opus".to_owned()),
        ]
    );
    assert!(!lib.lib().join("_Unsorted").exists(), "album を動かさない");
}

/// 同期の間に PATCH で購読が変わったら（updated_at が進む）失敗して何も投入しない（古い追記先で投入しない）
#[tokio::test]
async fn patch_during_sync_fails_the_run_without_enqueueing() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add(1, "aa", Some("a")));
    lib.scan().await;
    lib.playlist("PL1", &["a", "b"], Some(2));
    std::fs::write(lib.fake.join("slow"), "").unwrap();
    let sub = lib.subscribe("PL1").await;
    lib.start();
    let job = match lib.jobs.enqueue(new_sync_job(sub)).await.unwrap() {
        EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while lib.sub(sub).last_attempted_at.is_none() {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // 列挙（1 秒）の間に追記先を変える
    lib.db
        .write(move |c| {
            subscriptions::update(
                c,
                sub,
                &subscriptions::Patch {
                    album: Some("Other".to_owned()),
                    ..Default::default()
                },
                99,
            )
        })
        .await
        .unwrap();
    let st = lib.wait(job).await;
    assert_eq!(
        st,
        JobState::Queued,
        "再試行待ち: {:?}",
        lib.last_error(job)
    );
    assert!(lib.last_error(job).unwrap().contains("変更された"));
    assert!(lib.ytdl_payloads().is_empty(), "投入しない");
    assert_eq!(lib.sub(sub).album_id, None, "古い解決で束ねない");
}

/// 子バッチが全件 applied でなければ（ここでは外部の書き換えで tags が conflict）同期は失敗して投入しない
#[tokio::test]
async fn failed_child_batch_fails_the_sync_and_nothing_is_enqueued() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add(1, "aa", Some("a")));
    let c = lib.add(2, "cc", Some("c")).unwrap();
    lib.scan().await;
    // c は 3 番へ動く（b が未取り込み）。走査の後にファイルを外部が書き換えている（事前条件が外れて
    // tagwrite が skipped_conflict で閉じる）
    common::retag(&c, |t| {
        use lofty::tag::Accessor;
        t.set_comment("外部の書き換え".to_owned())
    });
    lib.playlist("PL1", &["a", "b", "c"], Some(3));
    let sub = lib.subscribe("PL1").await;
    lib.start();
    let (job, st) = lib.sync(sub).await;
    assert_eq!(
        st,
        JobState::Queued,
        "再試行待ち: {:?}",
        lib.last_error(job)
    );
    let err = lib.last_error(job).unwrap();
    assert!(err.contains("全件反映されていない"), "{err}");
    assert!(lib.ytdl_payloads().is_empty(), "揃え終わる前は投入しない");
    let r = lib.result(sub);
    assert_eq!(r["state"], "failed");
    assert_eq!(r["align"]["tags"]["conflict"], 1);
}

/// 同じ動画が再生リストに複数回あっても投入は 1 回、揃えは動かさず「揃えられない」
#[tokio::test]
async fn duplicate_entries_are_enqueued_once_and_not_aligned() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add(1, "aa", Some("a")));
    lib.scan().await;
    lib.playlist("PL1", &["a", "c", "c", "a"], Some(4));
    let sub = lib.subscribe("PL1").await;
    lib.start();
    let (job, st) = lib.sync(sub).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.last_error(job));
    let r = lib.result(sub);
    assert_eq!(r["enqueued"], serde_json::json!([2]));
    assert_eq!(r["running"], serde_json::json!([]));
    assert_eq!(r["align"]["moved"], 0);
    assert_eq!(
        r["align"]["blocked"][0]["reason"]["kind"],
        "duplicate_entry"
    );
    assert_eq!(lib.ytdl_payloads().len(), 1);
    // 再実行しても同じ（往復しない）
    let (job, st) = lib.sync(sub).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.last_error(job));
    assert_eq!(lib.result(sub)["align"]["moved"], 0);
}

/// 子バッチの終端は batch_id で待つ: 対象の行が走査で missing になっても pending の op が残っていれば
/// 待ち続け、上限で失敗して投入しない（album の active 行だけ見ていると pending を見落とす）
#[tokio::test]
async fn pending_op_on_a_missing_track_is_not_mistaken_for_completion() {
    let lib = Lib::new();
    require_ffmpeg!(lib.add(1, "aa", Some("a")));
    lib.add(2, "cc", Some("c"));
    lib.scan().await;
    lib.playlist("PL1", &["a", "b", "c"], Some(3));
    let sub = lib.subscribe("PL1").await;
    // tagwrite を動かさない（op が pending のまま）。待ちの上限は短く
    let mut env = lib.env();
    env.pending_wait = Duration::from_millis(600);
    lib.start_with(false, env);
    let job = match lib.jobs.enqueue(new_sync_job(sub)).await.unwrap() {
        EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
    };
    // tags バッチが出来たら対象の行を missing にする
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while lib.batches() == 0 {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    lib.conn()
        .execute(
            "UPDATE tracks SET missing_since = 5 WHERE rel_path = '_Unsorted/Art/Alb/02 cc.opus'",
            [],
        )
        .unwrap();
    let st = lib.wait(job).await;
    assert_eq!(
        st,
        JobState::Queued,
        "再試行待ち: {:?}",
        lib.last_error(job)
    );
    assert!(
        lib.last_error(job).unwrap().contains("反映が終わらない"),
        "{:?}",
        lib.last_error(job)
    );
    assert!(lib.ytdl_payloads().is_empty(), "投入しない");
}

// ---------------------------------------------------------------- dispatcher

/// latch の立った購読は（interval に関わらず）投入され、interval > 0 なら last_attempted_at から
/// interval 経った enabled の購読も投入される。走行中の Duplicate は次の tick で拾う
#[tokio::test]
async fn dispatcher_enqueues_requested_and_due_subscriptions() {
    use spindle::jobs::handlers::playlist_sync::spawn_dispatcher;
    let lib = Lib::new();
    let a = lib.subscribe("PLa").await;
    let b = {
        let s = NewSubscription {
            list_id: "PLb".to_owned(),
            url: "https://www.youtube.com/playlist?list=PLb".to_owned(),
            albumartist: "B".to_owned(),
            album: "B".to_owned(),
            category: None,
            align: true,
            enabled: true,
            max_enqueue: 50,
        };
        match lib
            .db
            .write(move |c| subscriptions::insert(c, &s, 1))
            .await
            .unwrap()
        {
            WriteOutcome::Ok(id) => id,
            other => panic!("{other:?}"),
        }
    };
    // a は昨日試した、b は 1 秒前に試した
    let now = spindle::db::now_epoch();
    lib.db
        .write(move |c| {
            subscriptions::begin_attempt(c, a, now - 86_400)?;
            subscriptions::begin_attempt(c, b, now - 1)?;
            Ok(())
        })
        .await
        .unwrap();
    let shutdown = CancellationToken::new();
    // interval = 0: latch だけ
    let h = spawn_dispatcher(
        lib.jobs.clone(),
        0,
        Duration::from_millis(20),
        shutdown.clone(),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        lib.conn()
            .query_row("SELECT count(*) FROM jobs", [], |r| r.get::<_, i64>(0))
            .unwrap()
            == 0
    );
    lib.db
        .write(move |c| subscriptions::request_sync(c, b, now))
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let keys: Vec<String> = {
            let c = lib.conn();
            let mut st = c.prepare("SELECT dedup_key FROM jobs ORDER BY id").unwrap();
            st.query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        if keys == [format!("playlist_sync:{b}")] {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{keys:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), h)
        .await
        .unwrap()
        .unwrap();

    // interval = 1 時間: a は due、b は due でない（latch はワーカーが無いので queued のまま = Duplicate）
    let shutdown = CancellationToken::new();
    let h = spawn_dispatcher(
        lib.jobs.clone(),
        1,
        Duration::from_millis(20),
        shutdown.clone(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let keys: Vec<String> = {
            let c = lib.conn();
            let mut st = c.prepare("SELECT dedup_key FROM jobs ORDER BY id").unwrap();
            st.query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        if keys == [format!("playlist_sync:{b}"), format!("playlist_sync:{a}")] {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{keys:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        lib.conn()
            .query_row("SELECT count(*) FROM jobs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2,
        "二重には投入しない"
    );
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), h)
        .await
        .unwrap()
        .unwrap();
}

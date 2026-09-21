//! YouTube のダウンロード（`import::ytmusic::downloader`、`ytdl` ジョブ。SPEC §7.7、D-70、P3-3）。
//! 偽の yt-dlp（bash。dump と download を演じる）と偽のプラグインで、Inbox に置くところまでを
//! 確かめる。実タイトルや固有名詞は使わない。ffmpeg が無ければ skip

#![cfg(target_os = "linux")]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use spindle::db::Db;
use spindle::fsroot::RootDir;
use spindle::import::ytmusic::downloader::{DownloaderEnv, DEDUP_PREFIX};
use spindle::import::ytmusic::sidecar::Sidecar;
use spindle::import::ytmusic::MetadataProvider;
use spindle::jobs::handlers::ytdl::{new_ytdl_job, YtdlHandler};
use spindle::jobs::{EnqueueResult, JobState, JobType, Jobs, Registry};

/// 偽の yt-dlp。`--dump-single-json` なら `dump/<url をキー化した名前>.json` を返し、download なら
/// `-o` のテンプレートに `audio.webm` と `thumb.jpg` を置く。呼び出しは `calls.log` に残す
const FAKE_YTDLP: &str = r#"
FAKE="$1"; shift
printf '%s\n' "$*" >> "$FAKE/calls.log"
url="${@: -1}"
key=$(printf '%s' "$url" | tr -c 'A-Za-z0-9' '_')
if [[ " $* " == *" --dump-single-json "* ]]; then
  if [ -f "$FAKE/dump/$key.json" ]; then cat "$FAKE/dump/$key.json"; exit 0; fi
  if [ -f "$FAKE/unsupported" ]; then echo "ERROR: Unsupported URL: $url" >&2; exit 1; fi
  echo "ERROR: [youtube] $url: Video unavailable" >&2; exit 1
fi
if [ -f "$FAKE/fail_download" ]; then echo "ERROR: unable to download: network down" >&2; exit 1; fi
tmpl=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-o" ]; then tmpl="$a"; fi
  prev="$a"
done
id=$(grep -o '"id": *"[^"]*"' "$FAKE/dump/$key.json" | head -1 | sed 's/.*"\([^"]*\)"$/\1/')
out="${tmpl//%(id)s/$id}"; out="${out//%(ext)s/webm}"
cp "$FAKE/audio.webm" "$out"
cp "$FAKE/thumb.jpg" "${out%.webm}.jpg"
"#;

/// 偽のプラグイン。channel で分岐: KnownCh → ok、SkipCh → skip、BrokenCh → 不正な JSON、他 → unmatched
const FAKE_PLUGIN: &str = r#"
IN=$(cat)
case "$IN" in
  *'"channel":"KnownCh"'*) echo '{"protocol":1,"ok":true,"track":{"title":"Song One","artists":["Artist A"],"albumartist":"Artist A","album":"Songs of A","category":"Pop","date":"2026","tags":[["ORIGINALARTIST","Someone"]]}}' ;;
  *'"channel":"SkipCh"'*) echo '{"protocol":1,"ok":false,"reason":"skip","message":"配信の告知"}' ;;
  *'"channel":"BrokenCh"'*) echo 'not json' ;;
  *) echo '{"protocol":1,"ok":false,"reason":"unmatched","message":"ルールを足してください"}' ;;
esac
"#;

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    db: Arc<Db>,
    jobs: Arc<Jobs>,
    inbox: Arc<RootDir>,
    fake: PathBuf,
    shutdown: CancellationToken,
}

impl Lib {
    fn new() -> Option<Self> {
        let dir = tempfile::tempdir().unwrap();
        for d in ["Inbox", "Archive", "tmp", "fake", "fake/dump"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let fake = dir.path().join("fake");
        common::make_audio(&fake, "audio.webm", "webm", 1)?;
        // サムネイル（ffmpeg で 16x16 の JPEG を作る）
        let st = std::process::Command::new(common::ffmpeg()?)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg("color=c=red:s=16x16")
            .args(["-frames:v", "1"])
            .arg(fake.join("thumb.jpg"))
            .status()
            .unwrap();
        assert!(st.success());
        std::fs::write(fake.join("ytdlp.sh"), FAKE_YTDLP).unwrap();
        std::fs::write(fake.join("plugin.sh"), FAKE_PLUGIN).unwrap();
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let jobs = Jobs::new(db.clone());
        let inbox = Arc::new(RootDir::open(&dir.path().join("Inbox")).unwrap());
        Some(Self {
            dir,
            db_path,
            db,
            jobs,
            inbox,
            fake,
            shutdown: CancellationToken::new(),
        })
    }

    fn env(&self) -> DownloaderEnv {
        let plugin = vec![
            "/bin/bash".to_owned(),
            self.fake.join("plugin.sh").display().to_string(),
        ];
        DownloaderEnv {
            db: self.db.clone(),
            inbox: self.inbox.clone(),
            archive: Arc::new(RootDir::open(&self.dir.path().join("Archive")).unwrap()),
            jobs: self.jobs.clone(),
            provider: MetadataProvider::new(&plugin, Duration::from_secs(10)).unwrap(),
            // 引数配列の先頭にスクリプトと fake ディレクトリを固定する（`ytdlp` は引数配列）
            ytdlp: vec![
                "/bin/bash".to_owned(),
                self.fake.join("ytdlp.sh").display().to_string(),
                self.fake.display().to_string(),
            ],
            ffmpeg: common::ffmpeg().unwrap_or_else(|| PathBuf::from("ffmpeg")),
            tmp_root: self.dir.path().join("tmp").join("ytdl"),
            download_timeout: Duration::from_secs(60),
        }
    }

    fn start(&self) {
        let mut reg = Registry::new();
        reg.register(JobType::Ytdl, Arc::new(YtdlHandler::new(self.env())));
        self.jobs.start(reg, self.shutdown.clone());
    }

    /// dump の JSON を置く（動画）
    fn video(&self, url: &str, id: &str, uploader: &str, title: &str, webm: bool) {
        let formats = if webm {
            r#"[{"format_id":"251","ext":"webm","acodec":"opus","vcodec":"none"},{"format_id":"140","ext":"m4a","acodec":"mp4a.40.2","vcodec":"none"}]"#
        } else {
            r#"[{"format_id":"140","ext":"m4a","acodec":"mp4a.40.2","vcodec":"none"}]"#
        };
        let json = format!(
            r#"{{"id":"{id}","title":"{title}","uploader":"{uploader}","channel":"{uploader} Channel",
                 "webpage_url":"https://www.youtube.com/watch?v={id}","upload_date":"20260901",
                 "duration":123.4,"_type":"video","formats":{formats}}}"#
        );
        self.dump(url, &json);
    }

    fn dump(&self, url: &str, json: &str) {
        let key: String = url
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        std::fs::write(self.fake.join("dump").join(format!("{key}.json")), json).unwrap();
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.fake.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    async fn run(&self, url: &str) -> (i64, JobState) {
        let id = match self.jobs.enqueue(new_ytdl_job(url)).await.unwrap() {
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
            // 再試行待ち（queued + attempts > 0）も終端として返す
            if st.is_terminal() || (st == JobState::Queued && attempts > 0) {
                return st;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("ytdl ジョブが終わらない");
    }

    fn note(&self, id: i64) -> Option<String> {
        self.conn()
            .query_row("SELECT note FROM jobs WHERE id = ?1", [id], |r| r.get(0))
            .unwrap()
    }

    fn job(&self, id: i64) -> (i64, Option<String>) {
        self.conn()
            .query_row(
                "SELECT attempts, last_error FROM jobs WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.db_path).unwrap()
    }

    fn inbox_path(&self, rel: &str) -> PathBuf {
        self.dir.path().join("Inbox").join(rel)
    }

    fn count(&self, sql: &str) -> i64 {
        self.conn().query_row(sql, [], |r| r.get(0)).unwrap()
    }
}

impl Drop for Lib {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

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

macro_rules! lib {
    () => {
        match Lib::new() {
            Some(l) => l,
            None => {
                eprintln!("ffmpeg が無いので skip");
                return;
            }
        }
    };
}

const U1: &str = "https://youtu.be/v1";
const U2: &str = "https://youtu.be/v2";

#[tokio::test]
async fn ok_video_is_staged_in_inbox_with_tags_sidecar_and_archive() {
    let lib = lib!();
    lib.video(U1, "v1", "KnownCh", "Song One (Official Video)", true);
    lib.start();
    let (id, st) = lib.run(U1).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.job(id));

    // Inbox: youtube/<albumartist>/<album>/<YYYYMMDD> <title> [<id>].opus
    let rel = "youtube/Artist A/Songs of A/20260901 Song One [v1].opus";
    let p = lib.inbox_path(rel);
    assert!(p.exists(), "{}", p.display());
    let (af, pictures) = spindle::domain::tags::read_audio_file_with_pictures(
        std::fs::File::open(&p).unwrap(),
        Some("opus"),
    )
    .unwrap();
    assert_eq!(af.tags.first("TITLE"), Some("Song One"));
    assert_eq!(af.tags.first("ARTIST"), Some("Artist A"));
    assert_eq!(af.tags.first("ALBUM"), Some("Songs of A"));
    assert_eq!(af.tags.first("ALBUMARTIST"), Some("Artist A"));
    assert_eq!(af.tags.first("DATE"), Some("2026"));
    assert_eq!(af.tags.first("ORIGINALARTIST"), Some("Someone"));
    assert_eq!(
        af.tags.first("SOURCE_URL"),
        Some("https://www.youtube.com/watch?v=v1")
    );
    assert_eq!(af.tags.first("TRACKNUMBER"), None, "採番は Inbox");
    assert_eq!(pictures.len(), 1);
    // サイドカー
    let dir = spindle::domain::relpath::RelPath::parse("youtube/Artist A/Songs of A").unwrap();
    let s = Sidecar::read(&lib.inbox, &dir).unwrap().unwrap();
    assert_eq!(s.category.as_deref(), Some("Pop"));
    let e = &s.files["20260901 Song One [v1].opus"];
    assert_eq!(e.verdict, "ok");
    assert_eq!(e.source, "youtube");
    assert_eq!(e.channel.as_deref(), Some("KnownCh"));
    assert_eq!(e.url.as_deref(), Some("https://www.youtube.com/watch?v=v1"));
    assert!(e.message.is_none());
    // Archive の webm、inbox ジョブの投入、作業領域の後始末
    assert!(lib.dir.path().join("Archive/youtube/v1.webm").exists());
    assert_eq!(
        lib.count("SELECT count(*) FROM jobs WHERE type = 'inbox' AND state = 'queued'"),
        1
    );
    assert!(std::fs::read_dir(lib.dir.path().join("tmp/ytdl"))
        .map(|d| d.count() == 0)
        .unwrap_or(true));
    // yt-dlp は dump と download の 2 回。download は webm の音声だけ、URL は `--` の後
    let calls = lib.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[0].contains("--dump-single-json") && calls[0].contains("--flat-playlist"));
    assert!(calls[1].contains("ba[ext=webm]") && calls[1].ends_with(&format!("-- {U1}")));
}

#[tokio::test]
async fn unmatched_video_goes_to_the_catch_all_directory_with_the_message() {
    let lib = lib!();
    lib.video(U1, "v1", "OtherCh", "Some Title", true);
    lib.start();
    let (id, st) = lib.run(U1).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.job(id));
    let rel = "youtube/_unmatched/OtherCh/20260901 Some Title [v1].opus";
    let p = lib.inbox_path(rel);
    assert!(p.exists(), "{}", p.display());
    let af = spindle::domain::tags::read_audio_file(std::fs::File::open(&p).unwrap(), Some("opus"))
        .unwrap();
    assert_eq!(af.tags.first("TITLE"), Some("Some Title"));
    assert_eq!(af.tags.first("ALBUM"), None);
    assert_eq!(
        af.tags.first("SOURCE_URL"),
        Some("https://www.youtube.com/watch?v=v1")
    );
    let dir = spindle::domain::relpath::RelPath::parse("youtube/_unmatched/OtherCh").unwrap();
    let s = Sidecar::read(&lib.inbox, &dir).unwrap().unwrap();
    assert!(s.category.is_none());
    let e = &s.files["20260901 Some Title [v1].opus"];
    assert_eq!(e.verdict, "unmatched");
    assert_eq!(e.message.as_deref(), Some("ルールを足してください"));
    assert!(lib.dir.path().join("Archive/youtube/v1.webm").exists());
}

#[tokio::test]
async fn skip_is_done_without_downloading() {
    let lib = lib!();
    lib.video(U1, "v1", "SkipCh", "Announcement", true);
    lib.start();
    let (id, st) = lib.run(U1).await;
    assert_eq!(st, JobState::Done);
    assert_eq!(lib.calls().len(), 1, "download を呼ばない");
    assert!(!lib.inbox_path("youtube").exists());
    assert!(
        lib.note(id)
            .as_deref()
            .unwrap_or("")
            .starts_with("プラグインが skip"),
        "{:?}",
        lib.note(id)
    );
    assert_eq!(
        lib.count("SELECT count(*) FROM jobs WHERE type = 'inbox'"),
        0
    );
}

#[tokio::test]
async fn playlist_is_expanded_into_one_job_per_entry() {
    let lib = lib!();
    let pl = "https://www.youtube.com/playlist?list=PL1";
    lib.dump(
        pl,
        r#"{"_type":"playlist","id":"PL1","title":"List","entries":[
             {"_type":"url","id":"v1","url":"https://www.youtube.com/watch?v=v1"},
             {"_type":"url","id":"v2","url":"https://www.youtube.com/watch?v=v2"}]}"#,
    );
    lib.video(
        "https://www.youtube.com/watch?v=v1",
        "v1",
        "KnownCh",
        "One",
        true,
    );
    lib.video(
        "https://www.youtube.com/watch?v=v2",
        "v2",
        "KnownCh",
        "Two",
        true,
    );
    lib.start();
    let (_, st) = lib.run(pl).await;
    assert_eq!(st, JobState::Done);
    let ids: Vec<i64> = lib
        .conn()
        .prepare("SELECT id FROM jobs WHERE type = 'ytdl' AND dedup_key LIKE ?1 ORDER BY id")
        .unwrap()
        .query_map(
            [format!("{DEDUP_PREFIX}https://www.youtube.com/watch%")],
            |r| r.get(0),
        )
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(ids.len(), 2);
    for id in ids {
        assert_eq!(lib.wait(id).await, JobState::Done, "{:?}", lib.job(id));
    }
    assert!(lib
        .inbox_path("youtube/Artist A/Songs of A/20260901 Song One [v1].opus")
        .exists());
    assert!(lib
        .inbox_path("youtube/Artist A/Songs of A/20260901 Song One [v2].opus")
        .exists());
    let dir = spindle::domain::relpath::RelPath::parse("youtube/Artist A/Songs of A").unwrap();
    assert_eq!(
        Sidecar::read(&lib.inbox, &dir)
            .unwrap()
            .unwrap()
            .files
            .len(),
        2
    );
}

/// Inbox に .opus を置いた直後に落ちた（サイドカー / inbox ジョブの投入前）状況からの再実行:
/// 同名のファイルの SOURCE_URL が同じなら自分の成果物として採用し、サイドカーと投入を済ませる
#[tokio::test]
async fn rerun_after_placing_the_opus_adopts_it_and_finishes() {
    let lib = lib!();
    lib.video(U1, "v1", "KnownCh", "Song One", true);
    lib.start();
    let (id, st) = lib.run(U1).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.job(id));
    let dir = spindle::domain::relpath::RelPath::parse("youtube/Artist A/Songs of A").unwrap();
    // 「置いた後に落ちた」を作る: サイドカーと inbox ジョブを消す（inbox_files はまだ無い）
    std::fs::remove_file(lib.inbox_path("youtube/Artist A/Songs of A/spindle-inbox.json")).unwrap();
    lib.conn().execute("DELETE FROM jobs", []).unwrap();
    let (id, st) = lib.run(U1).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.job(id));
    let s = Sidecar::read(&lib.inbox, &dir).unwrap().unwrap();
    assert_eq!(s.files["20260901 Song One [v1].opus"].verdict, "ok");
    assert_eq!(
        lib.count("SELECT count(*) FROM jobs WHERE type = 'inbox' AND state = 'queued'"),
        1
    );
    assert_eq!(
        std::fs::read_dir(lib.inbox_path("youtube/Artist A/Songs of A"))
            .unwrap()
            .count(),
        2,
        "同じファイルを二重に置かない"
    );

    // 同名でも SOURCE_URL が違う（人が置いた等）なら Fatal
    std::fs::remove_file(lib.inbox_path("youtube/Artist A/Songs of A/spindle-inbox.json")).unwrap();
    lib.conn().execute("DELETE FROM jobs", []).unwrap();
    set_tags(
        &lib.inbox_path("youtube/Artist A/Songs of A/20260901 Song One [v1].opus"),
        "opus",
        &[("SOURCE_URL", &["https://other"])],
    );
    let (id, st) = lib.run(U1).await;
    assert_eq!(st, JobState::Failed);
    assert!(
        lib.job(id).1.as_deref().unwrap_or("").contains("同名"),
        "{:?}",
        lib.job(id)
    );
}

/// 走査が先に件を作っていた（inbox_files に SOURCE_URL がある）ときの再実行: 置き場所が自分の宛先と
/// 同じなら、ダウンロードせずにサイドカーと投入だけ済ませる
#[tokio::test]
async fn rerun_after_the_scan_registered_the_file_completes_without_downloading() {
    let lib = lib!();
    lib.video(U1, "v1", "KnownCh", "Song One", true);
    lib.start();
    let (id, st) = lib.run(U1).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.job(id));
    std::fs::remove_file(lib.inbox_path("youtube/Artist A/Songs of A/spindle-inbox.json")).unwrap();
    lib.conn().execute("DELETE FROM jobs", []).unwrap();
    spindle::import::inbox::scan_inbox(&lib.db, &lib.inbox, 1000)
        .await
        .unwrap();
    assert_eq!(lib.count("SELECT count(*) FROM inbox_files"), 1);
    let calls_before = lib.calls().len();
    let (id, st) = lib.run(U1).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.job(id));
    assert_eq!(
        lib.calls().len(),
        calls_before + 1,
        "dump だけで download しない"
    );
    let dir = spindle::domain::relpath::RelPath::parse("youtube/Artist A/Songs of A").unwrap();
    assert!(Sidecar::read(&lib.inbox, &dir).unwrap().is_some());
    assert_eq!(
        lib.count("SELECT count(*) FROM jobs WHERE type = 'inbox' AND state = 'queued'"),
        1
    );
}

/// 走査済みの採用は rel_path_key で比べ（大小文字 / NFD 違いでも自分の宛先）、DB を信じず実ファイルの
/// SOURCE_URL を読み直す（走査後に差し替えられていれば採用しない）
#[tokio::test]
async fn adoption_of_a_scanned_file_uses_the_path_key_and_rereads_the_file() {
    let lib = lib!();
    lib.video(U1, "v1", "KnownCh", "Song One", true);
    lib.start();
    let (id, st) = lib.run(U1).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.job(id));
    let rel = "youtube/Artist A/Songs of A/20260901 Song One [v1].opus";
    std::fs::remove_file(lib.inbox_path("youtube/Artist A/Songs of A/spindle-inbox.json")).unwrap();
    lib.conn().execute("DELETE FROM jobs", []).unwrap();
    spindle::import::inbox::scan_inbox(&lib.db, &lib.inbox, 1000)
        .await
        .unwrap();
    // (1) 走査が持つ rel_path の表記が違っても（大小文字）、key が同じなら自分の宛先
    lib.conn()
        .execute("UPDATE inbox_files SET rel_path = ?1", [rel.to_uppercase()])
        .unwrap();
    let calls_before = lib.calls().len();
    let (id, st) = lib.run(U1).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.job(id));
    assert_eq!(lib.calls().len(), calls_before + 1, "download しない");

    // (2) 走査の後に実ファイルの SOURCE_URL が差し替えられていれば、DB が旧値でも採用しない
    std::fs::remove_file(lib.inbox_path("youtube/Artist A/Songs of A/spindle-inbox.json")).unwrap();
    lib.conn().execute("DELETE FROM jobs", []).unwrap();
    set_tags(
        &lib.inbox_path(rel),
        "opus",
        &[("SOURCE_URL", &["https://other"])],
    );
    let (id, st) = lib.run(U1).await;
    assert_eq!(st, JobState::Failed, "{:?}", lib.job(id));
    assert!(
        lib.job(id).1.as_deref().unwrap_or("").contains("同名"),
        "{:?}",
        lib.job(id)
    );
    assert!(!lib
        .inbox_path("youtube/Artist A/Songs of A/spindle-inbox.json")
        .exists());

    // (3) DB にはあるが実ファイルが消えていれば、普通にダウンロードして置き直す
    lib.conn().execute("DELETE FROM jobs", []).unwrap();
    std::fs::remove_file(lib.inbox_path(rel)).unwrap();
    let calls_before = lib.calls().len();
    let (id, st) = lib.run(U1).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.job(id));
    assert_eq!(lib.calls().len(), calls_before + 2, "dump と download");
    assert!(lib.inbox_path(rel).exists());
}

#[tokio::test]
async fn already_imported_url_is_fatal() {
    let lib = lib!();
    lib.video(U1, "v1", "KnownCh", "One", true);
    // Library に SOURCE_URL を持つ active なトラック
    lib.conn()
        .execute_batch(
            "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                                 title, artist_display, album, albumartist, seen_at)
             VALUES (1, 'Pop/A/B/01 One.opus', 'pop/a/b/01 one.opus', 0, 0, 0, 'opus', 0, 't', 'a', 'al', 'aa', 0);
             INSERT INTO track_tags (track_id, key, idx, value)
             VALUES (1, 'SOURCE_URL', 0, 'https://www.youtube.com/watch?v=v1');",
        )
        .unwrap();
    lib.start();
    let (id, st) = lib.run(U1).await;
    assert_eq!(st, JobState::Failed);
    let (attempts, err) = lib.job(id);
    assert_eq!(attempts, 1);
    assert!(
        err.as_deref().unwrap_or("").contains("Pop/A/B/01 One.opus"),
        "{err:?}"
    );
    assert_eq!(lib.calls().len(), 1, "download を呼ばない");

    // Inbox にある（承認待ち）ものも同じ
    lib.conn()
        .execute("UPDATE tracks SET missing_since = 1", [])
        .unwrap();
    lib.conn()
        .execute_batch(
            r#"INSERT INTO inbox_items (id, rel_dir, rel_dir_key, detected_at, seen_at) VALUES (1, 'youtube/x', 'youtube/x', 0, 0);
               INSERT INTO inbox_files (item_id, rel_path, rel_path_key, inode, size, mtime_ns, ctime_ns, codec, lossless, tags)
               VALUES (1, 'youtube/x/a.opus', 'youtube/x/a.opus', 1, 1, 0, 0, 'opus', 0,
                       '[["TITLE","t"],["SOURCE_URL","https://www.youtube.com/watch?v=v1"]]');"#,
        )
        .unwrap();
    lib.conn()
        .execute("UPDATE jobs SET dedup_key = NULL", [])
        .unwrap();
    let (id, st) = lib.run(U1).await;
    assert_eq!(st, JobState::Failed);
    assert!(
        lib.job(id)
            .1
            .as_deref()
            .unwrap_or("")
            .contains("youtube/x/a.opus"),
        "{:?}",
        lib.job(id)
    );
}

#[tokio::test]
async fn download_failure_is_retried_but_a_missing_webm_format_is_fatal() {
    let lib = lib!();
    lib.video(U1, "v1", "KnownCh", "One", true);
    std::fs::write(lib.fake.join("fail_download"), b"").unwrap();
    lib.start();
    let (id, st) = lib.run(U1).await;
    assert_eq!(st, JobState::Queued, "再試行を予約する");
    let (attempts, err) = lib.job(id);
    assert_eq!(attempts, 1);
    assert!(
        err.as_deref().unwrap_or("").contains("network down"),
        "{err:?}"
    );
    assert!(!lib.inbox_path("youtube").exists());

    // webm の音声が無い動画は再試行しても変わらない
    let u2 = "https://youtu.be/v2";
    lib.video(u2, "v2", "KnownCh", "Two", false);
    let (id, st) = lib.run(u2).await;
    assert_eq!(st, JobState::Failed);
    assert!(
        lib.job(id).1.as_deref().unwrap_or("").contains("webm"),
        "{:?}",
        lib.job(id)
    );
}

#[tokio::test]
async fn plugin_fault_and_unknown_video_are_fatal() {
    let lib = lib!();
    lib.video(U1, "v1", "BrokenCh", "One", true);
    lib.start();
    let (id, st) = lib.run(U1).await;
    assert_eq!(st, JobState::Failed, "{:?}", lib.job(id));
    assert_eq!(lib.job(id).0, 1);
    // dump が失敗（存在しない動画等）は Failed（再試行）: yt-dlp の失敗は一時的か判別できない
    let (id, st) = lib.run("https://youtu.be/nope").await;
    assert_eq!(st, JobState::Queued, "{:?}", lib.job(id));
    // ただし yt-dlp が「対応していない URL」と言ったものは再試行しても変わらない
    std::fs::write(lib.fake.join("unsupported"), b"").unwrap();
    let (id, st) = lib.run("https://example.com/page").await;
    assert_eq!(st, JobState::Failed, "{:?}", lib.job(id));
}

// ---------------------------------------------------------------- 純粋な部分

use spindle::import::ytmusic::downloader::{inbox_dir, parse_dump, Dump};

#[test]
fn parse_dump_reads_videos_and_playlists_and_rejects_incomplete_ones() {
    let v = parse_dump(
        br#"{"id":"v1","title":"T","uploader":"U","channel":"C","webpage_url":"https://www.youtube.com/watch?v=v1",
             "upload_date":"20260901","duration":12.6,"formats":[{"ext":"webm","acodec":"opus"}],"extra":1}"#,
    )
    .unwrap();
    let Dump::Video(v) = v else { panic!("{v:?}") };
    assert_eq!(v.id, "v1");
    assert_eq!(v.channel_key(), "U");
    assert_eq!(v.duration_ms, Some(12_600));
    assert!(v.has_webm_audio);
    let item = v.item();
    assert_eq!(item.uploaded_at.as_deref(), Some("2026-09-01"));
    assert_eq!(item.channel_title.as_deref(), Some("C"));
    assert_eq!(
        item.url.as_deref(),
        Some("https://www.youtube.com/watch?v=v1")
    );
    // uploader が無ければ channel、日付が不正なら無し、webm でも acodec none は音声ではない
    let v = parse_dump(
        br#"{"id":"v2","title":"T","channel":"C","webpage_url":"u","upload_date":"2026-09",
             "formats":[{"ext":"webm","acodec":"none"},{"ext":"m4a","acodec":"aac"}]}"#,
    )
    .unwrap();
    let Dump::Video(v) = v else { panic!("{v:?}") };
    assert_eq!(v.channel_key(), "C");
    assert_eq!(v.upload_date, None);
    assert!(!v.has_webm_audio);
    assert_eq!(v.item().uploaded_at, None);
    // playlist: YouTube の entry は id から正規形（https://www.youtube.com/watch?v=<id>）を組む。
    // url が youtu.be 形・list= 付きでも、SOURCE_URL に書く webpage_url の正規形と一致する（P4-13）。
    // YouTube 以外は webpage_url → url の順。どちらも無ければ落とす
    let p = parse_dump(
        br#"{"_type":"playlist","entries":[
             {"id":"a1","url":"https://youtu.be/a1","ie_key":"Youtube"},
             {"id":"b2","url":"https://www.youtube.com/watch?v=b2&list=PLx","webpage_url":"https://www.youtube.com/watch?v=b2","ie_key":"Youtube"},
             {"url":"https://example.com/u","webpage_url":"https://example.com/w"},
             {"url":"https://example.com/only-url"},
             {"id":"m1","url":"https://music.youtube.com/watch?v=m1"},
             {"id":"x1","url":"https://notyoutube.com/x1"},
             {"id":"x2","url":"https://example.com/path/youtube.com/x2"},
             {"id":"x3","url":"https://example.com/?u=https://youtu.be/x3"},
             {"id":"c"}]}"#,
    )
    .unwrap();
    assert_eq!(
        p,
        Dump::Playlist(vec![
            "https://www.youtube.com/watch?v=a1".into(),
            "https://www.youtube.com/watch?v=b2".into(),
            "https://example.com/w".into(),
            "https://example.com/only-url".into(),
            // サブドメイン（music.youtube.com）は YouTube
            "https://www.youtube.com/watch?v=m1".into(),
            // ホスト名が youtube.com でない / path や query に youtube.com を含むだけのものは YouTube ではない
            "https://notyoutube.com/x1".into(),
            "https://example.com/path/youtube.com/x2".into(),
            "https://example.com/?u=https://youtu.be/x3".into(),
        ])
    );
    // 不足
    for body in [
        r#"{"title":"T","webpage_url":"u"}"#,
        r#"{"id":"v","webpage_url":"u"}"#,
        r#"{"id":"v","title":"T"}"#,
        "not json",
    ] {
        assert!(parse_dump(body.as_bytes()).is_err(), "{body}");
    }
}

#[test]
fn inbox_names_are_sanitized_and_bounded() {
    let mk = |date: Option<&str>| {
        let Dump::Video(v) = parse_dump(
            format!(
                r#"{{"id":"v1","title":"T","webpage_url":"u"{}}}"#,
                date.map(|d| format!(r#","upload_date":"{d}""#))
                    .unwrap_or_default()
            )
            .as_bytes(),
        )
        .unwrap() else {
            panic!()
        };
        v
    };
    assert_eq!(
        mk(Some("20260901")).inbox_file_name("A / B: C?"),
        "20260901 A ／ B： C？ [v1].opus"
    );
    assert_eq!(mk(None).inbox_file_name("T"), "T [v1].opus");
    assert_eq!(mk(None).inbox_file_name("   "), "_ [v1].opus");
    // 長いタイトルは切り詰めるが id と日付は残る
    let long = "あ".repeat(300);
    let name = mk(Some("20260901")).inbox_file_name(&long);
    assert!(name.len() <= 255, "{}", name.len());
    assert!(name.starts_with("20260901 あ") && name.ends_with(" [v1].opus"));
    // ディレクトリ
    let t = spindle::import::ytmusic::Track {
        title: "t".into(),
        artists: vec!["a".into()],
        albumartist: "Artist/X".into(),
        album: "Album: Y".into(),
        category: None,
        date: None,
        tags: vec![],
    };
    assert_eq!(
        inbox_dir(Some(&t), "ch").unwrap().as_str(),
        "youtube/Artist／X/Album： Y"
    );
    assert_eq!(
        inbox_dir(None, "Ch: One").unwrap().as_str(),
        "youtube/_unmatched/Ch： One"
    );
    assert_eq!(
        inbox_dir(None, "").unwrap().as_str(),
        "youtube/_unmatched/_"
    );
}

#[test]
fn sweep_tmp_removes_leftover_work_dirs() {
    use spindle::import::ytmusic::downloader::sweep_tmp;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ytdl");
    std::fs::create_dir_all(root.join("12")).unwrap();
    std::fs::write(root.join("12/v.webm"), b"x").unwrap();
    assert_eq!(sweep_tmp(&root), 1);
    assert!(!root.join("12").exists());
    // 無ければ 0
    assert_eq!(sweep_tmp(&dir.path().join("none")), 0);
}

/// 実 yt-dlp（と JS ランタイム）で短い動画を 1 本取る。ネットワークが要るので CI では走らせない:
/// `cargo test --test ytmusic_download -- --ignored real_ytdlp`
#[tokio::test]
#[ignore]
async fn real_ytdlp_end_to_end() {
    let lib = lib!();
    let mut env = lib.env();
    env.ytdlp = vec!["yt-dlp".to_owned()];
    env.download_timeout = Duration::from_secs(300);
    let mut reg = Registry::new();
    reg.register(JobType::Ytdl, Arc::new(YtdlHandler::new(env)));
    lib.jobs.start(reg, lib.shutdown.clone());
    // 「Me at the zoo」（YouTube 最初の動画。19 秒）。uploader は偽プラグインに無いので受け皿へ
    let url = "https://www.youtube.com/watch?v=jNQXAC9IVRw";
    let (id, st) = lib.run(url).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.job(id));
    let dir = lib.inbox_path("youtube/_unmatched/jawed");
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n.ends_with(" [jNQXAC9IVRw].opus")),
        "{names:?}"
    );
    assert!(lib
        .dir
        .path()
        .join("Archive/youtube/jNQXAC9IVRw.webm")
        .exists());
    let opus = names.iter().find(|n| n.ends_with(".opus")).unwrap();
    let (af, pictures) = spindle::domain::tags::read_audio_file_with_pictures(
        std::fs::File::open(dir.join(opus)).unwrap(),
        Some("opus"),
    )
    .unwrap();
    assert_eq!(af.tags.first("TITLE"), Some("Me at the zoo"));
    assert_eq!(af.tags.first("SOURCE_URL"), Some(url));
    assert_eq!(pictures.len(), 1);
    assert!(af.duration_ms.unwrap_or(0) > 15_000);
}

/// P4-13: 再生リストの展開時に、Library / Inbox に `SOURCE_URL` のある動画は投入しない（取り込み済みの
/// 失敗ジョブで一覧を埋めない）
#[tokio::test]
async fn playlist_expansion_skips_videos_already_imported() {
    let lib = lib!();
    let pl = "https://www.youtube.com/playlist?list=PL2";
    lib.dump(
        pl,
        r#"{"_type":"playlist","id":"PL2","title":"List","entries":[
             {"_type":"url","id":"v1","url":"https://www.youtube.com/watch?v=v1"},
             {"_type":"url","id":"v2","url":"https://www.youtube.com/watch?v=v2"},
             {"_type":"url","id":"v3","url":"https://www.youtube.com/watch?v=v3"}]}"#,
    );
    lib.video(
        "https://www.youtube.com/watch?v=v3",
        "v3",
        "KnownCh",
        "Three",
        true,
    );
    // v1 は Library、v2 は Inbox に取り込み済み
    let c = lib.conn();
    c.execute(
        "INSERT INTO tracks (rel_path, rel_path_key, dev, inode, size, mtime_ns, ctime_ns, codec, lossless,
           title, artist_display, album, albumartist, track_no, disc_no, seen_at)
         VALUES ('A/01.opus', 'a/01.opus', 1, 1, 1, 1, 1, 'opus', 0, 'One', 'Ar', 'Al', 'AA', 1, 1, 1)",
        [],
    )
    .unwrap();
    let tid = c.last_insert_rowid();
    c.execute(
        "INSERT INTO track_tags (track_id, key, idx, value) VALUES (?1, 'SOURCE_URL', 0, 'https://www.youtube.com/watch?v=v1')",
        [tid],
    )
    .unwrap();
    c.execute(
        "INSERT INTO inbox_items (rel_dir, rel_dir_key, state, detected_at, seen_at) VALUES ('youtube/x', 'youtube/x', 'pending', 1, 1)",
        [],
    )
    .unwrap();
    let iid = c.last_insert_rowid();
    c.execute(
        "INSERT INTO inbox_files (item_id, rel_path, rel_path_key, inode, size, mtime_ns, ctime_ns, codec, lossless, tags)
         VALUES (?1, 'youtube/x/v2.opus', 'youtube/x/v2.opus', 2, 1, 1, 1, 'opus', 0, '[[\"SOURCE_URL\",\"https://www.youtube.com/watch?v=v2\"]]')",
        [iid],
    )
    .unwrap();
    drop(c);

    lib.start();
    let (id, st) = lib.run(pl).await;
    assert_eq!(st, JobState::Done);
    let ids: Vec<i64> = lib
        .conn()
        .prepare("SELECT id FROM jobs WHERE type = 'ytdl' AND dedup_key LIKE ?1 ORDER BY id")
        .unwrap()
        .query_map(
            [format!("{DEDUP_PREFIX}https://www.youtube.com/watch%")],
            |r| r.get(0),
        )
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(ids.len(), 1, "v3 だけが投入される");
    assert_eq!(
        lib.note(id).as_deref(),
        Some("再生リストを展開した: 1 件を投入、2 件は取り込み済み")
    );
    assert_eq!(lib.wait(ids[0]).await, JobState::Done);
    assert_eq!(
        lib.note(ids[0]).as_deref(),
        Some("Inbox に置いた: youtube/Artist A/Songs of A/20260901 Song One [v3].opus")
    );
    assert!(lib
        .inbox_path("youtube/Artist A/Songs of A/20260901 Song One [v3].opus")
        .exists());
    let _ = id;
}

// ---------------------------------------------------------------- 購読由来（P4-16、D-78）

/// 購読を登録して id を返す
async fn subscribe(
    lib: &Lib,
    albumartist: &str,
    album: &str,
    category: Option<&str>,
    align: bool,
) -> i64 {
    use spindle::db::subscriptions::{insert, NewSubscription, WriteOutcome};
    let s = NewSubscription {
        list_id: "PL1".to_owned(),
        url: "https://www.youtube.com/playlist?list=PL1".to_owned(),
        albumartist: albumartist.to_owned(),
        album: album.to_owned(),
        category: category.map(str::to_owned),
        align,
        enabled: true,
        max_enqueue: 50,
    };
    match lib.db.write(move |c| insert(c, &s, 1)).await.unwrap() {
        WriteOutcome::Ok(id) => id,
        other => panic!("{other:?}"),
    }
}

async fn run_for(lib: &Lib, url: &str, sub: i64, position: u32) -> (i64, JobState) {
    use spindle::jobs::handlers::ytdl::new_subscription_ytdl_job;
    let id = match lib
        .jobs
        .enqueue(new_subscription_ytdl_job(url, sub, position))
        .await
        .unwrap()
    {
        EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
    };
    (id, lib.wait(id).await)
}

/// 購読由来: 追記先（ALBUMARTIST / ALBUM / category）は購読の値、TITLE / ARTIST はプラグイン、
/// align なら TRACKNUMBER = 位置。サイドカーに購読 id と位置
#[tokio::test]
async fn subscription_download_uses_the_subscription_target_and_position() {
    let lib = lib!();
    lib.video(U1, "v1", "KnownCh", "Song One (Official Video)", true);
    let sub = subscribe(&lib, "Sub Artist", "Sub Album", Some("Rock"), true).await;
    lib.start();
    let (id, st) = run_for(&lib, U1, sub, 7).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.job(id));
    let rel = "youtube/Sub Artist/Sub Album/20260901 Song One [v1].opus";
    let p = lib.inbox_path(rel);
    assert!(p.exists(), "{}", p.display());
    let af = spindle::domain::tags::read_audio_file(std::fs::File::open(&p).unwrap(), Some("opus"))
        .unwrap();
    assert_eq!(af.tags.first("TITLE"), Some("Song One"));
    assert_eq!(af.tags.first("ARTIST"), Some("Artist A"));
    assert_eq!(af.tags.first("ALBUM"), Some("Sub Album"));
    assert_eq!(af.tags.first("ALBUMARTIST"), Some("Sub Artist"));
    assert_eq!(af.tags.first("TRACKNUMBER"), Some("7"));
    let dir = spindle::domain::relpath::RelPath::parse("youtube/Sub Artist/Sub Album").unwrap();
    let s = Sidecar::read(&lib.inbox, &dir).unwrap().unwrap();
    assert_eq!(s.category.as_deref(), Some("Rock"));
    let e = &s.files["20260901 Song One [v1].opus"];
    assert_eq!((e.subscription_id, e.position), (Some(sub), Some(7)));
    assert_eq!(e.verdict, "ok");
}

/// 購読由来は skip / 判定不能でも投入する（TITLE = 動画タイトル、ARTIST = albumartist）。align が
/// off なら TRACKNUMBER は書かない
#[tokio::test]
async fn subscription_download_ignores_plugin_skip_and_unmatched() {
    let lib = lib!();
    lib.video(U1, "v1", "SkipCh", "Announcement", true);
    lib.video(U2, "v2", "OtherCh", "Some Title", true);
    let sub = subscribe(&lib, "Sub Artist", "Sub Album", None, false).await;
    lib.start();
    let (id, st) = run_for(&lib, U1, sub, 1).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.job(id));
    assert!(
        lib.note(id).unwrap().starts_with("Inbox に置いた"),
        "{:?}",
        lib.note(id)
    );
    let (id2, st) = run_for(&lib, U2, sub, 2).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.job(id2));
    for (name, title, verdict) in [
        ("20260901 Announcement [v1].opus", "Announcement", "skip"),
        ("20260901 Some Title [v2].opus", "Some Title", "unmatched"),
    ] {
        let p = lib.inbox_path(&format!("youtube/Sub Artist/Sub Album/{name}"));
        assert!(p.exists(), "{}", p.display());
        let af =
            spindle::domain::tags::read_audio_file(std::fs::File::open(&p).unwrap(), Some("opus"))
                .unwrap();
        assert_eq!(af.tags.first("TITLE"), Some(title));
        assert_eq!(af.tags.first("ARTIST"), Some("Sub Artist"));
        assert_eq!(af.tags.first("ALBUMARTIST"), Some("Sub Artist"));
        assert_eq!(af.tags.first("ALBUM"), Some("Sub Album"));
        assert_eq!(
            af.tags.first("TRACKNUMBER"),
            None,
            "align が off なら採番は Inbox"
        );
        let dir = spindle::domain::relpath::RelPath::parse("youtube/Sub Artist/Sub Album").unwrap();
        let s = Sidecar::read(&lib.inbox, &dir).unwrap().unwrap();
        assert_eq!(s.files[name].verdict, verdict);
        assert_eq!(s.files[name].subscription_id, Some(sub));
    }
}

/// 購読が消えていれば通常の ytdl として振る舞う（プラグインの宛先、購読の情報なし）
#[tokio::test]
async fn subscription_download_falls_back_when_the_subscription_is_gone() {
    let lib = lib!();
    lib.video(U1, "v1", "KnownCh", "Song One", true);
    lib.start();
    let (id, st) = run_for(&lib, U1, 999, 3).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.job(id));
    let p = lib.inbox_path("youtube/Artist A/Songs of A/20260901 Song One [v1].opus");
    assert!(p.exists());
    let af = spindle::domain::tags::read_audio_file(std::fs::File::open(&p).unwrap(), Some("opus"))
        .unwrap();
    assert_eq!(af.tags.first("TRACKNUMBER"), None);
    let dir = spindle::domain::relpath::RelPath::parse("youtube/Artist A/Songs of A").unwrap();
    let s = Sidecar::read(&lib.inbox, &dir).unwrap().unwrap();
    assert_eq!(s.files["20260901 Song One [v1].opus"].subscription_id, None);
}

/// 束ねた album の category が未推定（NULL）なら購読の category を使う（宛先のディレクトリは category で決まる）
#[tokio::test]
async fn subscription_download_uses_the_bound_album_but_falls_back_to_its_own_category() {
    let lib = lib!();
    lib.video(U1, "v1", "KnownCh", "Song One", true);
    let sub = subscribe(&lib, "Old Artist", "Old Album", Some("Rock"), true).await;
    lib.conn()
        .execute_batch(
            "INSERT INTO albums (id, rel_dir, rel_dir_key, albumartist, album) VALUES (5, 'Rock/New Artist/New Album', 'rock/new artist/new album', 'New Artist', 'New Album');",
        )
        .unwrap();
    assert_eq!(
        spindle::db::subscriptions::bind_album(&lib.conn(), sub, 5).unwrap(),
        spindle::db::subscriptions::BindOutcome::Bound
    );
    lib.start();
    let (id, st) = run_for(&lib, U1, sub, 2).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.job(id));
    let p = lib.inbox_path("youtube/New Artist/New Album/20260901 Song One [v1].opus");
    assert!(p.exists(), "{}", p.display());
    let dir = spindle::domain::relpath::RelPath::parse("youtube/New Artist/New Album").unwrap();
    let s = Sidecar::read(&lib.inbox, &dir).unwrap().unwrap();
    assert_eq!(s.category.as_deref(), Some("Rock"));
}

/// 購読が消えていれば skip も通常どおり（ダウンロードしない）
#[tokio::test]
async fn subscription_download_respects_skip_when_the_subscription_is_gone() {
    let lib = lib!();
    lib.video(U1, "v1", "SkipCh", "Announcement", true);
    lib.start();
    let (id, st) = run_for(&lib, U1, 999, 3).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.job(id));
    assert!(
        lib.note(id).unwrap().starts_with("プラグインが skip"),
        "{:?}",
        lib.note(id)
    );
    assert!(!lib.inbox_path("youtube").exists());
    assert_eq!(lib.calls().len(), 1, "download を呼ばない");
}

//! YouTube のテストの土台: 偽の yt-dlp（bash。dump と download を演じる）と、偽のプラグインで動く
//! ダウンローダの環境（`Lib`）。プラグインの本文はテストごとに渡す

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use spindle::db::Db;
use spindle::fsroot::RootDir;
use spindle::import::ytmusic::downloader::DownloaderEnv;
use spindle::import::ytmusic::MetadataProvider;
use spindle::jobs::handlers::ytdl::{new_ytdl_job, YtdlHandler};
use spindle::jobs::{EnqueueResult, JobState, JobType, Jobs, Registry};

/// 偽の yt-dlp。`--dump-single-json` なら `dump/<url をキー化した名前>.json` を返し、download なら
/// `-o` のテンプレートに `audio.webm` と `thumb.jpg` を置く。呼び出しは `calls.log` に残す
pub const FAKE_YTDLP: &str = r#"
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

pub struct Lib {
    pub dir: tempfile::TempDir,
    pub db_path: PathBuf,
    pub db: Arc<Db>,
    pub jobs: Arc<Jobs>,
    pub inbox: Arc<RootDir>,
    pub fake: PathBuf,
    pub shutdown: CancellationToken,
}

impl Lib {
    /// `plugin` は偽のメタデータプラグイン（bash のスクリプト本文）
    pub fn new(plugin: &str) -> Option<Self> {
        let dir = tempfile::tempdir().unwrap();
        for d in ["Inbox", "Archive", "tmp", "fake", "fake/dump"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let fake = dir.path().join("fake");
        super::make_audio(&fake, "audio.webm", "webm", 1)?;
        // サムネイル（ffmpeg で 16x16 の JPEG を作る）
        let st = std::process::Command::new(super::ffmpeg()?)
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
        std::fs::write(fake.join("plugin.sh"), plugin).unwrap();
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

    pub fn env(&self) -> DownloaderEnv {
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
            ffmpeg: super::ffmpeg().unwrap_or_else(|| PathBuf::from("ffmpeg")),
            tmp_root: self.dir.path().join("tmp").join("ytdl"),
            download_timeout: Duration::from_secs(60),
        }
    }

    pub fn start(&self) {
        let mut reg = Registry::new();
        reg.register(JobType::Ytdl, Arc::new(YtdlHandler::new(self.env())));
        self.jobs.start(reg, self.shutdown.clone());
    }

    /// dump の JSON を置く（動画）
    pub fn video(&self, url: &str, id: &str, uploader: &str, title: &str, webm: bool) {
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

    pub fn dump(&self, url: &str, json: &str) {
        let key: String = url
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        std::fs::write(self.fake.join("dump").join(format!("{key}.json")), json).unwrap();
    }

    pub fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.fake.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    pub async fn run(&self, url: &str) -> (i64, JobState) {
        let id = match self.jobs.enqueue(new_ytdl_job(url)).await.unwrap() {
            EnqueueResult::Inserted(j) | EnqueueResult::Duplicate(j) => j,
        };
        (id, self.wait(id).await)
    }

    pub async fn wait(&self, id: i64) -> JobState {
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

    pub fn note(&self, id: i64) -> Option<String> {
        self.conn()
            .query_row("SELECT note FROM jobs WHERE id = ?1", [id], |r| r.get(0))
            .unwrap()
    }

    pub fn job(&self, id: i64) -> (i64, Option<String>) {
        self.conn()
            .query_row(
                "SELECT attempts, last_error FROM jobs WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    pub fn conn(&self) -> Connection {
        Connection::open(&self.db_path).unwrap()
    }

    pub fn inbox_path(&self, rel: &str) -> PathBuf {
        self.dir.path().join("Inbox").join(rel)
    }

    pub fn count(&self, sql: &str) -> i64 {
        self.conn().query_row(sql, [], |r| r.get(0)).unwrap()
    }
}

impl Drop for Lib {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// テスト用: ファイルのタグを直接書き換える（`write_tag_changes` を tmp 無しで当てる）
pub fn set_tags(path: &std::path::Path, ext: &str, tags: &[(&str, &[&str])]) {
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

/// `Lib::new(plugin)`。ffmpeg が無ければテストを skip する
#[allow(unused_macros)]
macro_rules! lib {
    ($plugin:expr) => {
        match $crate::common::ytmusic::Lib::new($plugin) {
            Some(l) => l,
            None => {
                eprintln!("ffmpeg が無いので skip");
                return;
            }
        }
    };
}
#[allow(unused_imports)]
pub(crate) use lib;

/// 購読を登録して id を返す
pub async fn subscribe(
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

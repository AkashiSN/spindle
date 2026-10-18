//! Inbox の件の再判定（`import::ytmusic::rejudge`）。偽の yt-dlp で置いた曲を、応答を切り替えられる
//! 偽のプラグインで判定し直し、タグ・ファイル名・置き場所・サイドカーが初回と同じ形になることを
//! 確かめる。実タイトルや固有名詞は使わない。ffmpeg が無ければ skip

#![cfg(target_os = "linux")]

mod common;

use std::path::Path;

use tokio_util::sync::CancellationToken;

use common::ytmusic::*;
use spindle::domain::relpath::RelPath;
use spindle::domain::tags::{read_audio_file, read_audio_file_with_pictures};
use spindle::import::sidecar::Sidecar;
use spindle::import::ytmusic::rejudge::{rejudge_item, RejudgeError, RejudgeReport};
use spindle::jobs::JobState;

/// 偽のプラグイン。`fake/mode` の中身で応答を切り替える（BrokenCh は `fake/broken` があれば不正な JSON）。
/// mode が無ければ KnownCh → a、他 → unmatched
const PLUGIN: &str = r#"
IN=$(cat)
FAKE=$(dirname "$0")
case "$IN" in
  *'"channel":"BrokenCh"'*) if [ -f "$FAKE/broken" ]; then echo 'not json'; exit 0; fi ;;
esac
mode=$(cat "$FAKE/mode" 2>/dev/null)
if [ -z "$mode" ]; then
  case "$IN" in
    *'"channel":"KnownCh"'*|*'"channel":"BrokenCh"'*) mode=a ;;
    *) mode=unmatched ;;
  esac
fi
case "$mode" in
  a) echo '{"protocol":1,"ok":true,"track":{"title":"Song One","artists":["Artist A"],"albumartist":"Artist A","album":"Songs of A","category":"Pop","date":"2026","tags":[["ORIGINALARTIST","Someone"]]},"plugin":{"name":"fake","version":"v-a"}}' ;;
  b) echo '{"protocol":1,"ok":true,"track":{"title":"Song Two","artists":["Artist A","Guest"],"albumartist":"Artist A","album":"Songs of A","category":"Pop","date":"2026"},"plugin":{"name":"fake","version":"v-b"}}' ;;
  c) echo '{"protocol":1,"ok":true,"track":{"title":"Song One","artists":["Artist A"],"albumartist":"Artist A","album":"Other Album","category":"Pop","date":"2026"},"plugin":{"name":"fake","version":"v-c"}}' ;;
  skip) echo '{"protocol":1,"ok":false,"reason":"skip","message":"配信の告知","plugin":{"name":"fake","version":"v-s"}}' ;;
  *) echo '{"protocol":1,"ok":false,"reason":"unmatched","message":"ルールを足してください","plugin":{"name":"fake","version":"v-u"}}' ;;
esac
"#;

const U1: &str = "https://youtu.be/v1";
const U2: &str = "https://youtu.be/v2";
const ALBUM_DIR: &str = "youtube/Artist A/Songs of A";

impl Lib {
    fn mode(&self, mode: &str) {
        std::fs::write(self.fake.join("mode"), mode).unwrap();
    }
}

fn rel(s: &str) -> RelPath {
    RelPath::parse(s).unwrap()
}

/// `url` を ytdl ジョブで置く（置いた後の inbox ジョブは走らない）
async fn stage(lib: &Lib, url: &str) {
    let (id, st) = lib.run(url).await;
    assert_eq!(st, JobState::Done, "{:?}", lib.job(id));
}

/// 件の行（pending）を作って id を返す
fn pending_item(lib: &Lib, dir: &str) -> i64 {
    spindle::db::inbox::insert_item(&lib.conn(), dir, &rel(dir).key(), 1).unwrap()
}

async fn rejudge(lib: &Lib, item_id: i64) -> Result<RejudgeReport, RejudgeError> {
    let (tx, _rx) = tokio::sync::watch::channel((0, 0));
    rejudge_item(&lib.env(), item_id, &CancellationToken::new(), &tx).await
}

fn tags_of(path: &Path) -> spindle::domain::tags::TagSet {
    read_audio_file(std::fs::File::open(path).unwrap(), Some("opus"))
        .unwrap()
        .tags
}

fn values(tags: &spindle::domain::tags::TagSet, key: &str) -> Vec<String> {
    tags.values(key).map(str::to_owned).collect()
}

/// ディレクトリの音声ファイル名（名前順）
fn audio_names(lib: &Lib, dir: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(lib.inbox_path(dir))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".opus"))
        .collect();
    v.sort();
    v
}

fn sidecar(lib: &Lib, dir: &str) -> Sidecar {
    Sidecar::read(&lib.inbox, &rel(dir)).unwrap().unwrap()
}

#[tokio::test]
async fn ok_to_ok_rewrites_title_artist_and_file_name() {
    let lib = lib!(PLUGIN);
    lib.video(U1, "v1", "KnownCh", "Song One (Official Video)", true);
    lib.start();
    lib.mode("a");
    stage(&lib, U1).await;
    let id = pending_item(&lib, ALBUM_DIR);

    lib.mode("b");
    let report = rejudge(&lib, id).await.unwrap();
    assert_eq!((report.total, report.rejudged), (1, 1), "{report:?}");
    assert!(report.failed.is_empty() && report.moved.is_empty());

    let name = "20260901 Song Two [v1].opus";
    assert_eq!(audio_names(&lib, ALBUM_DIR), vec![name.to_owned()]);
    let p = lib.inbox_path(&format!("{ALBUM_DIR}/{name}"));
    let (af, pictures) =
        read_audio_file_with_pictures(std::fs::File::open(&p).unwrap(), Some("opus")).unwrap();
    assert_eq!(af.tags.first("TITLE"), Some("Song Two"));
    assert_eq!(values(&af.tags, "ARTIST"), vec!["Artist A", "Guest"]);
    assert_eq!(af.tags.first("ORIGINALARTIST"), None);
    assert_eq!(
        af.tags.first("SOURCE_URL"),
        Some("https://www.youtube.com/watch?v=v1")
    );
    assert_eq!(pictures.len(), 1, "PICTURE は触らない");
    let s = sidecar(&lib, ALBUM_DIR);
    assert_eq!(s.files.len(), 1);
    let e = &s.files[name];
    assert_eq!(e.plugin_version.as_deref(), Some("v-b"));
    assert_eq!(e.verdict, "ok");
    assert_eq!(e.plugin_tags, Some(Vec::new()));
    assert!(e.rejudge_error.is_none());
    // 仕上げに inbox の走査を投入する
    assert_eq!(
        lib.count("SELECT count(*) FROM jobs WHERE type = 'inbox' AND state = 'queued'"),
        1
    );
}

#[tokio::test]
async fn unmatched_to_ok_moves_into_the_album_directory() {
    let lib = lib!(PLUGIN);
    lib.video(U1, "v1", "UnknownCh", "Song One (Official Video)", true);
    lib.start();
    stage(&lib, U1).await;
    let src = "youtube/_unmatched/UnknownCh";
    assert_eq!(
        audio_names(&lib, src),
        vec!["20260901 Song One (Official Video) [v1].opus"]
    );
    let id = pending_item(&lib, src);

    lib.mode("a");
    let report = rejudge(&lib, id).await.unwrap();
    assert_eq!(report.moved.len(), 1, "{report:?}");
    let name = "20260901 Song One [v1].opus";
    assert_eq!(
        report.moved[0],
        (
            "20260901 Song One (Official Video) [v1].opus".to_owned(),
            rel(&format!("{ALBUM_DIR}/{name}"))
        )
    );
    assert!(audio_names(&lib, src).is_empty());
    assert_eq!(audio_names(&lib, ALBUM_DIR), vec![name.to_owned()]);
    let t = tags_of(&lib.inbox_path(&format!("{ALBUM_DIR}/{name}")));
    assert_eq!(t.first("TITLE"), Some("Song One"));
    assert_eq!(t.first("ALBUMARTIST"), Some("Artist A"));
    assert_eq!(t.first("ORIGINALARTIST"), Some("Someone"));
    assert!(sidecar(&lib, src).files.is_empty());
    let s = sidecar(&lib, ALBUM_DIR);
    assert_eq!(s.category.as_deref(), Some("Pop"));
    assert_eq!(s.files[name].verdict, "ok");
    assert_eq!(
        s.files[name].plugin_tags,
        Some(vec!["ORIGINALARTIST".to_owned()])
    );
    // 語彙に足してある
    assert_eq!(
        lib.count("SELECT count(*) FROM categories WHERE name = 'Pop'"),
        1
    );
}

#[tokio::test]
async fn ok_to_unmatched_moves_back_and_clears_plugin_tags() {
    let lib = lib!(PLUGIN);
    lib.video(U1, "v1", "KnownCh", "Song One (Official Video)", true);
    lib.start();
    stage(&lib, U1).await;
    let id = pending_item(&lib, ALBUM_DIR);

    lib.mode("unmatched");
    let report = rejudge(&lib, id).await.unwrap();
    assert_eq!(report.moved.len(), 1, "{report:?}");
    let dst = "youtube/_unmatched/KnownCh";
    let name = "20260901 Song One (Official Video) [v1].opus";
    assert_eq!(audio_names(&lib, dst), vec![name.to_owned()]);
    assert!(audio_names(&lib, ALBUM_DIR).is_empty());
    let t = tags_of(&lib.inbox_path(&format!("{dst}/{name}")));
    assert_eq!(t.first("TITLE"), Some("Song One (Official Video)"));
    for k in ["ARTIST", "ALBUM", "ALBUMARTIST", "DATE", "ORIGINALARTIST"] {
        assert_eq!(t.first(k), None, "{k}");
    }
    assert!(t.first("SOURCE_URL").is_some());
    let e = &sidecar(&lib, dst).files[name];
    assert_eq!(e.verdict, "unmatched");
    assert_eq!(e.message.as_deref(), Some("ルールを足してください"));
    assert_eq!(e.plugin_tags, None);
}

#[tokio::test]
async fn skip_leaves_tags_untouched() {
    let lib = lib!(PLUGIN);
    lib.video(U1, "v1", "KnownCh", "Song One (Official Video)", true);
    lib.start();
    stage(&lib, U1).await;
    let id = pending_item(&lib, ALBUM_DIR);
    let name = "20260901 Song One [v1].opus";
    let before = tags_of(&lib.inbox_path(&format!("{ALBUM_DIR}/{name}")));

    lib.mode("skip");
    let report = rejudge(&lib, id).await.unwrap();
    assert_eq!((report.rejudged, report.moved.len()), (1, 0), "{report:?}");
    assert_eq!(audio_names(&lib, ALBUM_DIR), vec![name.to_owned()]);
    let after = tags_of(&lib.inbox_path(&format!("{ALBUM_DIR}/{name}")));
    assert_eq!(before.items(), after.items());
    let e = &sidecar(&lib, ALBUM_DIR).files[name];
    assert_eq!(e.verdict, "skip");
    assert_eq!(e.plugin_version.as_deref(), Some("v-s"));
    // 書いた追加タグはまだファイルにあるので、記録も残す（次の再判定で消せる）
    assert_eq!(e.plugin_tags, Some(vec!["ORIGINALARTIST".to_owned()]));
}

#[tokio::test]
async fn subscription_track_keeps_the_subscription_album() {
    let lib = lib!(PLUGIN);
    lib.video(U1, "v1", "KnownCh", "Song One (Official Video)", true);
    let sub = subscribe(&lib, "Sub Artist", "Sub Album", Some("Rock"), false).await;
    lib.start();
    let job = match lib
        .jobs
        .enqueue(spindle::jobs::handlers::ytdl::new_subscription_ytdl_job(
            U1, sub, 1,
        ))
        .await
        .unwrap()
    {
        spindle::jobs::EnqueueResult::Inserted(j) | spindle::jobs::EnqueueResult::Duplicate(j) => j,
    };
    assert_eq!(lib.wait(job).await, JobState::Done, "{:?}", lib.job(job));
    let dir = "youtube/Sub Artist/Sub Album";
    assert_eq!(audio_names(&lib, dir), vec!["20260901 Song One [v1].opus"]);
    let id = pending_item(&lib, dir);

    lib.mode("unmatched");
    let report = rejudge(&lib, id).await.unwrap();
    assert!(
        report.moved.is_empty() && report.failed.is_empty(),
        "{report:?}"
    );
    let name = "20260901 Song One (Official Video) [v1].opus";
    assert_eq!(audio_names(&lib, dir), vec![name.to_owned()]);
    let t = tags_of(&lib.inbox_path(&format!("{dir}/{name}")));
    assert_eq!(t.first("TITLE"), Some("Song One (Official Video)"));
    assert_eq!(t.first("ARTIST"), Some("Sub Artist"));
    assert_eq!(t.first("ALBUMARTIST"), Some("Sub Artist"));
    assert_eq!(t.first("ALBUM"), Some("Sub Album"));
    assert_eq!(t.first("DATE"), Some("2026-09-01"));
    assert_eq!(t.first("ORIGINALARTIST"), None);
    let s = sidecar(&lib, dir);
    assert_eq!(s.category.as_deref(), Some("Rock"));
    assert_eq!(s.files[name].verdict, "unmatched");
    assert_eq!(s.files[name].subscription_id, Some(sub));
}

#[tokio::test]
async fn plugin_fault_fails_that_track_only() {
    let lib = lib!(PLUGIN);
    lib.video(U1, "v1", "KnownCh", "Song One (Official Video)", true);
    lib.video(U2, "v2", "BrokenCh", "Song One (Live)", true);
    lib.start();
    lib.mode("a");
    stage(&lib, U1).await;
    stage(&lib, U2).await;
    let id = pending_item(&lib, ALBUM_DIR);
    let broken = "20260901 Song One [v2].opus";
    let before = tags_of(&lib.inbox_path(&format!("{ALBUM_DIR}/{broken}")));

    std::fs::write(lib.fake.join("broken"), "").unwrap();
    lib.mode("b");
    let report = rejudge(&lib, id).await.unwrap();
    assert_eq!((report.total, report.rejudged), (2, 1), "{report:?}");
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, broken);
    assert_eq!(
        audio_names(&lib, ALBUM_DIR),
        vec![broken.to_owned(), "20260901 Song Two [v1].opus".to_owned()]
    );
    let after = tags_of(&lib.inbox_path(&format!("{ALBUM_DIR}/{broken}")));
    assert_eq!(before.items(), after.items());
    let s = sidecar(&lib, ALBUM_DIR);
    assert!(s.files[broken].rejudge_error.is_some());
    assert_eq!(s.files[broken].plugin_version.as_deref(), Some("v-a"));
    assert_eq!(s.files["20260901 Song Two [v1].opus"].verdict, "ok");
}

#[tokio::test]
async fn missing_item_is_refetched_with_ytdlp() {
    let lib = lib!(PLUGIN);
    lib.video(U1, "v1", "KnownCh", "Song One (Official Video)", true);
    lib.start();
    stage(&lib, U1).await;
    let id = pending_item(&lib, ALBUM_DIR);
    let mut s = sidecar(&lib, ALBUM_DIR);
    for e in s.files.values_mut() {
        e.item = None;
    }
    s.write(&lib.inbox, &rel(ALBUM_DIR)).unwrap();
    std::fs::remove_file(lib.fake.join("calls.log")).unwrap();
    // 取り直しはサイドカーの url（正規形）で引く
    let url = "https://www.youtube.com/watch?v=v1";
    lib.video(url, "v1", "KnownCh", "Song One (Official Video)", true);

    lib.mode("b");
    let report = rejudge(&lib, id).await.unwrap();
    assert_eq!(report.rejudged, 1, "{report:?}");
    assert_eq!(
        lib.calls(),
        vec![format!(
            "--dump-single-json --flat-playlist --no-download --no-warnings -- {url}"
        )]
    );
    let e = &sidecar(&lib, ALBUM_DIR).files["20260901 Song Two [v1].opus"];
    let item = e.item.as_ref().unwrap();
    assert_eq!(item.title, "Song One (Official Video)");
    assert_eq!(item.uploaded_at.as_deref(), Some("2026-09-01"));
}

/// 出どころ（サイドカーの項の source）を持たないファイルは判定し直さない
#[tokio::test]
async fn files_without_source_are_left_alone() {
    let lib = lib!(PLUGIN);
    lib.video(U1, "v1", "KnownCh", "Song One (Official Video)", true);
    lib.start();
    stage(&lib, U1).await;
    let manual = common::make_audio(&lib.inbox_path(ALBUM_DIR), "manual.opus", "opus", 3).unwrap();
    set_tags(&manual, "opus", &[("TITLE", &["Hand Placed"])]);
    let before = std::fs::read(&manual).unwrap();
    let id = pending_item(&lib, ALBUM_DIR);

    lib.mode("b");
    let report = rejudge(&lib, id).await.unwrap();
    assert_eq!((report.total, report.rejudged), (1, 1), "{report:?}");
    assert_eq!(std::fs::read(&manual).unwrap(), before);
    assert_eq!(
        audio_names(&lib, ALBUM_DIR),
        vec![
            "20260901 Song Two [v1].opus".to_owned(),
            "manual.opus".to_owned()
        ]
    );
    assert!(!sidecar(&lib, ALBUM_DIR).files.contains_key("manual.opus"));
}

/// 移動先に同名で別の曲（SOURCE_URL が違う）があれば、その曲は失敗にして元の場所に残す
#[tokio::test]
async fn move_onto_a_different_file_fails_that_track_only() {
    let lib = lib!(PLUGIN);
    lib.video(U1, "v1", "UnknownCh", "Song One (Official Video)", true);
    lib.start();
    stage(&lib, U1).await;
    let src = "youtube/_unmatched/UnknownCh";
    let name = "20260901 Song One (Official Video) [v1].opus";
    let id = pending_item(&lib, src);
    std::fs::create_dir_all(lib.inbox_path(ALBUM_DIR)).unwrap();
    let other = lib.inbox_path(&format!("{ALBUM_DIR}/20260901 Song One [v1].opus"));
    std::fs::copy(lib.inbox_path(&format!("{src}/{name}")), &other).unwrap();
    set_tags(
        &other,
        "opus",
        &[("SOURCE_URL", &["https://example.com/other"])],
    );
    let before = tags_of(&lib.inbox_path(&format!("{src}/{name}")));

    lib.mode("a");
    let report = rejudge(&lib, id).await.unwrap();
    assert_eq!((report.rejudged, report.failed.len()), (0, 1), "{report:?}");
    assert!(report.moved.is_empty());
    assert_eq!(audio_names(&lib, src), vec![name.to_owned()]);
    let after = tags_of(&lib.inbox_path(&format!("{src}/{name}")));
    assert_eq!(before.items(), after.items());
    assert_eq!(
        tags_of(&other).first("SOURCE_URL"),
        Some("https://example.com/other")
    );
    let e = &sidecar(&lib, src).files[name];
    assert!(e.rejudge_error.is_some());
    assert_eq!(e.verdict, "unmatched");
    // tmp を残さない
    assert!(std::fs::read_dir(lib.inbox_path(ALBUM_DIR))
        .unwrap()
        .all(|e| !e.unwrap().file_name().to_string_lossy().starts_with('.')));
}

/// 移動先の件が pending でなければ移さず、今の件の中でタグとファイル名だけ直す
#[tokio::test]
async fn move_is_skipped_when_the_target_item_is_not_pending() {
    let lib = lib!(PLUGIN);
    lib.video(U1, "v1", "UnknownCh", "Song One (Official Video)", true);
    lib.start();
    stage(&lib, U1).await;
    let src = "youtube/_unmatched/UnknownCh";
    let id = pending_item(&lib, src);
    let target = pending_item(&lib, ALBUM_DIR);
    lib.conn()
        .execute(
            "UPDATE inbox_items SET state = 'approved' WHERE id = ?1",
            [target],
        )
        .unwrap();

    lib.mode("a");
    let report = rejudge(&lib, id).await.unwrap();
    assert_eq!(report.rejudged, 1, "{report:?}");
    assert!(report.moved.is_empty());
    assert_eq!(
        report.not_moved,
        vec![(
            "20260901 Song One (Official Video) [v1].opus".to_owned(),
            "approved".to_owned()
        )]
    );
    let name = "20260901 Song One [v1].opus";
    assert_eq!(audio_names(&lib, src), vec![name.to_owned()]);
    let t = tags_of(&lib.inbox_path(&format!("{src}/{name}")));
    assert_eq!(t.first("TITLE"), Some("Song One"));
    assert_eq!(t.first("ARTIST"), Some("Artist A"));
    assert_eq!(sidecar(&lib, src).files[name].verdict, "ok");
}

/// 移せなかった曲は、移動先の件が pending に戻れば次の再判定で移る（置き場所は実際のディレクトリで見る）
#[tokio::test]
async fn not_moved_track_moves_once_the_target_item_is_pending_again() {
    let lib = lib!(PLUGIN);
    lib.video(U1, "v1", "UnknownCh", "Song One (Official Video)", true);
    lib.start();
    stage(&lib, U1).await;
    let src = "youtube/_unmatched/UnknownCh";
    let id = pending_item(&lib, src);
    let target = pending_item(&lib, ALBUM_DIR);
    lib.conn()
        .execute(
            "UPDATE inbox_items SET state = 'approved' WHERE id = ?1",
            [target],
        )
        .unwrap();
    lib.mode("a");
    let report = rejudge(&lib, id).await.unwrap();
    assert_eq!(report.not_moved.len(), 1, "{report:?}");
    let name = "20260901 Song One [v1].opus";
    assert_eq!(sidecar(&lib, src).files[name].verdict, "ok");

    lib.conn()
        .execute(
            "UPDATE inbox_items SET state = 'pending' WHERE id = ?1",
            [target],
        )
        .unwrap();
    let report = rejudge(&lib, id).await.unwrap();
    assert_eq!(
        report.moved,
        vec![(name.to_owned(), rel(&format!("{ALBUM_DIR}/{name}")))],
        "{report:?}"
    );
    assert!(audio_names(&lib, src).is_empty());
    assert_eq!(audio_names(&lib, ALBUM_DIR), vec![name.to_owned()]);
    assert_eq!(sidecar(&lib, ALBUM_DIR).files[name].verdict, "ok");
}

/// 件のディレクトリにある曲は、前回の判定が skip でも、ok で別のアルバムが返ったときに移さない
#[tokio::test]
async fn track_in_an_album_dir_does_not_move_after_skip_then_ok() {
    let lib = lib!(PLUGIN);
    lib.video(U1, "v1", "KnownCh", "Song One (Official Video)", true);
    lib.start();
    stage(&lib, U1).await;
    let id = pending_item(&lib, ALBUM_DIR);
    let name = "20260901 Song One [v1].opus";
    lib.mode("skip");
    rejudge(&lib, id).await.unwrap();
    assert_eq!(sidecar(&lib, ALBUM_DIR).files[name].verdict, "skip");

    lib.mode("c");
    let report = rejudge(&lib, id).await.unwrap();
    assert!(
        report.moved.is_empty() && report.not_moved.is_empty() && report.failed.is_empty(),
        "{report:?}"
    );
    assert_eq!(audio_names(&lib, ALBUM_DIR), vec![name.to_owned()]);
    assert!(!lib.inbox_path("youtube/Artist A/Other Album").exists());
    let t = tags_of(&lib.inbox_path(&format!("{ALBUM_DIR}/{name}")));
    assert_eq!(t.first("ALBUM"), Some("Other Album"));
    let e = &sidecar(&lib, ALBUM_DIR).files[name];
    assert_eq!(
        (e.verdict.as_str(), e.plugin_version.as_deref()),
        ("ok", Some("v-c"))
    );
}

/// 新しい名前に置いた後（元のファイルは消えた）・サイドカーの前に落ちた状態から、もう一度で収束する
#[tokio::test]
async fn rerun_after_rename_before_sidecar_converges() {
    let lib = lib!(PLUGIN);
    lib.video(U1, "v1", "KnownCh", "Song One (Official Video)", true);
    lib.start();
    stage(&lib, U1).await;
    let id = pending_item(&lib, ALBUM_DIR);
    let old = "20260901 Song One [v1].opus";
    let new = "20260901 Song Two [v1].opus";
    std::fs::rename(
        lib.inbox_path(&format!("{ALBUM_DIR}/{old}")),
        lib.inbox_path(&format!("{ALBUM_DIR}/{new}")),
    )
    .unwrap();
    assert!(sidecar(&lib, ALBUM_DIR).files.contains_key(old));

    lib.mode("b");
    let report = rejudge(&lib, id).await.unwrap();
    assert_eq!((report.total, report.rejudged), (1, 1), "{report:?}");
    assert_eq!(audio_names(&lib, ALBUM_DIR), vec![new.to_owned()]);
    let s = sidecar(&lib, ALBUM_DIR);
    assert_eq!(s.files.keys().collect::<Vec<_>>(), vec![new]);
    assert_eq!(s.files[new].plugin_version.as_deref(), Some("v-b"));
    assert_eq!(
        tags_of(&lib.inbox_path(&format!("{ALBUM_DIR}/{new}"))).first("TITLE"),
        Some("Song Two")
    );
}

#[tokio::test]
async fn draft_is_cleared() {
    let lib = lib!(PLUGIN);
    lib.video(U1, "v1", "KnownCh", "Song One (Official Video)", true);
    lib.start();
    stage(&lib, U1).await;
    let id = pending_item(&lib, ALBUM_DIR);
    spindle::db::inbox::set_draft(&lib.conn(), id, &serde_json::json!({"album": "Typed"})).unwrap();

    lib.mode("b");
    rejudge(&lib, id).await.unwrap();
    assert_eq!(
        lib.count(&format!(
            "SELECT count(*) FROM inbox_items WHERE id = {id} AND draft IS NULL"
        )),
        1
    );
}

/// pending でない件は判定し直さない
#[tokio::test]
async fn item_that_is_not_pending_is_refused() {
    let lib = lib!(PLUGIN);
    lib.video(U1, "v1", "KnownCh", "Song One (Official Video)", true);
    lib.start();
    stage(&lib, U1).await;
    let id = pending_item(&lib, ALBUM_DIR);
    lib.conn()
        .execute(
            "UPDATE inbox_items SET state = 'approved' WHERE id = ?1",
            [id],
        )
        .unwrap();
    lib.mode("b");
    assert!(matches!(
        rejudge(&lib, id).await,
        Err(RejudgeError::NotPending(_))
    ));
    assert!(matches!(
        rejudge(&lib, 999).await,
        Err(RejudgeError::NotPending(_))
    ));
    assert_eq!(
        audio_names(&lib, ALBUM_DIR),
        vec!["20260901 Song One [v1].opus"]
    );
}

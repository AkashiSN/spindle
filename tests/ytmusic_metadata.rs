//! メタデータプラグインのプロトコル v1（SPEC §7.7、D-69）。偽のプラグイン（シェルスクリプト）で
//! 往復・判定なし・故障・タイムアウトを確かめる。実タイトルや固有名詞は使わない

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::time::Duration;

use spindle::db::open_memory_connection;
use spindle::import::ytmusic::{Item, MetadataProvider, Outcome, ProviderError, Track};
use tokio_util::sync::CancellationToken;

struct Fake {
    dir: tempfile::TempDir,
}

impl Fake {
    /// `script` は bash の本文。stdin は $IN に保存されてから実行される
    fn new(script: &str) -> Fake {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin.sh");
        std::fs::write(
            &path,
            format!(
                "#!/bin/bash\nIN=\"{}/stdin.json\"\ncat > \"$IN\"\n{script}\n",
                dir.path().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Fake { dir }
    }

    fn provider(&self, timeout_secs: u64) -> MetadataProvider {
        let cmd = vec![
            self.dir.path().join("plugin.sh").display().to_string(),
            "metadata".to_owned(),
        ];
        MetadataProvider::new(&cmd, Duration::from_secs(timeout_secs)).unwrap()
    }

    fn stdin(&self) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(self.dir.path().join("stdin.json")).unwrap())
            .unwrap()
    }
}

fn item() -> Item {
    Item {
        source: "youtube".into(),
        channel: "ch1".into(),
        channel_title: Some("Channel One".into()),
        id: Some("vid1".into()),
        url: None,
        title: "Song Title (Official Video)".into(),
        uploaded_at: Some("2024-05-06".into()),
        duration_ms: Some(180_000),
    }
}

const OK: &str = r#"echo '{"protocol":1,"ok":true,"track":{"title":"Song Title","artists":["Artist A","Artist B"],"albumartist":"Artist A","album":"Songs of A","category":"Pop","date":"2024","tags":[["originalartist","Someone"]],"future_field":1}}'"#;

#[tokio::test]
async fn round_trip_sends_request_and_reads_track() {
    let fake = Fake::new(OK);
    let out = fake
        .provider(10)
        .resolve(&item(), &CancellationToken::new())
        .await
        .unwrap();
    let Outcome::Track(t) = out else {
        panic!("{out:?}")
    };
    assert_eq!(t.title, "Song Title");
    assert_eq!(t.artists, ["Artist A", "Artist B"]);
    assert_eq!(t.category.as_deref(), Some("Pop"));
    // stdin の Request
    let req = fake.stdin();
    assert_eq!(req["protocol"], 1);
    assert_eq!(req["op"], "metadata");
    assert_eq!(req["item"]["channel"], "ch1");
    assert_eq!(req["item"]["title"], "Song Title (Official Video)");
    assert_eq!(req["item"]["uploaded_at"], "2024-05-06");
    assert!(req["item"]["url"].is_null());
    // タグと配置のフィールド
    let tags = t.tags(3);
    assert_eq!(
        tags,
        [
            ("TITLE", "Song Title"),
            ("ARTIST", "Artist A"),
            ("ARTIST", "Artist B"),
            ("ALBUM", "Songs of A"),
            ("ALBUMARTIST", "Artist A"),
            ("DATE", "2024"),
            ("TRACKNUMBER", "3"),
            ("ORIGINALARTIST", "Someone"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
    );
    let f = t.track_fields(3, "opus", "vid1");
    assert_eq!(f.category.as_deref(), Some("Pop"));
    assert_eq!(f.artist.as_deref(), Some("Artist A"));
    assert_eq!((f.disc_no, f.track_no), (Some(1), Some(3)));
    assert_eq!(f.year.as_deref(), Some("2024"));
    assert_eq!((f.ext.as_str(), f.stem.as_str()), ("opus", "vid1"));
}

#[tokio::test]
async fn declined_reasons_are_reported_not_errors() {
    for (reason, skip) in [
        ("unmatched", false),
        ("unknown_channel", false),
        ("skip", true),
    ] {
        let fake = Fake::new(&format!(
            r#"echo '{{"protocol":1,"ok":false,"reason":"{reason}","message":"why"}}'"#
        ));
        let out = fake
            .provider(10)
            .resolve(&item(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            out,
            Outcome::Declined {
                reason: reason.into(),
                message: "why".into()
            }
        );
        assert_eq!(out.is_skip(), skip);
    }
}

#[tokio::test]
async fn plugin_faults_are_errors() {
    // 非ゼロ終了
    let fake = Fake::new("echo boom >&2; exit 3");
    let e = fake
        .provider(10)
        .resolve(&item(), &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(e, ProviderError::Process(_)), "{e}");
    assert!(e.to_string().contains("boom"));
    // JSON でない
    let fake = Fake::new("echo not-json");
    assert!(matches!(
        fake.provider(10)
            .resolve(&item(), &CancellationToken::new())
            .await,
        Err(ProviderError::Json(_))
    ));
    // プロトコル違い
    let fake = Fake::new(r#"echo '{"protocol":2,"ok":true}'"#);
    assert!(matches!(
        fake.provider(10)
            .resolve(&item(), &CancellationToken::new())
            .await,
        Err(ProviderError::Protocol(2))
    ));
    // ok なのに track が無い / 必須が空 / date が不正 / category が空文字
    for body in [
        r#"{"protocol":1,"ok":true}"#,
        r#"{"protocol":1,"ok":true,"track":{"title":"","artists":["A"],"albumartist":"A","album":"B"}}"#,
        r#"{"protocol":1,"ok":true,"track":{"title":"T","artists":[],"albumartist":"A","album":"B"}}"#,
        r#"{"protocol":1,"ok":true,"track":{"title":"T","artists":["A"],"albumartist":"A","album":"B","date":"2024-13"}}"#,
        r#"{"protocol":1,"ok":true,"track":{"title":"T","artists":["A"],"albumartist":"A","album":"B","category":" "}}"#,
    ] {
        let fake = Fake::new(&format!("echo '{body}'"));
        let r = fake
            .provider(10)
            .resolve(&item(), &CancellationToken::new())
            .await;
        assert!(matches!(r, Err(ProviderError::Invalid(_))), "{body}: {r:?}");
    }
    // 起動できない
    let p = MetadataProvider::new(
        &[PathBuf::from("/nonexistent/plugin").display().to_string()],
        Duration::from_secs(1),
    )
    .unwrap();
    assert!(matches!(
        p.resolve(&item(), &CancellationToken::new()).await,
        Err(ProviderError::Process(_))
    ));
    // 空のコマンドは None
    assert!(MetadataProvider::new(&[], Duration::from_secs(1)).is_none());
}

#[tokio::test]
async fn timeout_kills_the_plugin() {
    let fake = Fake::new("sleep 30");
    let started = std::time::Instant::now();
    let e = fake
        .provider(1)
        .resolve(&item(), &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(e, ProviderError::Process(_)), "{e}");
    assert!(e.to_string().contains("終わらない"), "{e}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn category_is_added_to_the_vocabulary_when_missing() {
    let c = open_memory_connection().unwrap();
    let id = spindle::db::categories::ensure(&c, "Pop").unwrap();
    // 同じ canonical key は再利用（表記は最初のまま）
    assert_eq!(spindle::db::categories::ensure(&c, "pop").unwrap(), id);
    assert_eq!(spindle::db::categories::list(&c).unwrap().len(), 1);
    assert_ne!(spindle::db::categories::ensure(&c, "Rock").unwrap(), id);
    let _ = Track {
        title: String::new(),
        artists: vec![],
        albumartist: String::new(),
        album: String::new(),
        category: None,
        date: None,
        tags: vec![],
    };
}

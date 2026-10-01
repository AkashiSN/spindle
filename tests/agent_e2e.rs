//! エージェント（偽のミュージック.app・ファイルのトークン）と本物の spindle サーバの結合試験。
//! サーバは `127.0.0.1:0` で立て、エージェントは blocking HTTP で `spawn_blocking` の中から動かす
mod agent_support;

use std::net::SocketAddr;
use std::path::PathBuf;

use agent_support::*;
use spindle::db::devices as dbdev;
use spindle_agent::ctx::Ui;
use spindle_agent::failpoint::Failpoints;
use spindle_agent::music::fake::FakeMusic;
use spindle_agent::secrets::{FileSecrets, Secrets};
use spindle_agent::server::HttpServer;
use spindle_agent::sync::{pair_cmd, sync, Paths, SyncOutcome};

/// 確認に必ず y と答える UI
struct Yes;

impl Ui for Yes {
    fn info(&mut self, _: &str) {}
    fn confirm(&mut self, _: &agent_proto::ManifestResponse) -> bool {
        true
    }
}

async fn serve(app: &App) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app.router.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    format!("http://{addr}/")
}

/// pair して sync を 1 回走らせた結果
struct Synced {
    /// 偽のミュージック.app にある track の root 相対パス
    tracks: Vec<String>,
    /// プレイリスト名と曲数
    lists: Vec<(String, usize)>,
    outcome: SyncOutcome,
}

fn pair_and_sync(url: String, code: String, dir: PathBuf) -> Synced {
    let paths = Paths {
        state_dir: dir.join("state"),
        root: dir.join("Music/spindle"),
    };
    let fp = Failpoints::none();
    let music = FakeMusic::new(dir.join("Media"), fp.clone());
    let secrets = FileSecrets::new(&paths.state_dir);
    let mut ui = Yes;
    let anon = HttpServer::new(&url, true, None).unwrap();
    pair_cmd(
        &music, &anon, &secrets, &paths, &fp, &mut ui, &url, true, &code,
    )
    .unwrap();
    let server = HttpServer::new(&url, true, secrets.get().unwrap()).unwrap();
    let outcome = sync(&music, &server, &paths, &fp, &mut ui).unwrap();
    let mut tracks: Vec<String> = music
        .tracks()
        .into_iter()
        .filter_map(|t| spindle_agent::pathkey::to_rel(&paths.root, t.location.as_deref()?))
        .collect();
    tracks.sort();
    let lists = music
        .playlists()
        .into_iter()
        .filter(|p| !p.is_folder)
        .map(|p| (p.name, p.tracks.len()))
        .collect();
    Synced {
        tracks,
        lists,
        outcome,
    }
}

/// 曲 2 つとプレイリスト「Favs」を持つ iPhone を用意し、(端末 id, 曲, ワンタイムコード) を返す
async fn prepare(app: &App) -> (i64, Vec<SeededTrack>, String) {
    let dev = app.create_iphone("iPhone").await;
    let a = app.seed_track(1, "J-Pop/A/1-01 a.flac", b"aaa").await;
    let b = app.seed_track(2, "J-Pop/A/1-02 b.flac", b"bbb").await;
    app.seed_playlist(dev, 10, "Favs", &[2, 1]).await;
    let code = app.pair_code(dev).await;
    (dev, vec![a, b], code)
}

async fn run_agent(url: String, code: String) -> (Synced, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let synced = tokio::task::spawn_blocking(move || pair_and_sync(url, code, path))
        .await
        .unwrap();
    (synced, dir)
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_and_first_sync_reach_the_real_server() {
    let app = App::with_roots().await;
    let (dev, seeded, code) = prepare(&app).await;
    let url = serve(&app).await;
    let (synced, _dir) = run_agent(url, code).await;

    let mut want: Vec<String> = seeded.iter().map(|t| t.dest_path.clone()).collect();
    want.sort();
    assert_eq!(synced.tracks, want);
    assert_eq!(synced.lists, vec![("Favs".to_owned(), 2)]);
    assert!(
        matches!(synced.outcome, SyncOutcome::Applied { errors: 0, .. }),
        "{:?}",
        synced.outcome
    );

    // 報告が反映され、UI の差分は空になる
    let (st, v) = app
        .call(Method::GET, &format!("/api/devices/{dev}/diff"), None)
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["items"].as_array().map(Vec::len), Some(0), "{v}");
    let items = app.db.read(move |c| dbdev::items(c, dev)).await.unwrap();
    assert_eq!(items.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn changed_source_is_skipped_and_reported() {
    let app = App::with_roots().await;
    let (_dev, seeded, code) = prepare(&app).await;
    // 送る元が差し替わった（source_hashes の行は古いまま）
    app.replace_derived_bytes(2, b"changed").await;
    let url = serve(&app).await;
    let (synced, _dir) = run_agent(url, code).await;

    // 1 だけが届き、2 は 412 で次の差分に回る
    assert_eq!(synced.tracks, vec![seeded[0].dest_path.clone()]);
    assert!(
        matches!(synced.outcome, SyncOutcome::Applied { .. }),
        "{:?}",
        synced.outcome
    );
    let rows: i64 = app
        .db
        .read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM source_hashes WHERE track_id = 2",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[test]
fn canonical_key_matches_the_server() {
    let samples = [
        "ガギグゲゴ",                       // NFC
        "\u{30AB}\u{3099}\u{30AD}\u{3099}", // NFD の仮名
        "パ/ﾊﾟ",
        "Straße",
        "STRASSE",
        "MiXeD Case/Track.m4a",
        "İstanbul",
        "ıi",
        "Ǆ ǅ ǆ",
        "Ω Å ﬁ",
    ];
    for s in samples {
        assert_eq!(
            spindle_agent::pathkey::canonical_key(s),
            spindle::domain::relpath::canonical_key(s),
            "{s}"
        );
    }
}

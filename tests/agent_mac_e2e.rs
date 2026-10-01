//! 実機の Mac と通しで確かめる台（`#[ignore]`。CI では走らせない）。
//! 開発ホストで本物の spindle サーバを立て、Mac から `spindle-agent pair <url> <code> --insecure-http`
//! と `sync` を打って確かめる。
//!
//! ```sh
//! SPINDLE_E2E_ADDR=172.16.100.2:18080 SPINDLE_E2E_SHRINK_AFTER=300 \
//!   cargo test --test agent_mac_e2e -- --ignored --nocapture
//! ```
//!
//! - `SPINDLE_E2E_ADDR`: 待ち受けのアドレス（既定 `0.0.0.0:18080`）
//! - `SPINDLE_E2E_SECS`: 待ち受ける秒数（既定 900）。過ぎたら端末の状態を出して終える
//! - `SPINDLE_E2E_SHRINK_AFTER`: この秒数を過ぎたら選曲を `playlists` に変える（未設定なら変えない）。
//!   プレイリストの 2 曲だけが残り、次の sync で 1 曲が消える
mod agent_support;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use agent_support::*;
use spindle::db::devices as dbdev;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    match std::env::var(key) {
        Ok(v) => v.parse().unwrap_or_else(|_| panic!("{key} が不正: {v}")),
        Err(_) => default,
    }
}

/// 選曲を `playlists` に変える。sync の途中（開いた計画がある）なら 409 なので、通るまで待つ
async fn shrink_to_playlists(app: &App, dev: i64) {
    loop {
        let (st, v) = app
            .call(
                Method::PATCH,
                &format!("/api/devices/{dev}"),
                Some(json!({ "selection": "playlists" })),
            )
            .await;
        if st == StatusCode::OK {
            eprintln!("選曲を playlists に変えた（プレイリストの 2 曲だけが残る）");
            return;
        }
        assert_eq!(st, StatusCode::CONFLICT, "{v}");
        eprintln!("同期の途中なので選曲の変更を待つ: {v}");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "実機の Mac から接続して確かめる台。手で走らせる"]
async fn serve_for_mac() {
    let addr: String = env_or("SPINDLE_E2E_ADDR", "0.0.0.0:18080".to_owned());
    let secs: u64 = env_or("SPINDLE_E2E_SECS", 900);
    let shrink_after: Option<u64> = std::env::var("SPINDLE_E2E_SHRINK_AFTER").ok().map(|v| {
        v.parse()
            .unwrap_or_else(|_| panic!("SPINDLE_E2E_SHRINK_AFTER が不正: {v}"))
    });

    let app = App::with_roots().await;
    let dev = app.create_iphone("Mac 実機").await;
    app.seed_track(1, "J-Pop/Artist/1-01 first.flac", b"first-aac")
        .await;
    app.seed_track(2, "J-Pop/歌手/1-02 Don't 止まれ.flac", b"second-aac")
        .await;
    app.seed_track(3, "Rock/Band/1-03 third.flac", b"third-aac")
        .await;
    app.seed_playlist(dev, 10, "お気に入り", &[2, 1]).await;
    let code = app.pair_code(dev).await;

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    let local = listener.local_addr().unwrap();
    let router = app.router.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    eprintln!("待ち受け: http://{local}/");
    eprintln!("ペアリングコード: {code}");
    eprintln!("Mac で: spindle-agent pair http://{local} {code} --insecure-http");
    eprintln!("{secs} 秒後に終える");

    let start = Instant::now();
    let end = start + Duration::from_secs(secs);
    if let Some(after) = shrink_after.filter(|a| *a < secs) {
        tokio::time::sleep_until((start + Duration::from_secs(after)).into()).await;
        shrink_to_playlists(&app, dev).await;
    }
    tokio::time::sleep_until(end.into()).await;

    let items = app.db.read(move |c| dbdev::items(c, dev)).await.unwrap();
    let (st, v) = app
        .call(Method::GET, &format!("/api/devices/{dev}/diff"), None)
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let diff = v["items"].as_array().map(Vec::len).unwrap_or(0);
    eprintln!("device_items: {} 件", items.len());
    eprintln!("差分: {diff} 件");
}

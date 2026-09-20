//! MusicBrainz 照会（SPEC §7.2「メタデータ照会」、D-21、P2-3）。DiscID で引き、無ければ TOC で
//! fuzzy に引く。UA 必須・1 req/s。
//!
//! 応答の解釈は実サーバから保存したフィクスチャ（tests/fixtures/mb/）で確かめる:
//! - nevermind.json: `ws/2/discid/y6Br7t4P.bldLe_6Im2d9Z42IU4-`（2 リリース）
//! - cdextra.json: `ws/2/discid/BPnh1KU.hea1C.KMYWLGZkHJr0w-`（Enhanced CD、7 トラック）
//! - fuzzy.json: DiscID 不一致 + `?toc=`（Nevermind の TOC。7 リリース）
//! - fuzzy_none.json: 3 トラックの適当な TOC の fuzzy（1 リリース）
//! - notfound.json: 未登録 DiscID の 404

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::Router;
use tokio::sync::Mutex;

use spindle::cd::musicbrainz::{parse_lookup, MusicBrainzClient, ReleaseCandidate};
use spindle::cd::toc::{Toc, TocTrack};

const NEVERMIND: &str = include_str!("fixtures/mb/nevermind.json");
const CDEXTRA: &str = include_str!("fixtures/mb/cdextra.json");
const FUZZY: &str = include_str!("fixtures/mb/fuzzy.json");
const FUZZY_NONE: &str = include_str!("fixtures/mb/fuzzy_none.json");
const NOTFOUND: &str = include_str!("fixtures/mb/notfound.json");

const NEVERMIND_ID: &str = "y6Br7t4P.bldLe_6Im2d9Z42IU4-";

// ---------------------------------------------------------------- 解釈

#[test]
fn exact_lookup_yields_one_candidate_per_matching_medium() {
    let c: Vec<ReleaseCandidate> = parse_lookup(NEVERMIND, NEVERMIND_ID, 12).expect("解釈できる");
    assert_eq!(c.len(), 2);
    let first = &c[0];
    assert_eq!(first.release_id, "c12262e2-7185-4942-87ee-da27ddd45ddf");
    assert_eq!(
        first.release_group_id.as_deref(),
        Some("1b022e01-4da6-387b-8658-8678046e4cef")
    );
    assert_eq!(first.title, "Nevermind");
    assert_eq!(first.artist, "Nirvana");
    assert_eq!(first.date.as_deref(), Some("1991-09-24"));
    assert_eq!(first.country.as_deref(), Some("US"));
    assert_eq!(first.status.as_deref(), Some("Official"));
    assert_eq!(first.barcode.as_deref(), Some("720642442524"));
    assert_eq!(
        first.disambiguation.as_deref(),
        Some("First press error, no Endless Nameless")
    );
    assert_eq!(
        first.labels,
        vec![("DGC Records".to_owned(), Some("DGCD-24425".to_owned()))]
    );
    assert!(first.exact, "この medium は DiscID を持つ");
    assert_eq!(first.medium_position, 1);
    assert_eq!(first.medium_count, 1);
    assert_eq!(first.format.as_deref(), Some("CD"));
    assert_eq!(first.tracks.len(), 12);
    let t1 = &first.tracks[0];
    assert_eq!(t1.number, "1");
    assert_eq!(t1.position, 1);
    assert_eq!(t1.title, "Smells Like Teen Spirit");
    assert_eq!(t1.artist, "Nirvana");
    assert_eq!(t1.length_ms, Some(301_240));
    assert_eq!(t1.recording_id, "5fb524f1-8cc8-4c04-a921-e34c0a911ea7");
    assert_eq!(t1.track_id, "420ab068-930a-3d14-999e-885612d734c3");
    assert_eq!(t1.isrcs, vec!["USGF19942501".to_owned()]);
    let t12 = &first.tracks[11];
    assert_eq!(t12.title, "Something in the Way");
    assert_eq!(t12.length_ms, Some(232_866));

    // 2 つ目: 日付なし、ラベルが 2 つ
    let second = &c[1];
    assert_eq!(second.release_id, "28379cd4-8ded-4d98-847b-53acdb4dedc8");
    assert_eq!(second.date, None);
    assert_eq!(second.labels.len(), 2);
    assert!(second.exact);
}

#[test]
fn enhanced_cd_lookup_uses_the_audio_track_count() {
    let c = parse_lookup(CDEXTRA, "BPnh1KU.hea1C.KMYWLGZkHJr0w-", 7).expect("解釈できる");
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].title, "Back to Attraction");
    assert_eq!(c[0].tracks.len(), 7);
    assert!(c[0].exact);
}

/// DiscID が一致しない fuzzy 応答: トラック数の合う medium だけを候補にし、exact ではない
#[test]
fn fuzzy_lookup_keeps_media_with_the_same_track_count_as_inexact() {
    let c = parse_lookup(FUZZY, "AAAAAAAAAAAAAAAAAAAAAAAAAAAA-", 12).expect("解釈できる");
    assert_eq!(c.len(), 7);
    assert!(c.iter().all(|x| !x.exact && x.tracks.len() == 12));
    assert!(c.iter().any(|x| x.title == "交響曲「ダーク・ウィザード」"));
    // トラック数が違えば候補にならない
    let none = parse_lookup(FUZZY, "AAAAAAAAAAAAAAAAAAAAAAAAAAAA-", 11).expect("解釈できる");
    assert!(none.is_empty());
}

/// fuzzy 応答の中に自分の DiscID を持つ medium があれば、それは exact
#[test]
fn fuzzy_lookup_marks_media_carrying_our_discid_as_exact() {
    let c = parse_lookup(FUZZY, NEVERMIND_ID, 12).expect("解釈できる");
    let exact: Vec<&str> = c
        .iter()
        .filter(|x| x.exact)
        .map(|x| x.release_id.as_str())
        .collect();
    // 2 つの US 盤の medium が y6Br… を持つ（片方は 18 の DiscID を持つ）。MB の順のまま
    assert_eq!(
        exact,
        vec![
            "28379cd4-8ded-4d98-847b-53acdb4dedc8",
            "c12262e2-7185-4942-87ee-da27ddd45ddf"
        ]
    );
    // exact が先頭に来る
    assert!(c[0].exact && c[1].exact && !c[2].exact);
}

#[test]
fn fuzzy_lookup_of_a_small_toc() {
    let c = parse_lookup(FUZZY_NONE, "_hUhfEimFSRfNAAjQxow0OWMbZw-", 3).expect("解釈できる");
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].title, "French Versions");
    assert_eq!(c[0].artist, "Seb & The Rhââ Dicks");
}

#[test]
fn error_bodies_and_garbage_are_rejected() {
    assert!(parse_lookup(NOTFOUND, NEVERMIND_ID, 12).is_err());
    assert!(parse_lookup("not json", NEVERMIND_ID, 12).is_err());
    assert!(parse_lookup(r#"{"releases": "x"}"#, NEVERMIND_ID, 12).is_err());
}

/// artist-credit は name + joinphrase を順に繋ぐ
#[test]
fn artist_credit_joins_names_with_joinphrases() {
    let json = r#"{"releases":[{"id":"r1","title":"T","artist-credit":[
        {"name":"A","joinphrase":" feat. ","artist":{"id":"a1","name":"A"}},
        {"name":"B","joinphrase":" & ","artist":{"id":"a2","name":"B"}},
        {"name":"C","joinphrase":"","artist":{"id":"a3","name":"C"}}],
        "media":[{"position":1,"track-count":1,"discs":[{"id":"D"}],
          "tracks":[{"id":"t1","number":"1","position":1,"title":"S","length":1000,
                     "artist-credit":[{"name":"X","joinphrase":"","artist":{"id":"x","name":"X"}}],
                     "recording":{"id":"rec1","title":"S"}}]}]}]}"#;
    let c = parse_lookup(json, "D", 1).expect("解釈できる");
    assert_eq!(c[0].artist, "A feat. B & C");
    assert_eq!(c[0].tracks[0].artist, "X");
    assert!(c[0].exact);
    assert_eq!(c[0].tracks[0].isrcs, Vec::<String>::new());
}

/// トラックにアーティストが無ければリリースのものを使う
#[test]
fn track_artist_falls_back_to_the_release_artist() {
    let json = r#"{"releases":[{"id":"r1","title":"T","artist-credit":[
        {"name":"A","joinphrase":"","artist":{"id":"a1","name":"A"}}],
        "media":[{"position":2,"track-count":1,"tracks":[{"id":"t1","number":"1","position":1,"title":"S",
                     "recording":{"id":"rec1","title":"S"}}]},{"position":1,"track-count":5,"tracks":[]}]}]}"#;
    let c = parse_lookup(json, "D", 1).expect("解釈できる");
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].tracks[0].artist, "A");
    assert_eq!(c[0].medium_position, 2);
    assert_eq!(c[0].medium_count, 2);
    assert!(!c[0].exact);
}

// ---------------------------------------------------------------- HTTP

/// (DiscID, クエリ, User-Agent)
type SeenRequest = (String, Vec<(String, String)>, String);

#[derive(Default)]
struct Seen {
    requests: Vec<SeenRequest>,
    /// 503 を返す回数
    fail_503: usize,
    started: Vec<Instant>,
}

type Shared = Arc<Mutex<Seen>>;

async fn discid_handler(
    State(seen): State<Shared>,
    Path(discid): Path<String>,
    Query(q): Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> (StatusCode, String) {
    let mut s = seen.lock().await;
    s.started.push(Instant::now());
    let ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    s.requests.push((discid.clone(), q.clone(), ua));
    if s.fail_503 > 0 {
        s.fail_503 -= 1;
        return (StatusCode::SERVICE_UNAVAILABLE, String::new());
    }
    let has_toc = q.iter().any(|(k, _)| k == "toc");
    let no_stubs = q.iter().any(|(k, v)| k == "cdstubs" && v == "no");
    if discid == NEVERMIND_ID {
        (StatusCode::OK, NEVERMIND.to_owned())
    } else if !no_stubs && !has_toc {
        // cdstubs=no が無いと未登録 DiscID でも CD stub が 200 で返る（releases が無い別の形）
        (
            StatusCode::OK,
            format!(
                r#"{{"id":"{discid}","sectors":240000,"offsets":[150],"artist":"stub","title":"stub","track-count":1}}"#
            ),
        )
    } else if has_toc {
        (StatusCode::OK, FUZZY.to_owned())
    } else {
        (StatusCode::NOT_FOUND, NOTFOUND.to_owned())
    }
}

async fn serve() -> (String, Shared) {
    let seen: Shared = Arc::default();
    let app = Router::new()
        .route("/ws/2/discid/{discid}", get(discid_handler))
        .with_state(Arc::clone(&seen));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (format!("http://{addr}/ws/2/"), seen)
}

fn nevermind_toc() -> Toc {
    let starts = [
        0, 22593, 41700, 58133, 71920, 91198, 104468, 115188, 131988, 143758, 159678, 174415,
    ];
    let tracks = starts
        .iter()
        .enumerate()
        .map(|(i, &s)| TocTrack {
            number: i as u8 + 1,
            start_lba: s,
            is_audio: true,
        })
        .collect();
    Toc::new(tracks, 191880).expect("TOC")
}

fn other_toc() -> Toc {
    let tracks = [
        0u32, 20000, 40000, 60000, 80000, 100_000, 120_000, 140_000, 160_000, 180_000, 200_000,
        220_000,
    ]
    .iter()
    .enumerate()
    .map(|(i, &s)| TocTrack {
        number: i as u8 + 1,
        start_lba: s,
        is_audio: true,
    })
    .collect();
    Toc::new(tracks, 240_000).expect("TOC")
}

#[tokio::test]
async fn client_looks_up_by_discid_first_and_falls_back_to_toc() {
    let (base, seen) = serve().await;
    let client =
        MusicBrainzClient::new(base, "spindle-test/0.1", Duration::from_millis(0)).expect("client");
    let toc = nevermind_toc();
    let r = client.lookup_disc(&toc).await.expect("lookup");
    assert_eq!(r.discid, NEVERMIND_ID);
    assert!(r.exact);
    assert_eq!(r.candidates.len(), 2);
    assert!(r.candidates.iter().all(|c| c.exact));

    let other = other_toc();
    let r2 = client.lookup_disc(&other).await.expect("lookup");
    assert!(!r2.exact);
    assert_eq!(r2.candidates.len(), 7);
    assert!(r2.candidates.iter().all(|c| !c.exact));

    let s = seen.lock().await;
    assert_eq!(s.requests.len(), 3, "{:?}", s.requests);
    let (id, q, ua) = &s.requests[0];
    assert_eq!(id, NEVERMIND_ID);
    assert_eq!(ua, "spindle-test/0.1");
    let get = |q: &Vec<(String, String)>, k: &str| {
        q.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone())
    };
    assert_eq!(get(q, "fmt").as_deref(), Some("json"));
    let inc = get(q, "inc").expect("inc");
    for want in [
        "recordings",
        "artist-credits",
        "labels",
        "release-groups",
        "isrcs",
    ] {
        assert!(inc.contains(want), "inc に {want} が無い: {inc}");
    }
    assert_eq!(get(q, "toc"), None, "DiscID で引くときは toc を付けない");
    assert!(
        s.requests
            .iter()
            .all(|(_, q, _)| get(q, "cdstubs").as_deref() == Some("no")),
        "全要求に cdstubs=no（無いと stub の 200 が返って fuzzy に進めない）: {:?}",
        s.requests
    );
    // 2 本目は 404 → 3 本目は toc 付き（MB の TOC 形式: 先頭 末尾 リードアウト+150 各オフセット+150）
    assert_eq!(s.requests[1].0, other.musicbrainz_disc_id());
    assert_eq!(s.requests[2].0, other.musicbrainz_disc_id());
    // 線上は `toc=1+12+…`（空白の form エンコード）で、MB の例と同じ形。axum は空白に戻す
    assert_eq!(
        get(&s.requests[2].1, "toc").as_deref(),
        Some(other.musicbrainz_toc().as_str())
    );
}

#[tokio::test]
async fn client_retries_once_on_503_then_gives_up() {
    let (base, seen) = serve().await;
    let client =
        MusicBrainzClient::new(base, "spindle-test/0.1", Duration::from_millis(0)).expect("client");
    seen.lock().await.fail_503 = 1;
    let r = client
        .lookup_disc(&nevermind_toc())
        .await
        .expect("1 回の 503 は再試行で通る");
    assert_eq!(r.candidates.len(), 2);
    seen.lock().await.fail_503 = 5;
    assert!(client.lookup_disc(&nevermind_toc()).await.is_err());
    let s = seen.lock().await;
    assert_eq!(
        s.requests.len(),
        2 + 2,
        "1 回目 2 本、2 回目は 2 本で諦める"
    );
}

/// 1 req/s: 連続した照会は間隔を空ける。見るのは**サーバ側の受信間隔**（規約が数えるのはそれ）。
/// クライアントは応答が返ってから間隔を数えるので、1 本目の接続確立が遅い CI でも縮まない
#[tokio::test]
async fn client_spaces_requests_by_the_minimum_interval() {
    let (base, seen) = serve().await;
    let client = MusicBrainzClient::new(base, "spindle-test/0.1", Duration::from_millis(300))
        .expect("client");
    let t0 = Instant::now();
    for _ in 0..3 {
        client.lookup_disc(&nevermind_toc()).await.expect("lookup");
    }
    assert!(
        t0.elapsed() >= Duration::from_millis(600),
        "{:?}",
        t0.elapsed()
    );
    let s = seen.lock().await;
    for w in s.started.windows(2) {
        assert!(w[1].duration_since(w[0]) >= Duration::from_millis(290));
    }
}

#[tokio::test]
async fn unreachable_server_is_an_error() {
    let client = MusicBrainzClient::new(
        "http://127.0.0.1:9/ws/2/",
        "spindle-test/0.1",
        Duration::from_millis(0),
    )
    .expect("client");
    assert!(client.lookup_disc(&nevermind_toc()).await.is_err());
}

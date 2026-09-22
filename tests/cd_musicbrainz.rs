//! MusicBrainz 照会（SPEC §7.2「メタデータ照会」、D-21、P2-3）。DiscID で引き、無ければ TOC で
//! fuzzy に引く。UA 必須・1 req/s。
//!
//! 応答の解釈は実サーバから保存したフィクスチャ（tests/fixtures/mb/）で確かめる:
//! - nevermind.json: `ws/2/discid/y6Br7t4P.bldLe_6Im2d9Z42IU4-`（2 リリース）
//! - cdextra.json: `ws/2/discid/BPnh1KU.hea1C.KMYWLGZkHJr0w-`（Enhanced CD、7 トラック）
//! - fuzzy.json: DiscID 不一致 + `?toc=`（Nevermind の TOC。7 リリース）
//! - fuzzy_none.json: 3 トラックの適当な TOC の fuzzy（1 リリース）
//! - notfound.json: 未登録 DiscID の 404
//! - isrc_search_five.json: `ws/2/recording?query=isrc:JPQ402600330 OR isrc:JPQ402600340`（嵐「Five」。
//!   1 recording が 5 リリースに載る。340 のカラオケは未登録）
//! - barcode_search_five.json: `ws/2/release?query=barcode:4582515778491`（1 件）
//! - release_five.json: `ws/2/release/f1223d63-…`（CD 2 曲 + Blu-ray。DiscID 未登録、トラック長も無い =
//!   TOC の fuzzy では出ない盤）
//! - release_notfound.json: 不正な MBID の 400

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::Router;
use tokio::sync::Mutex;

use spindle::cd::musicbrainz::{
    merge_candidates, parse_lookup, parse_recording_search, parse_release, parse_release_ref,
    parse_release_search, DiscQuery, MatchedBy, MusicBrainzClient, ReleaseCandidate,
};
use spindle::cd::toc::{Toc, TocTrack};

const NEVERMIND: &str = include_str!("fixtures/mb/nevermind.json");
const CDEXTRA: &str = include_str!("fixtures/mb/cdextra.json");
const FUZZY: &str = include_str!("fixtures/mb/fuzzy.json");
const FUZZY_NONE: &str = include_str!("fixtures/mb/fuzzy_none.json");
const NOTFOUND: &str = include_str!("fixtures/mb/notfound.json");

const ISRC_SEARCH_FIVE: &str = include_str!("fixtures/mb/isrc_search_five.json");
const BARCODE_SEARCH_FIVE: &str = include_str!("fixtures/mb/barcode_search_five.json");
const RELEASE_FIVE: &str = include_str!("fixtures/mb/release_five.json");
const RELEASE_NOTFOUND: &str = include_str!("fixtures/mb/release_notfound.json");

const NEVERMIND_ID: &str = "y6Br7t4P.bldLe_6Im2d9Z42IU4-";
const FIVE_RELEASE: &str = "f1223d63-f359-457d-b935-fc27eb24a6de";
/// 嵐「Five」のシングル（実ドライブで読んだ TOC）
const FIVE_TOC: &str = "0:20144:40290";

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

// ---------------------------------------------------------------- DiscID 以外の経路（P2-3 拡張）

#[test]
fn recording_search_lists_releases_by_isrc_matches() {
    // 1 recording（JPQ402600330）が 5 リリースに載る。順は MB の並び（一致数は全部 1）
    let ids = parse_recording_search(ISRC_SEARCH_FIVE).unwrap();
    assert_eq!(ids.len(), 5, "{ids:?}");
    assert_eq!(ids[0], FIVE_RELEASE);
    assert!(ids.iter().all(|i| i.len() == 36));
}

#[test]
fn recording_search_ranks_releases_carrying_more_isrcs_first() {
    // 合成: recording A（1 件目）は X と Y に、B は Y だけに載る → Y（2 本一致）が先
    let json = r#"{"count":2,"recordings":[
      {"id":"a","title":"A","releases":[{"id":"x","title":"X"},{"id":"y","title":"Y"}]},
      {"id":"b","title":"B","releases":[{"id":"y","title":"Y"}]}]}"#;
    assert_eq!(parse_recording_search(json).unwrap(), vec!["y", "x"]);
}

#[test]
fn release_search_lists_release_ids() {
    assert_eq!(
        parse_release_search(BARCODE_SEARCH_FIVE).unwrap(),
        vec![FIVE_RELEASE.to_owned()]
    );
}

#[test]
fn release_fetch_keeps_media_with_the_audio_track_count() {
    // CD（2 曲）は候補、Blu-ray（1 トラック）は落ちる。DiscID は無いので exact でない
    let c = parse_release(RELEASE_FIVE, "Pmj4hPdkGckCxpSFFMoexmR6r1s-", 2).unwrap();
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0].release_id, FIVE_RELEASE);
    assert_eq!(c[0].title, "Five");
    assert_eq!(c[0].artist, "嵐");
    assert_eq!(c[0].barcode.as_deref(), Some("4582515778491"));
    assert_eq!(c[0].medium_position, 1);
    assert_eq!(c[0].medium_count, 2);
    assert_eq!(c[0].format.as_deref(), Some("CD"));
    assert!(!c[0].exact);
    assert_eq!(c[0].tracks[0].isrcs, vec!["JPQ402600330"]);
    assert_eq!(c[0].tracks[1].title, "Five (オリジナル・カラオケ)");
    // トラック数が合う medium が無ければ空
    assert!(parse_release(RELEASE_FIVE, "x", 12).unwrap().is_empty());
    assert!(parse_release(RELEASE_NOTFOUND, "x", 2).is_err());
}

#[test]
fn release_ref_accepts_urls_and_bare_ids() {
    let id = FIVE_RELEASE;
    for s in [
        id.to_owned(),
        format!("https://musicbrainz.org/release/{id}"),
        format!("https://musicbrainz.org/release/{id}/disc/1"),
        format!("  https://beta.musicbrainz.org/release/{id}?tab=details  "),
        format!("musicbrainz.org/release/{id}#x"),
        id.to_uppercase(),
    ] {
        assert_eq!(parse_release_ref(&s).as_deref(), Some(id), "{s}");
    }
    for s in [
        "",
        "https://musicbrainz.org/recording/f1223d63-f359-457d-b935-fc27eb24a6de",
        "f1223d63-f359-457d-b935",
        "https://example.com/release/f1223d63-f359-457d-b935-fc27eb24a6de",
    ] {
        assert_eq!(parse_release_ref(s), None, "{s}");
    }
}

fn cand(release: &str, medium: u32, exact: bool) -> ReleaseCandidate {
    let mut c = parse_release(RELEASE_FIVE, "x", 2).unwrap().remove(0);
    c.release_id = release.to_owned();
    c.medium_position = medium;
    c.exact = exact;
    c.matched_by = Vec::new();
    c
}

#[test]
fn merge_dedupes_media_and_orders_by_the_strongest_route() {
    // 同じリリース × medium が複数の経路で出たら 1 件に束ね、経路は強い順（discid > release >
    // isrc > barcode > toc）。並びも最強の経路の順、同じ経路の中は出てきた順
    let merged = merge_candidates(vec![
        (
            MatchedBy::Toc,
            vec![cand("t1", 1, false), cand("f", 1, false)],
        ),
        (
            MatchedBy::Isrc,
            vec![cand("f", 1, false), cand("i2", 1, false)],
        ),
        (
            MatchedBy::Barcode,
            vec![cand("f", 1, false), cand("f", 2, false)],
        ),
        (MatchedBy::Release, vec![cand("r", 1, false)]),
    ]);
    let keys: Vec<(String, u32)> = merged
        .iter()
        .map(|c| (c.release_id.clone(), c.medium_position))
        .collect();
    assert_eq!(
        keys,
        vec![
            ("r".to_owned(), 1),
            ("f".to_owned(), 1),
            ("i2".to_owned(), 1),
            ("f".to_owned(), 2),
            ("t1".to_owned(), 1),
        ]
    );
    assert_eq!(
        merged[1].matched_by,
        vec![MatchedBy::Isrc, MatchedBy::Barcode, MatchedBy::Toc]
    );
    assert_eq!(merged[0].matched_by, vec![MatchedBy::Release]);
}

#[test]
fn merge_puts_discid_matches_first_and_marks_them_exact() {
    let merged = merge_candidates(vec![
        (
            MatchedBy::Toc,
            vec![cand("a", 1, false), cand("b", 1, true)],
        ),
        (MatchedBy::Discid, vec![cand("c", 1, true)]),
    ]);
    let ids: Vec<&str> = merged.iter().map(|c| c.release_id.as_str()).collect();
    // b は fuzzy の応答に混ざった DiscID 持ち（D-64）。経路としては discid 扱いで前に出る
    assert_eq!(ids, vec!["c", "b", "a"]);
    assert!(merged[0].exact && merged[1].exact && !merged[2].exact);
    assert_eq!(
        merged[1].matched_by,
        vec![MatchedBy::Discid, MatchedBy::Toc]
    );
}

#[test]
fn matched_by_serializes_snake_case() {
    assert_eq!(
        serde_json::to_string(&MatchedBy::Discid).unwrap(),
        "\"discid\""
    );
    assert_eq!(
        serde_json::to_string(&MatchedBy::Barcode).unwrap(),
        "\"barcode\""
    );
}

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

/// `ws/2/recording?query=isrc:…`（検索）。Five の ISRC なら実応答、他は 0 件
async fn recording_search_handler(
    State(seen): State<Shared>,
    Query(q): Query<Vec<(String, String)>>,
) -> (StatusCode, String) {
    let mut s = seen.lock().await;
    s.started.push(Instant::now());
    s.requests
        .push(("recording".to_owned(), q.clone(), String::new()));
    let query = q
        .iter()
        .find(|(k, _)| k == "query")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    if query.contains("isrc:JPQ402600330") {
        (StatusCode::OK, ISRC_SEARCH_FIVE.to_owned())
    } else {
        (
            StatusCode::OK,
            r#"{"count":0,"offset":0,"recordings":[]}"#.to_owned(),
        )
    }
}

/// `ws/2/release?query=barcode:…`（検索）
async fn release_search_handler(
    State(seen): State<Shared>,
    Query(q): Query<Vec<(String, String)>>,
) -> (StatusCode, String) {
    let mut s = seen.lock().await;
    s.started.push(Instant::now());
    s.requests
        .push(("release".to_owned(), q.clone(), String::new()));
    let query = q
        .iter()
        .find(|(k, _)| k == "query")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    if query == "barcode:4582515778491" {
        (StatusCode::OK, BARCODE_SEARCH_FIVE.to_owned())
    } else {
        (
            StatusCode::OK,
            r#"{"count":0,"offset":0,"releases":[]}"#.to_owned(),
        )
    }
}

/// `ws/2/release/{id}`（取得）。Five は実応答、`df1b88a4…`（ISRC 検索の 2 件目）は Five と同じ中身で
/// id だけ違う盤、他は 404
async fn release_handler(
    State(seen): State<Shared>,
    Path(id): Path<String>,
    Query(q): Query<Vec<(String, String)>>,
) -> (StatusCode, String) {
    let mut s = seen.lock().await;
    s.started.push(Instant::now());
    s.requests
        .push((format!("release/{id}"), q.clone(), String::new()));
    if id == FIVE_RELEASE {
        (StatusCode::OK, RELEASE_FIVE.to_owned())
    } else if id.starts_with("df1b88a4") {
        (StatusCode::OK, RELEASE_FIVE.replace(FIVE_RELEASE, &id))
    } else {
        (
            StatusCode::NOT_FOUND,
            r#"{"error":"Not Found","help":"For usage, please see: https://musicbrainz.org/development/mmd"}"#.to_owned(),
        )
    }
}

async fn serve() -> (String, Shared) {
    let seen: Shared = Arc::default();
    let app = Router::new()
        .route("/ws/2/discid/{discid}", get(discid_handler))
        .route("/ws/2/recording", get(recording_search_handler))
        .route("/ws/2/release", get(release_search_handler))
        .route("/ws/2/release/{id}", get(release_handler))
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

// ---------------------------------------------------------------- クライアント（複数経路）

fn five_toc() -> Toc {
    Toc::parse(FIVE_TOC).unwrap()
}

#[tokio::test]
async fn client_combines_isrc_barcode_and_release_routes() {
    let (base, seen) = serve().await;
    let client =
        MusicBrainzClient::new(base, "spindle-test/0.1", Duration::from_millis(0)).expect("client");
    let toc = five_toc();
    let isrcs = ["JPQ402600330".to_owned(), "JPQ402600340".to_owned()];
    let release = format!("https://musicbrainz.org/release/{FIVE_RELEASE}");
    let r = client
        .lookup(&DiscQuery {
            toc: &toc,
            isrcs: &isrcs,
            mcn: Some("4582515778491"),
            release: Some(&release),
        })
        .await
        .expect("lookup");
    assert!(!r.exact);
    assert!(r.notes.is_empty(), "{:?}", r.notes);
    // Five の CD（medium 1）は 3 経路で出て 1 件に束なる。df1b88a4（同じ中身の別盤）は ISRC 経路だけ
    let five = r
        .candidates
        .iter()
        .find(|c| c.release_id == FIVE_RELEASE)
        .expect("Five");
    assert_eq!(five.medium_position, 1);
    assert_eq!(
        five.matched_by,
        vec![MatchedBy::Release, MatchedBy::Isrc, MatchedBy::Barcode]
    );
    assert_eq!(
        r.candidates[0].release_id, FIVE_RELEASE,
        "指定したリリースが先頭"
    );
    let other = r
        .candidates
        .iter()
        .find(|c| c.release_id.starts_with("df1b88a4"))
        .expect("df1b88a4");
    assert_eq!(other.matched_by, vec![MatchedBy::Isrc]);
    // fuzzy（Nevermind の 12 曲）はトラック数が合わないので候補に入らない
    assert_eq!(r.candidates.len(), 2, "{:?}", r.candidates);

    let s = seen.lock().await;
    let paths: Vec<&str> = s.requests.iter().map(|(p, _, _)| p.as_str()).collect();
    // discid → toc の fuzzy → 指定リリース → ISRC 検索 → その上位（Five は取得済みなので省く）→
    // バーコード検索（Five は取得済み）
    assert_eq!(paths[0], toc.musicbrainz_disc_id());
    assert_eq!(paths[1], toc.musicbrainz_disc_id());
    assert_eq!(paths[2], format!("release/{FIVE_RELEASE}"));
    assert_eq!(paths[3], "recording");
    let get = |q: &Vec<(String, String)>, k: &str| {
        q.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone())
    };
    assert_eq!(
        get(&s.requests[3].1, "query").as_deref(),
        Some("isrc:JPQ402600330 OR isrc:JPQ402600340")
    );
    let fetched: Vec<&str> = paths
        .iter()
        .filter(|p| p.starts_with("release/"))
        .copied()
        .collect();
    assert_eq!(
        fetched
            .iter()
            .filter(|p| **p == format!("release/{FIVE_RELEASE}"))
            .count(),
        1,
        "同じリリースは 1 回しか取らない: {paths:?}"
    );
    // ISRC 検索の 5 件のうち Five 以外の 4 件を取り（404 も含む）、バーコード検索の 1 件は取得済み
    assert_eq!(fetched.len(), 5, "{paths:?}");
    assert_eq!(paths.iter().filter(|p| **p == "release").count(), 1);
    assert_eq!(
        get(&s.requests[paths.len() - 1].1, "query").as_deref(),
        Some("barcode:4582515778491")
    );
    // 取得はどれも discid 照会と同じ inc
    for (p, q, _) in &s.requests {
        if p.starts_with("release/") {
            assert!(
                get(q, "inc").unwrap_or_default().contains("recordings"),
                "{p}"
            );
        }
    }
}

#[tokio::test]
async fn client_notes_a_release_without_a_matching_medium() {
    let (base, _seen) = serve().await;
    let client =
        MusicBrainzClient::new(base, "spindle-test/0.1", Duration::from_millis(0)).expect("client");
    // 12 曲の TOC に Five（2 曲）を指定 → 候補は空で、理由を notes に
    let toc = nevermind_toc();
    let r = client
        .lookup(&DiscQuery {
            toc: &toc,
            isrcs: &[],
            mcn: None,
            release: Some(FIVE_RELEASE),
        })
        .await
        .expect("lookup");
    assert!(r.candidates.iter().all(|c| c.release_id != FIVE_RELEASE));
    assert_eq!(r.notes.len(), 1, "{:?}", r.notes);
    assert!(r.notes[0].contains("12"), "{:?}", r.notes);
    // 不正な指定・無いリリースも notes（照会全体は失敗にしない）
    let r = client
        .lookup(&DiscQuery {
            toc: &toc,
            isrcs: &[],
            mcn: None,
            release: Some("not-an-id"),
        })
        .await
        .expect("lookup");
    assert_eq!(r.notes.len(), 1, "{:?}", r.notes);
    let r = client
        .lookup(&DiscQuery {
            toc: &toc,
            isrcs: &[],
            mcn: None,
            release: Some("00000000-0000-0000-0000-000000000000"),
        })
        .await
        .expect("lookup");
    assert_eq!(r.notes.len(), 1, "{:?}", r.notes);
    assert!(r.notes[0].contains("404"), "{:?}", r.notes);
}

#[tokio::test]
async fn client_skips_routes_without_inputs_and_after_an_exact_hit() {
    let (base, seen) = serve().await;
    let client =
        MusicBrainzClient::new(base, "spindle-test/0.1", Duration::from_millis(0)).expect("client");
    // 入力が TOC だけなら discid → toc の 2 本
    let toc = other_toc();
    let r = client
        .lookup(&DiscQuery {
            toc: &toc,
            isrcs: &[],
            mcn: None,
            release: None,
        })
        .await
        .expect("lookup");
    assert_eq!(r.candidates.len(), 7);
    assert!(r
        .candidates
        .iter()
        .all(|c| c.matched_by == vec![MatchedBy::Toc]));
    assert_eq!(seen.lock().await.requests.len(), 2);
    // DiscID で当たれば ISRC / バーコードは引かない（指定リリースだけは足す）
    let toc = nevermind_toc();
    let isrcs = ["JPQ402600330".to_owned()];
    let r = client
        .lookup(&DiscQuery {
            toc: &toc,
            isrcs: &isrcs,
            mcn: Some("4582515778491"),
            release: None,
        })
        .await
        .expect("lookup");
    assert!(r.exact);
    assert_eq!(r.candidates.len(), 2);
    assert!(r
        .candidates
        .iter()
        .all(|c| c.exact && c.matched_by == vec![MatchedBy::Discid]));
    assert_eq!(seen.lock().await.requests.len(), 3);
}

// ---------------------------------------------------------------- 接続の失敗（診断と再試行）

/// `error_chain` はエラーの原因を末端まで繋ぐ（reqwest の Display は「error sending request」までで、
/// TLS の失敗かタイムアウトか接続断かが消える）
#[test]
fn error_chain_joins_the_sources() {
    #[derive(Debug, thiserror::Error)]
    #[error("上")]
    struct Outer(#[source] Inner);
    #[derive(Debug, thiserror::Error)]
    #[error("中: {0}")]
    struct Inner(String);
    assert_eq!(
        spindle::cd::error_chain(&Outer(Inner("下".into()))),
        "上: 中: 下"
    );
    // 同じ文言が続くときは繰り返さない
    assert_eq!(spindle::cd::error_chain(&Inner("x".into())), "中: x");
}

/// 接続が落ちる（TLS の失敗・idle なコネクションの再利用・一過性の切断）ときは 1 度だけ張り直す。
/// 最初の接続を受理してすぐ閉じ、2 本目から応答する生の HTTP サーバで模す
#[tokio::test]
async fn client_retries_once_when_the_connection_drops() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                // 1 本目: 要求を読まずに閉じる（相手からは「送ったのに切られた」に見える）
                drop(stream);
                continue;
            }
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let body = NEVERMIND;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    let client = MusicBrainzClient::new(
        format!("http://{addr}/ws/2/"),
        "spindle-test/0.1",
        Duration::from_millis(0),
    )
    .expect("client");
    let r = client
        .lookup_disc(&nevermind_toc())
        .await
        .expect("接続断は張り直して通る");
    assert_eq!(r.discid, NEVERMIND_ID);
    assert!(r.exact);
    assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 2);
}

// ---------------------------------------------------------------- アドレスファミリの選択

/// `[musicbrainz].address_family` は解決したアドレスの並べ替え / 絞り込みに落ちる（D-64 追記 2）。
/// IPv4 が塞がれている環境（MetaBrainz のエッジが我々の v4 を落とす）で v6 を選べるようにする
#[test]
fn address_family_orders_resolved_addresses() {
    use spindle::cd::select_addrs;
    use spindle::config::AddressFamily;
    use std::net::SocketAddr;

    let v4: SocketAddr = "142.132.241.153:443".parse().unwrap();
    let v6: SocketAddr = "[2a01:4f8:c011:f68::1]:443".parse().unwrap();
    let all = vec![v4, v6];
    // auto は解決順のまま
    assert_eq!(select_addrs(all.clone(), AddressFamily::Auto), vec![v4, v6]);
    // ipv6 / ipv4 はその族だけにする
    assert_eq!(select_addrs(all.clone(), AddressFamily::V6), vec![v6]);
    assert_eq!(select_addrs(all.clone(), AddressFamily::V4), vec![v4]);
    // 指定した族が 1 つも無ければ空（黙って塞がっている族へ倒すと、原因の分からない TLS エラーになる。
    // 呼び側はここで「IPv6 のアドレスが無い」と分かる失敗にする）
    assert!(select_addrs(vec![v4], AddressFamily::V6).is_empty());
    assert!(select_addrs(vec![v6], AddressFamily::V4).is_empty());
    assert!(select_addrs(vec![], AddressFamily::V6).is_empty());
}

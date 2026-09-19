//! AccurateRip / CTDB の照会クライアント（SPEC §7.3、D-13、P2-9。照会部分は P2-7 と共通）。
//!
//! 応答の解釈は実サーバから保存したフィクスチャで確かめる:
//! - `tests/fixtures/cd/dBAR-012-0013f127-00b61059-a109fe0c.bin`（Nevermind。15 エントリ）
//! - `tests/fixtures/cd/ctdb_hybrid_theory.xml`（Hybrid Theory JP。41 エントリ）
//!
//! HTTP はローカルの axum サーバに向けて、パス・クエリ・UA・404 の扱いを確かめる

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::Router;
use tokio::sync::Mutex;

use spindle::cd::accuraterip::{parse_response as parse_ar, AccurateRipClient, ArDiscEntry};
use spindle::cd::ctdb::{parse_response as parse_ctdb, CtdbClient, CtdbEntry};
use spindle::cd::toc::{AccurateRipId, Toc, TocTrack};

const AR_BIN: &[u8] = include_bytes!("fixtures/cd/dBAR-012-0013f127-00b61059-a109fe0c.bin");
const CTDB_XML: &str = include_str!("fixtures/cd/ctdb_hybrid_theory.xml");

// ---------------------------------------------------------------- AccurateRip の応答

#[test]
fn accuraterip_response_is_a_sequence_of_disc_entries() {
    let entries: Vec<ArDiscEntry> = parse_ar(AR_BIN).expect("解釈できる");
    assert_eq!(entries.len(), 15);
    let e = &entries[0];
    assert_eq!(e.id.audio_tracks, 12);
    assert_eq!(e.id.to_string(), "0013f127-00b61059-a109fe0c");
    assert_eq!(e.tracks.len(), 12);
    // 実サーバの値（フィクスチャ取得時に確認）
    assert_eq!(e.tracks[0].confidence, 182);
    assert_eq!(e.tracks[0].crc, 0x9d3c46b4);
    assert_eq!(e.tracks[0].crc450, 0xf2e5814e);
    assert_eq!(e.tracks[1].confidence, 181);
    assert_eq!(e.tracks[1].crc, 0x92d4e44b);
    assert_eq!(entries[1].tracks[0].crc, 0xcb951fe2);
    assert_eq!(entries[2].tracks[0].confidence, 69);
}

#[test]
fn accuraterip_response_rejects_truncated_data() {
    assert!(parse_ar(&AR_BIN[..AR_BIN.len() - 1]).is_err());
    assert!(parse_ar(&AR_BIN[..5]).is_err());
    assert_eq!(parse_ar(&[]).expect("空は 0 件").len(), 0);
}

#[test]
fn accuraterip_db_path_uses_the_low_nibbles_of_id1() {
    let id = AccurateRipId {
        id1: 0x0013f127,
        id2: 0x00b61059,
        cddb: 0xa109fe0c,
        audio_tracks: 12,
    };
    assert_eq!(
        spindle::cd::accuraterip::db_path(&id),
        "7/2/1/dBAR-012-0013f127-00b61059-a109fe0c.bin"
    );
}

// ---------------------------------------------------------------- CTDB の応答

#[test]
fn ctdb_response_lists_entries_with_track_crcs() {
    let entries: Vec<CtdbEntry> = parse_ctdb(CTDB_XML).expect("解釈できる");
    assert_eq!(entries.len(), 41);
    let e = &entries[0];
    assert_eq!(e.id, 70967);
    assert_eq!(e.confidence, 549);
    assert_eq!(e.crc32, 0xb95af55c);
    assert_eq!(e.npar, 16);
    assert_eq!(e.stride, 5880);
    assert_eq!(
        e.toc,
        "0:13915:25592:40835:55855:71530:85325:99560:115782:129627:144212:156000:170495:190165:-218477:241060"
    );
    assert_eq!(e.track_crcs.len(), 14);
    assert_eq!(e.track_crcs[0], 0xd902ba25);
    assert_eq!(e.track_crcs[13], 0x7c892678);
    assert_eq!(e.has_parity.as_deref(), Some("http://p.cuetools.net/70967"));
    assert!(e.syndrome.is_some());
    // parity 属性だけのエントリもある
    assert!(entries
        .iter()
        .any(|e| e.parity.is_some() && e.syndrome.is_none()));
}

#[test]
fn ctdb_response_without_entries_is_empty() {
    let xml = r#"<ctdb xmlns="http://db.cuetools.net/ns/mmd-1.0#"><metadata /></ctdb>"#;
    assert_eq!(parse_ctdb(xml).expect("空").len(), 0);
    assert!(parse_ctdb("<ctdb><entry confidence=\"x\" /></ctdb>").is_err());
    assert!(parse_ctdb("not xml at all <<<").is_err());
}

// ---------------------------------------------------------------- HTTP

#[derive(Default)]
struct Seen {
    ar_paths: Vec<String>,
    ctdb_queries: Vec<Vec<(String, String)>>,
    user_agents: Vec<String>,
}

type Shared = Arc<Mutex<Seen>>;

async fn ar_handler(
    State(seen): State<Shared>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> (StatusCode, Vec<u8>) {
    let mut s = seen.lock().await;
    s.ar_paths.push(path.clone());
    s.user_agents.push(
        headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned(),
    );
    if path.ends_with("dBAR-012-0013f127-00b61059-a109fe0c.bin") {
        (StatusCode::OK, AR_BIN.to_vec())
    } else {
        (StatusCode::NOT_FOUND, Vec::new())
    }
}

async fn ctdb_handler(
    State(seen): State<Shared>,
    Query(q): Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> (StatusCode, String) {
    let mut s = seen.lock().await;
    s.user_agents.push(
        headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned(),
    );
    let toc = q
        .iter()
        .find(|(k, _)| k == "toc")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    s.ctdb_queries.push(q);
    if toc.starts_with("0:13915:") {
        (StatusCode::OK, CTDB_XML.to_owned())
    } else {
        (
            StatusCode::OK,
            r#"<ctdb xmlns="http://db.cuetools.net/ns/mmd-1.0#"><metadata /></ctdb>"#.to_owned(),
        )
    }
}

async fn serve() -> (String, Shared) {
    let seen: Shared = Arc::default();
    let app = Router::new()
        .route("/accuraterip/{*path}", get(ar_handler))
        .route("/lookup2.php", get(ctdb_handler))
        .with_state(Arc::clone(&seen));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (format!("http://{addr}"), seen)
}

fn nevermind_id() -> AccurateRipId {
    AccurateRipId {
        id1: 0x0013f127,
        id2: 0x00b61059,
        cddb: 0xa109fe0c,
        audio_tracks: 12,
    }
}

fn hybrid_theory() -> Toc {
    let starts = [
        0, 13915, 25592, 40835, 55855, 71530, 85325, 99560, 115782, 129627, 144212, 156000, 170495,
        190165,
    ];
    let mut tracks: Vec<TocTrack> = starts
        .iter()
        .enumerate()
        .map(|(i, &s)| TocTrack {
            number: i as u8 + 1,
            start_lba: s,
            is_audio: true,
        })
        .collect();
    tracks.push(TocTrack {
        number: 15,
        start_lba: 218477,
        is_audio: false,
    });
    Toc::new(tracks, 241060).expect("TOC")
}

#[tokio::test]
async fn accuraterip_client_fetches_the_bin_by_id_and_treats_404_as_no_entries() {
    let (base, seen) = serve().await;
    let client =
        AccurateRipClient::new(format!("{base}/accuraterip/"), "spindle-test/0.1").expect("client");
    let entries = client.lookup(&nevermind_id()).await.expect("lookup");
    assert_eq!(entries.len(), 15);
    let none = client
        .lookup(&AccurateRipId {
            id1: 1,
            id2: 2,
            cddb: 3,
            audio_tracks: 4,
        })
        .await
        .expect("404 は 0 件");
    assert!(none.is_empty());
    let s = seen.lock().await;
    assert_eq!(
        s.ar_paths,
        vec![
            "7/2/1/dBAR-012-0013f127-00b61059-a109fe0c.bin".to_owned(),
            "1/0/0/dBAR-004-00000001-00000002-00000003.bin".to_owned()
        ]
    );
    assert!(s.user_agents.iter().all(|ua| ua == "spindle-test/0.1"));
}

#[tokio::test]
async fn ctdb_client_queries_lookup2_with_the_toc_string() {
    let (base, seen) = serve().await;
    let client =
        CtdbClient::new(format!("{base}/lookup2.php"), "spindle-test/0.1").expect("client");
    let entries = client.lookup(&hybrid_theory()).await.expect("lookup");
    assert_eq!(entries.len(), 41);
    let s = seen.lock().await;
    let q = &s.ctdb_queries[0];
    let get = |k: &str| q.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.as_str());
    assert_eq!(get("version"), Some("3"));
    assert_eq!(get("ctdb"), Some("1"));
    assert_eq!(get("fuzzy"), Some("1"));
    assert_eq!(get("toc"), Some(hybrid_theory().ctdb_toc().as_str()));
    assert!(s.user_agents.iter().all(|ua| ua == "spindle-test/0.1"));
}

#[tokio::test]
async fn lookup_errors_are_reported_not_swallowed() {
    // 誰も聞いていないポート
    let client = AccurateRipClient::new("http://127.0.0.1:9/accuraterip/", "spindle-test/0.1")
        .expect("client");
    assert!(client.lookup(&nevermind_id()).await.is_err());
    let client =
        CtdbClient::new("http://127.0.0.1:9/lookup2.php", "spindle-test/0.1").expect("client");
    assert!(client.lookup(&hybrid_theory()).await.is_err());
}

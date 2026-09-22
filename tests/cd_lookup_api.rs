//! `POST /api/cd/lookup { toc }`（SPEC §9、P2-3）。TOC 文字列（CTDB 形式か MusicBrainz 形式）
//! から DiscID を出して MusicBrainz に照会し、候補を返す。MB はローカルの axum で模す

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Query};
use axum::http::{header, Method, Request, StatusCode};
use axum::routing::get;
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::cd::musicbrainz::MusicBrainzClient;
use spindle::config::Config;
use spindle::db::Db;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";
const NEVERMIND: &str = include_str!("fixtures/mb/nevermind.json");
const NOTFOUND: &str = include_str!("fixtures/mb/notfound.json");
const RELEASE_FIVE: &str = include_str!("fixtures/mb/release_five.json");
const ISRC_SEARCH_FIVE: &str = include_str!("fixtures/mb/isrc_search_five.json");
const FIVE_RELEASE: &str = "f1223d63-f359-457d-b935-fc27eb24a6de";
const NEVERMIND_ID: &str = "y6Br7t4P.bldLe_6Im2d9Z42IU4-";
const NEVERMIND_TOC: &str =
    "0:22593:41700:58133:71920:91198:104468:115188:131988:143758:159678:174415:191880";

async fn mb_handler(
    Path(discid): Path<String>,
    Query(q): Query<Vec<(String, String)>>,
) -> (StatusCode, String) {
    // 1 トラックの TOC の fuzzy 照会は常に 503（負荷制限の模擬）
    if q.iter().any(|(k, v)| k == "toc" && v.starts_with("1 1 ")) {
        return (StatusCode::SERVICE_UNAVAILABLE, String::new());
    }
    if discid == NEVERMIND_ID {
        (StatusCode::OK, NEVERMIND.to_owned())
    } else if q.iter().any(|(k, _)| k == "toc") {
        (StatusCode::OK, r#"{"releases":[]}"#.to_owned())
    } else {
        (StatusCode::NOT_FOUND, NOTFOUND.to_owned())
    }
}

async fn release_handler(Path(id): Path<String>) -> (StatusCode, String) {
    if id == FIVE_RELEASE {
        (StatusCode::OK, RELEASE_FIVE.to_owned())
    } else {
        (StatusCode::NOT_FOUND, NOTFOUND.to_owned())
    }
}

async fn recording_search_handler(Query(q): Query<Vec<(String, String)>>) -> (StatusCode, String) {
    let hit = q
        .iter()
        .any(|(k, v)| k == "query" && v.contains("isrc:JPQ402600330"));
    if hit {
        (StatusCode::OK, ISRC_SEARCH_FIVE.to_owned())
    } else {
        (StatusCode::OK, r#"{"count":0,"recordings":[]}"#.to_owned())
    }
}

async fn serve_mb() -> String {
    let app = Router::new()
        .route("/ws/2/discid/{discid}", get(mb_handler))
        .route("/ws/2/release/{id}", get(release_handler))
        .route("/ws/2/recording", get(recording_search_handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}/ws/2/")
}

struct App {
    router: Router,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

impl App {
    async fn new(mb_base: Option<String>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let config = Arc::new(Config::parse(EXAMPLE).unwrap());
        let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
            .await
            .unwrap();
        let mut state = AppState::new(config, db, mode);
        if let Some(base) = mb_base {
            let client =
                MusicBrainzClient::new(base, "spindle-test/0.1", Duration::from_millis(0)).unwrap();
            state = state.with_musicbrainz(Arc::new(client));
        }
        Self {
            router: api::router(state),
            dir,
        }
    }

    async fn cookie(&self) -> String {
        let r = req(Method::POST, "/api/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(r#"{"password":"correct horse"}"#))
            .unwrap();
        let res = self.router.clone().oneshot(r).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let set = res
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        set.split(';').next().unwrap().to_string()
    }

    async fn post(&self, c: &str, body: Value) -> (StatusCode, Value) {
        let r = req(Method::POST, "/api/cd/lookup")
            .header(header::COOKIE, c)
            .header(header::CONTENT_TYPE, "application/json")
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = self.router.clone().oneshot(r).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}

fn req(method: Method, uri: &str) -> axum::http::request::Builder {
    let peer: SocketAddr = LAN.parse().unwrap();
    let mut b = Request::builder().method(method).uri(uri);
    b.extensions_mut().unwrap().insert(ConnectInfo(peer));
    b
}

#[tokio::test]
async fn lookup_returns_discid_and_candidates() {
    let app = App::new(Some(serve_mb().await)).await;
    let c = app.cookie().await;
    let (st, body) = app.post(&c, json!({ "toc": NEVERMIND_TOC })).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["discid"], NEVERMIND_ID);
    assert_eq!(body["exact"], true);
    assert_eq!(
        body["mb_toc"],
        "1 12 192030 150 22743 41850 58283 72070 91348 104618 115338 132138 143908 159828 174565"
    );
    assert_eq!(body["accuraterip_id"], "0013f127-00b61059-a109fe0c");
    // 手入力フォーム（P2-4）の行数と長さの元: TOC から出した音声トラック（75 セクタ = 1 秒）
    let tracks = body["tracks"].as_array().unwrap();
    assert_eq!(tracks.len(), 12);
    assert_eq!(tracks[0]["number"], 1);
    assert_eq!(tracks[0]["length_ms"], 22593 * 1000 / 75);
    assert_eq!(tracks[11]["number"], 12);
    assert_eq!(tracks[11]["length_ms"], (191880 - 174415) * 1000 / 75);
    let cands = body["candidates"].as_array().unwrap();
    assert_eq!(cands.len(), 2);
    assert_eq!(cands[0]["title"], "Nevermind");
    assert_eq!(cands[0]["artist"], "Nirvana");
    assert_eq!(cands[0]["tracks"].as_array().unwrap().len(), 12);
    assert_eq!(cands[0]["tracks"][0]["title"], "Smells Like Teen Spirit");
    assert_eq!(cands[0]["labels"][0][0], "DGC Records");
}

/// MusicBrainz 形式（先頭 末尾 リードアウト+150 各オフセット+150）でも同じ
#[tokio::test]
async fn lookup_accepts_the_musicbrainz_toc_form() {
    let app = App::new(Some(serve_mb().await)).await;
    let c = app.cookie().await;
    let (st, body) = app
        .post(
            &c,
            json!({ "toc": "1 12 192030 150 22743 41850 58283 72070 91348 104618 115338 132138 143908 159828 174565" }),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["discid"], NEVERMIND_ID);
    assert_eq!(body["candidates"].as_array().unwrap().len(), 2);
}

/// DiscID もトラック長も未登録の盤（嵐「Five」）は、ディスクの ISRC と貼り付けたリリース URL で当たる。
/// `isrcs` の null（読めなかったトラック）は捨てる
#[tokio::test]
async fn lookup_uses_isrcs_and_a_pasted_release() {
    let app = App::new(Some(serve_mb().await)).await;
    let c = app.cookie().await;
    let (st, body) = app
        .post(
            &c,
            json!({
                "toc": "0:20144:40290",
                "isrcs": ["JPQ402600330", null],
                "mcn": null,
                "release": format!("https://musicbrainz.org/release/{FIVE_RELEASE}/disc/1"),
            }),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["exact"], false);
    assert_eq!(body["notes"], json!([]));
    let cands = body["candidates"].as_array().unwrap();
    assert_eq!(cands.len(), 1, "{body}");
    assert_eq!(cands[0]["release_id"], FIVE_RELEASE);
    assert_eq!(cands[0]["title"], "Five");
    assert_eq!(cands[0]["matched_by"], json!(["release", "isrc"]));
    assert_eq!(cands[0]["tracks"].as_array().unwrap().len(), 2);
    // 指定だけが読めないときは notes に理由（候補は他の経路のまま）
    let (st, body) = app
        .post(&c, json!({ "toc": "0:20144:40290", "release": "nonsense" }))
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["notes"].as_array().unwrap().len(), 1, "{body}");
}

/// 入力の ISRC / MCN は検証してから MB のクエリに載せる（codex 指摘: API は任意の JSON を受ける）
#[tokio::test]
async fn bad_isrcs_and_mcn_are_400() {
    let app = App::new(Some(serve_mb().await)).await;
    let c = app.cookie().await;
    for (body, what) in [
        (
            json!({ "toc": "0:20144:40290", "isrcs": ["JPQ40"] }),
            "短い ISRC",
        ),
        (
            json!({ "toc": "0:20144:40290", "isrcs": ["JPQ4026003 0"] }),
            "空白入り",
        ),
        (
            json!({ "toc": "0:20144:40290", "isrcs": ["JPQ402600330\" OR *"] }),
            "クエリ文字",
        ),
        (
            json!({ "toc": "0:20144:40290", "isrcs": ["JPQ402600330", "JPQ402600340", "JPQ402600350"] }),
            "トラック数超え",
        ),
        (
            json!({ "toc": "0:20144:40290", "mcn": "45825157784" }),
            "短い MCN",
        ),
        (
            json!({ "toc": "0:20144:40290", "mcn": "458251577849A" }),
            "数字でない MCN",
        ),
    ] {
        let (st, res) = app.post(&c, body).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{what}: {res}");
        assert_eq!(res["error"], "bad_request", "{what}");
    }
    // 小文字・前後の空白は正規化して通す。空文字と null は「無い」扱い
    let (st, res) = app
        .post(
            &c,
            json!({ "toc": "0:20144:40290", "isrcs": [" jpq402600330 ", "", null], "mcn": "  " }),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{res}");
    assert_eq!(res["candidates"].as_array().unwrap().len(), 1, "{res}");
}

#[tokio::test]
async fn unknown_disc_yields_no_candidates() {
    let app = App::new(Some(serve_mb().await)).await;
    let c = app.cookie().await;
    let (st, body) = app.post(&c, json!({ "toc": "0:20000:40000:60000" })).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["exact"], false);
    assert_eq!(body["candidates"].as_array().unwrap().len(), 0);
    // 候補が無くても手入力へ進めるようにトラックは返す
    assert_eq!(body["tracks"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn bad_toc_is_400_and_missing_client_is_503() {
    let app = App::new(Some(serve_mb().await)).await;
    let c = app.cookie().await;
    for bad in ["", "abc", "0:10:5:20", "1 2", "1 3 1000 150 300"] {
        let (st, body) = app.post(&c, json!({ "toc": bad })).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{bad}: {body}");
        assert_eq!(body["error"], "bad_request");
    }
    let (st, _) = app.post("", json!({ "toc": NEVERMIND_TOC })).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    let app = App::new(None).await;
    let c = app.cookie().await;
    let (st, body) = app.post(&c, json!({ "toc": NEVERMIND_TOC })).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{body}");
}

#[tokio::test]
async fn unreachable_musicbrainz_is_502() {
    let app = App::new(Some("http://127.0.0.1:9/ws/2/".to_owned())).await;
    let c = app.cookie().await;
    let (st, body) = app.post(&c, json!({ "toc": NEVERMIND_TOC })).await;
    assert_eq!(st, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["error"], "lookup_failed");
}

/// 再試行しても 503 なら musicbrainz_unavailable（一時的。502 の lookup_failed とは分ける）
#[tokio::test]
async fn overloaded_musicbrainz_is_503() {
    let app = App::new(Some(serve_mb().await)).await;
    let c = app.cookie().await;
    // 1 トラックの TOC は mock が常に 503 を返す
    let (st, body) = app.post(&c, json!({ "toc": "0:5000" })).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "musicbrainz_unavailable");
}

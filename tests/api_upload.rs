//! アップロードの HTTP API（`/api/uploads`。設計 §4.2・§5.2・§5.5・§5.7）。
//! 一時ディレクトリの Inbox と本物のルータで、送信・分け方・公開・捨てるを通しで確かめる。
//! jobs 行を直接 running にはしない（D-76）: 展開中は「queued のジョブがある」で作る

#![cfg(target_os = "linux")]

mod common;

use std::io::{Cursor, Write as _};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use rusqlite::Connection;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use spindle::api::{self, auth, upload::CHUNK_LIMIT, AppState};
use spindle::config::{Config, UploadConfig};
use spindle::db::jobs::{self as dbjobs, JobState, JobType};
use spindle::db::Db;
use spindle::fsroot::RootDir;
use spindle::import::upload::{
    read_manifest, with_manifest, Manifest, UploadId, UploadLocks, ZipError, DISCARD_MARK,
};
use spindle::jobs::handlers::upload::UploadHandler;
use spindle::jobs::Registry;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

struct App {
    state: AppState,
    router: Router,
    dir: tempfile::TempDir,
    inbox: Arc<RootDir>,
    locks: UploadLocks,
    cookie: String,
    shutdown: CancellationToken,
}

impl App {
    async fn new() -> Self {
        Self::build(true).await
    }

    async fn build(with_inbox: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["Inbox", "src"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let config = Arc::new(Config::parse(EXAMPLE).unwrap());
        let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
            .await
            .unwrap();
        let inbox = Arc::new(RootDir::open(&dir.path().join("Inbox")).unwrap());
        let locks = UploadLocks::default();
        let mut state = AppState::new(config, db, mode);
        if with_inbox {
            state = state
                .with_inbox(inbox.clone())
                .with_uploads(locks.clone(), UploadConfig::default());
        }
        let router = api::router(state.clone());
        let mut app = Self {
            state,
            router,
            dir,
            inbox,
            locks,
            cookie: String::new(),
            shutdown: CancellationToken::new(),
        };
        app.cookie = app.login().await;
        app
    }

    async fn login(&self) -> String {
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

    /// 本物の upload ハンドラでワーカーを起動する
    fn start_worker(&self) {
        let mut reg = Registry::new();
        reg.register(
            JobType::Upload,
            Arc::new(UploadHandler::new(
                self.inbox.clone(),
                self.locks.clone(),
                UploadConfig::default(),
            )),
        );
        self.state.jobs.start(reg, self.shutdown.clone());
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        site: &str,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) -> (StatusCode, Value) {
        let mut b = req(method, path)
            .header(header::COOKIE, &self.cookie)
            .header("sec-fetch-site", site);
        if let Some(ct) = content_type {
            b = b.header(header::CONTENT_TYPE, ct);
        }
        let res = self
            .router
            .clone()
            .oneshot(b.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn json(&self, method: Method, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(
            method,
            path,
            "same-origin",
            Some("application/json"),
            body.to_string().into_bytes(),
        )
        .await
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.call(Method::GET, path, "same-origin", None, Vec::new())
            .await
    }

    async fn delete(&self, path: &str) -> StatusCode {
        self.call(Method::DELETE, path, "same-origin", None, Vec::new())
            .await
            .0
    }

    async fn create(&self) -> String {
        let (st, v) = self
            .call(
                Method::POST,
                "/api/uploads",
                "same-origin",
                None,
                Vec::new(),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        v["id"].as_str().unwrap().to_string()
    }

    async fn begin(&self, id: &str, path: &str, total: usize) -> String {
        let (st, v) = self
            .json(
                Method::POST,
                &format!("/api/uploads/{id}/origins"),
                json!({ "path": path, "total": total }),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        v["origin"]["id"].as_str().unwrap().to_string()
    }

    async fn put(
        &self,
        id: &str,
        origin: &str,
        offset: usize,
        bytes: &[u8],
    ) -> (StatusCode, Value) {
        self.call(
            Method::PUT,
            &format!("/api/uploads/{id}/origins/{origin}?offset={offset}"),
            "same-origin",
            Some("application/octet-stream"),
            bytes.to_vec(),
        )
        .await
    }

    /// 1 ファイルを 1 チャンクで送り終える
    async fn send(&self, id: &str, path: &str, bytes: &[u8]) -> String {
        let origin = self.begin(id, path, bytes.len()).await;
        let (st, v) = self.put(id, &origin, 0, bytes).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["complete"], true);
        origin
    }

    fn wav(&self, name: &str, seed: u32, album: Option<&str>, track: u32) -> Vec<u8> {
        let p = self.dir.path().join("src").join(format!("{seed}-{name}"));
        common::write_wav(&p, &common::pcm_samples(seed), 16);
        if let Some(album) = album {
            common::set_basic_tags(&p, name, "Artist", album, "Artist", track, 1);
        }
        std::fs::read(&p).unwrap()
    }

    /// タグ付きの 2 曲を入れた zip
    fn zip(&self) -> Vec<u8> {
        let x1 = self.wav("01.wav", 11, Some("Z"), 1);
        let x2 = self.wav("02.wav", 12, Some("Z"), 2);
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in [("Z/01.wav", &x1), ("Z/02.wav", &x2)] {
            w.start_file(name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    /// 2 件: タグ付きの `X`（2 曲）と、ばらの未タグ 1 曲
    async fn two_groups(&self, id: &str) {
        let x1 = self.wav("01.wav", 1, Some("X"), 1);
        let x2 = self.wav("02.wav", 2, Some("X"), 2);
        let loose = self.wav("loose.wav", 3, None, 1);
        self.send(id, "X/01.wav", &x1).await;
        self.send(id, "X/02.wav", &x2).await;
        self.send(id, "loose.wav", &loose).await;
    }

    fn uid(id: &str) -> UploadId {
        UploadId::parse(id).unwrap()
    }

    fn manifest(&self, id: &str) -> Manifest {
        read_manifest(&self.inbox, &Self::uid(id)).unwrap()
    }

    fn edit(&self, id: &str, f: impl FnOnce(&mut Manifest)) {
        with_manifest(&self.inbox, &self.locks, &Self::uid(id), |m| {
            f(m);
            Ok(())
        })
        .unwrap();
    }

    fn staging(&self, id: &str) -> PathBuf {
        self.dir
            .path()
            .join("Inbox")
            .join(Self::uid(id).staging_dir().as_str())
    }

    fn conn(&self) -> Connection {
        Connection::open(self.dir.path().join("spindle.db")).unwrap()
    }

    fn upload_jobs(&self) -> Vec<dbjobs::Job> {
        let c = self.conn();
        let mut st = c
            .prepare("SELECT id FROM jobs WHERE type = 'upload' ORDER BY id")
            .unwrap();
        let ids: Vec<i64> = st
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        ids.into_iter()
            .map(|id| dbjobs::get(&c, id).unwrap().unwrap())
            .collect()
    }

    async fn wait_terminal(&self, job: i64) -> dbjobs::Job {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            let j = dbjobs::get(&self.conn(), job).unwrap().unwrap();
            if j.state.is_terminal() {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("ジョブ {job} が終端にならない");
    }

    /// 壊れた zip を送り、本物のワーカーで展開させて（やり直せない失敗）、その origin を返す
    async fn send_broken_zip_and_unpack(&self, id: &str) -> String {
        let origin = self.send(id, "bad.zip", b"this is not a zip at all").await;
        let job = self
            .upload_jobs()
            .into_iter()
            .find(|j| j.payload["origin"] == origin.as_str())
            .unwrap();
        let done = self.wait_terminal(job.id).await;
        assert_eq!(done.state, JobState::Done, "{:?}", done.last_error);
        let m = self.manifest(id);
        let o = m.origins.iter().find(|o| o.id == origin).unwrap();
        let err = o.zip.as_ref().unwrap().error.as_ref().unwrap();
        assert!(!err.retryable, "{err:?}");
        origin
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn req(method: Method, uri: &str) -> axum::http::request::Builder {
    let peer: SocketAddr = LAN.parse().unwrap();
    let mut b = Request::builder().method(method).uri(uri);
    b.extensions_mut().unwrap().insert(ConnectInfo(peer));
    b
}

#[tokio::test]
async fn create_upload_and_send_chunks() {
    let app = App::new().await;
    let id = app.create().await;
    assert!(app.staging(&id).join("upload.json").is_file());
    let bytes = app.wav("01.wav", 1, Some("X"), 1);
    let origin = app.begin(&id, "X/01.wav", bytes.len()).await;
    assert_eq!(origin, "o1");
    let half = bytes.len() / 2;
    let (st, v) = app.put(&id, &origin, 0, &bytes[..half]).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v, json!({ "received": half, "complete": false }));
    let (st, v) = app.put(&id, &origin, half, &bytes[half..]).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v, json!({ "received": bytes.len(), "complete": true }));

    let (st, v) = app.get(&format!("/api/uploads/{id}")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["origins"][0]["complete"], true);
    assert_eq!(v["groups"].as_array().unwrap().len(), 1);
    assert_eq!(v["groups"][0]["album"], "X");
    assert_eq!(v["groups"][0]["track_count"], 1);
    assert_eq!(v["busy"], false);
    assert!(v["publish_error"].is_null());

    // 一覧にも出る（新しい順）
    let id2 = app.create().await;
    let (st, v) = app.get("/api/uploads").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let ids: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&id.as_str()) && ids.contains(&id2.as_str()));
    assert_eq!(v["broken"], json!([]));

    // 同じパスの完了済みは 409 exists、不正なパスは 400
    let (st, v) = app
        .json(
            Method::POST,
            &format!("/api/uploads/{id}/origins"),
            json!({ "path": "X/01.wav", "total": 3 }),
        )
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("exists"))
    );
    let (st, v) = app
        .json(
            Method::POST,
            &format!("/api/uploads/{id}/origins"),
            json!({ "path": "../x.flac", "total": 3 }),
        )
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("bad_path"))
    );
}

#[tokio::test]
async fn unknown_upload_and_origin_are_distinct_404() {
    let app = App::new().await;
    let id = app.create().await;
    // 照合と一覧はあるアップロードの鍵を取る（ここまでで表にあるのは実在の 1 つ）
    app.get("/api/uploads").await;
    let before = app.locks.entries();
    assert_eq!(before, 1);
    // 無いアップロード: 404 not_found で、鍵の表に項を作らない
    let none = "0123456789abcdef";
    let (st, v) = app.get(&format!("/api/uploads/{none}")).await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::NOT_FOUND, Some("not_found"))
    );
    let (st, v) = app.put(none, "o1", 0, b"x").await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::NOT_FOUND, Some("not_found"))
    );
    assert_eq!(
        app.delete(&format!("/api/uploads/{none}")).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(app.locks.entries(), before);
    // 無い origin: 404 origin_not_found
    let (st, v) = app.put(&id, "o9", 0, b"x").await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::NOT_FOUND, Some("origin_not_found"))
    );
    let (st, v) = app
        .call(
            Method::POST,
            &format!("/api/uploads/{id}/origins/o9/unpack"),
            "same-origin",
            None,
            Vec::new(),
        )
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::NOT_FOUND, Some("origin_not_found"))
    );
    let (st, v) = app
        .json(
            Method::PATCH,
            &format!("/api/uploads/{id}"),
            json!({ "removed_origins": ["o9"] }),
        )
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::NOT_FOUND, Some("origin_not_found"))
    );
}

#[tokio::test]
async fn zip_complete_enqueues_unpack() {
    let app = App::new().await;
    let id = app.create().await;
    let bytes = app.zip();
    let origin = app.begin(&id, "z.zip", bytes.len()).await;
    let half = bytes.len() / 2;
    app.put(&id, &origin, 0, &bytes[..half]).await;
    assert!(app.upload_jobs().is_empty(), "途中では投入しない");
    let (st, _) = app.put(&id, &origin, half, &bytes[half..]).await;
    assert_eq!(st, StatusCode::OK);
    let jobs = app.upload_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].state, JobState::Queued);
    assert_eq!(jobs[0].payload["op"], "unpack");
    assert_eq!(jobs[0].payload["origin"], origin.as_str());
    assert_eq!(jobs[0].payload["retry"], false);

    // 展開すると件が見える
    app.start_worker();
    let done = app.wait_terminal(jobs[0].id).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.last_error);
    let (_, v) = app.get(&format!("/api/uploads/{id}")).await;
    assert_eq!(v["origins"][0]["zip"]["generation"], 1);
    assert_eq!(v["groups"][0]["album"], "Z");
    assert_eq!(v["groups"][0]["track_count"], 2);
}

#[tokio::test]
async fn offset_mismatch_409_with_received() {
    let app = App::new().await;
    let id = app.create().await;
    let origin = app.begin(&id, "a.wav", 20).await;
    let (st, _) = app.put(&id, &origin, 0, &[1u8; 10]).await;
    assert_eq!(st, StatusCode::OK);
    let (st, v) = app.put(&id, &origin, 5, &[2u8; 5]).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(v, json!({ "error": "offset", "received": 10 }));
    // 返った位置から続けられる
    let (st, v) = app.put(&id, &origin, 10, &[2u8; 10]).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["complete"], true);
}

#[tokio::test]
async fn publish_requires_complete_and_nonempty() {
    let app = App::new().await;
    let id = app.create().await;
    let publish = format!("/api/uploads/{id}/publish");
    let bytes = app.wav("01.wav", 1, Some("X"), 1);
    let origin = app.begin(&id, "X/01.wav", bytes.len()).await;
    let half = bytes.len() / 2;
    app.put(&id, &origin, 0, &bytes[..half]).await;
    let (st, v) = app.json(Method::POST, &publish, json!({})).await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("incomplete"))
    );
    app.put(&id, &origin, half, &bytes[half..]).await;

    // 件を全部外すと空
    let (_, v) = app.get(&format!("/api/uploads/{id}")).await;
    let key = v["groups"][0]["key"].as_str().unwrap().to_string();
    let (st, _) = app
        .json(
            Method::PATCH,
            &format!("/api/uploads/{id}"),
            json!({ "excluded_groups": [key] }),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let (st, v) = app.json(Method::POST, &publish, json!({})).await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("empty"))
    );
    assert!(app.manifest(&id).publish.is_none());
    assert!(app.upload_jobs().is_empty());
}

#[tokio::test]
async fn publish_twice_is_idempotent() {
    let app = App::new().await;
    let id = app.create().await;
    app.two_groups(&id).await;
    let publish = format!("/api/uploads/{id}/publish");
    let (st, v1) = app.json(Method::POST, &publish, json!({})).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{v1}");
    let frozen = app.manifest(&id).publish.unwrap();
    let (st, v2) = app.json(Method::POST, &publish, json!({})).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{v2}");
    assert_eq!(v1["job_id"], v2["job_id"], "active な公開ジョブにまとまる");
    assert_eq!(
        app.manifest(&id).publish.unwrap(),
        frozen,
        "計画は変わらない"
    );
    let jobs = app.upload_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].payload["op"], "publish");

    // 公開を走らせる。ファイルは publish/<n>/ を経て Inbox へ移るが、表示は計画を出す
    app.start_worker();
    let done = app.wait_terminal(jobs[0].id).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.last_error);
    let (st, v) = app.get(&format!("/api/uploads/{id}")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["groups"], json!([]));
    assert_eq!(v["ignored"], json!([]));
    let groups = v["publish"]["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    for g in groups {
        assert_eq!(g["stage"], "published", "{g}");
        assert!(g["dest"].as_str().unwrap().starts_with("upload/"), "{g}");
        assert!(
            g["base_dest"].as_str().unwrap().starts_with("upload/"),
            "{g}"
        );
    }
    let x = groups.iter().find(|g| g["album"] == "X").unwrap();
    assert_eq!(x["track_count"], 2);
    assert_eq!(x["dest"], "upload/Artist/X");
    // 公開を終えて捨てた扱いになったものは一覧には出ない
    let (_, list) = app.get("/api/uploads").await;
    assert_eq!(list["items"], json!([]));
    // 公開後の押し直しも 202（公開は済んでいるので何も置き直さない）
    let (st, _) = app.json(Method::POST, &publish, json!({})).await;
    assert_eq!(st, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn frozen_view_does_not_read_moved_files() {
    let app = App::new().await;
    let id = app.create().await;
    app.two_groups(&id).await;
    let (st, _) = app
        .json(
            Method::POST,
            &format!("/api/uploads/{id}/publish"),
            json!({}),
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED);
    // 組み立ての途中（files/ から publish/1/ へ移した）を模して、元のファイルを消す
    let plan = app.manifest(&id).publish.unwrap();
    for g in &plan.groups {
        for mem in &g.members {
            std::fs::remove_file(app.staging(&id).join(&mem.from)).unwrap();
        }
    }
    let (st, v) = app.get(&format!("/api/uploads/{id}")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        v["groups"],
        json!([]),
        "凍結後はステージングを件に分け直さない"
    );
    assert_eq!(v["ignored"], json!([]));
    assert_eq!(v["unassigned_companions"], json!([]));
    for o in v["origins"].as_array().unwrap() {
        assert_eq!(o["damaged"], false, "{o}");
    }
    let groups = v["publish"]["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert!(groups.iter().all(|g| g["stage"] == "planned"));
    assert_eq!(v["busy"], true, "公開ジョブが待っている");
}

#[tokio::test]
async fn publish_error_skipped_by_reconcile_and_cleared_by_republish() {
    let app = App::new().await;
    let id = app.create().await;
    app.two_groups(&id).await;
    let (st, v) = app
        .json(
            Method::POST,
            &format!("/api/uploads/{id}/publish"),
            json!({}),
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED);
    // 公開ジョブが失敗で止まった（ジョブは終端、マニフェストに理由）状態を作る
    let job = v["job_id"].as_i64().unwrap();
    app.conn()
        .execute("DELETE FROM jobs WHERE id = ?1", [job])
        .unwrap();
    app.edit(&id, |m| {
        m.publish_error = Some("公開するファイルが無い: x".into())
    });
    let (_, v) = app.get(&format!("/api/uploads/{id}")).await;
    assert_eq!(v["publish_error"], "公開するファイルが無い: x");
    assert!(
        app.upload_jobs().is_empty(),
        "照合は失敗した公開を投入し直さない"
    );
    // 押し直しで消えて投入し直す
    let (st, _) = app
        .json(
            Method::POST,
            &format!("/api/uploads/{id}/publish"),
            json!({}),
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED);
    assert!(app.manifest(&id).publish_error.is_none());
    let jobs = app.upload_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].payload["op"], "publish");
}

#[tokio::test]
async fn patch_merge_changes_groups() {
    let app = App::new().await;
    let id = app.create().await;
    let x = app.wav("01.wav", 1, Some("X"), 1);
    let y = app.wav("01.wav", 2, Some("Y"), 1);
    app.send(&id, "X/01.wav", &x).await;
    app.send(&id, "Y/01.wav", &y).await;
    app.edit(&id, |m| m.updated_at = 1);
    let (_, v) = app.get(&format!("/api/uploads/{id}")).await;
    let keys: Vec<String> = v["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["key"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(keys.len(), 2);
    let (st, v) = app
        .json(
            Method::PATCH,
            &format!("/api/uploads/{id}"),
            json!({ "merges": [keys] }),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let groups = v["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["track_count"], 2);
    let m = app.manifest(&id);
    assert_eq!(m.merges.len(), 1);
    assert!(m.updated_at > 1, "利用者の操作として updated_at を進める");
    // 与えなかった項目は変えない
    let (st, _) = app
        .json(
            Method::PATCH,
            &format!("/api/uploads/{id}"),
            json!({ "excluded_files": ["files/o1"] }),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let m = app.manifest(&id);
    assert_eq!(m.merges.len(), 1);
    assert_eq!(m.excluded_files, vec!["files/o1".to_string()]);
    // 知らない項目は 4xx
    let (st, _) = app
        .json(
            Method::PATCH,
            &format!("/api/uploads/{id}"),
            json!({ "bogus": 1 }),
        )
        .await;
    assert!(st.is_client_error(), "{st}");
}

#[tokio::test]
async fn remove_failed_zip_then_publish() {
    let app = App::new().await;
    app.start_worker();
    let id = app.create().await;
    let wav = app.wav("01.wav", 1, Some("X"), 1);
    app.send(&id, "X/01.wav", &wav).await;
    let origin = app.send_broken_zip_and_unpack(&id).await;
    let publish = format!("/api/uploads/{id}/publish");
    let (st, v) = app.json(Method::POST, &publish, json!({})).await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("incomplete"))
    );
    let (st, v) = app
        .json(
            Method::PATCH,
            &format!("/api/uploads/{id}"),
            json!({ "removed_origins": [origin] }),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let o = v["origins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == origin.as_str())
        .unwrap();
    assert_eq!(o["removed"], true);
    let (st, v) = app.json(Method::POST, &publish, json!({})).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{v}");
    let plan = app.manifest(&id).publish.unwrap();
    assert_eq!(plan.groups.len(), 1);
}

#[tokio::test]
async fn retry_unpack_only_when_retryable() {
    let app = App::new().await;
    app.start_worker();
    let id = app.create().await;
    let origin = app.send_broken_zip_and_unpack(&id).await;
    let retry = format!("/api/uploads/{id}/origins/{origin}/unpack");
    let (st, v) = app
        .call(Method::POST, &retry, "same-origin", None, Vec::new())
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("not_retryable"))
    );
    // やり直せる失敗（空き不足など）なら retry: true で投入する
    app.shutdown.cancel();
    tokio::time::sleep(Duration::from_millis(50)).await;
    app.edit(&id, |m| {
        m.origins[0].zip.as_mut().unwrap().error = Some(ZipError {
            reason: "空き容量が足りない".into(),
            retryable: true,
        });
    });
    let (st, v) = app
        .call(Method::POST, &retry, "same-origin", None, Vec::new())
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{v}");
    let job = dbjobs::get(&app.conn(), v["job_id"].as_i64().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(job.payload["op"], "unpack");
    assert_eq!(job.payload["retry"], true);
    // ばらのファイル（zip でない）も 409
    let wav = app.wav("01.wav", 1, Some("X"), 1);
    let loose = app.send(&id, "X/01.wav", &wav).await;
    let (st, _) = app
        .call(
            Method::POST,
            &format!("/api/uploads/{id}/origins/{loose}/unpack"),
            "same-origin",
            None,
            Vec::new(),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
}

#[tokio::test]
async fn get_one_runs_reconcile() {
    let app = App::new().await;
    let id = app.create().await;
    let bytes = app.zip();
    app.send(&id, "z.zip", &bytes).await;
    // 投入の後に DB を失った（jobs が空）
    app.conn().execute("DELETE FROM jobs", []).unwrap();
    assert!(app.upload_jobs().is_empty());
    let (st, v) = app.get(&format!("/api/uploads/{id}")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let jobs = app.upload_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].payload["op"], "unpack");
    assert_eq!(v["busy"], true);
    // 一覧でも同じく照合する（重ねて投入しない）
    let (st, _) = app.get("/api/uploads").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(app.upload_jobs().len(), 1);
}

#[tokio::test]
async fn get_syncs_manifest_with_disk_and_persists() {
    let app = App::new().await;
    let id = app.create().await;
    let origin = app.begin(&id, "a.wav", 20).await;
    app.put(&id, &origin, 0, &[1u8; 10]).await;
    // 電源断でマニフェストが遅れた（.part は 10 バイトあるが received は 0）
    app.edit(&id, |m| m.origins[0].received = 0);
    let (_, v) = app.get(&format!("/api/uploads/{id}")).await;
    assert_eq!(v["origins"][0]["received"], 10);
    assert_eq!(
        app.manifest(&id).origins[0].received,
        10,
        "合わせた結果を書く"
    );
}

#[tokio::test]
async fn patch_and_delete_busy_while_unpacking() {
    let app = App::new().await;
    let id = app.create().await;
    let bytes = app.zip();
    app.send(&id, "z.zip", &bytes).await;
    assert_eq!(app.upload_jobs()[0].state, JobState::Queued);
    let (st, v) = app
        .json(
            Method::PATCH,
            &format!("/api/uploads/{id}"),
            json!({ "excluded_files": [] }),
        )
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("busy"))
    );
    let (st, v) = app
        .call(
            Method::DELETE,
            &format!("/api/uploads/{id}"),
            "same-origin",
            None,
            Vec::new(),
        )
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("busy"))
    );
    assert!(app.manifest(&id).discarded_at.is_none());
}

#[tokio::test]
async fn delete_after_publish_requested_is_409() {
    let app = App::new().await;
    let id = app.create().await;
    app.two_groups(&id).await;
    let (st, _) = app
        .json(
            Method::POST,
            &format!("/api/uploads/{id}/publish"),
            json!({}),
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED);
    // 公開ジョブを消しても（ジョブの有無によらず）公開要求後は捨てられない
    app.conn().execute("DELETE FROM jobs", []).unwrap();
    let (st, v) = app
        .call(
            Method::DELETE,
            &format!("/api/uploads/{id}"),
            "same-origin",
            None,
            Vec::new(),
        )
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("busy"))
    );
    app.conn().execute("DELETE FROM jobs", []).unwrap();
    let (st, v) = app
        .json(
            Method::PATCH,
            &format!("/api/uploads/{id}"),
            json!({ "merges": [] }),
        )
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("busy"))
    );
    assert!(app.manifest(&id).discarded_at.is_none());
}

#[tokio::test]
async fn delete_marks_discarded_and_hides() {
    let app = App::new().await;
    let id = app.create().await;
    let wav = app.wav("01.wav", 1, Some("X"), 1);
    app.send(&id, "X/01.wav", &wav).await;
    app.get("/api/uploads").await;
    assert!(!app.state.uploads.as_ref().unwrap().cache.is_empty());
    assert_eq!(
        app.delete(&format!("/api/uploads/{id}")).await,
        StatusCode::NO_CONTENT
    );
    assert!(app.manifest(&id).discarded_at.is_some());
    assert!(app.state.uploads.as_ref().unwrap().cache.is_empty());
    // 物理削除はしない（GC が消す）
    assert!(app.staging(&id).join("files/o1").is_file());
    let (_, v) = app.get("/api/uploads").await;
    assert_eq!(v["items"], json!([]));
    let (st, _) = app.get(&format!("/api/uploads/{id}")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // 二度押しも 204
    assert_eq!(
        app.delete(&format!("/api/uploads/{id}")).await,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn broken_listed_and_marked() {
    let app = App::new().await;
    let good = app.create().await;
    let dir = ".spindle-upload-00000000000000aa";
    std::fs::create_dir(app.dir.path().join("Inbox").join(dir)).unwrap();
    let (_, v) = app.get("/api/uploads").await;
    let broken = v["broken"].as_array().unwrap();
    assert_eq!(broken.len(), 1);
    assert_eq!(broken[0]["dir"], dir);
    assert!(broken[0]["mtime_ns"].as_i64().unwrap() > 0);
    assert!(!broken[0]["reason"].as_str().unwrap().is_empty());
    assert_eq!(v["items"].as_array().unwrap().len(), 1);

    assert_eq!(
        app.delete(&format!("/api/uploads/broken/{dir}")).await,
        StatusCode::NO_CONTENT
    );
    let marked = app.dir.path().join("Inbox").join(dir).join(DISCARD_MARK);
    assert!(marked.is_file());
    let (_, v) = app.get("/api/uploads").await;
    assert_eq!(v["broken"], json!([]), "印を付けたものは出さない");
    assert!(
        app.dir.path().join("Inbox").join(dir).is_dir(),
        "消すのは GC"
    );

    // 読めるアップロードには付けない・名前の形が違うものは 400
    let good_dir = App::uid(&good).staging_dir();
    assert_eq!(
        app.delete(&format!("/api/uploads/broken/{}", good_dir.as_str()))
            .await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        app.delete("/api/uploads/broken/Album").await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.delete("/api/uploads/broken/.spindle-upload-00000000000000bb")
            .await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn chunk_over_limit_413() {
    let app = App::new().await;
    let id = app.create().await;
    let total = CHUNK_LIMIT + 1;
    let origin = app.begin(&id, "big.wav", total).await;
    let (st, _) = app.put(&id, &origin, 0, &vec![0u8; total]).await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(app.manifest(&id).origins[0].received, 0);
    // 宣言サイズを超えるチャンクも 413
    let small = app.begin(&id, "small.wav", 4).await;
    let (st, v) = app.put(&id, &small, 0, b"12345").await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::PAYLOAD_TOO_LARGE, Some("limit"))
    );
}

#[tokio::test]
async fn csrf_cross_site_put_rejected() {
    let app = App::new().await;
    let id = app.create().await;
    let origin = app.begin(&id, "a.wav", 4).await;
    let (st, v) = app
        .call(
            Method::PUT,
            &format!("/api/uploads/{id}/origins/{origin}?offset=0"),
            "cross-site",
            Some("application/octet-stream"),
            b"abcd".to_vec(),
        )
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("csrf"))
    );
    assert_eq!(app.manifest(&id).origins[0].received, 0);
    assert!(!app.manifest(&id).origins[0].complete);
}

#[tokio::test]
async fn inbox_unset_is_503() {
    let app = App::build(false).await;
    let (st, v) = app.get("/api/uploads").await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("inbox_unavailable"))
    );
    let (st, _) = app
        .call(
            Method::POST,
            "/api/uploads",
            "same-origin",
            None,
            Vec::new(),
        )
        .await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
}

/// 画面は 2 秒ごとに取り直す: 変わっていないファイルのタグと指紋を読み直さない。
/// 読んだ回数は `StagedReadCache::reads`（実際に read_audio_file と指紋を取った回数）で数える
#[tokio::test]
async fn get_does_not_reread_unchanged_files() {
    let app = App::new().await;
    let id = app.create().await;
    app.two_groups(&id).await;
    let cache = &app.state.uploads.as_ref().unwrap().cache;
    let (_, first) = app.get(&format!("/api/uploads/{id}")).await;
    let reads = cache.reads();
    assert_eq!(reads, 3, "最初は 3 曲を読む");
    let (_, second) = app.get(&format!("/api/uploads/{id}")).await;
    let (_, _) = app.get("/api/uploads").await;
    assert_eq!(cache.reads(), reads, "変わっていなければ読み直さない");
    assert_eq!(first["groups"], second["groups"]);

    // 実体が変わった（外から書き換えた）ファイルだけ読み直す
    let other = app.wav("01.wav", 9, Some("X"), 1);
    std::fs::write(app.staging(&id).join("files/o1"), &other).unwrap();
    app.get(&format!("/api/uploads/{id}")).await;
    assert_eq!(cache.reads(), reads + 1);

    // 外した origin の項は落ちる
    let (st, _) = app
        .json(
            Method::PATCH,
            &format!("/api/uploads/{id}"),
            json!({ "removed_origins": ["o3"] }),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(cache.len(), 2);
}

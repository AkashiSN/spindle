//! `/api/devices` の Android（adb）分岐（P5-3b Task 6）。偽の adb が手元の sh で端末側スクリプトを実行し、
//! `<tmp>/storage` を端末の `/storage` に見立てる。ワーカーは起動しない（投入されたジョブは DB で確かめる）

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use spindle::api::{self, auth, AppState};
use spindle::config::Config;
use spindle::db::devices::{self, Confirm};
use spindle::db::{now_epoch, Db};
use spindle::device::adb::AdbConfig;
use spindle::device::runtime::AdbRuntime;
use spindle::device::track::TrackedDevice;
use tokio_util::sync::CancellationToken;

const EXAMPLE: &str = include_str!("../deploy/config.example.toml");
const LAN: &str = "192.168.1.23:50000";

/// 偽の adb。書き込み中の fd を別スレッドの fork が引き継ぐと ETXTBSY になるので、1 度だけ書く
fn fake_adb() -> PathBuf {
    static ONCE: std::sync::OnceLock<(tempfile::TempDir, PathBuf)> = std::sync::OnceLock::new();
    ONCE.get_or_init(write_fake_adb).1.clone()
}

/// `tests/device_jobs.rs` と同じ本体（SER1 だけ・`tcp:adb:5037`・HOME 必須・`shell` と `get-state`）
fn write_fake_adb() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("adb");
    std::fs::write(
        &path,
        r#"#!/bin/sh
[ "$ADB_SERVER_SOCKET" = "tcp:adb:5037" ] || { echo "* cannot connect to daemon at $ADB_SERVER_SOCKET" >&2; exit 1; }
[ -n "$HOME" ] && [ -d "$HOME" ] && [ -w "$HOME" ] || { echo "adb_utils.cpp:315 Cannot mkdir '$HOME/.android': Permission denied" >&2; exit 134; }
[ "$1" = "-s" ] || { echo "-s が無い" >&2; exit 2; }
[ "$2" = "SER1" ] || { echo "adb: device '$2' not found" >&2; exit 1; }
if [ "$3" = "get-state" ]; then
  [ "$#" -eq 3 ] || { echo "引数の数が違う: $#" >&2; exit 2; }
  if [ -f "$HOME/offline" ]; then echo "adb: device '$2' not found" >&2; exit 1; fi
  if [ -f "$HOME/unauthorized" ]; then echo "unauthorized"; exit 0; fi
  echo device; exit 0
fi
[ "$3" = "shell" ] || { echo "未対応のサブコマンド $3" >&2; exit 2; }
[ "$#" -eq 4 ] || { echo "引数の数が違う: $#" >&2; exit 2; }
if [ -f "$HOME/drop" ]; then exit 255; fi
exec sh -c "$4"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (dir, path)
}

struct App {
    router: Router,
    db: Arc<Db>,
    rt: Arc<AdbRuntime>,
    cookie: String,
    storage: PathBuf,
    shutdown: CancellationToken,
    _dir: tempfile::TempDir,
}

impl Drop for App {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

impl App {
    async fn new() -> Self {
        Self::build(true).await
    }

    async fn new_without_adb() -> Self {
        Self::build(false).await
    }

    async fn build(adb: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let root: toml::Table = toml::from_str(EXAMPLE).unwrap();
        let config = Arc::new(Config::parse(&toml::to_string(&root).unwrap()).unwrap());
        let mode = auth::bootstrap(&db, Some("correct horse".to_owned()))
            .await
            .unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(storage.join("emulated/0")).unwrap();
        let shutdown = CancellationToken::new();
        let rt = AdbRuntime::with_storage_base(
            AdbConfig {
                program: fake_adb(),
                server: "tcp:adb:5037".into(),
                home,
                timeout: Duration::from_secs(30),
                transfer_timeout: Duration::from_secs(60),
            },
            shutdown.clone(),
            storage.to_str().unwrap().to_owned(),
        );
        let mut state = AppState::new(config, db.clone(), mode);
        if adb {
            state = state.with_adb(rt.clone());
        }
        let router = api::router(state);
        let r = req(Method::POST, "/api/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(r#"{"password":"correct horse"}"#))
            .unwrap();
        let res = router.clone().oneshot(r).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let cookie = res
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        Self {
            router,
            db,
            rt,
            cookie,
            storage,
            shutdown,
            _dir: dir,
        }
    }

    async fn call(&self, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut r = req(method, uri)
            .header("sec-fetch-site", "same-origin")
            .header(header::COOKIE, &self.cookie);
        let body = match body {
            Some(v) => {
                r = r.header(header::CONTENT_TYPE, "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let res = self
            .router
            .clone()
            .oneshot(r.body(body).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn connect(&self, serial: &str) {
        self.rt.apply(vec![TrackedDevice {
            serial: serial.into(),
            state: "device".into(),
            model: Some("XQ_DQ44".into()),
        }]);
    }

    fn disconnect(&self) {
        self.rt.apply(vec![]);
    }

    /// `<tmp>/storage/<volume>/…/Music/spindle`
    fn device_root(&self, volume: &str) -> PathBuf {
        let vol = if volume == "emulated" {
            self.storage.join("emulated/0")
        } else {
            self.storage.join(volume)
        };
        vol.join("Music/spindle")
    }

    async fn register(&self, name: &str, serial: &str, volume: &str) -> (StatusCode, Value) {
        self.call(
            Method::POST,
            "/api/devices",
            Some(json!({
                "name": name, "transport": "adb", "variant": "opus", "selection": "all",
                "serial": serial, "volume": volume
            })),
        )
        .await
    }

    async fn register_ok(&self, name: &str, serial: &str, volume: &str) -> i64 {
        let (s, v) = self.register(name, serial, volume).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        v["id"].as_i64().unwrap()
    }

    async fn diff_token(&self, id: i64) -> String {
        let (s, v) = self
            .call(Method::GET, &format!("/api/devices/{id}/diff"), None)
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["plan_token"].as_str().unwrap().to_owned()
    }

    /// 差分の `plan_token` で計画を直接確定する（ジョブは投入しない）
    async fn confirm(&self, id: i64) {
        let token = self.diff_token(id).await;
        let r = self
            .db
            .write(move |c| devices::confirm_plan(c, id, &token, now_epoch()))
            .await
            .unwrap();
        assert!(matches!(r, Confirm::Created(_)), "{r:?}");
    }

    fn query<T: rusqlite::types::FromSql>(&self, sql: &str, p: impl rusqlite::Params) -> T {
        let conn = rusqlite::Connection::open(self._dir.path().join("spindle.db")).unwrap();
        conn.query_row(sql, p, |r| r.get(0)).unwrap()
    }

    fn plan_job(&self, id: i64) -> Option<i64> {
        self.query(
            "SELECT job_id FROM device_sync_plans WHERE device_id = ?1 ORDER BY id DESC LIMIT 1",
            [id],
        )
    }

    fn plan_state(&self, id: i64) -> Option<String> {
        let conn = rusqlite::Connection::open(self._dir.path().join("spindle.db")).unwrap();
        use rusqlite::OptionalExtension as _;
        conn.query_row(
            "SELECT state FROM device_sync_plans WHERE device_id = ?1 ORDER BY id DESC LIMIT 1",
            [id],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    }

    fn raw_exec(&self, sql: &str) {
        let conn = rusqlite::Connection::open(self._dir.path().join("spindle.db")).unwrap();
        conn.execute_batch(sql).unwrap();
    }

    fn count_jobs(&self, job_type: &str) -> i64 {
        self.query("SELECT COUNT(*) FROM jobs WHERE type = ?1", [job_type])
    }

    fn device_count(&self) -> i64 {
        self.query("SELECT COUNT(*) FROM devices", [])
    }

    /// ADB 同期が無効でも行が残っている端末（後から無効にした）と、その open な計画を直接作る
    async fn insert_adb_device_with_open_plan(&self) -> i64 {
        let id = self
            .db
            .write(|c| {
                devices::create(
                    c,
                    &devices::NewDevice {
                        name: "Xperia",
                        transport: spindle::domain::device::Transport::Adb,
                        variant: spindle::domain::derived::Variant::Opus,
                        selection: devices::Selection::All,
                        adb: Some(("SER1", "emulated", "Music/spindle")),
                    },
                    0,
                )
            })
            .await
            .unwrap()
            .id;
        self.raw_exec(&format!(
            "INSERT INTO device_sync_plans (device_id, plan_token, plan, state, created_at)
             VALUES ({id}, 'x', '[]', 'open', 0)"
        ));
        id
    }

    fn uuid_of(&self, id: i64) -> String {
        self.query("SELECT uuid FROM devices WHERE id = ?1", [id])
    }
}

fn req(method: Method, uri: &str) -> axum::http::request::Builder {
    let peer: SocketAddr = LAN.parse().unwrap();
    let mut b = Request::builder().method(method).uri(uri);
    b.extensions_mut().unwrap().insert(ConnectInfo(peer));
    b
}

#[tokio::test]
async fn unregistered_lists_connected_unknown_devices_with_volumes() {
    let app = App::new().await;
    app.connect("SER1");
    let (s, v) = app
        .call(Method::GET, "/api/devices/adb/unregistered", None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["items"][0]["serial"], "SER1");
    assert_eq!(v["items"][0]["model"], "XQ_DQ44");
    assert_eq!(v["items"][0]["state"], "device");
    assert_eq!(v["items"][0]["volumes"][0]["volume"], "emulated");
    assert_eq!(v["items"][0]["volumes"][0]["state"], "missing");
    assert!(v["items"][0]["error"].is_null());
}

#[tokio::test]
async fn unregistered_does_not_probe_unauthorized_devices() {
    let app = App::new().await;
    app.rt.apply(vec![TrackedDevice {
        serial: "SER1".into(),
        state: "unauthorized".into(),
        model: None,
    }]);
    let (s, v) = app
        .call(Method::GET, "/api/devices/adb/unregistered", None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["items"][0]["state"], "unauthorized");
    assert_eq!(v["items"][0]["volumes"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn register_creates_manifest_row_and_scan() {
    let app = App::new().await;
    app.connect("SER1");
    let (s, v) = app.register("Xperia", "SER1", "emulated").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["transport"], "adb");
    assert_eq!(v["connected"], true);
    assert_eq!(v["adb_state"], "device");
    assert_eq!(v["adb_volume"], "emulated");
    assert_eq!(v["adb_root"], "Music/spindle");
    let manifest =
        std::fs::read_to_string(app.device_root("emulated").join(".spindle/manifest.json"))
            .unwrap();
    let uuid: String = app.uuid_of(v["id"].as_i64().unwrap());
    assert!(manifest.contains(&uuid));
    assert_eq!(app.count_jobs("device_scan"), 1);
    // 2 回目は同じシリアルで 409
    let (s, v) = app.register("Xperia2", "SER1", "emulated").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("serial_registered"))
    );
    // 名前の重複
    let (s, v) = app.register("XPERIA", "SER1", "emulated").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("duplicate"))
    );
    // 未登録の一覧から消える
    let (_, v) = app
        .call(Method::GET, "/api/devices/adb/unregistered", None)
        .await;
    assert_eq!(v["items"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn register_rejects_bad_values() {
    let app = App::new().await;
    app.connect("SER1");
    for (serial, volume) in [("SER 1", "emulated"), ("SER1", "../x"), ("", "emulated")] {
        let (s, v) = app.register("X", serial, volume).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{serial:?} {volume:?} {v}");
    }
}

#[tokio::test]
async fn register_refuses_not_connected_and_non_empty_root() {
    let app = App::new().await;
    let (s, v) = app.register("X", "SER1", "emulated").await;
    assert_eq!(v["error"], "not_connected", "{s}");
    app.rt.apply(vec![TrackedDevice {
        serial: "SER1".into(),
        state: "unauthorized".into(),
        model: None,
    }]);
    let (s, v) = app.register("X", "SER1", "emulated").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("not_connected"))
    );
    assert!(v["message"].as_str().unwrap().contains("USB デバッグ"));
    app.connect("SER1");
    let root = app.device_root("emulated");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("mine.flac"), b"m").unwrap();
    let (s, v) = app.register("X", "SER1", "emulated").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("not_empty"))
    );
    assert_eq!(app.device_count(), 0);
    assert!(root.join("mine.flac").is_file());
}

#[tokio::test]
async fn register_reclaims_a_leftover_spindle_dir_of_a_failed_registration() {
    let app = App::new().await;
    app.connect("SER1");
    let root = app.device_root("emulated");
    std::fs::create_dir_all(root.join(".spindle")).unwrap();
    std::fs::write(root.join(".spindle/manifest.json"), b"{broken").unwrap();
    let (s, v) = app.register("X", "SER1", "emulated").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
}

#[tokio::test]
async fn register_does_not_reclaim_the_store_of_a_registered_device() {
    let app = App::new().await;
    app.connect("SER1");
    let id = app.register_ok("Xperia", "SER1", "emulated").await;
    // シリアルだけを変えて別の端末に見せかける（同じ保存先に別の端末の manifest がある）
    app.raw_exec(&format!(
        "UPDATE devices SET adb_serial = 'OTHER' WHERE id = {id}"
    ));
    let (s, v) = app.register("X", "SER1", "emulated").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("not_empty"))
    );
    assert!(app
        .device_root("emulated")
        .join(".spindle/manifest.json")
        .is_file());
}

#[tokio::test]
async fn register_failure_leaves_no_row() {
    let app = App::new().await;
    app.connect("SER1");
    // Music を通常ファイルにして mkdir を失敗させる
    let vol = app
        .device_root("emulated")
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::fs::create_dir_all(&vol).unwrap();
    std::fs::write(vol.join("Music"), b"not a dir").unwrap();
    let (s, v) = app.register("X", "SER1", "emulated").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::BAD_GATEWAY, Some("device_failed")),
        "{v}"
    );
    assert_eq!(app.device_count(), 0);
}

#[tokio::test]
async fn sync_confirms_plan_and_enqueues_once() {
    let app = App::new().await;
    app.connect("SER1");
    let id = app.register_ok("Xperia", "SER1", "emulated").await;
    let token = app.diff_token(id).await;
    let (s, v) = app
        .call(
            Method::POST,
            &format!("/api/devices/{id}/sync"),
            Some(json!({"plan_token": token})),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let job = v["job_id"].as_i64().unwrap();
    let (_, v2) = app
        .call(
            Method::POST,
            &format!("/api/devices/{id}/sync"),
            Some(json!({"plan_token": token})),
        )
        .await;
    assert_eq!(v2["job_id"].as_i64(), Some(job), "同じ計画は同じジョブ");
    let (s, v) = app
        .call(
            Method::POST,
            &format!("/api/devices/{id}/sync"),
            Some(json!({"plan_token": "other"})),
        )
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("open_plan_exists"))
    );
    assert_eq!(app.plan_job(id), Some(job));
    assert_eq!(app.count_jobs("device_sync"), 1);
}

#[tokio::test]
async fn sync_with_stale_token_is_plan_changed() {
    let app = App::new().await;
    app.connect("SER1");
    let id = app.register_ok("Xperia", "SER1", "emulated").await;
    let (s, v) = app
        .call(
            Method::POST,
            &format!("/api/devices/{id}/sync"),
            Some(json!({"plan_token": "stale"})),
        )
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("plan_changed"))
    );
    assert!(v["plan_token"].is_string());
}

#[tokio::test]
async fn sync_and_verify_reject_agent_and_unknown_devices() {
    let app = App::new().await;
    let (s, v) = app
        .call(
            Method::POST,
            "/api/devices",
            Some(json!({"name": "iPhone", "transport": "agent", "variant": "aac", "selection": "all"})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let agent = v["id"].as_i64().unwrap();
    assert!(v["connected"].is_null());
    assert!(v["adb_state"].is_null());
    let (s, _) = app
        .call(
            Method::POST,
            &format!("/api/devices/{agent}/sync"),
            Some(json!({"plan_token": "x"})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = app
        .call(Method::POST, &format!("/api/devices/{agent}/verify"), None)
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = app
        .call(
            Method::POST,
            "/api/devices/999/sync",
            Some(json!({"plan_token": "x"})),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = app
        .call(Method::POST, "/api/devices/999/verify", None)
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn abandon_requires_connection_and_free_device() {
    let app = App::new().await;
    app.connect("SER1");
    let id = app.register_ok("Xperia", "SER1", "emulated").await;
    app.confirm(id).await;
    let lock = app.rt.device_lock(id);
    let g = lock.try_lock().unwrap();
    let (s, v) = app
        .call(
            Method::POST,
            &format!("/api/devices/{id}/plans/open/abandon"),
            None,
        )
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("busy"))
    );
    drop(g);
    app.disconnect();
    let (_, v) = app
        .call(
            Method::POST,
            &format!("/api/devices/{id}/plans/open/abandon"),
            None,
        )
        .await;
    assert_eq!(v["error"], "not_connected");
    app.connect("SER1");
    let (s, v) = app
        .call(
            Method::POST,
            &format!("/api/devices/{id}/plans/open/abandon"),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["open_plan"], false);
    assert_eq!(app.plan_state(id).as_deref(), Some("abandoned"));
    let (s, v) = app
        .call(
            Method::POST,
            &format!("/api/devices/{id}/plans/open/abandon"),
            None,
        )
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::NOT_FOUND, Some("no_open_plan"))
    );
}

#[tokio::test]
async fn abandon_refuses_a_running_sync_and_cancels_a_queued_one() {
    let app = App::new().await;
    app.connect("SER1");
    let id = app.register_ok("Xperia", "SER1", "emulated").await;
    let token = app.diff_token(id).await;
    let (s, v) = app
        .call(
            Method::POST,
            &format!("/api/devices/{id}/sync"),
            Some(json!({"plan_token": token})),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let job = v["job_id"].as_i64().unwrap();
    // 実行中の同期（ワーカーは動いていない。状態だけを見る）
    app.raw_exec(&format!(
        "UPDATE jobs SET state = 'running' WHERE id = {job}"
    ));
    let (s, v) = app
        .call(
            Method::POST,
            &format!("/api/devices/{id}/plans/open/abandon"),
            None,
        )
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("busy"))
    );
    app.raw_exec(&format!(
        "UPDATE jobs SET state = 'queued' WHERE id = {job}"
    ));
    let (s, v) = app
        .call(
            Method::POST,
            &format!("/api/devices/{id}/plans/open/abandon"),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let state: String = app.query("SELECT state FROM jobs WHERE id = ?1", [job]);
    assert_eq!(state, "cancelled");
}

#[tokio::test]
async fn broken_plan_json_can_still_be_abandoned_but_not_resumed() {
    let app = App::new().await;
    app.connect("SER1");
    let id = app.register_ok("Xperia", "SER1", "emulated").await;
    app.confirm(id).await;
    // 列は json_valid の CHECK があるので、JSON としては正しいが計画として読めないものにする
    app.raw_exec(r#"UPDATE device_sync_plans SET plan = '{"broken": true}'"#);
    let (s, v) = app
        .call(
            Method::POST,
            &format!("/api/devices/{id}/plans/open/resume"),
            None,
        )
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("plan_unreadable"))
    );
    let (s, _) = app
        .call(
            Method::POST,
            &format!("/api/devices/{id}/plans/open/abandon"),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn resume_and_verify_enqueue_jobs() {
    let app = App::new().await;
    app.connect("SER1");
    let id = app.register_ok("Xperia", "SER1", "emulated").await;
    let (s, v) = app
        .call(
            Method::POST,
            &format!("/api/devices/{id}/plans/open/resume"),
            None,
        )
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::NOT_FOUND, Some("no_open_plan"))
    );
    app.confirm(id).await;
    let (s, v) = app
        .call(
            Method::POST,
            &format!("/api/devices/{id}/plans/open/resume"),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let job = v["job_id"].as_i64().unwrap();
    assert_eq!(app.plan_job(id), Some(job));
    assert_eq!(app.count_jobs("device_sync"), 1);
    let (s, v) = app
        .call(Method::POST, &format!("/api/devices/{id}/verify"), None)
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert!(v["job_id"].is_i64());
    assert_eq!(app.count_jobs("device_verify"), 1);
}

#[tokio::test]
async fn adb_endpoints_are_503_when_disabled() {
    let app = App::new_without_adb().await;
    let (s, v) = app
        .call(Method::GET, "/api/devices/adb/unregistered", None)
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("adb_disabled"))
    );
    let (s, v) = app.register("X", "SER1", "emulated").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("adb_disabled"))
    );
    // 行が既にある（ADB 同期を後から無効にした）端末は、一覧で未接続になり、同期・検証は 503
    let id = app
        .db
        .write(|c| {
            devices::create(
                c,
                &devices::NewDevice {
                    name: "Xperia",
                    transport: spindle::domain::device::Transport::Adb,
                    variant: spindle::domain::derived::Variant::Opus,
                    selection: devices::Selection::All,
                    adb: Some(("SER1", "emulated", "Music/spindle")),
                },
                0,
            )
        })
        .await
        .unwrap()
        .id;
    let (_, v) = app.call(Method::GET, "/api/devices", None).await;
    assert_eq!(v["items"][0]["connected"], false);
    assert!(v["items"][0]["adb_state"].is_null());
    for (uri, body) in [
        (
            format!("/api/devices/{id}/sync"),
            Some(json!({"plan_token": "x"})),
        ),
        (format!("/api/devices/{id}/verify"), None),
        (format!("/api/devices/{id}/plans/open/resume"), None),
        (format!("/api/devices/{id}/plans/open/abandon"), None),
    ] {
        let (s, v) = app.call(Method::POST, &uri, body).await;
        assert_eq!(
            (s, v["error"].as_str()),
            (StatusCode::SERVICE_UNAVAILABLE, Some("adb_disabled")),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn list_reports_connection_and_diff_reports_free() {
    let app = App::new().await;
    app.connect("SER1");
    let id = app.register_ok("Xperia", "SER1", "emulated").await;
    app.rt.set_free(id, 12345);
    let (_, v) = app
        .call(Method::GET, &format!("/api/devices/{id}/diff"), None)
        .await;
    assert_eq!(v["estimate"]["free"], 12345);
    let (_, v) = app.call(Method::GET, "/api/devices", None).await;
    assert_eq!(v["items"][0]["connected"], true);
    app.disconnect();
    let (_, v) = app.call(Method::GET, "/api/devices", None).await;
    assert_eq!(v["items"][0]["connected"], false);
    assert!(v["items"][0]["adb_state"].is_null());
}

#[tokio::test]
async fn list_reports_open_plan_and_sync_job() {
    let app = App::new().await;
    app.connect("SER1");
    let id = app.register_ok("Xperia", "SER1", "emulated").await;
    let (_, v) = app.call(Method::GET, "/api/devices", None).await;
    assert_eq!(v["items"][0]["plan_open"], false);
    assert!(v["items"][0]["sync_job"].is_null());
    let token = app.diff_token(id).await;
    let (_, j) = app
        .call(
            Method::POST,
            &format!("/api/devices/{id}/sync"),
            Some(json!({"plan_token": token})),
        )
        .await;
    let (_, v) = app.call(Method::GET, "/api/devices", None).await;
    assert_eq!(v["items"][0]["plan_open"], true);
    assert_eq!(v["items"][0]["sync_job"]["id"], j["job_id"]);
    assert_eq!(v["items"][0]["sync_job"]["state"], "queued");
}

// ---- 端末が戻らないときの逃げ道（F2: 削除と強制破棄。D-98） ----

#[tokio::test]
async fn delete_is_allowed_with_an_open_plan_and_leaves_device_files() {
    let app = App::new().await;
    app.connect("SER1");
    let id = app.register_ok("Xperia", "SER1", "emulated").await;
    app.confirm(id).await;
    app.disconnect();
    let (s, v) = app
        .call(Method::DELETE, &format!("/api/devices/{id}"), None)
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    assert_eq!(app.device_count(), 0);
    let plans: i64 = app.query("SELECT COUNT(*) FROM device_sync_plans", []);
    assert_eq!(plans, 0, "計画は端末の行と一緒に消える");
    assert!(
        app.device_root("emulated").join(".spindle").exists(),
        "端末のファイルには触らない"
    );
}

#[tokio::test]
async fn delete_refuses_a_running_sync_and_cancels_queued_device_jobs() {
    let app = App::new().await;
    app.connect("SER1");
    let id = app.register_ok("Xperia", "SER1", "emulated").await;
    let token = app.diff_token(id).await;
    let (s, v) = app
        .call(
            Method::POST,
            &format!("/api/devices/{id}/sync"),
            Some(json!({"plan_token": token})),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let sync = v["job_id"].as_i64().unwrap();
    let (s, v) = app
        .call(Method::POST, &format!("/api/devices/{id}/verify"), None)
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let verify = v["job_id"].as_i64().unwrap();
    let scan: i64 = app.query(
        "SELECT id FROM jobs WHERE type = 'device_scan' AND state = 'queued'",
        [],
    );
    // 実行中の同期（ワーカーは動いていない。状態だけを見る）
    app.raw_exec(&format!(
        "UPDATE jobs SET state = 'running' WHERE id = {sync}"
    ));
    let (s, v) = app
        .call(Method::DELETE, &format!("/api/devices/{id}"), None)
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("busy"))
    );
    assert_eq!(app.device_count(), 1);
    let state: String = app.query("SELECT state FROM jobs WHERE id = ?1", [verify]);
    assert_eq!(state, "queued", "断ったときは何も取り消さない");
    app.raw_exec(&format!(
        "UPDATE jobs SET state = 'queued' WHERE id = {sync}"
    ));
    let (s, v) = app
        .call(Method::DELETE, &format!("/api/devices/{id}"), None)
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    for job in [sync, verify, scan] {
        let state: String = app.query("SELECT state FROM jobs WHERE id = ?1", [job]);
        assert_eq!(state, "cancelled", "job {job}");
    }
}

#[tokio::test]
async fn delete_works_when_adb_is_disabled() {
    let app = App::new_without_adb().await;
    let id = app.insert_adb_device_with_open_plan().await;
    let (s, v) = app
        .call(Method::DELETE, &format!("/api/devices/{id}"), None)
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    assert_eq!(app.device_count(), 0);
}

async fn force_abandon(app: &App, id: i64) -> (StatusCode, Value) {
    app.call(
        Method::POST,
        &format!("/api/devices/{id}/plans/open/abandon"),
        Some(json!({"force": true})),
    )
    .await
}

#[tokio::test]
async fn force_abandon_closes_the_plan_while_disconnected() {
    let app = App::new().await;
    app.connect("SER1");
    let id = app.register_ok("Xperia", "SER1", "emulated").await;
    let token = app.diff_token(id).await;
    let (s, v) = app
        .call(
            Method::POST,
            &format!("/api/devices/{id}/sync"),
            Some(json!({"plan_token": token})),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let job = v["job_id"].as_i64().unwrap();
    app.disconnect();
    // force が無い・false なら今までどおり接続が要る
    for body in [None, Some(json!({})), Some(json!({"force": false}))] {
        let (s, v) = app
            .call(
                Method::POST,
                &format!("/api/devices/{id}/plans/open/abandon"),
                body,
            )
            .await;
        assert_eq!(
            (s, v["error"].as_str()),
            (StatusCode::CONFLICT, Some("not_connected"))
        );
    }
    let (s, v) = force_abandon(&app, id).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["plan_open"], false);
    assert_eq!(app.plan_state(id).as_deref(), Some("abandoned"));
    let state: String = app.query("SELECT state FROM jobs WHERE id = ?1", [job]);
    assert_eq!(state, "cancelled");
    let (s, v) = force_abandon(&app, id).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::NOT_FOUND, Some("no_open_plan"))
    );
}

#[tokio::test]
async fn force_abandon_works_with_unreadable_plan_json() {
    let app = App::new().await;
    app.connect("SER1");
    let id = app.register_ok("Xperia", "SER1", "emulated").await;
    app.confirm(id).await;
    app.raw_exec(r#"UPDATE device_sync_plans SET plan = '{"broken": true}'"#);
    app.disconnect();
    let (s, v) = force_abandon(&app, id).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(app.plan_state(id).as_deref(), Some("abandoned"));
}

#[tokio::test]
async fn force_abandon_refuses_a_running_sync_and_a_held_lock() {
    let app = App::new().await;
    app.connect("SER1");
    let id = app.register_ok("Xperia", "SER1", "emulated").await;
    let token = app.diff_token(id).await;
    let (_, v) = app
        .call(
            Method::POST,
            &format!("/api/devices/{id}/sync"),
            Some(json!({"plan_token": token})),
        )
        .await;
    let job = v["job_id"].as_i64().unwrap();
    app.disconnect();
    app.raw_exec(&format!(
        "UPDATE jobs SET state = 'running' WHERE id = {job}"
    ));
    let (s, v) = force_abandon(&app, id).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("busy"))
    );
    app.raw_exec(&format!(
        "UPDATE jobs SET state = 'queued' WHERE id = {job}"
    ));
    let lock = app.rt.device_lock(id);
    let g = lock.try_lock().unwrap();
    let (s, v) = force_abandon(&app, id).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("busy"))
    );
    drop(g);
    assert_eq!(app.plan_state(id).as_deref(), Some("open"));
    let (s, v) = force_abandon(&app, id).await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

#[tokio::test]
async fn force_abandon_works_when_adb_is_disabled() {
    let app = App::new_without_adb().await;
    let id = app.insert_adb_device_with_open_plan().await;
    let (s, v) = force_abandon(&app, id).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(app.plan_state(id).as_deref(), Some("abandoned"));
}

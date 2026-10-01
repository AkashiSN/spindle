//! エージェント API（`/api/agent/*`）の結合試験の準備。各試験ファイルが `mod agent_support;` で取り込む。
//! UI 側の呼び出し（ログイン済み Cookie 付き）と、エージェント側の呼び出し（Bearer のみ。
//! Cookie・Origin・Sec-Fetch-Site を付けない）を持つ。後のタスクは `App` にメソッドを足す
#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Request};
use axum::Router;
use http_body_util::BodyExt;
use tower::ServiceExt;

use rusqlite::params;
use spindle::api::{self, auth, AppState};
use spindle::config::{AacVariantConfig, Config, DerivedConfig};
use spindle::db::derived::{self as dbderived, Profiles, TagState};
use spindle::db::devices as dbdev;
use spindle::db::{now_epoch, Db};
use spindle::domain::derived::{expected_rel_path, Variant};
use spindle::domain::device::{
    delivery_token, dest_path, semantic_derived, sha256_hex, SourceHash, SourceKind, TrackInput,
};
use spindle::domain::relpath::{canonical_key, RelPath};
use spindle::fsroot::{self, RootDir};

pub use axum::http::{HeaderMap, HeaderName, Method, StatusCode};
pub use serde_json::{json, Value};

const EXAMPLE: &str = include_str!("../../deploy/config.example.toml");
/// 接続元（LAN。trusted_cidrs の扱いを試験に持ち込まないため、どの試験も同じピアから呼ぶ）
pub const LAN: &str = "192.168.1.23:50000";

pub struct App {
    pub router: Router,
    pub db: Arc<Db>,
    pub cookie: String,
    /// `with_roots` のときだけ持つ Library / Derived の root
    pub roots: Option<Roots>,
    _dir: tempfile::TempDir,
}

/// 試験用の Library と Derived の root（tempdir の下）
pub struct Roots {
    pub library: PathBuf,
    pub derived: PathBuf,
    pub library_root: Arc<RootDir>,
    pub derived_root: Arc<RootDir>,
}

/// `seed_track` で入れた曲の、manifest に載るはずの値
#[derive(Debug, Clone)]
pub struct SeededTrack {
    pub track_id: i64,
    /// Derived root 相対のパス（`aac/...m4a`）
    pub derived_rel: String,
    pub dest_path: String,
    pub token: String,
    pub sha256: String,
}

pub fn req(method: Method, uri: &str) -> axum::http::request::Builder {
    let peer: SocketAddr = LAN.parse().unwrap();
    let mut b = Request::builder().method(method).uri(uri);
    b.extensions_mut().unwrap().insert(ConnectInfo(peer));
    b
}

impl App {
    pub async fn new() -> Self {
        Self::build(Some("correct horse"), "", false).await
    }

    /// Library と Derived の root を tempdir に作り、aac 系統を有効にした App（曲を `seed_track` で入れる）
    pub async fn with_roots() -> Self {
        let app = Self::build(Some("correct horse"), "", true).await;
        app.db
            .write(|c| {
                let cfg = DerivedConfig {
                    opus: Default::default(),
                    aac: AacVariantConfig {
                        enabled: true,
                        ..Default::default()
                    },
                };
                dbderived::sync_variants(c, &cfg, false, 0)?;
                Ok(())
            })
            .await
            .unwrap();
        app
    }

    /// `[auth].trusted_cidrs` に接続元の LAN（192.168.1.0/24）を入れた App
    pub async fn with_trusted_lan() -> Self {
        Self::build(
            Some("correct horse"),
            r#"trusted_cidrs = ["192.168.1.0/24"]"#,
            false,
        )
        .await
    }

    /// パスワード未設定のロックモードの App（ログインできないので Cookie は空）
    pub async fn locked() -> Self {
        Self::build(None, "", false).await
    }

    /// `[auth]` を `auth_override` で上書きして組み立てる。パスワードがあればログインして Cookie を持つ
    async fn build(password: Option<&str>, auth_override: &str, with_roots: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&dir.path().join("spindle.db")).unwrap());
        let mut root: toml::Table = toml::from_str(EXAMPLE).unwrap();
        let patch: toml::Table = toml::from_str(auth_override).unwrap();
        let section = root.get_mut("auth").unwrap().as_table_mut().unwrap();
        for (k, v) in patch {
            section.insert(k, v);
        }
        let config = Arc::new(Config::parse(&toml::to_string(&root).unwrap()).unwrap());
        let mode = auth::bootstrap(&db, password.map(str::to_owned))
            .await
            .unwrap();
        let mut state = AppState::new(config, db.clone(), mode);
        let roots = with_roots.then(|| {
            let library = dir.path().join("library");
            let derived = dir.path().join("derived");
            std::fs::create_dir_all(&library).unwrap();
            std::fs::create_dir_all(&derived).unwrap();
            Roots {
                library_root: Arc::new(RootDir::open(&library).unwrap()),
                derived_root: Arc::new(RootDir::open(&derived).unwrap()),
                library,
                derived,
            }
        });
        if let Some(r) = &roots {
            state = state.with_roots(r.library_root.clone(), r.derived_root.clone());
        }
        let router = api::router(state);
        let Some(password) = password else {
            return Self {
                router,
                db,
                cookie: String::new(),
                roots,
                _dir: dir,
            };
        };
        let r = req(Method::POST, "/api/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(json!({ "password": password }).to_string()))
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
            cookie,
            roots,
            _dir: dir,
        }
    }

    /// UI からの呼び出し（セッション Cookie と sec-fetch-site 付き）
    pub async fn call(
        &self,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
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

    /// エージェントからの呼び出し。`token` があれば Bearer を付ける。Cookie・Origin・
    /// Sec-Fetch-Site は付けない
    pub async fn agent_call(
        &self,
        token: Option<&str>,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut r = req(method, uri);
        if let Some(t) = token {
            r = r.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
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

    /// エージェントからの呼び出し（本文なし・任意のヘッダ付き）。生の応答を返す。
    /// Cookie・Origin・Sec-Fetch-Site は呼び出し側が `headers` で明示しない限り付けない
    pub async fn agent_raw(
        &self,
        token: Option<&str>,
        method: Method,
        uri: &str,
        headers: &[(HeaderName, String)],
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut r = req(method, uri);
        if let Some(t) = token {
            r = r.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        for (k, v) in headers {
            r = r.header(k, v);
        }
        let res = self
            .router
            .clone()
            .oneshot(r.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let hdrs = res.headers().clone();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, hdrs, bytes.to_vec())
    }

    /// iPhone（Mac 経由。agent / aac / selection = all）を作って id を返す
    pub async fn create_iphone(&self, name: &str) -> i64 {
        let (st, v) = self
            .call(
                Method::POST,
                "/api/devices",
                Some(json!({"name": name, "transport": "agent", "variant": "aac", "selection": "all"})),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        v["id"].as_i64().unwrap()
    }

    /// UI からワンタイムコードを発行して返す
    pub async fn pair_code(&self, id: i64) -> String {
        let (st, v) = self
            .call(Method::POST, &format!("/api/devices/{id}/pair-code"), None)
            .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        v["code"].as_str().unwrap().to_owned()
    }

    /// コードを発行して pair し、トークンを返す
    pub async fn pair(&self, id: i64) -> String {
        let code = self.pair_code(id).await;
        let (st, v) = self
            .agent_call(
                None,
                Method::POST,
                "/api/agent/pair",
                Some(json!({"code": code})),
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        v["token"].as_str().unwrap().to_owned()
    }

    fn roots(&self) -> &Roots {
        self.roots
            .as_ref()
            .expect("App::with_roots で作った App で呼ぶ")
    }

    /// 曲を 1 つ入れる: Library の原本（FLAC 扱い）、aac の Derived の実ファイル（中身 `bytes`）と
    /// `derived_files` の行、その実ファイルの identity と一致する `source_hashes` の行。
    /// aac の端末からは desired（送れる曲）に見える
    pub async fn seed_track(&self, track_id: i64, rel_path: &str, bytes: &[u8]) -> SeededTrack {
        let roots = self.roots();
        let lib_path = roots.library.join(rel_path);
        std::fs::create_dir_all(lib_path.parent().unwrap()).unwrap();
        std::fs::write(&lib_path, b"FLAC-ORIGINAL").unwrap();
        let lib = fsroot::fstat(
            &roots
                .library_root
                .open_file(&RelPath::parse(rel_path).unwrap())
                .unwrap(),
        )
        .unwrap();
        let derived_rel = expected_rel_path(Variant::Aac, rel_path);
        let d_path = roots.derived.join(&derived_rel);
        std::fs::create_dir_all(d_path.parent().unwrap()).unwrap();
        std::fs::write(&d_path, bytes).unwrap();
        let st = fsroot::fstat(
            &roots
                .derived_root
                .open_file(&RelPath::parse(&derived_rel).unwrap())
                .unwrap(),
        )
        .unwrap();
        let sha256 = sha256_hex(bytes);
        let (rel, drel, sha) = (rel_path.to_owned(), derived_rel.clone(), sha256.clone());
        let semantic = self
            .db
            .write(move |c| {
                c.execute(
                    "INSERT INTO tracks (id, rel_path, rel_path_key, inode, size, mtime_ns, ctime_ns,
                                         codec, lossless, channels, audio_version, tag_version, seen_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'flac', 1, 2, 1, 1, 0)",
                    params![
                        track_id,
                        rel,
                        canonical_key(&rel),
                        lib.inode as i64,
                        lib.size as i64,
                        lib.mtime_ns,
                        lib.ctime_ns
                    ],
                )?;
                let settings = dbderived::settings_of(c, Variant::Aac)?.unwrap();
                dbderived::upsert(
                    c,
                    track_id,
                    Variant::Aac,
                    &drel,
                    Some(256),
                    1,
                    TagState {
                        src_tag_version: 1,
                        src_artwork_id: None,
                        src_rg_scanned_at: None,
                    },
                    &Profiles::of(&settings),
                    0,
                )?;
                let row = dbderived::get(c, track_id, Variant::Aac)?.unwrap();
                let semantic = semantic_derived(Variant::Aac, &row);
                let h = SourceHash {
                    semantic: semantic.clone(),
                    inode: st.inode,
                    size: st.size,
                    mtime_ns: st.mtime_ns,
                    ctime_ns: st.ctime_ns,
                    sha256: sha,
                };
                dbdev::put_source_hash(c, track_id, SourceKind::Derived(Variant::Aac), &h, 0)?;
                Ok(semantic)
            })
            .await
            .unwrap();
        let input = TrackInput {
            track_id,
            rel_path: rel_path.to_owned(),
            lossless: true,
            channels: Some(2),
            audio_version: 1,
            tag_version: 1,
            rg_ready: false,
            derived: None,
            hash_master: None,
            hash_derived: None,
        };
        SeededTrack {
            track_id,
            derived_rel,
            dest_path: dest_path(&input, SourceKind::Derived(Variant::Aac)),
            token: delivery_token(&semantic, &sha256),
            sha256,
        }
    }

    /// 手動プレイリストを作り（`track_ids` の順）、端末の印に足す
    pub async fn seed_playlist(
        &self,
        device_id: i64,
        playlist_id: i64,
        name: &str,
        track_ids: &[i64],
    ) {
        let (name, ids) = (name.to_owned(), track_ids.to_vec());
        self.db
            .write(move |c| {
                c.execute(
                    "INSERT INTO playlists (id, name, name_key, kind, created_at, updated_at)
                     VALUES (?1, ?2, ?3, 'manual', 0, 0)",
                    params![playlist_id, name, canonical_key(&name)],
                )?;
                for (pos, tid) in ids.iter().enumerate() {
                    c.execute(
                        "INSERT INTO playlist_items (playlist_id, position, track_id) VALUES (?1, ?2, ?3)",
                        params![playlist_id, pos as i64, tid],
                    )?;
                }
                let mut marked = dbdev::playlist_ids(c, device_id)?;
                marked.push(playlist_id);
                dbdev::set_playlists(c, device_id, &marked, now_epoch())?;
                Ok(())
            })
            .await
            .unwrap();
    }
}

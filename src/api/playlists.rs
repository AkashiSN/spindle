//! `/api/playlists`（SPEC §9 / §10、docs/TASKS.md P1-6、D-53）。
//!
//! - `GET /api/playlists`、`POST { name, rule? }`（rule があれば smart）、`GET /:id`、
//!   `PATCH /:id { name?, auto_export?, rule? }`、`DELETE /:id`
//! - `POST /api/playlists/preview { rule }`（保存せずに検証と件数）、`POST /:id/refresh`（smart の再評価）
//! - `POST /:id/items { selection, sort? }`（末尾に追加。ids 形は送られた順、filter 形は sort 順）、
//!   `DELETE /:id/items { track_ids?, selection? }`、`POST /:id/items/move { track_ids, before }`
//! - `GET /:id/export?profile=` は m3u8 の本文（trusted CIDR の allowlist 対象）、`POST` は
//!   Playlists root の `<profile>/<name>.m3u8` へ書いて `playlist_exports` に記録する
//! - `GET /api/playlists/import` は Playlists root 下の m3u / m3u8 の一覧、`POST { path, name? }` は
//!   その 1 本を解決して手動プレイリストを作る（未解決行は応答に返すだけ）
//!
//! 一覧・並べ替えは `GET /api/tracks?filter={"playlist_id":N}&sort=position` で表と共通

use std::collections::HashSet;
use std::io::Read;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db::playlists::{self as dbpl, MoveError, Playlist, Rename};
use crate::db::{now_epoch, tracks};
use crate::domain::filter::Sort;
use crate::domain::relpath::{RelPath, RelPathError};
use crate::domain::selection::SelectionBody;
use crate::fsroot::{FileKind, FsError, RootDir};
use crate::jobs::{Event, PlaylistEvent};
use crate::playlist::compile;
use crate::playlist::dsl;
use crate::playlist::export::EXPORT_EXT;
use crate::playlist::import::{parse_m3u8, resolve_entries, Resolver};
use crate::playlist::smart;
use crate::playlist::writer;

use super::error::{error_response, error_response_with_message, ApiError};
use super::selection::SelectionError;
use super::AppState;

/// 取り込み対象と見なす拡張子（小文字比較）
const IMPORT_EXTENSIONS: [&str; 2] = ["m3u8", "m3u"];
/// 取り込みで読む m3u8 の上限（旧ライブラリの最大は 700KB。桁違いに大きい物は誤り）
const IMPORT_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// 取り込みの一覧で辿る深さの上限（root 直下 + 数段で足りる）
const IMPORT_MAX_DEPTH: usize = 4;

#[derive(Serialize)]
pub struct PlaylistList {
    pub items: Vec<Playlist>,
}

#[derive(Deserialize)]
pub struct CreateBody {
    pub name: String,
    /// あればスマートプレイリスト（docs/DSL.md）。構文・型エラーは 400
    #[serde(default)]
    pub rule: Option<String>,
}

#[derive(Deserialize)]
pub struct PatchBody {
    pub name: Option<String>,
    pub auto_export: Option<bool>,
    /// smart のルール差し替え（再評価する）。manual に付けると 409 `manual`
    #[serde(default)]
    pub rule: Option<String>,
}

#[derive(Deserialize)]
pub struct PreviewBody {
    pub rule: String,
}

#[derive(Serialize)]
pub struct Previewed {
    /// 評価結果の件数（ORDER BY / LIMIT 適用後）
    pub count: usize,
    pub ast: dsl::Rule,
}

#[derive(Serialize)]
pub struct Refreshed {
    pub count: usize,
    /// 項目が書き換わったか
    pub changed: bool,
}

#[derive(Deserialize)]
pub struct AppendBody {
    pub selection: SelectionBody,
    /// filter 形の並び（一覧と同じ `sort`）。省略時は既定ソート
    pub sort: Option<String>,
}

#[derive(Deserialize)]
pub struct RemoveBody {
    /// 明示の id 列。`selection` と併用可（和集合）
    #[serde(default)]
    pub track_ids: Vec<i64>,
    /// 一覧の selection（Ctrl+A のフィルタ形をそのまま送れる）
    #[serde(default)]
    pub selection: Option<SelectionBody>,
}

#[derive(Deserialize)]
pub struct MoveBody {
    pub track_ids: Vec<i64>,
    /// この直前へ。`null` は末尾
    #[serde(default)]
    pub before: Option<i64>,
}

#[derive(Deserialize)]
pub struct ExportQuery {
    pub profile: Option<String>,
}

#[derive(Serialize)]
pub struct Exported {
    pub out_path: String,
    pub count: usize,
    pub skipped_missing: usize,
}

#[derive(Serialize)]
pub struct ImportCandidateList {
    pub items: Vec<ImportCandidate>,
}

#[derive(Serialize)]
pub struct ImportCandidate {
    /// Playlists root からの相対パス
    pub path: String,
    pub size: u64,
}

#[derive(Deserialize)]
pub struct ImportBody {
    pub path: String,
    /// 省略時はファイル名の stem
    pub name: Option<String>,
}

#[derive(Serialize)]
pub struct Imported {
    pub playlist: Playlist,
    pub matched: usize,
    pub duplicates: usize,
    pub unresolved: Vec<String>,
}

// ---------------------------------------------------------------- 名前

/// プレイリスト名はそのまま書き出しファイル名になる。名前単体と `<name>.m3u8` の両方を単一の
/// パス要素として検証する（D-53。長さの上限は拡張子込み）
fn validate_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("name が空".to_owned());
    }
    if name.contains('/') {
        return Err("name に `/` は使えない".to_owned());
    }
    for candidate in [name.to_owned(), format!("{name}{EXPORT_EXT}")] {
        match RelPath::parse(&candidate) {
            Ok(_) => {}
            Err(e @ RelPathError::ComponentTooLong) => {
                return Err(format!("name が長すぎる（{EXPORT_EXT} 込み）: {e}"))
            }
            Err(e) => return Err(format!("name が不正: {e}")),
        }
    }
    Ok(name.to_owned())
}

fn bad_request(msg: impl Into<String>) -> Response {
    error_response_with_message(StatusCode::BAD_REQUEST, "bad_request", msg)
}

fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not_found")
}

fn duplicate() -> Response {
    error_response(StatusCode::CONFLICT, "duplicate")
}

fn unavailable() -> Response {
    error_response(StatusCode::SERVICE_UNAVAILABLE, "playlists_unavailable")
}

/// ルールの実行時の失敗（`DbError::Rule`）。トランザクションは巻き戻し済み
fn rule_failed(msg: String) -> Response {
    error_response_with_message(StatusCode::BAD_REQUEST, "rule_failed", msg)
}

/// `Db` の結果のうち `DbError::Rule` だけを 400 に写す（それ以外は `?` で 500）
fn rule_or<T>(r: Result<T, crate::db::DbError>) -> Result<Result<T, Response>, ApiError> {
    match r {
        Ok(v) => Ok(Ok(v)),
        Err(crate::db::DbError::Rule(m)) => Ok(Err(rule_failed(m))),
        Err(e) => Err(e.into()),
    }
}

fn smart_only() -> Response {
    error_response(StatusCode::CONFLICT, "manual")
}

fn manual_only() -> Response {
    error_response(StatusCode::CONFLICT, "smart")
}

/// ルールを構文・型まで検証して AST とその JSON を返す。失敗は 400（位置付き）
fn parse_rule(src: &str) -> Result<(dsl::Rule, String), Box<Response>> {
    let rule = dsl::parse(src).map_err(|e| Box::new(bad_request(format!("rule が不正: {e}"))))?;
    compile::check(&rule).map_err(|e| Box::new(bad_request(format!("rule が不正: {e}"))))?;
    let json = serde_json::to_string(&rule)
        .map_err(|e| Box::new(bad_request(format!("rule を保存できない: {e}"))))?;
    Ok((rule, json))
}

/// 項目操作の前に kind を見る。manual でなければ応答（404 / 409）
async fn require_manual(state: &AppState, id: i64) -> Result<(), Box<Response>> {
    match state.db.read(move |c| dbpl::kind(c, id)).await {
        Ok(Some(k)) if k == "manual" => Ok(()),
        Ok(Some(_)) => Err(Box::new(manual_only())),
        Ok(None) => Err(Box::new(not_found())),
        Err(e) => Err(Box::new(ApiError::from(e).into_response())),
    }
}

fn selection_error(e: SelectionError) -> Result<Response, ApiError> {
    match e {
        SelectionError::Filter(e) => Ok(bad_request(e.to_string())),
        SelectionError::Api(e) => Err(e),
    }
}

// ---------------------------------------------------------------- CRUD

pub async fn list(State(state): State<AppState>) -> Result<Json<PlaylistList>, ApiError> {
    let items = state.db.read(dbpl::list).await?;
    Ok(Json(PlaylistList { items }))
}

pub async fn get(State(state): State<AppState>, Path(id): Path<i64>) -> Result<Response, ApiError> {
    let row = state.db.read(move |c| dbpl::get(c, id)).await?;
    Ok(match row {
        Some(p) => Json(p).into_response(),
        None => not_found(),
    })
}

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateBody>,
) -> Result<Response, ApiError> {
    let name = match validate_name(&body.name) {
        Ok(n) => n,
        Err(m) => return Ok(bad_request(m)),
    };
    let rule = match body
        .rule
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        Some(src) => match parse_rule(src) {
            Ok((rule, json)) => Some((src.to_owned(), rule, json)),
            Err(r) => return Ok(*r),
        },
        None => None,
    };
    // 作成と評価は 1 トランザクション（評価が失敗すれば空の smart 行を残さない）
    let row = match rule_or(
        state
            .db
            .transaction(move |c| {
                let now = now_epoch();
                match rule {
                    None => dbpl::create(c, &name, now),
                    Some((src, rule, json)) => {
                        let Some(p) = dbpl::create_smart(c, &name, &src, &json, now)? else {
                            return Ok(None);
                        };
                        smart::refresh_one(c, p.id, &rule, now)?;
                        dbpl::get(c, p.id)
                    }
                }
            })
            .await,
    )? {
        Ok(row) => row,
        Err(r) => return Ok(r),
    };
    Ok(match row {
        Some(p) => (StatusCode::CREATED, Json(p)).into_response(),
        None => duplicate(),
    })
}

/// `POST /api/playlists/preview { rule }`: 保存せずに構文検証と評価件数
pub async fn preview(
    State(state): State<AppState>,
    Json(body): Json<PreviewBody>,
) -> Result<Response, ApiError> {
    let (rule, _) = match parse_rule(body.rule.trim()) {
        Ok(v) => v,
        Err(r) => return Ok(*r),
    };
    let ast = rule.clone();
    let count = match rule_or(state.db.read(move |c| smart::evaluate(c, &rule)).await)? {
        Ok(ids) => ids.len(),
        Err(r) => return Ok(r),
    };
    Ok(Json(Previewed { count, ast }).into_response())
}

/// `POST /api/playlists/:id/refresh`: smart を今の DB で再評価する
pub async fn refresh(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let result = match rule_or(
        state
            .db
            .transaction(move |c| {
                let Some(kind) = dbpl::kind(c, id)? else {
                    return Ok(None);
                };
                if kind != "smart" {
                    return Ok(Some(None));
                }
                let Some(rule) = smart::load_rule(c, id)? else {
                    return Ok(None);
                };
                let (count, changed) = smart::refresh_one(c, id, &rule, now_epoch())?;
                Ok(Some(Some(Refreshed { count, changed })))
            })
            .await,
    )? {
        Ok(r) => r,
        Err(r) => return Ok(r),
    };
    Ok(match result {
        None => not_found(),
        Some(None) => smart_only(),
        Some(Some(r)) => {
            if r.changed {
                state.jobs.publish(Event::Playlist(PlaylistEvent {
                    playlist_ids: vec![id],
                }));
            }
            Json(r).into_response()
        }
    })
}

pub async fn patch(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<PatchBody>,
) -> Result<Response, ApiError> {
    let name = match body.name.as_deref().map(validate_name) {
        Some(Ok(n)) => Some(n),
        Some(Err(m)) => return Ok(bad_request(m)),
        None => None,
    };
    let auto_export = body.auto_export;
    let rule = match body.rule.as_deref().map(str::trim) {
        Some(src) => match parse_rule(src) {
            Ok((rule, json)) => Some((src.to_owned(), rule, json)),
            Err(r) => return Ok(*r),
        },
        None => None,
    };
    // 全フィールドを 1 トランザクションで。どれかが通らなければ何も変えない
    let result = match rule_or(
        state
            .db
            .transaction(move |c| {
                let now = now_epoch();
                let Some(kind) = dbpl::kind(c, id)? else {
                    return Ok(Err(PatchFail::Rename(Rename::NotFound)));
                };
                if rule.is_some() && kind != "smart" {
                    return Ok(Err(PatchFail::Manual));
                }
                if let Some(n) = &name {
                    match dbpl::rename(c, id, n, now)? {
                        Rename::Ok => {}
                        other => return Ok(Err(PatchFail::Rename(other))),
                    }
                }
                if let Some(on) = auto_export {
                    if !dbpl::set_auto_export(c, id, on, now)? {
                        return Ok(Err(PatchFail::Rename(Rename::NotFound)));
                    }
                }
                let mut changed = false;
                if let Some((src, rule, json)) = rule {
                    dbpl::set_rule(c, id, &src, &json, now)?;
                    changed = smart::refresh_one(c, id, &rule, now)?.1;
                }
                Ok(Ok((dbpl::get(c, id)?, changed)))
            })
            .await,
    )? {
        Ok(r) => r,
        Err(r) => return Ok(r),
    };
    Ok(match result {
        Ok((Some(p), changed)) => {
            if changed {
                state.jobs.publish(Event::Playlist(PlaylistEvent {
                    playlist_ids: vec![id],
                }));
            }
            Json(p).into_response()
        }
        Ok((None, _)) | Err(PatchFail::Rename(Rename::NotFound)) => not_found(),
        Err(PatchFail::Rename(Rename::Duplicate)) => duplicate(),
        Err(PatchFail::Rename(Rename::Ok)) => unreachable_response(),
        Err(PatchFail::Manual) => smart_only(),
    })
}

enum PatchFail {
    Rename(Rename),
    Manual,
}

/// 型の上で到達しうるが論理的に起きない分岐。500 にせず 404 に倒す
fn unreachable_response() -> Response {
    not_found()
}

pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let deleted = state.db.write(move |c| dbpl::delete(c, id)).await?;
    Ok(if deleted {
        StatusCode::NO_CONTENT.into_response()
    } else {
        not_found()
    })
}

// ---------------------------------------------------------------- 項目

pub async fn append(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<AppendBody>,
) -> Result<Response, ApiError> {
    if let Err(r) = require_manual(&state, id).await {
        return Ok(*r);
    }
    let sort = match body.sort.as_deref().map(Sort::parse) {
        Some(Ok(s)) => Some(s),
        Some(Err(e)) => return Ok(bad_request(e.to_string())),
        None => None,
    };
    // ids 形は送られた順を保つ（DB は id 順で返す）
    let given_order = match &body.selection {
        SelectionBody::Ids { ids } => Some(ids.clone()),
        SelectionBody::Filter { .. } => None,
    };
    let given_order_len = given_order.as_ref().map(Vec::len);
    let sel = match body.selection.parse() {
        Ok(s) => s,
        Err(e) => return Ok(bad_request(e.to_string())),
    };
    let rows = match state
        .db
        .read(move |c| tracks::resolve_selection_sorted(c, &sel, sort))
        .await
    {
        Ok(rows) => rows,
        Err(e) => return selection_error(e.into()),
    };
    let track_ids: Vec<i64> = match given_order {
        Some(order) => {
            let present: HashSet<i64> = rows.iter().map(|r| r.id).collect();
            let mut seen = HashSet::new();
            order
                .into_iter()
                .filter(|id| present.contains(id) && seen.insert(*id))
                .collect()
        }
        None => rows.iter().map(|r| r.id).collect(),
    };
    // skipped は入力の件数基準（存在しない id も数える）
    let input_len = match &given_order_len {
        Some(n) => *n,
        None => track_ids.len(),
    };
    let result = state
        .db
        .transaction(move |c| dbpl::append(c, id, &track_ids, now_epoch()))
        .await?;
    Ok(match result {
        Some(a) => Json(serde_json::json!({
            "added": a.added,
            "skipped": input_len - a.added,
        }))
        .into_response(),
        None => not_found(),
    })
}

pub async fn remove(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<RemoveBody>,
) -> Result<Response, ApiError> {
    if let Err(r) = require_manual(&state, id).await {
        return Ok(*r);
    }
    if body.track_ids.is_empty() && body.selection.is_none() {
        return Ok(bad_request("track_ids か selection が必要"));
    }
    let mut track_ids = body.track_ids;
    if let Some(sel) = body.selection {
        let sel = match sel.parse() {
            Ok(s) => s,
            Err(e) => return Ok(bad_request(e.to_string())),
        };
        let rows = match state
            .db
            .read(move |c| tracks::resolve_selection(c, &sel))
            .await
        {
            Ok(rows) => rows,
            Err(e) => return selection_error(e.into()),
        };
        track_ids.extend(rows.iter().map(|r| r.id));
    }
    let result = state
        .db
        .transaction(move |c| dbpl::remove(c, id, &track_ids, now_epoch()))
        .await?;
    Ok(match result {
        Some(n) => Json(serde_json::json!({ "removed": n })).into_response(),
        None => not_found(),
    })
}

pub async fn move_items(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<MoveBody>,
) -> Result<Response, ApiError> {
    if let Err(r) = require_manual(&state, id).await {
        return Ok(*r);
    }
    let result = state
        .db
        .transaction(move |c| dbpl::move_items(c, id, &body.track_ids, body.before, now_epoch()))
        .await?;
    Ok(match result {
        Some(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Some(Err(e @ (MoveError::BeforeNotInPlaylist | MoveError::BeforeInMovedSet))) => {
            bad_request(e.to_string())
        }
        None => not_found(),
    })
}

// ---------------------------------------------------------------- 書き出し

fn render_error(e: writer::RenderError) -> Result<Response, ApiError> {
    Ok(match e {
        writer::RenderError::NoProfile => bad_request("profile が不明"),
        writer::RenderError::NotFound => not_found(),
        writer::RenderError::BadFormat(f) => {
            bad_request(format!("format={f} のプロファイルは m3u8 を書けない"))
        }
        writer::RenderError::Db(e) => return Err(e.into()),
    })
}

fn profile_name(q: ExportQuery) -> Result<String, Box<Response>> {
    q.profile
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
        .ok_or_else(|| Box::new(bad_request("profile が必要")))
}

/// `GET /api/playlists/:id/export?profile=`: 本文を返す（ファイルは書かない）
pub async fn export_get(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<ExportQuery>,
) -> Result<Response, ApiError> {
    let profile = match profile_name(q) {
        Ok(p) => p,
        Err(r) => return Ok(*r),
    };
    let r = match writer::render(&state.db, id, &profile).await {
        Ok(r) => r,
        Err(e) => return render_error(e),
    };
    let disposition = format!(
        "attachment; filename*=UTF-8''{}",
        percent_encode(&format!("{}{EXPORT_EXT}", r.playlist.name))
    );
    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "audio/x-mpegurl; charset=utf-8".to_owned(),
            ),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        r.body,
    )
        .into_response())
}

/// RFC 5987 の `filename*` 用（unreserved 以外をパーセントエンコード）
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `POST /api/playlists/:id/export?profile=`: `Playlists/<profile>/<name>.m3u8` に tmp + rename で
/// 書き、`playlist_exports` に記録する（`playlist::writer`）
pub async fn export_post(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<ExportQuery>,
) -> Result<Response, ApiError> {
    let Some(root) = state.playlists.clone() else {
        return Ok(unavailable());
    };
    let profile = match profile_name(q) {
        Ok(p) => p,
        Err(r) => return Ok(*r),
    };
    match writer::export_to_root(&state.db, root, id, &profile).await {
        Ok(w) => Ok(Json(Exported {
            out_path: w.out_path,
            count: w.count,
            skipped_missing: w.skipped_missing,
        })
        .into_response()),
        Err(writer::WriteError::Render(e)) => render_error(e),
        Err(writer::WriteError::BadPath(m)) => Ok(bad_request(m)),
        Err(e) => Err(ApiError::Internal(e.to_string())),
    }
}

// ---------------------------------------------------------------- 取り込み

fn is_import_file(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| {
        IMPORT_EXTENSIONS
            .iter()
            .any(|e| ext.eq_ignore_ascii_case(e))
    })
}

/// Playlists root 下の m3u / m3u8 を深さ優先で集める（symlink は辿らない）
fn list_import_files(root: &RootDir) -> Result<Vec<ImportCandidate>, FsError> {
    let mut out = Vec::new();
    let mut stack: Vec<(Option<RelPath>, usize)> = vec![(None, 0)];
    while let Some((dir, depth)) = stack.pop() {
        let entries = match root.read_dir(dir.as_ref()) {
            Ok(e) => e,
            // 途中で消えたディレクトリは飛ばす
            Err(FsError::NotFound) => continue,
            Err(e) => return Err(e),
        };
        for entry in entries {
            let Some(name) = entry.name.to_str() else {
                continue;
            };
            let rel = match &dir {
                Some(d) => d.join(name),
                None => RelPath::parse(name),
            };
            let Ok(rel) = rel else { continue };
            match entry.kind {
                FileKind::Dir if depth < IMPORT_MAX_DEPTH => stack.push((Some(rel), depth + 1)),
                FileKind::File if is_import_file(name) => {
                    let size = root.stat(&rel).map(|s| s.size).unwrap_or(0);
                    out.push(ImportCandidate {
                        path: rel.as_str().to_owned(),
                        size,
                    });
                }
                _ => {}
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// `GET /api/playlists/import`
pub async fn import_list(State(state): State<AppState>) -> Result<Response, ApiError> {
    let Some(root) = state.playlists.clone() else {
        return Ok(unavailable());
    };
    let items = tokio::task::spawn_blocking(move || list_import_files(&root))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .map_err(|e| ApiError::Internal(format!("Playlists を読めない: {e}")))?;
    Ok(Json(ImportCandidateList { items }).into_response())
}

/// `POST /api/playlists/import { path, name? }`
pub async fn import_create(
    State(state): State<AppState>,
    Json(body): Json<ImportBody>,
) -> Result<Response, ApiError> {
    let Some(root) = state.playlists.clone() else {
        return Ok(unavailable());
    };
    let rel = match RelPath::parse(&body.path) {
        Ok(r) => r,
        Err(e) => return Ok(bad_request(format!("path が不正: {e}"))),
    };
    if !is_import_file(rel.file_name()) {
        return Ok(bad_request("path は .m3u8 / .m3u でなければならない"));
    }
    let stem = rel
        .file_name()
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(rel.file_name())
        .to_owned();
    let name = match validate_name(body.name.as_deref().unwrap_or(&stem)) {
        Ok(n) => n,
        Err(m) => return Ok(bad_request(m)),
    };
    let read_rel = rel.clone();
    let text =
        tokio::task::spawn_blocking(move || -> Result<Result<String, FsError>, std::io::Error> {
            let mut f = match root.open_file(&read_rel) {
                Ok(f) => f,
                Err(e) => return Ok(Err(e)),
            };
            let len = f.metadata()?.len();
            if len > IMPORT_MAX_BYTES {
                return Err(std::io::Error::other(format!(
                    "{len} バイトは取り込みの上限 {IMPORT_MAX_BYTES} を超える"
                )));
            }
            let mut buf = Vec::with_capacity(len as usize);
            f.read_to_end(&mut buf)?;
            Ok(Ok(String::from_utf8_lossy(&buf).into_owned()))
        })
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let text = match text {
        Ok(Ok(t)) => t,
        Ok(Err(FsError::NotFound)) => return Ok(not_found()),
        Ok(Err(FsError::Symlink | FsError::Escaped)) => {
            return Ok(bad_request("path が root の外を指す"))
        }
        Ok(Err(e)) => return Err(ApiError::Internal(format!("{} を読めない: {e}", rel))),
        Err(e) => return Ok(bad_request(e.to_string())),
    };
    let entries = parse_m3u8(&text);
    let path_for_log = rel.as_str().to_owned();
    let result = state
        .db
        .transaction(move |c| {
            let now = now_epoch();
            let resolver = Resolver::new(dbpl::import_candidates(c)?);
            let resolved = resolve_entries(&resolver, entries);
            let Some(created) = dbpl::create(c, &name, now)? else {
                return Ok(None);
            };
            dbpl::append(c, created.id, &resolved.track_ids, now)?;
            let playlist = dbpl::get(c, created.id)?.unwrap_or(created);
            Ok(Some((playlist, resolved)))
        })
        .await?;
    Ok(match result {
        None => duplicate(),
        Some((playlist, resolved)) => {
            tracing::info!(
                path = %path_for_log,
                playlist = %playlist.name,
                matched = resolved.track_ids.len(),
                duplicates = resolved.duplicates,
                unresolved = resolved.unresolved.len(),
                "m3u8 を取り込んだ"
            );
            (
                StatusCode::CREATED,
                Json(Imported {
                    playlist,
                    matched: resolved.track_ids.len(),
                    duplicates: resolved.duplicates,
                    unresolved: resolved.unresolved,
                }),
            )
                .into_response()
        }
    })
}

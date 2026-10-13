//! `pair --adopt`: サーバの DB 全損のあと、この Mac の保存先を新しい登録へ引き継ぐ
//! （docs/superpowers/specs/2026-10-13-p5-8-agent-adopt-design.md 2）。
//! pair で行の uuid を前の登録へ差し替え、`adopt_pending` を保存してから `adopt` で id を付け替える。
//! その間で落ちたら、次の `sync`（または `pair --adopt` のやり直し）が `finish` で続きを行う。
//! 付け替えで外れた曲・プレイリストは state から外すだけで、ファイルもミュージック.app の項目も消さない

use std::collections::BTreeMap;

use agent_proto::{AdoptPlaylist, AdoptRequest, AdoptTrack, IdMap};

use crate::ctx::Ctx;
use crate::music::Music;
use crate::pair;
use crate::recover;
use crate::secrets::Secrets;
use crate::server::Server;
use crate::state::{SetupPhase, ADOPTED_TOKEN, STALE_TOKEN};
use crate::{Error, Result};

const NO_PREVIOUS: &str = "引き継ぐ前の登録がありません。--adopt を付けずに pair してください";
const PENDING_REMAINS: &str =
    "途中の操作が残っています。spindle-agent status で確かめ、片付けてから pair --adopt し直してください";

/// 引き継ぎの件数
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AdoptSummary {
    pub tracks_adopted: usize,
    pub tracks_dropped: usize,
    pub playlists_adopted: usize,
    pub playlists_dropped: usize,
}

/// pair --adopt: 前提 → 回復 → pair(adopt_uuid) → adopt_pending を保存 → finish
pub fn pair_adopt<M: Music, S: Server>(
    cx: &mut Ctx<'_, M, S>,
    secrets: &dyn Secrets,
    url: &str,
    insecure_http: bool,
    code: &str,
) -> Result<AdoptSummary> {
    pair::check_copy_setting(cx)?;
    pair::check_root_not_media_folder(cx)?;
    // ワンタイムコードを使う前に Automation の許可とトークンの置き場を確かめる（pair と同じ）
    cx.music.probe()?;
    secrets.check_writable()?;
    let done = cx
        .state
        .setup
        .as_ref()
        .is_some_and(|s| s.phase == SetupPhase::Done);
    let uuid = match (&cx.state.server, done) {
        (Some(s), true) => s.device_uuid.clone(),
        _ => return Err(Error::Stop(NO_PREVIOUS.to_owned())),
    };
    // root の印が前の登録のものであること（sync と同じ照合。別の端末の root ならコードを使わずに止める）
    crate::sync::check_marker(cx)?;
    // ローカルの回復はサーバを使わない。片付かない途中の操作があれば、コードを使わずに止める
    recover::recover(cx)?;
    if !cx.state.pending_ops.is_empty() || !cx.state.pending_batches.is_empty() {
        return Err(Error::Stop(PENDING_REMAINS.to_owned()));
    }
    let resp = cx.server.pair(code, Some(&uuid))?;
    if resp.device_uuid != uuid {
        return Err(Error::Stop(format!(
            "spindle が別の端末（{}）として登録しました。引き継げません。spindle の端末タブで確かめてください",
            resp.device_name
        )));
    }
    secrets.set(&resp.token)?;
    // 続く adopt はこのトークンで送る（CLI の pair はトークン無しのサーバで始まる）
    cx.server.use_token(&resp.token)?;
    if let Some(s) = cx.state.server.as_mut() {
        s.url = url.to_owned();
        s.insecure_http = insecure_http;
        s.device_name = resp.device_name;
    }
    cx.state.adopt_pending = true;
    cx.save()?;
    let summary = finish(cx)?;
    Ok(summary.unwrap_or_default())
}

/// adopt_pending なら adopt を呼んで state を付け替え、adopt_pending を消す（1 回の保存）
pub fn finish<M: Music, S: Server>(cx: &mut Ctx<'_, M, S>) -> Result<Option<AdoptSummary>> {
    if !cx.state.adopt_pending {
        return Ok(None);
    }
    let req = AdoptRequest {
        tracks: cx
            .state
            .tracks
            .iter()
            .map(|(id, e)| AdoptTrack {
                track_id: *id,
                path: e.path.clone(),
                rel_path: e.rel_path.clone(),
                source: e.source.clone(),
                size: e.size,
                sha256: e.sha256.clone(),
                stale: e.token == STALE_TOKEN,
            })
            .collect(),
        playlists: cx
            .state
            .playlists
            .iter()
            .map(|(id, e)| AdoptPlaylist {
                playlist_id: *id,
                name: e.name.clone(),
            })
            .collect(),
    };
    let resp = cx.server.adopt(&req)?;
    let tracks_map = id_map(&resp.tracks);
    let playlists_map = id_map(&resp.playlists);
    let mut summary = AdoptSummary::default();

    let mut tracks = BTreeMap::new();
    for (id, e) in &cx.state.tracks {
        let mut e = e.clone();
        match tracks_map.get(id) {
            Some(&to) => {
                if e.token != STALE_TOKEN {
                    e.token = ADOPTED_TOKEN.to_owned();
                }
                if tracks.insert(to, e).is_some() {
                    return Err(duplicate_target("曲", to));
                }
                summary.tracks_adopted += 1;
            }
            // 管理外にするだけ（ファイルとミュージック.app の track は残す）
            None => summary.tracks_dropped += 1,
        }
    }
    let mut playlists = BTreeMap::new();
    for (id, e) in &cx.state.playlists {
        let mut e = e.clone();
        match playlists_map.get(id) {
            Some(&to) => {
                // サーバの device_playlist_state と同じく STALE_TOKEN（次の差分で中身を入れ直す）
                e.token = STALE_TOKEN.to_owned();
                if playlists.insert(to, e).is_some() {
                    return Err(duplicate_target("プレイリスト", to));
                }
                summary.playlists_adopted += 1;
            }
            None => summary.playlists_dropped += 1,
        }
    }
    // 付け替え表を全部確かめてから差し替える（途中で止めても Ctx の state は元のまま）
    cx.state.tracks = tracks;
    cx.state.playlists = playlists;
    cx.state.plan_id = None;
    cx.state.needs_report = false;
    cx.state.adopt_pending = false;
    cx.save()?;
    Ok(Some(summary))
}

/// 2 つの項目が同じ id へ付け替えられた（サーバは曖昧なものを外すので起きない）。state は書かない
fn duplicate_target(kind: &str, to: i64) -> Error {
    Error::Server(format!(
        "引き継ぎの付け替え表で、2 つの{kind}が同じ id（{to}）を指しています"
    ))
}

/// 付け替え表の `from → to`（`to` が無いもの = 外すものは入れない）
fn id_map(m: &[IdMap]) -> BTreeMap<i64, i64> {
    m.iter()
        .filter_map(|x| x.to.map(|to| (x.from, to)))
        .collect()
}

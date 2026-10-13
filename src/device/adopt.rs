//! DB 全損後の保存先の引き継ぎ（仕様 ⑤「引き継ぎ」、D-103）。manifest の `rel_path` で今の `tracks` を
//! 引いて `track_id` を付け替え（remap）、プレイリストは名前で付け替える。引けない・曖昧な項目は
//! manifest から外して管理外にする（ファイルは消さない）

use std::collections::{HashMap, HashSet};

use crate::device::ondevice::{DeviceManifest, ManifestItem, ManifestPlaylist};
use crate::device::recover::{recover, Expect, RecoverError, STALE_TOKEN};
use crate::device::remote::{DeviceFs, RemoteError};
use crate::device::store::{compact, read_manifest, StoreError};
use crate::domain::device::{ADOPTED_TOKEN, PLAYLIST_DIR};
use crate::domain::relpath::canonical_key;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryTrack {
    pub track_id: i64,
    pub rel_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryPlaylist {
    pub playlist_id: i64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remapped {
    pub manifest: DeviceManifest,
    pub tracks_adopted: usize,
    pub tracks_dropped: usize,
    /// 付け替えたプレイリスト（印を付け直す）
    pub playlists_adopted: Vec<i64>,
    pub playlists_dropped: usize,
}

/// 鍵 → 値。2 つ以上に当たる鍵は曖昧なので採らない
fn unique_index(pairs: impl Iterator<Item = (String, i64)>) -> HashMap<String, i64> {
    let mut seen: HashMap<String, Option<i64>> = HashMap::new();
    for (k, id) in pairs {
        seen.entry(k).and_modify(|v| *v = None).or_insert(Some(id));
    }
    seen.into_iter()
        .filter_map(|(k, v)| v.map(|id| (k, id)))
        .collect()
}

/// `Playlists/<名前>.m3u8` の名前
fn playlist_name(path: &str) -> Option<&str> {
    path.strip_prefix(PLAYLIST_DIR)?
        .strip_prefix('/')?
        .strip_suffix(".m3u8")
        .filter(|n| !n.is_empty() && !n.contains('/'))
}

/// 付け替え表（端末の id → 今の id）。引けない・曖昧な項目は載らない
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdRemap {
    pub tracks: HashMap<i64, i64>,
    pub playlists: HashMap<i64, i64>,
}

/// 付け替えの核。曲は `canonical_key(rel_path)`、プレイリストは `canonical_key(名前)` で今の行を引く。
/// 今の行の鍵が曖昧なら採らない。同じ曲に 2 項目以上が当たったらどれも採らない。同じプレイリストに
/// 2 つ以上が当たったら先に来た方だけを採る。`items` / `pls` の端末の id は一意であること
pub fn remap_ids(
    items: &[(i64, &str)],
    pls: &[(i64, &str)],
    tracks: &[LibraryTrack],
    playlists: &[LibraryPlaylist],
) -> IdRemap {
    let by_rel = unique_index(
        tracks
            .iter()
            .map(|t| (canonical_key(&t.rel_path), t.track_id)),
    );
    let hits: Vec<(i64, i64)> = items
        .iter()
        .filter_map(|(from, rel)| by_rel.get(&canonical_key(rel)).map(|to| (*from, *to)))
        .collect();
    let mut count: HashMap<i64, usize> = HashMap::new();
    for (_, to) in &hits {
        *count.entry(*to).or_default() += 1;
    }
    let track_map = hits
        .into_iter()
        .filter(|(_, to)| count.get(to) == Some(&1))
        .collect();

    let by_name = unique_index(
        playlists
            .iter()
            .map(|p| (canonical_key(&p.name), p.playlist_id)),
    );
    let mut used = HashSet::new();
    let mut pl_map = HashMap::new();
    for (from, name) in pls {
        if let Some(to) = by_name.get(&canonical_key(name)) {
            if used.insert(*to) {
                pl_map.insert(*from, *to);
            }
        }
    }
    IdRemap {
        tracks: track_map,
        playlists: pl_map,
    }
}

pub fn remap(
    m: &DeviceManifest,
    tracks: &[LibraryTrack],
    playlists: &[LibraryPlaylist],
) -> Remapped {
    let item_keys: Vec<(i64, &str)> = m
        .items
        .iter()
        .map(|i| (i.track_id, i.rel_path.as_str()))
        .collect();
    // 名前を取り出せないパスのプレイリストは付け替えない
    let pl_keys: Vec<(i64, &str)> = m
        .playlists
        .iter()
        .filter_map(|p| playlist_name(&p.path).map(|n| (p.playlist_id, n)))
        .collect();
    let ids = remap_ids(&item_keys, &pl_keys, tracks, playlists);

    let mut items = Vec::new();
    for i in &m.items {
        let Some(id) = ids.tracks.get(&i.track_id).copied() else {
            continue;
        };
        let token = if i.token == STALE_TOKEN {
            STALE_TOKEN
        } else {
            ADOPTED_TOKEN
        };
        items.push(ManifestItem {
            track_id: id,
            token: token.to_owned(),
            ..i.clone()
        });
    }
    let mut out_pl = Vec::new();
    for p in &m.playlists {
        let Some(id) = ids.playlists.get(&p.playlist_id).copied() else {
            continue;
        };
        out_pl.push(ManifestPlaylist {
            playlist_id: id,
            path: p.path.clone(),
            token: STALE_TOKEN.to_owned(),
        });
    }
    let adopted_pl: Vec<i64> = out_pl.iter().map(|p| p.playlist_id).collect();
    Remapped {
        tracks_adopted: items.len(),
        tracks_dropped: m.items.len() - items.len(),
        playlists_dropped: m.playlists.len() - out_pl.len(),
        playlists_adopted: adopted_pl,
        manifest: DeviceManifest {
            items,
            playlists: out_pl,
            ..m.clone()
        },
    }
}

/// 保存先の manifest（引き継ぎの候補）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Candidate {
    None,
    Unreadable,
    Manifest(DeviceManifest),
}

pub async fn read_candidate<F: DeviceFs>(fs: &F) -> Result<Candidate, RemoteError> {
    match read_manifest(fs).await {
        Ok(Some(m)) => Ok(Candidate::Manifest(m)),
        Ok(None) => Ok(Candidate::None),
        Err(StoreError::Remote(e)) => Err(e),
        Err(_) => Ok(Candidate::Unreadable),
    }
}

/// 曲かプレイリストを指している（空の manifest は前の登録の失敗の残り）
pub fn is_adoptable(m: &DeviceManifest) -> bool {
    !m.items.is_empty() || !m.playlists.is_empty()
}

/// このボリュームの保存先として引き継げる manifest か。ボリュームが一致し、曲かプレイリストを指し、
/// `device_uuid` が `random_uuid` の形（32 桁の小文字 16 進）であること
pub fn adoptable_for(m: &DeviceManifest, volume: &str) -> bool {
    m.volume == volume && is_adoptable(m) && is_device_uuid(&m.device_uuid)
}

/// 端末の uuid の形（`random_uuid` が作る 32 桁の小文字 16 進）か
pub fn is_device_uuid(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 回復（ジャーナルの片付けとサイズの照合）→ remap → manifest の書き直し。uuid が DB に無いことは
/// 呼び出し側が先に確かめること。remap は冪等なので、行の作成の前に落ちてもやり直せる
pub async fn rewrite<F: DeviceFs>(
    fs: &F,
    m: &DeviceManifest,
    tracks: &[LibraryTrack],
    playlists: &[LibraryPlaylist],
) -> Result<Remapped, RecoverError> {
    let expect = Expect {
        device_uuid: m.device_uuid.clone(),
        volume: m.volume.clone(),
    };
    let rec = recover(fs, &expect).await?;
    let r = remap(&rec.manifest, tracks, playlists);
    compact(fs, &r.manifest).await?;
    Ok(r)
}

/// 管理外のファイルの数（プレビュー用。`.spindle` の下と転送の残骸を除く）
pub async fn unmanaged_count<F: DeviceFs>(
    fs: &F,
    m: &DeviceManifest,
) -> Result<usize, RemoteError> {
    let managed: HashSet<String> = m
        .items
        .iter()
        .map(|i| canonical_key(&i.path))
        .chain(m.playlists.iter().map(|p| canonical_key(&p.path)))
        .collect();
    Ok(fs
        .list_files()
        .await?
        .iter()
        .filter(|f| {
            !crate::device::ondevice::is_reserved(&f.path)
                && !f.path.ends_with(crate::device::ondevice::TMP_SUFFIX)
                && !managed.contains(&canonical_key(&f.path))
        })
        .count())
}

//! 端末への配信の DB 層（docs/superpowers/specs/2026-09-29-device-delivery-design.md ②③、D-95）。
//! 判定は `domain::device` の純粋関数に任せ、ここは入力を組み立てて呼ぶだけ

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension as _};

use crate::db::derived as dbderived;
use crate::db::jobs::{self as dbjobs, EnqueueResult, JobType, NewJob};
use crate::db::{playlists as dbpl, Result};
use crate::domain::derived::{Variant, VariantSettings};
use crate::domain::device::{
    build_manifest, build_playlists, decide_source, diff, plan_token, resolve_collisions,
    utf16_len, DesiredItem, DesiredPlaylist, DeviceItem, Diff, PlaylistInput, PlaylistState,
    SourceHash, SourceKind, TrackInput, Transport,
};
use crate::domain::device::{counts, track_states, Counts, PendingSets, TrackState};
use crate::domain::relpath::canonical_key;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    All,
    Playlists,
}

impl Selection {
    pub fn as_str(self) -> &'static str {
        match self {
            Selection::All => "all",
            Selection::Playlists => "playlists",
        }
    }

    pub fn parse(s: &str) -> Option<Selection> {
        match s {
            "all" => Some(Selection::All),
            "playlists" => Some(Selection::Playlists),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub transport: Transport,
    pub variant: Variant,
    pub selection: Selection,
    pub generation: i64,
    pub adb_serial: Option<String>,
    pub adb_volume: Option<String>,
    pub adb_root: Option<String>,
    pub last_synced_at: Option<i64>,
}

pub struct NewDevice<'a> {
    pub name: &'a str,
    pub transport: Transport,
    pub variant: Variant,
    pub selection: Selection,
    /// adb のとき `(serial, volume, root)`
    pub adb: Option<(&'a str, &'a str, &'a str)>,
}

const DEVICE_COLUMNS: &str = "id, uuid, name, transport, variant, selection, generation,
                              adb_serial, adb_volume, adb_root, last_synced_at";

fn device_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Option<Device>> {
    let transport: String = r.get(3)?;
    let variant: String = r.get(4)?;
    let selection: String = r.get(5)?;
    let (Some(transport), Some(variant), Some(selection)) = (
        Transport::parse(&transport),
        Variant::parse(&variant),
        Selection::parse(&selection),
    ) else {
        return Ok(None);
    };
    Ok(Some(Device {
        id: r.get(0)?,
        uuid: r.get(1)?,
        name: r.get(2)?,
        transport,
        variant,
        selection,
        generation: r.get(6)?,
        adb_serial: r.get(7)?,
        adb_volume: r.get(8)?,
        adb_root: r.get(9)?,
        last_synced_at: r.get(10)?,
    }))
}

/// `f` を SAVEPOINT で囲んで原子的に実行する。呼び出し側が既にトランザクションを張っていても
/// 張っていなくても安全（`src/db/playlists.rs` の `rewrite_items` と同じ流儀）。`name` は
/// 固定の定数だけを渡すこと（SAVEPOINT 名はバインドパラメータにできないので直接埋め込むが、
/// 外部からの値を差し込むことはない）
fn atomically<T>(conn: &Connection, name: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    conn.execute_batch(&format!("SAVEPOINT {name}"))?;
    match f() {
        Ok(v) => {
            conn.execute_batch(&format!("RELEASE {name}"))?;
            Ok(v)
        }
        Err(e) => {
            let _ = conn.execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name}"));
            Err(e)
        }
    }
}

/// 128 bit の乱数を 16 進 32 桁で
fn random_uuid() -> Result<String> {
    let mut raw = [0u8; 16];
    getrandom::fill(&mut raw)
        .map_err(|e| crate::db::DbError::Internal(format!("乱数を取れない: {e}")))?;
    Ok(raw.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn create(conn: &Connection, d: &NewDevice<'_>, now: i64) -> Result<Device> {
    let uuid = random_uuid()?;
    let (serial, volume, root) = match d.adb {
        Some((s, v, r)) => (Some(s), Some(v), Some(r)),
        None => (None, None, None),
    };
    conn.execute(
        "INSERT INTO devices (uuid, name, name_key, transport, variant, selection,
                              adb_serial, adb_volume, adb_root, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
        params![
            uuid,
            d.name,
            canonical_key(d.name),
            d.transport.as_str(),
            d.variant.as_str(),
            d.selection.as_str(),
            serial,
            volume,
            root,
            now
        ],
    )?;
    let id = conn.last_insert_rowid();
    get(conn, id)?.ok_or_else(|| crate::db::DbError::Internal("作った端末を読めない".into()))
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Device>> {
    Ok(conn
        .query_row(
            &format!("SELECT {DEVICE_COLUMNS} FROM devices WHERE id = ?1"),
            [id],
            device_row,
        )
        .optional()?
        .flatten())
}

/// 端末が 1 台でも登録されているか（無ければ一覧の端末の状態のためにスナップショットを取らない）
pub fn any(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS (SELECT 1 FROM devices)", [], |r| r.get(0))?)
}

pub fn list(conn: &Connection) -> Result<Vec<Device>> {
    let mut st =
        conn.prepare_cached(&format!("SELECT {DEVICE_COLUMNS} FROM devices ORDER BY id"))?;
    let rows = st
        .query_map([], device_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.into_iter().flatten().collect())
}

/// 印の全置換。generation を進める（仕様 ③「端末の設定変更と同期の排他」。排他の検査は API 側）。
/// SAVEPOINT で囲むので、未知の playlist_id による FK 違反などで失敗しても、印も generation も
/// 呼び出し前のまま（`INSERT OR IGNORE` は UNIQUE 違反は黙らせるが FK 違反は黙らせない）
pub fn set_playlists(
    conn: &Connection,
    device_id: i64,
    playlist_ids: &[i64],
    now: i64,
) -> Result<()> {
    atomically(conn, "device_set_playlists", || {
        conn.execute(
            "DELETE FROM device_playlists WHERE device_id = ?1",
            [device_id],
        )?;
        let mut st = conn.prepare_cached(
            "INSERT OR IGNORE INTO device_playlists (device_id, playlist_id) VALUES (?1, ?2)",
        )?;
        for pid in playlist_ids {
            st.execute(params![device_id, pid])?;
        }
        conn.execute(
            "UPDATE devices SET generation = generation + 1, updated_at = ?2 WHERE id = ?1",
            params![device_id, now],
        )?;
        Ok(())
    })
}

pub fn playlist_ids(conn: &Connection, device_id: i64) -> Result<Vec<i64>> {
    let mut st = conn.prepare_cached(
        "SELECT playlist_id FROM device_playlists WHERE device_id = ?1 ORDER BY playlist_id",
    )?;
    let rows = st
        .query_map([device_id], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn source_hash(
    conn: &Connection,
    track_id: i64,
    kind: SourceKind,
) -> Result<Option<SourceHash>> {
    Ok(conn
        .query_row(
            "SELECT token, inode, size, mtime_ns, ctime_ns, sha256 FROM source_hashes
             WHERE track_id = ?1 AND source = ?2",
            params![track_id, kind.as_str()],
            |r| {
                Ok(SourceHash {
                    semantic: r.get(0)?,
                    inode: r.get::<_, i64>(1)? as u64,
                    size: r.get::<_, i64>(2)? as u64,
                    mtime_ns: r.get(3)?,
                    ctime_ns: r.get(4)?,
                    sha256: r.get(5)?,
                })
            },
        )
        .optional()?)
}

pub fn put_source_hash(
    conn: &Connection,
    track_id: i64,
    kind: SourceKind,
    h: &SourceHash,
    now: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO source_hashes (track_id, source, token, inode, size, mtime_ns, ctime_ns, sha256, computed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(track_id, source) DO UPDATE SET
           token = excluded.token, inode = excluded.inode, size = excluded.size,
           mtime_ns = excluded.mtime_ns, ctime_ns = excluded.ctime_ns,
           sha256 = excluded.sha256, computed_at = excluded.computed_at",
        params![
            track_id,
            kind.as_str(),
            h.semantic,
            h.inode as i64,
            h.size as i64,
            h.mtime_ns,
            h.ctime_ns,
            h.sha256,
            now
        ],
    )?;
    Ok(())
}

/// tracks 行から読んだ、送る元のハッシュを検査するための物理同一性（D-62。dev は含まない）
struct TrackIdentity {
    inode: Option<i64>,
    size: i64,
    mtime_ns: i64,
    ctime_ns: i64,
}

/// 送る元のハッシュが、その `tracks` 行の**現在の**物理同一性と一致するか（仕様 ③「送る元のハッシュ」）。
/// `tracks.inode` が NULL（未解決）なら常に不一致扱い。dev は比較しない（D-62）
fn identity_matches(h: &SourceHash, id: &TrackIdentity) -> bool {
    id.inode.is_some_and(|inode| inode as u64 == h.inode)
        && id.size as u64 == h.size
        && id.mtime_ns == h.mtime_ns
        && id.ctime_ns == h.ctime_ns
}

/// `track_inputs` の列: tracks（0..=10）、derived_files（11..=17）、source_hashes の master（18..=23）と
/// その系統（24..=29）
const INPUT_SQL_HEAD: &str = "
SELECT t.id, t.rel_path, t.lossless, t.channels, t.audio_version, t.tag_version,
       t.rg_scanned_at IS NOT NULL AND t.rg_track_gain IS NOT NULL AND t.rg_track_peak IS NOT NULL,
       t.inode, t.size, t.mtime_ns, t.ctime_ns,
       d.rel_path, d.src_audio_version, d.src_tag_version, d.src_artwork_id,
       d.src_rg_scanned_at, d.audio_profile, d.tag_profile,
       hm.token, hm.inode, hm.size, hm.mtime_ns, hm.ctime_ns, hm.sha256,
       hd.token, hd.inode, hd.size, hd.mtime_ns, hd.ctime_ns, hd.sha256
  FROM tracks t
  LEFT JOIN derived_files d ON d.track_id = t.id AND d.variant = ?1
  LEFT JOIN source_hashes hm ON hm.track_id = t.id AND hm.source = 'master'
  LEFT JOIN source_hashes hd ON hd.track_id = t.id AND hd.source = ?1
 WHERE t.missing_since IS NULL AND t.channels IN (1, 2)";

const INPUT_WHERE_ALL: &str = " ORDER BY t.id";
const INPUT_WHERE_PLAYLISTS: &str = "
   AND t.id IN (SELECT pi.track_id FROM device_playlists dp
                  JOIN playlist_items pi ON pi.playlist_id = dp.playlist_id
                 WHERE dp.device_id = ?2)
 ORDER BY t.id";
/// 仮の選曲（選曲タブの見積もり）。?2 は playlist_id の JSON 配列
const INPUT_WHERE_PLAYLIST_IDS: &str = "
   AND t.id IN (SELECT pi.track_id FROM playlist_items pi
                 WHERE pi.playlist_id IN (SELECT value FROM json_each(?2)))
 ORDER BY t.id";

/// `track_inputs` の対象範囲
enum InputScope<'a> {
    All,
    /// 端末に印の付いたプレイリスト
    Device(i64),
    /// 指定したプレイリスト（playlist_id の JSON 配列）
    PlaylistIds(&'a str),
}

fn hash_at(r: &rusqlite::Row<'_>, base: usize) -> rusqlite::Result<Option<SourceHash>> {
    let Some(semantic) = r.get::<_, Option<String>>(base)? else {
        return Ok(None);
    };
    Ok(Some(SourceHash {
        semantic,
        inode: r.get::<_, i64>(base + 1)? as u64,
        size: r.get::<_, i64>(base + 2)? as u64,
        mtime_ns: r.get(base + 3)?,
        ctime_ns: r.get(base + 4)?,
        sha256: r.get(base + 5)?,
    }))
}

/// 選曲に入った曲（missing とマルチチャンネルを除く。これらは差分で削除になる）。
/// Derived・原本と系統のハッシュも 1 本の LEFT JOIN で読む（曲ごとの問い合わせにしない）
pub fn track_inputs(conn: &Connection, device: &Device) -> Result<Vec<TrackInput>> {
    let scope = match device.selection {
        Selection::All => InputScope::All,
        Selection::Playlists => InputScope::Device(device.id),
    };
    track_inputs_in(conn, device.variant, scope)
}

fn track_inputs_in(
    conn: &Connection,
    variant: Variant,
    scope: InputScope<'_>,
) -> Result<Vec<TrackInput>> {
    let tail = match scope {
        InputScope::All => INPUT_WHERE_ALL,
        InputScope::Device(_) => INPUT_WHERE_PLAYLISTS,
        InputScope::PlaylistIds(_) => INPUT_WHERE_PLAYLIST_IDS,
    };
    let mut st = conn.prepare_cached(&format!("{INPUT_SQL_HEAD}{tail}"))?;
    let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<TrackInput> {
        let identity = TrackIdentity {
            inode: r.get(7)?,
            size: r.get(8)?,
            mtime_ns: r.get(9)?,
            ctime_ns: r.get(10)?,
        };
        let derived = match r.get::<_, Option<String>>(11)? {
            Some(_) => Some(dbderived::current_at(r, 11)?),
            None => None,
        };
        Ok(TrackInput {
            track_id: r.get(0)?,
            rel_path: r.get(1)?,
            lossless: r.get::<_, i64>(2)? == 1,
            channels: r.get(3)?,
            audio_version: r.get(4)?,
            tag_version: r.get(5)?,
            rg_ready: r.get::<_, i64>(6)? == 1,
            derived,
            hash_master: hash_at(r, 18)?.filter(|h| identity_matches(h, &identity)),
            hash_derived: hash_at(r, 24)?,
        })
    };
    let v = variant.as_str();
    // All のときは ?2 が SQL に現れない（rusqlite は未使用の引数を弾く）
    let rows = match scope {
        InputScope::All => st.query_map(params![v], map)?,
        InputScope::Device(id) => st.query_map(params![v, id], map)?,
        InputScope::PlaylistIds(json) => st.query_map(params![v, json], map)?,
    };
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn playlist_inputs(conn: &Connection, device_id: i64) -> Result<Vec<PlaylistInput>> {
    let mut st = conn.prepare_cached(
        "SELECT p.id, p.name FROM device_playlists dp JOIN playlists p ON p.id = dp.playlist_id
         WHERE dp.device_id = ?1 ORDER BY p.id",
    )?;
    let heads: Vec<(i64, String)> = st
        .query_map([device_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(heads.len());
    for (playlist_id, name) in heads {
        out.push(PlaylistInput {
            playlist_id,
            name,
            track_ids: dbpl::items(conn, playlist_id)?,
        });
    }
    Ok(out)
}

pub fn items(conn: &Connection, device_id: i64) -> Result<Vec<DeviceItem>> {
    let mut st = conn.prepare_cached(
        "SELECT track_id, dest_path, token, size, sha256 FROM device_items
         WHERE device_id = ?1 ORDER BY track_id",
    )?;
    let rows = st
        .query_map([device_id], |r| {
            Ok(DeviceItem {
                track_id: r.get(0)?,
                dest_path: r.get(1)?,
                token: r.get(2)?,
                size: r.get::<_, i64>(3)? as u64,
                sha256: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 端末側の正本を読んだ結果で全置換する。SAVEPOINT で囲むので、呼び出し側がトランザクションを
/// 張っていない（autocommit の）場合でも、`dest_path_key` の UNIQUE 違反などで失敗すれば
/// 直前の内容のまま残る（キャッシュが空・部分的になって次回差分が全曲送信になる事故を防ぐ）
pub fn replace_items(
    conn: &Connection,
    device_id: i64,
    items: &[DeviceItem],
    now: i64,
) -> Result<()> {
    atomically(conn, "device_replace_items", || {
        conn.execute("DELETE FROM device_items WHERE device_id = ?1", [device_id])?;
        let mut st = conn.prepare_cached(
            "INSERT INTO device_items (device_id, track_id, dest_path, dest_path_key, token, size, sha256, synced_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for it in items {
            st.execute(params![
                device_id,
                it.track_id,
                it.dest_path,
                canonical_key(&it.dest_path),
                it.token,
                it.size as i64,
                it.sha256,
                now
            ])?;
        }
        Ok(())
    })
}

pub fn playlist_states(conn: &Connection, device_id: i64) -> Result<Vec<PlaylistState>> {
    let mut st = conn.prepare_cached(
        "SELECT playlist_id, dest_path, token FROM device_playlist_state
         WHERE device_id = ?1 ORDER BY playlist_id",
    )?;
    let rows = st
        .query_map([device_id], |r| {
            Ok(PlaylistState {
                playlist_id: r.get(0)?,
                dest_path: r.get(1)?,
                token: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// SAVEPOINT で囲むので、autocommit でも失敗時は直前の内容のまま残る（`replace_items` と同じ理由）
pub fn replace_playlist_states(
    conn: &Connection,
    device_id: i64,
    states: &[PlaylistState],
    now: i64,
) -> Result<()> {
    atomically(conn, "device_replace_playlist_states", || {
        conn.execute(
            "DELETE FROM device_playlist_state WHERE device_id = ?1",
            [device_id],
        )?;
        let mut st = conn.prepare_cached(
            "INSERT INTO device_playlist_state (device_id, playlist_id, dest_path, token, synced_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for s in states {
            st.execute(params![device_id, s.playlist_id, s.dest_path, s.token, now])?;
        }
        Ok(())
    })
}

pub struct Computed {
    pub generation: i64,
    pub diff: Diff,
    pub plan_token: String,
    pub needs_hash: Vec<(i64, SourceKind)>,
    /// 衝突を解いた後の desired（送る元 `Source` を含む。同期の実行が読む）
    pub desired: Vec<DesiredItem>,
    /// 衝突を解いた後の desired から組み立てたプレイリスト（中身を含む）
    pub playlists: Vec<DesiredPlaylist>,
}

/// 端末上の root の前置き（区切りの `/` を含む）の UTF-16 長。パス長の上限（SPEC §5）の計算に使う。
/// adb は `/storage/<volume>/<root>/`（内部ストレージ `emulated` は `/storage/emulated/0/`）、
/// agent は `Music/spindle/`
pub fn root_prefix_utf16(device: &Device) -> usize {
    match device.transport {
        Transport::Adb => {
            let volume = match device.adb_volume.as_deref() {
                Some("emulated") | None => "emulated/0",
                Some(v) => v,
            };
            let root = device.adb_root.as_deref().unwrap_or("");
            utf16_len(&format!("/storage/{volume}/{root}/"))
        }
        Transport::Agent => utf16_len("Music/spindle/"),
    }
}

/// 端末の差分を計算する（保存しない。D-78 と同じく毎回計算し直す）。端末が無ければ None。
/// 系統が設定に無ければ凍結と同じ扱い（音声版が一致する既存の行だけ送れる）。
/// 行き先の衝突はプレイリストを組み立てる前に解く（保留になった曲を行き先のパスで載せない）
pub fn compute(conn: &Connection, device_id: i64) -> Result<Option<Computed>> {
    // 別の書き込みを跨いで世代の混ざった入力を読まないよう、1 つの読み取りトランザクションで読む
    let tx = conn.unchecked_transaction()?;
    let out = compute_in(&tx, device_id)?;
    tx.finish()?;
    Ok(out)
}

/// 系統の設定。設定に無ければ凍結と同じ扱い（音声版が一致する既存の行だけ送れる）
fn settings_or_frozen(conn: &Connection, variant: Variant) -> Result<VariantSettings> {
    Ok(
        dbderived::settings_of(conn, variant)?.unwrap_or(VariantSettings {
            variant,
            enabled: false,
            audio_profile: String::new(),
            tag_profile: String::new(),
            lossy_sources: false,
            multi_value_separator: " & ".into(),
        }),
    )
}

/// [`compute`] の本体。呼び出し側が読み取りトランザクションを持っているときに使う（入れ子にできないため）
pub(crate) fn compute_in(conn: &Connection, device_id: i64) -> Result<Option<Computed>> {
    let Some(device) = get(conn, device_id)? else {
        return Ok(None);
    };
    let settings = settings_or_frozen(conn, device.variant)?;
    let tracks = track_inputs(conn, &device)?;
    let current = items(conn, device_id)?;
    let manifest = resolve_collisions(
        &build_manifest(&settings, root_prefix_utf16(&device), &tracks),
        &current,
    );
    let (playlists, pl_errors) = build_playlists(
        device.transport,
        &playlist_inputs(conn, device_id)?,
        &manifest,
        &current,
    );
    let d = diff(
        &manifest,
        &current,
        &playlists,
        &playlist_states(conn, device_id)?,
        pl_errors,
    );
    let token = plan_token(device.generation, &d);
    Ok(Some(Computed {
        generation: device.generation,
        diff: d,
        plan_token: token,
        needs_hash: manifest.needs_hash,
        desired: manifest.desired,
        playlists,
    }))
}

pub fn source_hash_dedup_key(track_id: i64, kind: SourceKind) -> String {
    format!("source_hash:{track_id}:{}", kind.as_str())
}

/// 原本の `source_hash` ジョブが「走査待ち」（`tracks` の物理同一性が実ファイルと食い違う）で終わったときに
/// `jobs.note` へ残す 1 行。[`hashes_to_enqueue`] はこの印と payload の物理同一性で再投入を抑える
pub const SCAN_PENDING_NOTE: &str =
    "スキャン待ち（tracks の物理同一性が実ファイルと食い違う。スキャンを投入した）";

/// `needs` のうち、いま投入すべきもの。次のどちらかに当たる送る元は除く（1 本の問い合わせで引く）:
///
/// - 未完了（queued / running）の同じ `source_hash` ジョブがある
/// - 同じ dedup キーの最新のジョブが [`SCAN_PENDING_NOTE`] で終わっていて、その payload の物理同一性が
///   `tracks` の現在の値と同じ（走査がまだ `tracks` を直していない。投入しても同じ結果で終わるだけなので、
///   `tracks` の物理同一性が変わるまで待つ。変われば再投入される）
///
/// 最新のジョブは `MAX(id)` の集約で選び、同じ行の列（state / note / payload）を読む（SQLite は `MAX`
/// だけを含む集約で素の列をその最大の行から取ることを保証している）
pub fn hashes_to_enqueue(
    conn: &Connection,
    needs: &[(i64, SourceKind)],
) -> Result<Vec<(i64, SourceKind)>> {
    if needs.is_empty() {
        return Ok(Vec::new());
    }
    let mut st = conn.prepare_cached(
        "SELECT g.dedup_key
           FROM (SELECT dedup_key, MAX(id) AS id, state, note, payload
                   FROM jobs WHERE type = 'source_hash' AND dedup_key IS NOT NULL
                  GROUP BY dedup_key) g
           LEFT JOIN tracks t ON t.id = json_extract(g.payload, '$.track_id')
          WHERE g.state IN ('queued', 'running')
             OR (g.state = 'done' AND g.note = ?1 AND t.id IS NOT NULL
                 AND json_extract(g.payload, '$.inode') IS t.inode
                 AND json_extract(g.payload, '$.size') IS t.size
                 AND json_extract(g.payload, '$.mtime_ns') IS t.mtime_ns
                 AND json_extract(g.payload, '$.ctime_ns') IS t.ctime_ns)",
    )?;
    let skip = st
        .query_map([SCAN_PENDING_NOTE], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
    Ok(needs
        .iter()
        .copied()
        .filter(|(track_id, kind)| !skip.contains(&source_hash_dedup_key(*track_id, *kind)))
        .collect())
}

/// ハッシュの無い送る元に `source_hash` ジョブを投入し、投入した job id を返す（[`hashes_to_enqueue`] で
/// 除いたものは含めない）。原本の payload には `tracks` の現在の物理同一性（inode・size・mtime_ns・ctime_ns）
/// を入れる（走査待ちで終わったときの再投入の抑制に使う）。投入は 1 つの SAVEPOINT でまとめて行い、
/// 投入するものが無ければ何も書かない
pub fn enqueue_source_hashes(
    conn: &Connection,
    needs: &[(i64, SourceKind)],
    now: i64,
) -> Result<Vec<i64>> {
    let todo = hashes_to_enqueue(conn, needs)?;
    if todo.is_empty() {
        return Ok(Vec::new());
    }
    atomically(conn, "enqueue_source_hashes", || {
        let mut ids = Vec::new();
        let mut identity_of = conn
            .prepare_cached("SELECT inode, size, mtime_ns, ctime_ns FROM tracks WHERE id = ?1")?;
        for (track_id, kind) in &todo {
            let mut payload = serde_json::json!({ "track_id": track_id, "source": kind.as_str() });
            if *kind == SourceKind::Master {
                let identity = identity_of
                    .query_row([track_id], |r| {
                        Ok((
                            r.get::<_, Option<i64>>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, i64>(3)?,
                        ))
                    })
                    .optional()?;
                if let Some((inode, size, mtime_ns, ctime_ns)) = identity {
                    payload["inode"] = serde_json::json!(inode);
                    payload["size"] = serde_json::json!(size);
                    payload["mtime_ns"] = serde_json::json!(mtime_ns);
                    payload["ctime_ns"] = serde_json::json!(ctime_ns);
                }
            }
            let job = NewJob::new(JobType::SourceHash, payload)
                .dedup_key(source_hash_dedup_key(*track_id, *kind));
            if let EnqueueResult::Inserted(id) = dbjobs::enqueue(conn, &job, now)? {
                ids.push(id);
            }
        }
        Ok(ids)
    })
}

/// `device_errors`（kind, ref_id, reason）
pub fn device_errors(conn: &Connection, device_id: i64) -> Result<Vec<(String, i64, String)>> {
    let mut st = conn.prepare_cached(
        "SELECT kind, ref_id, reason FROM device_errors WHERE device_id = ?1 ORDER BY kind, ref_id",
    )?;
    let rows = st
        .query_map([device_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn synced_at_map(conn: &Connection, device_id: i64) -> Result<HashMap<i64, i64>> {
    let mut st =
        conn.prepare_cached("SELECT track_id, synced_at FROM device_items WHERE device_id = ?1")?;
    let rows = st
        .query_map([device_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    Ok(rows)
}

pub struct DeviceSnapshot {
    pub device: Device,
    pub computed: Computed,
    pub states: BTreeMap<i64, TrackState>,
    pub counts: Counts,
    /// `device_errors` の playlist 行（playlist_id, 理由）
    pub playlist_errors_reported: Vec<(i64, String)>,
    /// 反映済みの行（見積もりに使う）
    pub current: Vec<DeviceItem>,
}

pub struct Snapshot {
    pub devices: Vec<DeviceSnapshot>,
}

impl Snapshot {
    pub fn get(&self, device_id: i64) -> Option<&DeviceSnapshot> {
        self.devices.iter().find(|d| d.device.id == device_id)
    }

    pub fn pending_sets(&self) -> PendingSets {
        let mut by_key = HashMap::new();
        let mut by_id = HashMap::new();
        for d in &self.devices {
            let ids: Vec<i64> = d
                .states
                .iter()
                .filter(|(_, s)| matches!(s, TrackState::Pending { .. }))
                .map(|(id, _)| *id)
                .collect();
            by_key.insert(canonical_key(&d.device.name), ids.clone());
            by_id.insert(d.device.id, ids);
        }
        PendingSets {
            by_key: Arc::new(by_key),
            by_id: Arc::new(by_id),
        }
    }

    pub fn states_of(&self, track_id: i64) -> Vec<(i64, TrackState)> {
        self.devices
            .iter()
            .filter_map(|d| d.states.get(&track_id).map(|s| (d.device.id, s.clone())))
            .collect()
    }
}

/// 全端末の差分と状態を 1 つの読み取りトランザクションで計算する
pub fn snapshot(conn: &Connection) -> Result<Snapshot> {
    let tx = conn.unchecked_transaction()?;
    let mut out = Vec::new();
    for device in list(&tx)? {
        let Some(computed) = compute_in(&tx, device.id)? else {
            continue;
        };
        let errors = device_errors(&tx, device.id)?;
        let track_errors: Vec<(i64, String)> = errors
            .iter()
            .filter(|(k, _, _)| k == "track")
            .map(|(_, id, r)| (*id, r.clone()))
            .collect();
        let playlist_errors_reported = errors
            .into_iter()
            .filter(|(k, _, _)| k == "playlist")
            .map(|(_, id, r)| (id, r))
            .collect();
        let current = items(&tx, device.id)?;
        let states = track_states(
            &computed.diff,
            &current,
            &track_errors,
            &synced_at_map(&tx, device.id)?,
        );
        let counts = counts(&states);
        out.push(DeviceSnapshot {
            device,
            computed,
            states,
            counts,
            playlist_errors_reported,
            current,
        });
    }
    tx.finish()?;
    Ok(Snapshot { devices: out })
}

pub struct DevicePatch {
    pub name: Option<String>,
    pub selection: Option<Selection>,
    pub variant: Option<Variant>,
}

impl DevicePatch {
    /// 名前だけの変更（open な計画があっても許す。generation を進めない）
    pub fn only_name(&self) -> bool {
        self.selection.is_none() && self.variant.is_none()
    }
}

pub enum Update {
    Ok(Device),
    NotFound,
    Duplicate,
}

pub fn update(conn: &Connection, id: i64, p: &DevicePatch, now: i64) -> Result<Update> {
    atomically(conn, "device_update", || {
        if get(conn, id)?.is_none() {
            return Ok(Update::NotFound);
        }
        if let Some(name) = &p.name {
            let key = canonical_key(name);
            let taken: Option<i64> = conn
                .query_row(
                    "SELECT id FROM devices WHERE name_key = ?1 AND id <> ?2",
                    params![key, id],
                    |r| r.get(0),
                )
                .optional()?;
            if taken.is_some() {
                return Ok(Update::Duplicate);
            }
            conn.execute(
                "UPDATE devices SET name = ?2, name_key = ?3, updated_at = ?4 WHERE id = ?1",
                params![id, name, key, now],
            )?;
        }
        if let Some(s) = p.selection {
            conn.execute(
                "UPDATE devices SET selection = ?2 WHERE id = ?1",
                params![id, s.as_str()],
            )?;
        }
        if let Some(v) = p.variant {
            conn.execute(
                "UPDATE devices SET variant = ?2 WHERE id = ?1",
                params![id, v.as_str()],
            )?;
        }
        if !p.only_name() {
            conn.execute(
                "UPDATE devices SET generation = generation + 1, updated_at = ?2 WHERE id = ?1",
                params![id, now],
            )?;
        }
        get(conn, id)?
            .map(Update::Ok)
            .ok_or_else(|| crate::db::DbError::Internal("変更した端末を読めない".into()))
    })
}

/// 端末の行を消す（端末上のファイルは消さない。表は ON DELETE CASCADE）
pub fn delete(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.execute("DELETE FROM devices WHERE id = ?1", [id])? > 0)
}

/// open な計画か、queued / running の `device_sync` があるか（仕様 ③「端末の設定変更と同期の排他」）
pub fn has_open_work(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM device_sync_plans WHERE device_id = ?1 AND state = 'open')
             OR EXISTS (SELECT 1 FROM jobs WHERE type = 'device_sync' AND state IN ('queued', 'running')
                          AND json_extract(payload, '$.device_id') = ?1)",
        [id],
        |r| r.get::<_, i64>(0),
    )? == 1)
}

pub enum PlaylistCheck {
    Ok,
    Unknown(i64),
    Cycle(i64),
}

/// 印を付けてよいか: 実在し、端末のフィールドを使うスマートプレイリストでないこと（③ 循環の禁止）
pub fn check_playlists(conn: &Connection, ids: &[i64]) -> Result<PlaylistCheck> {
    for id in ids {
        if dbpl::kind(conn, *id)?.is_none() {
            return Ok(PlaylistCheck::Unknown(*id));
        }
        if crate::playlist::smart::load_rule(conn, *id)?
            .is_some_and(|r| r.references_device_fields())
        {
            return Ok(PlaylistCheck::Cycle(*id));
        }
    }
    Ok(PlaylistCheck::Ok)
}

/// どれかの端末の選曲に載っているか
pub fn is_registered(conn: &Connection, playlist_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM device_playlists WHERE playlist_id = ?1)",
        [playlist_id],
        |r| r.get::<_, i64>(0),
    )? == 1)
}

/// 曲名と表示用アーティスト（差分表の「曲」列）。GC で消えた曲は含まない
pub fn titles(conn: &Connection, ids: &[i64]) -> Result<HashMap<i64, (String, String)>> {
    let json =
        serde_json::to_string(ids).map_err(|e| crate::db::DbError::Internal(e.to_string()))?;
    let mut st = conn.prepare_cached(
        "SELECT id, coalesce(title, ''), coalesce(artist_display, '') FROM tracks
          WHERE id IN (SELECT value FROM json_each(?1))",
    )?;
    let rows = st
        .query_map([json], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    Ok(rows)
}

pub fn playlist_names(conn: &Connection, ids: &[i64]) -> Result<HashMap<i64, String>> {
    let json =
        serde_json::to_string(ids).map_err(|e| crate::db::DbError::Internal(e.to_string()))?;
    let mut st = conn.prepare_cached(
        "SELECT id, name FROM playlists WHERE id IN (SELECT value FROM json_each(?1))",
    )?;
    let rows = st
        .query_map([json], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    Ok(rows)
}

/// 端末に載っているスマートプレイリストの評価時刻（④ 差分画面）
pub fn smart_evaluations(
    conn: &Connection,
    device_id: i64,
) -> Result<Vec<(i64, String, Option<i64>)>> {
    let mut st = conn.prepare_cached(
        "SELECT p.id, p.name, p.evaluated_at FROM device_playlists dp JOIN playlists p ON p.id = dp.playlist_id
          WHERE dp.device_id = ?1 AND p.kind = 'smart' ORDER BY p.id",
    )?;
    let rows = st
        .query_map([device_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub struct SelectionEstimate {
    pub tracks: usize,
    pub bytes: u64,
    /// 現在の送る元の有効なハッシュ（= size）がまだ無い曲の数（Derived 待ちを含む。bytes に含まれない）
    pub unhashed: usize,
}

/// 仮の選曲の曲数と容量（選曲タブ）。差分（`build_manifest`）と同じく `decide_source` で**現在の**送る元を
/// 決め、そのハッシュの行の意味トークンが現在のものと一致する（原本は物理同一性も一致する）曲だけ
/// size を数える（Derived はエンコード後の大きさが分からないため、ハッシュの行から取る）。
/// Derived が無い・古い曲、ハッシュが無い・古い曲は unhashed に数える
pub fn estimate_selection(
    conn: &Connection,
    device: &Device,
    selection: Selection,
    playlist_ids: &[i64],
) -> Result<SelectionEstimate> {
    let json = serde_json::to_string(playlist_ids)
        .map_err(|e| crate::db::DbError::Internal(e.to_string()))?;
    let scope = match selection {
        Selection::All => InputScope::All,
        Selection::Playlists => InputScope::PlaylistIds(&json),
    };
    let settings = settings_or_frozen(conn, device.variant)?;
    let tracks = track_inputs_in(conn, device.variant, scope)?;
    let mut out = SelectionEstimate {
        tracks: tracks.len(),
        bytes: 0,
        unhashed: 0,
    };
    for t in &tracks {
        let size = decide_source(&settings, t).ok().and_then(|src| {
            let hash = match src.kind {
                SourceKind::Master => t.hash_master.as_ref(),
                SourceKind::Derived(_) => t.hash_derived.as_ref(),
            };
            hash.filter(|h| h.semantic == src.semantic).map(|h| h.size)
        });
        match size {
            Some(n) => out.bytes += n,
            None => out.unhashed += 1,
        }
    }
    Ok(out)
}

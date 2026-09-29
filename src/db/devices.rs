//! 端末への配信の DB 層（docs/superpowers/specs/2026-09-29-device-delivery-design.md ②③、D-95）。
//! 判定は `domain::device` の純粋関数に任せ、ここは入力を組み立てて呼ぶだけ

use rusqlite::{params, Connection, OptionalExtension as _};

use crate::db::derived as dbderived;
use crate::db::jobs::{self as dbjobs, EnqueueResult, JobType, NewJob};
use crate::db::{playlists as dbpl, Result};
use crate::domain::derived::{Variant, VariantSettings};
use crate::domain::device::{
    build_manifest, build_playlists, diff, plan_token, resolve_collisions, utf16_len, DesiredItem,
    DesiredPlaylist, DeviceItem, Diff, PlaylistInput, PlaylistState, SourceHash, SourceKind,
    TrackInput, Transport,
};
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

/// 選曲に入った曲（missing とマルチチャンネルを除く。これらは差分で削除になる）
pub fn track_inputs(conn: &Connection, device: &Device) -> Result<Vec<TrackInput>> {
    let ids: Vec<i64> = match device.selection {
        Selection::All => {
            let mut st = conn.prepare_cached(
                "SELECT id FROM tracks WHERE missing_since IS NULL AND channels IN (1, 2) ORDER BY id",
            )?;
            let rows = st
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        }
        Selection::Playlists => {
            let mut st = conn.prepare_cached(
                "SELECT DISTINCT t.id FROM device_playlists dp
                 JOIN playlist_items pi ON pi.playlist_id = dp.playlist_id
                 JOIN tracks t ON t.id = pi.track_id
                 WHERE dp.device_id = ?1 AND t.missing_since IS NULL AND t.channels IN (1, 2)
                 ORDER BY t.id",
            )?;
            let rows = st
                .query_map([device.id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        }
    };
    let mut out = Vec::with_capacity(ids.len());
    let mut st = conn.prepare_cached(
        "SELECT id, rel_path, lossless, channels, audio_version, tag_version,
                rg_scanned_at IS NOT NULL AND rg_track_gain IS NOT NULL AND rg_track_peak IS NOT NULL,
                inode, size, mtime_ns, ctime_ns
         FROM tracks WHERE id = ?1",
    )?;
    for id in ids {
        let Some((mut t, identity)) = st
            .query_row([id], |r| {
                let t = TrackInput {
                    track_id: r.get(0)?,
                    rel_path: r.get(1)?,
                    lossless: r.get::<_, i64>(2)? == 1,
                    channels: r.get(3)?,
                    audio_version: r.get(4)?,
                    tag_version: r.get(5)?,
                    rg_ready: r.get::<_, i64>(6)? == 1,
                    derived: None,
                    hash_master: None,
                    hash_derived: None,
                };
                let identity = TrackIdentity {
                    inode: r.get(7)?,
                    size: r.get(8)?,
                    mtime_ns: r.get(9)?,
                    ctime_ns: r.get(10)?,
                };
                Ok((t, identity))
            })
            .optional()?
        else {
            continue;
        };
        t.derived = dbderived::get(conn, id, device.variant)?;
        t.hash_master =
            source_hash(conn, id, SourceKind::Master)?.filter(|h| identity_matches(h, &identity));
        t.hash_derived = source_hash(conn, id, SourceKind::Derived(device.variant))?;
        out.push(t);
    }
    Ok(out)
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
    let Some(device) = get(conn, device_id)? else {
        return Ok(None);
    };
    let settings = dbderived::settings_of(conn, device.variant)?.unwrap_or(VariantSettings {
        variant: device.variant,
        enabled: false,
        audio_profile: String::new(),
        tag_profile: String::new(),
        lossy_sources: false,
        multi_value_separator: " & ".into(),
        rg_write_required: true,
    });
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

/// ハッシュの無い送る元に `source_hash` ジョブを投入する。未完了の同じジョブがあれば数えない
pub fn enqueue_source_hashes(
    conn: &Connection,
    needs: &[(i64, SourceKind)],
    now: i64,
) -> Result<usize> {
    let mut n = 0;
    for (track_id, kind) in needs {
        let job = NewJob::new(
            JobType::SourceHash,
            serde_json::json!({ "track_id": track_id, "source": kind.as_str() }),
        )
        .dedup_key(source_hash_dedup_key(*track_id, *kind));
        if matches!(
            dbjobs::enqueue(conn, &job, now)?,
            EnqueueResult::Inserted(_)
        ) {
            n += 1;
        }
    }
    Ok(n)
}

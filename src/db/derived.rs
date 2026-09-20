//! `derived_files` の読み書きと `transcode` ジョブの投入（SPEC §6「版管理」/ §7.6 / §8、D-51 / D-75）。
//!
//! 系統（[`Variant`]）ごとに 1 行。系統の設定は `config.toml` が正で、起動時に [`sync_variants`] が
//! `derived_variants` 表へ写す（投入判定はどこからでも同じ接続で設定を引けるように。D-55 の
//! `fb2k_prefix` と同じ流儀）。
//!
//! 投入判定は [`enqueue_if_stale`] に集約する。呼ぶのは scan 完了時（[`enqueue_all_stale`]）、
//! tagwrite / rename の applied、RG 解析の保存後、album gain の切り替え。ジョブは `(track_id,
//! variant, audio_version)` 単位で dedup され、ハンドラは現在値から必要な処理を判定するので、
//! 投入が重複しても最初の 1 本で全部片づく

use rusqlite::{params, Connection, OptionalExtension as _};

use crate::config::OpusVariantConfig;
use crate::db::jobs::{self as dbjobs, EnqueueResult, JobType, NewJob};
use crate::db::Result;
use crate::domain::derived::{
    expected_rel_path, opus_profiles, plan, Current, Target, Variant, VariantSettings,
};
use crate::domain::relpath::canonical_key;

// ---------------------------------------------------------------- 系統の設定（derived_variants）

/// 起動時に `config.toml` の系統設定を `derived_variants` 表へ写す（`aac` 系統は P4-8）
pub fn sync_variants(conn: &Connection, opus: &OpusVariantConfig, now: i64) -> Result<()> {
    let (audio_profile, tag_profile) = opus_profiles(opus.bitrate);
    conn.execute(
        "INSERT INTO derived_variants (variant, enabled, audio_profile, tag_profile, codec, bitrate, updated_at)
         VALUES ('opus', ?1, ?2, ?3, 'opus', ?4, ?5)
         ON CONFLICT(variant) DO UPDATE SET
           enabled = excluded.enabled, audio_profile = excluded.audio_profile,
           tag_profile = excluded.tag_profile, codec = excluded.codec, bitrate = excluded.bitrate,
           updated_at = excluded.updated_at",
        params![
            i64::from(opus.enabled),
            audio_profile,
            tag_profile,
            i64::from(opus.bitrate),
            now
        ],
    )?;
    Ok(())
}

fn settings_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Option<VariantSettings>> {
    let name: String = r.get(0)?;
    let Some(variant) = Variant::parse(&name) else {
        return Ok(None);
    };
    Ok(Some(VariantSettings {
        variant,
        enabled: r.get::<_, i64>(1)? == 1,
        audio_profile: r.get(2)?,
        tag_profile: r.get(3)?,
    }))
}

/// 表にある系統の設定（凍結中も含む。variant 名順）
pub fn variant_settings(conn: &Connection) -> Result<Vec<VariantSettings>> {
    let mut st = conn.prepare_cached(
        "SELECT variant, enabled, audio_profile, tag_profile FROM derived_variants ORDER BY variant",
    )?;
    let rows = st
        .query_map([], settings_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows.into_iter().flatten().collect())
}

/// 1 系統の設定。表に無ければ None（設定に無い系統 = 投入も生成もしない）
pub fn settings_of(conn: &Connection, variant: Variant) -> Result<Option<VariantSettings>> {
    Ok(conn
        .query_row(
            "SELECT variant, enabled, audio_profile, tag_profile FROM derived_variants WHERE variant = ?1",
            [variant.as_str()],
            settings_row,
        )
        .optional()?
        .flatten())
}

pub fn load_target(conn: &Connection, track_id: i64) -> Result<Option<Target>> {
    Ok(conn
        .query_row(
            "SELECT t.id, t.lossless, t.missing_since IS NOT NULL, t.channels, t.rel_path,
                    t.audio_version, t.tag_version, coalesce(t.artwork_id, a.artwork_id),
                    t.rg_scanned_at
             FROM tracks t LEFT JOIN albums a ON a.id = t.album_id
             WHERE t.id = ?1",
            [track_id],
            |r| {
                Ok(Target {
                    track_id: r.get(0)?,
                    lossless: r.get::<_, i64>(1)? == 1,
                    missing: r.get::<_, i64>(2)? == 1,
                    channels: r.get(3)?,
                    library_rel_path: r.get(4)?,
                    audio_version: r.get(5)?,
                    tag_version: r.get(6)?,
                    artwork_id: r.get(7)?,
                    rg_scanned_at: r.get(8)?,
                })
            },
        )
        .optional()?)
}

/// ジョブが読んだ `before` から、タグ側の世代（`tag_version` / 埋める画像 / RG の解析世代）・音声版・
/// 所在が動いたか。transcode が Library を読んでから Derived に記録するまでの間に、track lock を
/// 取らない経路（album gain の切り替え。D-74）が世代を進めていれば、書いた内容は古いので同じ
/// ジョブを再キューして揃え直す（投入は running の間 dedup で弾かれるため、自分で拾う）
pub fn target_drifted(conn: &Connection, before: &Target) -> Result<bool> {
    let Some(now) = load_target(conn, before.track_id)? else {
        return Ok(true);
    };
    Ok(now.audio_version != before.audio_version
        || now.tag_version != before.tag_version
        || now.artwork_id != before.artwork_id
        || now.rg_scanned_at != before.rg_scanned_at
        || now.library_rel_path != before.library_rel_path
        || now.missing != before.missing)
}

fn current_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Current> {
    Ok(Current {
        rel_path: r.get(0)?,
        src_audio_version: r.get(1)?,
        src_tag_version: r.get(2)?,
        src_artwork_id: r.get(3)?,
        src_rg_scanned_at: r.get(4)?,
        audio_profile: r.get(5)?,
        tag_profile: r.get(6)?,
    })
}

const CURRENT_COLUMNS: &str = "rel_path, src_audio_version, src_tag_version, src_artwork_id,
                    src_rg_scanned_at, audio_profile, tag_profile";

pub fn get(conn: &Connection, track_id: i64, variant: Variant) -> Result<Option<Current>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {CURRENT_COLUMNS} FROM derived_files WHERE track_id = ?1 AND variant = ?2"
            ),
            params![track_id, variant.as_str()],
            current_row,
        )
        .optional()?)
}

/// その Derived パス（canonical key）を持つトラックの状態
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub track_id: i64,
    pub variant: Variant,
    pub missing: bool,
    /// 持っているパスが自分（その系統）の期待パスと違う（Library 側で移動済みで、Derived が追随待ち）
    pub stale: bool,
}

/// `key` を `derived_files` で持つトラックとその状態
pub fn holder_state(conn: &Connection, key: &str) -> Result<Option<Holder>> {
    let found: Option<(i64, String, i64, String)> = conn
        .query_row(
            "SELECT d.track_id, d.variant, t.missing_since IS NOT NULL, t.rel_path
             FROM derived_files d JOIN tracks t ON t.id = d.track_id
             WHERE d.rel_path_key = ?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((track_id, variant, missing, library_rel_path)) = found else {
        return Ok(None);
    };
    let variant = Variant::parse(&variant).ok_or_else(|| {
        super::DbError::Internal(format!("derived_files に未知の variant: {variant}"))
    })?;
    Ok(Some(Holder {
        track_id,
        variant,
        missing: missing == 1,
        stale: canonical_key(&expected_rel_path(variant, &library_rel_path)) != key,
    }))
}

/// そのトラックのその系統の `transcode` が queued / running にあるか（追随待ちの相手が動いているか）。
/// 旧 payload（variant 無し）は opus 系統
pub fn has_active_job(conn: &Connection, track_id: i64, variant: Variant) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM jobs
              WHERE type = 'transcode' AND state IN ('queued', 'running')
                AND json_extract(payload, '$.track_id') = ?1
                AND coalesce(json_extract(payload, '$.variant'), 'opus') = ?2
              LIMIT 1",
            params![track_id, variant.as_str()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// その Derived パス（canonical key）を持つ track_id
pub fn holder_of_key(conn: &Connection, key: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT track_id FROM derived_files WHERE rel_path_key = ?1",
            [key],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn track_is_missing(conn: &Connection, track_id: i64) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT missing_since IS NOT NULL FROM tracks WHERE id = ?1",
            [track_id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0)
        == 1)
}

/// Derived に書いたタグ側の世代（`plan` の retag 判定に使う 3 つ）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TagState {
    pub src_tag_version: i64,
    pub src_artwork_id: Option<i64>,
    pub src_rg_scanned_at: Option<i64>,
}

impl TagState {
    pub fn of(t: &Target) -> Self {
        Self {
            src_tag_version: t.tag_version,
            src_artwork_id: t.artwork_id,
            src_rg_scanned_at: t.rg_scanned_at,
        }
    }
}

/// 作ったときの設定の世代（`derived_variants` の値をそのまま写す）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profiles {
    pub audio_profile: String,
    pub tag_profile: String,
}

impl Profiles {
    pub fn of(s: &VariantSettings) -> Self {
        Self {
            audio_profile: s.audio_profile.clone(),
            tag_profile: s.tag_profile.clone(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn upsert(
    conn: &Connection,
    track_id: i64,
    variant: Variant,
    rel_path: &str,
    bitrate: Option<i64>,
    src_audio_version: i64,
    tags: TagState,
    profiles: &Profiles,
    now: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO derived_files (track_id, variant, rel_path, rel_path_key, codec, bitrate,
                                    src_audio_version, src_tag_version, src_artwork_id,
                                    src_rg_scanned_at, generated_at, audio_profile, tag_profile)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(track_id, variant) DO UPDATE SET
           rel_path = excluded.rel_path, rel_path_key = excluded.rel_path_key,
           codec = excluded.codec, bitrate = excluded.bitrate,
           src_audio_version = excluded.src_audio_version,
           src_tag_version = excluded.src_tag_version,
           src_artwork_id = excluded.src_artwork_id,
           src_rg_scanned_at = excluded.src_rg_scanned_at,
           generated_at = excluded.generated_at,
           audio_profile = excluded.audio_profile, tag_profile = excluded.tag_profile",
        params![
            track_id,
            variant.as_str(),
            rel_path,
            canonical_key(rel_path),
            variant.codec(),
            bitrate,
            src_audio_version,
            tags.src_tag_version,
            tags.src_artwork_id,
            tags.src_rg_scanned_at,
            now,
            profiles.audio_profile,
            profiles.tag_profile,
        ],
    )?;
    Ok(())
}

pub fn set_path(conn: &Connection, track_id: i64, variant: Variant, rel_path: &str) -> Result<()> {
    conn.execute(
        "UPDATE derived_files SET rel_path = ?3, rel_path_key = ?4 WHERE track_id = ?1 AND variant = ?2",
        params![track_id, variant.as_str(), rel_path, canonical_key(rel_path)],
    )?;
    Ok(())
}

/// タグ側の世代を更新する（タグ上書きの後）。`tag_profile` も書いた設定の世代に揃える
pub fn set_tag_state(
    conn: &Connection,
    track_id: i64,
    variant: Variant,
    tags: TagState,
    tag_profile: &str,
    now: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE derived_files SET src_tag_version = ?3, src_artwork_id = ?4, src_rg_scanned_at = ?5,
                generated_at = ?6, tag_profile = ?7
         WHERE track_id = ?1 AND variant = ?2",
        params![
            track_id,
            variant.as_str(),
            tags.src_tag_version,
            tags.src_artwork_id,
            tags.src_rg_scanned_at,
            now,
            tag_profile,
        ],
    )?;
    Ok(())
}

pub fn delete(conn: &Connection, track_id: i64, variant: Variant) -> Result<bool> {
    Ok(conn.execute(
        "DELETE FROM derived_files WHERE track_id = ?1 AND variant = ?2",
        params![track_id, variant.as_str()],
    )? == 1)
}

pub fn dedup_key(track_id: i64, variant: Variant, audio_version: i64) -> String {
    format!("transcode:{track_id}:{variant}:{audio_version}")
}

/// `transcode` ジョブ（SPEC §8）。`audio_version` は基盤の stale 判定に使う。`tag_version` は
/// 参考情報（ハンドラは現在値を読み直す）
pub fn new_job(track_id: i64, variant: Variant, audio_version: i64, tag_version: i64) -> NewJob {
    NewJob::new(
        JobType::Transcode,
        serde_json::json!({
            "track_id": track_id,
            "variant": variant.as_str(),
            "audio_version": audio_version,
            "tag_version": tag_version,
        }),
    )
    .dedup_key(dedup_key(track_id, variant, audio_version))
}

/// payload の系統。旧 payload（variant 無し。0018 より前に queued だったジョブ）は opus
pub fn variant_of_payload(payload: &serde_json::Value) -> Option<Variant> {
    match payload.get("variant") {
        None => Some(Variant::Opus),
        Some(v) => v.as_str().and_then(Variant::parse),
    }
}

fn enqueue_target(
    conn: &Connection,
    t: &Target,
    variant: Variant,
    now: i64,
) -> Result<Option<i64>> {
    Ok(
        match dbjobs::enqueue(
            conn,
            &new_job(t.track_id, variant, t.audio_version, t.tag_version),
            now,
        )? {
            EnqueueResult::Inserted(id) => Some(id),
            EnqueueResult::Duplicate(_) => None,
        },
    )
}

/// 設定にある系統のうち、そのトラックの Derived が現在値と食い違うものに `transcode` を投入する。
/// 投入した job id（対象外・揃っている・凍結・dedup で既にあるものは含まない）
pub fn enqueue_if_stale(conn: &Connection, track_id: i64, now: i64) -> Result<Vec<i64>> {
    let Some(t) = load_target(conn, track_id)? else {
        return Ok(Vec::new());
    };
    let mut ids = Vec::new();
    for s in variant_settings(conn)? {
        let current = get(conn, track_id, s.variant)?;
        if !plan(&s, &t, current.as_ref()).needs_job() {
            continue;
        }
        if let Some(id) = enqueue_target(conn, &t, s.variant, now)? {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// 対象になりうる全トラック（active・1ch / 2ch）を系統ごとに見て食い違う分を一括投入する（scan 完了時）。
/// 期待パスの比較は SQL では書きにくいので行を取ってから Rust で判定する。非可逆は `eligible` が
/// 系統ごとに判定する（opus は対象外、aac は P4-8）
pub fn enqueue_all_stale(conn: &Connection, now: i64) -> Result<Vec<i64>> {
    let mut ids = Vec::new();
    for s in variant_settings(conn)? {
        if !s.enabled {
            continue;
        }
        let mut stmt = conn.prepare(
            "SELECT t.id, t.lossless, t.channels, t.rel_path, t.audio_version, t.tag_version,
                    coalesce(t.artwork_id, a.artwork_id), t.rg_scanned_at,
                    d.rel_path, d.src_audio_version, d.src_tag_version, d.src_artwork_id,
                    d.src_rg_scanned_at, d.audio_profile, d.tag_profile
             FROM tracks t
             LEFT JOIN albums a ON a.id = t.album_id
             LEFT JOIN derived_files d ON d.track_id = t.id AND d.variant = ?1
             WHERE t.missing_since IS NULL AND t.channels IN (1, 2)
             ORDER BY t.id",
        )?;
        let rows = stmt.query_map([s.variant.as_str()], |r| {
            let t = Target {
                track_id: r.get(0)?,
                lossless: r.get::<_, i64>(1)? == 1,
                missing: false,
                channels: r.get(2)?,
                library_rel_path: r.get(3)?,
                audio_version: r.get(4)?,
                tag_version: r.get(5)?,
                artwork_id: r.get(6)?,
                rg_scanned_at: r.get(7)?,
            };
            let rel: Option<String> = r.get(8)?;
            let current = match rel {
                Some(rel_path) => Some(Current {
                    rel_path,
                    src_audio_version: r.get(9)?,
                    src_tag_version: r.get(10)?,
                    src_artwork_id: r.get(11)?,
                    src_rg_scanned_at: r.get(12)?,
                    audio_profile: r.get(13)?,
                    tag_profile: r.get(14)?,
                }),
                None => None,
            };
            Ok((t, current))
        })?;
        for row in rows {
            let (t, current) = row?;
            if !plan(&s, &t, current.as_ref()).needs_job() {
                continue;
            }
            if let Some(id) = enqueue_target(conn, &t, s.variant, now)? {
                ids.push(id);
            }
        }
    }
    Ok(ids)
}

// ---------------------------------------------------------------- Derived パスの排他予約

/// `key`（Derived の canonical key）をジョブ `job_id` のために予約する。別のジョブが持っていれば
/// false。持ち主のジョブが `running` でなくなっていれば（panic / 強制終了）無効として奪う。
/// 同じジョブの再取得は true。`track_id` は GC の予約（孤児の削除。D-56）では `None`
pub fn lock_path(
    conn: &Connection,
    key: &str,
    track_id: Option<i64>,
    job_id: i64,
    now: i64,
) -> Result<bool> {
    conn.execute(
        "DELETE FROM derived_path_locks
          WHERE rel_path_key = ?1
            AND job_id NOT IN (SELECT id FROM jobs WHERE state = 'running')",
        [key],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO derived_path_locks (rel_path_key, track_id, job_id, acquired_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![key, track_id, job_id, now],
    )?;
    let holder: i64 = conn.query_row(
        "SELECT job_id FROM derived_path_locks WHERE rel_path_key = ?1",
        [key],
        |r| r.get(0),
    )?;
    Ok(holder == job_id)
}

/// ジョブが持つ予約を全部解放する
pub fn unlock_paths(conn: &Connection, job_id: i64) -> Result<usize> {
    Ok(conn.execute("DELETE FROM derived_path_locks WHERE job_id = ?1", [job_id])?)
}

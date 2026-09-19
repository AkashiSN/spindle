//! ロスレス → FLAC 正規化の coordinator（SPEC §7.4、D-10 / D-45 / D-46、docs/TASKS.md P1-4）。
//!
//! ```text
//! plan_normalize:     対象トラックの宛先（拡張子を .flac にした同じパス）と、対象外・衝突の理由を
//!                     返す（preview）。DB もファイルも書かない
//! prepare_normalize:  1 トランザクションで edit_batches(prepared) / edit_ops(kind=archive, pending) /
//!                     edits（rel_path 旧→新、codec 旧→新）を記録し、track 単位の normalize ジョブを
//!                     投入する。**DB は先行更新しない**（変換が終わるまで表は元ファイルの実体のまま。
//!                     pending の op があるので再編集は 409 になる）
//! apply_archive_op（normalize ジョブの本体。何度呼んでも結果は同じ）:
//!   1. Library/<旧> を開き事前条件（dev / inode / size / mtime / ctime / tag_hash）を確認。
//!      外れていれば skipped_conflict でファイルは触らない
//!   2. Library/<新> に置くファイルを用意する
//!        - Archive/<新> に held の台帳行があれば、それを Library へ**コピー**して戻す（巻き戻し。
//!          Archive は追記のみなので消さない）
//!        - 無ければ元をデコードして PCM MD5 → flac でエンコード → タグと画像を lofty で写す
//!          → STREAMINFO の MD5 と照合。不一致なら failed（生成物は捨て、元ファイルは残す）
//!   3. Library/<新> へ置く（親 dir に tmp → fsync → RENAME_NOREPLACE → dir fsync）
//!   4. Library/<旧> を Archive/<旧> へ move する（コピー → ハッシュ照合 → fsync → rename →
//!      Library 側を unlink）。Archive は別プールなので rename ではなく実コピー
//!   5. 1 トランザクションで op を applied にし、rel_path / 物理属性 / codec / audio_md5 /
//!      original_codec / normalized_at を追随、台帳（archived_files）を更新、バッチを集計
//! ```
//!
//! `audio_md5` は同じ PCM なので変わらず、`audio_version` も上げない（Derived は据え置き。不変条件 3）。
//! `tag_version` は写したタグの `tag_hash` が元と違うときだけ進める（形式間で表現できないキーが
//! 落ちた等。SPEC §6）。
//!
//! 冪等性: phase 境界でクラッシュしても、再投入されたジョブが Library/<旧> と Library/<新> と
//! Archive/<旧> の有無・内容（音声 MD5、バイト列）から続きを判定する。Library/<新> が既にあれば
//! MD5 が期待値と一致するときだけ自分の成果物とみなす（別のファイルなら conflict）。
//! Library/<旧> が既に無く、<新> と Archive/<旧> が揃っていれば「反映済み」として DB だけ確定する。
//!
//! 巻き戻し（`revert.rs`）は同じ op 種別で向きを逆にする: `rel_path` 新→旧、`codec` 新→旧。
//! 復元する元ファイルは Archive/<旧 rel_path> の held 行から取り、Library にあった FLAC は
//! Archive へ move して reason='restore' の行を足す（GC の対象になる）

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension as _};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::db::archive::{self, ArchiveReason, ArchiveState};
use crate::db::history::{self, OpKind, OpResult, Precondition};
use crate::db::jobs as dbjobs;
use crate::db::now_epoch;
use crate::db::scans::{self, Fingerprint};
use crate::domain::relpath::RelPath;
use crate::domain::tags::{
    read_audio_file, read_transfer_tags, tag_hash, write_flac_tags, AudioFile, Codec,
};
use crate::fsroot::{self, FsError, RootDir};
use crate::jobs::{Event, JobType, NewJob};
use crate::media::encode::FlacEncoder;
use crate::media::fingerprint;

use super::{
    batch_event, ext_of, mismatches, read_file_state, same_stat, sync_track_to_file, EditError,
    Editor, FileState, OpOutcome, Prepared,
};

/// 正規化の対象になるコーデック（Library のロスレスは FLAC に統一する。D-45）
pub const NORMALIZE_SOURCES: [Codec; 3] = [Codec::Wav, Codec::Alac, Codec::Aiff];

/// 正規化に要する環境（Archive の root、エンコーダ、台帳の保持期間）。無ければ正規化 API は使えない
#[derive(Debug, Clone)]
pub struct NormalizeEnv {
    pub archive: Arc<RootDir>,
    pub encoder: FlacEncoder,
    pub retention_days: u32,
}

/// 1 トラックの正規化（`prepare_normalize` の入力）。`new_rel_path` / `new_codec` は通常
/// [`plan_normalize`] の結果（拡張子 `.flac` / `flac`）、巻き戻しでは元の値
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizeTarget {
    pub track_id: i64,
    pub new_rel_path: String,
    pub new_codec: String,
    /// preview 時の事前条件（D-33）。無ければ記録時点の DB 値
    pub expected: Option<Precondition>,
    /// 計画の時点で衝突していた。理由付きの `skipped_conflict` op として記録だけする
    pub planned_conflict: Option<String>,
}

/// `plan_normalize` の結果 1 件
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedNormalize {
    pub track_id: i64,
    pub current_rel_path: String,
    pub codec: String,
    pub planned: NormalizePlan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizePlan {
    /// 宛先（拡張子を `.flac` にしたパス）
    Path(String),
    /// 対象外（既に FLAC、非可逆、対応外のロスレス）
    Unchanged,
    /// 衝突（宛先を別のトラックが占有、missing 等）。理由付き
    Conflict(String),
}

/// normalize ジョブの dedup key。op ごとに一意にする（`normalize:<track_id>` だけだと、巻き戻し・
/// やり直しで同じトラックの新しい op を、まだ `running` の前のジョブ（ハンドラは返ったが終端の
/// トランザクションが済んでいない）に相乗りさせてしまい、新しい op を実行するジョブが無くなる）。
/// 同じトラックの直列化は `track_locks` が担う
pub fn normalize_dedup_key(track_id: i64, op_id: i64) -> String {
    format!("normalize:{track_id}:{op_id}")
}

pub(super) fn normalize_job(track_id: i64, op_id: i64, batch_id: i64) -> NewJob {
    NewJob::new(
        JobType::Normalize,
        serde_json::json!({ "track_id": track_id, "op_id": op_id, "batch_id": batch_id }),
    )
    .dedup_key(normalize_dedup_key(track_id, op_id))
    .edit_batch_id(batch_id)
}

/// 拡張子を `.flac` に置き換えたパス（拡張子が無ければ付け足す）
pub fn flac_rel_path(rel_path: &str) -> String {
    match rel_path.rsplit_once('/') {
        Some((dir, name)) => format!("{dir}/{}", flac_file_name(name)),
        None => flac_file_name(rel_path),
    }
}

fn flac_file_name(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => format!("{stem}.flac"),
        _ => format!("{name}.flac"),
    }
}

// ---------------------------------------------------------------- plan / prepare

pub(super) fn plan_normalize_tx(
    conn: &Connection,
    ids: &[i64],
) -> Result<Vec<PlannedNormalize>, EditError> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let row: Option<(String, String, Option<i64>)> = conn
            .query_row(
                "SELECT rel_path, codec, missing_since FROM tracks WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((rel_path, codec, missing)) = row else {
            return Err(EditError::TrackNotFound(*id));
        };
        let planned = plan_one(conn, *id, &rel_path, &codec, missing.is_some())?;
        out.push(PlannedNormalize {
            track_id: *id,
            current_rel_path: rel_path,
            codec,
            planned,
        });
    }
    Ok(out)
}

fn plan_one(
    conn: &Connection,
    id: i64,
    rel_path: &str,
    codec: &str,
    missing: bool,
) -> Result<NormalizePlan, EditError> {
    let Some(c) = Codec::parse(codec) else {
        return Ok(NormalizePlan::Unchanged);
    };
    if !NORMALIZE_SOURCES.contains(&c) {
        return Ok(NormalizePlan::Unchanged);
    }
    if missing {
        return Ok(NormalizePlan::Conflict("ファイルが missing".to_owned()));
    }
    let new = flac_rel_path(rel_path);
    let new_rel = RelPath::parse(&new)?;
    let holder: Option<(i64, Option<i64>)> = conn
        .query_row(
            "SELECT id, missing_since FROM tracks WHERE rel_path_key = ?1",
            [new_rel.key()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((holder_id, holder_missing)) = holder {
        if holder_id != id && holder_missing.is_none() {
            return Ok(NormalizePlan::Conflict(format!(
                "宛先を別のトラックが占有: {new}"
            )));
        }
    }
    Ok(NormalizePlan::Path(new))
}

struct PlannedOp {
    track_id: i64,
    expected: Precondition,
    old: String,
    new: RelPath,
    old_codec: String,
    new_codec: String,
}

struct ConflictOp {
    track_id: i64,
    expected: Precondition,
    old: String,
    new: String,
    old_codec: String,
    new_codec: String,
    error: String,
}

/// バッチを記録して track ジョブを投入する。DB の rel_path / codec は先行更新しない
pub(super) fn prepare_normalize_tx(
    conn: &mut Connection,
    description: Option<&str>,
    targets: &[NormalizeTarget],
    reverts_batch_id: Option<i64>,
    now: i64,
) -> Result<Prepared, EditError> {
    let tx = conn.transaction()?;
    let mut ids: Vec<i64> = targets.iter().map(|t| t.track_id).collect();
    ids.sort_unstable();
    if let Some(w) = ids.windows(2).find(|w| w[0] == w[1]) {
        return Err(EditError::DuplicateTrack(w[0]));
    }
    let pending = history::pending_track_ids(&tx, &ids)?;
    if !pending.is_empty() {
        return Err(EditError::Pending { track_ids: pending });
    }

    let mut planned: Vec<PlannedOp> = Vec::new();
    let mut conflicts: Vec<ConflictOp> = Vec::new();
    let mut to_vacate: Vec<i64> = Vec::new();
    let mut unchanged = 0usize;
    for t in targets {
        let current = history::precondition_of_track(&tx, t.track_id)?
            .ok_or(EditError::TrackNotFound(t.track_id))?;
        let old_codec: String = tx.query_row(
            "SELECT codec FROM tracks WHERE id = ?1",
            [t.track_id],
            |r| r.get(0),
        )?;
        let old = current.rel_path.clone().unwrap_or_default();
        let current_hash = current.tag_hash.clone();
        let expected = t.expected.clone().unwrap_or(current);
        let mut conflict = |error: String| {
            conflicts.push(ConflictOp {
                track_id: t.track_id,
                expected: expected.clone(),
                old: old.clone(),
                new: t.new_rel_path.clone(),
                old_codec: old_codec.clone(),
                new_codec: t.new_codec.clone(),
                error,
            })
        };
        if let Some(reason) = &t.planned_conflict {
            conflict(reason.clone());
            continue;
        }
        if let Some(e) = &t.expected {
            if e.tag_hash.is_some() && e.tag_hash != current_hash {
                conflict("プレビューの後に変更された（tag_hash 不一致）".to_owned());
                continue;
            }
        }
        if t.new_rel_path == old && t.new_codec == old_codec {
            unchanged += 1;
            continue;
        }
        let new = match RelPath::parse(&t.new_rel_path) {
            Ok(p) => p,
            Err(e) => {
                conflict(format!("宛先が不正: {e}"));
                continue;
            }
        };
        let key = new.key();
        let holder: Option<(i64, Option<i64>)> = tx
            .query_row(
                "SELECT id, missing_since FROM tracks WHERE rel_path_key = ?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((holder_id, missing)) = holder {
            if holder_id != t.track_id {
                if missing.is_none() {
                    conflict(format!("宛先を別のトラックが占有: {new}"));
                    continue;
                }
                to_vacate.push(holder_id);
            }
        }
        planned.push(PlannedOp {
            track_id: t.track_id,
            expected,
            old,
            new,
            old_codec,
            new_codec: t.new_codec.clone(),
        });
    }
    if planned.is_empty() && conflicts.is_empty() {
        return Err(EditError::NoChanges);
    }

    let affected = planned.len() + conflicts.len();
    let batch_id = history::insert_batch(&tx, description, affected as i64, reverts_batch_id, now)?;
    let mut ordinal = 0i64;
    let mut job_ids = Vec::with_capacity(planned.len());
    // 宛先 key を missing 行が占有していれば明け渡させる（ファイルが残っていれば RENAME_NOREPLACE
    // が最終判定になり conflict になる）
    scans::vacate_track_paths(&tx, &to_vacate)?;
    for p in &planned {
        let op_id = history::insert_op(
            &tx,
            batch_id,
            ordinal,
            p.track_id,
            OpKind::Archive,
            &p.expected,
        )?;
        ordinal += 1;
        history::insert_edit(
            &tx,
            op_id,
            "rel_path",
            &serde_json::json!(p.old),
            &serde_json::json!(p.new.as_str()),
        )?;
        history::insert_edit(
            &tx,
            op_id,
            "codec",
            &serde_json::json!(p.old_codec),
            &serde_json::json!(p.new_codec),
        )?;
        let job_id = dbjobs::enqueue(&tx, &normalize_job(p.track_id, op_id, batch_id), now)?.id();
        history::set_op_job(&tx, op_id, job_id)?;
        job_ids.push(job_id);
    }
    for c in &conflicts {
        let op_id = history::insert_op(
            &tx,
            batch_id,
            ordinal,
            c.track_id,
            OpKind::Archive,
            &c.expected,
        )?;
        ordinal += 1;
        history::insert_edit(
            &tx,
            op_id,
            "rel_path",
            &serde_json::json!(c.old),
            &serde_json::json!(c.new),
        )?;
        history::insert_edit(
            &tx,
            op_id,
            "codec",
            &serde_json::json!(c.old_codec),
            &serde_json::json!(c.new_codec),
        )?;
        history::finish_op(
            &tx,
            op_id,
            OpResult::SkippedConflict,
            Some(&c.error),
            None,
            now,
        )?;
    }
    let event = match history::aggregate_batch(&tx, batch_id, now)? {
        Some(state) => Some(batch_event(&tx, batch_id, state)?),
        None => None,
    };
    tx.commit()?;
    tracing::info!(
        batch_id,
        affected,
        conflict = conflicts.len(),
        unchanged,
        "正規化バッチを記録した"
    );
    Ok(Prepared {
        batch_id,
        affected,
        unchanged,
        conflict: conflicts.len(),
        job_ids,
        event,
    })
}

// ---------------------------------------------------------------- apply（ファイル側）

/// op の edits から読む向き
struct Direction {
    old: RelPath,
    new: RelPath,
    old_codec: String,
    new_codec: String,
}

fn direction_of(op_id: i64, edits: &[history::Edit]) -> Result<Direction, EditError> {
    let find = |key: &str| -> Result<(String, String), EditError> {
        let e = edits
            .iter()
            .find(|e| e.key == key)
            .ok_or_else(|| EditError::Internal(format!("op {op_id} に {key} の edit が無い")))?;
        Ok((
            e.old_value.as_str().unwrap_or_default().to_owned(),
            e.new_value.as_str().unwrap_or_default().to_owned(),
        ))
    };
    let (old, new) = find("rel_path")?;
    let (old_codec, new_codec) = find("codec")?;
    Ok(Direction {
        old: RelPath::parse(&old)?,
        new: RelPath::parse(&new)?,
        old_codec,
        new_codec,
    })
}

/// 作業中に元ファイルを置く一時名（同じディレクトリの `spindle-normalize-<op_id>.<ext>`）。
/// 破壊フェーズの最初に元パスからここへ RENAME_NOREPLACE で退避し、以後に元パスへ現れる
/// ファイル（外部の tmp + rename 等）と自分の inode を分離する。隠しファイルにしない
/// （スキャナに見せて、pending の archive op の作業中として認識させる）
pub fn normalize_temp_rel_path(op_id: i64, source: &RelPath) -> Option<RelPath> {
    let name = match ext_of(source) {
        Some(e) => format!("spindle-normalize-{op_id}.{e}"),
        None => format!("spindle-normalize-{op_id}"),
    };
    match source.parent() {
        Some(dir) => dir.join(&name).ok(),
        None => RelPath::parse(&name).ok(),
    }
}

/// pending の archive op があるトラックについて、スキャナが「自分の作業中」とみなす所在の key
/// （元 / 一時名 / 宛先）
pub fn normalize_in_progress_keys(
    op_id: i64,
    expected_rel_path: &str,
    target_rel_path: &str,
) -> Vec<String> {
    let mut keys = vec![
        crate::domain::relpath::canonical_key(expected_rel_path),
        crate::domain::relpath::canonical_key(target_rel_path),
    ];
    if let Some(tmp) = RelPath::parse(expected_rel_path)
        .ok()
        .and_then(|p| normalize_temp_rel_path(op_id, &p))
    {
        keys.push(tmp.key());
    }
    keys
}

/// 元ファイルのコンテナ全体の SHA-256 を記録する edits のキー（unlink の前に書く。クラッシュ後の
/// 復旧で Archive の実体がこの値と一致するときだけ「退避済み」とみなす）
pub const SOURCE_HASH_KEY: &str = "source_sha256";

/// テスト用フックの呼び出し点
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalizeStep {
    /// 宛先を置き、元を一時名へ退避した直後（Archive へのコピーの前）
    Staged(i64),
    /// Archive へのコピーと照合が済み、unlink 前の最終照合（phase_verify）の直前
    BeforeUnlink(i64),
    /// 最終照合が済み、確定点（同一性の再確認 + unlink）の直前
    Verified(i64),
    /// 元を unlink した直後（不可逆な確定点の後。ここでの失敗は再試行で確定する）
    AfterUnlink(i64),
    /// undo で生成物のパスを stat で確認した直後、隔離名へ外す直前（同期呼び出し）
    UndoChecked(i64),
    /// undo で生成物を隔離名へ外した直後、照合の前（同期呼び出し）
    UndoQuarantined(i64),
    /// undo で自分の生成物を隔離名へ外して照合した直後、消す直前（同期呼び出し）
    UndoVerified(i64),
}

/// テスト用フック。`Err` を返すとその場で中断する（I/O 失敗の模擬）
pub type NormalizeHook = Arc<dyn Fn(NormalizeStep) -> Result<(), String> + Send + Sync>;

/// 元ファイルの確認結果
enum Inspected {
    /// 事前条件一致。続行
    Ready {
        source: File,
        stat: fsroot::Stat,
        af: AudioFile,
        /// 元の所在（元パスか、前回の試行が退避した一時名）
        source_rel: RelPath,
        /// 元の音声 MD5（FLAC は STREAMINFO、他はデコードした PCM）
        expected_md5: [u8; 16],
    },
    /// 元ファイルが無いが、宛先と Archive が揃っていて内容が一致する（unlink の後・DB 確定の前に
    /// 落ちた続き）
    AlreadyDone(FileState),
    Conflict {
        reason: String,
        current: Option<FileState>,
    },
    Failed(String),
}

/// ファイルの音声 MD5（拡張子で流儀を選ぶ）。FLAC の STREAMINFO が未設定なら `None`
fn audio_md5_of(
    file: File,
    rel: &RelPath,
) -> Result<Option<[u8; 16]>, fingerprint::FingerprintError> {
    let ext = ext_of(rel);
    let is_flac = ext.is_some_and(|e| e.eq_ignore_ascii_case("flac"));
    if is_flac {
        fingerprint::flac_streaminfo_md5(file)
    } else {
        fingerprint::decoded_pcm_md5(file, ext).map(Some)
    }
}

fn open_if_exists(root: &RootDir, rel: &RelPath) -> Result<Option<File>, EditError> {
    match root.open_file(rel) {
        Ok(f) => Ok(Some(f)),
        Err(FsError::NotFound | FsError::Symlink | FsError::Escaped) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn inspect_source(
    root: &RootDir,
    archive: &RootDir,
    dir: &Direction,
    op_id: i64,
    expected: &Precondition,
    db_md5: Option<[u8; 16]>,
    recorded_hash: Option<[u8; 32]>,
) -> Result<Inspected, EditError> {
    let old_path = dir.old.as_str();
    let tmp_rel = normalize_temp_rel_path(op_id, &dir.old);
    // 元パス → 前回の試行が退避した一時名 の順に探す
    let located = match open_if_exists(root, &dir.old)? {
        Some(f) => Some((f, dir.old.clone())),
        None => match &tmp_rel {
            Some(t) => open_if_exists(root, t)?.map(|f| (f, t.clone())),
            None => None,
        },
    };
    let Some((mut source, source_rel)) = located else {
        // 宛先・Archive・記録した SHA-256 が揃い、内容が一致するときだけ反映済み
        let Some(placed) = open_if_exists(root, &dir.new)? else {
            return Ok(Inspected::Conflict {
                reason: format!("元ファイルを開けない: {old_path}"),
                current: None,
            });
        };
        let placed_md5 = audio_md5_of(placed, &dir.new).ok().flatten();
        let archived_hash = match archive.open_file(&dir.old) {
            Ok(mut f) => Some(sha256_of(&mut f)?),
            Err(FsError::NotFound | FsError::Symlink | FsError::Escaped) => None,
            Err(e) => return Err(e.into()),
        };
        let ok = placed_md5.is_some()
            && placed_md5 == db_md5
            && recorded_hash.is_some()
            && archived_hash == recorded_hash;
        if !ok {
            return Ok(Inspected::Conflict {
                reason: format!(
                    "元ファイルが無く、宛先と Archive の実体を記録と照合できない: {old_path}"
                ),
                current: None,
            });
        }
        let Some(state) = read_file_state(root, None, dir.new.as_str()) else {
            return Ok(Inspected::Conflict {
                reason: format!("宛先を読めない: {}", dir.new),
                current: None,
            });
        };
        tracing::info!(path = %dir.new, "元ファイルは無いが宛先と Archive が記録と一致する。反映済みとして確定");
        return Ok(Inspected::AlreadyDone(state));
    };
    let stat = fsroot::fstat(&source)?;
    let af = {
        let mut reader = source.try_clone()?;
        reader.seek(SeekFrom::Start(0))?;
        match read_audio_file(reader, ext_of(&dir.old)) {
            Ok(af) => af,
            Err(e) => {
                return Ok(Inspected::Conflict {
                    reason: format!("タグを読めない: {e}"),
                    current: None,
                })
            }
        }
    };
    let hash = tag_hash(&af.tags);
    // 一時名にあるときは rename で ctime が進んでいる（`mismatches` は rel_path 違いのときだけ
    // ctime の差を許す）
    let diff = mismatches(expected, &stat, &hash, source_rel.as_str());
    if !diff.is_empty() {
        let current = FileState::external(root, &source_rel, stat, af, &[], None);
        return Ok(Inspected::Conflict {
            reason: format!("事前条件不一致: {}", diff.join(", ")),
            current: Some(current),
        });
    }
    if af.codec.as_str() != dir.old_codec {
        return Ok(Inspected::Conflict {
            reason: format!(
                "コーデックが記録と違う（{} ≠ {}）",
                af.codec.as_str(),
                dir.old_codec
            ),
            current: Some(FileState::external(root, &source_rel, stat, af, &[], None)),
        });
    }
    let expected_md5 = {
        let mut reader = source.try_clone()?;
        reader.seek(SeekFrom::Start(0))?;
        match audio_md5_of(reader, &dir.old) {
            Ok(Some(m)) => m,
            // STREAMINFO 未設定の FLAC（巻き戻しの元）は DB の値で代用する
            Ok(None) => match db_md5 {
                Some(m) => m,
                None => {
                    return Ok(Inspected::Failed(
                        "元ファイルの音声 MD5 を計算できない".to_owned(),
                    ))
                }
            },
            Err(e) => {
                return Ok(Inspected::Failed(format!(
                    "元ファイルをデコードできない: {e}"
                )))
            }
        }
    };
    source.seek(SeekFrom::Start(0))?;
    Ok(Inspected::Ready {
        source,
        stat,
        af,
        source_rel,
        expected_md5,
    })
}

/// Library/<新> に置くファイルの出どころ
enum Incoming {
    /// エンコードした FLAC（tmp。guard で消える）
    Encoded(crate::jobs::TempGuard),
    /// Archive/<新> の held 行（巻き戻し）。台帳の更新は DB 確定で `rel_path` から引く
    Restore,
}

/// 置いた後の結果
enum Placed {
    Applied(FileState),
    Conflict {
        reason: String,
        current: Option<FileState>,
    },
    Failed(String),
}

/// `src` の全内容を `dst` に写しながら SHA-256 を取る
fn copy_hashing(src: &mut File, dst: &mut File) -> std::io::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    src.seek(SeekFrom::Start(0))?;
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        dst.write_all(&buf[..n])?;
    }
    Ok(hasher.finalize().into())
}

fn sha256_of(file: &mut File) -> std::io::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    file.seek(SeekFrom::Start(0))?;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

/// 自分が置いたファイルの実体。消す前に「今そのパスにあるのが自分の置いたもので、しかも
/// 置いてから変わっていないか」を確かめる（パスは識別子ではない。外部が tmp + rename で
/// 差し替えていれば、また同じ inode に in-place で書いていれば触らない。書けば ctime が進む）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ident {
    dev: u64,
    inode: u64,
}

impl Ident {
    fn of(st: &fsroot::Stat) -> Self {
        Ident {
            dev: st.dev,
            inode: st.inode,
        }
    }

    fn matches(self, st: &fsroot::Stat) -> bool {
        self == Ident::of(st)
    }
}

/// 自分が置いた生成物の記録: 置いた直後の stat と、書いた内容の SHA-256
#[derive(Debug, Clone, Copy)]
struct Made {
    stat: fsroot::Stat,
    sha256: [u8; 32],
}

/// `rel` にあるのが `made` と同じ実体で未変更か（stat の一致に加えて内容のハッシュも見る。
/// カーネルの時刻は粗い粒度なので、置いた直後の同じ tick に書かれた変更は stat では見えない）
fn is_unchanged(root: &RootDir, rel: &RelPath, made: &Made) -> bool {
    let Ok(st) = root.stat(rel) else {
        return false;
    };
    if !same_stat(&made.stat, &st) {
        return false;
    }
    matches!(root.open_file(rel).and_then(|mut f| Ok(sha256_of(&mut f)?)), Ok(h) if h == made.sha256)
}

/// undo で消す前に生成物を外しておく隔離名（同じディレクトリの `.spindle-undo-<op_id>-<random>`）。
/// 隠しファイルなのでスキャナは見ないが、**`.spindle-tmp-` ではない**のでスキャナの取り残し回収
/// （1 時間後に unlink）の対象にもならない。照合前・照合不一致の実体はユーザデータかもしれず、
/// 自動で消してはいけない。照合に一致した自分の生成物だけを `.spindle-tmp-` へ移してから消す
/// （消す前に落ちても 1 時間後に回収される）
pub const UNDO_QUARANTINE_PREFIX: &str = ".spindle-undo-";

fn undo_quarantine_rel_path(op_id: i64, rel: &RelPath) -> Result<RelPath, EditError> {
    let mut buf = [0u8; 4];
    getrandom::fill(&mut buf).map_err(|e| std::io::Error::other(e.to_string()))?;
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    let name = format!("{UNDO_QUARANTINE_PREFIX}{op_id}-{hex}");
    Ok(match rel.parent() {
        Some(dir) => dir.join(&name)?,
        None => RelPath::parse(&name)?,
    })
}

/// 照合済みの自分の生成物を消す直前に移す名前（スキャナの取り残し回収の対象）
fn undo_disposal_rel_path(op_id: i64, rel: &RelPath) -> Result<RelPath, EditError> {
    let mut buf = [0u8; 4];
    getrandom::fill(&mut buf).map_err(|e| std::io::Error::other(e.to_string()))?;
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    let name = format!("{}undo-{op_id}-{hex}", fsroot::TMP_PREFIX);
    Ok(match rel.parent() {
        Some(dir) => dir.join(&name)?,
        None => RelPath::parse(&name)?,
    })
}

/// `rel` にあるのが `made` と同じ実体で未変更のときだけ消す。
/// 照合と unlink の間でパスが差し替えられて外部ファイルを消してしまわないよう、まず隔離名へ
/// 原子的に rename してパスから切り離し、**外した実体**を stat と SHA-256 で照合してから
/// unlink する。一致しなければ元のパスへ戻し（RENAME_NOREPLACE）、塞がっていれば回収パスへ残す
/// （元ファイルの回収と同型）。`hook` は照合の直後に呼ぶ（テストの fault injection 用）
fn remove_if_unchanged(
    root: &RootDir,
    rel: &RelPath,
    made: &Made,
    op_id: i64,
    what: &str,
    hook: Option<&NormalizeHook>,
) {
    let private = match undo_quarantine_rel_path(op_id, rel) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(path = %rel, error = %e, "{what}の隔離名を作れない。消さない");
            return;
        }
    };
    // rename は ctime を進めるので、stat 全体の照合はパスから外す前に行い、外した後は
    // 実体（dev / inode）と size / mtime と内容で照合する
    match root.stat(rel) {
        Ok(st) if same_stat(&made.stat, &st) => {}
        Ok(_) => {
            tracing::warn!(path = %rel, "{what}が外部に差し替え・更新されている。消さない");
            return;
        }
        Err(FsError::NotFound) => return,
        Err(e) => {
            tracing::warn!(path = %rel, error = %e, "{what}を確認できない。消さない");
            return;
        }
    }
    if let Some(h) = hook {
        let _ = h(NormalizeStep::UndoChecked(op_id));
    }
    match root.rename_noreplace(rel, &private) {
        Ok(()) => {}
        Err(FsError::NotFound) => return,
        Err(e) => {
            tracing::warn!(path = %rel, error = %e, "{what}を外せない。消さない");
            return;
        }
    }
    if let Some(h) = hook {
        let _ = h(NormalizeStep::UndoQuarantined(op_id));
    }
    let moved_is_mine = match root.stat(&private) {
        Ok(st) => {
            Ident::of(&made.stat).matches(&st)
                && st.size == made.stat.size
                && st.mtime_ns == made.stat.mtime_ns
                && matches!(
                    root.open_file(&private).and_then(|mut f| Ok(sha256_of(&mut f)?)),
                    Ok(h) if h == made.sha256
                )
        }
        Err(_) => false,
    };
    if moved_is_mine {
        if let Some(h) = hook {
            let _ = h(NormalizeStep::UndoVerified(op_id));
        }
        // 自分の生成物と確認できたので、回収対象の名前へ移してから消す（消す前に落ちても
        // スキャナが 1 時間後に回収する）
        let disposal = match undo_disposal_rel_path(op_id, rel) {
            Ok(d) => match root.rename_noreplace(&private, &d) {
                Ok(()) => d,
                Err(e) => {
                    tracing::warn!(path = %private, error = %e, "{what}を回収名へ移せない。隔離名のまま消す");
                    private
                }
            },
            Err(_) => private,
        };
        if let Err(e) = root.unlink(&disposal) {
            tracing::warn!(path = %disposal, error = %e, "{what}を消せない");
        }
        return;
    }
    // 外したのは自分の生成物ではない（差し替え・更新されていた）。元の場所へ戻す
    tracing::warn!(path = %rel, "{what}が外部に差し替え・更新されている。消さない");
    match root.rename_noreplace(&private, rel) {
        Ok(()) => {}
        Err(FsError::Exists) => match recovery_rel_path(op_id, rel) {
            Ok(rec) => match root.rename_noreplace(&private, &rec) {
                Ok(()) => {
                    tracing::error!(path = %rec, "元の場所が塞がっているので回収パスへ残した")
                }
                Err(e) => {
                    tracing::error!(path = %private, error = %e, "回収パスへ置けない。隔離名のまま残す（スキャナは消さない）")
                }
            },
            Err(e) => {
                tracing::error!(path = %private, error = %e, "回収パスを作れない。隔離名のまま残す（スキャナは消さない）")
            }
        },
        Err(e) => {
            tracing::error!(path = %private, error = %e, "元の場所へ戻せない。隔離名のまま残す（スキャナは消さない）")
        }
    }
}

/// 破壊フェーズで自分が行ったことの記録。途中で止まる（conflict / I/O 失敗）ときに元へ戻す
#[derive(Debug, Default, Clone, Copy)]
struct Undo {
    /// 宛先 FLAC を自分が新規に置いた
    placed: Option<Made>,
    /// 元を一時名へ退避した
    staged: bool,
    /// Archive へ自分がコピーした（台帳にはまだ無い）
    archived: Option<Made>,
}

/// 戻した後の元ファイルの所在
#[derive(Debug, Clone, PartialEq, Eq)]
enum SourceKept {
    /// 元パス（または再試行のために一時名のまま）にある
    InPlace,
    /// 元パスも一時名も外部に塞がれていたので、回収パスへ書き出した
    Recovered(RelPath),
    /// どこにも durable に残せなかった（自分の生成物を複製として残す）
    Lost,
}

/// undo の結果: 元ファイルの所在と、複製として残した自分の生成物
#[derive(Debug, Clone, PartialEq, Eq)]
struct UndoResult {
    source: SourceKept,
    /// 元を残せなかったので、宛先の FLAC を複製として残した
    kept_flac: bool,
    /// 元を残せなかったので、Archive のコピーを複製として残した（台帳には無い）
    kept_archive: bool,
}

impl UndoResult {
    /// 元を戻せなかったときの、実在する複製だけを述べる注記
    fn note(&self) -> Option<String> {
        match &self.source {
            SourceKept::InPlace => None,
            SourceKept::Recovered(at) => Some(format!("元ファイルは {at} に残した")),
            SourceKept::Lost => {
                let mut kept = Vec::new();
                if self.kept_flac {
                    kept.push("宛先の FLAC");
                }
                if self.kept_archive {
                    kept.push("Archive のコピー");
                }
                Some(if kept.is_empty() {
                    "元ファイルを Library に戻せず、複製も残せなかった".to_owned()
                } else {
                    format!(
                        "元ファイルを Library に戻せなかった。複製として {} を残した",
                        kept.join("と")
                    )
                })
            }
        }
    }
}

impl Undo {
    /// 自分が置いた宛先と Archive のコピーを消し、`restore_source` なら元ファイルを元パスへ戻す。
    /// unlink の前まではユーザデータは元の inode（`source` の FD）に無傷で残っているので、消すのは
    /// 自分の生成物だけで、しかも今そのパスにあるのが自分の置いた実体で未変更のときだけ。
    /// 元ファイルを durable なパスへ残せなかったときは Archive のコピーを消さない（唯一の複製）。
    /// 再試行する（I/O 失敗）ときは元を一時名のまま残す: 戻す rename は ctime を進めるので、
    /// 元パスに戻すと次の試行の事前条件（ctime）が外れて conflict になる。一時名にあれば
    /// 「rename による ctime の差」として許容される（`mismatches`）。閉じるときに戻すのは
    /// [`Editor::close_op`]
    #[allow(clippy::too_many_arguments)]
    fn run(
        self,
        root: &RootDir,
        archive: &RootDir,
        dir: &Direction,
        op_id: i64,
        tmp: Option<&RelPath>,
        source: &File,
        restore_source: bool,
        hook: Option<&NormalizeHook>,
    ) -> UndoResult {
        let source_kept = match (self.staged, restore_source, tmp) {
            (true, true, Some(tmp)) => restore_staged_source(root, tmp, &dir.old, op_id, source),
            (true, false, Some(tmp)) => keep_staged_source(root, tmp, &dir.old, op_id, source),
            _ => SourceKept::InPlace,
        };
        if source_kept == SourceKept::Lost {
            // 元を durable に残せなかった。同じ音声を持つ自分の生成物は 1 つも消さない。
            // 「複製として残した」と言えるのは、置いた直後から変わっていない実体だけ
            let kept_flac = self
                .placed
                .as_ref()
                .is_some_and(|made| is_unchanged(root, &dir.new, made));
            let kept_archive = self
                .archived
                .as_ref()
                .is_some_and(|made| is_unchanged(archive, &dir.old, made));
            tracing::error!(
                path = %dir.old,
                kept_flac,
                kept_archive,
                "元ファイルを残せなかった。自分の生成物を複製として残す（台帳には無い）"
            );
            return UndoResult {
                source: source_kept,
                kept_flac,
                kept_archive,
            };
        }
        if let Some(made) = &self.placed {
            remove_if_unchanged(root, &dir.new, made, op_id, "置いた宛先", hook);
        }
        if let Some(made) = &self.archived {
            remove_if_unchanged(
                archive,
                &dir.old,
                made,
                op_id,
                "Archive の未確定コピー",
                hook,
            );
        }
        UndoResult {
            source: source_kept,
            kept_flac: false,
            kept_archive: false,
        }
    }
}

/// 一時名へ退避した元ファイル（`source` の FD の実体）を元パスへ戻す。一時名にあるのが自分の
/// inode でなければ（外部が差し替えた）その実体は触らず、FD の内容を元パス、それも塞がっていれば
/// 回収パスへ書き出す
fn restore_staged_source(
    root: &RootDir,
    tmp: &RelPath,
    old: &RelPath,
    op_id: i64,
    source: &File,
) -> SourceKept {
    let Ok(mine) = fsroot::fstat(source).map(|st| Ident::of(&st)) else {
        return SourceKept::Lost;
    };
    match root.stat(tmp) {
        Ok(st) if mine.matches(&st) => match rename_back(root, tmp, old) {
            Ok(()) => SourceKept::InPlace,
            // 元パスが塞がっている。一時名のまま残す（実体は自分の inode なので durable）
            Err(_) => SourceKept::Recovered(tmp.clone()),
        },
        Ok(_) => {
            tracing::error!(path = %tmp, "一時名の元ファイルが外部に差し替えられている。FD の内容を書き戻す");
            write_back_from_fd(root, old, op_id, source)
        }
        Err(FsError::NotFound) => {
            tracing::error!(path = %tmp, "一時名の元ファイルが無い。FD の内容を書き戻す");
            write_back_from_fd(root, old, op_id, source)
        }
        Err(e) => {
            tracing::error!(path = %tmp, error = %e, "一時名を確認できない");
            SourceKept::Lost
        }
    }
}

/// 再試行のために元を一時名に残す。一時名の実体が自分の inode でなくなっていれば書き戻す
fn keep_staged_source(
    root: &RootDir,
    tmp: &RelPath,
    old: &RelPath,
    op_id: i64,
    source: &File,
) -> SourceKept {
    let Ok(mine) = fsroot::fstat(source).map(|st| Ident::of(&st)) else {
        return SourceKept::Lost;
    };
    match root.stat(tmp) {
        Ok(st) if mine.matches(&st) => SourceKept::InPlace,
        _ => {
            tracing::error!(path = %tmp, "一時名の元ファイルが外部に差し替えられている。FD の内容を書き戻す");
            write_back_from_fd(root, old, op_id, source)
        }
    }
}

fn rename_back(root: &RootDir, tmp: &RelPath, old: &RelPath) -> Result<(), FsError> {
    match root.rename_noreplace(tmp, old) {
        Ok(()) => {
            if let Err(e) = root.fsync_dir(old.parent().as_ref()) {
                tracing::warn!(path = %old, error = %e, "ディレクトリを fsync できない");
            }
            Ok(())
        }
        Err(e) => {
            tracing::warn!(from = %tmp, to = %old, error = %e, "元ファイルを元パスへ戻せない。一時名のまま残す");
            Err(e)
        }
    }
}

/// 回収パス（同じディレクトリの `spindle-recovery-<op_id>-<random>.<ext>`）。一時名も元パスも
/// 塞がれたときの最後の置き場。スキャナには普通のファイルとして見える（消さない）
fn recovery_rel_path(op_id: i64, old: &RelPath) -> Result<RelPath, EditError> {
    let mut buf = [0u8; 4];
    getrandom::fill(&mut buf).map_err(|e| std::io::Error::other(e.to_string()))?;
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    let name = match ext_of(old) {
        Some(e) => format!("spindle-recovery-{op_id}-{hex}.{e}"),
        None => format!("spindle-recovery-{op_id}-{hex}"),
    };
    Ok(match old.parent() {
        Some(dir) => dir.join(&name)?,
        None => RelPath::parse(&name)?,
    })
}

/// 開いている FD の内容を `old` へ、塞がっていれば回収パスへ書く（fsync + RENAME_NOREPLACE）
fn write_back_from_fd(root: &RootDir, old: &RelPath, op_id: i64, source: &File) -> SourceKept {
    let parent = old.parent();
    let staged = (|| -> Result<RelPath, EditError> {
        let (tmp_rel, mut tmp) = root.create_tmp(parent.as_ref())?;
        let written = (|| -> Result<(), EditError> {
            let mut src = source.try_clone()?;
            src.seek(SeekFrom::Start(0))?;
            std::io::copy(&mut src, &mut tmp)?;
            fsroot::copy_attrs(source, &tmp)?;
            tmp.sync_all()?;
            Ok(())
        })();
        if let Err(e) = written {
            let _ = root.unlink(&tmp_rel);
            return Err(e);
        }
        Ok(tmp_rel)
    })();
    let tmp_rel = match staged {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(path = %old, error = %e, "元ファイルを書き戻せない");
            return SourceKept::Lost;
        }
    };
    let kept = match root.rename_noreplace(&tmp_rel, old) {
        Ok(()) => SourceKept::InPlace,
        Err(FsError::Exists) => match recovery_rel_path(op_id, old) {
            Ok(rec) => match root.rename_noreplace(&tmp_rel, &rec) {
                Ok(()) => {
                    tracing::error!(path = %rec, "元パスが塞がっているので回収パスへ書き戻した");
                    SourceKept::Recovered(rec)
                }
                Err(e) => {
                    tracing::error!(path = %rec, error = %e, "回収パスへ置けない");
                    SourceKept::Lost
                }
            },
            Err(e) => {
                tracing::error!(path = %old, error = %e, "回収パスを作れない");
                SourceKept::Lost
            }
        },
        Err(e) => {
            tracing::error!(path = %old, error = %e, "元ファイルを書き戻せない");
            SourceKept::Lost
        }
    };
    if kept == SourceKept::Lost {
        // `.spindle-tmp-*` のままだとスキャナが 1 時間後に回収してしまう。残すなら名前を変える
        if let Ok(rec) = recovery_rel_path(op_id, old) {
            if root.rename_noreplace(&tmp_rel, &rec).is_ok() {
                tracing::error!(path = %rec, "回収パスへ書き戻した");
                return SourceKept::Recovered(rec);
            }
        }
        let _ = root.unlink(&tmp_rel);
    } else if let Err(e) = root.fsync_dir(parent.as_ref()) {
        tracing::warn!(path = %old, error = %e, "ディレクトリを fsync できない");
    }
    kept
}

/// pending の archive op を閉じるとき、一時名に残った元ファイルを元パスへ戻す。一時名にあるのが
/// 記録時点の inode（事前条件）と違えば触らない
pub(super) fn restore_staged_source_of(
    root: &RootDir,
    op_id: i64,
    expected: &Precondition,
    edits: &[history::Edit],
) {
    let Ok(dir) = direction_of(op_id, edits) else {
        return;
    };
    let Some(tmp) = normalize_temp_rel_path(op_id, &dir.old) else {
        return;
    };
    // dev は照合しない（マウントのたびに振り直されうる。D-62）
    match root.stat(&tmp) {
        Ok(st) if expected.inode == Some(st.inode as i64) => {
            let _ = rename_back(root, &tmp, &dir.old);
        }
        Ok(_) => {
            tracing::warn!(path = %tmp, "一時名にあるのは記録した元ファイルではない。触らない")
        }
        Err(_) => {}
    }
}

/// 破壊フェーズ 1 の結果（宛先を置き、元を一時名へ退避した）
struct Staged {
    source: File,
    source_hash: [u8; 32],
    /// 退避（rename）後の stat。rename は ctime を進めるので、以後の照合はこれと比べる
    staged_stat: fsroot::Stat,
    undo: Undo,
    archived_already: bool,
}

/// 破壊フェーズ 1（同期）: 元の再確認 → Archive の同名確認 → 宛先の配置 → 元を一時名へ退避。
/// 失敗・衝突はここで自分の生成物を戻してから返す
#[allow(clippy::too_many_arguments)]
fn phase_place(
    root: &RootDir,
    archive: &RootDir,
    dir: &Direction,
    op_id: i64,
    tmp_rel: &RelPath,
    mut source: File,
    source_stat: fsroot::Stat,
    source_rel: &RelPath,
    mut incoming: File,
    expected_md5: [u8; 16],
) -> Result<Result<Staged, Placed>, EditError> {
    // 変換の間に元ファイルが外部で更新されていないか（同じ FD の fstat。書けば ctime が進む）
    let now_st = fsroot::fstat(&source)?;
    if !same_stat(&source_stat, &now_st) {
        return Ok(Err(Placed::Conflict {
            reason: "変換の間に元ファイルが外部で更新された".to_owned(),
            current: read_file_state(root, None, source_rel.as_str()),
        }));
    }
    let source_hash = sha256_of(&mut source)?;

    // Archive の同じパスに別のファイルがあれば、Library を触る前に止める
    let archived_already = match archive.open_file(&dir.old) {
        Ok(mut f) => {
            if sha256_of(&mut f)? == source_hash {
                true
            } else {
                return Ok(Err(Placed::Failed(format!(
                    "Archive に同じパスの別のファイルがある: {}",
                    dir.old
                ))));
            }
        }
        Err(FsError::NotFound) => false,
        Err(FsError::Symlink | FsError::Escaped) => {
            return Ok(Err(Placed::Failed(format!(
                "Archive の退避先を開けない（symlink）: {}",
                dir.old
            ))))
        }
        Err(e) => return Err(e.into()),
    };

    let mut undo = Undo::default();
    // 宛先。既にあれば MD5 が一致するときだけ自分の成果物とみなす
    let placed_already = match root.open_file(&dir.new) {
        Ok(f) => match audio_md5_of(f, &dir.new) {
            Ok(Some(m)) if m == expected_md5 => true,
            _ => {
                return Ok(Err(Placed::Conflict {
                    reason: format!("宛先に別のファイルがある: {}", dir.new),
                    current: None,
                }))
            }
        },
        Err(FsError::NotFound) => false,
        Err(FsError::Symlink | FsError::Escaped) => {
            return Ok(Err(Placed::Conflict {
                reason: format!("宛先を開けない（symlink）: {}", dir.new),
                current: None,
            }))
        }
        Err(e) => return Err(e.into()),
    };
    if !placed_already {
        let parent = dir.new.parent();
        let (place_tmp, mut tmp) = root.create_tmp(parent.as_ref())?;
        let placed = (|| -> Result<Made, EditError> {
            let sha256 = copy_hashing(&mut incoming, &mut tmp)?;
            // mode / 所有者 / xattr（ACL）は元ファイルから写す（D-41）
            fsroot::copy_attrs(&source, &tmp)?;
            tmp.sync_all()?;
            root.rename_noreplace(&place_tmp, &dir.new)?;
            root.fsync_dir(parent.as_ref())?;
            Ok(Made {
                stat: fsroot::fstat(&tmp)?,
                sha256,
            })
        })();
        let ident = match placed {
            Ok(i) => i,
            Err(e) => {
                if let Err(u) = root.unlink(&place_tmp) {
                    if !matches!(u, FsError::NotFound) {
                        tracing::warn!(path = %place_tmp, error = %u, "tmp を消せない");
                    }
                }
                return match e {
                    EditError::Fs(FsError::Exists) => Ok(Err(Placed::Conflict {
                        reason: format!("宛先が反映の直前に取られた: {}", dir.new),
                        current: None,
                    })),
                    other => Err(other),
                };
            }
        };
        undo.placed = Some(ident);
    }

    // 元を一時名へ退避し、以後に元パスへ現れるファイルと分離する。rename した実体が自分の
    // inode でなければ（stat と rename の間に差し替えられた）戻して衝突にする
    if source_rel != tmp_rel {
        match root.rename_noreplace(&dir.old, tmp_rel) {
            Ok(()) => {}
            Err(FsError::Exists | FsError::NotFound) => {
                undo.run(root, archive, dir, op_id, None, &source, true, None);
                return Ok(Err(Placed::Conflict {
                    reason: format!("元ファイルを退避できない（一時名が塞がっている、または元が無い）: {tmp_rel}"),
                    current: read_file_state(root, None, dir.old.as_str()),
                }));
            }
            Err(e) => {
                undo.run(root, archive, dir, op_id, None, &source, true, None);
                return Err(e.into());
            }
        }
        undo.staged = true;
        let staged_st = root.stat(tmp_rel)?;
        if (staged_st.dev, staged_st.inode) != (now_st.dev, now_st.inode) {
            undo.run(
                root,
                archive,
                dir,
                op_id,
                Some(tmp_rel),
                &source,
                true,
                None,
            );
            return Ok(Err(Placed::Conflict {
                reason: "反映の直前に元ファイルが差し替えられた".to_owned(),
                current: read_file_state(root, None, dir.old.as_str()),
            }));
        }
    }
    let staged_stat = fsroot::fstat(&source)?;
    Ok(Ok(Staged {
        source,
        source_hash,
        staged_stat,
        undo,
        archived_already,
    }))
}

/// 破壊フェーズ 2（同期）: Archive へ実コピーし、書いた内容を読み戻して照合する
fn phase_archive(
    archive: &RootDir,
    dir: &Direction,
    source: &mut File,
    source_hash: [u8; 32],
) -> Result<Result<Made, Placed>, EditError> {
    let archive_parent = dir.old.parent();
    if let Some(p) = &archive_parent {
        archive.create_dir_all(p)?;
    }
    let (tmp_rel, mut tmp) = archive.create_tmp(archive_parent.as_ref())?;
    let copied = (|| -> Result<Result<Made, Placed>, EditError> {
        let written = copy_hashing(source, &mut tmp)?;
        if written != source_hash {
            // 読んだ内容が最初のハッシュと違う = コピーの間に同じ inode が更新された
            return Ok(Err(Placed::Conflict {
                reason: "退避の間に元ファイルが外部で更新された".to_owned(),
                current: None,
            }));
        }
        fsroot::copy_attrs(source, &tmp)?;
        tmp.sync_all()?;
        let readback = sha256_of(&mut tmp)?;
        if readback != source_hash {
            return Err(EditError::Internal(
                "Archive へ書いた内容を読み戻すと元と一致しない".to_owned(),
            ));
        }
        archive.rename_noreplace(&tmp_rel, &dir.old)?;
        archive.fsync_dir(archive_parent.as_ref())?;
        Ok(Ok(Made {
            stat: fsroot::fstat(&tmp)?,
            sha256: source_hash,
        }))
    })();
    if !matches!(copied, Ok(Ok(_))) {
        if let Err(u) = archive.unlink(&tmp_rel) {
            if !matches!(u, FsError::NotFound) {
                tracing::warn!(path = %tmp_rel, error = %u, "Archive の tmp を消せない");
            }
        }
    }
    copied
}

/// 破壊フェーズ 3a（同期）: unlink の前の最終照合。同じ FD の stat とバイト列、一時名にあるのが
/// その FD の実体であること、宛先にあるのが期待した音声であることを確かめ、DB に書く宛先の状態を
/// 読む。ここまでは全て可逆（元は無傷）
fn phase_verify(
    root: &RootDir,
    dir: &Direction,
    tmp_rel: &RelPath,
    source: &mut File,
    source_stat: fsroot::Stat,
    source_hash: [u8; 32],
    expected_md5: [u8; 16],
) -> Result<Result<(FileState, Ident), Placed>, EditError> {
    let now_st = fsroot::fstat(source)?;
    if !same_stat(&source_stat, &now_st) || sha256_of(source)? != source_hash {
        return Ok(Err(Placed::Conflict {
            reason: "退避の間に元ファイルが外部で更新された".to_owned(),
            current: None,
        }));
    }
    match root.stat(tmp_rel) {
        Ok(st) if Ident::of(&now_st).matches(&st) => {}
        _ => {
            return Ok(Err(Placed::Conflict {
                reason: "退避の間に一時名の元ファイルが差し替えられた".to_owned(),
                current: None,
            }))
        }
    }
    let Some(state) = read_file_state(root, None, dir.new.as_str()) else {
        return Err(EditError::Internal(format!(
            "置いたファイルを読み直せない: {}",
            dir.new
        )));
    };
    if state.fp != Some(Fingerprint::Md5(Some(expected_md5))) {
        return Ok(Err(Placed::Conflict {
            reason: format!("退避の間に宛先が差し替えられた: {}", dir.new),
            current: None,
        }));
    }
    Ok(Ok((state, Ident::of(&now_st))))
}

/// 確定点の結果
enum Unlinked {
    /// unlink した（不可逆）
    Done,
    /// unlink の直前に一時名の実体が変わっていた。何も消していない（可逆）
    Conflict(Box<Placed>),
    /// unlink の前に失敗した。何も消していない（可逆。再試行）
    FailedBefore(EditError),
    /// unlink の後（dir の fsync）に失敗した。宛先と Archive は残す（再試行が反映済みで確定する）
    FailedAfter(EditError),
}

/// 破壊フェーズ 3b（同期）: **不可逆な確定点**。一時名を unlink する（物理削除ではなく move の
/// 後半）。ここから先の失敗（fsync 等）では宛先も Archive も消さない。再投入されたジョブが
/// 反映済み（AlreadyDone）として確定する
fn phase_unlink(root: &RootDir, dir: &Direction, tmp_rel: &RelPath, verified: Ident) -> Unlinked {
    // stat と unlink の間の窓は縮められない（排他ロックはしない。不変条件 5）が、同じ関数の中で
    // 続けて行い、async 境界を挟まない
    match root.stat(tmp_rel) {
        Ok(st) if verified.matches(&st) => {}
        Ok(_) => {
            return Unlinked::Conflict(Box::new(Placed::Conflict {
                reason: "確定の直前に一時名の元ファイルが差し替えられた".to_owned(),
                current: None,
            }))
        }
        Err(FsError::NotFound) => {
            return Unlinked::Conflict(Box::new(Placed::Conflict {
                reason: "確定の直前に一時名の元ファイルが無くなった".to_owned(),
                current: None,
            }))
        }
        Err(e) => return Unlinked::FailedBefore(e.into()),
    }
    if let Err(e) = root.unlink(tmp_rel) {
        return match e {
            FsError::NotFound => Unlinked::Conflict(Box::new(Placed::Conflict {
                reason: "確定の直前に一時名の元ファイルが無くなった".to_owned(),
                current: None,
            })),
            other => Unlinked::FailedBefore(other.into()),
        };
    }
    match root.fsync_dir(dir.old.parent().as_ref()) {
        Ok(()) => Unlinked::Done,
        Err(e) => Unlinked::FailedAfter(e.into()),
    }
}

// ---------------------------------------------------------------- Editor

impl Editor {
    fn normalize_env(&self) -> Result<&NormalizeEnv, EditError> {
        self.normalize.as_ref().ok_or_else(|| {
            EditError::Internal("正規化の環境（Archive / エンコーダ）が無い".to_owned())
        })
    }

    /// 対象トラックの宛先を計画する（preview）。DB もファイルも書かない
    pub async fn plan_normalize(&self, ids: &[i64]) -> Result<Vec<PlannedNormalize>, EditError> {
        let ids = ids.to_vec();
        self.db
            .read(move |c| Ok(plan_normalize_tx(c, &ids)))
            .await?
    }

    /// 正規化バッチを記録し、track 単位の normalize ジョブを投入する
    pub async fn prepare_normalize(
        &self,
        description: Option<&str>,
        targets: Vec<NormalizeTarget>,
    ) -> Result<Prepared, EditError> {
        self.normalize_env()?;
        let description = description.map(str::to_owned);
        let prepared = self
            .db
            .write(move |c| {
                Ok(prepare_normalize_tx(
                    c,
                    description.as_deref(),
                    &targets,
                    None,
                    now_epoch(),
                ))
            })
            .await??;
        self.jobs.notify_enqueued(&prepared.job_ids).await;
        if let Some(ev) = &prepared.event {
            self.jobs.publish(Event::Batch(ev.clone()));
        }
        Ok(prepared)
    }

    /// pending の archive op を反映する（normalize ジョブの本体。何度呼んでも結果は同じ）。
    /// `token` が倒れたら、Library を触る前なら [`EditError::Cancelled`] で止まる
    pub async fn apply_archive_op(
        &self,
        op_id: i64,
        job_id: Option<i64>,
        token: &CancellationToken,
    ) -> Result<OpOutcome, EditError> {
        let env = self.normalize_env()?.clone();
        let (op, edits, current_rel_path) = self.load_op(op_id).await?;
        if op.result != OpResult::Pending {
            return Ok(OpOutcome::AlreadyTerminal(op.result));
        }
        if op.kind != OpKind::Archive {
            return Err(EditError::UnsupportedKind(op.kind));
        }
        let dir = Arc::new(direction_of(op.id, &edits)?);
        self.mark_applying(op.batch_id).await?;

        let track_id = op.track_id;
        let (db_md5, db_tag_hash, db_tag_version): (Option<[u8; 16]>, Option<Vec<u8>>, i64) = self
            .db
            .read(move |c| {
                let row = scans::load_current_row(c, track_id)?;
                Ok((
                    row.audio_md5,
                    row.tag_hash.map(|h| h.to_vec()),
                    row.tag_version,
                ))
            })
            .await?;

        // 記録の後に外部 rename をスキャナが追随していれば、op の前提（パス）が崩れている
        let staged: Placed = if current_rel_path != dir.old.as_str() {
            Placed::Conflict {
                reason: format!("記録の後に移動された: {current_rel_path}"),
                current: None,
            }
        } else {
            let recorded_hash = recorded_source_hash(&edits);
            self.stage_archive(&env, &op, &dir, db_md5, recorded_hash, token)
                .await?
        };

        if !matches!(staged, Placed::Applied(_)) {
            // 終端（conflict / failed）にするので、一時名に残った元ファイルがあれば元パスへ戻す
            let root = Arc::clone(&self.root);
            let (op_id, expected, edits) = (op.id, op.expected.clone(), edits.clone());
            tokio::task::spawn_blocking(move || {
                restore_staged_source_of(&root, op_id, &expected, &edits)
            })
            .await?;
        }
        let batch_id = op.batch_id;
        let env_retention = env.retention_days;
        let reference = self.rg_reference;
        let dir_for_db = Arc::clone(&dir);
        let (outcome, event) = self
            .db
            .write(move |c| {
                let tx = c.transaction()?;
                let now = now_epoch();
                let outcome = match staged {
                    Placed::Applied(fs) => {
                        history::finish_op(&tx, op.id, OpResult::Applied, None, job_id, now)?;
                        commit_applied(
                            &tx,
                            &op,
                            &dir_for_db,
                            &fs,
                            db_tag_hash.as_deref(),
                            db_tag_version,
                            env_retention,
                            now,
                        )?;
                        OpOutcome::Applied
                    }
                    Placed::Conflict { reason, current } => {
                        if history::finish_op(
                            &tx,
                            op.id,
                            OpResult::SkippedConflict,
                            Some(&reason),
                            job_id,
                            now,
                        )? {
                            if let Some(fs) = current.as_ref() {
                                // 正規化の元ファイルは画像を読まない（Unread）ので追随ジョブは出ない
                                let _ = sync_track_to_file(&tx, op.track_id, fs, reference, now)?;
                            }
                        }
                        OpOutcome::Conflict(reason)
                    }
                    Placed::Failed(reason) => {
                        history::finish_op(
                            &tx,
                            op.id,
                            OpResult::Failed,
                            Some(&reason),
                            job_id,
                            now,
                        )?;
                        OpOutcome::Failed(reason)
                    }
                };
                let event = match history::aggregate_batch(&tx, batch_id, now)? {
                    Some(state) => Some(batch_event(&tx, batch_id, state)?),
                    None => None,
                };
                tx.commit()?;
                Ok((outcome, event))
            })
            .await?;
        if let Some(ev) = event {
            tracing::info!(batch_id, state = %ev.state, applied = ev.applied, conflict = ev.conflict, failed = ev.failed, "正規化バッチが終端になった");
            self.jobs.publish(Event::Batch(ev));
        }
        Ok(outcome)
    }

    /// ファイル側の全段（確認 → 用意 → 配置 → 退避）
    async fn stage_archive(
        &self,
        env: &NormalizeEnv,
        op: &history::Op,
        dir: &Arc<Direction>,
        db_md5: Option<[u8; 16]>,
        recorded_hash: Option<[u8; 32]>,
        token: &CancellationToken,
    ) -> Result<Placed, EditError> {
        let inspected = {
            let root = Arc::clone(&self.root);
            let archive = Arc::clone(&env.archive);
            let dir = Arc::clone(dir);
            let expected = op.expected.clone();
            let op_id = op.id;
            tokio::task::spawn_blocking(move || {
                inspect_source(
                    &root,
                    &archive,
                    &dir,
                    op_id,
                    &expected,
                    db_md5,
                    recorded_hash,
                )
            })
            .await??
        };
        let (source, stat, af, source_rel, expected_md5) = match inspected {
            Inspected::Ready {
                source,
                stat,
                af,
                source_rel,
                expected_md5,
            } => (source, stat, af, source_rel, expected_md5),
            Inspected::AlreadyDone(fs) => {
                return Ok(Placed::Applied(FileState {
                    fp: Some(Fingerprint::Md5(db_md5)),
                    ..fs
                }))
            }
            Inspected::Conflict { reason, current } => {
                return Ok(Placed::Conflict { reason, current })
            }
            Inspected::Failed(reason) => return Ok(Placed::Failed(reason)),
        };

        // 2. 置くファイルを用意する
        let new_path = dir.new.as_str().to_owned();
        let held = self
            .db
            .read(move |c| archive::get_by_rel_path(c, &new_path))
            .await?
            .filter(|a| a.state == ArchiveState::Held);
        let incoming = match held {
            Some(_) => Incoming::Restore,
            None => {
                let new_is_flac = ext_of(&dir.new).is_some_and(|e| e.eq_ignore_ascii_case("flac"));
                if !(new_is_flac && NORMALIZE_SOURCES.contains(&af.codec)) {
                    return Ok(Placed::Failed(format!(
                        "復元元が Archive に無い: {}",
                        dir.new
                    )));
                }
                let Some(bit_depth) = af.bit_depth else {
                    return Ok(Placed::Failed("ビット深度が読めない".to_owned()));
                };
                let reader = {
                    let mut f = source.try_clone()?;
                    f.seek(SeekFrom::Start(0))?;
                    f
                };
                let encoded = match env.encoder.encode(reader, bit_depth, token).await {
                    Ok(e) => e,
                    Err(e) if e.is_cancelled() => return Err(EditError::Cancelled),
                    Err(crate::media::encode::EncodeError::UnsupportedBitDepth(b)) => {
                        return Ok(Placed::Failed(format!("対応していないビット深度: {b}")))
                    }
                    Err(e) => return Err(EditError::Internal(format!("エンコードに失敗: {e}"))),
                };
                match encoded.streaminfo_md5 {
                    Some(m) if m == expected_md5 => {}
                    got => {
                        tracing::warn!(
                            op_id = op.id,
                            path = %dir.old,
                            expected = %hex(&expected_md5),
                            got = ?got.map(|m| hex(&m)),
                            "PCM MD5 が一致しない。正規化を中止する"
                        );
                        return Ok(Placed::Failed(
                            "変換前後の PCM MD5 が一致しない（生成した FLAC は捨てた）".to_owned(),
                        ));
                    }
                }
                // タグと画像を写す（tmp の FLAC に直接。まだ Library には無い）
                let tags = {
                    let mut f = source.try_clone()?;
                    f.seek(SeekFrom::Start(0))?;
                    let ext = ext_of(&dir.old).map(str::to_owned);
                    let path = encoded.guard.path().to_path_buf();
                    tokio::task::spawn_blocking(move || -> Result<(), EditError> {
                        let t = read_transfer_tags(f, ext.as_deref())?;
                        let mut out = std::fs::OpenOptions::new()
                            .read(true)
                            .write(true)
                            .open(&path)?;
                        write_flac_tags(&mut out, &t)?;
                        out.sync_all()?;
                        Ok(())
                    })
                    .await?
                };
                tags?;
                Incoming::Encoded(encoded.guard)
            }
        };

        // ここから Library を触る。cancel はここまで
        if token.is_cancelled() {
            return Err(EditError::Cancelled);
        }

        let incoming_file = match &incoming {
            Incoming::Encoded(guard) => File::open(guard.path())?,
            Incoming::Restore => {
                let f = env.archive.open_file(&dir.new)?;
                // 退避ファイルの音声が今の track と一致することを確かめてから戻す
                let m = {
                    let f = f.try_clone()?;
                    let rel = dir.new.clone();
                    tokio::task::spawn_blocking(move || audio_md5_of(f, &rel)).await?
                };
                match m {
                    Ok(Some(m)) if m == expected_md5 => {}
                    Ok(_) => {
                        return Ok(Placed::Failed(format!(
                            "Archive の退避ファイルの音声が一致しない: {}",
                            dir.new
                        )))
                    }
                    Err(e) => {
                        return Ok(Placed::Failed(format!(
                            "Archive の退避ファイルを読めない: {}: {e}",
                            dir.new
                        )))
                    }
                }
                f
            }
        };
        let Some(tmp_rel) = normalize_temp_rel_path(op.id, &dir.old) else {
            return Ok(Placed::Failed(format!("一時名を作れないパス: {}", dir.old)));
        };
        let tmp_rel = Arc::new(tmp_rel);
        let root = Arc::clone(&self.root);
        let archive = Arc::clone(&env.archive);

        // 破壊フェーズ 1: 宛先を置き、元を一時名へ退避（失敗・衝突は自分の生成物を戻して返す）
        let op_id = op.id;
        let staged = {
            let (root, archive, dir, tmp_rel) = (
                Arc::clone(&root),
                Arc::clone(&archive),
                Arc::clone(dir),
                Arc::clone(&tmp_rel),
            );
            tokio::task::spawn_blocking(move || {
                phase_place(
                    &root,
                    &archive,
                    &dir,
                    op_id,
                    &tmp_rel,
                    source,
                    stat,
                    &source_rel,
                    incoming_file,
                    expected_md5,
                )
            })
            .await??
        };
        drop(incoming);
        let Staged {
            mut source,
            source_hash,
            staged_stat,
            mut undo,
            archived_already,
        } = match staged {
            Ok(s) => s,
            Err(placed) => return Ok(placed),
        };

        // ここから確定点（unlink）までの中断（I/O 失敗・フックの Err）は、自分の生成物を戻してから返す。
        // 同じ file description の複製を渡し、元の FD は undo（書き戻し）のために手元に残す
        let source_for_undo = source.try_clone()?;
        let pre_commit: Result<Result<(FileState, Ident), Placed>, EditError> = async {
            self.call_normalize_hook(NormalizeStep::Staged(op.id))
                .await?;
            // unlink の前にコンテナの SHA-256 を記録する（クラッシュ後の復旧が Archive の実体を
            // これと照合する）
            let op_id = op.id;
            let hash_hex = hex(&source_hash);
            self.db
                .write(move |c| record_source_hash(c, op_id, &hash_hex))
                .await?;

            // 破壊フェーズ 2: Archive へコピー（既に同じ内容があれば省く）
            if !archived_already {
                let (archive, dir) = (Arc::clone(&archive), Arc::clone(dir));
                let (s, r) = tokio::task::spawn_blocking(move || {
                    let r = phase_archive(&archive, &dir, &mut source, source_hash);
                    (source, r)
                })
                .await?;
                source = s;
                match r? {
                    Ok(ident) => undo.archived = Some(ident),
                    Err(placed) => return Ok(Err(placed)),
                }
            }
            self.call_normalize_hook(NormalizeStep::BeforeUnlink(op.id))
                .await?;

            // 破壊フェーズ 3a: 最終照合と宛先の読み取り
            let (root, dir, tmp_rel) = (Arc::clone(&root), Arc::clone(dir), Arc::clone(&tmp_rel));
            let (s, r) = tokio::task::spawn_blocking(move || {
                let r = phase_verify(
                    &root,
                    &dir,
                    &tmp_rel,
                    &mut source,
                    staged_stat,
                    source_hash,
                    expected_md5,
                );
                (source, r)
            })
            .await?;
            drop(s);
            r
        }
        .await;
        let (state, verified) = match pre_commit {
            Ok(Ok(v)) => v,
            Ok(Err(placed)) => {
                return self
                    .undo_conflict(
                        undo,
                        &root,
                        &archive,
                        dir,
                        op_id,
                        &tmp_rel,
                        &source_for_undo,
                        placed,
                    )
                    .await;
            }
            Err(e) => {
                // 再試行する: 生成物は消すが元は一時名のまま（次の試行が続きを行う）
                self.undo_staged(
                    undo,
                    &root,
                    &archive,
                    dir,
                    op_id,
                    &tmp_rel,
                    &source_for_undo,
                    false,
                )
                .await?;
                return Err(e);
            }
        };
        if let Err(e) = self
            .call_normalize_hook(NormalizeStep::Verified(op.id))
            .await
        {
            self.undo_staged(
                undo,
                &root,
                &archive,
                dir,
                op_id,
                &tmp_rel,
                &source_for_undo,
                false,
            )
            .await?;
            return Err(e);
        }

        // 破壊フェーズ 3b: 確定点。unlink の直前に同一性を再確認し、unlink したら何があっても
        // undo しない（元・宛先・Archive のうち元だけが消え、宛先と Archive は残る。失敗は再試行が
        // AlreadyDone で確定する）
        let unlinked = {
            let (root, dir, tmp_rel) = (Arc::clone(&root), Arc::clone(dir), Arc::clone(&tmp_rel));
            tokio::task::spawn_blocking(move || phase_unlink(&root, &dir, &tmp_rel, verified))
                .await?
        };
        match unlinked {
            Unlinked::Done => {}
            Unlinked::Conflict(placed) => {
                let placed = *placed;
                return self
                    .undo_conflict(
                        undo,
                        &root,
                        &archive,
                        dir,
                        op_id,
                        &tmp_rel,
                        &source_for_undo,
                        placed,
                    )
                    .await;
            }
            Unlinked::FailedBefore(e) => {
                self.undo_staged(
                    undo,
                    &root,
                    &archive,
                    dir,
                    op_id,
                    &tmp_rel,
                    &source_for_undo,
                    false,
                )
                .await?;
                return Err(e);
            }
            Unlinked::FailedAfter(e) => return Err(e),
        }
        drop(source_for_undo);
        self.call_normalize_hook(NormalizeStep::AfterUnlink(op.id))
            .await?;
        Ok(Placed::Applied(state))
    }

    /// conflict で止まる: 元は無傷なので戻し、自分の生成物（宛先・Archive の未確定コピー）を消す。
    /// 元を回収パスへしか残せなかったときはその所在を理由に添える
    #[allow(clippy::too_many_arguments)]
    async fn undo_conflict(
        &self,
        undo: Undo,
        root: &Arc<RootDir>,
        archive: &Arc<RootDir>,
        dir: &Arc<Direction>,
        op_id: i64,
        tmp_rel: &Arc<RelPath>,
        source: &File,
        placed: Placed,
    ) -> Result<Placed, EditError> {
        let kept = self
            .undo_staged(undo, root, archive, dir, op_id, tmp_rel, source, true)
            .await?;
        let current = {
            let (root, dir) = (Arc::clone(root), Arc::clone(dir));
            tokio::task::spawn_blocking(move || read_file_state(&root, None, dir.old.as_str()))
                .await?
        };
        Ok(match placed {
            Placed::Conflict { reason, .. } => {
                let reason = match kept.note() {
                    Some(note) => format!("{reason}（{note}）"),
                    None => reason,
                };
                Placed::Conflict { reason, current }
            }
            other => other,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn undo_staged(
        &self,
        undo: Undo,
        root: &Arc<RootDir>,
        archive: &Arc<RootDir>,
        dir: &Arc<Direction>,
        op_id: i64,
        tmp_rel: &Arc<RelPath>,
        source: &File,
        restore_source: bool,
    ) -> Result<UndoResult, EditError> {
        let (root, archive, dir, tmp_rel) = (
            Arc::clone(root),
            Arc::clone(archive),
            Arc::clone(dir),
            Arc::clone(tmp_rel),
        );
        let source = source.try_clone()?;
        let hook = self
            .normalize_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Ok(tokio::task::spawn_blocking(move || {
            undo.run(
                &root,
                &archive,
                &dir,
                op_id,
                Some(&tmp_rel),
                &source,
                restore_source,
                hook.as_ref(),
            )
        })
        .await?)
    }

    /// テスト用: 破壊フェーズの境界で呼ばれるフックを置く
    #[doc(hidden)]
    pub fn set_normalize_hook(&self, hook: NormalizeHook) {
        *self
            .normalize_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    /// フックはブロックし得るので、ランタイムのスレッドでは呼ばない
    async fn call_normalize_hook(&self, step: NormalizeStep) -> Result<(), EditError> {
        let hook = self
            .normalize_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(h) = hook else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || h(step))
            .await?
            .map_err(|e| EditError::Internal(format!("中断: {e}")))
    }
}

/// 元ファイルの SHA-256 を op の edits に記録する（unlink の前）。既にあれば何もしない
fn record_source_hash(conn: &Connection, op_id: i64, hash_hex: &str) -> crate::db::Result<()> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM edits WHERE op_id = ?1 AND key = ?2",
            params![op_id, SOURCE_HASH_KEY],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_none() {
        history::insert_edit(
            conn,
            op_id,
            SOURCE_HASH_KEY,
            &serde_json::Value::Null,
            &serde_json::json!(hash_hex),
        )?;
    }
    Ok(())
}

/// edits に記録した元ファイルの SHA-256
fn recorded_source_hash(edits: &[history::Edit]) -> Option<[u8; 32]> {
    let hex = edits
        .iter()
        .find(|e| e.key == SOURCE_HASH_KEY)?
        .new_value
        .as_str()?;
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// applied の DB 確定: パス・物理属性・内容・MD5・出自・台帳
#[allow(clippy::too_many_arguments)]
fn commit_applied(
    tx: &Connection,
    op: &history::Op,
    dir: &Direction,
    fs: &FileState,
    db_tag_hash: Option<&[u8]>,
    db_tag_version: i64,
    retention_days: u32,
    now: i64,
) -> crate::db::Result<()> {
    let track_id = op.track_id;
    scans::move_track_paths(
        tx,
        &[(track_id, dir.new.as_str().to_owned(), dir.new.key())],
    )?;
    history::set_track_physical(tx, track_id, &fs.ph)?;
    // 写したタグが元と違うときだけ tag_version を進める（同値の書き直しでは進めない。SPEC §6）
    let tag_version = if db_tag_hash == Some(fs.content.tag_hash.as_slice()) {
        db_tag_version
    } else {
        db_tag_version + 1
    };
    scans::update_content(tx, track_id, &fs.content, tag_version)?;
    if let Some(fp) = fs.fp {
        let audio_version: i64 = tx.query_row(
            "SELECT audio_version FROM tracks WHERE id = ?1",
            [track_id],
            |r| r.get(0),
        )?;
        // 同じ PCM なので audio_version は据え置く（不変条件 3）
        scans::update_fingerprint(tx, track_id, fp, audio_version)?;
    }
    let new_is_flac = dir.new_codec == "flac";
    let old_is_flac = dir.old_codec == "flac";
    if new_is_flac && !old_is_flac {
        tx.execute(
            "UPDATE tracks SET original_codec = ?2, normalized_at = ?3 WHERE id = ?1",
            params![track_id, dir.old_codec, now],
        )?;
    } else if old_is_flac && !new_is_flac {
        tx.execute(
            "UPDATE tracks SET original_codec = NULL, normalized_at = NULL WHERE id = ?1",
            [track_id],
        )?;
    }
    // 台帳: Library から外した <旧> を held に、Archive から戻した <新> を restored に
    let reason = if old_is_flac {
        ArchiveReason::Restore
    } else {
        ArchiveReason::Normalize
    };
    archive::record_held(
        tx,
        track_id,
        Some(op.id),
        dir.old.as_str(),
        dir.old.as_str(),
        reason,
        now,
        retention_days,
    )?;
    if let Some(row) = archive::get_by_rel_path(tx, dir.new.as_str())? {
        if row.state == ArchiveState::Held {
            archive::set_state(tx, row.id, ArchiveState::Restored, now)?;
        }
    }
    Ok(())
}

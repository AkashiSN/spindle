//! FLAC の MD5 補填（SPEC §7.9、P1-5b、D-57 / D-59）。
//!
//! STREAMINFO の MD5 が全ゼロ（未設定）の FLAC に、デコードした PCM MD5 を書く編集バッチ。
//! 再エンコードはせず **STREAMINFO の 16 バイトだけ**を tmp + rename で書き換えるので、音声も
//! タグも変わらず `audio_version` / `tag_version` は据え置き。物理属性（inode / mtime）は新しい
//! 実体へ追随する。
//!
//! - op は `kind = 'md5'`、edits は `key = 'audio_md5'`、値は 32 桁の hex 文字列（全ゼロ = 未設定）。
//!   `new_value` は記録時 null（デコードして初めて分かる）で、反映時に計算値を書く。巻き戻しは
//!   逆向きの md5 op（old = 計算値、new = 全ゼロ。`new_value` があれば計算せずそれを書く）
//! - DB は先行更新しない（overlay が無い。archive op と同じ）。反映の同じトランザクションで
//!   `audio_md5` と `flac_check`（値あり → `ok`、全ゼロ → `md5_missing`）を揃える
//! - ジョブは tagwrite を再利用する（契約は「そのトラックの pending op をファイルへ反映する」で
//!   同じ。`track_locks`・stale ゲート・再試行・最終失敗で op を閉じる仕組みがそのまま効く）

use std::io::{Seek, SeekFrom, Write};
use std::sync::Arc;

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::db::flaccheck::{self as dbfc, Status};
use crate::db::history::{self, Op, OpKind, OpResult};
use crate::db::jobs as dbjobs;
use crate::db::now_epoch;
use crate::db::scans::Physical;
use crate::domain::relpath::RelPath;
use crate::fsroot::{self, FsError, RootDir};
use crate::jobs::{BatchEvent, Event, JobType, NewJob};
use crate::media::fingerprint::{decoded_pcm_md5, flac_streaminfo_md5_at, FingerprintError};

use super::{batch_event, BeforeRenameHook, EditError, Editor, OpOutcome};

/// edits のキー
pub const MD5_EDIT_KEY: &str = "audio_md5";
/// 未設定（全ゼロ）の hex 表現
pub const MD5_ZERO_HEX: &str = "00000000000000000000000000000000";

/// `prepare_md5_fill` の結果
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Md5FillPrepared {
    /// 記録したバッチ。対象が無ければ None（呼び出し側は `NoChanges` を受ける）
    pub batch_id: Option<i64>,
    /// 記録した op 数
    pub affected: usize,
    /// FLAC でない・欠落・`md5_missing` でないため対象外にした行数
    pub skipped: usize,
    pub job_ids: Vec<i64>,
    #[serde(skip)]
    pub event: Option<BatchEvent>,
}

/// md5 op を反映する tagwrite ジョブ。md5 op は tags overlay を持たず `tag_version` も進めないので、
/// - 版の stale ゲートは通さない（`unversioned`。外部のタグ変更で版が進んでも補填は有効）
/// - dedup key は op ごとに一意（tags と同じ `tagwrite:<id>:<ver>` だと、元ジョブが running のうちに
///   巻き戻したとき逆 op のジョブが Duplicate になって走らない）
///
/// `track_locks` は track_id で通常どおり取る（同じトラックの書き手と直列化）
pub(super) fn md5_job(track_id: i64, op_id: i64, batch_id: i64) -> NewJob {
    NewJob::new(
        JobType::Tagwrite,
        serde_json::json!({
            "track_id": track_id,
            "op_id": op_id,
            "batch_id": batch_id,
            dbjobs::UNVERSIONED_KEY: true,
        }),
    )
    .dedup_key(format!("tagwrite:{track_id}:md5:{op_id}"))
    .edit_batch_id(batch_id)
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn parse_md5_hex(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// edits の値（hex 文字列 / null）→ MD5。null は None
fn md5_of_value(v: &serde_json::Value) -> Result<Option<[u8; 16]>, EditError> {
    match v {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(s) => parse_md5_hex(s)
            .map(Some)
            .ok_or_else(|| EditError::Internal(format!("audio_md5 の値が hex でない: {s:?}"))),
        other => Err(EditError::Internal(format!(
            "audio_md5 の値が文字列でない: {other}"
        ))),
    }
}

impl Editor {
    /// selection の FLAC のうち `flac_check = 'md5_missing'` で active なものに md5 op を記録し、
    /// track 単位の tagwrite ジョブを投入する。対象トラックに pending の op があれば
    /// [`EditError::Pending`]、対象が 1 件も無ければ [`EditError::NoChanges`]
    pub async fn prepare_md5_fill(
        &self,
        description: Option<&str>,
        track_ids: Vec<i64>,
    ) -> Result<Md5FillPrepared, EditError> {
        let description = description.map(str::to_owned);
        let prepared = self
            .db
            .write(move |c| {
                Ok(prepare_md5_fill_tx(
                    c,
                    description.as_deref(),
                    &track_ids,
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

    /// md5 op の反映（`apply_op` から。op は pending、kind は Md5）
    pub(super) async fn apply_md5_op(
        &self,
        op: Op,
        mut edits: Vec<history::Edit>,
        rel_path: String,
        job_id: Option<i64>,
    ) -> Result<OpOutcome, EditError> {
        let hook = self
            .before_rename
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let stage =
            |edits: Vec<history::Edit>, root: Arc<RootDir>, hook: Option<BeforeRenameHook>| {
                let op = op.clone();
                let rel_path = rel_path.clone();
                tokio::task::spawn_blocking(move || {
                    stage_md5(&root, &op, &edits, &rel_path, hook.as_ref())
                })
            };
        let mut staged = stage(edits.clone(), Arc::clone(&self.root), hook.clone()).await??;
        // 補填（new_value が null）は 2 段階: 計算値を rename の前に edits へ耐久化してから書く。
        // rename 後・DB 確定前に落ちても、再実行で「ファイルは新値」と分かり applied に確定できる
        if let Md5Staged::Computed(md5) = staged {
            let op_id = op.id;
            self.db
                .write(move |c| set_edit_new_value(c, op_id, &md5))
                .await?;
            for e in edits.iter_mut().filter(|e| e.key == MD5_EDIT_KEY) {
                e.new_value = serde_json::json!(hex(&md5));
            }
            staged = stage(edits, Arc::clone(&self.root), hook).await??;
            if matches!(staged, Md5Staged::Computed(_)) {
                return Err(EditError::Internal(
                    "new_value を耐久化した後も計算だけで戻った".to_owned(),
                ));
            }
        }
        let batch_id = op.batch_id;
        let (outcome, event) = self
            .db
            .write(move |c| {
                let tx = c.transaction()?;
                let now = now_epoch();
                let outcome = match staged {
                    Md5Staged::Written { ph, md5 } => {
                        history::finish_op(&tx, op.id, OpResult::Applied, None, job_id, now)?;
                        history::set_track_physical(&tx, op.track_id, &ph)?;
                        set_md5(&tx, op.track_id, md5, now)?;
                        OpOutcome::Applied
                    }
                    Md5Staged::Computed(_) => {
                        return Err(crate::db::DbError::Internal(
                            "計算のみの結果が残った".to_owned(),
                        ))
                    }
                    Md5Staged::Conflict { reason, current } => {
                        if history::finish_op(
                            &tx,
                            op.id,
                            OpResult::SkippedConflict,
                            Some(&reason),
                            job_id,
                            now,
                        )? {
                            // ファイルが正: 現在値（補填済み等）を DB に揃える
                            if let Some((ph, md5)) = current {
                                history::set_track_physical(&tx, op.track_id, &ph)?;
                                set_md5(&tx, op.track_id, md5, now)?;
                            }
                        }
                        OpOutcome::Conflict(reason)
                    }
                    Md5Staged::Failed { reason } => {
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
            tracing::info!(batch_id, state = %ev.state, applied = ev.applied, conflict = ev.conflict, failed = ev.failed, "MD5 補填バッチが終端になった");
            self.jobs.publish(Event::Batch(ev));
        }
        Ok(outcome)
    }
}

/// `audio_md5` と検査結果を書いた値に揃える。全ゼロは未設定（NULL / `md5_missing`）。
/// 音声は変わらないので `audio_version` は据え置き
fn set_md5(tx: &Connection, track_id: i64, md5: [u8; 16], now: i64) -> crate::db::Result<()> {
    let (value, status) = if md5 == [0u8; 16] {
        (None, Status::Md5Missing)
    } else {
        (Some(md5.to_vec()), Status::Ok)
    };
    tx.execute(
        "UPDATE tracks SET audio_md5 = ?2 WHERE id = ?1",
        params![track_id, value],
    )?;
    let audio_version: i64 = tx.query_row(
        "SELECT audio_version FROM tracks WHERE id = ?1",
        [track_id],
        |r| r.get(0),
    )?;
    dbfc::record(tx, track_id, audio_version, status, None, now)?;
    Ok(())
}

/// 計算した値を `edits.new_value` に残す（rename の前に耐久化する。巻き戻しの old になる）
fn set_edit_new_value(tx: &Connection, op_id: i64, md5: &[u8; 16]) -> crate::db::Result<()> {
    tx.execute(
        "UPDATE edits SET new_value = ?3 WHERE op_id = ?1 AND key = ?2",
        params![op_id, MD5_EDIT_KEY, serde_json::json!(hex(md5)).to_string()],
    )?;
    Ok(())
}

/// 対象の行（トランザクション本体が読む）
struct Candidate {
    id: i64,
    codec: String,
    missing: bool,
    flac_check: Option<String>,
}

fn load_candidates(conn: &Connection, ids: &[i64]) -> rusqlite::Result<Vec<Candidate>> {
    let json = serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_owned());
    let mut stmt = conn.prepare_cached(
        "SELECT id, codec, missing_since IS NOT NULL, flac_check
         FROM tracks WHERE id IN (SELECT value FROM json_each(?1)) ORDER BY id",
    )?;
    let rows = stmt.query_map([json], |r| {
        Ok(Candidate {
            id: r.get(0)?,
            codec: r.get(1)?,
            missing: r.get(2)?,
            flac_check: r.get(3)?,
        })
    })?;
    rows.collect()
}

fn prepare_md5_fill_tx(
    conn: &mut Connection,
    description: Option<&str>,
    track_ids: &[i64],
    now: i64,
) -> Result<Md5FillPrepared, EditError> {
    let tx = conn.transaction()?;
    let mut ids: Vec<i64> = track_ids.to_vec();
    ids.sort_unstable();
    if let Some(w) = ids.windows(2).find(|w| w[0] == w[1]) {
        return Err(EditError::DuplicateTrack(w[0]));
    }
    let pending = history::pending_track_ids(&tx, &ids)?;
    if !pending.is_empty() {
        return Err(EditError::Pending { track_ids: pending });
    }
    let rows = load_candidates(&tx, &ids)?;
    if let Some(missing) = ids.iter().find(|id| !rows.iter().any(|r| r.id == **id)) {
        return Err(EditError::TrackNotFound(*missing));
    }
    let (targets, skipped): (Vec<&Candidate>, Vec<&Candidate>) = rows.iter().partition(|r| {
        r.codec == "flac"
            && !r.missing
            && r.flac_check.as_deref() == Some(Status::Md5Missing.as_str())
    });
    if targets.is_empty() {
        return Err(EditError::NoChanges);
    }
    let batch_id = history::insert_batch(&tx, description, targets.len() as i64, None, now)?;
    let mut job_ids = Vec::with_capacity(targets.len());
    for (ordinal, t) in targets.iter().enumerate() {
        let expected = history::precondition_of_track(&tx, t.id)?.unwrap_or_default();
        let op_id =
            history::insert_op(&tx, batch_id, ordinal as i64, t.id, OpKind::Md5, &expected)?;
        history::insert_edit(
            &tx,
            op_id,
            MD5_EDIT_KEY,
            &serde_json::json!(MD5_ZERO_HEX),
            &serde_json::Value::Null,
        )?;
        let job = md5_job(t.id, op_id, batch_id);
        let job_id = dbjobs::enqueue(&tx, &job, now)?.id();
        history::set_op_job(&tx, op_id, job_id)?;
        job_ids.push(job_id);
    }
    tx.commit()?;
    tracing::info!(
        batch_id,
        affected = targets.len(),
        skipped = skipped.len(),
        "MD5 補填バッチを記録した"
    );
    Ok(Md5FillPrepared {
        batch_id: Some(batch_id),
        affected: targets.len(),
        skipped: skipped.len(),
        job_ids,
        event: None,
    })
}

// ---------------------------------------------------------------- ファイル側（同期）

enum Md5Staged {
    /// 事前条件は一致したが `new_value` が無い: デコードして計算した値。書く前に耐久化する
    Computed([u8; 16]),
    /// 書いた（または既に新値だった）。`ph` は現在の実体、`md5` は STREAMINFO の値
    Written { ph: Physical, md5: [u8; 16] },
    /// 事前条件不一致。ファイルは触っていない。`current` は読めた現在値
    Conflict {
        reason: String,
        current: Option<(Physical, [u8; 16])>,
    },
    /// 書けない（FLAC として読めない・デコードできない）。再試行しても直らない
    Failed { reason: String },
}

fn stage_md5(
    root: &RootDir,
    op: &Op,
    edits: &[history::Edit],
    rel_path: &str,
    before_rename: Option<&BeforeRenameHook>,
) -> Result<Md5Staged, EditError> {
    let edit = edits
        .iter()
        .find(|e| e.key == MD5_EDIT_KEY)
        .ok_or_else(|| {
            EditError::Internal(format!("op {} に {MD5_EDIT_KEY} の edit が無い", op.id))
        })?;
    let old = md5_of_value(&edit.old_value)?
        .ok_or_else(|| EditError::Internal(format!("op {} の old_value が null", op.id)))?;
    let wanted = md5_of_value(&edit.new_value)?;

    let rel = RelPath::parse(rel_path)?;
    let mut file = match root.open_file(&rel) {
        Ok(f) => f,
        Err(FsError::NotFound | FsError::Symlink | FsError::Escaped) => {
            return Ok(Md5Staged::Conflict {
                reason: format!("ファイルを開けない: {rel_path}"),
                current: None,
            })
        }
        Err(e) => return Err(e.into()),
    };
    let st = fsroot::fstat(&file)?;
    // 現在の STREAMINFO（FLAC でなければ書けない）
    let (offset, current) = match flac_streaminfo_md5_at(&mut file) {
        Ok(v) => v,
        Err(e @ (FingerprintError::NotFlac | FingerprintError::BadStreamInfo)) => {
            return Ok(Md5Staged::Failed {
                reason: format!("FLAC として読めない: {e}"),
            })
        }
        Err(e) => return Err(EditError::Internal(e.to_string())),
    };
    // 事前条件: 実体（inode / size / mtime / ctime。dev は再起動で変わるので見ない、D-62）と、
    // 記録時の MD5 値。外部で補填・差し替えされていればファイルが正（conflict。DB を現在値に揃える）
    let mut diff: Vec<&str> = Vec::new();
    if op.expected.inode != Some(st.inode as i64) {
        diff.push("inode");
    }
    if op.expected.size != Some(st.size as i64) {
        diff.push("size");
    }
    if op.expected.mtime_ns != Some(st.mtime_ns) {
        diff.push("mtime_ns");
    }
    if op.expected.ctime_ns != Some(st.ctime_ns) {
        diff.push("ctime_ns");
    }
    if current != old {
        diff.push("audio_md5");
    }
    if !diff.is_empty() {
        // rename 済み・DB 未確定で落ちた再実行: ファイルが既に新値なら applied として確定する
        if wanted == Some(current) {
            tracing::info!(
                op_id = op.id,
                path = rel_path,
                "事前条件は外れているがファイルは新値。applied として確定"
            );
            return Ok(Md5Staged::Written {
                ph: st.into(),
                md5: current,
            });
        }
        return Ok(Md5Staged::Conflict {
            reason: format!("事前条件不一致: {}", diff.join(", ")),
            current: Some((st.into(), current)),
        });
    }
    // 書く値: 巻き戻しは記録済み。補填はデコードして計算し、書く前に呼び出し側が耐久化する
    let md5 = match wanted {
        Some(m) => m,
        None => {
            let mut reader = file.try_clone()?;
            reader.seek(SeekFrom::Start(0))?;
            return match decoded_pcm_md5(reader, Some("flac")) {
                Ok(m) => Ok(Md5Staged::Computed(m)),
                Err(e) => Ok(Md5Staged::Failed {
                    reason: format!("デコードできない: {e}"),
                }),
            };
        }
    };
    if md5 == current {
        return Ok(Md5Staged::Written { ph: st.into(), md5 });
    }
    // 親 dir に tmp を O_EXCL で作り、内容をコピーして 16 バイトだけ書き、fsync → rename
    let parent = rel.parent();
    let (tmp_rel, mut tmp) = root.create_tmp(parent.as_ref())?;
    let written = (|| -> Result<Md5Staged, EditError> {
        file.seek(SeekFrom::Start(0))?;
        std::io::copy(&mut file, &mut tmp)?;
        fsroot::copy_attrs(&file, &tmp)?;
        tmp.seek(SeekFrom::Start(offset))?;
        tmp.write_all(&md5)?;
        tmp.sync_all()?;
        // 書いた内容を読み戻して確かめる
        let mut reader = tmp.try_clone()?;
        reader.seek(SeekFrom::Start(0))?;
        let (_, got) = flac_streaminfo_md5_at(&mut reader)
            .map_err(|e| EditError::Internal(format!("書いた tmp を読み戻せない: {e}")))?;
        if got != md5 {
            return Ok(Md5Staged::Failed {
                reason: "書き込み結果が意図と一致しない".to_owned(),
            });
        }
        if let Some(hook) = before_rename {
            hook(rel_path);
        }
        // 確認から rename までの窓で外部が書き換えていないか宛先を開き直して確かめる（stage_tags と同じ）
        let recheck = match root.open_file(&rel) {
            Ok(f) => f,
            Err(FsError::NotFound | FsError::Symlink | FsError::Escaped) => {
                return Ok(Md5Staged::Conflict {
                    reason: "反映の直前にファイルが無くなった".to_owned(),
                    current: None,
                })
            }
            Err(e) => return Err(e.into()),
        };
        let now_st = fsroot::fstat(&recheck)?;
        if !super::same_stat(&st, &now_st) {
            let current = flac_streaminfo_md5_at(recheck)
                .ok()
                .map(|(_, m)| (now_st.into(), m));
            return Ok(Md5Staged::Conflict {
                reason: "反映の直前に外部で更新された".to_owned(),
                current,
            });
        }
        drop(recheck);
        root.replace_file(&tmp_rel, &rel)?;
        let st = fsroot::fstat(&tmp)?;
        Ok(Md5Staged::Written { ph: st.into(), md5 })
    })();
    if !matches!(written, Ok(Md5Staged::Written { .. })) {
        if let Err(u) = root.unlink(&tmp_rel) {
            if !matches!(u, FsError::NotFound) {
                tracing::warn!(path = %tmp_rel, error = %u, "tmp を消せない");
            }
        }
    }
    written
}

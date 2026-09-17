//! 一括リネームの coordinator（SPEC §7.5「リネーム」、§5、D-7、docs/TASKS.md P0-11）。
//!
//! ```text
//! plan_rename:     テンプレートと album のメタデータからパスを計画する（`domain::pathgen`）。
//!                  DB もファイルも書かない（preview）
//! prepare_rename:  1 トランザクションで edit_batches(prepared) / edit_ops(kind=rename, pending) /
//!                  edits(rel_path 旧→新) を記録 → DB の rel_path を 2 段階更新で新値へ（overlay）
//!                  → album の所属を追随 → バッチ 1 つに rename ジョブ 1 つを投入
//! apply_rename_batch（rename ジョブの本体。何度呼んでも結果は同じ）:
//!   phase 1  全 op の source を同じディレクトリの一時名（spindle-rename-<op_id>.<ext>）へ
//!            RENAME_NOREPLACE で退避。事前条件（dev/inode/size/mtime/ctime）が外れていれば
//!            skipped_conflict でファイルは触らない。cancel はここでだけ効き、退避済みを戻す
//!   phase 2  ordinal 順に一時名 → 最終名（RENAME_NOREPLACE、宛先ディレクトリは作る）。
//!            宛先が外部に取られていれば conflict にして source へ戻す
//!   commit   1 トランザクションで op を終端にし、rel_path をファイルの実際の所在へ揃え
//!            （2 段階更新）、物理属性を追随し、バッチを集計する
//! ```
//!
//! swap / 循環は phase 1 で全件が一時名に退避されるので解ける。phase 境界でクラッシュしても、
//! 再投入されたジョブが各 op の所在（最終名 / 一時名 / source）をファイルから判定して続きを行う。
//! DB の rel_path は prepare の時点で新値（overlay。D-24）なので、スキャナは pending の rename op が
//! あるトラックの rel_path を触らず、所在が source / 一時名 / 最終名のいずれでもなければ衝突にする。
//!
//! album は「ディレクトリ = album」の不変条件を DB でも保つため、overlay と overlay 解消の両方で
//! 追随させる（album 全体の移動は id を維持して `rel_dir` を書き換える。D-32）。スキャンは
//! 変更のないディレクトリの照合を省くので、ここで揃えないと album が古いまま残る

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension as _};

use crate::config::LayoutConfig;
use crate::db::history::{self, Op, OpKind, OpResult, Precondition, CANCELLED_ERROR};
use crate::db::jobs as dbjobs;
use crate::db::now_epoch;
use crate::db::scans::{self, AlbumMeta};
use crate::domain::pathgen::{self, Occupancy, PlanItem, Planned, Template, TrackFields};
use crate::domain::relpath::{canonical_key, RelPath};
use crate::fsroot::{self, FsError, RootDir};
use crate::jobs::{Event, JobContext, JobType, NewJob};

use super::{batch_event, EditError, Editor, Prepared};

/// 1 トラックのリネーム（`prepare_rename` の入力）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameTarget {
    pub track_id: i64,
    pub new_rel_path: String,
    /// preview 時の事前条件（D-33）。無ければ記録時点の DB 値。`tag_hash` が現在値と違えば
    /// 「プレビューの後に変更された」として `skipped_conflict` で記録だけする
    pub expected: Option<Precondition>,
    /// 計画の時点で衝突していた（`pathgen::Planned::Conflict`）。理由付きの
    /// `skipped_conflict` op として記録し、DB もファイルも触らない
    pub planned_conflict: Option<String>,
}

/// `plan_rename` の結果 1 件
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRename {
    pub track_id: i64,
    pub current_rel_path: String,
    pub planned: Planned,
}

/// テスト用フックの呼び出し点
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameStep {
    /// phase 1 が全件終わった（全ファイルが一時名にある）
    Staged,
    /// この op を最終名へ置く直前
    BeforeFinal(i64),
}

/// テスト用フック。`Err` を返すとその場で中断する（クラッシュの模擬）
pub type RenameHook = Arc<dyn Fn(RenameStep) -> Result<(), String> + Send + Sync>;

/// `apply_rename_batch` の結果
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RenameOutcome {
    pub applied: usize,
    pub conflict: usize,
    pub failed: usize,
    /// phase 1 の途中で cancel 要求を受け、退避済みを戻して閉じた
    pub cancelled: bool,
}

/// rename ジョブの dedup key（バッチ 1 つに 1 ジョブ。SPEC §8）
pub fn rename_dedup_key(batch_id: i64) -> String {
    format!("rename:batch:{batch_id}")
}

pub(super) fn rename_job(batch_id: i64) -> NewJob {
    NewJob::new(JobType::Rename, serde_json::json!({ "batch_id": batch_id }))
        .dedup_key(rename_dedup_key(batch_id))
        .edit_batch_id(batch_id)
}

/// phase 1 の退避先。source と同じディレクトリの `spindle-rename-<op_id>.<ext>`。
/// 隠しファイルにしない（スキャナに見せて、pending の rename op の所在として認識させる）
pub fn temp_rel_path(op_id: i64, source: &RelPath) -> Option<RelPath> {
    let ext = source.file_name().rsplit_once('.').map(|(_, e)| e);
    let name = match ext {
        Some(e) => format!("spindle-rename-{op_id}.{e}"),
        None => format!("spindle-rename-{op_id}"),
    };
    match source.parent() {
        Some(dir) => dir.join(&name).ok(),
        None => RelPath::parse(&name).ok(),
    }
}

/// pending の rename op があるトラックについて、スキャナが「自分の作業中」とみなす所在の key
/// （source / 一時名 / 最終名）。これ以外にあれば外部 rename との衝突
pub fn in_progress_keys(op_id: i64, expected_rel_path: &str, final_rel_path: &str) -> Vec<String> {
    let mut keys = vec![
        canonical_key(expected_rel_path),
        canonical_key(final_rel_path),
    ];
    if let Some(tmp) = RelPath::parse(expected_rel_path)
        .ok()
        .and_then(|src| temp_rel_path(op_id, &src))
    {
        keys.push(tmp.key());
    }
    keys
}

// ---------------------------------------------------------------- plan

struct TrackForPlan {
    id: i64,
    rel_path: String,
    fields: TrackFields,
    release: String,
    multi_disc: bool,
}

fn year_of(date: Option<&str>) -> Option<String> {
    let d = date?;
    let y: String = d.chars().take(4).collect();
    (y.len() == 4 && y.chars().all(|c| c.is_ascii_digit())).then_some(y)
}

fn load_track_for_plan(conn: &Connection, id: i64) -> crate::db::Result<Option<TrackForPlan>> {
    let row = conn
        .query_row(
            "SELECT t.rel_path, t.title, t.albumartist, t.artist_display, t.album, t.track_no,
                    t.disc_no, t.date, t.album_id, c.name, a.date, a.edition, a.mb_release_id,
                    a.discid, a.disc_count,
                    (SELECT max(x.disc_no) FROM tracks x
                      WHERE x.album_id = t.album_id AND x.missing_since IS NULL)
             FROM tracks t
             LEFT JOIN albums a ON a.id = t.album_id
             LEFT JOIN categories c ON c.id = a.category_id
             WHERE t.id = ?1",
            [id],
            |r| {
                let rel_path: String = r.get(0)?;
                let album_id: Option<i64> = r.get(8)?;
                let album_date: Option<String> = r.get(10)?;
                let track_date: Option<String> = r.get(7)?;
                let mb: Option<String> = r.get(12)?;
                let discid: Option<String> = r.get(13)?;
                let disc_count: Option<i64> = r.get(14)?;
                let max_disc: Option<i64> = r.get(15)?;
                let (stem, ext) = match rel_path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&rel_path)
                    .rsplit_once('.')
                {
                    Some((s, e)) => (s.to_owned(), e.to_owned()),
                    None => (rel_path.clone(), String::new()),
                };
                let release = match (mb, discid, album_id) {
                    (Some(m), _, _) if !m.is_empty() => format!("mb:{m}"),
                    (_, Some(d), _) if !d.is_empty() => format!("disc:{d}"),
                    (_, _, Some(a)) => format!("album:{a}"),
                    _ => format!("track:{id}"),
                };
                Ok(TrackForPlan {
                    id,
                    rel_path,
                    fields: TrackFields {
                        category: r.get(9)?,
                        albumartist: r.get(2)?,
                        artist: r.get(3)?,
                        album: r.get(4)?,
                        title: r.get(1)?,
                        disc_no: r.get(6)?,
                        track_no: r.get(5)?,
                        year: year_of(album_date.as_deref())
                            .or_else(|| year_of(track_date.as_deref())),
                        edition: r.get(11)?,
                        ext,
                        stem,
                    },
                    release,
                    multi_disc: disc_count.is_some_and(|n| n > 1)
                        || max_disc.is_some_and(|n| n > 1),
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// 選択外の active トラックの占有状況
fn load_occupancy(conn: &Connection, selected: &HashSet<i64>) -> crate::db::Result<Occupancy> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.rel_path_key, t.album_id, a.mb_release_id, a.discid, a.rel_dir_key
         FROM tracks t LEFT JOIN albums a ON a.id = t.album_id
         WHERE t.missing_since IS NULL",
    )?;
    let mut occ = Occupancy::default();
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<i64>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
        ))
    })?;
    for row in rows {
        let (id, key, album_id, mb, discid, dir_key) = row?;
        if selected.contains(&id) {
            continue;
        }
        let release = match (mb, discid, album_id) {
            (Some(m), _, _) if !m.is_empty() => format!("mb:{m}"),
            (_, Some(d), _) if !d.is_empty() => format!("disc:{d}"),
            (_, _, Some(a)) => format!("album:{a}"),
            _ => format!("track:{id}"),
        };
        if let Some(dir_key) = dir_key {
            occ.dir_releases.entry(dir_key).or_default().insert(release);
        }
        occ.path_keys.insert(key);
    }
    Ok(occ)
}

struct Templates {
    single: Template,
    multi: Template,
    unsorted: Template,
}

impl Templates {
    fn parse(layout: &LayoutConfig) -> Result<Self, EditError> {
        let parse = |name: &str, s: &str| {
            Template::parse(s)
                .map_err(|e| EditError::Internal(format!("[layout].{name} が不正: {e}")))
        };
        Ok(Templates {
            single: parse("single_disc", &layout.single_disc)?,
            multi: parse("multi_disc", &layout.multi_disc)?,
            unsorted: parse("unsorted", &layout.unsorted)?,
        })
    }

    fn for_track(&self, t: &TrackForPlan) -> Template {
        if t.fields.category.is_none() {
            self.unsorted.clone()
        } else if t.multi_disc {
            self.multi.clone()
        } else {
            self.single.clone()
        }
    }
}

/// [`Editor::plan_rename`] の本体（読み取りのみ）
pub(super) fn plan_rename_tx(
    conn: &Connection,
    track_ids: &[i64],
    layout: &LayoutConfig,
) -> Result<Vec<PlannedRename>, EditError> {
    let templates = Templates::parse(layout)?;
    let selected: HashSet<i64> = track_ids.iter().copied().collect();
    let mut tracks = Vec::with_capacity(track_ids.len());
    for id in track_ids {
        let t = load_track_for_plan(conn, *id)?.ok_or(EditError::TrackNotFound(*id))?;
        tracks.push(t);
    }
    let occ = load_occupancy(conn, &selected)?;
    let items: Vec<PlanItem> = tracks
        .iter()
        .map(|t| PlanItem {
            track_id: t.id,
            template: templates.for_track(t),
            fields: t.fields.clone(),
            release: t.release.clone(),
            current_rel_path: t.rel_path.clone(),
        })
        .collect();
    let planned = pathgen::plan(&items, &occ);
    Ok(tracks
        .into_iter()
        .zip(planned)
        .map(|(t, planned)| PlannedRename {
            track_id: t.id,
            current_rel_path: t.rel_path,
            planned,
        })
        .collect())
}

// ---------------------------------------------------------------- prepare

struct PlannedOp {
    track_id: i64,
    expected: Precondition,
    old: String,
    new: RelPath,
}

struct ConflictOp {
    track_id: i64,
    expected: Precondition,
    old: String,
    new: String,
    error: String,
}

/// [`Editor::prepare_rename`] のトランザクション本体
pub(super) fn prepare_rename_tx(
    conn: &mut Connection,
    description: Option<&str>,
    targets: &[RenameTarget],
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
    let selected: HashSet<i64> = ids.iter().copied().collect();

    // 宛先 key の重複（選択内）
    let mut key_count: HashMap<String, usize> = HashMap::new();
    for t in targets {
        if let Ok(p) = RelPath::parse(&t.new_rel_path) {
            *key_count.entry(p.key()).or_default() += 1;
        }
    }

    let mut planned: Vec<PlannedOp> = Vec::new();
    let mut conflicts: Vec<ConflictOp> = Vec::new();
    let mut to_vacate: Vec<i64> = Vec::new();
    let mut unchanged = 0usize;
    for t in targets {
        let current = history::precondition_of_track(&tx, t.track_id)?
            .ok_or(EditError::TrackNotFound(t.track_id))?;
        let old = current.rel_path.clone().unwrap_or_default();
        let current_hash = current.tag_hash.clone();
        let expected = t.expected.clone().unwrap_or(current);
        let mut conflict = |error: String| {
            conflicts.push(ConflictOp {
                track_id: t.track_id,
                expected: expected.clone(),
                old: old.clone(),
                new: t.new_rel_path.clone(),
                error,
            })
        };
        if let Some(reason) = &t.planned_conflict {
            conflict(reason.clone());
            continue;
        }
        // preview の後にタグが変わっていれば、計画の前提が崩れているので記録だけする（D-33）
        if let Some(e) = &t.expected {
            if e.tag_hash.is_some() && e.tag_hash != current_hash {
                conflict("プレビューの後に変更された（tag_hash 不一致）".to_owned());
                continue;
            }
        }
        if t.new_rel_path == old {
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
        if key_count.get(&key).copied().unwrap_or(0) > 1 {
            conflict(format!("宛先が選択内の別のトラックと重複: {new}"));
            continue;
        }
        // 選択外の行が宛先 key を占有していれば衝突。missing 行なら key を明け渡させる
        // （ファイルが残っていれば RENAME_NOREPLACE が最終判定になる）
        let holder: Option<(i64, Option<i64>)> = tx
            .query_row(
                "SELECT id, missing_since FROM tracks WHERE rel_path_key = ?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((holder_id, missing)) = holder {
            if !selected.contains(&holder_id) {
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
        });
    }
    if planned.is_empty() && conflicts.is_empty() {
        return Err(EditError::NoChanges);
    }

    let affected = planned.len() + conflicts.len();
    let batch_id = history::insert_batch(&tx, description, affected as i64, reverts_batch_id, now)?;
    let mut ordinal = 0i64;
    let mut op_ids = Vec::with_capacity(planned.len());
    for p in &planned {
        let op_id = history::insert_op(
            &tx,
            batch_id,
            ordinal,
            p.track_id,
            OpKind::Rename,
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
        op_ids.push(op_id);
    }
    for c in &conflicts {
        let op_id = history::insert_op(
            &tx,
            batch_id,
            ordinal,
            c.track_id,
            OpKind::Rename,
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
        history::finish_op(
            &tx,
            op_id,
            OpResult::SkippedConflict,
            Some(&c.error),
            None,
            now,
        )?;
    }

    // overlay: DB の rel_path を新値へ（2 段階更新）。album も追随する
    let mut job_ids = Vec::new();
    if !planned.is_empty() {
        scans::vacate_track_paths(&tx, &to_vacate)?;
        let moves: Vec<(i64, String, String)> = planned
            .iter()
            .map(|p| (p.track_id, p.new.as_str().to_owned(), p.new.key()))
            .collect();
        scans::move_track_paths(&tx, &moves)?;
        let album_moves: Vec<(i64, String)> = planned
            .iter()
            .map(|p| (p.track_id, p.new.as_str().to_owned()))
            .collect();
        reassign_albums(&tx, &album_moves, now)?;
        let job_id = dbjobs::enqueue(&tx, &rename_job(batch_id), now)?.id();
        for op_id in &op_ids {
            history::set_op_job(&tx, *op_id, job_id)?;
        }
        job_ids.push(job_id);
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
        "リネームバッチを記録した"
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

// ---------------------------------------------------------------- album の追随

/// 移動したトラックの album 所属を「ディレクトリ = album」に揃える（D-32 の coordinator 版）。
/// `moves` は `(track_id, 新 rel_path)`。
///
/// - 宛先ディレクトリに album 行があればそれに合流する（missing なら復活）
/// - 無ければ、ある album の active な構成トラック**全部**が同じ宛先へ動くとき（album 全体の移動）
///   はその album の `rel_dir` を書き換えて id を維持する。それ以外は新規 album
/// - 構成が 0 になった album は `missing_since` を立てる（行は消さない）
pub(super) fn reassign_albums(
    tx: &Connection,
    moves: &[(i64, String)],
    now: i64,
) -> crate::db::Result<()> {
    struct Move {
        track_id: i64,
        old_album: Option<i64>,
        new_dir: Option<RelPath>,
    }
    let mut ms: Vec<Move> = Vec::with_capacity(moves.len());
    for (track_id, new_path) in moves {
        let old_album: Option<i64> = tx
            .query_row(
                "SELECT album_id FROM tracks WHERE id = ?1",
                [track_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let new_dir = RelPath::parse(new_path).ok().and_then(|p| p.parent());
        ms.push(Move {
            track_id: *track_id,
            old_album,
            new_dir,
        });
    }
    let old_albums: HashSet<i64> = ms.iter().filter_map(|m| m.old_album).collect();

    // 宛先ディレクトリ key ごとにまとめる
    let mut groups: HashMap<String, (RelPath, Vec<usize>)> = HashMap::new();
    for (i, m) in ms.iter().enumerate() {
        match &m.new_dir {
            Some(d) => groups
                .entry(d.key())
                .or_insert_with(|| (d.clone(), Vec::new()))
                .1
                .push(i),
            // root 直下は album を持たない
            None => scans::clear_track_album(tx, m.track_id)?,
        }
    }
    let mut keys: Vec<&String> = groups.keys().collect();
    keys.sort();
    for key in keys {
        let (dir, idxs) = &groups[key];
        let existing: Option<(i64, Option<String>, Option<i64>)> = tx
            .query_row(
                "SELECT id, album, missing_since FROM albums WHERE rel_dir_key = ?1",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        // album 全体の移動か: 旧 album の active な構成トラックが全員この宛先へ動く
        let mut whole: Option<(i64, usize)> = None;
        let mut counts: HashMap<i64, usize> = HashMap::new();
        for i in idxs {
            if let Some(a) = ms[*i].old_album {
                *counts.entry(a).or_default() += 1;
            }
        }
        for (album_id, n) in counts {
            let active: i64 = tx.query_row(
                "SELECT count(*) FROM tracks WHERE album_id = ?1 AND missing_since IS NULL",
                [album_id],
                |r| r.get(0),
            )?;
            if active as usize == n && whole.is_none_or(|(_, m)| n > m) {
                whole = Some((album_id, n));
            }
        }
        let (target_id, album_name) = match (existing, whole) {
            // 宛先に active な album がある → 合流
            (Some((id, name, None)), _) => (id, name),
            // 宛先の album 行は missing で、こちらは album 全体の移動 → 行を退かせて id を維持
            (Some((stale_id, _, Some(_))), Some((album_id, _))) => {
                let displaced = format!("\0displaced:{stale_id}");
                tx.execute(
                    "UPDATE albums SET rel_dir = ?2, rel_dir_key = ?2 WHERE id = ?1",
                    params![stale_id, displaced],
                )?;
                scans::move_album_dirs(tx, &[(album_id, dir.as_str().to_owned(), key.clone())])?;
                (album_id, album_name_of(tx, album_id)?)
            }
            // 宛先の album 行は missing → 復活して合流
            (Some((id, name, Some(_))), None) => {
                tx.execute("UPDATE albums SET missing_since = NULL WHERE id = ?1", [id])?;
                (id, name)
            }
            // 宛先に album が無く、album 全体の移動 → rel_dir を書き換えて id を維持
            (None, Some((album_id, _))) => {
                scans::move_album_dirs(tx, &[(album_id, dir.as_str().to_owned(), key.clone())])?;
                (album_id, album_name_of(tx, album_id)?)
            }
            // 新規 album
            (None, None) => {
                let track_ids: Vec<i64> = idxs.iter().map(|i| ms[*i].track_id).collect();
                let meta = album_meta_from_tracks(tx, dir, &track_ids)?;
                let id = scans::insert_album(tx, dir.as_str(), key, &meta)?;
                (id, meta.album)
            }
        };
        for i in idxs {
            scans::set_track_album(tx, ms[*i].track_id, target_id, album_name.as_deref())?;
        }
    }
    // 構成 0 になった旧 album は missing
    for album_id in old_albums {
        tx.execute(
            "UPDATE albums SET missing_since = ?2 WHERE id = ?1 AND missing_since IS NULL
               AND NOT EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = ?1 AND t.missing_since IS NULL)",
            params![album_id, now],
        )?;
    }
    Ok(())
}

fn album_name_of(tx: &Connection, album_id: i64) -> crate::db::Result<Option<String>> {
    Ok(tx
        .query_row("SELECT album FROM albums WHERE id = ?1", [album_id], |r| {
            r.get(0)
        })
        .optional()?
        .flatten())
}

/// 新規 album のメタデータ: 構成トラックのキャッシュ列の最頻値。category は先頭ディレクトリ名が
/// 語彙に一致すればそれ、無ければ旧 album から引き継ぐ
fn album_meta_from_tracks(
    tx: &Connection,
    dir: &RelPath,
    track_ids: &[i64],
) -> crate::db::Result<AlbumMeta> {
    let mut albumartist: HashMap<String, usize> = HashMap::new();
    let mut album: HashMap<String, usize> = HashMap::new();
    let mut date: HashMap<String, usize> = HashMap::new();
    let mut old_category: Option<i64> = None;
    for id in track_ids {
        let row: (Option<String>, Option<String>, Option<String>, Option<i64>) = tx.query_row(
            "SELECT t.albumartist, t.album, t.date, a.category_id
             FROM tracks t LEFT JOIN albums a ON a.id = t.album_id WHERE t.id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        if let Some(v) = row.0 {
            *albumartist.entry(v).or_default() += 1;
        }
        if let Some(v) = row.1 {
            *album.entry(v).or_default() += 1;
        }
        if let Some(v) = row.2 {
            *date.entry(v).or_default() += 1;
        }
        old_category = old_category.or(row.3);
    }
    let mode = |m: HashMap<String, usize>| {
        m.into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
            .map(|(v, _)| v)
    };
    let top = dir
        .components()
        .next()
        .map(canonical_key)
        .unwrap_or_default();
    let mut category_id: Option<i64> = None;
    let mut stmt = tx.prepare_cached("SELECT id, name FROM categories")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
        let (id, name) = row?;
        if canonical_key(&name) == top {
            category_id = Some(id);
            break;
        }
    }
    Ok(AlbumMeta {
        category_id: category_id.or(old_category),
        albumartist: mode(albumartist),
        album: mode(album),
        date: mode(date),
        ..AlbumMeta::default()
    })
}

// ---------------------------------------------------------------- apply

/// ファイル側で確定した op の所在
#[derive(Debug, Clone)]
enum Location {
    /// 最終名にある（このジョブが置いた、またはクラッシュ前に置いてあった）
    Final(fsroot::Stat),
    /// 一時名にある
    Staged(fsroot::Stat),
    /// source にある（事前条件確認済み、未着手）
    Source(fsroot::Stat),
    /// 事前条件不一致・見つからない。`at` は実際の所在（分かれば）
    Conflict {
        reason: String,
        at: Option<(RelPath, fsroot::Stat)>,
    },
}

struct RenameOp {
    op: Op,
    source: RelPath,
    temp: RelPath,
    final_path: RelPath,
}

fn rename_ops(ops: Vec<(Op, Vec<history::Edit>)>) -> Result<Vec<RenameOp>, EditError> {
    let mut out = Vec::with_capacity(ops.len());
    for (op, edits) in ops {
        let source_s = op.expected.rel_path.clone().ok_or_else(|| {
            EditError::Internal(format!("op {} に expected_rel_path が無い", op.id))
        })?;
        let new = edits
            .iter()
            .find(|e| e.key == "rel_path")
            .and_then(|e| e.new_value.as_str().map(str::to_owned))
            .ok_or_else(|| {
                EditError::Internal(format!("op {} に rel_path の edit が無い", op.id))
            })?;
        let source = RelPath::parse(&source_s)?;
        let final_path = RelPath::parse(&new)?;
        let temp = temp_rel_path(op.id, &source)
            .ok_or_else(|| EditError::Internal(format!("op {} の一時名を作れない", op.id)))?;
        out.push(RenameOp {
            op,
            source,
            temp,
            final_path,
        });
    }
    Ok(out)
}

fn stat_if_exists(root: &RootDir, rel: &RelPath) -> Result<Option<fsroot::Stat>, FsError> {
    match root.stat(rel) {
        Ok(st) => Ok(Some(st)),
        Err(FsError::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

fn same_inode(expected: &Precondition, st: &fsroot::Stat) -> bool {
    expected.dev == Some(st.dev as i64) && expected.inode == Some(st.inode as i64)
}

/// 所在をファイルから判定する（何度呼んでも同じ）
fn locate(root: &RootDir, r: &RenameOp) -> Result<Location, FsError> {
    if let Some(st) = stat_if_exists(root, &r.final_path)? {
        if same_inode(&r.op.expected, &st) {
            return Ok(Location::Final(st));
        }
    }
    if let Some(st) = stat_if_exists(root, &r.temp)? {
        if same_inode(&r.op.expected, &st) {
            return Ok(Location::Staged(st));
        }
    }
    match stat_if_exists(root, &r.source)? {
        Some(st) => {
            let e = &r.op.expected;
            let mut diff = Vec::new();
            if !same_inode(e, &st) {
                diff.push("inode");
            }
            if e.size != Some(st.size as i64) {
                diff.push("size");
            }
            if e.mtime_ns != Some(st.mtime_ns) {
                diff.push("mtime_ns");
            }
            if e.ctime_ns != Some(st.ctime_ns) {
                diff.push("ctime_ns");
            }
            if diff.is_empty() {
                Ok(Location::Source(st))
            } else {
                let at = same_inode(e, &st).then(|| (r.source.clone(), st));
                Ok(Location::Conflict {
                    reason: format!("事前条件不一致: {}", diff.join(", ")),
                    at,
                })
            }
        }
        None => Ok(Location::Conflict {
            reason: format!("ファイルが無い: {}", r.source),
            at: None,
        }),
    }
}

/// phase 1: source → 一時名
fn stage(root: &RootDir, r: &RenameOp) -> Result<Location, FsError> {
    let loc = locate(root, r)?;
    let Location::Source(_) = loc else {
        return Ok(loc);
    };
    match root.rename_noreplace(&r.source, &r.temp) {
        Ok(()) => root.fsync_dir(r.temp.parent().as_ref())?,
        Err(FsError::Exists) => {
            return Ok(Location::Conflict {
                reason: format!("一時名が既に存在する: {}", r.temp),
                at: stat_if_exists(root, &r.source)?.map(|st| (r.source.clone(), st)),
            })
        }
        Err(FsError::NotFound) => {
            return Ok(Location::Conflict {
                reason: format!("退避の直前にファイルが無くなった: {}", r.source),
                at: None,
            })
        }
        Err(e) => return Err(e),
    }
    match stat_if_exists(root, &r.temp)? {
        Some(st) => Ok(Location::Staged(st)),
        None => Ok(Location::Conflict {
            reason: "退避の直後にファイルが無くなった".to_owned(),
            at: None,
        }),
    }
}

/// 一時名 → source へ戻す（cancel / 衝突）。戻せなければ一時名のまま
fn unstage(root: &RootDir, r: &RenameOp) -> Result<Location, FsError> {
    match root.rename_noreplace(&r.temp, &r.source) {
        Ok(()) => {
            root.fsync_dir(r.source.parent().as_ref())?;
            match stat_if_exists(root, &r.source)? {
                Some(st) => Ok(Location::Conflict {
                    reason: String::new(),
                    at: Some((r.source.clone(), st)),
                }),
                None => Ok(Location::Conflict {
                    reason: String::new(),
                    at: None,
                }),
            }
        }
        Err(FsError::NotFound) => Ok(Location::Conflict {
            reason: format!("一時名のファイルが無くなった: {}", r.temp),
            at: None,
        }),
        Err(FsError::Exists) => {
            tracing::warn!(op_id = r.op.id, path = %r.source, "元の場所が占有されているので一時名のまま残す");
            Ok(Location::Conflict {
                reason: String::new(),
                at: stat_if_exists(root, &r.temp)?.map(|st| (r.temp.clone(), st)),
            })
        }
        Err(e) => Err(e),
    }
}

/// phase 2: 一時名 → 最終名
fn finalize(root: &RootDir, r: &RenameOp) -> Result<Location, FsError> {
    if let Some(dir) = r.final_path.parent() {
        root.create_dir_all(&dir)?;
    }
    match root.rename_noreplace(&r.temp, &r.final_path) {
        Ok(()) => {
            root.fsync_dir(r.final_path.parent().as_ref())?;
            if r.final_path.parent() != r.temp.parent() {
                root.fsync_dir(r.temp.parent().as_ref())?;
            }
            match stat_if_exists(root, &r.final_path)? {
                Some(st) => Ok(Location::Final(st)),
                None => Ok(Location::Conflict {
                    reason: "最終名へ置いた直後にファイルが無くなった".to_owned(),
                    at: None,
                }),
            }
        }
        Err(FsError::Exists) => {
            let mut loc = unstage(root, r)?;
            if let Location::Conflict { reason, .. } = &mut loc {
                *reason = format!("宛先が既に存在する: {}", r.final_path);
            }
            Ok(loc)
        }
        // 一時名のファイルが外部に消された。所在不明として閉じる（再試行しても直らない）
        Err(FsError::NotFound) => Ok(Location::Conflict {
            reason: format!("一時名のファイルが無くなった: {}", r.temp),
            at: None,
        }),
        Err(e) => Err(e),
    }
}

/// op の終端結果（commit で使う）
struct Settled {
    op: Op,
    result: OpResult,
    error: Option<String>,
    /// ファイルの実際の所在と stat（分からなければ None → 記録時点へ戻す）
    at: Option<(RelPath, fsroot::Stat)>,
}

/// 所在から op の終端結果を決める。`interrupted` は途中で止まった op（一時名・source のまま、
/// または戻した）に付ける error（cancel なら `CANCELLED_ERROR`）
fn settle(r: &RenameOp, loc: Location, interrupted: &str) -> Settled {
    let failed = |at: Option<(RelPath, fsroot::Stat)>| Settled {
        op: r.op.clone(),
        result: OpResult::Failed,
        error: Some(interrupted.to_owned()),
        at,
    };
    match loc {
        Location::Final(st) => Settled {
            op: r.op.clone(),
            result: OpResult::Applied,
            error: None,
            at: Some((r.final_path.clone(), st)),
        },
        Location::Staged(st) => failed(Some((r.temp.clone(), st))),
        Location::Source(st) => failed(Some((r.source.clone(), st))),
        Location::Conflict { reason, at } if reason.is_empty() => failed(at),
        Location::Conflict { reason, at } => Settled {
            op: r.op.clone(),
            result: OpResult::SkippedConflict,
            error: Some(reason),
            at,
        },
    }
}

fn holder_of(tx: &Connection, key: &str) -> crate::db::Result<Option<i64>> {
    Ok(tx
        .query_row(
            "SELECT id FROM tracks WHERE rel_path_key = ?1",
            [key],
            |r| r.get(0),
        )
        .optional()?)
}

/// op を終端にし、rel_path をファイルの実際の所在へ揃え、物理属性を追随し、バッチを集計する
/// `commit_settled` の結果。`derived_jobs` は applied のトラックへ投入した Derived の追随ジョブ
/// （commit 後にワーカーを起こす）
struct Committed {
    outcome: RenameOutcome,
    event: Option<crate::jobs::BatchEvent>,
    derived_jobs: Vec<i64>,
}

fn commit_settled(
    conn: &mut Connection,
    batch_id: i64,
    job_id: Option<i64>,
    settled: &[Settled],
) -> Result<Committed, EditError> {
    let tx = conn.transaction()?;
    let now = now_epoch();
    let mut outcome = RenameOutcome::default();
    let mut moves: Vec<(i64, String, String)> = Vec::new();
    let mut album_moves: Vec<(i64, String)> = Vec::new();
    let mut lost: Vec<(i64, String)> = Vec::new();
    for s in settled {
        if !history::finish_op(&tx, s.op.id, s.result, s.error.as_deref(), job_id, now)? {
            continue; // 既に終端（スキャナが衝突にした等）。所在は変えない
        }
        match s.result {
            OpResult::Applied => outcome.applied += 1,
            OpResult::SkippedConflict => outcome.conflict += 1,
            _ => outcome.failed += 1,
        }
        let current = history::track_rel_path(&tx, s.op.track_id)?.unwrap_or_default();
        match &s.at {
            Some((p, st)) => {
                history::set_track_physical(&tx, s.op.track_id, &(*st).into())?;
                let actual = p.as_str().to_owned();
                if actual != current {
                    moves.push((s.op.track_id, actual.clone(), canonical_key(&actual)));
                    album_moves.push((s.op.track_id, actual));
                }
            }
            None => {
                history::restore_track_precondition(&tx, s.op.track_id, &s.op.expected)?;
                let fallback = s.op.expected.rel_path.clone().unwrap_or(current.clone());
                if fallback != current {
                    lost.push((s.op.track_id, fallback));
                }
            }
        }
    }
    // 順序が結果を決める（UNIQUE を踏まない）:
    //   1. 所在不明の行の key を先に予約 key へ退避する（overlay の最終名が、所在の分かった別の行の
    //      戻り先と重なり得る）
    //   2. 所在が分かっている行を実在パスへ（2 段階更新。バッチ内同士は実在パスなので重複しない）。
    //      その key をバッチ外の行が占有していれば明け渡させる（スキャンが外部 rename をその key に
    //      追随させた等。ファイルの所在が正）
    //   3. 所在不明の行を記録時点のパスへ戻す。その key を誰かが持っていれば予約 key のまま
    //      （次のスキャンが inode で見つけるか missing にする）
    let lost_ids: Vec<i64> = lost.iter().map(|(id, _)| *id).collect();
    scans::vacate_track_paths(&tx, &lost_ids)?;
    let batch_tracks: HashSet<i64> = settled.iter().map(|s| s.op.track_id).collect();
    let mut evict = Vec::new();
    for (id, _, key) in &moves {
        if let Some(h) = holder_of(&tx, key)? {
            if h != *id && !batch_tracks.contains(&h) {
                tracing::warn!(
                    track_id = h,
                    new_holder = id,
                    key,
                    "パスを別のトラックに明け渡した"
                );
                evict.push(h);
            }
        }
    }
    scans::vacate_track_paths(&tx, &evict)?;
    scans::move_track_paths(&tx, &moves)?;
    for (id, fallback) in lost {
        let key = canonical_key(&fallback);
        match holder_of(&tx, &key)? {
            Some(h) if h != id => {
                tracing::warn!(
                    track_id = id,
                    path = %fallback,
                    holder = h,
                    "所在不明のトラックの元パスは占有されている。予約 key へ退避する"
                );
            }
            _ => {
                scans::move_track_paths(&tx, &[(id, fallback.clone(), key)])?;
                album_moves.push((id, fallback));
            }
        }
    }
    reassign_albums(&tx, &album_moves, now)?;
    // Derived の追随（D-51）。パスが変わった（applied）トラックの Derived を rename させる
    let mut derived_jobs = Vec::new();
    for s in settled {
        if s.result == OpResult::Applied {
            if let Some(id) = crate::db::derived::enqueue_if_stale(&tx, s.op.track_id, now)? {
                derived_jobs.push(id);
            }
        }
    }
    let event = match history::aggregate_batch(&tx, batch_id, now)? {
        Some(state) => Some(batch_event(&tx, batch_id, state)?),
        None => None,
    };
    tx.commit()?;
    Ok(Committed {
        outcome,
        event,
        derived_jobs,
    })
}

impl Editor {
    /// テスト用: phase の境目で呼ばれるフックを置く
    #[doc(hidden)]
    pub fn set_rename_hook(&self, hook: RenameHook) {
        *self.rename_hook.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    /// フックはブロックし得る（テストがゲートに使う）ので、ランタイムのスレッドでは呼ばない
    async fn call_rename_hook(&self, step: RenameStep) -> Result<(), EditError> {
        let hook = self
            .rename_hook
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

    /// テンプレートからリネームを計画する（preview。DB もファイルも書かない）
    pub async fn plan_rename(
        &self,
        track_ids: &[i64],
        layout: &LayoutConfig,
    ) -> Result<Vec<PlannedRename>, EditError> {
        let ids = track_ids.to_vec();
        let layout = layout.clone();
        self.db
            .read(move |c| {
                let tx = c.unchecked_transaction()?;
                let r = plan_rename_tx(&tx, &ids, &layout);
                tx.finish()?;
                Ok(r)
            })
            .await?
    }

    /// リネームバッチを記録し、DB を先行更新して rename ジョブを投入する。
    /// 対象トラックに pending の op があれば何も記録せず [`EditError::Pending`]
    pub async fn prepare_rename(
        &self,
        description: Option<&str>,
        targets: Vec<RenameTarget>,
    ) -> Result<Prepared, EditError> {
        let description = description.map(str::to_owned);
        let prepared = self
            .db
            .write(move |c| {
                Ok(prepare_rename_tx(
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

    /// バッチの pending な rename op（ordinal 順）と edits
    async fn load_rename_ops(&self, batch_id: i64) -> Result<Vec<RenameOp>, EditError> {
        let ops = self
            .db
            .read(move |c| {
                let mut out = Vec::new();
                for op in history::pending_ops(c, batch_id)? {
                    if op.kind != OpKind::Rename {
                        continue;
                    }
                    let edits = history::list_edits(c, op.id)?;
                    out.push((op, edits));
                }
                Ok(out)
            })
            .await?;
        rename_ops(ops)
    }

    /// rename ジョブの本体（2 phase）。バッチが終端なら何もしない。
    /// `ctx` があれば進捗を報告し、phase 1 の間は cancel 要求に応じる
    pub async fn apply_rename_batch(
        &self,
        batch_id: i64,
        job_id: Option<i64>,
        ctx: Option<&JobContext>,
    ) -> Result<RenameOutcome, EditError> {
        let batch = self
            .db
            .read(move |c| history::get_batch(c, batch_id))
            .await?;
        let Some(batch) = batch else {
            return Err(EditError::Internal(format!(
                "バッチが存在しない: {batch_id}"
            )));
        };
        if batch.state.is_terminal() {
            return Ok(RenameOutcome::default());
        }
        let ops = self.load_rename_ops(batch_id).await?;
        if ops.is_empty() {
            self.aggregate(batch_id).await?;
            return Ok(RenameOutcome::default());
        }
        self.mark_applying(batch_id).await?;
        let total = (ops.len() * 2) as i64;
        let mut locs: Vec<Location> = Vec::with_capacity(ops.len());

        // phase 1
        let mut cancelled = false;
        for (i, r) in ops.iter().enumerate() {
            if let Some(ctx) = ctx {
                if ctx.progress(i as i64, total).await.is_err() {
                    cancelled = true;
                    break;
                }
            }
            let loc = self.blocking(r, stage).await?;
            locs.push(loc);
        }
        if !cancelled {
            self.call_rename_hook(RenameStep::Staged).await?;
            // phase 2 に入る前の最後の確認。ここまでは cancel で全件を戻せる
            if let Some(ctx) = ctx {
                cancelled = ctx.check_cancel().await.is_err();
            }
        }
        if cancelled {
            let outcome = self.cancel_staged(batch_id, job_id, &ops, &locs).await?;
            return Ok(outcome);
        }

        // phase 2（ordinal 順。cancel は見ない: 一部だけ戻すと swap / 循環が解けない）
        for (i, r) in ops.iter().enumerate() {
            if !matches!(locs[i], Location::Staged(_)) {
                continue;
            }
            self.call_rename_hook(RenameStep::BeforeFinal(r.op.id))
                .await?;
            locs[i] = self.blocking(r, finalize).await?;
            if let Some(ctx) = ctx {
                let _ = ctx.progress((ops.len() + i + 1) as i64, total).await;
            }
        }
        let settled: Vec<Settled> = ops
            .iter()
            .zip(locs)
            .map(|(r, loc)| settle(r, loc, "反映の途中で止まった"))
            .collect();
        let committed = self
            .db
            .write(move |c| Ok(commit_settled(c, batch_id, job_id, &settled)))
            .await??;
        let outcome = self.finish_commit(committed).await;
        tracing::info!(
            batch_id,
            applied = outcome.applied,
            conflict = outcome.conflict,
            failed = outcome.failed,
            "リネームバッチを反映した"
        );
        Ok(outcome)
    }

    /// phase 1 の途中・直後の cancel: 退避済みを source へ戻し、全件を cancelled に閉じる
    async fn cancel_staged(
        &self,
        batch_id: i64,
        job_id: Option<i64>,
        ops: &[RenameOp],
        locs: &[Location],
    ) -> Result<RenameOutcome, EditError> {
        let mut settled = Vec::with_capacity(ops.len());
        for (i, r) in ops.iter().enumerate() {
            let loc = match locs.get(i) {
                Some(Location::Staged(_)) => self.blocking(r, unstage).await?,
                Some(l) => l.clone(),
                None => self.blocking(r, locate).await?,
            };
            settled.push(settle(r, loc, CANCELLED_ERROR));
        }
        let committed = self
            .db
            .write(move |c| Ok(commit_settled(c, batch_id, job_id, &settled)))
            .await??;
        let mut outcome = self.finish_commit(committed).await;
        outcome.cancelled = true;
        tracing::info!(batch_id, "リネームバッチを phase 1 でキャンセルした");
        Ok(outcome)
    }

    /// バッチの pending な rename op を全件 `failed(error)` に閉じる（ジョブが走っていないとき:
    /// cancel / リカバリ / 最終失敗）。一時名にあるファイルは source へ戻し、DB の rel_path を
    /// 所在へ揃える。閉じた op 数を返す
    pub(super) async fn close_rename_ops(
        &self,
        batch_id: i64,
        job_id: Option<i64>,
        error: &str,
    ) -> Result<usize, EditError> {
        let ops = self.load_rename_ops(batch_id).await?;
        if ops.is_empty() {
            return Ok(0);
        }
        let mut settled = Vec::with_capacity(ops.len());
        for r in &ops {
            let loc = match self.blocking(r, locate).await? {
                Location::Staged(_) => self.blocking(r, unstage).await?,
                l => l,
            };
            settled.push(settle(r, loc, error));
        }
        let committed = self
            .db
            .write(move |c| Ok(commit_settled(c, batch_id, job_id, &settled)))
            .await??;
        let outcome = self.finish_commit(committed).await;
        Ok(outcome.applied + outcome.conflict + outcome.failed)
    }

    /// ハンドラが手を付ける前に cancel を受けたとき: バッチの pending を cancelled に閉じる
    pub async fn close_op_batch_cancelled(
        &self,
        batch_id: i64,
        job_id: Option<i64>,
    ) -> Result<usize, EditError> {
        self.close_rename_ops(batch_id, job_id, CANCELLED_ERROR)
            .await
    }

    /// ハンドラの最終失敗: バッチの pending を failed に閉じる（overlay の解消を伴う）
    pub async fn close_op_batch_failed(
        &self,
        batch_id: i64,
        job_id: Option<i64>,
        error: &str,
    ) -> Result<usize, EditError> {
        self.close_rename_ops(batch_id, job_id, error).await
    }

    async fn blocking(
        &self,
        r: &RenameOp,
        f: fn(&RootDir, &RenameOp) -> Result<Location, FsError>,
    ) -> Result<Location, EditError> {
        let root = Arc::clone(&self.root);
        let r = RenameOp {
            op: r.op.clone(),
            source: r.source.clone(),
            temp: r.temp.clone(),
            final_path: r.final_path.clone(),
        };
        Ok(tokio::task::spawn_blocking(move || f(&root, &r)).await??)
    }

    /// commit 後の後始末: 投入した Derived の追随ジョブでワーカーを起こし、バッチの終端を通知する
    async fn finish_commit(&self, c: Committed) -> RenameOutcome {
        self.jobs.notify_enqueued(&c.derived_jobs).await;
        self.publish_batch(c.event);
        c.outcome
    }

    fn publish_batch(&self, event: Option<crate::jobs::BatchEvent>) {
        if let Some(ev) = event {
            tracing::info!(batch_id = ev.id, state = %ev.state, applied = ev.applied, conflict = ev.conflict, failed = ev.failed, "編集バッチが終端になった");
            self.jobs.publish(Event::Batch(ev));
        }
    }
}

//! Inbox の件の再判定（`inbox_rejudge` ジョブの本体）。承認前（pending）の件の YouTube 由来の曲を
//! メタデータプラグインに問い合わせ直し、タグ・ファイル名・置き場所・サイドカーを初回の取り込みと
//! 同じ形に書き直す。
//!
//! ```text
//! 件     pending でなければ NotPending。下書き（inbox_items.draft）を消す
//! 対象   サイドカーに source = youtube の項がある音声ファイル（名前順）。項の名前のファイルが無ければ、
//!        項を持たないファイルのうち SOURCE_URL が項の url と同じもの（前回の途中で名前を変えた後に落ちた）
//! 1 曲   Item（項の item か yt-dlp の dump）→ judge → タグ → 宛先（移動の規則）→ 新しい名前 →
//!        tmp に写してタグ → RENAME_NOREPLACE（同名は SOURCE_URL が同じときだけ自分の成果物）→
//!        サイドカー → 元を unlink
//! 仕上げ ディレクトリを fsync して inbox ジョブを投入
//! ```
//!
//! 曲ごとに独立に失敗させ（理由はサイドカーの項の `rejudge_error`）、他の曲は続ける。どこで落ちても、
//! もう一度で同じ応答から同じ状態に収束する（冪等）。行はキャッシュで、走査が作り直す（D-68）

use std::collections::{HashMap, HashSet};

use tokio_util::sync::CancellationToken;

use crate::db::inbox::{self as dbinbox, ItemState};
use crate::domain::relpath::{canonical_key, RelPath};
use crate::domain::tags::{write_tag_changes, Codec, TagChange};
use crate::fsroot::{copy_attrs, FileKind, FsError, RootDir};
use crate::import::sidecar::{FileEntry, Sidecar, SidecarError};
use crate::import::ytmusic::downloader::{
    dump_video, inbox_dir, inbox_file_name_for, is_own_product, judge, plugin_tag_keys,
    subscription_target, tag_changes_for, DownloadError, DownloaderEnv, SUBDIR, UNMATCHED_DIR,
};
use crate::import::ytmusic::metadata::SOURCE_URL_KEY;
use crate::import::ytmusic::{Item, Track};

/// 再判定の結果
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RejudgeReport {
    /// 対象の曲数
    pub total: usize,
    /// 判定し直して書いた曲数（移せなかった曲を含む）
    pub rejudged: usize,
    /// (元のファイル名, 移動先の rel_path)
    pub moved: Vec<(String, RelPath)>,
    /// (ファイル名, 理由)
    pub failed: Vec<(String, String)>,
    /// (ファイル名, 移動先の件の state)
    pub not_moved: Vec<(String, String)>,
}

#[derive(Debug, thiserror::Error)]
pub enum RejudgeError {
    /// 件が無い・pending でない（途中で外れたら、それまでの曲は書いたまま止める）
    #[error("{0}")]
    NotPending(String),
    #[error(transparent)]
    Download(#[from] DownloadError),
}

impl From<crate::db::DbError> for RejudgeError {
    fn from(e: crate::db::DbError) -> Self {
        RejudgeError::Download(e.into())
    }
}

impl From<FsError> for RejudgeError {
    fn from(e: FsError) -> Self {
        RejudgeError::Download(e.into())
    }
}

/// 件 `item_id` を判定し直す。`progress` には曲を 1 つ終えるたびに (done, total) を送る
pub async fn rejudge_item(
    env: &DownloaderEnv,
    item_id: i64,
    token: &CancellationToken,
    progress: &tokio::sync::watch::Sender<(usize, usize)>,
) -> Result<RejudgeReport, RejudgeError> {
    let item = ensure_pending(env, item_id).await?;
    // 補正の下書きは提案でタグより勝つので、判定結果で置き換えるために消す（ユーザの判断）
    if !env
        .db
        .write(move |c| dbinbox::clear_pending_draft(c, item_id))
        .await?
    {
        return Err(RejudgeError::NotPending(format!(
            "件 {item_id} が承認前でなくなった"
        )));
    }
    let dir = RelPath::parse(&item.rel_dir)
        .map_err(|e| DownloadError::Fatal(format!("件のディレクトリが不正: {e}")))?;
    let targets = targets(&env.inbox, &dir)?;
    let total = targets.len();
    progress.send_replace((0, total));
    let mut report = RejudgeReport {
        total,
        ..RejudgeReport::default()
    };
    let mut touched: Vec<RelPath> = vec![dir.clone()];
    for (i, target) in targets.into_iter().enumerate() {
        if token.is_cancelled() {
            return Err(DownloadError::Cancelled.into());
        }
        // 待っている間に人が承認した等。続けると走査が承認を pending に戻してしまう
        ensure_pending(env, item_id).await?;
        let Target {
            entry_name,
            file_name,
            mut entry,
        } = target;
        match rejudge_track(env, &dir, &file_name, &entry_name, &mut entry, token).await {
            Ok(done) => {
                report.rejudged += 1;
                if let Some(to) = done.moved {
                    if let Some(p) = to.parent() {
                        if !touched.iter().any(|d| d.key() == p.key()) {
                            touched.push(p);
                        }
                    }
                    report.moved.push((file_name, to));
                } else if let Some(state) = done.not_moved {
                    report.not_moved.push((file_name, state));
                }
            }
            Err(TrackError::Abort(e)) => return Err(e),
            Err(TrackError::Failed(reason)) => {
                tracing::warn!(item_id, file = %file_name, reason, "再判定できない");
                entry.rejudge_error = Some(reason.clone());
                update_sidecar(&env.inbox, &dir, None, None, Some((&entry_name, entry)))
                    .map_err(sidecar_failed)?;
                report.failed.push((file_name, reason));
            }
        }
        progress.send_replace((i + 1, total));
    }
    for d in &touched {
        env.inbox.fsync_dir(Some(d))?;
    }
    env.jobs
        .enqueue(crate::jobs::handlers::inbox::new_inbox_job())
        .await
        .map_err(DownloadError::from)?;
    tracing::info!(
        item_id,
        total,
        rejudged = report.rejudged,
        moved = report.moved.len(),
        failed = report.failed.len(),
        "Inbox の件を判定し直した"
    );
    Ok(report)
}

/// 件を読み、pending でなければ NotPending
async fn ensure_pending(env: &DownloaderEnv, item_id: i64) -> Result<dbinbox::Item, RejudgeError> {
    match env.db.read(move |c| dbinbox::get(c, item_id)).await? {
        None => Err(RejudgeError::NotPending(format!("件 {item_id} が無い"))),
        Some(it) if it.state != ItemState::Pending => Err(RejudgeError::NotPending(format!(
            "件 {item_id} が承認前でない（{}）",
            it.state.as_str()
        ))),
        Some(it) => Ok(it),
    }
}

/// 再判定する曲 1 つ
struct Target {
    /// サイドカーの項の名前
    entry_name: String,
    /// 実際のファイル名（ふつうは項の名前と同じ）
    file_name: String,
    entry: FileEntry,
}

/// 件のディレクトリの対象の曲（項の名前順）
fn targets(root: &RootDir, dir: &RelPath) -> Result<Vec<Target>, RejudgeError> {
    let Some(sidecar) = Sidecar::read(root, dir).map_err(sidecar_failed)? else {
        return Ok(Vec::new());
    };
    let audio: Vec<String> = root
        .read_dir(Some(dir))?
        .into_iter()
        .filter(|e| e.kind == FileKind::File)
        .filter_map(|e| e.name.into_string().ok())
        .filter(|n| {
            !n.starts_with('.')
                && n.rsplit_once('.')
                    .and_then(|(_, ext)| Codec::from_extension(ext))
                    .is_some()
        })
        .collect();
    let by_key: HashMap<String, &String> = audio.iter().map(|n| (canonical_key(n), n)).collect();
    let entry_keys: HashSet<String> = sidecar.files.keys().map(|n| canonical_key(n)).collect();
    let mut claimed: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for (name, entry) in &sidecar.files {
        if entry.source != "youtube" {
            continue;
        }
        let file = match by_key.get(&canonical_key(name)) {
            Some(f) => Some((*f).clone()),
            // 前回の途中（新しい名前に置いて元を消した後、サイドカーの前）に落ちた
            None => entry.url.as_deref().and_then(|url| {
                audio
                    .iter()
                    .filter(|f| {
                        let k = canonical_key(f);
                        !entry_keys.contains(&k) && !claimed.contains(&k)
                    })
                    .find(|f| dir.join(f).is_ok_and(|r| is_own_product(root, &r, url)))
                    .cloned()
            }),
        };
        let Some(file_name) = file else {
            tracing::info!(dir = %dir, name, "サイドカーの項のファイルが無いので飛ばす");
            continue;
        };
        claimed.insert(canonical_key(&file_name));
        out.push(Target {
            entry_name: name.clone(),
            file_name,
            entry: entry.clone(),
        });
    }
    Ok(out)
}

/// 1 曲の成功
struct TrackDone {
    /// 別の件へ移した先
    moved: Option<RelPath>,
    /// 移すはずが移せなかった（移動先の件の state）
    not_moved: Option<String>,
}

enum TrackError {
    /// その曲だけの失敗（他の曲は続ける）
    Failed(String),
    /// ジョブごと止める（キャンセル）
    Abort(RejudgeError),
}

impl From<DownloadError> for TrackError {
    fn from(e: DownloadError) -> Self {
        match e {
            DownloadError::Cancelled => TrackError::Abort(DownloadError::Cancelled.into()),
            other => TrackError::Failed(other.to_string()),
        }
    }
}

impl From<FsError> for TrackError {
    fn from(e: FsError) -> Self {
        TrackError::Failed(e.to_string())
    }
}

impl From<crate::db::DbError> for TrackError {
    fn from(e: crate::db::DbError) -> Self {
        TrackError::Failed(e.to_string())
    }
}

impl From<SidecarError> for TrackError {
    fn from(e: SidecarError) -> Self {
        TrackError::Failed(format!("spindle-inbox.json を更新できない: {e}"))
    }
}

/// 1 曲を判定し直して書く。`entry` は yt-dlp で取り直した `item` を書き足したもの（失敗の記録に使う）
async fn rejudge_track(
    env: &DownloaderEnv,
    dir: &RelPath,
    file_name: &str,
    entry_name: &str,
    entry: &mut FileEntry,
    token: &CancellationToken,
) -> Result<TrackDone, TrackError> {
    let current = dir
        .join(file_name)
        .map_err(|e| TrackError::Failed(e.to_string()))?;
    // 1. Item（既存の曲には無いので、初回の手順 1 と同じ引数で取り直す）
    let item: Item = match &entry.item {
        Some(i) => i.clone(),
        None => {
            let url = entry
                .url
                .clone()
                .ok_or_else(|| TrackError::Failed("サイドカーの項に URL が無い".into()))?;
            let item = dump_video(env, &url, token).await?.item();
            entry.item = Some(item.clone());
            item
        }
    };
    // 2. 購読（消えていれば None）と判定
    let subscription = match entry.subscription_id {
        Some(id) => env.db.read(move |c| subscription_target(c, id)).await?,
        None => None,
    };
    let judged = judge(env, &item, subscription.as_ref(), token).await?;
    let from_subscription = entry.subscription_id.is_some();
    let ok = judged.verdict == "ok";
    // 3. / 4. タグを書くか。skip（購読でない）と「購読が消えて ok でない」は触らない
    let rewrite =
        subscription.is_some() || !(judged.verdict == "skip" || (from_subscription && !ok));
    let track = judged.track.as_ref().filter(|_| rewrite);
    let changes = if rewrite {
        rejudge_tag_changes(track, &item.title, entry.plugin_tags.as_deref())
    } else {
        Vec::new()
    };
    let plugin_tags = if !rewrite {
        entry.plugin_tags.clone()
    } else {
        track.filter(|_| ok).map(plugin_tag_keys)
    };
    // 移動の規則: 判定（ok か否か）と今の置き場所（受け皿か件のディレクトリか）が食い違うときだけ移す。
    // 置き場所はサイドカーの前回の判定でなく実際のディレクトリで見る（移せなかった曲を次で移せる）。
    // 購読由来は移さない。ok のまま宛先だけ違っても移さない
    let mut not_moved = None;
    let mut dest = dir.clone();
    if rewrite && !from_subscription && ok == in_unmatched_dir(dir) {
        let channel = entry.channel.as_deref().unwrap_or(&item.channel);
        let d = inbox_dir(track, channel).map_err(TrackError::Failed)?;
        if d.key() != dir.key() {
            let key = d.key();
            match env
                .db
                .read(move |c| dbinbox::find_by_dir_key(c, &key))
                .await?
            {
                Some(it) if it.state != ItemState::Pending => {
                    not_moved = Some(it.state.as_str().to_owned());
                }
                _ => dest = d,
            }
        }
    }
    let moved = dest.key() != dir.key();
    // 5. 新しい名前（タグを触らないなら名前も変えない）
    let new_name = match (rewrite, track) {
        (true, Some(t)) => inbox_file_name_for(&item, &t.title),
        (true, None) => inbox_file_name_for(&item, &item.title),
        (false, _) => file_name.to_owned(),
    };
    let target = dest
        .join(&new_name)
        .map_err(|e| TrackError::Failed(e.to_string()))?;
    // 大文字小文字・正規化だけの違いは同じファイル（置き換える）
    let in_place = target.key() == current.key();
    let final_rel = if in_place { current.clone() } else { target };
    if !(changes.is_empty() && in_place) {
        if moved {
            env.inbox.create_dir_all(&dest)?;
        }
        let url = entry.url.as_deref().or(item.url.as_deref()).unwrap_or("");
        place(&env.inbox, &current, &dest, &final_rel, &changes, url)?;
    }
    // 6. サイドカー（件の category は判定の値。語彙に無ければ足す）
    let category = track.and_then(|t| t.category.clone());
    if let Some(cat) = category.clone() {
        env.db
            .write(move |c| crate::db::categories::ensure(c, &cat).map(|_| ()))
            .await?;
    }
    let new_entry = FileEntry {
        verdict: judged.verdict.clone(),
        message: judged.message.clone(),
        plugin_version: judged.plugin_version.clone(),
        judged_at: Some(crate::db::now_epoch()),
        plugin_tags,
        item: Some(item),
        rejudge_error: None,
        ..entry.clone()
    };
    let final_name = final_rel.file_name().to_owned();
    if moved {
        // 宛先の項を先に書く（元を消した後に落ちても、宛先は自分の成果物として揃っている）
        update_sidecar(
            &env.inbox,
            &dest,
            category.as_deref(),
            None,
            Some((&final_name, new_entry)),
        )?;
        unlink_if_exists(&env.inbox, &current)?;
        update_sidecar(&env.inbox, dir, None, Some(entry_name), None)?;
    } else {
        // 元を先に消す（サイドカーの前に落ちても、次の実行が SOURCE_URL で新しい名前を拾う）
        if !in_place {
            unlink_if_exists(&env.inbox, &current)?;
        }
        update_sidecar(
            &env.inbox,
            dir,
            category.as_deref(),
            Some(entry_name),
            Some((&final_name, new_entry)),
        )?;
    }
    tracing::info!(from = %current, to = %final_rel, verdict = judged.verdict, "判定し直した");
    Ok(TrackDone {
        moved: moved.then_some(final_rel),
        not_moved,
    })
}

/// `dir` が判定できなかったものの受け皿（`youtube/_unmatched/...`）か
fn in_unmatched_dir(dir: &RelPath) -> bool {
    let mut c = dir.components().map(canonical_key);
    c.next() == Some(canonical_key(SUBDIR)) && c.next() == Some(canonical_key(UNMATCHED_DIR))
}

/// 再判定で書くタグ: TITLE / ARTIST / ALBUM / ALBUMARTIST / DATE（今回無ければ削除）+ 応答の追加タグ +
/// 前回書いた追加タグ（`recorded`）のうち今回無いもの（削除）。SOURCE_URL / TRACKNUMBER / PICTURE は
/// 触らない。`track` が None なら TITLE を動画タイトルに戻す（初回の未判定と同じ形）
fn rejudge_tag_changes(
    track: Option<&Track>,
    video_title: &str,
    recorded: Option<&[String]>,
) -> Vec<TagChange> {
    let mut changes: Vec<TagChange> = tag_changes_for(track, video_title, "", None)
        .into_iter()
        .filter(|c| c.key != SOURCE_URL_KEY)
        .collect();
    let removed = ["TITLE", "ARTIST", "ALBUM", "ALBUMARTIST", "DATE"]
        .into_iter()
        .map(str::to_owned)
        .chain(recorded.unwrap_or_default().iter().cloned());
    for key in removed {
        if !changes.iter().any(|c| c.key.eq_ignore_ascii_case(&key)) {
            changes.push(TagChange { key, values: None });
        }
    }
    changes
}

/// `current` を `dest` の tmp へ写してタグを書き、`final_rel` に置く。`final_rel` が `current` なら
/// 置き換え、違えば `RENAME_NOREPLACE`（同名があれば SOURCE_URL が `url` のときだけ自分の成果物 =
/// 前回の途中として置き換える）。元のファイルは消さない
fn place(
    root: &RootDir,
    current: &RelPath,
    dest: &RelPath,
    final_rel: &RelPath,
    changes: &[TagChange],
    url: &str,
) -> Result<(), TrackError> {
    let mut from = root.open_file(current)?;
    let (tmp_rel, mut tmp) = root.create_tmp(Some(dest))?;
    let ext = current.file_name().rsplit_once('.').map(|(_, e)| e);
    let written = (|| -> Result<(), String> {
        std::io::copy(&mut from, &mut tmp).map_err(|e| e.to_string())?;
        copy_attrs(&from, &tmp).map_err(|e| e.to_string())?;
        write_tag_changes(&mut tmp, ext, changes, None)
            .map_err(|e| format!("タグを書けない: {e}"))?;
        tmp.sync_all().map_err(|e| e.to_string())
    })();
    let placed = written.and_then(|()| {
        if final_rel.key() == current.key() {
            return root
                .replace_file(&tmp_rel, final_rel)
                .map_err(|e| e.to_string());
        }
        match root.rename_noreplace(&tmp_rel, final_rel) {
            Ok(()) => Ok(()),
            Err(FsError::Exists) if !url.is_empty() && is_own_product(root, final_rel, url) => {
                tracing::info!(path = %final_rel, "前回の途中で置いた自分の成果物を置き換える");
                root.replace_file(&tmp_rel, final_rel)
                    .map_err(|e| e.to_string())
            }
            Err(FsError::Exists) => Err(format!(
                "移動先に同名で別の内容のファイルがある: {final_rel}"
            )),
            Err(e) => Err(e.to_string()),
        }
    });
    if let Err(e) = placed {
        let _ = root.unlink(&tmp_rel);
        return Err(TrackError::Failed(e));
    }
    Ok(())
}

fn unlink_if_exists(root: &RootDir, rel: &RelPath) -> Result<(), FsError> {
    match root.unlink(rel) {
        Ok(()) | Err(FsError::NotFound) => Ok(()),
        Err(e) => Err(e),
    }
}

/// `dir` のサイドカーを読んで、`remove` の項を消し、`insert` の項を足して書く。`category` が `Some` なら
/// 件の category も置き換える
fn update_sidecar(
    root: &RootDir,
    dir: &RelPath,
    category: Option<&str>,
    remove: Option<&str>,
    insert: Option<(&str, FileEntry)>,
) -> Result<(), SidecarError> {
    let mut s = Sidecar::read(root, dir)?.unwrap_or_default();
    if let Some(c) = category {
        s.category = Some(c.to_owned());
    }
    if let Some(name) = remove {
        s.files.remove(name);
    }
    if let Some((name, entry)) = insert {
        s.files.insert(name.to_owned(), entry);
    }
    s.write(root, dir)
}

fn sidecar_failed(e: SidecarError) -> RejudgeError {
    DownloadError::Failed(anyhow::anyhow!("spindle-inbox.json を扱えない: {e}")).into()
}

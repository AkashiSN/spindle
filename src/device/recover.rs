//! 接続のたびに行う回復（仕様 ⑤「ジャーナルと中断からの回復」）。DB は見ない。
//! manifest とジャーナルを読み、完了の無い意図を実ファイルの sha256 で判定して取り込み、
//! 封印済みで `vacating` 以降のバッチは前進して完遂する（`prepared` 以前は `batch_abort` を
//! 耐久化してから一時ファイルを消す）。`.spindle-tmp` の残骸を消し、`.spindle/moving/` の残りは
//! 見失った曲と sha256 で照合して戻す。最後に manifest を書き直してジャーナルを空にする

use std::collections::{HashMap, HashSet};

use crate::device::journal::{
    self, BatchLog, BatchMember, Intent, IntentOp, MemberOp, Phase, Record, Step,
};
use crate::device::ondevice::{
    is_reserved, Book, DeviceManifest, ManifestItem, ManifestPlaylist, MOVING_DIR, TMP_SUFFIX,
};
use crate::device::remote::{DeviceFs, RemoteError, RemoteFile};
use crate::device::store::{append_durable, compact, read_journal, read_manifest, StoreError};
use crate::domain::device::{DeviceItem, EntryKind, PlaylistState};
use crate::domain::relpath::canonical_key;

/// 実ファイルが無い・サイズが違う項目のトークン。差分はこれを「更新」として出す
pub const STALE_TOKEN: &str = "";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expect {
    pub device_uuid: String,
    pub volume: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RecoverError {
    #[error("保存先が見つからない（manifest が無い。カードの差し替えか初期化の可能性）")]
    NoManifest,
    #[error("保存先の manifest が別の端末のもの")]
    UuidMismatch,
    #[error("保存先の manifest が別のボリュームのもの")]
    VolumeMismatch,
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<RemoteError> for RecoverError {
    fn from(e: RemoteError) -> Self {
        RecoverError::Store(StoreError::Remote(e))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovered {
    /// 回復後の正本（ジャーナルがあれば書き直し済み）
    pub manifest: DeviceManifest,
    /// キャッシュ（`device_items`）へ入れる値。実ファイルが無い・サイズが違う曲は [`STALE_TOKEN`]
    pub items: Vec<DeviceItem>,
    pub playlists: Vec<PlaylistState>,
    /// spindle 管理外のファイルの数（`.spindle` の下と残骸を除く）
    pub unmanaged: usize,
}

pub async fn recover<F: DeviceFs>(fs: &F, expect: &Expect) -> Result<Recovered, RecoverError> {
    let Some(manifest) = read_manifest(fs).await? else {
        return Err(RecoverError::NoManifest);
    };
    if manifest.device_uuid != expect.device_uuid {
        return Err(RecoverError::UuidMismatch);
    }
    if manifest.volume != expect.volume {
        return Err(RecoverError::VolumeMismatch);
    }
    let records = read_journal(fs).await?;
    let mut book = Book::from(manifest);
    for step in journal::replay(&records) {
        match step {
            Step::Intent { intent, done } => apply_intent(fs, &intent, done, &mut book).await?,
            Step::Batch(b) => settle_batch(fs, &b, &mut book).await?,
        }
    }

    let files = fs.list_files().await?;
    let mut present: HashSet<String> = files.iter().map(|f| canonical_key(&f.path)).collect();
    let managed = book.path_keys();
    let moving_prefix = format!("{MOVING_DIR}/");
    let mut unmanaged = 0;
    for f in &files {
        if f.path.ends_with(TMP_SUFFIX) {
            fs.remove(&f.path).await?;
        } else if f.path.starts_with(&moving_prefix) {
            if !restore_leftover(fs, f, &book, &mut present).await? {
                unmanaged += 1;
            }
        } else if !is_reserved(&f.path) && !managed.contains(&canonical_key(&f.path)) {
            unmanaged += 1;
        }
    }
    let manifest = book.manifest();
    if !records.is_empty() {
        compact(fs, &manifest).await?;
    }

    let sizes: HashMap<String, u64> = fs
        .list_files()
        .await?
        .into_iter()
        .map(|f| (canonical_key(&f.path), f.size))
        .collect();
    let items = book
        .items
        .values()
        .map(|i| DeviceItem {
            track_id: i.track_id,
            dest_path: i.path.clone(),
            token: if sizes.get(&canonical_key(&i.path)) == Some(&i.size) {
                i.token.clone()
            } else {
                STALE_TOKEN.to_owned()
            },
            size: i.size,
            sha256: i.sha256.clone(),
        })
        .collect();
    let playlists = book
        .playlists
        .values()
        .map(|p| PlaylistState {
            playlist_id: p.playlist_id,
            dest_path: p.path.clone(),
            token: if sizes.contains_key(&canonical_key(&p.path)) {
                p.token.clone()
            } else {
                STALE_TOKEN.to_owned()
            },
        })
        .collect();
    Ok(Recovered {
        manifest,
        items,
        playlists,
        unmanaged,
    })
}

/// 完了の無い意図を判定して取り込む（仕様 ⑤ の表の「追加・更新」「削除」「プレイリスト」の行）
async fn apply_intent<F: DeviceFs>(
    fs: &F,
    it: &Intent,
    done: bool,
    book: &mut Book,
) -> Result<(), RecoverError> {
    match it.op {
        IntentOp::Put => {
            let Some(to) = it.to.as_deref() else {
                return Ok(());
            };
            let landed = if done {
                true
            } else {
                fs.remove(&format!("{to}{TMP_SUFFIX}")).await?;
                it.sha256.is_some() && fs.sha256(to).await?.as_deref() == it.sha256.as_deref()
            };
            if !landed {
                return Ok(());
            }
            match it.kind {
                EntryKind::Track => {
                    book.items.insert(
                        it.ref_id,
                        ManifestItem {
                            track_id: it.ref_id,
                            path: to.to_owned(),
                            token: it.token.clone().unwrap_or_default(),
                            size: it.size.unwrap_or(0),
                            sha256: it.sha256.clone().unwrap_or_default(),
                        },
                    );
                }
                EntryKind::Playlist => {
                    book.playlists.insert(
                        it.ref_id,
                        ManifestPlaylist {
                            playlist_id: it.ref_id,
                            path: to.to_owned(),
                            token: it.token.clone().unwrap_or_default(),
                        },
                    );
                }
            }
        }
        IntentOp::Rm => {
            let Some(from) = it.from.as_deref() else {
                return Ok(());
            };
            if !done {
                fs.remove(from).await?;
            }
            let key = canonical_key(from);
            match it.kind {
                EntryKind::Track => {
                    if book
                        .items
                        .get(&it.ref_id)
                        .is_some_and(|i| canonical_key(&i.path) == key)
                    {
                        book.items.remove(&it.ref_id);
                    }
                }
                EntryKind::Playlist => {
                    if book
                        .playlists
                        .get(&it.ref_id)
                        .is_some_and(|p| canonical_key(&p.path) == key)
                    {
                        book.playlists.remove(&it.ref_id);
                    }
                }
            }
        }
    }
    Ok(())
}

async fn settle_batch<F: DeviceFs>(
    fs: &F,
    b: &BatchLog,
    book: &mut Book,
) -> Result<(), RecoverError> {
    if !b.sealed {
        // 封印前は副作用が起きていない
        return Ok(());
    }
    if b.aborted {
        remove_new_files(fs, &b.members).await?;
        return Ok(());
    }
    match b.phase {
        None | Some(Phase::Prepared) => {
            // 先に終端を耐久化してから一時ファイルを消す（回復は abort のあるバッチを進めない）
            append_durable(
                fs,
                &[Record::BatchAbort {
                    batch_id: b.batch_id.clone(),
                    superseded_by: None,
                }],
            )
            .await?;
            remove_new_files(fs, &b.members).await?;
        }
        Some(Phase::Done) => {
            for m in &b.members {
                place_in_book(book, m);
            }
        }
        Some(phase) => complete_batch(fs, &b.batch_id, &b.members, phase, true, book).await?,
    }
    Ok(())
}

async fn remove_new_files<F: DeviceFs>(fs: &F, members: &[BatchMember]) -> Result<(), StoreError> {
    for m in members {
        if let Some(new) = &m.new {
            fs.remove(new).await?;
        }
    }
    Ok(())
}

fn place_in_book(book: &mut Book, m: &BatchMember) {
    book.items.insert(
        m.track_id,
        ManifestItem {
            track_id: m.track_id,
            path: m.to.clone(),
            token: m.token.clone(),
            size: m.size,
            sha256: m.sha256.clone(),
        },
    );
}

/// `vacating` 以降のバッチを `done` まで前進させる（巻き戻さない）。`from` は最後に耐久化された相。
/// `verify` は回復のとき真: メンバーごとにファイルの実在と sha256 から位置を判定する。
/// 同期の実行中（偽）は記録どおりに `mv` するだけ。全メンバーが空くまで行き先を埋めない
pub(crate) async fn complete_batch<F: DeviceFs>(
    fs: &F,
    batch_id: &str,
    members: &[BatchMember],
    from: Phase,
    verify: bool,
    book: &mut Book,
) -> Result<(), StoreError> {
    let phase = |phase| Record::BatchPhase {
        batch_id: batch_id.to_owned(),
        phase,
    };
    if from < Phase::Vacating {
        // Vacating を記録しないまま旧パスを空けない
        append_durable(fs, &[phase(Phase::Vacating)]).await?;
    }
    if from < Phase::Vacated {
        for m in members {
            vacate(fs, m, verify).await?;
        }
        append_durable(fs, &[phase(Phase::Vacated)]).await?;
    }
    if from < Phase::Placing {
        append_durable(fs, &[phase(Phase::Placing)]).await?;
    }
    let mut placed = Vec::with_capacity(members.len());
    for m in members {
        placed.push(place(fs, m, verify).await?);
    }
    // done の前に置き場の残り（更新 + 移動の旧版は vacate で消している）を片付ける
    for m in members {
        fs.remove(&m.staging).await?;
        if let Some(new) = &m.new {
            fs.remove(new).await?;
        }
    }
    append_durable(fs, &[phase(Phase::Done)]).await?;
    for (m, ok) in members.iter().zip(placed) {
        if ok {
            place_in_book(book, m);
        } else {
            book.items.remove(&m.track_id);
        }
    }
    Ok(())
}

/// 旧パスを空ける: 移動は `from → staging`、更新 + 移動は旧版を消す。
/// 回復（verify）の位置判定は「在るかどうか」で行う（仕様 ⑤: from にあれば未着手、staging にあれば
/// 空け済み）。内容の食い違いはここでは見ない。最後のサイズ照合（STALE_TOKEN）と「内容を検証」に任せる
async fn vacate<F: DeviceFs>(fs: &F, m: &BatchMember, verify: bool) -> Result<(), StoreError> {
    match m.op {
        MemberOp::UpdateMove => fs.remove(&m.from).await?,
        MemberOp::Move if !verify => fs.rename(&m.from, &m.staging).await?,
        MemberOp::Move => {
            if exists(fs, &m.staging).await? {
                return Ok(()); // 空け済み
            }
            if exists(fs, &m.from).await? {
                fs.rename(&m.from, &m.staging).await?;
            }
            // どちらにも無い: 手で消された。place で見つからず、manifest から外れる
        }
    }
    Ok(())
}

async fn exists<F: DeviceFs>(fs: &F, path: &str) -> Result<bool, StoreError> {
    Ok(fs.sha256(path).await?.is_some())
}

/// 行き先を埋める: 移動は `staging → to`、更新 + 移動は `new → to`。置けたら true。
/// 回復では src が在れば置き、無くて to が在れば置き済みとみなす（内容は見ない）
async fn place<F: DeviceFs>(fs: &F, m: &BatchMember, verify: bool) -> Result<bool, StoreError> {
    let src = match (m.op, &m.new) {
        (MemberOp::Move, _) => m.staging.as_str(),
        (MemberOp::UpdateMove, Some(new)) => new.as_str(),
        (MemberOp::UpdateMove, None) => return Ok(false),
    };
    if !verify {
        fs.rename(src, &m.to).await?;
        return Ok(true);
    }
    if exists(fs, src).await? {
        fs.rename(src, &m.to).await?;
        return Ok(true);
    }
    exists(fs, &m.to).await
}

/// `.spindle/moving/` の残りを、実ファイルを見失った管理下の曲と sha256 で照合して戻す。戻せたら true
async fn restore_leftover<F: DeviceFs>(
    fs: &F,
    f: &RemoteFile,
    book: &Book,
    present: &mut HashSet<String>,
) -> Result<bool, StoreError> {
    let Some(sha) = fs.sha256(&f.path).await? else {
        return Ok(false);
    };
    let owner = book
        .items
        .values()
        .find(|i| i.sha256 == sha && !present.contains(&canonical_key(&i.path)));
    match owner {
        Some(i) => {
            fs.rename(&f.path, &i.path).await?;
            fs.sync().await?;
            present.insert(canonical_key(&i.path));
            Ok(true)
        }
        None => Ok(false),
    }
}

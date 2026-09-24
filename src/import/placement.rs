//! 新しいファイルを Library に置いて登録するときの共通部分（D-67 / D-68）。CD の配置
//! （`cd::place`）と Inbox の配置（`import::inbox`）が使う。
//!
//! - `place_one`: `dir` 内の tmp に内容を写して整形し、`RENAME_NOREPLACE` で置く。既にあれば
//!   自分の成果物かを呼び出し側の判定で決める（冪等な再実行）
//! - `remove_placed`: 登録に失敗したとき、この呼び出しで新しく置いたファイルだけを消す
//! - `find_or_create_album`: 宛先ディレクトリの album を計画のリリースキーで再検証して返す
//!   （rename は `library` の排他を取らないので、計画と登録の間に別の album が入り得る）
//! - `register_track`: 同じパスの行があれば音声の同一性が一致するときだけ採用、無ければ挿入

use std::fs::File;
use std::io::Read;

use rusqlite::{params, Connection, OptionalExtension as _};

use crate::db::scans::{self, AlbumMeta, Fingerprint, Physical, TrackContent};
use crate::db::DbError;
use crate::domain::relpath::RelPath;
use crate::fsroot::{FsError, RootDir};

#[derive(Debug, thiserror::Error)]
pub enum PlacementError {
    #[error("配置先が衝突: {0}")]
    Conflict(String),
    #[error(transparent)]
    Fs(#[from] FsError),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// 配置したファイル 1 本の結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacedFile {
    New,
    Reused,
}

/// `content` を `dir` 内の tmp へ写し、`prepare`（タグの書き込み等。tmp を読み書きで受ける）を
/// 通してから `target` へ `RENAME_NOREPLACE`。既にあれば `verify_existing` で自分の成果物か判定し、
/// そうなら `Reused`、違えば `Conflict`。失敗したら tmp は消す
pub fn place_one(
    root: &RootDir,
    dir: &RelPath,
    target: &RelPath,
    content: impl Read,
    prepare: impl FnOnce(&mut File) -> Result<(), PlacementError>,
    verify_existing: impl FnOnce(File) -> Result<bool, PlacementError>,
) -> Result<PlacedFile, PlacementError> {
    place(root, dir, target, content, prepare, verify_existing, false)
}

/// [`place_one`] と同じだが、既存が自分の成果物（`verify_existing` が true）なら捨てずに今回の tmp で
/// **置き換える**（`Reused`）。承認画面の補正（任意のタグ・画像。D-86）はパスを変えないので、途中で
/// 落ちた件を直して再承認すると同じ宛先に当たる。そのまま再利用すると今回の補正が黙って捨てられる。
/// **呼び出し側は、宛先に active な行が無い（登録前に落ちた孤児）ときだけ使う**こと。登録済みのファイルは
/// ファイルが正で、外部の変更を履歴なしに上書きしてはならない（codex 指摘）。置き換える直前に、確かめた FD と
/// 今の宛先が同じ実体（dev / inode / size / mtime / ctime）であることを照合し、違えば Conflict
pub fn place_one_refreshing(
    root: &RootDir,
    dir: &RelPath,
    target: &RelPath,
    content: impl Read,
    prepare: impl FnOnce(&mut File) -> Result<(), PlacementError>,
    verify_existing: impl FnOnce(File) -> Result<bool, PlacementError>,
) -> Result<PlacedFile, PlacementError> {
    place(root, dir, target, content, prepare, verify_existing, true)
}

fn place(
    root: &RootDir,
    dir: &RelPath,
    target: &RelPath,
    mut content: impl Read,
    prepare: impl FnOnce(&mut File) -> Result<(), PlacementError>,
    verify_existing: impl FnOnce(File) -> Result<bool, PlacementError>,
    refresh: bool,
) -> Result<PlacedFile, PlacementError> {
    let (tmp_rel, mut tmp) = root.create_tmp(Some(dir))?;
    let written = (|| -> Result<(), PlacementError> {
        std::io::copy(&mut content, &mut tmp)?;
        prepare(&mut tmp)?;
        tmp.sync_all()?;
        Ok(())
    })();
    if let Err(e) = written {
        let _ = root.unlink(&tmp_rel);
        return Err(e);
    }
    match root.rename_noreplace(&tmp_rel, target) {
        Ok(()) => Ok(PlacedFile::New),
        Err(FsError::Exists) => {
            let (existing, verified) = match root
                .open_file(target)
                .and_then(|f| crate::fsroot::fstat(&f).map(|st| (f, st)))
            {
                Ok(v) => v,
                Err(e) => {
                    let _ = root.unlink(&tmp_rel);
                    return Err(e.into());
                }
            };
            let own = match verify_existing(existing) {
                Ok(v) => v,
                Err(e) => {
                    let _ = root.unlink(&tmp_rel);
                    return Err(e);
                }
            };
            if own && refresh {
                // 確かめたものと同じ実体のときだけ、今回の内容（同じ音声 + 今回の補正）で置き換える
                let same = root.stat(target).is_ok_and(|now| {
                    (now.dev, now.inode, now.size, now.mtime_ns, now.ctime_ns)
                        == (
                            verified.dev,
                            verified.inode,
                            verified.size,
                            verified.mtime_ns,
                            verified.ctime_ns,
                        )
                });
                if !same {
                    let _ = root.unlink(&tmp_rel);
                    return Err(PlacementError::Conflict(format!(
                        "{target}: 確かめている間に宛先が変わった"
                    )));
                }
                if let Err(e) = root.replace_file(&tmp_rel, target) {
                    let _ = root.unlink(&tmp_rel);
                    return Err(e.into());
                }
                return Ok(PlacedFile::Reused);
            }
            let _ = root.unlink(&tmp_rel);
            if own {
                Ok(PlacedFile::Reused)
            } else {
                Err(PlacementError::Conflict(format!(
                    "{target}: 別の内容のファイルが既にある"
                )))
            }
        }
        Err(e) => {
            let _ = root.unlink(&tmp_rel);
            Err(e.into())
        }
    }
}

/// この呼び出しで新しく置いたファイルを消し、ディレクトリが空なら消す（登録に失敗したとき）
pub fn remove_placed(root: &RootDir, dir: &RelPath, placed_new: &[RelPath]) {
    for rel in placed_new {
        match root.unlink(rel) {
            Ok(()) | Err(FsError::NotFound) => {}
            Err(u) => tracing::warn!(path = %rel, error = %u, "配置したファイルを消せない"),
        }
    }
    let _ = root.remove_dir(dir);
}

/// 既存 album のリリースキー（`edit::rename::load_occupancy` と同じ規則: mb → album）。DiscID は
/// 1 枚ごとの値なので鍵にしない（D-67 追記 3）
pub fn release_key(id: i64, mb: Option<&str>) -> String {
    match mb {
        Some(m) if !m.is_empty() => mb_key(m),
        _ => format!("album:{id}"),
    }
}

/// MBID のリリースキー `mb:<id>`。**比較用に ASCII 小文字へ揃える**（外部のツールが大文字の UUID を書く。
/// タグと `mb_release_id` の値はそのまま保つ。D-84）
pub fn mb_key(mbid: &str) -> String {
    format!("mb:{}", mbid.trim().to_ascii_lowercase())
}

/// 宛先ディレクトリの album を計画のリリースキー `release` で再検証して返す。active で同じキー →
/// 採用、active で別キー → `Err(理由)`（衝突）、missing で同じキー → 復活、missing で別キー →
/// `\0displaced:` へ退かせて新規（rename の coordinator と同じ）、無ければ `meta` で新規
pub fn find_or_create_album(
    tx: &Connection,
    rel_dir: &RelPath,
    release: &str,
    meta: &AlbumMeta,
) -> Result<Result<i64, String>, PlacementError> {
    struct Existing {
        id: i64,
        missing: bool,
        mb: Option<String>,
    }
    let key = rel_dir.key();
    let existing: Option<Existing> = tx
        .query_row(
            "SELECT id, missing_since, mb_release_id FROM albums WHERE rel_dir_key = ?1",
            [&key],
            |r| {
                Ok(Existing {
                    id: r.get(0)?,
                    missing: r.get::<_, Option<i64>>(1)?.is_some(),
                    mb: r.get(2)?,
                })
            },
        )
        .optional()?;
    let Some(e) = existing else {
        return Ok(Ok(scans::insert_album(tx, rel_dir.as_str(), &key, meta)?));
    };
    let same = release_key(e.id, e.mb.as_deref()) == release;
    Ok(match (e.missing, same) {
        (false, true) => Ok(e.id),
        (false, false) => Err(format!(
            "{rel_dir}: 計画の後に別のリリースの album {} が入った",
            e.id
        )),
        (true, true) => {
            tx.execute(
                "UPDATE albums SET missing_since = NULL WHERE id = ?1",
                [e.id],
            )?;
            Ok(e.id)
        }
        (true, false) => {
            let displaced = format!("\0displaced:{}", e.id);
            tx.execute(
                "UPDATE albums SET rel_dir = ?2, rel_dir_key = ?2 WHERE id = ?1",
                params![e.id, displaced],
            )?;
            Ok(scans::insert_album(tx, rel_dir.as_str(), &key, meta)?)
        }
    })
}

/// 登録した行
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Registered {
    pub id: i64,
    /// 既存の行を採用した（配置の後に落ちてスキャナが先に拾った等）
    pub adopted: bool,
    pub audio_version: i64,
}

/// 同じパスの active な行があれば、音声の同一性（可逆は `audio_md5`、非可逆は `audio_fp`）が
/// 一致するときだけ採用し、違えば `Err(理由)`。無ければ挿入する（`source_type` は呼び出し側）
pub fn register_track(
    tx: &Connection,
    rel: &RelPath,
    ph: &Physical,
    content: &TrackContent,
    fp: Fingerprint,
    now: i64,
) -> Result<Result<Registered, String>, PlacementError> {
    struct Row {
        id: i64,
        md5: Option<Vec<u8>>,
        afp: Option<Vec<u8>>,
        version: i64,
    }
    let rel_key = rel.key();
    let existing: Option<Row> = tx
        .query_row(
            "SELECT id, audio_md5, audio_fp, audio_version FROM tracks
              WHERE rel_path_key = ?1 AND missing_since IS NULL",
            [&rel_key],
            |r| {
                Ok(Row {
                    id: r.get(0)?,
                    md5: r.get(1)?,
                    afp: r.get(2)?,
                    version: r.get(3)?,
                })
            },
        )
        .optional()?;
    let Some(Row {
        id,
        md5,
        afp,
        version,
    }) = existing
    else {
        let id = scans::insert_track(tx, rel.as_str(), &rel_key, ph, content, fp, None, now)?;
        return Ok(Ok(Registered {
            id,
            adopted: false,
            audio_version: 1,
        }));
    };
    let same = match fp {
        Fingerprint::Md5(Some(m)) => md5.as_deref() == Some(m.as_slice()),
        Fingerprint::Fp(Some(f)) => afp.as_deref() == Some(f.as_slice()),
        Fingerprint::Md5(None) | Fingerprint::Fp(None) => false,
    };
    if same {
        Ok(Ok(Registered {
            id,
            adopted: true,
            audio_version: version,
        }))
    } else {
        Ok(Err(format!("{rel}: 別の音声の行（track {id}）が既にある")))
    }
}

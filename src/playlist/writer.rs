//! プレイリスト 1 本をプロファイルで m3u8 に組み、Playlists root へ書く（P1-6 / P1-7、D-53）。
//! API の `POST /api/playlists/:id/export` と自動再書き出し（`autoexport`）が共用する

use std::io::Write;
use std::sync::Arc;

use crate::db::playlists::{self as dbpl, Playlist, ProfileRow};
use crate::db::{now_epoch, Db, DbError};
use crate::domain::relpath::RelPath;
use crate::fsroot::{FsError, RootDir};

use super::export::{render_m3u8, EXPORT_EXT};

/// 組み立て結果
pub struct Rendered {
    pub playlist: Playlist,
    pub profile: ProfileRow,
    pub body: String,
    pub count: usize,
    pub skipped_missing: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("profile が不明")]
    NoProfile,
    #[error("プレイリストが無い")]
    NotFound,
    #[error("format={0} のプロファイルは m3u8 を書けない")]
    BadFormat(String),
    #[error(transparent)]
    Db(#[from] DbError),
}

/// DB から読んで m3u8 を組む（ファイルは書かない）
pub async fn render(db: &Db, id: i64, profile: &str) -> Result<Rendered, RenderError> {
    let profile = profile.to_owned();
    let result = db
        .read(move |c| {
            let Some(profile) = dbpl::profile_by_name(c, &profile)? else {
                return Ok(None);
            };
            let Some(playlist) = dbpl::get(c, id)? else {
                return Ok(Some(None));
            };
            let Some((rows, skipped)) = dbpl::export_tracks(c, id, profile.profile.source)? else {
                return Ok(Some(None));
            };
            Ok(Some(Some((playlist, profile, rows, skipped))))
        })
        .await?;
    match result {
        None => Err(RenderError::NoProfile),
        Some(None) => Err(RenderError::NotFound),
        Some(Some((playlist, profile, rows, skipped_missing))) => {
            if profile.format != "m3u8" {
                return Err(RenderError::BadFormat(profile.format.clone()));
            }
            let body = render_m3u8(&profile.profile, &rows);
            Ok(Rendered {
                playlist,
                profile,
                body,
                count: rows.len(),
                skipped_missing,
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// Playlists root からの相対パス
    pub out_path: String,
    pub count: usize,
    pub skipped_missing: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error(transparent)]
    Render(#[from] RenderError),
    #[error("出力パスが不正: {0}")]
    BadPath(String),
    #[error("m3u8 を書けない: {0}")]
    Fs(#[from] FsError),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("書き込みタスクが異常終了: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// `Playlists/<profile>/<name>.m3u8` に tmp + rename で書き、`playlist_exports` に記録する
pub async fn export_to_root(
    db: &Db,
    root: Arc<RootDir>,
    id: i64,
    profile: &str,
) -> Result<Written, WriteError> {
    let r = render(db, id, profile).await?;
    let dir = RelPath::parse(&r.profile.name).map_err(|e| WriteError::BadPath(e.to_string()))?;
    let dst = dir
        .join(&format!("{}{EXPORT_EXT}", r.playlist.name))
        .map_err(|e| WriteError::BadPath(e.to_string()))?;
    let out_path = dst.as_str().to_owned();
    let body = r.body;
    tokio::task::spawn_blocking(move || write_atomic(&root, &dir, &dst, body.as_bytes())).await??;
    let profile_id = r.profile.id;
    let rec = out_path.clone();
    db.write(move |c| dbpl::record_export(c, id, profile_id, &rec, now_epoch()))
        .await?;
    Ok(Written {
        out_path,
        count: r.count,
        skipped_missing: r.skipped_missing,
    })
}

/// `dir` を作り、tmp に書いて fsync → rename で `dst` に置く。失敗時は tmp を消す
fn write_atomic(root: &RootDir, dir: &RelPath, dst: &RelPath, bytes: &[u8]) -> Result<(), FsError> {
    root.create_dir_all(dir)?;
    let (tmp, mut file) = root.create_tmp(Some(dir))?;
    let r = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok::<(), std::io::Error>(())
    })();
    if let Err(e) = r {
        let _ = root.unlink(&tmp);
        return Err(FsError::Io(e));
    }
    drop(file);
    if let Err(e) = root.replace_file(&tmp, dst) {
        let _ = root.unlink(&tmp);
        return Err(e);
    }
    Ok(())
}

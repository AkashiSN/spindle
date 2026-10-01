//! ミュージック.app の操作（仕様 ⑥）。本物は `osascript` で JXA を実行する（P5-4c）。
//! 実装の契約: 外部プロセスはタイムアウトと終了コードの検査を伴い、stderr をログに出す。
//! 名前の比較は `canonical_key`。persistent ID は 16 進の文字列で扱う

use std::path::{Path, PathBuf};

use crate::{Error, Result};

#[cfg(any(test, feature = "fake"))]
pub mod fake;
pub mod jxa;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MusicTrack {
    pub persistent_id: String,
    pub database_id: i64,
    /// file track の場所（見つからない file track は None）
    pub location: Option<PathBuf>,
    /// 追加日時（epoch 秒）
    pub date_added: i64,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MusicPlaylist {
    pub persistent_id: String,
    pub name: String,
}

pub trait Music {
    /// 読み取りだけの無害な操作（初回の Automation 許可を求める）
    fn probe(&self) -> Result<()>;
    /// 「［ミュージック］フォルダにコピー」の設定。読めなければ None
    fn copy_to_library(&self) -> Result<Option<bool>>;
    /// file track のうち `location` が `root` の下にあるもの
    fn tracks_under(&self, root: &Path) -> Result<Vec<MusicTrack>>;
    fn track(&self, persistent_id: &str) -> Result<Option<MusicTrack>>;
    /// database ID が `after` より大きい file track
    fn tracks_added_after(&self, after: i64) -> Result<Vec<MusicTrack>>;
    fn max_database_id(&self) -> Result<i64>;
    fn add(&self, path: &Path) -> Result<MusicTrack>;
    fn delete_track(&self, persistent_id: &str) -> Result<()>;
    fn set_location(&self, persistent_id: &str, path: &Path) -> Result<()>;
    fn refresh(&self, persistent_id: &str) -> Result<()>;
    /// トップレベルのフォルダのうち名前が `name` のもの
    fn folders_named(&self, name: &str) -> Result<Vec<MusicPlaylist>>;
    fn folder(&self, persistent_id: &str) -> Result<Option<MusicPlaylist>>;
    fn create_folder(&self, name: &str) -> Result<MusicPlaylist>;
    /// フォルダ・プレイリストの改名
    fn rename_playlist(&self, persistent_id: &str, name: &str) -> Result<()>;
    /// フォルダの直下のプレイリスト（フォルダを除く）
    fn playlists_in(&self, folder_id: &str) -> Result<Vec<MusicPlaylist>>;
    fn create_playlist(&self, folder_id: &str, name: &str) -> Result<MusicPlaylist>;
    /// 中身を `tracks`（persistent ID の順）で置き換える
    fn set_playlist_tracks(&self, persistent_id: &str, tracks: &[String]) -> Result<()>;
    fn delete_playlist(&self, persistent_id: &str) -> Result<()>;
}

/// P5-4c までの本番用の実装（全操作が失敗する）
pub struct UnsupportedMusic;

fn unsupported<T>() -> Result<T> {
    Err(Error::Music(
        "ミュージック.app の操作は P5-4c で実装する".to_owned(),
    ))
}

impl Music for UnsupportedMusic {
    fn probe(&self) -> Result<()> {
        unsupported()
    }
    fn copy_to_library(&self) -> Result<Option<bool>> {
        unsupported()
    }
    fn tracks_under(&self, _: &Path) -> Result<Vec<MusicTrack>> {
        unsupported()
    }
    fn track(&self, _: &str) -> Result<Option<MusicTrack>> {
        unsupported()
    }
    fn tracks_added_after(&self, _: i64) -> Result<Vec<MusicTrack>> {
        unsupported()
    }
    fn max_database_id(&self) -> Result<i64> {
        unsupported()
    }
    fn add(&self, _: &Path) -> Result<MusicTrack> {
        unsupported()
    }
    fn delete_track(&self, _: &str) -> Result<()> {
        unsupported()
    }
    fn set_location(&self, _: &str, _: &Path) -> Result<()> {
        unsupported()
    }
    fn refresh(&self, _: &str) -> Result<()> {
        unsupported()
    }
    fn folders_named(&self, _: &str) -> Result<Vec<MusicPlaylist>> {
        unsupported()
    }
    fn folder(&self, _: &str) -> Result<Option<MusicPlaylist>> {
        unsupported()
    }
    fn create_folder(&self, _: &str) -> Result<MusicPlaylist> {
        unsupported()
    }
    fn rename_playlist(&self, _: &str, _: &str) -> Result<()> {
        unsupported()
    }
    fn playlists_in(&self, _: &str) -> Result<Vec<MusicPlaylist>> {
        unsupported()
    }
    fn create_playlist(&self, _: &str, _: &str) -> Result<MusicPlaylist> {
        unsupported()
    }
    fn set_playlist_tracks(&self, _: &str, _: &[String]) -> Result<()> {
        unsupported()
    }
    fn delete_playlist(&self, _: &str) -> Result<()> {
        unsupported()
    }
}

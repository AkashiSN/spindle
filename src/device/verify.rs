//! 内容の検証（仕様 ⑤「内容の検証」。差分画面の「内容を検証」ジョブの本体）。
//! 日常の差分はサイズまでしか見ないので、同じサイズの破損や端末側の書き換えはここで見つける。
//! 食い違った曲・欠損した曲は manifest のトークンを空にして書き直す（次の差分で更新に戻る。
//! ファイルは消さない。manifest から外すと管理外になり、同期が上書きできない）。
//! 呼び出し側は同じ完了処理の中で `device_items` も全置換すること

use crate::device::ondevice::{Book, DeviceManifest};
use crate::device::recover::STALE_TOKEN;
use crate::device::remote::DeviceFs;
use crate::device::store::{compact, read_journal_with_len};
use crate::device::sync::{Control, SyncError};
use crate::domain::device::{DeviceItem, PlaylistState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub manifest: DeviceManifest,
    pub mismatched: Vec<i64>,
    pub missing: Vec<i64>,
}

impl VerifyReport {
    pub fn items(&self) -> Vec<DeviceItem> {
        Book::from(self.manifest.clone()).device_items()
    }

    pub fn playlists(&self) -> Vec<PlaylistState> {
        Book::from(self.manifest.clone()).playlist_states()
    }
}

/// `start` には回復済み（ジャーナルが空）の正本を渡すこと。呼ばれた時点でジャーナルが空でなければ
/// [`SyncError::NotRecovered`] を返す（書き直しの圧縮で未完了の意図を捨てないため）
pub async fn verify<F: DeviceFs, C: Control>(
    fs: &F,
    control: &C,
    start: DeviceManifest,
) -> Result<VerifyReport, SyncError> {
    if read_journal_with_len(fs).await?.1 > 0 {
        return Err(SyncError::NotRecovered);
    }
    let mut book = Book::from(start);
    let total = book.items.len() as u64;
    let mut mismatched = Vec::new();
    let mut missing = Vec::new();
    for (n, item) in book.items.values().enumerate() {
        if control.cancelled() {
            return Err(SyncError::Cancelled);
        }
        match fs.sha256(&item.path).await? {
            Some(h) if h == item.sha256 => {}
            Some(_) => mismatched.push(item.track_id),
            None => missing.push(item.track_id),
        }
        control.progress(n as u64 + 1, total).await;
    }
    if !mismatched.is_empty() || !missing.is_empty() {
        // トークンを空にして次の差分で「更新」に戻す（外すと管理外になり上書きできない）
        for id in mismatched.iter().chain(&missing) {
            if let Some(item) = book.items.get_mut(id) {
                item.token = STALE_TOKEN.to_owned();
            }
        }
        compact(fs, &book.manifest()).await?;
    }
    Ok(VerifyReport {
        manifest: book.manifest(),
        mismatched,
        missing,
    })
}

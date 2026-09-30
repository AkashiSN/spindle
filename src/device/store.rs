//! 端末側の正本の読み書きと耐久化の順序（仕様 ⑤「耐久化の順序」）。
//! - ジャーナルの追記: 追記 → `sync`。意図の `sync` が返ってから副作用を起こす
//! - ファイルの置き換え: 同じディレクトリの tmp に書く → `sync` → `mv -f` → `sync`
//! - 圧縮: manifest を書き直して確定させてから、ジャーナルを空にする（逆だと回復できない）

use crate::device::journal::{self, JournalError, Record};
use crate::device::ondevice::{
    self, DeviceManifest, ManifestError, JOURNAL_PATH, MANIFEST_PATH, TMP_SUFFIX,
};
use crate::device::remote::{DeviceFs, DirState, RemoteError};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Journal(#[from] JournalError),
}

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("保存先が空でない（既存のファイルの扱いを推測しないので、別の場所を選んでください）")]
    NotEmpty,
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<RemoteError> for InitError {
    fn from(e: RemoteError) -> Self {
        InitError::Store(StoreError::Remote(e))
    }
}

pub async fn append_durable<F: DeviceFs>(fs: &F, records: &[Record]) -> Result<(), StoreError> {
    if records.is_empty() {
        return Ok(());
    }
    fs.append(JOURNAL_PATH, &journal::encode_all(records)?)
        .await?;
    fs.sync().await?;
    Ok(())
}

pub async fn write_file_durable<F: DeviceFs>(
    fs: &F,
    path: &str,
    bytes: &[u8],
) -> Result<(), StoreError> {
    let tmp = format!("{path}{TMP_SUFFIX}");
    fs.write(&tmp, bytes).await?;
    fs.sync().await?;
    fs.rename(&tmp, path).await?;
    fs.sync().await?;
    Ok(())
}

pub async fn read_manifest<F: DeviceFs>(fs: &F) -> Result<Option<DeviceManifest>, StoreError> {
    match fs.read(MANIFEST_PATH).await? {
        Some(bytes) => Ok(Some(ondevice::parse(&bytes)?)),
        None => Ok(None),
    }
}

pub async fn read_journal<F: DeviceFs>(fs: &F) -> Result<Vec<Record>, StoreError> {
    Ok(read_journal_with_len(fs).await?.0)
}

/// ジャーナルのレコードと生のバイト長。切れた断片だけのとき（レコード 0 件）も長さは 0 にならない
pub async fn read_journal_with_len<F: DeviceFs>(
    fs: &F,
) -> Result<(Vec<Record>, usize), StoreError> {
    match fs.read(JOURNAL_PATH).await? {
        Some(bytes) => Ok((journal::parse(&bytes)?, bytes.len())),
        None => Ok((Vec::new(), 0)),
    }
}

/// 末尾の切れた行（`parse` が捨てた断片）を落としたジャーナルに置き換える（tmp + `mv -f` で差し替える）。
/// 断片を残したまま追記すると、新しいレコードが断片と 1 行に連結されて読めなくなる。
/// 読めたレコードを書き直したものが今のバイト列と同じなら何もしない
pub async fn drop_torn_tail<F: DeviceFs>(fs: &F, records: &[Record]) -> Result<(), StoreError> {
    let Some(bytes) = fs.read(JOURNAL_PATH).await? else {
        return Ok(());
    };
    let clean = journal::encode_all(records)?;
    if bytes != clean {
        write_file_durable(fs, JOURNAL_PATH, &clean).await?;
    }
    Ok(())
}

pub async fn write_manifest<F: DeviceFs>(fs: &F, m: &DeviceManifest) -> Result<(), StoreError> {
    write_file_durable(fs, MANIFEST_PATH, &ondevice::render(m)?).await
}

/// manifest を確定させてからジャーナルを空にする
pub async fn compact<F: DeviceFs>(fs: &F, m: &DeviceManifest) -> Result<(), StoreError> {
    write_manifest(fs, m).await?;
    fs.write(JOURNAL_PATH, b"").await?;
    fs.sync().await?;
    Ok(())
}

/// 登録（仕様 ⑤「登録」4）: root が空か存在しないときだけ manifest を作る
pub async fn initialize<F: DeviceFs>(
    fs: &F,
    device_uuid: &str,
    volume: &str,
) -> Result<DeviceManifest, InitError> {
    if fs.root_state().await? == DirState::NonEmpty {
        return Err(InitError::NotEmpty);
    }
    let m = DeviceManifest::new(device_uuid, volume);
    compact(fs, &m).await?;
    Ok(m)
}

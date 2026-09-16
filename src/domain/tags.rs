//! タグ集合の正規化とハッシュ（SPEC §6「変更検出と版の遷移」）。
//!
//! `tag_version` は `tag_hash` の実差分があるときだけ進める。同じ内容の再保存（外部ツールの
//! 同値保存、tagwrite の再実行）で Derived の追随が空回りしないよう、表記差（キーの大小文字、
//! 値の NFC / NFD、キーの並び）を吸収してからハッシュする。lofty からの読み取りは P0-6。

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// 埋め込み画像を表す擬似キー。カバー差し替えも `tag_version` を進める（Derived のタグ追随対象）
const PICTURE_KEY: &str = "PICTURE";

/// 正規化済みのタグ集合。キーは大文字、値は NFC、キー順に整列（同一キーの多値は入力順を保持）
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TagSet {
    items: Vec<(String, String)>,
}

impl TagSet {
    pub fn items(&self) -> &[(String, String)] {
        &self.items
    }

    /// 埋め込み画像をバイト列のハッシュとして加える。画像そのものは保持しない
    pub fn add_picture(&mut self, mime: &str, data: &[u8]) {
        let digest = Sha256::digest(data);
        let value = format!("{mime}:{}", hex(&digest));
        self.insert(PICTURE_KEY.to_owned(), value);
    }

    /// キー順を保ったまま末尾（同一キーの最後）に挿入する
    fn insert(&mut self, key: String, value: String) {
        let pos = self
            .items
            .partition_point(|(k, _)| k.as_str() <= key.as_str());
        self.items.insert(pos, (key, value));
    }
}

/// `(key, value)` の列を正規化タグ集合にする
pub fn normalize_tags<I>(items: I) -> TagSet
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut set = TagSet::default();
    for (key, value) in items {
        let key = key.to_uppercase();
        let value: String = value.nfc().collect();
        set.insert(key, value);
    }
    set
}

/// 正規化タグ集合の SHA-256。`KEY` と値をそれぞれ長さ前置で連結し、区切りの曖昧さを無くす
pub fn tag_hash(set: &TagSet) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for (key, value) in &set.items {
        for part in [key, value] {
            hasher.update((part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
    }
    hasher.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

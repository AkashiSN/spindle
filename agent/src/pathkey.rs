//! パスの鍵と、root 相対パス ↔ 絶対パス。比較は必ず `canonical_key`（casefold + NFD。本体の
//! `domain::relpath::canonical_key` と同じ定義。APFS の大小文字・正規化を区別しない比較に合わせる）

use std::path::{Component, Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

use crate::{Error, Result};

pub fn canonical_key(s: &str) -> String {
    let nfd: String = s.nfd().collect();
    let folded = caseless::default_case_fold_str(&nfd);
    folded.nfd().collect()
}

/// サーバから来た root 相対パス（`/` 区切り）を検証する。空・先頭 `/`・空の要素・`.`・`..`・NUL を拒否
pub fn check_rel(rel: &str) -> std::result::Result<(), String> {
    if rel.is_empty() {
        return Err("パスが空".to_owned());
    }
    if rel.starts_with('/') {
        return Err(format!("絶対パスは使えない（{rel}）"));
    }
    if rel.contains('\0') {
        return Err("パスに NUL がある".to_owned());
    }
    for c in rel.split('/') {
        if c.is_empty() || c == "." || c == ".." {
            return Err(format!("パスの要素が不正（{rel}）"));
        }
    }
    Ok(())
}

pub fn to_abs(root: &Path, rel: &str) -> Result<PathBuf> {
    check_rel(rel).map_err(Error::Stop)?;
    let mut p = root.to_path_buf();
    for c in rel.split('/') {
        p.push(c);
    }
    Ok(p)
}

/// root の下の絶対パスを root 相対（`/` 区切り）にする。root の外・UTF-8 でない要素は None
pub fn to_rel(root: &Path, abs: &Path) -> Option<String> {
    let rest = abs.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for c in rest.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str()?.to_owned()),
            _ => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// 128 bit の乱数を小文字 16 進で（op_id・batch_id・nonce）
pub fn random_id() -> Result<String> {
    let mut raw = [0u8; 16];
    getrandom::fill(&mut raw).map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
    Ok(raw.iter().map(|b| format!("{b:02x}")).collect())
}

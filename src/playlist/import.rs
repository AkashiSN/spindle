//! m3u8 の取り込み（docs/TASKS.md P1-6、D-53）。
//!
//! 行を Library のトラックへ解決する規則:
//! 1. `\` を `/` に。相対なら先頭の `./` `../` を剥がし先頭の `Library/` / `Derived/` を落とす。
//!    絶対（Unix / UNC / ドライブレター）なら root 名の出現以降を取り、無ければ解決しない
//! 2. `rel_path_key`（casefold + NFD）の完全一致
//! 3. 拡張子を除いた stem の一致。旧ライブラリの `.opus` 行が今の `.m4a` / `.flac` に当たる。
//!    候補が複数なら active（`missing_since IS NULL`）を優先し、その中でまだ複数なら曖昧として
//!    解決しない（どれか分からないものを黙って選ばない）
//!
//! 解決できない行は結果に返すだけで DB には残さない

use std::collections::HashMap;

use crate::domain::relpath::canonical_key;

/// media root の名前。この後ろが root 相対パス
const ROOT_NAMES: [&str; 2] = ["Library", "Derived"];

/// m3u / m3u8 の本文からパス行だけを取り出す。BOM・空行・`#` 行（EXTM3U / EXTINF）は落とす
pub fn parse_m3u8(text: &str) -> Vec<String> {
    text.trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// 行を root 相対パスに正規化する。URL・root 名を含まない絶対パスなど相対にできないものは `None`
pub fn normalize_entry(line: &str) -> Option<String> {
    let s = line.trim().replace('\\', "/");
    if s.is_empty() || s.contains("://") {
        return None;
    }
    let rest: &str = if is_absolute(&s) {
        // 絶対パス（Unix / UNC `//host/share/…` / ドライブレター `C:/…`）: 文字列中で最も早く現れる
        // root 名の直後から取る。無ければ root の外なので当てない（偶然同じ rel_path があっても
        // 誤一致させない）
        ROOT_NAMES
            .iter()
            .filter_map(|root| {
                let needle = format!("/{root}/");
                s.find(&needle).map(|i| (i, i + needle.len()))
            })
            .min_by_key(|(start, _)| *start)
            .map(|(_, end)| &s[end..])?
    } else {
        // 相対: ./ ../ を剥がし、先頭の root 名を落とす（Library の中に `Library` という
        // ディレクトリがあっても切らない）
        let mut rest: &str = &s;
        loop {
            if let Some(r) = rest.strip_prefix("../") {
                rest = r;
            } else if let Some(r) = rest.strip_prefix("./") {
                rest = r;
            } else {
                break;
            }
        }
        ROOT_NAMES
            .iter()
            .find_map(|root| rest.strip_prefix(root).and_then(|r| r.strip_prefix('/')))
            .unwrap_or(rest)
    };
    let rest = rest.trim_start_matches('/');
    if rest.is_empty() {
        return None;
    }
    Some(rest.to_owned())
}

/// `/…`（UNC の `//host/…` を含む）か `X:/…`
fn is_absolute(s: &str) -> bool {
    if s.starts_with('/') {
        return true;
    }
    let mut chars = s.chars();
    matches!(
        (chars.next(), chars.next(), chars.next()),
        (Some(c), Some(':'), Some('/')) if c.is_ascii_alphabetic()
    )
}

/// 拡張子を除いた key。最後の要素に `.` が無ければそのまま
pub fn stem_key(key: &str) -> &str {
    let name_start = key.rfind('/').map(|i| i + 1).unwrap_or(0);
    match key[name_start..].rfind('.') {
        Some(dot) if dot > 0 => &key[..name_start + dot],
        _ => key,
    }
}

/// 解決の候補（`tracks` の 1 行）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub track_id: i64,
    pub rel_path_key: String,
    /// `missing_since IS NULL`
    pub active: bool,
}

/// 全トラックの key を引ける形にしたもの。取り込み 1 回分で作る
#[derive(Debug, Default)]
pub struct Resolver {
    exact: HashMap<String, i64>,
    stem: HashMap<String, Vec<(i64, bool)>>,
}

impl Resolver {
    pub fn new(rows: impl IntoIterator<Item = Candidate>) -> Self {
        let mut r = Resolver::default();
        for c in rows {
            r.stem
                .entry(stem_key(&c.rel_path_key).to_owned())
                .or_default()
                .push((c.track_id, c.active));
            r.exact.insert(c.rel_path_key, c.track_id);
        }
        for v in r.stem.values_mut() {
            // active 優先、同点は id 昇順
            v.sort_by_key(|(id, active)| (!*active, *id));
        }
        r
    }

    /// 行をトラック id へ解決する
    pub fn resolve(&self, line: &str) -> Option<i64> {
        let rel = normalize_entry(line)?;
        let key = canonical_key(&rel);
        if let Some(id) = self.exact.get(&key) {
            return Some(*id);
        }
        // 最上位（active があれば active）の候補が 1 つのときだけ当てる。複数なら曖昧
        let v = self.stem.get(stem_key(&key))?;
        let (id, active) = *v.first()?;
        let same_tier = v.iter().filter(|(_, a)| *a == active).count();
        (same_tier == 1).then_some(id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Resolved {
    /// 解決できたトラック（行の順、同じトラックは最初の 1 回だけ）
    pub track_ids: Vec<i64>,
    /// 解決できなかった行（原文）
    pub unresolved: Vec<String>,
    /// 同じトラックに解決して落とした行数
    pub duplicates: usize,
}

pub fn resolve_entries(r: &Resolver, entries: impl IntoIterator<Item = String>) -> Resolved {
    let mut out = Resolved::default();
    let mut seen = std::collections::HashSet::new();
    for line in entries {
        match r.resolve(&line) {
            Some(id) if seen.insert(id) => out.track_ids.push(id),
            Some(_) => out.duplicates += 1,
            None => out.unresolved.push(line),
        }
    }
    out
}

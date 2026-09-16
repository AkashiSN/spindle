//! selection の 2 形とプレビュー時のスナップショット（SPEC §9、§12.2、D-33、D-39）。
//!
//! - `{"ids": [...]}`: 明示的な id 列挙
//! - `{"filter": "<JSON 文字列>", "exclude_ids": [...]}`: 選択した時点のフィルタ式 + 除外。
//!   Ctrl+A はこちら（6 万件の id を HTTP で運ばない）
//!
//! preview はサーバで selection を解決し、対象行の版と事前条件を [`SelectionStore`] に
//! 固定して `selection_token` を返す。apply は token の集合だけを対象にするので、preview の
//! 後にスキャンで増えた行は対象にならない（D-33）。store はプロセス内メモリで、再起動で
//! 消える（クライアントは 409 `preview_stale` を受けて preview からやり直す）

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use serde::{Deserialize, Serialize};

use super::filter::{Filter, FilterError};

const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// `selection_token` の有効期間（D-33）
pub const TOKEN_TTL: Duration = Duration::from_secs(15 * 60);
/// 同時に保持するスナップショットの上限。超えたら最古を落とす（単一ユーザなので小さくてよい）
pub const MAX_SNAPSHOTS: usize = 16;

/// HTTP ボディの形（`filter` は JSON 文字列のまま受ける）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SelectionBody {
    Ids {
        ids: Vec<i64>,
    },
    Filter {
        filter: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude_ids: Vec<i64>,
    },
}

/// 解決前の selection（フィルタは検証済み）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    Ids(Vec<i64>),
    Filter {
        filter: Filter,
        exclude_ids: Vec<i64>,
    },
}

impl SelectionBody {
    pub fn parse(self) -> Result<Selection, FilterError> {
        Ok(match self {
            SelectionBody::Ids { ids } => Selection::Ids(ids),
            SelectionBody::Filter {
                filter,
                exclude_ids,
            } => Selection::Filter {
                filter: Filter::parse(&filter)?,
                exclude_ids,
            },
        })
    }
}

/// スナップショットの 1 行。apply 時の conflict 判定（`tag_version`）と
/// op の事前条件（SPEC §7.5）に使う
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRow {
    pub id: i64,
    pub tag_version: i64,
    pub audio_version: i64,
    pub dev: Option<i64>,
    pub inode: Option<i64>,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub tag_hash: Option<Vec<u8>>,
    pub rel_path: String,
}

/// preview が固定した集合
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub rows: Vec<SnapshotRow>,
    /// preview 時の ops（JSON）。apply の ops と一致しなければ `preview_stale`（SPEC §9）
    pub ops: serde_json::Value,
}

struct Entry {
    snapshot: Snapshot,
    expires_at: Instant,
    /// apply が処理中（claim 済み）。二重 apply を防ぎ、409 なら release で戻す
    claimed: bool,
}

/// token → スナップショット。TTL 15 分、上限 `MAX_SNAPSHOTS`
pub struct SelectionStore {
    entries: Mutex<HashMap<String, Entry>>,
    ttl: Duration,
}

impl Default for SelectionStore {
    fn default() -> Self {
        Self::new(TOKEN_TTL)
    }
}

impl SelectionStore {
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// スナップショットを登録して token を返す。OS の乱数源が読めなければエラー
    /// （推測可能な token を発行しない）
    pub fn insert(&self, snapshot: Snapshot) -> Result<String, TokenError> {
        let token = new_token()?;
        let now = Instant::now();
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, e| e.expires_at > now);
        while map.len() >= MAX_SNAPSHOTS {
            let oldest = map
                .iter()
                .min_by_key(|(_, e)| e.expires_at)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    map.remove(&k);
                }
                None => break,
            }
        }
        map.insert(
            token.clone(),
            Entry {
                snapshot,
                expires_at: now + self.ttl,
                claimed: false,
            },
        );
        Ok(token)
    }

    /// apply のために token を**占有**する（消さない）。既に占有中・期限切れ・不明なら None。
    /// 成功（201）なら [`finish`](Self::finish) で消し、409 なら [`release`](Self::release) で
    /// 戻す（SPEC §9「409 では token を消費しない」と「並行 apply は 1 つだけ通る」の両立）
    pub fn claim(&self, token: &str) -> Option<Snapshot> {
        let now = Instant::now();
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let e = map.get_mut(token)?;
        if e.expires_at <= now || e.claimed {
            return None;
        }
        e.claimed = true;
        Some(e.snapshot.clone())
    }

    /// claim を解く（409 でやり直せるようにする）
    pub fn release(&self, token: &str) {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = map.get_mut(token) {
            e.claimed = false;
        }
    }

    /// 消費する（201 の後）
    pub fn finish(&self, token: &str) {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(token);
    }

    /// 期限内なら参照する（消さない。件数表示や preview の再表示に使う）
    pub fn get(&self, token: &str) -> Option<Snapshot> {
        let now = Instant::now();
        let map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        map.get(token)
            .filter(|e| e.expires_at > now)
            .map(|e| e.snapshot.clone())
    }

    /// 期限内なら**取り出して消す**。apply はこれで token を原子的に消費する
    /// （並行する 2 つの apply が同じ token で両方通ることはなく、期限切れは None）
    pub fn take(&self, token: &str) -> Option<Snapshot> {
        let now = Instant::now();
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        match map.remove(token) {
            Some(e) if e.expires_at > now => Some(e.snapshot),
            _ => None,
        }
    }

    pub fn len(&self) -> usize {
        let now = Instant::now();
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|e| e.expires_at > now)
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, thiserror::Error)]
#[error("selection_token の乱数を取得できない: {0}")]
pub struct TokenError(String);

/// 推測不能なランダム token（32 バイト、D-39）。乱数源が読めなければ発行しない
fn new_token() -> Result<String, TokenError> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| TokenError(e.to_string()))?;
    Ok(BASE64.encode(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64) -> SnapshotRow {
        SnapshotRow {
            id,
            tag_version: 1,
            audio_version: 1,
            dev: None,
            inode: None,
            size: 0,
            mtime_ns: 0,
            ctime_ns: 0,
            tag_hash: None,
            rel_path: format!("{id}.flac"),
        }
    }

    #[test]
    fn body_parses_both_forms() {
        let b: SelectionBody = serde_json::from_str(r#"{"ids":[3,1]}"#).unwrap();
        assert_eq!(b.parse().unwrap(), Selection::Ids(vec![3, 1]));
        let b: SelectionBody =
            serde_json::from_str(r#"{"filter":"{\"flags\":[\"missing\"]}","exclude_ids":[7]}"#)
                .unwrap();
        match b.parse().unwrap() {
            Selection::Filter {
                filter,
                exclude_ids,
            } => {
                assert_eq!(filter.flags, vec![super::super::filter::Flag::Missing]);
                assert_eq!(exclude_ids, vec![7]);
            }
            other => panic!("{other:?}"),
        }
        let b: SelectionBody = serde_json::from_str(r#"{"filter":"{\"nope\":1}"}"#).unwrap();
        assert!(b.parse().is_err(), "フィルタ文字列の未知キーは拒否");
        assert!(serde_json::from_str::<SelectionBody>(r#"{"exclude_ids":[1]}"#).is_err());
    }

    #[test]
    fn store_returns_snapshot_until_ttl_and_is_bounded() {
        let store = SelectionStore::new(Duration::from_millis(50));
        let snap = Snapshot {
            rows: vec![row(1), row(2)],
            ops: serde_json::json!([]),
        };
        let token = store.insert(snap.clone()).unwrap();
        assert_eq!(store.get(&token), Some(snap.clone()));
        assert_eq!(store.get("nope"), None);
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(store.get(&token), None, "TTL 経過で消える");
        assert_eq!(store.take(&token), None, "take も期限切れを返さない");

        let store = SelectionStore::default();
        let mut tokens = Vec::new();
        for _ in 0..(MAX_SNAPSHOTS + 3) {
            tokens.push(store.insert(snap.clone()).unwrap());
        }
        assert_eq!(store.len(), MAX_SNAPSHOTS);
        assert!(store.get(&tokens[0]).is_none(), "最古が追い出される");
        let last = tokens.last().unwrap();
        assert!(store.get(last).is_some());
        assert_eq!(
            store.take(last),
            Some(snap.clone()),
            "1 回目の take は取れる"
        );
        assert_eq!(store.take(last), None, "同じ token は二度消費できない");
        assert!(store.get(last).is_none());
    }

    #[test]
    fn concurrent_takes_of_one_token_succeed_exactly_once() {
        let store = std::sync::Arc::new(SelectionStore::default());
        let snap = Snapshot {
            rows: vec![row(1)],
            ops: serde_json::json!([]),
        };
        let token = store.insert(snap).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                let token = token.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.take(&token).is_some()
                })
            })
            .collect();
        let wins = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(wins, 1);
    }

    #[test]
    fn claim_is_exclusive_and_release_returns_the_token() {
        let store = SelectionStore::default();
        let snap = Snapshot {
            rows: vec![row(1)],
            ops: serde_json::json!([]),
        };
        let token = store.insert(snap.clone()).unwrap();
        // claim 中は二重に claim できない（並行 apply は 1 つだけ通る）
        assert_eq!(store.claim(&token), Some(snap.clone()));
        assert_eq!(store.claim(&token), None);
        assert!(store.get(&token).is_some(), "claim 中も参照はできる");
        // 409 で release すると同じ token でやり直せる
        store.release(&token);
        assert_eq!(store.claim(&token), Some(snap.clone()));
        // 201 で finish すると消える
        store.finish(&token);
        assert_eq!(store.claim(&token), None);
        assert!(store.get(&token).is_none());
        store.release(&token); // 消えた後の release は何もしない
        assert!(store.get(&token).is_none());
    }

    #[test]
    fn concurrent_claims_of_one_token_succeed_exactly_once() {
        let store = std::sync::Arc::new(SelectionStore::default());
        let token = store
            .insert(Snapshot {
                rows: vec![row(1)],
                ops: serde_json::json!([]),
            })
            .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                let token = token.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.claim(&token).is_some()
                })
            })
            .collect();
        let wins = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(wins, 1);
    }

    #[test]
    fn tokens_are_unique_and_url_safe() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert_ne!(a, b);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}

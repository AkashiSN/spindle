//! エージェントが送る同期の進捗（メモリだけ。DB に書かない。仕様「サーバへの送信」）。
//! 再起動で消えてよい（次の送信で戻る）。端末あたり 1 件で、計画が閉じたら消す

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use agent_proto::{ProgressPhase, ProgressRequest};
use serde::Serialize;

/// 端末 id → 最後に受けた進捗
#[derive(Debug, Default)]
pub struct ProgressStore(Mutex<HashMap<i64, StoredProgress>>);

#[derive(Debug, Clone)]
pub struct StoredProgress {
    pub plan_id: i64,
    pub req: ProgressRequest,
    pub updated_at: i64,
}

/// 端末の一覧と SSE に載せる形。`age` は受けてからの秒数（時計のずれに依らず UI が古さを出せる）
#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct SyncProgressView {
    pub plan_id: i64,
    pub done: u64,
    pub total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub rate: u64,
    pub phase: ProgressPhase,
    pub current: Option<String>,
    pub updated_at: i64,
    pub age: i64,
}

impl ProgressStore {
    fn lock(&self) -> MutexGuard<'_, HashMap<i64, StoredProgress>> {
        // poison は中身が壊れたわけではない（単純な挿入・削除だけ）ので回復して使う
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn put(&self, device_id: i64, plan_id: i64, req: ProgressRequest, now: i64) {
        self.lock().insert(
            device_id,
            StoredProgress {
                plan_id,
                req,
                updated_at: now,
            },
        );
    }

    /// 消したら true
    pub fn clear(&self, device_id: i64) -> bool {
        self.lock().remove(&device_id).is_some()
    }

    /// 保持しているのが `plan_id` の進捗のときだけ消す（別の計画の進捗は残す）。消したら true
    pub fn clear_plan(&self, device_id: i64, plan_id: i64) -> bool {
        let mut m = self.lock();
        if m.get(&device_id).is_some_and(|s| s.plan_id == plan_id) {
            m.remove(&device_id);
            true
        } else {
            false
        }
    }

    /// 受理後の確かめ直し。今の open な計画（`open_plan`）が `plan_id` でなければ、`plan_id` の進捗だけを消す。
    /// 消したら true（null を流すのはこのときだけ。新しい計画の進捗が入っていれば消さず、流さない）
    pub fn clear_if_closed(&self, device_id: i64, plan_id: i64, open_plan: Option<i64>) -> bool {
        open_plan != Some(plan_id) && self.clear_plan(device_id, plan_id)
    }

    pub fn view(&self, device_id: i64, now: i64) -> Option<SyncProgressView> {
        self.lock().get(&device_id).map(|s| view_of(s, now))
    }
}

fn view_of(s: &StoredProgress, now: i64) -> SyncProgressView {
    SyncProgressView {
        plan_id: s.plan_id,
        done: s.req.done,
        total: s.req.total,
        bytes_done: s.req.bytes_done,
        bytes_total: s.req.bytes_total,
        rate: s.req.rate,
        phase: s.req.phase,
        current: s.req.current.clone(),
        updated_at: s.updated_at,
        age: (now - s.updated_at).max(0),
    }
}

/// SSE `device_progress` の本文。`progress` が null なら表示を消す
#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct DeviceProgressEvent {
    pub device_id: i64,
    pub progress: Option<SyncProgressView>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> ProgressRequest {
        ProgressRequest {
            generation: 1,
            done: 2,
            total: 5,
            bytes_done: 10,
            bytes_total: 50,
            rate: 3,
            phase: ProgressPhase::Fetch,
            current: Some("a".into()),
        }
    }

    #[test]
    fn put_view_clear() {
        let s = ProgressStore::default();
        assert!(s.view(1, 100).is_none());
        s.put(1, 7, req(), 100);
        let v = s.view(1, 105).expect("あるはず");
        assert_eq!((v.plan_id, v.done, v.age), (7, 2, 5));
        assert_eq!(s.view(1, 90).map(|v| v.age), Some(0));
        assert!(!s.clear_plan(1, 8));
        assert!(s.clear_plan(1, 7));
        s.put(1, 7, req(), 100);
        assert!(s.clear(1));
        assert!(!s.clear(1));
    }

    /// 確かめ直しで古い計画が閉じていても、新しい計画の進捗は消さない（null を流さない）
    #[test]
    fn clear_if_closed_keeps_newer_plan() {
        let s = ProgressStore::default();
        // 計画 7 がまだ open なら消さない
        s.put(1, 7, req(), 100);
        assert!(!s.clear_if_closed(1, 7, Some(7)));
        assert!(s.view(1, 100).is_some());
        // 計画 7 が閉じた。保持しているのが 7 なら消す
        assert!(s.clear_if_closed(1, 7, None));
        assert!(s.view(1, 100).is_none());
        // 新しい計画 8 の進捗が既に入っている。古い計画 7 の確かめ直しでは消さない
        s.put(1, 8, req(), 100);
        assert!(!s.clear_if_closed(1, 7, Some(8)));
        assert!(!s.clear_if_closed(1, 7, None));
        assert_eq!(s.view(1, 100).map(|v| v.plan_id), Some(8));
    }
}

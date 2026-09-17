-- ジョブ間の名前付き排他（P1-11、D-56）。scan と gc は同じ `library` を取り、取れた側だけが走る
-- （check-then-requeue を両側に置くだけでは同時 claim で譲り合いが続く）。track_locks と同じく
-- プロセス生存中しか意味を持たず、持ち主のジョブが running でなくなれば無効。起動時リカバリで全件消す
CREATE TABLE job_mutexes (
  name        TEXT PRIMARY KEY,
  job_id      INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  acquired_at INTEGER NOT NULL
) STRICT;

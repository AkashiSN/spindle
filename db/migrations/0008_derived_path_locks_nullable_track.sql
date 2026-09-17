-- derived_path_locks.track_id を NULL 可にする（P1-11、D-56）。
-- GC が Derived の孤児（どのトラックにも紐づかない実体）を消す間、transcode と同じ排他予約を
-- 持てるようにする。予約はプロセス生存中しか意味を持たず起動時リカバリで全件消すので、
-- 作り直しで失うものは無い
CREATE TABLE derived_path_locks_new (
  rel_path_key TEXT PRIMARY KEY,
  track_id     INTEGER REFERENCES tracks(id) ON DELETE CASCADE,   -- NULL = GC の予約
  job_id       INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  acquired_at  INTEGER NOT NULL
) STRICT;
INSERT INTO derived_path_locks_new SELECT rel_path_key, track_id, job_id, acquired_at FROM derived_path_locks;
DROP TABLE derived_path_locks;
ALTER TABLE derived_path_locks_new RENAME TO derived_path_locks;

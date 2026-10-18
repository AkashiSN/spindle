-- 再判定（D-114）。jobs は CHECK の列挙に inbox_rejudge を足すため作り直す
-- （foreign_keys=OFF で適用。migrations.rs の FOREIGN_KEYS_OFF）

CREATE TABLE jobs_new (
  id          INTEGER PRIMARY KEY,
  type        TEXT NOT NULL
      CHECK (type IN ('scan','rip','verify','rg','transcode','tagwrite','rename',
                      'normalize','thumbnail','flaccheck','inbox','ytdl','gc','backup','hirescheck',
                      'playlist_sync','source_hash','device_scan','device_sync','device_verify',
                      'rgwrite','inbox_rejudge')),
  dedup_key   TEXT,
  payload     TEXT NOT NULL CHECK (json_valid(payload)),
  state       TEXT NOT NULL DEFAULT 'queued'
      CHECK (state IN ('queued','running','done','failed','cancelled')),
  edit_batch_id INTEGER REFERENCES edit_batches(id) ON DELETE SET NULL,
  priority    INTEGER NOT NULL DEFAULT 0,
  run_after   INTEGER,
  progress    REAL CHECK (progress IS NULL OR (progress >= 0.0 AND progress <= 1.0)),
  total       INTEGER CHECK (total IS NULL OR total >= 0),
  done        INTEGER CHECK (done IS NULL OR done >= 0),
  attempts    INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  max_attempts INTEGER NOT NULL DEFAULT 5 CHECK (max_attempts >= 1),
  last_error  TEXT,
  cancel_requested_at INTEGER,
  created_at  INTEGER NOT NULL,
  started_at  INTEGER,
  finished_at INTEGER,
  note        TEXT
) STRICT;

INSERT INTO jobs_new SELECT * FROM jobs;
DROP TABLE jobs;
ALTER TABLE jobs_new RENAME TO jobs;

CREATE UNIQUE INDEX idx_jobs_dedup_active ON jobs(dedup_key)
  WHERE dedup_key IS NOT NULL AND state IN ('queued','running');
CREATE INDEX idx_jobs_queue ON jobs(state, run_after, priority DESC, created_at)
  WHERE state IN ('queued','running');
CREATE INDEX idx_jobs_finished ON jobs(finished_at) WHERE finished_at IS NOT NULL;
CREATE INDEX idx_jobs_batch ON jobs(edit_batch_id) WHERE edit_batch_id IS NOT NULL;

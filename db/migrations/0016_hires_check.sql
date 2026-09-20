-- 偽ハイレゾ検出の結果（P3-5、D-71、SPEC §7.10）。hirescheck ジョブが書く。
-- hires_check_version は検査時の audio_version で、現在値と違えば結果は古い（スキャナは触らない）。
-- 計測値（カットオフ周波数 / 崖 / 実効ビット）は判定の根拠として残す。計測しなかった側は NULL
ALTER TABLE tracks ADD COLUMN hires_check TEXT
  CHECK (hires_check IS NULL OR hires_check IN ('ok','upsampled','padded','both','inconclusive','decode_error'));
ALTER TABLE tracks ADD COLUMN hires_checked_at INTEGER;
ALTER TABLE tracks ADD COLUMN hires_check_version INTEGER;
ALTER TABLE tracks ADD COLUMN hires_check_error TEXT;
ALTER TABLE tracks ADD COLUMN hires_cutoff_hz INTEGER;
ALTER TABLE tracks ADD COLUMN hires_cliff_db REAL;
ALTER TABLE tracks ADD COLUMN hires_effective_bits INTEGER;

-- jobs.type に 'hirescheck' を加える。SQLite は CHECK を ALTER できないので表を作り直す
-- （0015 と同じ手順。runner が foreign_keys=OFF で適用する。src/db/migrations.rs の FOREIGN_KEYS_OFF。
-- RENAME は legacy_alter_table=OFF なので子表の参照先の名前は書き換わらず、新しい jobs を指したままになる）

CREATE TABLE jobs_new (
  id          INTEGER PRIMARY KEY,
  type        TEXT NOT NULL
      CHECK (type IN ('scan','rip','verify','rg','transcode','tagwrite','rename',
                      'normalize','thumbnail','flaccheck','inbox','ytdl','gc','backup','hirescheck')),
  dedup_key   TEXT,                            -- 二重投入防止。type を含めて構成する（例 'tagwrite:123:7'）
  payload     TEXT NOT NULL CHECK (json_valid(payload)),
  state       TEXT NOT NULL DEFAULT 'queued'
      CHECK (state IN ('queued','running','done','failed','cancelled')),
  edit_batch_id INTEGER REFERENCES edit_batches(id) ON DELETE SET NULL,
  priority    INTEGER NOT NULL DEFAULT 0,
  run_after   INTEGER,                         -- 指数バックオフの次回実行時刻。NULL なら即時
  progress    REAL CHECK (progress IS NULL OR (progress >= 0.0 AND progress <= 1.0)),
  total       INTEGER CHECK (total IS NULL OR total >= 0),
  done        INTEGER CHECK (done IS NULL OR done >= 0),
  attempts    INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  max_attempts INTEGER NOT NULL DEFAULT 5 CHECK (max_attempts >= 1),
  last_error  TEXT,
  cancel_requested_at INTEGER,                 -- ハンドラが進捗更新のたびに見て自発的に止まる
  created_at  INTEGER NOT NULL,
  started_at  INTEGER,
  finished_at INTEGER
) STRICT;

INSERT INTO jobs_new (id, type, dedup_key, payload, state, edit_batch_id, priority, run_after,
                      progress, total, done, attempts, max_attempts, last_error,
                      cancel_requested_at, created_at, started_at, finished_at)
  SELECT id, type, dedup_key, payload, state, edit_batch_id, priority, run_after,
         progress, total, done, attempts, max_attempts, last_error,
         cancel_requested_at, created_at, started_at, finished_at
  FROM jobs;

DROP TABLE jobs;
ALTER TABLE jobs_new RENAME TO jobs;

-- 索引は表と一緒に消えるので作り直す（0001 と同じ定義）
CREATE UNIQUE INDEX idx_jobs_dedup_active ON jobs(dedup_key)
  WHERE dedup_key IS NOT NULL AND state IN ('queued','running');
CREATE INDEX idx_jobs_queue ON jobs(state, run_after, priority DESC, created_at)
  WHERE state IN ('queued','running');
CREATE INDEX idx_jobs_finished ON jobs(finished_at) WHERE finished_at IS NOT NULL;
CREATE INDEX idx_jobs_batch ON jobs(edit_batch_id) WHERE edit_batch_id IS NOT NULL;

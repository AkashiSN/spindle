-- 端末への配信（docs/superpowers/specs/2026-09-29-device-delivery-design.md ②、D-95）。
-- jobs は CHECK の列挙を広げるため作り直す（foreign_keys=OFF で適用。migrations.rs の FOREIGN_KEYS_OFF）

CREATE TABLE jobs_new (
  id          INTEGER PRIMARY KEY,
  type        TEXT NOT NULL
      CHECK (type IN ('scan','rip','verify','rg','transcode','tagwrite','rename',
                      'normalize','thumbnail','flaccheck','inbox','ytdl','gc','backup','hirescheck',
                      'playlist_sync','source_hash','device_scan','device_sync','device_verify')),
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

CREATE TABLE devices (
  id           INTEGER PRIMARY KEY,
  uuid         TEXT NOT NULL UNIQUE,              -- 128 bit 乱数（16 進 32 桁）。再利用しない
  name         TEXT NOT NULL,
  name_key     TEXT NOT NULL UNIQUE,              -- canonical_key(name)
  transport    TEXT NOT NULL CHECK (transport IN ('adb', 'agent')),
  variant      TEXT NOT NULL CHECK (variant IN ('opus', 'aac')),
  selection    TEXT NOT NULL CHECK (selection IN ('all', 'playlists')),
  generation   INTEGER NOT NULL DEFAULT 1,        -- 設定を変えるたびに ++（ジョブと報告の CAS）
  adb_serial   TEXT,
  adb_volume   TEXT,                              -- 'emulated' または SD の UUID
  adb_root     TEXT,                              -- ボリューム内の相対パス
  agent_selector    TEXT UNIQUE,
  agent_secret_hash TEXT,
  pair_selector     TEXT UNIQUE,
  pair_code_hash    TEXT,
  pair_code_expires INTEGER,
  pair_code_attempts INTEGER NOT NULL DEFAULT 0,
  last_synced_at INTEGER,
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL,
  CHECK (transport <> 'adb' OR (adb_serial IS NOT NULL AND adb_volume IS NOT NULL AND adb_root IS NOT NULL))
) STRICT;

CREATE UNIQUE INDEX idx_devices_adb_serial ON devices(adb_serial) WHERE adb_serial IS NOT NULL;

CREATE TABLE device_playlists (
  device_id   INTEGER NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
  playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
  PRIMARY KEY (device_id, playlist_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE device_items (
  device_id   INTEGER NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
  track_id    INTEGER NOT NULL,                  -- GC で tracks が消えても削除の差分を出せるよう FK にしない
  dest_path   TEXT NOT NULL,
  dest_path_key TEXT NOT NULL,
  token       TEXT NOT NULL,                     -- 配信トークン
  size        INTEGER NOT NULL,
  sha256      TEXT NOT NULL,
  synced_at   INTEGER NOT NULL,
  PRIMARY KEY (device_id, track_id),
  UNIQUE (device_id, dest_path_key)
) STRICT;

CREATE TABLE device_playlist_state (
  device_id   INTEGER NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
  playlist_id INTEGER NOT NULL,
  dest_path   TEXT NOT NULL,
  token       TEXT NOT NULL,
  synced_at   INTEGER NOT NULL,
  PRIMARY KEY (device_id, playlist_id)
) STRICT;

CREATE TABLE device_errors (
  device_id   INTEGER NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
  kind        TEXT NOT NULL CHECK (kind IN ('track', 'playlist')),
  ref_id      INTEGER NOT NULL,
  reason      TEXT NOT NULL,
  reported_at INTEGER NOT NULL,
  PRIMARY KEY (device_id, kind, ref_id)
) STRICT;

CREATE TABLE source_hashes (
  track_id    INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  source      TEXT NOT NULL CHECK (source IN ('master', 'opus', 'aac')),
  token       TEXT NOT NULL,                     -- ハッシュを取ったときの意味トークン
  inode       INTEGER NOT NULL,
  size        INTEGER NOT NULL,
  mtime_ns    INTEGER NOT NULL,
  ctime_ns    INTEGER NOT NULL,
  sha256      TEXT NOT NULL,
  computed_at INTEGER NOT NULL,
  PRIMARY KEY (track_id, source)
) STRICT;

CREATE TABLE device_sync_plans (
  id          INTEGER PRIMARY KEY,
  device_id   INTEGER NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
  job_id      INTEGER REFERENCES jobs(id) ON DELETE SET NULL,
  plan_token  TEXT NOT NULL,
  plan        TEXT NOT NULL CHECK (json_valid(plan)),
  state       TEXT NOT NULL CHECK (state IN ('open', 'completed', 'abandoned')),
  report_digest TEXT,
  created_at  INTEGER NOT NULL,
  closed_at   INTEGER
) STRICT;

CREATE UNIQUE INDEX idx_device_sync_plans_open ON device_sync_plans(device_id) WHERE state = 'open';

ALTER TABLE playlists ADD COLUMN evaluated_at INTEGER;

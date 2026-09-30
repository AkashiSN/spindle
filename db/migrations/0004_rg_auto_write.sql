-- ReplayGain の自動書き込み（D-97）。
-- jobs は CHECK の列挙に rgwrite を足すため作り直す（foreign_keys=OFF で適用。migrations.rs の FOREIGN_KEYS_OFF）

CREATE TABLE jobs_new (
  id          INTEGER PRIMARY KEY,
  type        TEXT NOT NULL
      CHECK (type IN ('scan','rip','verify','rg','transcode','tagwrite','rename',
                      'normalize','thumbnail','flaccheck','inbox','ytdl','gc','backup','hirescheck',
                      'playlist_sync','source_hash','device_scan','device_sync','device_verify',
                      'rgwrite')),
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

-- Derived を「RG が書き込み済み」になるまで待たせるか（`[replaygain].write_tags` を起動時に写す）。
-- 系統ごとの設定と同じく sync_variants が毎回上書きする
ALTER TABLE derived_variants ADD COLUMN rg_write_required INTEGER NOT NULL DEFAULT 1
  CHECK (rg_write_required IN (0, 1));

-- 解析値をタグへ書く必要がある行の印（D-97）。rg の保存・album gain の off で立て、書き込みの編集バッチを
-- 作ったとき（一致済みで rg_written_at だけ立てた行も）に下ろす。巻き戻しでは立てない（書き直さない）。
-- rgwrite はこの印の付いた active な行だけを書く（ジョブの payload に対象を持たないので、album の移動や
-- missing・実行中のジョブへの dedup 合流で書き漏れない）
ALTER TABLE tracks ADD COLUMN rg_write_due INTEGER NOT NULL DEFAULT 0
  CHECK (rg_write_due IN (0, 1));
CREATE INDEX idx_tracks_rg_write_due ON tracks(id) WHERE rg_write_due = 1;

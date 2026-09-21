-- 再生リストの購読と同期（P4-16、D-78、SPEC §7.7「再生リストの購読」）。
--
-- playlist_subscriptions: YouTube の再生リスト 1 本 → Library の album 1 つ（追記先）。
-- - album_id は追記先の同一性（album 全体の移動は id を維持する。D-32）。登録時は NULL で、同期か配置が
--   albumartist / album / category から解決できたときに束ねる（album_id IS NULL のときだけ = CAS）。
--   album が消えれば NULL に戻り（SET NULL）、次の同期で再解決する
-- - target_key は canonical_key(albumartist) || '/' || canonical_key(album)。同じ追記先の購読は 1 つだけ
--   （Inbox の youtube/<albumartist>/<album> = 1 購読 = 1 category にする）。album_id も非 NULL の間は
--   UNIQUE で、束ね同士の競合は DB で片方が失敗する
-- - sync_requested_at は承認の後続・手動要求の latch（時刻は表示用で、比較には使わない）。同期の開始で
--   NULL にし、終了時に立っていれば Requeue、残れば dispatcher が回収する
-- - last_attempted_at は同期の開始時刻（成否を問わない。定期投入の基準）、last_synced_at は成功の終端
-- - last_result は最終同期の結果 JSON（取れない一覧・揃えられない一覧・持ち越し・バッチ id 等）
CREATE TABLE playlist_subscriptions (
  id                INTEGER PRIMARY KEY,
  list_id           TEXT NOT NULL UNIQUE,
  url               TEXT NOT NULL,
  album_id          INTEGER REFERENCES albums(id) ON DELETE SET NULL,
  target_key        TEXT NOT NULL UNIQUE,
  albumartist       TEXT NOT NULL,
  album             TEXT NOT NULL,
  category          TEXT,
  align             INTEGER NOT NULL DEFAULT 1 CHECK (align IN (0, 1)),
  enabled           INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
  max_enqueue       INTEGER NOT NULL DEFAULT 50 CHECK (max_enqueue >= 1),
  created_at        INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL,
  last_attempted_at INTEGER,
  last_synced_at    INTEGER,
  sync_requested_at INTEGER,
  last_result       TEXT CHECK (last_result IS NULL OR json_valid(last_result))
) STRICT;

CREATE UNIQUE INDEX idx_playlist_subscriptions_album ON playlist_subscriptions(album_id)
  WHERE album_id IS NOT NULL;

-- jobs.type に 'playlist_sync' を加える。SQLite は CHECK を ALTER できないので表を作り直す
-- （0015 / 0016 と同じ手順。runner が foreign_keys=OFF で適用する。src/db/migrations.rs の FOREIGN_KEYS_OFF。
-- RENAME は legacy_alter_table=OFF なので子表の参照先の名前は書き換わらず、新しい jobs を指したままになる）。
-- 0020 の note 列を含む

CREATE TABLE jobs_new (
  id          INTEGER PRIMARY KEY,
  type        TEXT NOT NULL
      CHECK (type IN ('scan','rip','verify','rg','transcode','tagwrite','rename',
                      'normalize','thumbnail','flaccheck','inbox','ytdl','gc','backup','hirescheck',
                      'playlist_sync')),
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
  finished_at INTEGER,
  note        TEXT                             -- 完了時の結果 1 行（0020）
) STRICT;

INSERT INTO jobs_new (id, type, dedup_key, payload, state, edit_batch_id, priority, run_after,
                      progress, total, done, attempts, max_attempts, last_error,
                      cancel_requested_at, created_at, started_at, finished_at, note)
  SELECT id, type, dedup_key, payload, state, edit_batch_id, priority, run_after,
         progress, total, done, attempts, max_attempts, last_error,
         cancel_requested_at, created_at, started_at, finished_at, note
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

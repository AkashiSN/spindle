-- edit_ops.kind に 'md5' を加える（P1-5b、SPEC §7.9、D-57 / D-59）。
--
-- FLAC の STREAMINFO に MD5 が無い（全ゼロ）トラックへ、デコードした PCM MD5 を書く編集バッチ。
-- 音声もタグも変わらず audio_version / tag_version は据え置き。edits は key = 'audio_md5'、
-- 値は 32 桁の hex 文字列（全ゼロ = 未設定）。new_value は記録時 null で、反映時に計算値を書く。
--
-- SQLite は CHECK を ALTER できないので表を作り直す。edit_ops は edits（ON DELETE CASCADE）と
-- archived_files（ON DELETE SET NULL）から参照されているため、この版はマイグレーション runner が
-- foreign_keys=OFF で適用する（トランザクション中は PRAGMA が効かないので runner が外で切る。
-- src/db/migrations.rs の FOREIGN_KEYS_OFF）。RENAME は legacy_alter_table=OFF なので、子表の
-- 参照先の名前は書き換わらず、新しい edit_ops を指したままになる

CREATE TABLE edit_ops_new (
  id                INTEGER PRIMARY KEY,
  batch_id          INTEGER NOT NULL REFERENCES edit_batches(id) ON DELETE CASCADE,
  ordinal           INTEGER NOT NULL,
  track_id          INTEGER NOT NULL,
  kind              TEXT NOT NULL CHECK (kind IN ('tags','rename','delete','archive','md5')),
  expected_dev      INTEGER,
  expected_inode    INTEGER,
  expected_size     INTEGER,
  expected_mtime_ns INTEGER,
  expected_ctime_ns INTEGER,
  expected_tag_hash BLOB,
  expected_rel_path TEXT,
  result            TEXT NOT NULL DEFAULT 'pending'
      CHECK (result IN ('pending','applied','skipped_conflict','failed','superseded')),
  error             TEXT,
  job_id            INTEGER REFERENCES jobs(id) ON DELETE SET NULL,
  applied_at        INTEGER,
  UNIQUE (batch_id, ordinal)
) STRICT;

INSERT INTO edit_ops_new (id, batch_id, ordinal, track_id, kind, expected_dev, expected_inode,
                          expected_size, expected_mtime_ns, expected_ctime_ns, expected_tag_hash,
                          expected_rel_path, result, error, job_id, applied_at)
  SELECT id, batch_id, ordinal, track_id, kind, expected_dev, expected_inode,
         expected_size, expected_mtime_ns, expected_ctime_ns, expected_tag_hash,
         expected_rel_path, result, error, job_id, applied_at
  FROM edit_ops;

DROP TABLE edit_ops;
ALTER TABLE edit_ops_new RENAME TO edit_ops;

CREATE INDEX idx_edit_ops_track ON edit_ops(track_id, id DESC);
CREATE UNIQUE INDEX idx_edit_ops_pending ON edit_ops(track_id) WHERE result = 'pending';
CREATE INDEX idx_edit_ops_job ON edit_ops(job_id) WHERE job_id IS NOT NULL;

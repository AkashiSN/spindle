-- archived_files.reason に 'restore' を加える（P1-4、SPEC §7.4、D-46）。
--
-- ロスレス正規化の巻き戻し（restore）では、元ファイルを Archive から Library へ**コピー**で戻し
-- （Archive は追記のみ。台帳は state='restored'）、Library にあった FLAC を Archive へ move する。
-- その FLAC も GC の回収対象なので台帳に載せるが、退避の理由は正規化ではなく巻き戻しなので
-- reason を分ける。SQLite は CHECK を ALTER できないので表を作り直す（archived_files を参照する
-- 表は無い）。

CREATE TABLE archived_files_new (
  id             INTEGER PRIMARY KEY,
  track_id       INTEGER,                      -- FK にしない（トラック削除後も台帳は残す）
  op_id          INTEGER REFERENCES edit_ops(id) ON DELETE SET NULL,
  rel_path       TEXT NOT NULL UNIQUE,         -- Archive/ からの相対パス
  rel_path_key   TEXT NOT NULL UNIQUE,         -- casefold(NFD(rel_path))
  source_rel_path TEXT NOT NULL,               -- 退避前の Library/ 相対パス（履歴値。比較には使わない）
  reason         TEXT NOT NULL CHECK (reason IN ('normalize','restore')),
  archived_at    INTEGER NOT NULL,
  eligible_after INTEGER NOT NULL,             -- archived_at + [gc].retention_days
  state          TEXT NOT NULL DEFAULT 'held'
      CHECK (state IN ('held','restored','deleted')),
  state_at       INTEGER
) STRICT;

INSERT INTO archived_files_new
  (id, track_id, op_id, rel_path, rel_path_key, source_rel_path, reason,
   archived_at, eligible_after, state, state_at)
SELECT id, track_id, op_id, rel_path, rel_path_key, source_rel_path, reason,
       archived_at, eligible_after, state, state_at
FROM archived_files;

DROP TABLE archived_files;
ALTER TABLE archived_files_new RENAME TO archived_files;

CREATE INDEX idx_archived_gc ON archived_files(eligible_after) WHERE state = 'held';

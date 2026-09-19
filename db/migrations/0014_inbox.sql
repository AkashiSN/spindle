-- Inbox の承認キュー（SPEC §7.8、D-68、P2-10）。正は Inbox のファイルで、行はキャッシュ
-- （ディレクトリが消えれば行も消す。placed だけは結果を見せるために一定時間残す）

CREATE TABLE inbox_items (
  id              INTEGER PRIMARY KEY,
  rel_dir         TEXT NOT NULL,            -- Inbox 相対（root 直下の音声は ''）
  rel_dir_key     TEXT NOT NULL UNIQUE,     -- casefold(NFD)
  state           TEXT NOT NULL DEFAULT 'pending'
      CHECK (state IN ('pending','approved','placing','placed','rejected','failed')),
  detected_at     INTEGER NOT NULL,
  seen_at         INTEGER NOT NULL,         -- 最後に走査で見た時刻
  approved_at     INTEGER,
  draft           TEXT,                     -- 承認時の補正（JSON。InboxDraft）
  error           TEXT,                     -- failed の理由、pending に戻した理由
  placed_album_id INTEGER REFERENCES albums(id) ON DELETE SET NULL,
  placed_at       INTEGER
) STRICT;

CREATE INDEX idx_inbox_items_state ON inbox_items(state);

CREATE TABLE inbox_files (
  item_id      INTEGER NOT NULL REFERENCES inbox_items(id) ON DELETE CASCADE,
  rel_path     TEXT NOT NULL,               -- Inbox 相対
  rel_path_key TEXT NOT NULL UNIQUE,
  inode        INTEGER NOT NULL,
  size         INTEGER NOT NULL,
  mtime_ns     INTEGER NOT NULL,
  ctime_ns     INTEGER NOT NULL,
  codec        TEXT NOT NULL,
  lossless     INTEGER NOT NULL CHECK (lossless IN (0,1)),
  sample_rate  INTEGER,
  bit_depth    INTEGER,
  channels     INTEGER,
  duration_ms  INTEGER,
  tags         TEXT NOT NULL,               -- [[key, value], ...] の JSON（表示と下書きに使う）
  PRIMARY KEY (item_id, rel_path_key)
) STRICT, WITHOUT ROWID;

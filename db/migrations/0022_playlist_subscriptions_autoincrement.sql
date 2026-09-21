-- playlist_subscriptions の id を再利用しない（AUTOINCREMENT。P4-16 のレビューで指摘）。
--
-- INTEGER PRIMARY KEY だけだと DELETE 後の INSERT が最大 rowid を再利用する。ytdl / playlist_sync の payload と
-- Inbox の spindle-inbox.json は裸の subscription_id を持ち、DELETE は走行中のジョブを止めないので、削除直後に
-- 別の購読を作ると旧ジョブが新しい購読へ誤帰属する。0021 は書き換えず表を作り直す（sqlite_sequence は
-- 既存の最大 id から続く）
CREATE TABLE playlist_subscriptions_new (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
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

INSERT INTO playlist_subscriptions_new
  SELECT id, list_id, url, album_id, target_key, albumartist, album, category, align, enabled, max_enqueue,
         created_at, updated_at, last_attempted_at, last_synced_at, sync_requested_at, last_result
  FROM playlist_subscriptions;

DROP TABLE playlist_subscriptions;
ALTER TABLE playlist_subscriptions_new RENAME TO playlist_subscriptions;

CREATE UNIQUE INDEX idx_playlist_subscriptions_album ON playlist_subscriptions(album_id)
  WHERE album_id IS NOT NULL;

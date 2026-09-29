-- Inbox の件で配置した曲（可視化 B。承認済みの件に「配置 → RG → 系統 → 端末」の段を出す）。
-- 既存アルバムへ追記した件でも、その件の曲だけを数えるために持つ。件が消えれば一緒に消える
CREATE TABLE inbox_item_tracks (
  item_id  INTEGER NOT NULL REFERENCES inbox_items(id) ON DELETE CASCADE,
  track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  PRIMARY KEY (item_id, track_id)
) STRICT, WITHOUT ROWID;

CREATE INDEX idx_inbox_item_tracks_track ON inbox_item_tracks(track_id);

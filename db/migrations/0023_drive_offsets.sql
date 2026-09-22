-- 学習した読み取りオフセット（P2-5、D-83）。rip は最初オフセット 0 で吸い、CTDB / AccurateRip の照合で
-- 見つかったずれを PCM に当ててから配置し、その値をドライブの型番（INQUIRY の vendor + product）ごとに
-- 覚えて次の盤から使う。キャッシュなので消えても次に照合が通った盤で覚え直す（rip.log にも残る）
CREATE TABLE drive_offsets (
  drive        TEXT PRIMARY KEY,
  offset       INTEGER NOT NULL,
  method       TEXT NOT NULL CHECK (method IN ('ctdb', 'accuraterip')),
  confidence   INTEGER NOT NULL,
  detected_at  INTEGER NOT NULL
);

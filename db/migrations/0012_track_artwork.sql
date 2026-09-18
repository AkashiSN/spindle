-- トラック自身の埋め込み画像（P1-3c、D-61）。
--
-- album のアートワーク（albums.artwork_id。同梱画像 → 最初のトラックの埋め込み。D-49）とは別に、
-- 各トラックが自分の埋め込み画像（front cover 優先）を持つ。トラックごとに画像が違う album で
-- Derived と再生表示がそのトラックの絵になる。スキャナ Phase 3 と tagwrite の読み戻しが埋め、
-- 変更なしの行は読まないので既存行は deep scan で埋まる（NULL の間は album の絵へ倒す）。

ALTER TABLE tracks ADD COLUMN artwork_id INTEGER REFERENCES artwork(id) ON DELETE SET NULL;
CREATE INDEX idx_tracks_artwork ON tracks(artwork_id) WHERE artwork_id IS NOT NULL;

-- 画像をキャッシュへ置けなかった（store の I/O 失敗）行の印。物理属性が変わらなくても次のスキャンが
-- 読み直す（Phase 3 の対象に含める）。画像を記録できたら 0 に戻す
ALTER TABLE tracks ADD COLUMN artwork_dirty INTEGER NOT NULL DEFAULT 0;
CREATE INDEX idx_tracks_artwork_dirty ON tracks(id) WHERE artwork_dirty = 1;

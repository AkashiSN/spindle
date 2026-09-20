-- album gain は album ごとの属性（D-74、P4-5）。既定 off。true の album だけ rg を album 単位で投入し、
-- rg_album_* を持つ。既存の album はすべて off にし、既存の rg_album_* は NULL に揃える。ファイルには
-- 書かず（次の書き込みで album のキーが消える）、rg_written_at を NULL に戻し、rg_scanned_at を 1 進めて
-- Derived の R128_ALBUM_GAIN がタグ上書きで追随するようにする（src_rg_scanned_at の差分。D-51）
ALTER TABLE albums ADD COLUMN album_gain INTEGER NOT NULL DEFAULT 0 CHECK (album_gain IN (0, 1));

UPDATE tracks
   SET rg_album_gain = NULL,
       rg_album_peak = NULL,
       rg_written_at = NULL,
       rg_scanned_at = CASE WHEN rg_scanned_at IS NULL THEN NULL ELSE rg_scanned_at + 1 END
 WHERE rg_album_gain IS NOT NULL OR rg_album_peak IS NOT NULL;

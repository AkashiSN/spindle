-- FLAC 健全性チェックの結果（P1-5、D-57）。flaccheck ジョブが flac -t と STREAMINFO の MD5 から
-- 判定して書く。flac_check_version は検査時の audio_version で、現在値と違えば結果は古い
-- （スキャナはこれらの列を触らない）
ALTER TABLE tracks ADD COLUMN flac_check TEXT
  CHECK (flac_check IS NULL OR flac_check IN ('ok','md5_missing','decode_error'));
ALTER TABLE tracks ADD COLUMN flac_checked_at INTEGER;
ALTER TABLE tracks ADD COLUMN flac_check_version INTEGER;
ALTER TABLE tracks ADD COLUMN flac_check_error TEXT;

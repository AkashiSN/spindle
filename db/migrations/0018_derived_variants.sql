-- Derived を系統（variant）ごとに 1 本にする（P4-7、D-75、SPEC §7.6）。
-- 1. delivery ビューは derived_files を参照するので先に落とす
-- 2. derived_files を (track_id, variant) 主キーで作り直し、既存行は opus 系統として移す
--    （rel_path はルート直下のまま。audio_profile は 128k 時代の値にして、次の transcode が
--    設定の 256k との差分で作り直し opus/ 配下へ置く。設定を 128 のままにすればパスの差分だけで Move）
-- 3. delivery を variant = 'opus' で作り直す（配布ビューは opus 系統に固定）
-- 4. 系統の設定を写す表（config.toml が正。起動時に db::derived::sync_variants が揃える）
DROP VIEW delivery;

CREATE TABLE derived_files_new (
  track_id          INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  variant           TEXT NOT NULL CHECK (variant IN ('opus', 'aac')),
  rel_path          TEXT NOT NULL UNIQUE,      -- Derived/ からの相対パス（<variant>/ 以下）
  rel_path_key      TEXT NOT NULL UNIQUE,
  codec             TEXT NOT NULL,
  bitrate           INTEGER,
  src_audio_version INTEGER NOT NULL CHECK (src_audio_version >= 1),  -- 差分 → 再エンコード
  src_tag_version   INTEGER NOT NULL CHECK (src_tag_version >= 1),    -- 差分のみ → タグ上書き
  generated_at      INTEGER NOT NULL,
  src_artwork_id    INTEGER REFERENCES artwork(id) ON DELETE SET NULL,
  src_rg_scanned_at INTEGER,
  audio_profile     TEXT NOT NULL,             -- 音声に効く設定の世代（差分 → 再エンコード）
  tag_profile       TEXT NOT NULL,             -- タグに効く設定の世代（差分 → タグ上書き）
  PRIMARY KEY (track_id, variant)
) STRICT;

INSERT INTO derived_files_new (track_id, variant, rel_path, rel_path_key, codec, bitrate,
                               src_audio_version, src_tag_version, generated_at, src_artwork_id,
                               src_rg_scanned_at, audio_profile, tag_profile)
SELECT track_id, 'opus', rel_path, rel_path_key, codec, bitrate, src_audio_version, src_tag_version,
       generated_at, src_artwork_id, src_rg_scanned_at,
       'opus:' || COALESCE(bitrate, 128) || ':v1', 'opus:v1'
  FROM derived_files;

DROP TABLE derived_files;
ALTER TABLE derived_files_new RENAME TO derived_files;
CREATE INDEX idx_derived_files_variant ON derived_files(variant);

CREATE VIEW delivery AS
SELECT
  t.id AS track_id,
  CASE WHEN t.lossless = 1 AND d.rel_path IS NOT NULL
            AND d.src_audio_version = t.audio_version
       THEN 'Derived/' || d.rel_path
       ELSE 'Library/' || t.rel_path
  END AS path,
  CASE WHEN t.lossless = 1 AND d.rel_path IS NOT NULL
            AND d.src_audio_version = t.audio_version
       THEN d.codec ELSE t.codec
  END AS codec,
  CASE WHEN t.lossless = 1 AND d.rel_path IS NOT NULL
            AND d.src_audio_version = t.audio_version
            AND d.src_tag_version <> t.tag_version
       THEN 1 ELSE 0
  END AS stale_tags
FROM tracks t
LEFT JOIN derived_files d ON d.track_id = t.id AND d.variant = 'opus'
WHERE t.missing_since IS NULL;

CREATE TABLE derived_variants (
  variant       TEXT PRIMARY KEY CHECK (variant IN ('opus', 'aac')),
  enabled       INTEGER NOT NULL CHECK (enabled IN (0, 1)),
  audio_profile TEXT NOT NULL,
  tag_profile   TEXT NOT NULL,
  codec         TEXT NOT NULL,
  bitrate       INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
) STRICT;

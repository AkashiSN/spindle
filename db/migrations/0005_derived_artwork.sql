-- Derived に埋めた album アートワークと RG 解析値の世代（P1-10、D-51）。
--
-- どちらも tag_version に乗らない（カバーの差し替えはトラックのタグではない。RG の解析値は
-- DB の列で、ファイルへの書き込みは別のバッチ）ので、Derived に書いた時点の値を持って
-- 現在値と比べる。
-- - src_artwork_id: 埋めた画像の artwork.id（NULL = 画像なし）。artwork 行が GC で消えたら
--   NULL に戻り、album 側も SET NULL なので一致し続ける
-- - src_rg_scanned_at: 埋めた R128_* の元になった tracks.rg_scanned_at（NULL = 未解析で書いた）

ALTER TABLE derived_files ADD COLUMN src_artwork_id INTEGER REFERENCES artwork(id) ON DELETE SET NULL;
ALTER TABLE derived_files ADD COLUMN src_rg_scanned_at INTEGER;

-- Derived のパス（canonical key）の排他予約。transcode ジョブが物理的な書き込みの前に取り、
-- 終了時に解放する。x.flac と x.wav のように別の Library パスが同じ Derived パスに写る場合や、
-- 占有を確定してから書くまでの間に別のジョブが同じ宛先へ書くのを防ぐ（track_locks は track 単位
-- なので宛先の衝突は防げない）。プロセス生存中しか意味を持たず、持ち主のジョブが running で
-- なくなれば無効（起動時リカバリで全件消す）
CREATE TABLE derived_path_locks (
  rel_path_key TEXT PRIMARY KEY,
  track_id     INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  job_id       INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  acquired_at  INTEGER NOT NULL
) STRICT;

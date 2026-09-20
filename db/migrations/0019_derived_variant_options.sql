-- aac 系統の設定（SPEC §7.6、D-75、P4-8）。`eligible` が lossy_sources を、transcode ハンドラが
-- multi_value_separator を derived_variants から引く（config.toml が正。起動時に
-- db::derived::sync_variants が両系統を写す）。既存行（opus）は既定値で埋まる
ALTER TABLE derived_variants ADD COLUMN lossy_sources INTEGER NOT NULL DEFAULT 0
  CHECK (lossy_sources IN (0, 1));
ALTER TABLE derived_variants ADD COLUMN multi_value_separator TEXT NOT NULL DEFAULT ' & ';

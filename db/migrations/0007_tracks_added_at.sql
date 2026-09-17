-- トラックの初回登録時刻（P1-7、D-54）。
--
-- スマートプレイリスト DSL の `added` フィールド（docs/DSL.md「初回スキャン日時」）の元になる。
-- seen_at は走査のたびに更新されるので使えない。既存行は seen_at で埋める（正確な初回時刻は
-- 残っていない。リハーサル環境では全行が初回 deep scan で入ったので実質同じ）。
-- 以後はスキャナが INSERT 時に設定する。復活（missing → 再発見）でも変えない

ALTER TABLE tracks ADD COLUMN added_at INTEGER NOT NULL DEFAULT 0;
UPDATE tracks SET added_at = seen_at;

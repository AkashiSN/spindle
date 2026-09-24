-- Inbox の却下した件の破棄（D-90、P4-22）。rejected の件に「削除」で破棄要求の時刻を入れ、
-- GC が [gc].retention_days 経過後にファイルと行を消す。NULL = 破棄待ちでない。状態は rejected のまま
-- （CHECK の作り直しを避ける）で、rejected 以外へ移るとき・走査でファイルが変わったときに NULL へ戻す
ALTER TABLE inbox_items ADD COLUMN discard_requested_at INTEGER;

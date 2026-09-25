-- CD の取り込みの表の画像を Cover Art Archive から一度だけ取る（D-91）。承認画面の提案の初期値に使う。
-- caa_picture: 置いた画像（`<mime>:<sha256hex>`。下書きの picture と同じ形）。NULL = まだ無い
-- caa_tries:   取得を試みた回数。上限（2）に達したら以後は取りに行かない（画像の無い盤・CD でない取り込み
--              は 1 回目で上限にする）
ALTER TABLE inbox_items ADD COLUMN caa_picture TEXT;
ALTER TABLE inbox_items ADD COLUMN caa_tries INTEGER NOT NULL DEFAULT 0;

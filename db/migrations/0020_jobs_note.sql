-- ジョブの完了時の結果 1 行（P4-13、SPEC §8）。ytdl が「Inbox に置いた: <path>」「プラグインが skip: <理由>」
-- 「再生リストを展開した: N 件を投入、M 件は取り込み済み」を書き、YouTube 画面が出す。失敗の理由は
-- 従来どおり last_error。既存行は NULL
ALTER TABLE jobs ADD COLUMN note TEXT;

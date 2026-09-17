-- プレイリスト名の canonical key（P1-6、D-53）。
--
-- 名前はそのまま書き出しファイル名（Playlists/<profile>/<name>.m3u8）になる。ZFS の
-- insensitive + formD では Foo.m3u8 と foo.m3u8、NFC と NFD の同名が同じ実体なので、
-- `name` の BINARY UNIQUE だけでは別プレイリストの書き出しが上書きし合う。rel_path_key と
-- 同じ規則（casefold + NFD。domain::relpath::canonical_key）の name_key を持ち、こちらで一意にする。
--
-- 既存行の backfill は SQL の lower() ではなく、マイグレーション実行時に登録される
-- spindle_canonical_key()（Rust の canonical_key そのもの）で行う。
-- 同じ key の行が既にあるときの改名（id 最小を残し、後続を空いている "<name> (n)" にする。
-- 二次衝突と ".m3u8" 込み 255 バイトの上限を見ながら決めるので SQL では書けない）と
-- UNIQUE INDEX idx_playlists_name_key の作成は、この SQL の直後に同じトランザクションで
-- src/db/migrations.rs の post_sql(6) が行う。

ALTER TABLE playlists ADD COLUMN name_key TEXT NOT NULL DEFAULT '';
UPDATE playlists SET name_key = spindle_canonical_key(name);

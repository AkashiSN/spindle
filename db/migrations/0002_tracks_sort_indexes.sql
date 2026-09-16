-- GET /api/tracks のソート列ごとのキーセット索引（P0-7、D-39）。
--
-- カーソルページングは (ソートキー, id) の行値比較で次ページを引く。ソート列は NULL を
-- 許すが、行値比較は NULL を含むと不定になるので、索引と ORDER BY の両方で
-- coalesce() した式を使い NULL を '' / 0 / -1 に畳む（NULL は昇順で先頭、降順で末尾）。
-- 式索引は ORDER BY / WHERE の式と字面が一致するときだけ使われるため、
-- クエリ側（src/db/tracks.rs）の式はここと完全に同じでなければならない。
--
-- 既存の idx_tracks_sort（albumartist, album_id, disc_no, track_no）は NULL を畳まないので
-- キーセットには使えない。スキャナの album 照合が使う可能性があるので残す。

CREATE INDEX idx_tracks_ks_album ON tracks(
  coalesce(albumartist, ''), coalesce(album_id, 0), coalesce(disc_no, 0), coalesce(track_no, 0), id
);
CREATE INDEX idx_tracks_ks_title       ON tracks(coalesce(title, ''), id);
CREATE INDEX idx_tracks_ks_artist      ON tracks(coalesce(artist_display, ''), id);
CREATE INDEX idx_tracks_ks_album_title ON tracks(coalesce(album, ''), id);
CREATE INDEX idx_tracks_ks_albumartist ON tracks(coalesce(albumartist, ''), id);
CREATE INDEX idx_tracks_ks_date        ON tracks(coalesce(date, ''), id);
CREATE INDEX idx_tracks_ks_duration    ON tracks(coalesce(duration_ms, -1), id);
CREATE INDEX idx_tracks_ks_codec       ON tracks(codec, id);
-- rel_path は UNIQUE なので既存の自動索引で足りる（id のタイブレークは不要）

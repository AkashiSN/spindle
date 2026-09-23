-- albums.discid を落とす（D-67 追記 3）。DiscID は 1 枚ごとの値で、複数枚組の album には 1 つに
-- 収まらない。リリースの同一性は mb_release_id（MUSICBRAINZ_ALBUMID）→ album 行で引き、DiscID は
-- トラックのタグ（MUSICBRAINZ_DISCID）・album_verifications（ディスク単位）・rip.log に残る。
-- 索引の付いた列は落とせないので、先に索引を消す
DROP INDEX idx_albums_discid;
ALTER TABLE albums DROP COLUMN discid;

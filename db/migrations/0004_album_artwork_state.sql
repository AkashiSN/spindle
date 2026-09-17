-- アルバムのアートワーク解決の状態（P1-3、SPEC §7.1「アートワーク」、D-49）。
--
-- album のアートワークは「ディレクトリの同梱カバー画像（cover.jpg 等）があればそれ、無ければ
-- 最初のトラックの埋め込み画像」で決まる。同梱画像はトラックではないので tracks の最速パスでは
-- 変化を拾えない。album 側に同梱画像の stat を持ち、スキャンのたびに inventory と比べて
-- 変わった album だけ解決し直す（トラックの最速パスと同じ規則: inode / size / mtime / ctime）。
-- artwork_resolved_at が NULL の album は次のスキャンで必ず解決する（マイグレーション直後の
-- 既存 album を含む）。

ALTER TABLE albums ADD COLUMN cover_inode         INTEGER;
ALTER TABLE albums ADD COLUMN cover_size          INTEGER;
ALTER TABLE albums ADD COLUMN cover_mtime_ns      INTEGER;
ALTER TABLE albums ADD COLUMN cover_ctime_ns      INTEGER;
ALTER TABLE albums ADD COLUMN artwork_resolved_at INTEGER;

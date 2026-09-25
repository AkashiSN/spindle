-- category が NULL の active な album を引く部分索引（D-92）。スキャンごとに NULL の album を
-- 直下のディレクトリ名で埋めるので、埋まった後は索引が数件だけになり全 album を走査しない
CREATE INDEX idx_albums_category_null ON albums(rel_dir)
  WHERE category_id IS NULL AND missing_since IS NULL;

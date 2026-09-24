-- spindle schema (SQLite 3.38+)
-- 原則: ファイルが正 / パスは識別子でない / 音声とタグを別に版管理 / 破壊的操作は巻き戻せる
--
-- 最初の vX.Y.Z の前に、それまでの 0001〜0024 をこの 1 本に畳んだ（D-88）。以後の変更は
-- 既存ファイルを書き換えず、新しい連番ファイルを足す。
--
-- PRAGMA はこのファイルに書かない。マイグレーションは 1 ファイル = 1 トランザクションで
-- 適用するが、journal_mode / synchronous はトランザクション内で変更できず、
-- foreign_keys はトランザクション中は無視される。これらはコネクション初期化時に
-- アプリ側でトランザクション外から設定する（journal_mode=WAL, synchronous=NORMAL,
-- foreign_keys=ON）。
--
-- 列挙値・真偽値・版番号は CHECK で守る。STRICT は型しか見ない。

CREATE TABLE schema_version (
  version    INTEGER NOT NULL,
  applied_at INTEGER NOT NULL
) STRICT;

-- ============================================================
-- カテゴリ（統制語彙）
-- ============================================================

CREATE TABLE categories (
  id       INTEGER PRIMARY KEY,
  name     TEXT NOT NULL UNIQUE,
  sort_key TEXT
) STRICT;

-- GENRE タグ → category の写像。取り込み時の自動推定に使う。
-- GENRE は多値かつ表記揺れが激しいため、パス決定には category のみを使う。
CREATE TABLE genre_category_map (
  genre       TEXT PRIMARY KEY,
  category_id INTEGER NOT NULL REFERENCES categories(id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

-- ============================================================
-- アートワーク（ハッシュアドレス）
-- ============================================================

CREATE TABLE artwork (
  id       INTEGER PRIMARY KEY,
  sha256   BLOB NOT NULL UNIQUE CHECK (length(sha256) = 32),
  mime     TEXT NOT NULL,
  width    INTEGER,
  height   INTEGER,
  bytes    INTEGER NOT NULL CHECK (bytes >= 0),
  -- 'embedded' = ファイル埋め込み由来 / 'file' = cover.jpg 由来
  origin   TEXT NOT NULL CHECK (origin IN ('embedded','file'))
) STRICT;

-- ============================================================
-- アルバム（= Library 内の 1 ディレクトリ）
-- ============================================================

-- album の同一性は rel_dir ではない（パスは識別子ではない）。ディレクトリが外部で
-- rename されても、構成トラックの id 集合と mb_release_id から同じ行を
-- 引き当てて rel_dir を書き換える（SPEC §7.1）。
-- DiscID は 1 枚ごとの値で複数枚組の album には 1 つに収まらないので album には持たない
-- （トラックのタグ MUSICBRAINZ_DISCID・album_verifications・rip.log に残る。D-67）。
--
-- アートワークは「ディレクトリの同梱カバー画像（cover.jpg 等）があればそれ、無ければ最初の
-- トラックの埋め込み画像」で決まる（SPEC §7.1「アートワーク」、D-49）。同梱画像はトラックでは
-- ないので tracks の最速パスでは変化を拾えない。cover_* に同梱画像の stat を持ち、スキャンの
-- たびに inventory と比べて変わった album だけ解決し直す（トラックの最速パスと同じ規則:
-- inode / size / mtime / ctime）。artwork_resolved_at が NULL の album は次のスキャンで必ず解決する。
CREATE TABLE albums (
  id            INTEGER PRIMARY KEY,
  rel_dir       TEXT NOT NULL UNIQUE,          -- Library/ からの相対ディレクトリ（表示用）
  rel_dir_key   TEXT NOT NULL UNIQUE,          -- casefold(NFD(rel_dir))。ZFS insensitive+formD と同じ同値関係
  category_id   INTEGER REFERENCES categories(id) ON DELETE SET NULL,
  albumartist   TEXT,
  album         TEXT,
  date          TEXT,                          -- パスには出さないが必ず保持
  original_date TEXT,
  edition       TEXT,                          -- Remaster 等。衝突回避に使用
  mb_release_id TEXT,
  disc_count    INTEGER,
  artwork_id    INTEGER REFERENCES artwork(id) ON DELETE SET NULL,
  missing_since INTEGER,                       -- 構成トラックが 0 になった album。行は消さない
                                               -- （verifications が CASCADE で消えるため）。GC が回収
  cover_inode         INTEGER,
  cover_size          INTEGER,
  cover_mtime_ns      INTEGER,
  cover_ctime_ns      INTEGER,
  artwork_resolved_at INTEGER,
  -- album gain は album ごとの属性（D-74）。既定 off。1 の album だけ rg を album 単位で投入し、
  -- tracks.rg_album_* を持つ
  album_gain INTEGER NOT NULL DEFAULT 0 CHECK (album_gain IN (0, 1))
) STRICT;

CREATE INDEX idx_albums_artist   ON albums(albumartist, album);
CREATE INDEX idx_albums_release  ON albums(mb_release_id) WHERE mb_release_id IS NOT NULL;
CREATE INDEX idx_albums_missing  ON albums(missing_since) WHERE missing_since IS NOT NULL;

-- ============================================================
-- 走査の実行単位
-- ============================================================

-- 1 回の走査 = 1 行。tracks.seen_run_id の claim 判定に使う（seen_at は時刻のまま）。
-- missing_since を立てるのは state='completed' の finalize だけ（SPEC §7.1）
CREATE TABLE scan_runs (
  id          INTEGER PRIMARY KEY,
  kind        TEXT NOT NULL CHECK (kind IN ('incremental','deep')),
  state       TEXT NOT NULL DEFAULT 'running'
      CHECK (state IN ('running','completed','failed','cancelled')),
  started_at  INTEGER NOT NULL,
  finished_at INTEGER,
  files_seen  INTEGER NOT NULL DEFAULT 0 CHECK (files_seen >= 0),
  errors      INTEGER NOT NULL DEFAULT 0 CHECK (errors >= 0)
) STRICT;

-- ============================================================
-- トラック
-- ============================================================

CREATE TABLE tracks (
  id            INTEGER PRIMARY KEY,
  album_id      INTEGER REFERENCES albums(id) ON DELETE SET NULL,
  rel_path      TEXT NOT NULL UNIQUE,          -- Library/ からの相対パス（表示用。root 相対、'/' 区切り）
  rel_path_key  TEXT NOT NULL UNIQUE,          -- casefold(NFD(rel_path))。SQLite の BINARY 比較では
                                               -- ZFS insensitive+formD 上の同一ファイルを別と見るため

  -- 同一性解決: (dev,inode) → audio_md5 → rel_path の順（SPEC §6 / §7.1）
  dev           INTEGER,
  inode         INTEGER,
  nlink         INTEGER NOT NULL DEFAULT 1 CHECK (nlink >= 1),  -- >1 なら inode / md5 を同一性に使わない（hardlink、D-26）
  size          INTEGER NOT NULL CHECK (size >= 0),
  mtime_ns      INTEGER NOT NULL,
  ctime_ns      INTEGER NOT NULL,              -- mtime を保存する上書きの検出用
  audio_md5     BLOB CHECK (audio_md5 IS NULL OR length(audio_md5) = 16),
                                               -- 可逆のみ。FLAC は STREAMINFO 由来、ALAC/WAV はデコードして算出。
                                               -- 同一性（移動検出）と音声版の両方に使う
  audio_fp      BLOB CHECK (audio_fp IS NULL OR length(audio_fp) = 32),
                                               -- 非可逆のみ。エンコード済みパケット列の SHA-256（デコードしない）。
                                               -- 音声版の判定にだけ使い、同一性には使わない（SPEC §6）
  tag_hash      BLOB CHECK (tag_hash IS NULL OR length(tag_hash) = 32),
                                               -- 正規化タグ集合の SHA-256。差分がある時だけ tag_version++

  -- 音声属性
  codec         TEXT NOT NULL
      CHECK (codec IN ('flac','opus','alac','aac','mp3','wav','ogg','wv','ape','aiff')),
  lossless      INTEGER NOT NULL CHECK (lossless IN (0,1)),   -- codec から導出
  sample_rate   INTEGER,
  bit_depth     INTEGER,
  channels      INTEGER,
  bitrate       INTEGER,
  duration_ms   INTEGER CHECK (duration_ms IS NULL OR duration_ms >= 0),

  -- 出自（3 属性は直交）
  source_type   TEXT NOT NULL DEFAULT 'unknown'
      CHECK (source_type IN ('cd_rip','download','youtube','unknown')),
  verification  TEXT NOT NULL DEFAULT 'not_attempted'
      CHECK (verification IN ('verified_ar','verified_ctdb','mismatch','unverifiable','not_attempted')),
      -- unverifiable は「TOC が存在しない音源」であり品質の劣後を意味しない

  -- 正規化の来歴（WAV → FLAC）
  original_codec TEXT,
  normalized_at  INTEGER,

  -- ReplayGain: 内部は RG2.0 / -18 LUFS 基準の dB。書き出し時に形式変換
  rg_track_gain REAL,
  rg_track_peak REAL,
  rg_album_gain REAL,                          -- albums.album_gain = 1 の album だけが持つ（D-74）
  rg_album_peak REAL,
  rg_scanned_at INTEGER,
  rg_written_at INTEGER,                       -- スキャンと書き込みを分離

  -- 版管理（Derived の陳腐化判定）
  audio_version INTEGER NOT NULL DEFAULT 1 CHECK (audio_version >= 1),  -- 音声が変わったら ++ → 再エンコード
  tag_version   INTEGER NOT NULL DEFAULT 1 CHECK (tag_version >= 1),    -- タグのみ変更 → タグ上書きのみ

  -- 表示・ソート用キャッシュ列（正は track_tags 側）
  -- album は albums.album の複製。FTS5 external content は content 表の列しか
  -- 参照できないため、検索対象列はすべて tracks 側に持つ（albums_au で同期）
  title          TEXT,
  artist_display TEXT,
  album          TEXT,
  albumartist    TEXT,
  track_no       INTEGER CHECK (track_no IS NULL OR track_no >= 0),
  disc_no        INTEGER CHECK (disc_no IS NULL OR disc_no >= 0),
  date           TEXT,

  seen_at       INTEGER NOT NULL,
  seen_run_id   INTEGER REFERENCES scan_runs(id) ON DELETE SET NULL,  -- 最後に claim した走査
  missing_since INTEGER,                       -- 論理削除。既定 30 日後に GC

  -- 初回登録時刻（スマートプレイリスト DSL の `added`。D-54）。seen_at は走査のたびに更新される
  -- ので使えない。スキャナが INSERT 時に設定し、復活（missing → 再発見）でも変えない
  added_at INTEGER NOT NULL DEFAULT 0,

  -- FLAC 健全性チェックの結果（D-57）。flaccheck ジョブが flac -t と STREAMINFO の MD5 から
  -- 判定して書く。flac_check_version は検査時の audio_version で、現在値と違えば結果は古い
  -- （スキャナはこれらの列を触らない）
  flac_check TEXT
  CHECK (flac_check IS NULL OR flac_check IN ('ok','md5_missing','decode_error')),
  flac_checked_at INTEGER,
  flac_check_version INTEGER,
  flac_check_error TEXT,

  -- トラック自身の埋め込み画像（front cover 優先。D-61）。album のアートワーク
  -- （albums.artwork_id）とは別に持ち、トラックごとに画像が違う album で Derived と再生表示が
  -- そのトラックの絵になる。スキャナ Phase 3 と tagwrite の読み戻しが埋める（NULL の間は album の絵へ倒す）
  artwork_id INTEGER REFERENCES artwork(id) ON DELETE SET NULL,
  -- 画像をキャッシュへ置けなかった（store の I/O 失敗）行の印。物理属性が変わらなくても次のスキャンが
  -- 読み直す（Phase 3 の対象に含める）。画像を記録できたら 0 に戻す
  artwork_dirty INTEGER NOT NULL DEFAULT 0,

  -- 偽ハイレゾ検出の結果（D-71、SPEC §7.10）。hirescheck ジョブが書く。
  -- hires_check_version は検査時の audio_version で、現在値と違えば結果は古い（スキャナは触らない）。
  -- 計測値（カットオフ周波数 / 崖 / 実効ビット）は判定の根拠として残す。計測しなかった側は NULL
  hires_check TEXT
  CHECK (hires_check IS NULL OR hires_check IN ('ok','upsampled','padded','both','inconclusive','decode_error')),
  hires_checked_at INTEGER,
  hires_check_version INTEGER,
  hires_check_error TEXT,
  hires_cutoff_hz INTEGER,
  hires_cliff_db REAL,
  hires_effective_bits INTEGER
) STRICT;

CREATE INDEX idx_tracks_inode   ON tracks(dev, inode);
-- 同一性（移動検出）と duplicate_groups の両方が active 行だけを見るので partial にする
CREATE INDEX idx_tracks_md5     ON tracks(audio_md5) WHERE audio_md5 IS NOT NULL AND missing_since IS NULL;
CREATE INDEX idx_tracks_md5_all ON tracks(audio_md5) WHERE audio_md5 IS NOT NULL;
CREATE INDEX idx_tracks_run     ON tracks(seen_run_id);
CREATE INDEX idx_tracks_album   ON tracks(album_id, disc_no, track_no);
CREATE INDEX idx_tracks_missing ON tracks(missing_since) WHERE missing_since IS NOT NULL;
CREATE INDEX idx_tracks_rg      ON tracks(rg_scanned_at) WHERE rg_scanned_at IS NULL;
CREATE INDEX idx_tracks_sort    ON tracks(albumartist, album_id, disc_no, track_no);
CREATE INDEX idx_tracks_artwork ON tracks(artwork_id) WHERE artwork_id IS NOT NULL;
CREATE INDEX idx_tracks_artwork_dirty ON tracks(id) WHERE artwork_dirty = 1;

-- GET /api/tracks のソート列ごとのキーセット索引（D-39）。
--
-- カーソルページングは (ソートキー, id) の行値比較で次ページを引く。ソート列は NULL を
-- 許すが、行値比較は NULL を含むと不定になるので、索引と ORDER BY の両方で
-- coalesce() した式を使い NULL を '' / 0 / -1 に畳む（NULL は昇順で先頭、降順で末尾）。
-- 式索引は ORDER BY / WHERE の式と字面が一致するときだけ使われるため、
-- クエリ側（src/db/tracks.rs）の式はここと完全に同じでなければならない。
--
-- 上の idx_tracks_sort（albumartist, album_id, disc_no, track_no）は NULL を畳まないので
-- キーセットには使えない。スキャナの album 照合が使う可能性があるので残す。
-- rel_path は UNIQUE なので自動索引で足りる（id のタイブレークは不要）
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

-- 同一音声（audio_md5 一致）が複数の active トラックにある = 重複候補。
-- 自動マージはしない。UI のバッジと一覧に使う（D-29）
CREATE VIEW duplicate_groups AS
SELECT audio_md5, count(*) AS n, min(id) AS representative_id
FROM tracks
WHERE audio_md5 IS NOT NULL AND missing_since IS NULL
GROUP BY audio_md5
HAVING count(*) > 1;

-- ============================================================
-- タグ（任意キー・多値）
-- ============================================================

CREATE TABLE track_tags (
  track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  key      TEXT NOT NULL,                      -- 大文字正規化: TITLE, ARTIST, ...
  idx      INTEGER NOT NULL CHECK (idx >= 0),  -- 多値の順序
  value    TEXT NOT NULL,
  PRIMARY KEY (track_id, key, idx)
) STRICT, WITHOUT ROWID;

CREATE INDEX idx_track_tags_key ON track_tags(key, value);

-- ============================================================
-- 検証（AccurateRip / CTDB）
-- ============================================================

-- 遡及照合の単位は「アルバムの 1 ディスク」で、複数ディスクのアルバムはディスクごとに
-- TOC を再構成して別々に照会する（SPEC §7.3、D-63）。disc_no は 1 ディスクのアルバムでも
-- tracks.disc_no に合わせて 1 を入れる（disc_no が無いトラック群は 1 とみなす）。
-- job_id はジョブの冪等性のため。記録（DB の commit）の後、ログの確定やジョブの done の前に
-- 落ちて起動時リカバリで同じジョブが再実行されても、同じ (job_id, disc_no, method) の行が
-- あれば再利用し、履歴を重複させない
CREATE TABLE album_verifications (
  id              INTEGER PRIMARY KEY,
  album_id        INTEGER NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
  method          TEXT NOT NULL CHECK (method IN ('accuraterip','ctdb')),
  result          TEXT NOT NULL CHECK (result IN ('verified','mismatch','not_found','unverifiable')),
  source          TEXT NOT NULL CHECK (source IN ('rip','retro')),   -- rip（自前）/ retro（遡及照合）
  drive_offset    INTEGER,
  detected_offset INTEGER,
  confidence      INTEGER,
  verified_at     INTEGER NOT NULL,
  log_path        TEXT,                        -- rip.log / verify.log
  disc_no         INTEGER,
  job_id          INTEGER REFERENCES jobs(id) ON DELETE SET NULL
) STRICT;

CREATE INDEX idx_alb_verif ON album_verifications(album_id, verified_at DESC);
CREATE UNIQUE INDEX idx_alb_verif_job ON album_verifications(job_id, disc_no, method)
  WHERE job_id IS NOT NULL;

-- 履歴として積む。再照合時も過去の結果を消さない
CREATE TABLE track_verifications (
  track_id        INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  verification_id INTEGER NOT NULL REFERENCES album_verifications(id) ON DELETE CASCADE,
  crc_v1          INTEGER,
  crc_v2          INTEGER,
  ctdb_crc        INTEGER,
  matched         INTEGER NOT NULL CHECK (matched IN (0,1)),
  PRIMARY KEY (track_id, verification_id)
) STRICT, WITHOUT ROWID;

-- 学習した読み取りオフセット（P2-5、D-83）。rip は最初オフセット 0 で吸い、CTDB / AccurateRip の照合で
-- 見つかったずれを PCM に当ててから配置し、その値をドライブの型番（INQUIRY の vendor + product）ごとに
-- 覚えて次の盤から使う。キャッシュなので消えても次に照合が通った盤で覚え直す（rip.log にも残る）
CREATE TABLE drive_offsets (
  drive        TEXT PRIMARY KEY,
  offset       INTEGER NOT NULL,
  method       TEXT NOT NULL CHECK (method IN ('ctdb', 'accuraterip')),
  confidence   INTEGER NOT NULL,
  detected_at  INTEGER NOT NULL
);

-- ============================================================
-- 派生物（Derived）
-- ============================================================

-- Derived は系統（variant）ごとに 1 本（SPEC §7.6、D-75）。
-- - src_artwork_id: 埋めた画像の artwork.id（NULL = 画像なし）。artwork 行が GC で消えたら
--   NULL に戻り、album 側も SET NULL なので一致し続ける（D-51）
-- - src_rg_scanned_at: 埋めた R128_* の元になった tracks.rg_scanned_at（NULL = 未解析で書いた）
-- どちらも tag_version に乗らない（カバーの差し替えはトラックのタグではない。RG の解析値は
-- DB の列で、ファイルへの書き込みは別のバッチ）ので、Derived に書いた時点の値を持って現在値と比べる
CREATE TABLE derived_files (
  track_id          INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  variant           TEXT NOT NULL CHECK (variant IN ('opus', 'aac')),
  rel_path          TEXT NOT NULL UNIQUE,      -- Derived/ からの相対パス（<variant>/ 以下）
  rel_path_key      TEXT NOT NULL UNIQUE,
  codec             TEXT NOT NULL,
  bitrate           INTEGER,
  src_audio_version INTEGER NOT NULL CHECK (src_audio_version >= 1),  -- 差分 → 再エンコード
  src_tag_version   INTEGER NOT NULL CHECK (src_tag_version >= 1),    -- 差分のみ → タグ上書き
  generated_at      INTEGER NOT NULL,
  src_artwork_id    INTEGER REFERENCES artwork(id) ON DELETE SET NULL,
  src_rg_scanned_at INTEGER,
  audio_profile     TEXT NOT NULL,             -- 音声に効く設定の世代（差分 → 再エンコード）
  tag_profile       TEXT NOT NULL,             -- タグに効く設定の世代（差分 → タグ上書き）
  PRIMARY KEY (track_id, variant)
) STRICT;

CREATE INDEX idx_derived_files_variant ON derived_files(variant);

-- 系統の設定を写す表。config.toml が正で、起動時に db::derived::sync_variants が両系統を揃える。
-- `eligible` が lossy_sources を、transcode ハンドラが multi_value_separator を引く（aac 系統。D-75）
CREATE TABLE derived_variants (
  variant       TEXT PRIMARY KEY CHECK (variant IN ('opus', 'aac')),
  enabled       INTEGER NOT NULL CHECK (enabled IN (0, 1)),
  audio_profile TEXT NOT NULL,
  tag_profile   TEXT NOT NULL,
  codec         TEXT NOT NULL,
  bitrate       INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL,
  lossy_sources INTEGER NOT NULL DEFAULT 0
  CHECK (lossy_sources IN (0, 1)),
  multi_value_separator TEXT NOT NULL DEFAULT ' & '
) STRICT;

-- Derived のパス（canonical key）の排他予約。transcode ジョブが物理的な書き込みの前に取り、
-- 終了時に解放する。x.flac と x.wav のように別の Library パスが同じ Derived パスに写る場合や、
-- 占有を確定してから書くまでの間に別のジョブが同じ宛先へ書くのを防ぐ（track_locks は track 単位
-- なので宛先の衝突は防げない）。GC が Derived の孤児（どのトラックにも紐づかない実体）を消す間も
-- 同じ予約を track_id = NULL で持つ（D-56）。プロセス生存中しか意味を持たず、持ち主のジョブが
-- running でなくなれば無効（起動時リカバリで全件消す）
CREATE TABLE derived_path_locks (
  rel_path_key TEXT PRIMARY KEY,
  track_id     INTEGER REFERENCES tracks(id) ON DELETE CASCADE,   -- NULL = GC の予約
  job_id       INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  acquired_at  INTEGER NOT NULL
) STRICT;

-- 配布ビュー: 可逆は Derived（opus 系統）、非可逆は Library 原本
-- Derived を採用するのは音声版が現在値と一致するときだけ。音声が陳腐化した
-- Derived（再エンコード待ち）は配布せず Library 原本へフォールバックする。
-- タグだけ陳腐化した Derived は採用し stale_tags=1 で通知する（タグ上書きジョブが
-- 追随する。原本へ落とすと容量の大きい可逆を配ってしまう）
CREATE VIEW delivery AS
SELECT
  t.id AS track_id,
  CASE WHEN t.lossless = 1 AND d.rel_path IS NOT NULL
            AND d.src_audio_version = t.audio_version
       THEN 'Derived/' || d.rel_path
       ELSE 'Library/' || t.rel_path
  END AS path,
  CASE WHEN t.lossless = 1 AND d.rel_path IS NOT NULL
            AND d.src_audio_version = t.audio_version
       THEN d.codec ELSE t.codec
  END AS codec,
  CASE WHEN t.lossless = 1 AND d.rel_path IS NOT NULL
            AND d.src_audio_version = t.audio_version
            AND d.src_tag_version <> t.tag_version
       THEN 1 ELSE 0
  END AS stale_tags
FROM tracks t
LEFT JOIN derived_files d ON d.track_id = t.id AND d.variant = 'opus'
WHERE t.missing_since IS NULL;

-- ============================================================
-- プレイリスト
-- ============================================================

-- 名前はそのまま書き出しファイル名（Playlists/<profile>/<name>.m3u8）になる。ZFS の
-- insensitive + formD では Foo.m3u8 と foo.m3u8、NFC と NFD の同名が同じ実体なので、
-- `name` の BINARY UNIQUE だけでは別プレイリストの書き出しが上書きし合う。rel_path_key と
-- 同じ規則（casefold + NFD。domain::relpath::canonical_key）の name_key を持ち、こちらで一意にする（D-53）
CREATE TABLE playlists (
  id           INTEGER PRIMARY KEY,
  name         TEXT NOT NULL UNIQUE,
  kind         TEXT NOT NULL DEFAULT 'manual' CHECK (kind IN ('manual','smart')),
  rule_source  TEXT,                           -- smart: foobar 風 DSL の原文
  rule_ast     TEXT CHECK (rule_ast IS NULL OR json_valid(rule_ast)),  -- smart: パース済み AST (JSON)
  auto_export  INTEGER NOT NULL DEFAULT 1 CHECK (auto_export IN (0,1)),
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL,
  name_key TEXT NOT NULL DEFAULT ''            -- canonical_key(name)。アプリが INSERT / 改名時に書く
) STRICT;
-- rule_source を残すのは、AST から DSL を逆生成すると整形が原文と変わり
-- ユーザの意図した記述が失われるため。実行には rule_ast のみを使う。

CREATE UNIQUE INDEX idx_playlists_name_key ON playlists(name_key);

CREATE TABLE playlist_items (
  playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
  position    INTEGER NOT NULL,
  track_id    INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  PRIMARY KEY (playlist_id, position)
) STRICT, WITHOUT ROWID;

CREATE INDEX idx_playlist_items_track ON playlist_items(track_id);

-- 出力先ごとのパスマッピング。
-- NAS 上の /library/... をそのまま書いても foobar からは開けないため必須。
CREATE TABLE export_profiles (
  id          INTEGER PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,
  format      TEXT NOT NULL CHECK (format IN ('m3u8','pls','fb2k_query')),
  source      TEXT NOT NULL CHECK (source IN ('master','delivery')),   -- master(Library) / delivery(Derived優先)
  path_style  TEXT NOT NULL CHECK (path_style IN ('relative','absolute')),
  path_prefix TEXT,                            -- 例: \\TRUENAS\music\
  path_sep    TEXT NOT NULL DEFAULT '/'
) STRICT;

INSERT INTO export_profiles (name, format, source, path_style, path_prefix, path_sep) VALUES
  ('foobar',   'm3u8', 'master',   'absolute', '\\TRUENAS\music\', '\'),
  ('android',  'm3u8', 'delivery', 'relative', NULL, '/'),
  ('internal', 'm3u8', 'master',   'relative', NULL, '/');

CREATE TABLE playlist_exports (
  playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
  profile_id  INTEGER NOT NULL REFERENCES export_profiles(id) ON DELETE CASCADE,
  out_path    TEXT NOT NULL,
  exported_at INTEGER,
  PRIMARY KEY (playlist_id, profile_id)
) STRICT, WITHOUT ROWID;

-- ============================================================
-- 再生リストの購読と同期（D-78、SPEC §7.7「再生リストの購読」）
-- ============================================================

-- YouTube の再生リスト 1 本 → Library の album 1 つ（追記先）。
-- - id は再利用しない（AUTOINCREMENT）。ytdl / playlist_sync の payload と Inbox の
--   spindle-inbox.json は裸の subscription_id を持ち、DELETE は走行中のジョブを止めないので、
--   削除直後に別の購読を作ると旧ジョブが新しい購読へ誤帰属する
-- - album_id は追記先の同一性（album 全体の移動は id を維持する。D-32）。登録時は NULL で、同期か配置が
--   albumartist / album / category から解決できたときに束ねる（album_id IS NULL のときだけ = CAS）。
--   album が消えれば NULL に戻り（SET NULL）、次の同期で再解決する
-- - target_key は canonical_key(albumartist) || '/' || canonical_key(album)。同じ追記先の購読は 1 つだけ
--   （Inbox の youtube/<albumartist>/<album> = 1 購読 = 1 category にする）。album_id も非 NULL の間は
--   UNIQUE で、束ね同士の競合は DB で片方が失敗する
-- - sync_requested_at は承認の後続・手動要求の latch（時刻は表示用で、比較には使わない）。同期の開始で
--   NULL にし、終了時に立っていれば Requeue、残れば dispatcher が回収する
-- - last_attempted_at は同期の開始時刻（成否を問わない。定期投入の基準）、last_synced_at は成功の終端
-- - last_result は最終同期の結果 JSON（取れない一覧・揃えられない一覧・持ち越し・バッチ id 等）
CREATE TABLE playlist_subscriptions (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  list_id           TEXT NOT NULL UNIQUE,
  url               TEXT NOT NULL,
  album_id          INTEGER REFERENCES albums(id) ON DELETE SET NULL,
  target_key        TEXT NOT NULL UNIQUE,
  albumartist       TEXT NOT NULL,
  album             TEXT NOT NULL,
  category          TEXT,
  align             INTEGER NOT NULL DEFAULT 1 CHECK (align IN (0, 1)),
  enabled           INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
  max_enqueue       INTEGER NOT NULL DEFAULT 50 CHECK (max_enqueue >= 1),
  created_at        INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL,
  last_attempted_at INTEGER,
  last_synced_at    INTEGER,
  sync_requested_at INTEGER,
  last_result       TEXT CHECK (last_result IS NULL OR json_valid(last_result))
) STRICT;

CREATE UNIQUE INDEX idx_playlist_subscriptions_album ON playlist_subscriptions(album_id)
  WHERE album_id IS NOT NULL;

-- ============================================================
-- Inbox の承認キュー（SPEC §7.8、D-68）
-- ============================================================

-- 正は Inbox のファイルで、行はキャッシュ（ディレクトリが消えれば行も消す。placed だけは
-- 結果を見せるために一定時間残す）
CREATE TABLE inbox_items (
  id              INTEGER PRIMARY KEY,
  rel_dir         TEXT NOT NULL,            -- Inbox 相対（root 直下の音声は ''）
  rel_dir_key     TEXT NOT NULL UNIQUE,     -- casefold(NFD)
  state           TEXT NOT NULL DEFAULT 'pending'
      CHECK (state IN ('pending','approved','placing','placed','rejected','failed')),
  detected_at     INTEGER NOT NULL,
  seen_at         INTEGER NOT NULL,         -- 最後に走査で見た時刻
  approved_at     INTEGER,
  draft           TEXT,                     -- 承認時の補正（JSON。InboxDraft）
  error           TEXT,                     -- failed の理由、pending に戻した理由
  placed_album_id INTEGER REFERENCES albums(id) ON DELETE SET NULL,
  placed_at       INTEGER
) STRICT;

CREATE INDEX idx_inbox_items_state ON inbox_items(state);

CREATE TABLE inbox_files (
  item_id      INTEGER NOT NULL REFERENCES inbox_items(id) ON DELETE CASCADE,
  rel_path     TEXT NOT NULL,               -- Inbox 相対
  rel_path_key TEXT NOT NULL UNIQUE,
  inode        INTEGER NOT NULL,
  size         INTEGER NOT NULL,
  mtime_ns     INTEGER NOT NULL,
  ctime_ns     INTEGER NOT NULL,
  codec        TEXT NOT NULL,
  lossless     INTEGER NOT NULL CHECK (lossless IN (0,1)),
  sample_rate  INTEGER,
  bit_depth    INTEGER,
  channels     INTEGER,
  duration_ms  INTEGER,
  tags         TEXT NOT NULL,               -- [[key, value], ...] の JSON（表示と下書きに使う）
  PRIMARY KEY (item_id, rel_path_key)
) STRICT, WITHOUT ROWID;

-- ============================================================
-- 認証（単一ユーザ）
-- ============================================================

CREATE TABLE auth (
  id            INTEGER PRIMARY KEY CHECK (id = 1),
  password_hash TEXT NOT NULL,                 -- argon2id
  updated_at    INTEGER NOT NULL
) STRICT;

-- 生トークンは保存しない。Cookie のランダム 32 バイトを SHA-256 したものを鍵にする
CREATE TABLE sessions (
  token_hash BLOB PRIMARY KEY CHECK (length(token_hash) = 32),
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  user_agent TEXT
) STRICT, WITHOUT ROWID;

CREATE INDEX idx_sessions_exp ON sessions(expires_at);

-- ============================================================
-- 編集履歴（バッチ単位の巻き戻し）
-- ============================================================
--
-- 3 層: edit_batches（ユーザ操作 1 回）> edit_ops（トラック 1 本への 1 操作）> edits（フィールド値）
--
-- ファイル反映の単位は edit_ops。同一トラックの全タグ差分を 1 回の tmp+rename で書き、
-- 配下の edits を同時に確定する。フィールドごとに rename すると 2 件目以降で
-- expected_inode が必ず外れるため、フィールド単位で反映してはならない。
-- tag_version はトラックごと・バッチごとに 1 回だけ進める。
--
-- バッチ状態機械（SPEC §7.5）:
--   prepared → applying → applied | partial | failed | cancelled
--   prepared:  ops/edits 記録 + DB 更新済み、ファイル未反映
--   applying:  ジョブがファイルへ反映中
--   applied:   全 ops が applied
--   partial:   一部が skipped_conflict / failed（残りは applied）
--   failed:    1 件も反映できなかった
--   cancelled: cancel 要求で停止。applied になった ops は残る（partial と同様に revert 可）
-- 巻き戻しは新しいバッチとして記録し reverts_batch_id で元を指す。元バッチには
-- reverted_at を立て、同じバッチを二度 revert しない（redo は逆バッチを revert する）。
-- revert できるのは終端状態（applied / partial / failed / cancelled）のバッチだけで、
-- 実際に applied になった ops の集合だけを反転する。
CREATE TABLE edit_batches (
  id                INTEGER PRIMARY KEY,
  created_at        INTEGER NOT NULL,
  description       TEXT,
  state             TEXT NOT NULL DEFAULT 'prepared'
      CHECK (state IN ('prepared','applying','applied','partial','failed','cancelled')),
  affected          INTEGER CHECK (affected IS NULL OR affected >= 0),   -- ops 件数
  reverts_batch_id  INTEGER REFERENCES edit_batches(id) ON DELETE SET NULL,
  finished_at       INTEGER,                   -- 終端状態に入った時刻
  reverted_at       INTEGER
) STRICT;

CREATE INDEX idx_edit_batches_reverts ON edit_batches(reverts_batch_id)
  WHERE reverts_batch_id IS NOT NULL;
CREATE INDEX idx_edit_batches_open ON edit_batches(state)
  WHERE state IN ('prepared','applying');

-- 1 op = 1 トラックへの 1 操作。事前条件はここに持つ。
-- pending の op があるトラックは再編集できない（HTTP 409。SPEC §7.5）。
-- これを DB でも保証するため (track_id) WHERE result='pending' を UNIQUE にする。
-- kind = 'md5' は FLAC の STREAMINFO に MD5 が無い（全ゼロ）トラックへ、デコードした PCM MD5 を書く
-- 操作（SPEC §7.9、D-59）。音声もタグも変わらず audio_version / tag_version は据え置き
CREATE TABLE edit_ops (
  id                INTEGER PRIMARY KEY,
  batch_id          INTEGER NOT NULL REFERENCES edit_batches(id) ON DELETE CASCADE,
  ordinal           INTEGER NOT NULL,          -- バッチ内の適用順（rename の 2 段階更新で意味を持つ）
  track_id          INTEGER NOT NULL,          -- あえて FK にしない（削除後も履歴を残す）
  kind              TEXT NOT NULL CHECK (kind IN ('tags','rename','delete','archive','md5')),

  -- ファイル反映の事前条件。記録時点の実体。反映直前に一致しなければ
  -- 外部変更とみなし skipped_conflict にする（ファイルが正）。
  -- rel_path も含める: 外部 rename は inode/mtime を変えないため dev/inode/mtime だけでは
  -- 検知できない。tags op は外部 rename 後の新パスへ追随して書いてよいが、
  -- rename op は conflict にする（SPEC §7.1 / §7.5）
  -- ctime_ns を含めるのは、inode も mtime も保ったままの in-place 更新を検出するため。
  -- 反映直前に同じ FD の fstat で全件を確認し、その FD の親 dir に tmp を作る
  expected_dev      INTEGER,
  expected_inode    INTEGER,
  expected_size     INTEGER,
  expected_mtime_ns INTEGER,
  expected_ctime_ns INTEGER,
  expected_tag_hash BLOB,
  expected_rel_path TEXT,

  result            TEXT NOT NULL DEFAULT 'pending'
      CHECK (result IN ('pending','applied','skipped_conflict','failed','superseded')),
      -- superseded は将来の「後続バッチが先行 intent を統合する」方式のために予約。
      -- 現行仕様（再編集は 409）では使わない
  error             TEXT,
  job_id            INTEGER REFERENCES jobs(id) ON DELETE SET NULL,  -- この op を反映した track ジョブ
  applied_at        INTEGER,
  UNIQUE (batch_id, ordinal)
) STRICT;

-- 「そのトラックの最新 op」を引く（conflict バッジは最新 op が skipped_conflict のときだけ）
CREATE INDEX idx_edit_ops_track ON edit_ops(track_id, id DESC);
CREATE UNIQUE INDEX idx_edit_ops_pending ON edit_ops(track_id) WHERE result = 'pending';
CREATE INDEX idx_edit_ops_job ON edit_ops(job_id) WHERE job_id IS NOT NULL;

-- フィールド値。old_value / new_value は JSON:
--   tags:    key = 大文字正規化済みタグ名。値は文字列配列（多値の順序を保つ）。
--            タグ不存在は JSON null（SQL NULL は使わない）
--   rename:  key = 'rel_path'。値は文字列
--   delete:  key = 'missing_since'。値は整数または null
--   archive: key = 'archive'。値は {"from": Library 相対パス, "to": Archive 相対パス}
--   md5:     key = 'audio_md5'。値は 32 桁の hex 文字列（全ゼロ = 未設定）。new_value は記録時 null で、
--            反映時に計算値を書く
CREATE TABLE edits (
  id        INTEGER PRIMARY KEY,
  op_id     INTEGER NOT NULL REFERENCES edit_ops(id) ON DELETE CASCADE,
  key       TEXT NOT NULL,
  old_value TEXT NOT NULL CHECK (json_valid(old_value)),
  new_value TEXT NOT NULL CHECK (json_valid(new_value)),
  UNIQUE (op_id, key)
) STRICT;

-- Archive へ退避したファイルの台帳。GC の削除適格性はここで決める
-- （編集履歴は revert / redo で状態が動くため台帳に使わない）。
-- reason: normalize = ロスレス正規化で退避した元ファイル。restore = 正規化の巻き戻しで、Library に
-- あった FLAC を Archive へ move したもの（元ファイルは Archive から Library へコピーで戻し、その行は
-- state = 'restored'。Archive は追記のみ。SPEC §7.4、D-46）
CREATE TABLE archived_files (
  id             INTEGER PRIMARY KEY,
  track_id       INTEGER,                      -- FK にしない（トラック削除後も台帳は残す）
  op_id          INTEGER REFERENCES edit_ops(id) ON DELETE SET NULL,
  rel_path       TEXT NOT NULL UNIQUE,         -- Archive/ からの相対パス
  rel_path_key   TEXT NOT NULL UNIQUE,         -- casefold(NFD(rel_path))
  source_rel_path TEXT NOT NULL,               -- 退避前の Library/ 相対パス（履歴値。比較には使わない）
  reason         TEXT NOT NULL CHECK (reason IN ('normalize','restore')),
  archived_at    INTEGER NOT NULL,
  eligible_after INTEGER NOT NULL,             -- archived_at + [gc].retention_days
  state          TEXT NOT NULL DEFAULT 'held'
      CHECK (state IN ('held','restored','deleted')),
  state_at       INTEGER
) STRICT;

CREATE INDEX idx_archived_gc ON archived_files(eligible_after) WHERE state = 'held';

-- ============================================================
-- ジョブ
-- ============================================================

-- 編集バッチの実行モデルは coordinator + track ジョブ（SPEC §8）:
--   tagwrite / rename ジョブは track 単位（dedup_key = 'tagwrite:<track_id>:<tag_version>'）で
--   edit_batch_id によりバッチへ紐づく。バッチの終端状態は子ジョブ・ops の結果から集計する。
CREATE TABLE jobs (
  id          INTEGER PRIMARY KEY,
  type        TEXT NOT NULL
      CHECK (type IN ('scan','rip','verify','rg','transcode','tagwrite','rename',
                      'normalize','thumbnail','flaccheck','inbox','ytdl','gc','backup','hirescheck',
                      'playlist_sync')),
  dedup_key   TEXT,                            -- 二重投入防止。type を含めて構成する（例 'tagwrite:123:7'）
  payload     TEXT NOT NULL CHECK (json_valid(payload)),
  state       TEXT NOT NULL DEFAULT 'queued'
      CHECK (state IN ('queued','running','done','failed','cancelled')),
  edit_batch_id INTEGER REFERENCES edit_batches(id) ON DELETE SET NULL,
  priority    INTEGER NOT NULL DEFAULT 0,
  run_after   INTEGER,                         -- 指数バックオフの次回実行時刻。NULL なら即時
  progress    REAL CHECK (progress IS NULL OR (progress >= 0.0 AND progress <= 1.0)),
  total       INTEGER CHECK (total IS NULL OR total >= 0),
  done        INTEGER CHECK (done IS NULL OR done >= 0),
  attempts    INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  max_attempts INTEGER NOT NULL DEFAULT 5 CHECK (max_attempts >= 1),
  last_error  TEXT,
  cancel_requested_at INTEGER,                 -- ハンドラが進捗更新のたびに見て自発的に止まる
  created_at  INTEGER NOT NULL,
  started_at  INTEGER,
  finished_at INTEGER,
  note        TEXT                             -- 完了時の結果 1 行（SPEC §8。失敗の理由は last_error）
) STRICT;

-- dedup は「未完了のジョブ」の間だけ効かせる。列 UNIQUE にすると done/failed 後に
-- 同じキー（scan の固定キー、同 version の再試行）を永久に投入できなくなる
CREATE UNIQUE INDEX idx_jobs_dedup_active ON jobs(dedup_key)
  WHERE dedup_key IS NOT NULL AND state IN ('queued','running');

CREATE INDEX idx_jobs_queue ON jobs(state, run_after, priority DESC, created_at)
  WHERE state IN ('queued','running');
CREATE INDEX idx_jobs_finished ON jobs(finished_at) WHERE finished_at IS NOT NULL;
CREATE INDEX idx_jobs_batch ON jobs(edit_batch_id) WHERE edit_batch_id IS NOT NULL;

-- 同一トラックに対する競合ジョブの直列化。
-- 起動時リカバリで全行削除する（プロセス生存中しか意味を持たない）。
-- 複数トラックを掴むジョブ（album 単位の rg 等）は track_id 昇順に取得し、
-- 1 つでも取れなければ全解放して再キューする（デッドロック回避）
CREATE TABLE track_locks (
  track_id   INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
  job_id     INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  acquired_at INTEGER NOT NULL
) STRICT;

-- ジョブ間の名前付き排他（D-56）。scan と gc は同じ `library` を取り、取れた側だけが走る
-- （check-then-requeue を両側に置くだけでは同時 claim で譲り合いが続く）。track_locks と同じく
-- プロセス生存中しか意味を持たず、持ち主のジョブが running でなくなれば無効。起動時リカバリで全件消す
CREATE TABLE job_mutexes (
  name        TEXT PRIMARY KEY,
  job_id      INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  acquired_at INTEGER NOT NULL
) STRICT;

-- ============================================================
-- 全文検索（日本語主体のため trigram）
-- ============================================================

-- external content 方式。索引対象の 4 列はすべて tracks に実在する列でなければ
-- ならない（列値の取得・rebuild が content 表を直接読むため）。
-- 'delete' 行には挿入時と完全に同じ値を渡す必要がある。NULL で代用すると
-- 旧トークンが索引に残る
CREATE VIRTUAL TABLE tracks_fts USING fts5(
  title, artist_display, album, albumartist,
  content = 'tracks',
  content_rowid = 'id',
  tokenize = 'trigram'
);

CREATE TRIGGER tracks_ai AFTER INSERT ON tracks BEGIN
  INSERT INTO tracks_fts(rowid, title, artist_display, album, albumartist)
  VALUES (new.id, new.title, new.artist_display, new.album, new.albumartist);
END;

CREATE TRIGGER tracks_ad AFTER DELETE ON tracks BEGIN
  INSERT INTO tracks_fts(tracks_fts, rowid, title, artist_display, album, albumartist)
  VALUES ('delete', old.id, old.title, old.artist_display, old.album, old.albumartist);
END;

-- 索引対象列が変わったときだけ再索引する。seen_at 等の更新で毎回
-- 削除・再挿入が走るとスキャンの最速パスが FTS 書き込みで律速される
CREATE TRIGGER tracks_au AFTER UPDATE OF title, artist_display, album, albumartist
ON tracks BEGIN
  INSERT INTO tracks_fts(tracks_fts, rowid, title, artist_display, album, albumartist)
  VALUES ('delete', old.id, old.title, old.artist_display, old.album, old.albumartist);
  INSERT INTO tracks_fts(rowid, title, artist_display, album, albumartist)
  VALUES (new.id, new.title, new.artist_display, new.album, new.albumartist);
END;

-- albums.album の変更を tracks.album キャッシュへ伝播する（上の tracks_au が連鎖して FTS も追随）
CREATE TRIGGER albums_au AFTER UPDATE OF album ON albums BEGIN
  UPDATE tracks SET album = new.album WHERE album_id = new.id;
END;

-- 注意:
--  * rel_path の UNIQUE は一括リネームで衝突する。全削除→全挿入ではなく、
--    一時パス経由の 2 段階更新か deferred 相当の手順で処理すること。
--  * 3 文字未満の検索語は trigram では引けないため LIKE にフォールバックする。

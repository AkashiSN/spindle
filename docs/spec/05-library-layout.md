# 5. ライブラリレイアウト

```
/mnt/ssd/media/                        （実機のプールは ssd / hdd。D-45）
├── Library/                           [dataset] snapshot: 毎日 + 編集前
│   └── <Category>/<AlbumArtist>/<Album>/
│       ├── 1-01 Title.flac
│       ├── cover.jpg
│       ├── disc.cue                   CD リップ時のみ（複数枚組は disc<N>.cue。D-67）
│       ├── disc.toc                   CD リップ時のみ（同 disc<N>.toc）
│       └── rip.log                    自前リップ時のみ（同 rip<N>.log。先頭行 `spindle rip log v1`）
├── Derived/                           [dataset] snapshot: なし
│   ├── opus/<Category>/<AlbumArtist>/<Album>/1-01 Title.opus   Android・Web 再生・配布ビュー
│   └── aac/<Category>/<AlbumArtist>/<Album>/1-01 Title.m4a     Apple 向け（P4-8。D-75）
├── Archive/  → /mnt/hdd/media/Archive [dataset, hdd] snapshot: 週次
│   ├── youtube/<id>.webm              YouTube の原本（id 名。DB に行は作らない。D-70）
│   └── （FLAC 正規化で退避した WAV / ALAC / AIFF もここ。GC まで保持）
├── Inbox/                             [dataset] snapshot: なし
│   ├── （承認前の一時領域。ハイレゾ購入分などをここへ置く）
│   └── youtube/<AlbumArtist>/<Album>/ ytdl ジョブが置く（判定できないものは youtube/_unmatched/<channel>/。
│       spindle-inbox.json を同梱。D-70）
└── Playlists/
    ├── m3u8/                          旧ライブラリから移した m3u8（取り込み元）
    └── <profile>/<name>.m3u8          書き出し（internal / android / foobar。D-53）

/mnt/ssd/apps/spindle/                [dataset] snapshot: 毎日
├── spindle.db                        SQLite
├── backup/                            VACUUM INTO による日次バックアップ
├── thumbs/                            ハッシュアドレスのサムネイルキャッシュ
├── verify/<album_id>.log              遡及照合の verify.log（Library には置かない。D-63）
├── tmp/                               リップ・変換の作業領域
└── config.toml
```

## パステンプレート

```toml
[layout]
multi_disc  = "{category}/{albumartist}/{album}/{disc}-{track:02}. {title}"
single_disc = "{category}/{albumartist}/{album}/{track:02}. {title}"
unsorted    = "_Unsorted/{albumartist}/{album}/{track:02}. {title}"
```

- 階層は Artist ではなく **AlbumArtist**。コンピレーションは `Various Artists`
- マルチディスクはサブフォルダを作らず `1-01` 前置き（アルバム = 1ディレクトリを維持）
- 発売年はパスに含めない。DATE / ORIGINALDATE タグは必ず保持する
- パス衝突時のみ自動降格: `{album}` → `{album} ({year})` → `{album} ({edition})`。最後の段で edition を
  持たないリリースは元の `{album}` のまま（通常版と Hi-Res 版のように片方だけが EDITION を持つ同名の組は、持つ
  側だけが分かれる。edition の無いリリースが 2 つ以上、または edition が同じなら conflict）。edition は
  album の構成トラックの `EDITION` タグの最頻値（`albums.edition`。スキャナ・Inbox の配置が入れ、変化の無い
  既存の album は NULL のときだけ次のスキャンで埋める。[D-43](../decisions/D-043.md) 追記）
- **衝突 = マージではない。** 配置前に MusicBrainz Release ID（無ければ album 行）で
  同一リリース判定を行い、異なる場合は必ず別ディレクトリにする。DiscID は 1 枚ごとの値なので
  リリースの鍵にしない（[D-67](../decisions/D-067.md) 追記 3）

## ファイル名正規化

ytmusic の foo_fileops 互換置換テーブルを継承し、以下を追加:

```
~ → ～   * → ＊   ∕ → ／   : → ：   > → ＞   < → ＜   ? → ？
Ø → O   À → A   ô → o   è → e   é → e   ë → e   ゔ → う
```

- タグ値は NFC 正規化のみ（原文字を保持）、ファイル名にのみ置換テーブルを適用
- 表に無い禁止文字 `/` `\` `|` `"` も同じ流儀で全角化（`／` `＼` `｜` `＂`）、制御文字は除去（[D-43](../decisions/D-043.md)）
- SMB 制約: 末尾のドット・スペース禁止（削る）、予約名 (CON, PRN, AUX, NUL, COM1-9, LPT1-9) 回避
  （`_` を後置: `CON` → `CON_`）。空になった要素は `_`
- Android 側 exFAT 制約: パス長上限、`|` `"` 禁止
- **各コンポーネント**は 255 バイト以下（ZFS / SMB の上限。パス全体の上限ではない）。
  パス全体は Windows / Android 互換のため **240 文字（UTF-16 単位）**を上限とし、
  超える場合はタイトル部を省略記号付きで切り詰める
- 値が無いときのフォールバック: `albumartist` → `artist` → `Unknown Artist`、`album` →
  `Unknown Album`、`title` → 現在のファイル名、`track` → 0、`disc` → 1（[D-43](../decisions/D-043.md)）
- テンプレートの選択: category が無ければ `unsorted`、複数ディスク（`disc_count > 1` または
  構成トラックの `disc_no` の最大が 2 以上）なら `multi_disc`、それ以外は `single_disc`

## パスの表現と境界

DB・API・テンプレート展開・プレイリスト出力で扱うパスはすべて **root（Library / Derived /
Archive / Inbox / Playlists のいずれか）からの相対パス**で、次を満たす文字列だけを受け付ける。
満たさない入力は API では 400、スキャンでは「対象外」としてログに出す。

- 区切りは `/`。先頭 `/` なし、空コンポーネントなし、`.` / `..` コンポーネントなし、
  NUL なし、`\` を含まない
- 各コンポーネントは §「ファイル名正規化」の SMB / exFAT 制約を満たす
- 比較用に `casefold(NFD(path))` を **canonical key** として別列（`rel_path_key` /
  `rel_dir_key`、Archive の `archived_files.rel_path_key` も同様）に持ち UNIQUE にする。
  ZFS は `insensitive` + `formD` なので、SQLite の BINARY 比較で別と見えるパスが同じ
  ファイルを指し得る。衝突判定・リネームの一意性・2 段階更新の一時パスはすべて key 側で
  行う。表示は原文のまま
- この key は **spindle 側の保守的な同値規則**であり、OpenZFS の `u8_textprep` と同一とは
  言い切れない（ß、トルコ語の I、合字、Unicode バージョン差）。DB の key はあくまで
  事前判定で、**最終的な衝突判定はファイルシステムに任せる**: 作成は `O_EXCL`、rename は
  `RENAME_NOREPLACE` を使い、失敗したら衝突として扱う。導入時に対象 TrueNAS 上で
  代表ケースの corpus を作成して両者の差を確認するテストを P0-5 に置く

ファイルを開くときは**パス文字列を結合して open しない**。root の dirfd を起点に
`openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`（`rustix`）で開く。canonicalize
してから prefix を比較する方式は symlink の差し替え（TOCTOU）を防げない（[D-31](../decisions/D-031.md)）。
本番は Linux 5.6+ を前提とし、`openat2` が使えなければ**起動時に失敗**する（fail-fast）。
非 Linux の開発機では実ファイルを触る結合テストを skip し、Linux の CI で走らせる。
`canonicalize` + prefix 比較へのフォールバックは設けない。

- スキャナは symlink を**辿らない**（ディレクトリもファイルも）。見つけたら対象外として
  一覧に出す。hardlink（`nlink > 1`）は inode も md5 も同一性に使わず、`rel_path_key` だけで
  解決して警告バッジを出す（[D-26](../decisions/D-026.md)）
- SMB / exFAT 制約に反する名前（禁止文字・末尾ドット/スペース・予約名）は**スキャナが
  対象外にする**。したがって移行前の preflight でもブロッカーにする（[§14](14-deploy.md)、P0-14）
- 書き込みの一時ファイルは**対象と同じディレクトリ**に `.spindle-tmp-<random>` で
  `O_EXCL` 作成し、fsync → rename。取り残された `.spindle-tmp-*` はスキャンが回収する
- 外部コマンドは `sh -c` を使わず引数配列で起動する。パスは対応するツールでは `--` の後、
  非対応なら `./` を前置して先頭 `-` を無害化する。可能ならファイルを開いて
  stdin / stdout で渡す。タイムアウト・終了コード検査・stderr のログ出力・
  プロセスグループごとの kill を必須にする（規約）

## Category

- **パス決定に使う `category` と、ファイルに書く `GENRE` タグは別フィールド**
  - GENRE は多値を取り得るがパスは単一値
  - MusicBrainz 由来のジャンル文字列は表記揺れが激しい
  - ジャンルは後から変わる。パス直結だと数千ファイルが動く
- `categories` テーブルで統制語彙として管理、`genre_category_map` で自動推定
- **Library 直下のディレクトリ名はスキャナが自動で語彙にする**（[D-92](../decisions/D-092.md)）。音声を含む album ディレクトリ
  （直下 + 1 段以上。`Anime/<albumartist>/<album>`）の親になっている直下の名前を、canonical key で語彙に
  無ければ登録し、その album の category にする。`_Unsorted`（`[layout].unsorted` の先頭）と、直下の
  ディレクトリそのもの・root 直下に置かれた曲は対象外。category が NULL のまま残っている album（語彙が
  後から入った既存の DB）も次のスキャンで直下の名前から埋める（NULL だけ。人が付けた値は変えない。
  部分索引 `idx_albums_category_null` で NULL の行だけを読む）。埋めた album のトラックは `library` イベントで
  知らせ（語彙だけが増えた run は `bulk`）、画面の category の一覧は `library` イベントのたびと語彙の追加・削除のたびに取り直す。自動登録は追加だけで、不要になった
  語彙は使われていなければ `DELETE /api/categories/:id`（設定画面）で消す。
  album の category はディレクトリ名 → GENRE → `genre_category_map` の順
- 語彙にマッチしないものは `_Unsorted/` に配置し、取り込みを止めない
- ytmusic 由来のレーベル軸カテゴリ（神椿Studio 等）も同じ語彙表に混在させてよい

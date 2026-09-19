# spindle — 設計仕様

> ステータス: ドラフト v0.2 / 主要な設計判断は確定済み
>
> CD を積むスピンドルから。バイナリ名・コマンド名も `spindle`。

---

## 1. 目的とスコープ

TrueNAS 上で動作する単一コンテナのWebアプリケーション。CDリッピング、メディアライブラリ管理、
メタデータ一括編集、ReplayGain、プレイリスト管理、簡易再生を提供する。
Windows版 foobar2000 の実用機能を代替し、既存CLIツール `ytmusic` を統合する。

### 非目標

- 高度な再生機能（ギャップレス、DSP チェーン、クロスフェード、ASIO/WASAPI 相当）
- マルチユーザ、権限管理、公開インターネットへの露出
- モバイルアプリ（ブラウザで足りる）
- 音楽配信サーバ（Subsonic API 互換等）— **非対応で確定。**
  Android へは Derived + m3u8 のファイル同期を継続する
- DSD / SACD ISO の取り扱い

### 規模想定

- 総容量 300GB、1〜6万トラック
- 単一ユーザ、LAN 内アクセスのみ
- 常時稼働（NAS）

---

## 2. 用語

| 用語 | 定義 |
|---|---|
| Library | 唯一の正となるメディアツリー。1トラック=1ファイル |
| Derived | Library から生成される配布用 Opus。破棄・再生成可能 |
| Archive | アプリが再生成できない生データ（YouTube webm 等）。追記のみ |
| 配布ビュー (delivery) | 可逆なら Derived、非可逆なら Library を指す解決規則 |
| Category | パス最上位階層。統制語彙。GENRE タグとは別概念 |
| master | そのトラックについて手元にある最高品質のファイル（= Library の実体） |

---

## 3. 基本原則

1. **ファイルが正、DBはキャッシュ。** DB を消してもファイルから再構築できること。
   ただし以下は DB にしか存在しないため個別にバックアップする:
   プレイリスト / 編集履歴 / 検証結果 / ジョブ履歴
2. **パスは識別子ではない。** リネームは日常操作。同一性は inode と audio_md5 で解決する
3. **音声とタグを分けて版管理する。** タグ編集で再エンコードを起こさないため
4. **すべての破壊的操作はバッチ単位で巻き戻せる。** ZFS スナップショットは最後の砦であり、
   一括編集の取り消しには粒度が粗すぎる
5. **外部からの変更を前提とする。** SMB 経由で foobar2000 や他プレイヤーが同じファイルを
   触る。アプリはファイルを排他ロックせず、再スキャンで調停する
6. **ジョブは冪等。** コンテナ再起動、電源断、途中キャンセルから安全に再開できること

---

## 4. アーキテクチャ

```
┌─────────────────────────────────────────────────┐
│  Browser (React SPA, rust-embed で同梱)          │
└───────────────┬─────────────────────────────────┘
                │ REST + SSE
┌───────────────▼─────────────────────────────────┐
│  axum HTTP server                               │
│   ├── API handlers                              │
│   ├── streaming (Range / on-the-fly transcode)  │
│   └── SSE event bus                             │
├─────────────────────────────────────────────────┤
│  Job scheduler (tokio)                          │
│   ├── queue: rip(並列1) / scan / rg /           │
│   │          transcode / tagwrite / verify /    │
│   │          normalize / thumbnail              │
│   └── recovery on startup                       │
├─────────────────────────────────────────────────┤
│  Domain                                         │
│   scanner / tagger / pathgen / rg /             │
│   cdrom / accuraterip / musicbrainz / parser    │
├─────────────────────────────────────────────────┤
│  SQLite (WAL)          外部プロセス:             │
│                        cd-paranoia / ffmpeg /   │
│                        yt-dlp                   │
└─────────────────────────────────────────────────┘
```

### 技術スタック

| 領域 | 選定 | 備考 |
|---|---|---|
| 言語 | Rust (edition 2021+) | タグ書き込み・EBU R128・デコードが揃う唯一の現実解 |
| HTTP | axum + tokio + tower-http | |
| DB | rusqlite (bundled) + FTS5 | 書き込みは単一コネクション、読み込みはプール |
| タグ | lofty | FLAC / Opus / MP4 / WAV を一貫 API |
| デコード | symphonia | FLAC/ALAC/AAC/WAV。**Opus のデコードは非対応 → ffmpeg 経由**（demux は Opus も symphonia で可。`audio_fp` はこれを使う） |
| ラウドネス | ebur128 | libebur128 の純 Rust 移植 |
| 正規表現 | fancy-regex | 後方参照・先読みが必要（ytmusic パーサ） |
| HTTP client | reqwest | MusicBrainz / CTDB |
| FFT | rustfft | 偽ハイレゾ検出（P3、任意） |
| ファイル操作 | rustix | `openat2(RESOLVE_BENEATH \| RESOLVE_NO_SYMLINKS)`、dirfd 基準の open / rename |
| フロント | React + TypeScript + TanStack Table/Virtual | |
| 外部バイナリ | cd-paranoia, cdrdao, ffmpeg, yt-dlp, flac | コンテナイメージに同梱 |

---

## 5. ライブラリレイアウト

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
│   └── <Category>/<AlbumArtist>/<Album>/1-01 Title.opus
├── Archive/  → /mnt/hdd/media/Archive [dataset, hdd] snapshot: 週次
│   └── <Category>/<AlbumArtist>/<Album>/Title.webm
│       （FLAC 正規化で退避した WAV / ALAC / AIFF もここ。GC まで保持）
├── Inbox/                             [dataset] snapshot: なし
│   └── （承認前の一時領域。ハイレゾ購入分などをここへ置く）
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

### パステンプレート

```toml
[layout]
multi_disc  = "{category}/{albumartist}/{album}/{disc}-{track:02} {title}"
single_disc = "{category}/{albumartist}/{album}/{track:02} {title}"
unsorted    = "_Unsorted/{albumartist}/{album}/{track:02} {title}"
```

- 階層は Artist ではなく **AlbumArtist**。コンピレーションは `Various Artists`
- マルチディスクはサブフォルダを作らず `1-01` 前置き（アルバム = 1ディレクトリを維持）
- 発売年はパスに含めない。DATE / ORIGINALDATE タグは必ず保持する
- パス衝突時のみ自動降格: `{album}` → `{album} ({year})` → `{album} ({edition})`
- **衝突 = マージではない。** 配置前に MusicBrainz Release ID または DiscID で
  同一リリース判定を行い、異なる場合は必ず別ディレクトリにする

### ファイル名正規化

ytmusic の foo_fileops 互換置換テーブルを継承し、以下を追加:

```
~ → ～   * → ＊   ∕ → ／   : → ：   > → ＞   < → ＜   ? → ？
Ø → O   À → A   ô → o   è → e   é → e   ë → e   ゔ → う
```

- タグ値は NFC 正規化のみ（原文字を保持）、ファイル名にのみ置換テーブルを適用
- 表に無い禁止文字 `/` `\` `|` `"` も同じ流儀で全角化（`／` `＼` `｜` `＂`）、制御文字は除去（D-43）
- SMB 制約: 末尾のドット・スペース禁止（削る）、予約名 (CON, PRN, AUX, NUL, COM1-9, LPT1-9) 回避
  （`_` を後置: `CON` → `CON_`）。空になった要素は `_`
- Android 側 exFAT 制約: パス長上限、`|` `"` 禁止
- **各コンポーネント**は 255 バイト以下（ZFS / SMB の上限。パス全体の上限ではない）。
  パス全体は Windows / Android 互換のため **240 文字（UTF-16 単位）**を上限とし、
  超える場合はタイトル部を省略記号付きで切り詰める
- 値が無いときのフォールバック: `albumartist` → `artist` → `Unknown Artist`、`album` →
  `Unknown Album`、`title` → 現在のファイル名、`track` → 0、`disc` → 1（D-43）
- テンプレートの選択: category が無ければ `unsorted`、複数ディスク（`disc_count > 1` または
  構成トラックの `disc_no` の最大が 2 以上）なら `multi_disc`、それ以外は `single_disc`

### パスの表現と境界

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
してから prefix を比較する方式は symlink の差し替え（TOCTOU）を防げない（D-31）。
本番は Linux 5.6+ を前提とし、`openat2` が使えなければ**起動時に失敗**する（fail-fast）。
非 Linux の開発機では実ファイルを触る結合テストを skip し、Linux の CI で走らせる。
`canonicalize` + prefix 比較へのフォールバックは設けない。

- スキャナは symlink を**辿らない**（ディレクトリもファイルも）。見つけたら対象外として
  一覧に出す。hardlink（`nlink > 1`）は inode も md5 も同一性に使わず、`rel_path_key` だけで
  解決して警告バッジを出す（D-26）
- SMB / exFAT 制約に反する名前（禁止文字・末尾ドット/スペース・予約名）は**スキャナが
  対象外にする**。したがって移行前の preflight でもブロッカーにする（§14、P0-14）
- 書き込みの一時ファイルは**対象と同じディレクトリ**に `.spindle-tmp-<random>` で
  `O_EXCL` 作成し、fsync → rename。取り残された `.spindle-tmp-*` はスキャンが回収する
- 外部コマンドは `sh -c` を使わず引数配列で起動する。パスは対応するツールでは `--` の後、
  非対応なら `./` を前置して先頭 `-` を無害化する。可能ならファイルを開いて
  stdin / stdout で渡す。タイムアウト・終了コード検査・stderr のログ出力・
  プロセスグループごとの kill を必須にする（規約）

### Category

- **パス決定に使う `category` と、ファイルに書く `GENRE` タグは別フィールド**
  - GENRE は多値を取り得るがパスは単一値
  - MusicBrainz 由来のジャンル文字列は表記揺れが激しい
  - ジャンルは後から変わる。パス直結だと数千ファイルが動く
- `categories` テーブルで統制語彙として管理、`genre_category_map` で自動推定
- 語彙にマッチしないものは `_Unsorted/` に配置し、取り込みを止めない
- ytmusic 由来のレーベル軸カテゴリ（神椿Studio 等）も同じ語彙表に混在させてよい

---

## 6. データモデル

`db/migrations/0001_init.sql` を参照。主要な設計判断のみ以下に記す。

### 同一性解決の優先順位

1. `(dev, inode)` — 外部の rename や in-place のタグ書き換えでは inode は変わらない。
   spindle 自身の tagwrite（tmp + rename、§7.5）は inode を変えるので、成功トランザクションで
   DB を追随させる
2. `audio_md5` — FLAC は STREAMINFO に非圧縮音声の MD5 を持つ。タグ変更で不変。
   ALAC/WAV はデコードして算出。非可逆は算出しない（識別子としては使わない）
3. `rel_path_key` 一致
4. いずれも当たらなければ新規トラック

**どの識別子も「トラック実体の一意 ID」ではない**ので、各段で候補を検証する（D-30）。
そのために走査は**多相**で行い、判定は inventory 全体が揃ってから下す（§7.1）:

| 段 | 候補の採用条件 | 外れる例 |
|---|---|---|
| inode | inventory 内でその `(dev, inode)` を持つパスが **1 つだけ**（2 つ以上なら hardlink とみなし、全候補で inode 段を無効化）、候補行が未 claim、`nlink = 1`、かつ `size` か `mtime_ns` のどちらかが一致。両方違えば `audio_md5` を計算して一致を要求 | inode 再利用（削除後に別ファイルが同じ inode を得る）、hardlink |
| inode（dev の付け替え） | 同じ `dev` の候補が無いとき、**別の `dev` に同じ `inode` を持つ行がちょうど 1 つ**あり、`size` / `mtime_ns` / `ctime_ns` が**すべて**一致。同じ実体とみなし「変更なし」で `dev` だけ現在値へ直す（D-62） | 属性が 1 つでも違う（inode 再利用と区別できない）、候補が複数 |
| audio_md5 | 候補が**ちょうど 1 行**で、その行の `rel_path_key` が **inventory 全体に存在しない**（= 移動元が消えている。走査途中の未訪問ではなく、全 stat が終わった後に判定する） | コピー元が残っている、ベスト盤との重複、無音トラック |
| rel_path_key | 候補が未 claim | — |

- claim は `scan_runs.id` 単位（`tracks.seen_run_id`）。`seen_at` は時刻で、claim 判定には使わない
  （epoch 秒では同秒の再実行と区別できない）
- 複数のパスが同じ候補行を取り合ったら `rel_path_key` の昇順で先のものが取り、後は次の段へ落ちる。
  順序が固定なので並列に stat しても結果は決定的
- audio_md5 の候補が複数、または元パスが inventory にまだ存在する場合は**移動ではなく新規トラック**
  として登録する。自動マージはしない。同一 `audio_md5` の active 行が 2 つ以上あるものは
  `duplicate_groups` ビューに現れ、UI がバッジと一覧で示す（D-29）
- `nlink > 1`、または inventory 内で同じ inode を複数パスが持つファイルは **inode 段と md5 段を飛ばし
  `rel_path_key` だけで解決**し、警告バッジを出す。移行時は preflight のブロッカー（D-26）
- **復活**: どの段で採用されても、候補行に `missing_since` があれば NULL に戻す
- **missing の確定**: `scan_runs.state = 'completed'`（root を開けて walk がエラーなく完了）の finalize
  でだけ、その run で claim されなかった active 行に `missing_since` を立てる。途中失敗・キャンセルの
  run では立てない（SMB 一時切断で全曲が missing になる事故を防ぐ）

### 変更検出と版の遷移

音声の同一性を見るフィンガープリントは形式で分ける:

| 形式 | 音声フィンガープリント | 用途 |
|---|---|---|
| FLAC | STREAMINFO の MD5（`audio_md5`） | 同一性（移動検出）と音声版 |
| ALAC / WAV | デコードした PCM の MD5（`audio_md5`） | 同上 |
| 非可逆（opus / aac / mp3 / ogg） | エンコード済みパケット列の SHA-256（`audio_fp`。symphonia で demux のみ、デコードしない。Opus も同様） | **音声版のみ**。同一性には使わない |

| 観測 | 判定 | 版 |
|---|---|---|
| `(dev, inode, size, mtime_ns, ctime_ns)` すべて一致 | 変更なし | `seen_at` / `seen_run_id` のみ |
| `dev` だけ違う（inode 段の「dev の付け替え」で採用） | 変更なし | `dev` を現在値へ直し、`seen_at` / `seen_run_id` |
| いずれか変化 → タグを読み `tag_hash` を再計算、音声フィンガープリントを再計算 | どちらも同じ | 版は動かさない |
| 〃 | `tag_hash` のみ変化 | `tag_version++` |
| 〃 | 音声フィンガープリントのみ変化 | `audio_version++` |
| 〃 | 両方変化 | 両方 `++` |
| spindle 自身の tagwrite / rename / RG 書き込み / MD5 補填（§7.9） | 音声は不変と**既知** | フィンガープリントを再計算せず `audio_version` 据え置き（tagwrite は `tag_version++` 済み） |

非可逆のタグ書き換えはコンテナサイズを普通に変えるので、`size` の変化を音声の変化と
みなしてはならない（不変条件 3）。パケット列のハッシュは demux だけで済み、外部変更が
あったファイルにしか走らない。

`ctime_ns` を含めるのは、mtime を保存して書き戻すタグツールや `cp -p` を検出するため。
ZFS のスナップショット rollback は ctime も戻すので検出できない。これは **deep scan**
（全ファイルの `tag_hash` と音声フィンガープリントを再計算。`[scan].deep_interval_days` 既定 30 日、
UI から手動実行可）で吸収する。rollback を行ったら deep scan を手動で回すこと（運用手順）。

`tag_version` は「同じ値の書き直し」で進めない。tagwrite の再実行や外部ツールによる
同値の再保存で Derived の追随が空回りしないため。

### 出自の3属性（直交）

| 属性 | 値 |
|---|---|
| `source_type` | cd_rip / download / youtube / unknown |
| `lossless` | コーデックから自動判定 |
| `verification` | verified_ar / verified_ctdb / mismatch / **unverifiable** / not_attempted |

`unverifiable` はハイレゾ・配信音源など TOC が存在しない音源。
「未検証」とは意味が異なるため UI でも別バッジとする。

### 版管理

- `audio_version`: 音声バイト列が変わったら ++ → Derived 再エンコードが必要
- `tag_version`: タグのみ変更で ++ → Derived はタグ上書きのみ

`derived_files` に生成時の両版番号を記録し、差分で必要な処理を判定する。
数千件の一括タグ編集後に再エンコードが走らないための中核機構。

配布ビュー `delivery` は Derived の存在だけでなく**版の一致**を見る:

| 状態 | 配布するもの |
|---|---|
| `src_audio_version = audio_version` かつ `src_tag_version = tag_version` | Derived |
| 音声版は一致、タグ版のみ不一致 | Derived（`stale_tags = 1`。タグ上書きジョブが追随する） |
| 音声版が不一致（再エンコード待ち） | **Library 原本へフォールバック** |

音声が古い Derived を配ると、修復・差し替え前の音声を正常品として配ることになる。
タグだけ古い場合に原本へ落とさないのは、容量の大きい可逆を配布してしまうため。

### 全文検索（FTS5）

`tracks_fts` は external content 方式（`content='tracks'`）。索引対象の
`title / artist_display / album / albumartist` は**すべて `tracks` に実在する列**で
なければならない（列値の取得と `rebuild` が content 表を直接読むため）。
このため `tracks.album` を `albums.album` の複製キャッシュとして持ち、トリガで同期する。
FTS の更新トリガは索引対象列の `UPDATE OF` にだけ張る。`seen_at` 更新のたびに
削除・再挿入が走ると、スキャンの最速パスが FTS 書き込みで律速される。

### 論理削除

`missing_since` による論理削除。SMB 一時切断やスキャン中のマウント欠落で行を物理削除すると、
プレイリストと編集履歴が巻き添えになる。既定 30 日経過後に GC（`gc` ジョブ。missing トラック・
アルバムの行、`Archive/` の退避ファイル、Derived の孤児、アートワークの孤児を回収する。
1 日 1 回自動、`POST /api/gc` で手動、`GET /api/gc/preview` が dry-run。D-56）。

### ReplayGain の内部表現

- 内部は **RG 2.0 / -18 LUFS 基準の dB 値**で一元管理
- 書き出し時にフォーマットごとへ変換:
  - Opus: `R128_TRACK_GAIN` / `R128_ALBUM_GAIN`（Q7.8 固定小数、**-23 LUFS 基準**）
  - FLAC / Ogg: `REPLAYGAIN_TRACK_GAIN` 等の Vorbis Comment
  - MP4 (AAC/ALAC): RG 互換タグ
- Opus への変換式（`G18` = 内部の dB 値 = `-18 - 測定 LUFS`）:
  `R128 値 = round((G18 - 5.0) * 256)`、符号付き 16bit に飽和（-32768..32767）。
  基準が 5 dB 低いので**必ず減算**する。テストベクトル: 測定 -18 LUFS → `G18 = 0`
  → `R128 = -1280`。測定 -23 LUFS → `G18 = +5` → `R128 = 0`。
  `OpusHead` の output gain は 0 のまま触らない（合成すると再解析時に二重に掛かる）
- `rg_scanned_at` と `rg_written_at` を分離。数万件のスキャン後に書き込みが中断しても
  再スキャンなしで書き込みのみ再開できる
- **タグ書き込みは通常の編集バッチ**（`POST /api/rg/write { selection }` → tags op。旧値の
  記録・overlay・巻き戻しはタグ編集と同じ）。書くキーは形式ごとに固定で、値の無いキーは消す
  （Opus: `R128_TRACK_GAIN` / `R128_ALBUM_GAIN` を書き `REPLAYGAIN_*` を消す。他形式:
  `REPLAYGAIN_TRACK_GAIN` `+0.00 dB` / `REPLAYGAIN_TRACK_PEAK` `0.000000`（線形、true peak）/
  `REPLAYGAIN_ALBUM_*`。album の値が無いトラックは album のキーを消す。D-48）
- **`rg_written_at` は「ファイルの RG タグが解析値と一致していると確認した時刻」**。書き込み
  バッチの applied だけでなく、DB をファイルの現在値へ揃えるすべての経路（overlay の解消・
  外部変更の追随・巻き戻し）で判定し直し、一致しなければ NULL に戻す。再解析で値が変わった
  行も NULL にする（秒単位の時刻では同じ秒の再解析を `<` で検出できない）。DB のタグが既に
  一致している行は op にせず `rg_written_at` だけ立てる。スキャナも外部のタグ変更を取り込んだ
  行（`tag_hash` が変わった行）で判定し直す（D-48）
- **外部で音声が差し替わった（`audio_version` が進んだ）行の解析値は捨てる**（`rg_*` と
  `rg_scanned_at` / `rg_written_at` を NULL。スキャナと tagwrite の overlay 解消の両方。D-47）。
  古い解析値を Derived や再生に使わない。album の他のトラックの `rg_album_*` は次の album 解析で揃う
- album gain は `album_id` 単位。**2ch 以外は album 集計から除外**（判定はデコード結果の
  チャンネル数。除外されたトラックの album の値は NULL）。構成トラックが 1 本でも
  デコードできなければ album 全体を書かない（D-47）
- 無音（絶対ゲート以下、積分ラウドネス `-inf`）の gain は 0 dB

---

## 7. パイプライン

### 7.1 スキャン

走査は 4 相。同一性の判定は inventory 全体が揃ってから行う（走査途中では「移動元が消えた」
を判定できない）。DB への書き込みは最終相で単一 writer トランザクションにまとめる。

```
scan_runs に行を作る（state='running'）
Phase 1  inventory:  walk(Library) を並列に stat（symlink は辿らず一覧へ）。
                     (rel_path_key, dev, inode, nlink, size, mtime_ns, ctime_ns) をメモリに固定
Phase 2  candidates: inventory の各エントリと既存行の候補を、inode → audio_md5 → rel_path_key の
                     順で列挙（§6 の採用条件。md5 段の「移動元が消えている」は inventory 全体で判定）。
                     解決が要求しうる audio_md5（inode 一致で size も mtime も違う行が md5 を持つ /
                     移動候補があるときの未決エントリ）だけを先に並列で計算し、進捗を出す。初回や
                     移動の無い増分では要求が無く、可逆のデコードは Phase 3 に回る（D-50）
Phase 3  claim:      rel_path_key 昇順で決定的に採用。取り合いに負けたエントリは次の段へ。
                     変更ありのエントリはここでタグ読込・フィンガープリント計算（並列）
Phase 4  commit:     1 トランザクションで
                       a. 採用行の rel_path / rel_path_key を「予約済み一時 key」へ全件移す
                       b. 最終値へ全件移す（swap rename A↔B や循環でも UNIQUE を踏まない）
                       c. 物理属性・タグ・版・seen_run_id・復活（missing_since=NULL）を反映
                       d. 新規行の挿入
                       e. album 照合（下記）。album の rel_dir も同じ 2 段階で更新
                       f. state='completed' なら、この run で claim されなかった active 行に
                          missing_since を立てる。failed / cancelled では立てない
                     FTS はトリガで追随
```

**アルバムの照合**（ディレクトリ rename で album id を失わないため。D-32）:

```
ディレクトリ D に今回見つかったトラック集合 T(D) について
  1. T(D) の行が直前まで属していた album を数える
  2. mb_release_id / discid が一致する既存 album が**ちょうど 1 つ**で、その旧 rel_dir が
     inventory に無ければそれ（複数一致なら自動では寄せない → 3 へ）
  3. なければ、T(D) の過半数が属していた album A で、A の旧 rel_dir が inventory に無いもの
     → A.rel_dir を D に書き換える（id 維持。verifications / artwork が残る）
  4. どれにも当たらなければ新規 album
分割（一部だけ別ディレクトリへ）: 移った側は新規 album、残った側は id 維持
統合（2 ディレクトリが 1 つに）: 最多の album が id を維持、他は構成 0 になり
  albums.missing_since を立てる（行は消さない。GC が回収）
```

- 初回フルスキャンは並列度 = CPU コア数。以降は最速パス（§6）で大半を飛ばす
- 外部（foobar2000 等）による書き換えを検出したら DB を上書きする。
  **ファイルが常に勝つ**（原則1）
- **例外: ファイル反映待ちのトラック（`edit_ops.result = 'pending'` がある）は、
  pending の op が所有する論理フィールドを再評価しない。** 一括編集は DB を先に更新し、
  ファイルへの反映は非同期なので（§7.5）、この窓で「ファイルが勝つ」を適用すると
  編集が DB から巻き戻され、後続の tagwrite と食い違う。
  ただし**物理的な所在は pending 中も常に追随する**: `(dev, inode)` で見つけた
  ファイルの `rel_path` / `size` / `mtime_ns` / `ctime_ns` は更新する。外部 rename は inode も
  mtime も変えないので、これを止めると tagwrite が旧パスを開けなくなる。
  抑止するのは `tags` op ならタグとキャッシュ列・`tag_version`、`rename` op なら
  `rel_path`（外部 rename と衝突したら op を `skipped_conflict` にし、同じトランザクションで
  `rel_path` を実在パスへ追随させる。D-43）だけ。
  外部 rename の**宛先が別の行（missing 行を含む）に占有されていた**場合は、Phase 4 で
  その行の missing 確定と key の入れ替えを同じトランザクションで行う
- 走査対象は Library のみ。Derived / Archive はスキャンしない

**アートワーク**（Phase 5、commit の後。D-49）:

- album のアートワークは、ディレクトリの**同梱カバー画像**（`cover` / `folder` / `front` ×
  `jpg` / `jpeg` / `png` / `webp`。名前 → 拡張子の優先順、大文字小文字は区別しない）があれば
  それ、無ければ**構成トラックを `disc_no` / `track_no` / `rel_path` 順に見て最初に見つかる埋め込み
  画像**（front cover を優先）。形式はバイト列のヘッダで判別し、拡張子やタグの MIME は信用しない
- 画像は SHA-256 でハッシュアドレスし（`artwork` 表。行は消さない）、原画像を**元の形式のまま**
  `<data>/thumbs/<hex>/orig.<ext>` に置く。サムネイル（`<size>.webp`、256 / 768。長辺、拡大なし）は
  `thumbnail` ジョブが ffmpeg で作る。Library には何も書かない
- Phase 4 は行が変わったトラックの現在の album と直前まで属していた album の
  `albums.artwork_resolved_at` を NULL にして再解決を**予約**する（同じトランザクション）。
  Phase 5 が解決し直すのは deep なら全 album、それ以外は「予約された（NULL）」「同梱画像の有無・
  stat（inode / size / mtime / ctime。`albums.cover_*`）が前回と違う」「参照中の原画像がキャッシュに
  無い」album。missing の album は触らない。決められない album（I/O 失敗、読んでいる間に同梱画像が
  変わった、構成トラックを読めない）は状態を動かさず次回やり直す。同梱画像が画像として認識できない
  ときは埋め込みへ倒し、stat は記録する（変わるまで読み直さない）
- Phase 4 の commit 後なので、Phase 5 の cancel / 失敗は run の状態（completed）と missing の確定を
  戻さない。予約が残るので次のスキャンで続きを行う
- **トラック自身の画像**（D-61）: Phase 3 が各トラックの埋め込み画像（front cover 優先）を読んで
  キャッシュへ置き、Phase 4 が `tracks.artwork_id` に記録する（無ければ NULL）。tagwrite の書き戻しも
  同じ。変更なしの行は読まないので、既存行は deep scan で埋まる

### 7.2 CD 取り込み

```
[ディスク検出]  CDROM_DRIVE_STATUS ioctl を 2 秒間隔ポーリング
   ↓            （udev がコンテナに届かないため）
[TOC 取得]      cdrdao read-toc / SG_IO READ TOC
   ↓
[ID 算出]       MusicBrainz DiscID / AccurateRip id1,id2 / FreeDB ID
   ↓            すべて TOC からの整数演算。libdiscid FFI 不要
[メタデータ照会] MusicBrainz → 候補提示 → ユーザ確認・手動補正
   ↓            DiscID で ws/2/discid を引き、無ければ同じ TOC で fuzzy に引く。候補は
   ↓            「リリース × medium」（DiscID を持つ medium は exact、トラック数の合う medium は近似。D-64）
   ↓            ※同人・VTuber・インディーズ国内盤は MusicBrainz 未登録が常態。
   ↓              照会結果ゼロでもウィザードが完走できることを必須要件とする。
   ↓              候補も手入力も同じフォーム（候補を写して直す。D-65）に収束し、確定で
   ↓              DiscMetadata（album / album_artist / date / label / catalog_number / barcode /
   ↓              disc_no / disc_count / tracks[{ number, title, artist, mb }]、source）になる。
   ↓              トラックリスト貼り付け（通販ページ等からのテキストを行解析して
   ↓              トラック番号・タイトル・アーティストへ割り付け。web/src/lib/tracklist.ts）を
   ↓              一級の入力経路とする。行は TOC の音声トラックと 1:1 で、番号と長さは TOC から
   ↓
[オフセット決定] INQUIRY でドライブ型番取得 → 同梱オフセット表を引く
   ↓            UI に必ず表示。手動上書き可
[吸い出し]      cd-paranoia '1-' - で全ディスクを1本の PCM として取得
   ↓            ※トラック単位で吸うとオフセット補正が境界をまたげない
[オフセット適用 → トラック分割]
   ↓
[CRC 計算]      ARv1 / ARv2 / CTDB CRC32
   ↓            先頭トラック冒頭・末尾トラック終端の除外規則を厳守
[照合]          CTDB を主、AccurateRip を補助
   ↓
[エンコード]    FLAC (master) を Library へ。tmp の PCM は検証完了まで保持
   ↓
[ログ出力]      rip.log / disc.cue / disc.toc をアルバムディレクトリへ
   ↓
[後続ジョブ投入] rg → transcode(Derived) → thumbnail
```

配置（`src/cd/place.rs`、D-67）: rip ジョブの最終段。PCM を `TrackLayout` で切り、raw のまま
`flac -8 --verify` でエンコード（PCM の MD5 と STREAMINFO の MD5 が一致するときだけ成果物）、
タグは確定フォームの写像（D-65）+ `TRACKTOTAL` / `MUSICBRAINZ_DISCID`。パスは `[layout]` の
テンプレート（確定フォームで選んだ category。無ければ `_Unsorted`）で `pathgen::plan` に通し、
複数枚組の 2 枚目以降は宛先の同名 album に合流する。`job_mutexes` の `library` を取って
tmp → fsync → `RENAME_NOREPLACE` で置き、1 トランザクションで `albums` / `tracks`
（`source_type = 'cd_rip'`、`verification`）/ `track_tags` / `album_verifications`（`source = 'rip'`）/
`track_verifications` を登録して `rg` と `transcode` を投入する。再実行は MD5 で自分の成果物を
見分ける（宛先のファイル、スキャナが先に拾った行）。同梱ファイルは 1 枚なら `disc.cue` /
`disc.toc` / `rip.log`、複数枚組は `disc<N>.cue` / `disc<N>.toc` / `rip<N>.log`。`disc.cue` は
EAC 流の複数ファイル cue（ギャップは前トラック末尾。INDEX 00 は書かない）、`disc.toc` は `Toc` から
cdrdao 構文で生成、`rip.log` は先頭行 `spindle rip log v1` の自前形式でドライブ・オフセット・
トラックごとの CRC と照合結果を持つ。スキャナはこの rip.log のあるディレクトリで新規に登録する行を
`cd_rip` にする（DB を消しても出自が戻る。検証は §7.3 で付け直す）。

同梱ファイル（cover / disc.cue / disc.toc / rip.log）は album 全体の一括リネームに追随する: rename
ジョブが phase 2 の後、commit トランザクションの中で既知の名前のファイルを新ディレクトリへ移し、空になった
旧ディレクトリを消す（D-67。`edits` には記録せず、巻き戻しは逆向きの移動で戻る）。

不一致時の既定動作: 自動再リップ（最大2回）→ CTDB 修復データ適用 →
それでも不一致なら `mismatch` フラグ付きで取り込み、UI で要確認表示。

CTDB の修復（`src/cd/repair.rs`、D-66）: ディスクの 16 bit 語を 11760 語ずつの行に並べ、列ごとに
GF(2^16) の Reed-Solomon 符号語とみなす（データ行は先頭 1 行と末尾 1 行 + 端数を除く。CTDB の
ディスク CRC の範囲と同じ）。1 回目の走査でシンドローム表（`SyndromeSampler` → `SyndromeTable`。
80 分で 3〜5 秒）を作り、エントリの `syndrome`（列 0）でオフセットを探す（`find_offset`、±2939）。
誤りがあれば `hasparity` のパリティファイル（各列のシンドローム。`CtdbClient::fetch_syndromes` が
Range で先頭 npar 面だけ取る）と突き合わせ、列ごとに Berlekamp-Massey → Chien → Forney で位置と値を
出す（列あたり npar/2 個まで。1 セクタ丸ごとの傷は 1176 列に 1 個ずつなので直る）。直した後の
ディスク CRC がエントリの値に一致するときだけ計画（`RepairPlan`）を採用し、2 回目の走査で
`RepairApplier` が語を XOR する。直せなければ再リップ / `mismatch` へ。吸い出しジョブへの配線は
P2-5 で行う

**ドライブは物理的に1台なので rip キューの並列度は 1 に固定する。**

### 7.3 遡及照合（ログなし既存 FLAC）

CUETools の "verify from files" 相当。

```
アルバムをディスク（disc_no）ごとに分け、全トラックが揃っているか確認
  → 各トラックのサンプル数（STREAMINFO）から TOC 再構成
     offset = 150 + Σ(前トラックのセクタ数)
  → AccurateRip DiscID / MusicBrainz DiscID を算出
  → デコードして CRC 表を作る（1 回流すだけで ±2939 サンプルの全オフセットの CRC が出る）
  → CTDB / AccurateRip 照会 → オフセットを探して照合
  → verify.log 出力（data/verify/<album_id>.log。rip.log とは別物）
```

`verify` ジョブ（album 単位、並列 2。`POST /api/verify { selection }`）。読むだけでファイルは
書かない。ファイルは root の FD を fstat して DB の行と照合してから読む（D-62）。

適用条件:

- **44.1kHz / 16bit / 2ch の FLAC のみ。** それ以外を含むディスクは `unverifiable`
  （TOC が存在しない音源。品質の劣後ではない）
- 各トラックのサンプル数が **588 の倍数**（1セクタ）であること。
  端数があれば CD 由来でないか加工済みと判定して `unverifiable`
- トラック番号が 1 から連続していること（不完全なディスクは TOC を作れないので何もしない）
- **オフセットは探す**（D-63）。DB に登録された値は他人のドライブで吸ったもので、読み取り
  オフセットの補正が違えば同じ盤でも数十サンプルずれる。CUETools と同じ ±(5×588−1) の範囲で
  「一致したトラック数 → 信頼度の和 → 0 に近い」順に 1 つ選び、`detected_offset` に残す
  （ディスクで 1 つ。トラックごとに別のオフセットは採らない）。AccurateRip v2 はオフセットに
  対して線形でないので 0 だけ、それ以外は v1 で比べる
- CTDB は `fuzzy=1` で別リリースも返るので、音声部分の長さと音声トラック数が同じエントリだけを
  候補にする。AccurateRip の ID は CUETools / dBpoweramp 式（Enhanced CD でも実リードアウト）
- トラックごとに: CTDB 一致 → `verified_ctdb`、AccurateRip だけ一致 → `verified_ar`、
  候補はあるが不一致 → `mismatch`、どちらにも候補なし → 据え置き（`not_attempted`）。
  結果は `album_verifications`（手法 × ディスク。履歴として積む）と `track_verifications`
  （自分の CRC と一致の有無）に残す
- 照会に失敗したらジョブを失敗させて再試行し、何も記録しない（不一致を「照会できなかった」で
  汚さない）。記録は 1 トランザクション（ディスク × 手法の行、トラックの行、`tracks.verification`）で、
  照合を始めたときの `audio_version` が 1 本でも進んでいれば何も書かない。verify.log は tmp に書き、
  トランザクションの中で本来の名前に rename してから commit する。`album_verifications.job_id` で
  同じジョブの再実行（commit の後に落ちた場合）を見分け、何もしない（履歴もログも初回のまま）
- **不一致は不良を意味しない。** ギャップ処理差、隠しトラック、データトラックの存在で
  普通に外れる。`mismatch` は「要確認」として扱い、警告色で表示しない

### 7.4 ロスレス正規化（WAV / ALAC / AIFF → FLAC）

Library のロスレスは FLAC に統一する（D-45）。移行で取り込んだ ALAC も、取り込まれた WAV /
AIFF も、同じ機構で FLAC にする。

```
POST /api/normalize/preview → /apply（selection。rename と同型。D-46）
  → edit_batches + edit_ops(kind='archive', pending) + edits(rel_path 旧→新, codec 旧→'flac')
    を記録し、track 単位の normalize ジョブを投入。DB は先行更新しない
normalize ジョブ（track をロック）
  → 事前条件（stat + tag_hash）を確認。外れていれば skipped_conflict
  → symphonia でデコードして PCM MD5 算出
  → ffmpeg でデコード → flac -8 --verify でエンコード（data/tmp）。タグと画像は lofty で移す
  → 生成 FLAC の STREAMINFO MD5 と突き合わせ
     ├ 一致   → Library の同じディレクトリに tmp → fsync → RENAME_NOREPLACE で <name>.flac を置く
     │          → 元ファイルを同じディレクトリの一時名 spindle-normalize-<op_id>.<ext> へ退避
     │            （inode を確認。外部の tmp + rename と分離する）
     │          → Archive/ の同じ相対パスへ実コピー（SHA-256 を照合し、edits に記録）
     │          → unlink の直前に同じ FD の stat とバイト列を再照合 → 一時名を unlink
     │          → 1 トランザクションで op を applied、rel_path / 物理属性 / codec を追随、
     │            archived_files に台帳（eligible_after = now + retention）を追加、
     │            original_codec（'wav' | 'alac' | 'aiff'）、normalized_at 記録。
     │            audio_md5 は同じ PCM なので変わらず、audio_version も上げない（Derived は据え置き）。
     │            tag_version は写したタグの tag_hash が元と違うときだけ進める
     └ 不一致 → op は failed、元ファイルを残す（生成した FLAC は捨てる）
```

宛先は拡張子を `.flac` に置き換えた同じパス。宛先を別のトラックが占有していれば計画の時点で
conflict。反映の直前に宛先へ別のファイルが現れていれば `skipped_conflict`（音声 MD5 が期待値と
一致するときだけ自分の成果物とみなして続きを行う。クラッシュ後の再投入も同じ判定）。
スキャナは pending の archive op の元・一時名・宛先のパスを「作業中」として扱い、新規登録も
missing 判定もしない。unlink の前に元ファイルが外部で更新・差し替えされていれば conflict にして
元パスへ戻す（自分が置いた宛先と Archive のコピーは消す）。cancel は Library を触る前まで
（変換中の子プロセスは止める）。

WAV は RIFF INFO / ID3 のどちらを使うかがソフトごとに異なり、ReplayGain タグの
互換性も低い。ALAC（MP4 ilst）は複数値タグが弱く、Safari 以外のブラウザで再生できない。
可逆変換なので情報は失われない。読み込み互換のため WAV / ALAC / AIFF の再生・
取り込み自体は引き続きサポートする。

**元ファイルは即時削除しない。** 物理削除はユーザデータ全般と同じく GC ジョブのみが行う
（禁止事項）。退避した元ファイルは `archived_files`（state='held'）を台帳として GC が
`eligible_after` 経過後に回収し state='deleted' にする。それまでは履歴の巻き戻しで
Library へ戻せる: 元ファイルは Archive から Library へ**コピー**で戻し（Archive の実体は残る。
state='restored'）、Library にあった FLAC は Archive へ move して台帳に `reason='restore'` の行を
足す（GC の対象）。やり直し（逆バッチの revert）はその逆で、同じパスの台帳行は作り直さず `held` に
戻す。復元元が GC 済みの op は `skipped_conflict`（D-46）。
編集履歴は revert / redo で状態が動くので GC の台帳には使わない。
Archive の「追記のみ」の例外はこの GC だけ。

PCM MD5 の一致が保証するのは**音声サンプルの同一性だけ**で、WAV の RIFF INFO /
ID3 / 未知チャンク / コンテナのバイト列は FLAC から再生成できない。保持期間後に
コンテナを不可逆に捨てるのは意図した決定（D-10）。

### 7.5 タグ書き込み（編集バッチ）

```
編集バッチ確定
  → プレビュー（dry-run）。サーバは selection のスナップショット（対象 track_id と各行の
    tag_version / 事前条件）を作り selection_token を返す（§9、D-33）
  → 適用は selection_token の集合だけを対象にする。preview 後にスキャンで増えた行は含まれない
  → 対象トラックに pending の op があれば 409 で拒否（件数と track_id を返す）。
    `skip_pending: true` なら該当トラックを除外して続行（UI の「除外して適用」）
  → 1 トランザクションで:
       edit_batches(state='prepared')
       edit_ops(result='pending')  … トラック 1 本につき 1 op。事前条件
                                    (expected_dev / inode / size / mtime_ns / ctime_ns /
                                     tag_hash / rel_path) を記録
       edits                      … op 配下にフィールドごとの旧値・新値（JSON）
       → DB（track_tags / キャッシュ列）を新値へ更新、tag_version をトラックごとに 1 回 ++
       → track 単位の tagwrite ジョブを投入（edit_batch_id で紐づけ）
  → 各 tagwrite ジョブ（並列 4）:  バッチを state='applying' に
       rel_path で open → その FD の fstat で inode/size/mtime_ns/ctime_ns を確認
       （dev は照合しない。D-62）、同じ FD からタグを読んで tag_hash を確認
         ├ いずれか不一致 → result='skipped_conflict'。ファイルは触らない。
         │                  同一トランザクションでその FD の内容から DB を戻す（下記「overlay の解消」）
         ├ rel_path のみ不一致（外部 rename。スキャナが inode で新パスを追随済み）
         │     → 新パスの rel_path_key が他の行に占有されていなければ tags op は追随して続行。
         │       占有されていれば skipped_conflict。rename op は常に skipped_conflict。
         │       rename は ctime を進めるので、このとき ctime_ns だけの不一致は許容する（D-41）
         └ 一致 → その FD の親ディレクトリに tmp を O_EXCL で作成 → 内容をコピー →
                  op 配下の全フィールドを lofty で書き換え → fsync → rename →
                  同一トランザクションで (dev, inode, size, mtime_ns, ctime_ns, tag_hash) 更新、
                  op を result='applied'
  → 全 op が終端になったらバッチを集計: applied / partial / failed / cancelled
  → Derived のタグ上書きジョブを投入（tag_version の差分で判定）
```

- **ファイル反映の単位は op（トラック）。** フィールドごとに tmp+rename すると
  1 件目で inode が変わり 2 件目以降の事前条件が必ず外れる。同一トラックの全差分を
  1 回の書き込みで反映し、`tag_version` はトラックごと・バッチごとに 1 回だけ進める。
  結果（`result`）は op にだけ持ち、**フィールド単位の部分適用はしない**
  （1 フィールドでも事前条件に合わなければ op 全体が conflict）
- **事前条件に `ctime_ns` と `tag_hash` を含める。** inode も mtime も保ったままの in-place
  更新（`touch -r` を伴うタグツール）は dev/inode/mtime では見えない。ただし外部 rename も
  ctime を進めるため、rel_path が記録時点と違う（スキャナが追随した）ときに限り ctime_ns だけの
  不一致は許容する（D-41）
- **`expected_dev` は記録するが照合しない。** dev 番号はマウントのたびに振り直されうる（ZFS は
  ホスト再起動で変わる）ので、記録の後に再起動があると全 op が外れる。実体の同一性は
  inode / size / mtime_ns / ctime_ns / tag_hash で確認する。rename / normalize の所在判定
  （`same_inode`）、flaccheck / RG / 再生の「行と FD の照合」も同じ扱い（D-62）
- **DB 先行更新 + pending 記録**を採る。DB の値は「確定した真実」ではなく
  **書き込み意図のオーバーレイ**で、ファイル反映が終わるまで暫定。UI は pending を
  バッジで見せる。DB を失うと未反映の意図も失われる（ファイルは旧値のまま残るので
  データは壊れない）。DB を後から確定する案は、6 万件の一括編集で UI が数分間
  古いままになるため却下（D-24）
- **overlay の解消**: op が `applied` 以外の終端（skipped_conflict / failed / cancelled）に
  なるときは、**同じトランザクションで**そのトラックの DB 値をファイルの現在値に戻す
  （タグ・キャッシュ列・`tag_hash`・物理属性）。次回スキャン任せにしない。`tag_version` は
  既に進んでいるので据え置く（Derived の追随ジョブはファイルの現在値で書くだけなので無害）。
  ファイルを読めない（消えている・壊れている）ときだけ `edits.old_value` と事前条件の物理属性へ
  戻す（D-41）
- **pending 中の再編集は 409 で拒否する。** `edit_ops(track_id) WHERE result='pending'` の
  UNIQUE で DB 側でも保証する。後続バッチが先行 intent を統合する方式（`superseded`）は
  予約のみで P0 では実装しない
- **tmp + rename を採用**する。電源断でファイルが壊れないことを優先。
  代償として inode が変わるので、書き込み成功時に必ず DB を追随させる
- **リカバリ**: 起動時に `prepared` / `applying` のバッチについて、`pending` の op に
  対応する track ジョブを再投入する（dedup で二重にはならない）。クラッシュ前に
  rename まで済んでいた op は inode 不一致になるので、ファイルのタグを読み
  **全フィールドが新値と一致すれば `applied` として確定**、そうでなければ
  `skipped_conflict`（overlay の解消を伴う）。この確定は再投入されたジョブが通常の反映経路で
  行う（起動時の特別処理ではない）。同じジョブを何度再実行しても結果は変わらない。
  バックオフの上限に達する失敗では、ハンドラが op を `failed` に閉じて overlay を解消する（D-41）
- **stale ジョブ**: tagwrite の `dedup_key` は `tagwrite:<track_id>:<tag_version>`。
  409 により pending 中の再編集は起きないので、通常は stale にならない。
  それでも payload の版 < 現在値なら no-op で `done` にする（防御）
- **キャンセル**: バッチ単位。子ジョブに `cancel_requested_at` を立て、未着手の op は
  `failed`（error='cancelled'、overlay の解消を伴う）、進行中の op は完了を待つ。全 op が
  終端になったらバッチを `cancelled` に。applied になった op はそのまま残り、revert で戻せる
- **巻き戻し**（§9 `/api/history/:batch/revert`）:
  - 対象は**終端状態**（applied / partial / failed / cancelled）のバッチのみ。
    `prepared` / `applying` はまずキャンセルする
  - 対象 op 集合 = 元バッチで `applied` になった op − 既存の逆バッチ（`reverts_batch_id` = 元）で
    既に `applied` になっている op。**この集合が空なら 409 `already_reverted`**
  - 各 op について**全フィールドの現在値が元バッチの `new_value` と一致する場合だけ**
    `old_value` へ戻す（op 単位。1 フィールドでも違えば `skipped_conflict`）。
    一致しないものは外部変更または後続バッチの変更なので UI に件数を出す
  - 元バッチの `reverted_at` は、**逆バッチが終端になり、対象集合が全件 `applied` になったとき**
    だけ立てる。逆バッチが partial なら立てず、再度 revert すると残りだけが対象になる
  - やり直し（redo）= 逆バッチを revert する。同じ規則で処理される
  - 巻き戻しも通常のバッチなので、対象トラックに pending があれば 409
- **埋め込み画像の差し替えも tags op**（P1-3 書き側、D-60）。キー `PICTURE`（値は `tag_hash` と同じ
  `<mime>:<sha256hex>` の配列。`track_tags` にもこの形で入っている）を `Editor::prepare_picture` が
  「全画像を捨てて上げた 1 枚を front cover にする」変更として記録する。ユーザの JSON タグ操作は
  `PICTURE` を受け付けない。実体は `ArtworkStore`（`thumbs/<hex>/orig.<ext>`）にあり、tagwrite は
  **書く前に捨てる旧画像を同じ store へ退避**してから書く（退避できなければ `failed`。新画像が store に
  無ければ `failed`）。巻き戻しは通常の逆 op（旧画像は退避済み）。applied のとき album の再解決を予約し
  増分スキャンを投入する。spindle は同梱の cover ファイルを書かない（埋め込み統一。D-60）
- リネーム（P0-11）と論理削除も同じ機構に乗せる（`kind = rename / delete`）。
  巻き戻しの対象は tag に限らない。**一括リネームは coordinator が 2 phase で行う**（D-43）:
  prepare で DB の `rel_path` を 2 段階更新で新値にし（overlay。`expected_rel_path` が記録時点の
  物理パス）、album の所属も追随させ、**バッチ 1 つに `rename` ジョブ 1 つ**を投入する。ジョブは
  phase 1 で全 op の source を同じディレクトリの一時名 `spindle-rename-<op_id>.<ext>` へ
  `RENAME_NOREPLACE` で退避（事前条件は inode / size / mtime_ns / ctime_ns。dev は照合しない、
  D-62。外れていれば
  `skipped_conflict` で触らない）、phase 2 で `ordinal` 順に最終名へ置く（宛先ディレクトリは作る。
  宛先が取られていれば source へ戻して `skipped_conflict`）。最後に 1 トランザクションで op を
  終端にし、`rel_path` をファイルの所在へ揃え、物理属性を追随し、バッチを集計する。
  swap / 循環はこれで解ける。phase 境界でのクラッシュは、再投入されたジョブが op ごとの所在
  （最終名 / 一時名 / source）を inode で判定して続きを行う。cancel は phase 2 に入るまで
  （退避済みを戻して全 op を `failed('cancelled')`）。スキャナは pending の rename op があるトラックの
  所在が source / 一時名 / 最終名のいずれかなら衝突にしない

### 7.6 Derived 生成

| 項目 | 規則 |
|---|---|
| 対象 | Library 内の可逆のみ（flac / alac / wav）。opus / aac / mp3 は対象外 |
| 出力 | Opus 128kbps VBR（`--vbr`, signal=music）。可逆200GBで約25GB |
| パス | Library と完全ミラー（拡張子のみ `.opus`） |
| RG | 再解析しない。Library 側の解析値を `R128_*` へ変換して埋める（`REPLAYGAIN_*` は書かない） |
| 画像 | トラック自身の埋め込み画像（`tracks.artwork_id`。D-61）、無ければ album のアートワーク（§7.1）の長辺 768 の WebP を 1 枚だけ埋める（D-51） |
| 判定 | `audio_version` 差分 → 再エンコード / `tag_version`・埋めた画像（`src_artwork_id`）・RG の解析世代（`src_rg_scanned_at`）の差分のみ → タグ上書き / パスの差分のみ → rename |
| 投入 | scan ジョブの完了時に食い違う全トラック、tagwrite / rename の applied、RG 解析の保存（D-51）。ジョブは `transcode`（track 単位、`audio_version` で dedup）で、ハンドラが現在値から必要な処理を決める |
| 追随 | Library の移動に追随（Derived を rename）。削除には追随せず（missing は可逆）、`retention_days` 超の回収と孤児は GC ジョブ |
| マルチch | 既定で対象外（チャンネル数不明も対象外）。トラック単位の `-ac 2` ダウンミックスは需要が出たら（D-51。実データは全件 2ch） |

非可逆音源は Derived を作らず原本をそのまま配布する（多重劣化の回避）。
容量逼迫時のみ、トラック単位で `force_transcode` を手動指定可能（**元が 256kbps 以上の場合のみ**）
という例外は、需要が出るまで実装しない（D-51。実データの非可逆は Opus が大半で削減にならない）。

### 7.7 ytmusic 統合

| 元モジュール | 移行先 | 備考 |
|---|---|---|
| `parser.py`（タイトルパーサ）/ チャンネル定義 | **外部のメタデータプラグイン**（別リポジトリ） | spindle はタイトルの慣習を知らない（D-69） |
| `models.py` 正規化 | Rust | 置換テーブルは D-43 で 1 本化済み |
| `tagger.py` (mutagen) | lofty | |
| `audio.py` R128 | ebur128 | ffmpeg loudnorm より高精度 |
| `organizer.py` / `playlist.py` | Rust | m3u8 生成は継続 |
| `downloader.py` | Rust（yt-dlp を subprocess） | |
| `sync.py` (ADB) | **移植しない** | NAS に端末を繋ぐ運用が不自然。Syncthing / SMB へ |

**メタデータプラグインのプロトコル v1**（`src/import/ytmusic/metadata.rs`、P3-1 / P3-2、D-69）。
動画のタイトルからトラックのメタデータ（タイトル・アーティスト・アルバム・category）を決める知識は
利用者固有（チャンネル名、タイトルの慣習、パターンのルール）なので spindle には置かず、外部コマンドに
問い合わせる。spindle が知る契約はこのプロトコルだけ。

- **起動**: `[ytmusic].metadata_command`（引数配列。`sh -c` は使わない）をアイテム 1 件ごとに起動し、stdin に
  Request を 1 つ書き、stdout の Response（JSON 1 つ）を読む。`metadata_timeout_secs`（既定 30）で kill。
  stderr はログに出す。終了コードが非ゼロ・stdout が JSON（厳密な UTF-8）でない・`protocol` が違う・
  必須の値が空（`title` / `albumartist` / `album` / `artists`）・`ok: false` なのに `reason` / `message` が
  無い・`category` がディレクトリ名として不正（`POST /api/categories` と同じ規則: 前後の空白、禁止文字、
  末尾のドット、予約名は不可）・`tags` に spindle が決めるキー（TITLE / ARTIST / ALBUM / ALBUMARTIST /
  DATE / TRACKNUMBER / DISCNUMBER / METADATA_BLOCK_PICTURE）や不正なキー・空の値があるものは
  **プラグインの故障**として取り込みを止める
- **Request**（未知のフィールドはプラグインが無視する。前方互換）
  ```jsonc
  { "protocol": 1, "op": "metadata",
    "item": { "source": "youtube",            // 提供元
              "channel": "<設定のチャンネル識別子>",
              "channel_title": "…" | null,    // 提供元での表示名
              "id": "<動画 id>" | null, "url": "…" | null,
              "title": "<動画タイトル>",       // 必須
              "uploaded_at": "YYYY-MM-DD" | null, "duration_ms": 123 | null } }
  ```
- **Response**（判定できたかに関わらず終了コード 0）
  ```jsonc
  { "protocol": 1, "ok": true,
    "track": { "title": "…", "artists": ["…"], "albumartist": "…", "album": "…",
               "category": "<統制語彙の名前>" | null,   // null は未分類（_Unsorted）
               "date": "YYYY[-MM[-DD]]" | null,
               "tags": [["ORIGINALARTIST", "…"]] } }   // 追加のタグ（キー, 値）
  { "protocol": 1, "ok": false,
    "reason": "unmatched" | "unknown_channel" | "skip" | "unsupported",   // 未知の値も通す（前方互換）
    "message": "…" }   // unmatched / unknown_channel は要対応（メッセージをそのまま見せる）、skip は取り込まない。
                       // reason / message は必須
  ```
- **spindle 側の写像**: `track` → タグ（TITLE / ARTIST 多値 / ALBUM / ALBUMARTIST / DATE / TRACKNUMBER +
  `tags`）と `pathgen::TrackFields`（category / albumartist / artist（先頭）/ album / title / track_no / year）。
  `category` は `categories` に同じ canonical key の語彙が無ければ追加する（プラグインの定義が正。
  初回起動の空 DB でも動く）。トラック番号（album の active な `track_no` の最大 + 1）と配置は
  ダウンローダ（P3-3）が行う
- 参照実装: `AkashiSN/spindle-ytmusic-meta`（private。ルール TOML + フィクスチャ + チャンネル定義を同梱した
  Rust のバイナリ。新パターンは Claude がそのリポジトリでルールとフィクスチャを 1 件ずつ足す運用）

### 7.8 Inbox 取り込み

取り込み対象ディレクトリを増やす方式は採らない。外部ディレクトリを直接
スキャン対象にすると、そのツリーのパス規約とタグ品質がそのまま Library に
混入するため。Inbox をステージングとして挟む。

```
Inbox/ に配置（ポーリング検出）
  → ステージング: タグ解析、コーデック判定、アルバム単位でグルーピング
  → 承認キュー:   UI で category / albumartist / album を確認・補正
  →               ※メタデータ不足のまま Library に入れない
  → 配置:         テンプレート展開 → Library へ move
  → 後続ジョブ:   normalize(WAV) → rg → transcode → thumbnail
```

承認キューを挟むことが要点で、これがないと `_Unsorted` が際限なく育つ。

実装（`src/import/inbox.rs`、`inbox` ジョブ、D-68）:

- **検出**は `inbox` ジョブ（並列 1・固定キー）が `[inbox].poll_interval_secs`（既定 60、0 で自動なし）で
  周期投入されるほか、`POST /api/inbox/scan` で手動。Inbox を歩き、音声ファイルのあるディレクトリを
  1 件（アルバム候補。root 直下の音声は `""` の 1 件）として `inbox_items` / `inbox_files` に写す。
  stat（inode / size / mtime / ctime）が変わったファイルだけタグを読み直す。**正は Inbox のファイル**で、
  行はキャッシュ: ディレクトリが消えれば行も消す（`placed` は 24 時間残して結果を見せる）。`approved` の件で
  ファイルが変わっていたら `pending` に戻す（再承認）
- **承認キュー**は `GET /api/inbox`。件ごとにタグから作った下書き（`proposal`: albumartist / album / date の
  最頻値、category は GENRE → `genre_category_map`、トラックは TRACKNUMBER / DISCNUMBER / TITLE / ARTIST）と
  不足の警告を返し、UI の Inbox タブで category / albumartist / album / date と各トラックの
  disc_no / track_no / title / artist を補正して `POST /api/inbox/:id/approve { draft }`。検証（album /
  albumartist / 各 title が空でない、`(disc_no, track_no)` が 1 以上で重複なし、rel_path が件のファイルと
  一致）に通らなければ 400 で、メタデータ不足のまま Library に入れない。`reject` / `reopen` で状態を戻す
- **配置**は `approved` の件を `inbox` ジョブが順に処理する。`library` の排他（scan / gc / CD の配置と
  同じ）を取れなければ Requeue。draft から各ファイルの `TrackFields` を作り `pathgen::plan`（category 無しは
  `_Unsorted`、`disc_no` の最大 ≥ 2 なら `multi_disc`、リリースキーは MUSICBRAINZ_ALBUMID の最頻値があれば
  `mb:`、無ければ件ごとの新規）→ Inbox からハッシュを取りながら Library の tmp へコピー → 補正で変わる
  タグだけ `write_tag_changes` で書く（ファイルが正のまま再スキャンしても DB と一致する）→ fsync →
  `RENAME_NOREPLACE` → 読み戻して 1 トランザクション登録（`source_type = 'download'`。宛先 album の
  リリースキー再検証と同パス行の MD5 検証は §7.2 の配置と同じ）→ Inbox 側を unlink → 既知の同梱ファイル
  （cover 画像 / cue / toc / log）も移し、空になった Inbox のディレクトリを消す。コピーに使う FD を
  fstat して承認時の行（inode / size / mtime / ctime）と照合し、コピーの後にも同じ FD を照合し、置いた
  ファイルの音声の指紋が承認時に読んだものと一致することを確かめる。どれかが外れたら `Changed`（件は
  `pending` に戻して再承認）。衝突・不足は件を `failed` にして理由を残し、この呼び出しで置いたファイルは
  片付ける。
- **状態遷移は CAS。** `approve` / `reject` / `reopen` と worker の `approved → placing` は
  `UPDATE … WHERE id = ? AND state IN (…)` で行い（`db::inbox::transition`）、読んでから書くまでの間に
  他（API / worker / 走査）が動かした件を上書きしない（API は 409 `state`、worker はその件を飛ばす）。
  前のプロセスが配置の途中で落ちて `placing` のまま残った件は、次の `inbox` ジョブの先頭で `approved` に
  戻して配置し直す（並列 1 なので、そこで見える `placing` は必ず前の実行の残り。配置は音声の指紋で自分の
  成果物を採用するので冪等）。件の `placed` と、WAV / ALAC / AIFF の normalize バッチ（`[normalize].wav_to_flac`）も登録
  トランザクションの中で確定する（commit の直後に落ちても `placing` が残らず、投入も欠けない）。`placed` の件のディレクトリに走査で音声が見えたら（消せなかった原本、
  配置の後に置かれたファイル）`pending` に戻して件として出し直す。
  Inbox は Library と別データセットなので move は実コピー（§5）
- **後続**は `rg`（album）と `transcode`。WAV / ALAC / AIFF は `[normalize].wav_to_flac` なら `normalize` の
  編集バッチを作って投入する（D-46 の予告）。thumbnail は埋め込み画像があればスキャンと同じ経路で出る

### 7.9 FLAC 健全性チェック（移行時 + 任意）

```
flac -t で検証
  ├ STREAMINFO MD5 が未設定（全ゼロ） → デコードした PCM MD5 を STREAMINFO に補填（再エンコードしない）
  │    一部の古いエンコーダや配信由来の FLAC で実際に起きる。
  │    未設定だと同一性解決の第 2 手段が使えず、遡及照合も不可能
  ├ デコードエラー → 要対応としてフラグ
  └ 正常 → そのまま
```

**圧縮レベルを揃えるための一括再エンコードは行わない。**
`flac -8` は圧縮率だけの違いでデコード結果は完全に同一であり、削減は
概ね 0.5〜1.5%。対価として 200GB を書き直すことになり、ZFS スナップショットが
旧ブロックを掴むため保持期間中は実効使用量が倍増する。
`flac -8` は新規生成物（CD リップ、WAV 正規化）にのみ適用する。

MD5 補填のための書き換えでは `audio_version` を据え置く
（音声内容が変わらないため Derived の再生成は不要）。inode と mtime は変わるので
DB の追随は必要。補填は再エンコードではなく STREAMINFO の MD5 16 バイトだけを書き換える
（P1-5b、D-57 / D-59）。補填は編集バッチ（`edit_ops.kind = 'md5'`、`edits.key = 'audio_md5'`、
値は hex で全ゼロ = 未設定。`new_value` は反映時に計算値を書く）として記録し、tagwrite ジョブが
tmp + rename で反映する。巻き戻しは全ゼロを書き戻す（`audio_md5` は NULL、`flac_check` は
`md5_missing` に戻る）。

検査（`flaccheck` ジョブ）は読むだけで、結果を `tracks.flac_check`（`ok` / `md5_missing` /
`decode_error`）に検査時の `audio_version` 付きで記録する。版が進めば結果は古い扱いになり、
`[normalize].flac_verify_on_import` ならスキャン完了時に結果の無い FLAC を自動で検査する。
手動は `POST /api/flaccheck { selection }`。一覧の固定フィルタ `flac_unchecked` / `flac_error`。

---

## 8. ジョブシステム

| type | 並列度 | 冪等キー |
|---|---|---|
| `scan` | 1 | 固定 |
| `rip` | **1**（物理ドライブ1台） | discid |
| `verify` | 2 | album_id |
| `rg` | CPU コア数 | album_id |
| `transcode` | CPU コア数 - 1 | track_id + audio_version |
| `tagwrite` | 4 | track_id + tag_version（`edit_batch_id` でバッチに紐づく） |
| `rename` | 1 | batch_id（バッチ 1 つに 1 ジョブ。2 phase の順序を守るため直列。D-43） |
| `normalize` | 2 | track_id + op_id（同じトラックの直列化は track_locks） |
| `thumbnail` | 4 | artwork_id |
| `flaccheck` | CPU コア数 | track_id + audio_version（版付き。D-57） |
| `inbox` | 1 | 固定 |
| `gc` | 1 | 固定（scan と同じ排他 `library` を取れなければ Requeue。D-56） |
| `backup` | 1 | 固定 |

- 起動時リカバリ: `running` を `queued` へ戻し、`track_locks` / `derived_path_locks` / `job_mutexes` を
  **全件削除**する
  （ロックはプロセス生存中しか意味を持たない）。単一インスタンス前提。同じ DB を
  複数プロセスで開くことは想定しない（compose で replicas を増やさない）
- `dedup_key` は **`queued` / `running` の間だけ**一意（partial unique index）。
  列 UNIQUE にすると `done` / `failed` 後に同じキー（`scan` の固定キー、同 version の
  手動再試行）を永久に投入できない。キーは `type` を含めて構成する
  （`tagwrite:<track_id>:<tag_version>` 等）
- 失敗は `attempts` をインクリメントし、`run_after` に次回時刻を書いて指数バックオフ
  （再起動を跨いでも待ち時間が保たれる）。`attempts >= max_attempts` で `failed`
- キャンセルは `cancel_requested_at` を立てる協調方式。ハンドラは進捗更新のたびに
  確認して自発的に止め、`cancelled` へ遷移する。外部プロセス（ffmpeg 等）は
  子プロセスグループごと kill し、tmp の成果物を消す
- 進捗は SSE で配信。DB にも永続化してリロードに耐える
- 同一トラックに対する競合ジョブは `dedup_key` と track 単位の
  advisory lock テーブルで直列化。複数トラックを掴むジョブ（album 単位の `rg` 等）は
  `track_id` 昇順に取得し、1 つでも取れなければ全解放して再キュー（デッドロック回避）
- 版を持つジョブ（`tagwrite` / `transcode`）は開始直前に payload の版と現在値を比較し、
  古ければ no-op で `done`（§7.5 stale ジョブ）
- **編集バッチは coordinator + 子ジョブ。** `tagwrite` ジョブは track 単位、`rename` ジョブは
  バッチ単位で投入し `jobs.edit_batch_id` でバッチに紐づける。バッチ自体はジョブではなく、
  終端状態は子ジョブ・op の結果から集計する（最後に終端になった子ジョブが集計する）。
  起動時リカバリ・キャンセルはバッチ配下の子ジョブに対して行う

---

## 9. HTTP API

```
GET    /api/tracks?filter=&sort=&cursor=&limit=   カーソルページング（D-39）
                                                  filter = URL エンコードした JSON（下記）、
                                                  sort = album(既定) | title | artist | album_title |
                                                  albumartist | date | duration | codec | rel_path | id
                                                  （`-` 前置で降順）、limit = 1..=1000（既定 100）
GET    /api/tracks/:id                            セッション有りは一覧と同じ行。trusted_cidrs からの
                                                  セッション無しは限定フィールド（D-27 / D-39）
PATCH  /api/tracks/batch                          一括編集（dry_run フラグ）
POST   /api/tracks/batch/preview                  変更プレビュー
POST   /api/rename/preview                        テンプレート適用結果（selection_token を発行）
POST   /api/rename/apply                          token の集合をリネームバッチとして記録
POST   /api/normalize/preview                     ロスレス → FLAC 正規化の宛先（selection_token を発行）
POST   /api/normalize/apply                       token の集合を正規化バッチとして記録（§7.4）
POST   /api/rg                                    { selection }。album ごとに rg ジョブを投入（D-47）
POST   /api/rg/write                              { selection, description?, skip_pending? }。解析値を
                                                  タグとして書く編集バッチを記録（§6、D-48）。
                                                  preview 段階は無い（値は DB から決まる）
POST   /api/flaccheck                             { selection }。active な FLAC ごとに flaccheck ジョブを
                                                  投入（§7.9、D-57。読むだけで preview は無い）
POST   /api/md5fill                               { selection, description?, skip_pending? }。flac_check = md5_missing
                                                  の FLAC に md5 op の編集バッチを記録（§7.9、D-59）
                                                  → 201 { batch_id, affected, skipped, pending_excluded }
                                                  → 409 pending | no_changes | md5_fill_disabled
POST   /api/verify                                { selection }。selection のトラックが属する album ごとに
                                                  verify ジョブ（遡及照合）を投入（§7.3、D-13 / D-63。読むだけ）
                                                  → 202 { albums, duplicates, job_ids }
                                                  → 409 no_changes

GET    /api/albums / :id                         全件（ページングなし）。track_count / duration_ms は active のみ
                                                  /api/tracks の行と /api/tracks/:id には artwork_hash（トラック自身の
                                                  埋め込み画像。無ければ null。D-61）
GET    /api/categories, POST /api/categories        統制語彙 { "items": [{ id, name }] }。POST は { name }（重複は 409）。
                                                  CD 取り込みの確定フォームの category に使う（D-67）
GET    /api/search?q=                             FTS5 trigram（3 文字未満は LIKE）。/api/tracks と同じ
                                                  レスポンス形で、filter / sort / cursor / limit も受ける

GET    /api/stream/:id                            Range 対応。原本
GET    /api/stream/:id?transcode=opus             オンザフライ変換
GET    /api/artwork/:hash?size=                   size = 256 | 768 で WebP のサムネイル、無しで原画像
                                                  （元の MIME）。hash は albums.artwork_hash。未生成なら
                                                  原画像へ倒す（no-cache）。ハッシュアドレスなので immutable
POST   /api/artwork/upload                        生の画像バイト列（Content-Type: image/*、上限 32 MiB。形式は
                                                  ヘッダで判別。JPEG / PNG / WebP 以外は 400 unsupported_image）を
                                                  ArtworkStore と artwork 行に置き thumbnail ジョブを投入
                                                  → 201 { sha256, mime, width, height, bytes }（D-60）
POST   /api/artwork/embed                         { selection, sha256, description?, skip_pending? }。selection の
                                                  active 全行の埋め込み画像をその 1 枚に差し替える tags op
                                                  （PICTURE）の編集バッチを記録（§7.5、D-60）
                                                  → 201 { batch_id, affected, unchanged, pending_excluded }
                                                  → 404 artwork_not_found、409 pending | no_changes

GET    /api/playlists, POST, PATCH, DELETE        プレイリストの CRUD（D-53）。POST / PATCH に rule があれば
                                                  スマート（D-54。評価結果は playlist_items に書く）。並びは
                                                  /api/tracks?filter={"playlist_id":N}&sort=position
POST   /api/playlists/:id/items                   { selection, sort? } を末尾に追加（同じトラックは 1 回）
DELETE /api/playlists/:id/items                   { track_ids?, selection? } を外す
POST   /api/playlists/:id/items/move              { track_ids, before } before の直前（null は末尾）へ
POST   /api/playlists/preview                     { rule } スマートルールの検証と評価件数（保存しない。D-54）
POST   /api/playlists/:id/refresh                 スマートを今の DB で再評価（項目を書き直す）
GET    /api/playlists/:id/export?profile=         m3u8 の本文（trusted CIDR で認証スキップ）
POST   /api/playlists/:id/export?profile=         Playlists/<profile>/<name>.m3u8 へ書き出し。応答に
                                                  count / skipped_missing / stale_tags（delivery のタグ追随待ち）
GET    /api/playlists/import, POST                Playlists root 下の m3u8 の一覧 / { path, name? } で取り込み
GET    /api/playlists/:id/fb2k_query              foobar Autoplaylist 用の { query, sort, notes }（smart のみ。D-55）

POST   /api/auth/login, POST /api/auth/logout
GET    /api/auth/session

GET    /api/cd/status                             ディスク有無・TOC（P2-1。TOC は下の lookup に渡す文字列と同じ形）
POST   /api/cd/lookup                             { toc }。TOC 文字列（CTDB 形式 0:13915:…:leadout か MusicBrainz 形式
                                                  1 12 leadout+150 offset+150…）から各種 DiscID を出し、MusicBrainz に
                                                  照会（P2-3、D-21 / D-64）。→ 200 { discid, mb_toc, accuraterip_id,
                                                  ctdb_toc_id, exact, candidates: [リリース × medium],
                                                  tracks: [{ number, length_ms }]（TOC の音声トラック。手入力フォームの行。D-65）}。
                                                  400 bad_request（TOC）、502 lookup_failed（届かない・応答が壊れている）、
                                                  503 musicbrainz_unavailable（再試行しても 503 の負荷制限、または未構成）
POST   /api/cd/rip                                リップ開始
POST   /api/cd/eject

GET    /api/jobs, POST /api/jobs/:id/cancel, POST /api/jobs/:id/retry
GET    /api/config                                読み込んだ config.toml の原文 { "path", "text" }（設定画面 §12.6。秘密は config に無い）
GET    /api/archive                                退避台帳 { "items": [ archived_files の行 + "batch_id" ] }（新しい順。復元は batch の巻き戻し）
POST   /api/scan                                  {"kind": "incremental" | "deep"}。scan ジョブを投入
GET    /api/gc/preview                            GC の dry-run（区分ごとの件数・バイト数・先頭 50 件。何も消さない。D-56）
GET    /api/inbox                                 承認キュー { "items": [{ id, rel_dir, state, detected_at, error, placed_album_id,
                                                  proposal, draft, warnings, tracks: [{ rel_path, codec, lossless, sample_rate,
                                                  bit_depth, channels, duration_ms, tags }] }] }（§7.8、D-68）
POST   /api/inbox/scan                            inbox ジョブを投入（202 + job_id。queued / running があれば 409 duplicate）
POST   /api/inbox/:id/approve                     { category, albumartist, album, date, tracks: [{ rel_path, disc_no, track_no,
                                                  title, artist }] }。検証に通らなければ 400、pending / failed 以外は 409 → approved + ジョブ投入
POST   /api/inbox/:id/reject, /reopen             rejected へ / pending へ戻す（approved / rejected / failed から）
POST   /api/gc                                    gc ジョブを投入（未完了があれば 409）
                                                  （202 + job_id。queued / running があれば 409 duplicate）
GET    /api/events                                SSE: ジョブ進捗・ライブラリ変更

GET    /api/history, POST /api/history/:batch/revert
                                                  巻き戻しは新バッチとして記録（§7.5）。
                                                  終端状態のバッチのみ。pending 衝突・二重 revert は 409
POST   /api/history/:batch/cancel                 反映中バッチのキャンセル
```

- 一括編集は必ず preview → apply の 2 段階
- SSE は単一チャネル。イベント種別で多重化

### レスポンス形（UI が依存するもの）

```jsonc
// GET /api/tracks?filter=...&sort=title&cursor=...&limit=100
//   filter はホワイトリストのキーだけを持つ JSON（未知キーは 400。D-39）:
//   { "category": "J-Pop",            // ツリー: categories.name
//     "albumartist": "…",              // ツリー: tracks.albumartist の完全一致
//     "album_id": 12,                  // ツリー
//     "playlist_id": 3,                // プレイリスト所属
//     "flags": ["missing", "pending"], // 固定フィルタ（AND）: unverified | duplicate | missing |
//                                      //   no_rg | rg_unwritten | pending | conflict | hardlink |
//                                      //   flac_unchecked | flac_error（§7.9）
//     "q": "情緒" }                    // 検索語（3 文字以上 FTS5 / 未満 LIKE）
//   cursor は前ページの next_cursor をそのまま返す不透明文字列（キーセット）。sort が変わったら
//   捨てる（別ソートで発行したカーソルは 400）。
//   total はフィルタに一致する全件数（同じ読み取りスナップショットで数える）
{ "items": [ { "id": 1, "title": "...", "artist_display": "...", "album": "...", "albumartist": "...",
               "track_no": 1, "disc_no": 1, "date": "2024", "category": "J-Pop",
               "duration_ms": 280000, "codec": "flac", "lossless": true,
               "verification": "verified_ctdb", "rg_scanned_at": 1, "rg_written_at": 1,
               "derived": { "codec": "opus", "stale_tags": false },   // または null
               "flac_check": { "status": "ok", "checked_at": 1700000000, "stale": false, "error": null },
                                                                       // 未検査なら null（§7.9）
               "pending_batch_id": 42,                                // または null
               "conflict_batch_id": 41,                               // または null
               "duplicate_group": "a1b2…",                            // audio_md5 hex または null
               "hardlink": false, "missing_since": null,
               "rel_path": "J-Pop/…/01 ….flac" } ],
  "next_cursor": "…", "total": 61234 }

// 選択は 2 形。Ctrl+A はフィルタ形で送る（ID 列挙にしない）
// { "ids": [1, 2, 3] }  または  { "filter": "<選択時点のフィルタ式>", "exclude_ids": [7] }
// フィルタ形は「選択した時点のフィルタ」を immutable に保持する（表示中のフィルタとは別物）

// POST /api/tracks/batch/preview   { "selection": {...}, "ops": [...], "sort": "title" }
//   サーバは selection を解決して対象 track_id と各行の tag_version / 事前条件を
//   スナップショットに保存し、selection_token（TTL 15 分）を返す（D-33）。
//   ops は上から順に適用する操作の配列（D-42）:
//     { "op": "set",     "key": "TITLE",       "value": "…" | ["…", "…"] }   // 空は削除
//     { "op": "ref",     "key": "ALBUMARTIST", "template": "%artist%" }      // 先頭値で展開
//     { "op": "replace", "key": "TITLE",       "pattern": "…", "replacement": "$1" }
//     { "op": "number",  "key": "TRACKNUMBER", "start": 1, "pad": 0 }        // sort 順に連番
//     { "op": "delete",  "key": "COMMENT" }
//   items は値が変わる行だけ。反映待ちの行は評価せず pending_excluded に数える
{ "selection_token": "…", "count": 1207,
  "changed": 1180, "unchanged": 24, "pending_excluded": 3,
  "items": [ { "id": 1, "changes": { "TITLE": { "old": ["…"], "new": ["…"] } } } ] }

// PATCH /api/tracks/batch   { "selection_token", "ops", "description", "skip_pending": false }
//   対象は token のスナップショット集合だけ。preview 後にスキャンで増えた行は含まれない。
//   スナップショット時と tag_version が変わった行は skipped_conflict になる
//   201 { "batch_id": 42, "affected": 1180 }
//   409 { "error": "pending", "track_ids": [ … ], "count": 3 }   // skip_pending=true で除外して続行
//   409 { "error": "preview_stale" }                             // token 期限切れ・ops 不一致
//   409 { "error": "no_changes" }                                // 値が変わる行が無い
//   409 の応答では token を消費しない（同じ token でやり直せる）。201 で消費する

// POST /api/rename/preview   { "selection": {...}, "sort"? }
//   selection を解決して固定し selection_token を返す（D-33）。各行に [layout] のテンプレートを
//   適用した宛先を返す。items は宛先が変わる行と衝突した行だけ（変更なし・反映待ちは件数）
{ "selection_token": "…", "count": 980,
  "changed": 975, "unchanged": 2, "conflict": 2, "pending_excluded": 1,
  "items": [ { "id": 1, "old": "old/a.flac", "new": "J-Pop/花譜/魔法/03 過去を喰らう.flac" },
             { "id": 2, "old": "old/b.flac", "new": null, "reason": "同名の別リリースと衝突（…）" } ] }

// POST /api/rename/apply   { "selection_token", "description"?, "skip_pending": false }
//   token の集合で計画を取り直してバッチを記録する。衝突した行と preview の後にタグが変わった行は
//   skipped_conflict の op として記録だけする（affected に含む）
//   201 { "batch_id": 43, "affected": 977, "conflict": 2 }
//   409 { "error": "pending" | "preview_stale" | "no_changes" }   // 規則は PATCH /api/tracks/batch と同じ

// POST /api/normalize/preview   { "selection": {...}, "sort"? }
//   rename と同型。items は宛先が決まる行（old / codec / new）と衝突した行（new: null, reason）。
//   既に FLAC・非可逆は unchanged に数える
{ "selection_token": "…", "count": 7572,
  "changed": 7570, "unchanged": 0, "conflict": 2, "pending_excluded": 0,
  "items": [ { "id": 1, "old": "J-Pop/…/01 ….m4a", "codec": "alac", "new": "J-Pop/…/01 ….flac" } ] }
// POST /api/normalize/apply   { "selection_token", "description"?, "skip_pending": false }
//   201 { "batch_id": 44, "affected": 7572, "conflict": 2 }
//   409 { "error": "pending" | "preview_stale" | "no_changes" | "normalize_disabled" }

// GET /api/history
{ "items": [ { "id": 42, "created_at": 1, "description": "…", "kind": "tags",
               "state": "partial", "affected": 312, "applied": 309, "conflict": 3, "failed": 0,
               "reverts_batch_id": null, "reverted_by": 39, "finished_at": 1 } ] }
//   reverted_by は reverted_at が立っているときだけ（戻した逆バッチの id）。reverted_at も返す
// GET /api/history/:id  → 上 + "ops": [ { "id", "track_id", "kind", "result", "error", "rel_path",
//                                        "edits": { "TITLE": { "old": [...], "new": [...] } },
//                                        "current": { "TITLE": [...] } } ]   // skipped_conflict のときだけ
//                                        （編集キーの現在値 = ファイルの再読込結果。rename は rel_path）
// POST /api/history/:id/revert { "description"? }  → 201 { "batch_id": 43, "affected": 50, "conflict": 1 }
//                               → 404 | 409 { "error": "not_terminal" | "already_reverted" | "pending" }
// POST /api/history/:id/cancel  → 202 | 404 | 409 { "error": "not_cancellable" }

// GET /api/jobs
{ "items": [ { "id", "type", "state", "progress", "done", "total", "attempts", "last_error",
               "run_after", "edit_batch_id", "created_at", "started_at" } ],
  "summary": { "running": 3, "queued": 12, "pending_ops": 1204, "failed": 0 },
  "concurrency": { "scan": 1, "rg": 12, "transcode": 11, … } }   // 種別ごとの並列度（§8）
// POST /api/jobs/:id/cancel  → 202（queued は即 cancelled、running は cancel_requested_at を立てる）
//                            → 404 | 409 { "error": "not_cancellable" }   // 既に終端
// POST /api/jobs/:id/retry   → 202（failed / cancelled を attempts=0 で queued に戻す）
//                            → 404 | 409 { "error": "not_retryable" | "duplicate" }   // D-36

// SSE /api/events   event 種別: job | batch | library | resync
//   job:     { "id", "state", "progress", "done", "total" }
//   batch:   { "id", "state", "applied", "conflict", "failed" }
//   library: { "scan_run_id", "kind": "ids", "track_ids": [ … ] }   // 変更が 200 行以下
//            { "scan_run_id", "kind": "bulk" }                     // それ以上。ページを無効化
//            scan の完了時に 1 回（commit 後）。変更行 0 件なら流さない（D-39）
//   resync:  { "skipped": n }   // サーバ側で取りこぼした。一覧（jobs / history / 表示ページ）を再取得
//   クライアントは SSE を開いてから一覧を取得する（逆順だと開く前のイベントを失う。D-36）
```

`library` イベントを受けたクライアントは、**表示中のページ（クエリ + カーソル）を無効化して
再取得**する。行の差し替えだけでは、変更行がフィルタに出入りしたりソートキーが変わったり
カーソル境界を跨いだときに集合と順序が壊れる。`ids` のときは表示中に含まれる id があれば
再取得、`bulk` は無条件に再取得。選択（`selection`）はイベントで変えない。

`pending_batch_id` / `conflict_batch_id` / `duplicate_group` / `hardlink` は一覧の
バッジ列（§12.2）が直接使う。`GET /api/tracks` は 1 クエリで返す: `edit_ops` の pending と
最新 op は LEFT JOIN、重複は `audio_md5` 索引への相関 EXISTS で行ごとに引く
（`duplicate_groups` ビューを JOIN すると毎回 GROUP BY の実体化が走る。D-39）。

// GET /api/albums
{ "items": [ { "id": 1, "rel_dir": "J-Pop/…/…", "category": "J-Pop", "albumartist": "…", "album": "…",
               "date": "2024", "original_date": null, "edition": null, "mb_release_id": null,
               "disc_count": null, "artwork_id": null, "artwork_hash": null,   // SHA-256 hex
               "track_count": 12, "duration_ms": 2800000, "missing_since": null } ] }
// GET /api/albums/:id  → 上の 1 要素 | 404

### 認証

LAN 限定でも必須とする。攻撃者対策というより事故対策で、この API は 300GB の
ライブラリをリネーム・削除・タグ上書きできる。認証がないと、別タブのスクリプト、
家族の端末、うっかり有効にしたポートフォワードがそのまま破壊的操作に届く。

- 単一パスワード。argon2id でハッシュして DB に保存
- **初期パスワードは環境変数 `SPINDLE_INITIAL_PASSWORD`** で与える（D-28）。DB に
  パスワードが無い初回起動でハッシュ化して保存し、以後は無視する。環境変数も DB も
  無い場合は**ロックモード**で起動し、SPA の配信を含めて `/health` 以外は 503 で
  短い JSON（設定手順）を返す。`/health` は `{"status":"locked"}` を返して機械可読にする。
  「先着で設定できる初回セットアップ画面」は置かない。compose のサンプルは
  `${SPINDLE_INITIAL_PASSWORD:?...}` 形式で、値を与えないと起動できないようにする
- セッション Cookie（HttpOnly / SameSite=Lax / TLS 時のみ Secure）。Cookie 値はランダム
  32 バイトで、DB には **SHA-256 のみ**保存（`sessions.token_hash`）。失敗ログインは
  IP 単位でレート制限
- **変更系リクエスト（POST / PATCH / DELETE）の CSRF 検証**: `Origin` ヘッダが**あれば**
  scheme / host / port が自分自身と完全一致することを必須にする。`Origin` が無い場合だけ
  `Sec-Fetch-Site` が `same-origin` / `none` であることを要求し、それも無ければ拒否する。
  **`Host` の一致は判定に使わない**（クロスサイト POST でも Host は送信先になるため防御に
  ならない）。reverse proxy 越しの外部 origin は `trusted_proxies` からの `X-Forwarded-Host` /
  `X-Forwarded-Proto` だけで構成する
- CORS は全面禁止（SPA は同一オリジンで配信するため不要）
- `trusted_cidrs` からの接続は **route allowlist だけ認証をスキップ**する（D-27）:
  `GET /api/stream/:id`、`GET /api/artwork/:hash`、`GET /api/tracks/:id`（限定フィールド）、
  `GET /api/playlists/:id/export`。それ以外（一覧・検索・SSE・history・jobs・設定・session）は
  CIDR 内でもセッション必須。変更系は当然セッション必須。用途は他プレイヤーや curl からの
  ストリーム参照であり、履歴やジョブのエラー文（パスを含む）を LAN 全体に見せる理由はない。
  判定に使うのは接続元 socket のアドレスで、`X-Forwarded-For` は `trusted_proxies` に列挙した
  proxy からのものだけ採用する
- `/health` 以外の全ルート（SSE / stream / artwork 含む）が同じ認証ミドルウェアを通る
- Subsonic 非対応が確定したため、salt+md5 方式との併存を考慮する必要はない

---

## 10. プレイリストとエクスポート

### スマートプレイリスト DSL

foobar2000 風の文法を採用し、`pest` でパースして AST(JSON) を DB に保存、
実行時にパラメータ化 SQL へ変換する。生 SQL を保存しないのは、スキーマ変更で
全ルールが壊れることと、インジェクション面を抱え込むことを避けるため。

```
%albumartist% IS ヰ世界情緒 AND %verification% IS verified_ctdb
  AND NOT %category% IS _Unsorted
ORDER BY %date% DESC LIMIT 100
```

- 演算子: `IS` / `HAS`(部分一致) / `GREATER` / `LESS` / `MATCHES`(正規表現) /
  `MISSING` / `PRESENT`
- 論理: `AND` / `OR` / `NOT` / 括弧
- 拡張フィールド: `verification` `lossless` `codec` `samplerate` `bitdepth`
  `channels` `category` `added` `duration` `has_derived` `missing`
- 独自拡張（foobar に無い）: `MATCHES` / `LIMIT` / `ORDER BY random`
- SQL 生成はホワイトリスト列へのマッピング。任意タグは
  `EXISTS (SELECT 1 FROM track_tags ...)` に展開。値は全てバインドパラメータ
- 評価結果（ORDER BY / LIMIT 適用後）は `playlist_items` に書く（D-54）。表・書き出しは手動と同じ経路。
  ライブラリ変更をトリガに常駐タスクがデバウンス後に再評価する

### エクスポート形式

| 出力先 | 形式 | 内容 |
|---|---|---|
| foobar（静的） | `.m3u8` | 評価結果のトラック一覧。UTF-8 / BOM なし |
| foobar（動的） | クリップボード | **クエリ文字列とソートパターン**。Autoplaylist 作成時に貼る（`.txt` は作らない。D-55） |
| Android | `.m3u8` | 配布ビュー（Derived 優先）で解決したパス。タグ追随待ちの Derived は件数（`stale_tags`）で示す |
| 汎用 | `.pls` | 任意（未実装。`export_profiles.format` に列挙だけしてある） |

`.fpl` は非対応。非公開のバイナリ形式で foobar 1.x と 2.x で構造が異なり、
書き損じると foobar 側の状態を壊す。

### foobar クエリへの変換

1. **フィールド名の写像。** foobar はスペース区切り: `ALBUMARTIST` → `%album artist%`、
   `TRACKNUMBER` → `%tracknumber%`。写像表を持つ（docs/DSL.md）。技術情報（`codec` `samplerate`
   `bitrate` `channels` `bitdepth` `duration`）は foobar の技術フィールドへ、spindle 固有
   （`verification` `category` `source_type` `lossless` `added` `has_derived` `missing`）と `MATCHES` は
   変換不能としてその項を落とし `notes` に出す
2. **`ORDER BY` は分離。** foobar の Autoplaylist はソートをクエリに書かず、
   別欄のタイトルフォーマット文字列で指定する。「クエリ」「ソートパターン」の
   2 本を出力する
3. **`LIMIT` と `random`、降順は変換不能。** `notes` に明示する

`HAS` は foobar でも部分一致で、技術情報フィールドの `PRESENT` / `MISSING` も効く
（2026-09-19 に実機で確認。D-55）。

### パスマッピング（エクスポートプロファイル）

NAS 上の `/library/...` をそのまま書いても foobar からは開けない。
出力先ごとにプロファイルを持つ（`export_profiles` テーブル）。3 つは固定で、CRUD の API は
持たない。`foobar` の `prefix` だけ `[export].fb2k_prefix` を正として起動時に揃える（D-55）。

| プロファイル | source | path_style | prefix | sep |
|---|---|---|---|---|
| `foobar` | master (Library) | absolute | `\\TRUENAS\music\` | `\` |
| `android` | delivery (Derived 優先) | relative | — | `/` |
| `internal` | master | relative | — | `/` |

書き出し先は `Playlists/<profile>/<name>.m3u8`（プロファイルごとにディレクトリを分ける。D-53）。
`Playlists/<profile>/` は `Library/` と同じ深さなので、相対パスは `../../Library/...` で
正しく解決される。旧ライブラリから移した `Playlists/m3u8/` は取り込み元として残す。
スマートプレイリストはライブラリ変更をトリガに、
デバウンス（既定 30 秒）を挟んで自動再エクスポートする。

---

## 11. 再生とトランスコード

| コーデック | ブラウザ | 方針 |
|---|---|---|
| FLAC | Chrome / Firefox / Safari 対応 | 直送 |
| Opus | Chrome / Firefox / Safari 18.4+ 対応 | 直送（Opus 不可のブラウザ向けの AAC 変換は作らない。D-52） |
| AAC (m4a) | 全対応 | 直送 |
| WAV | 全対応 | 直送（サイズ大） |
| **ALAC** | **Safari のみ** | **既定で Opus へ変換** |
| ハイレゾ FLAC | 再生可だが帯域大 | 既定の Derived（Opus 48 kHz）再生で吸収。「原本」を選んだときはそのまま送る（D-52） |

- `GET /api/stream/:id` は原本を Range で直送。`?transcode=opus` は `delivery` が Derived を指せば
  Derived を Range で直送し、無いときだけ ffmpeg を `stdout` パイプで起動して chunked で返す（D-52）
- 変換中のシークは該当位置から ffmpeg を再起動（`?start=` → `-ss`）
- 変換結果はキャッシュしない（Derived がそのキャッシュ）
- クライアント能力は起動時に `canPlayType()` で判定し、再生時に URL で選ぶ（サーバへは通知しない）。
  可逆はブラウザが再生できても既定で Derived の Opus（設定「原本」で直送）

---

## 12. UI

デスクトップブラウザ専用。ダークモード・レスポンシブ・アニメーションは対象外。
キーボードは Ctrl+A / Delete / Enter / Esc のみ。

### 12.1 骨格（3 ペイン + 下部バー）

```
┌──────────────────────────────────────────────────────────────────────────────┐
│ spindle   [検索 ______________]   一覧 | アルバム | CD | ジョブ | 履歴 | 設定 │ ← 上部ナビ
├────────────┬────────────────────────────────────────────────┬────────────────┤
│ ツリー      │ トラック一覧（仮想スクロール）                     │ 右パネル        │
│ ▾ Category │ ☐ │#│Title      │Artist │Album   │▲▼ … │バッジ │ ┌ 一括編集 ────┐│
│   ▾ J-Pop  │ ☑ │1│…          │…      │…       │      │ ✔ RG  │ │ 選択 1,204 件 ││
│     ▸ 藍井 │ ☑ │2│…          │…      │…       │      │ ⏳    │ │ TITLE ← 置換 ││
│     ▸ …    │ ☐ │3│…          │…      │…       │      │ ⚠ dup │ │ ALBUMARTIST  ││
│ ▸ Game     │ … (6 万行)                                       │ │  ← %artist%  ││
│            │                                                │ │ [プレビュー]  ││
│ プレイリスト │                                                │ │ [適用]        ││
│  ♪ 通勤    │                                                │ └──────────────┘│
│  ⚙ 未検証  │                                                │ ┌ 選択の詳細 ─┐│
│            │                                                │ │ 共通タグ /   ││
│ フィルタ    │                                                │ │ 差異あり     ││
│  未検証    │                                                │ └──────────────┘│
│  重複      │                                                │                │
│  missing   │                                                │                │
│  反映待ち  │                                                │                │
├────────────┴────────────────────────────────────────────────┴────────────────┤
│ ▶ ‖ ──●────── 03:12 / 04:40  RG:album 🔊│ 実行中 3 · 反映待ち 1,204 · 失敗 0 │ ← 再生 + ステータス
└──────────────────────────────────────────────────────────────────────────────┘
```

- **上部ナビ**: 画面の切替。「一覧」がホーム。検索ボックスは表の `filter.q`（入力から 250ms 後に
  反映。3 文字以上で FTS、未満は LIKE。D-39 / D-40）
- **左サイドバー**: 3 区画（ツリー / プレイリスト / フィルタ）。どれを選んでも
  **中心の表の集合を差し替えるだけ**で、列・ソート・選択の仕組みは共通
  - ツリー: Category → AlbumArtist → Album（`GET /api/albums` から構築。選ぶと scope を置き換える）
  - プレイリスト: 手動 / スマート。表からドラッグで追加
  - フィルタ（固定）: 未検証 / 重複 / missing / RG なし / 反映待ち / conflict / hardlink。
    トグルで AND、ツリーの絞り込みと組み合わせられる（D-40）
- **右パネル**: 2 タブ（一括編集 / 選択の詳細）。折りたたみ可、幅は永続化
- **下部バー**: 左が再生（再生・停止・シーク・音量・RG の off / track / album）、右がジョブ要約
  「実行中 N · 反映待ち M · 失敗 K」。右側クリックでジョブ画面へ

### 12.2 トラック一覧

**列**（既定。順・幅・表示は localStorage に永続化）:
選択 / # / Title / Artist / Album / AlbumArtist / Date / Category / 長さ / Codec / バッジ /
rel_path（既定非表示）

**バッジ列**（1 セルに複数アイコン、ホバーで文言）:

| バッジ | 条件 | 出典 |
|---|---|---|
| 検証 | `verification` の 5 値をアイコン色で区別。`unverifiable` は「未検証」と別の見え方 | tracks |
| 可逆 / 非可逆 | `lossless` | tracks |
| RG | `rg_scanned_at` の有無。書き込み未反映（`rg_written_at < rg_scanned_at`）は半透明 | tracks |
| Derived | `derived_files` あり。`stale_tags` は点付き | delivery |
| 反映待ち ⏳ | `edit_ops.result = 'pending'` がある | edit_ops |
| conflict ⚠ | このトラックの**最新の op**（`edit_ops` を `id DESC` で 1 件）が `skipped_conflict` | edit_ops |
| 重複 | `duplicate_groups` に属する | view |
| hardlink | `nlink > 1` | tracks |
| missing | `missing_since` あり。行全体をグレー | tracks |

**選択**: クリック / Shift 範囲 / Ctrl 追加 / Ctrl+A（フィルタ結果全件）。
全件選択は ID 列挙ではなく**選択した時点のフィルタ式**をサーバに渡す（`selection.filter`）。
選択は immutable で、その後に表示フィルタやソートを変えても**選択集合は変わらない**
（表示中の行と選択集合が食い違うことはあり、右パネルは選択集合の件数を出す）。
プレビューでサーバが集合をスナップショットし（`selection_token`）、適用はその集合だけに
効く。右パネル頭に「選択 N 件（うち反映待ち M 件）」、表の上に「表示 K 件」。

**反映待ちの行**は編集不可。右パネルの [適用] は「M 件を除外して適用 / 待つ」の 2 択に
なる（API の 409 をここで吸収する）。

**インライン編集**: セルをダブルクリック → 1 件のバッチとして同じ経路（プレビュー省略）。

### 12.3 右パネル: 一括編集

```
操作リスト（上から順に適用）
  1. TITLE        正規表現置換   /\s*\(Official.*\)$//
  2. ALBUMARTIST  フィールド参照 %artist%
  3. TRACKNUMBER  連番          開始 1、現在のソート順に
  4. COMMENT      削除
  [+ 操作を追加]
[プレビュー]  → 表の該当セルに 旧値→新値 の差分表示。変更なしは薄く。
                頭に「変更 1,180 / 変更なし 24 / 反映待ちで除外 3」
[適用]        → 説明文（任意）→ バッチ作成 → 下部バーの反映待ちが動く
```

操作: 固定値代入 / フィールド参照（`%albumartist%`）/ 正規表現置換 / トラック番号連番 /
タグ削除。プレビューは `POST /api/tracks/batch/preview` の結果を表に重ねる。別表は開かない。
操作リストは選択を変えても残る（同じ操作を別の集合へ繰り返し適用できる）。

### 12.4 編集履歴

```
#42  09-16 14:03  "Official を除去"      tags    1,204 件   applied              [巻き戻す]
#41  09-16 13:50  "アーティスト統一"      tags      312 件   partial (conflict 3)  [巻き戻す] [conflict を見る]
#40  09-16 13:20  "リネーム J-Pop/…"     rename    980 件   applying 640/980      [キャンセル]
#39  09-15 …       (#38 の巻き戻し)      tags       50 件   applied   ↩ #38       [巻き戻す(=やり直し)]
#38  09-15 …       "…"                   tags       50 件   applied   reverted     —
```

- 行を開くと op 一覧（トラック / result / error）。conflict の op は「ファイルを再読込した」旨と
  現在値を表示
- [巻き戻す] は終端状態のみ有効。`reverted_at` 済みは無効化して「#39 で戻し済み」と表示
- 巻き戻しも新バッチなので同じ一覧に現れる（`reverts_batch_id` を ↩ で表示）
- [キャンセル] は `prepared` / `applying` のみ

### 12.5 ジョブ

種別ごとの並列度と待ち行列、実行中の進捗（`done / total`）、失敗の `last_error` と
[再試行] / [キャンセル]。編集バッチ由来のジョブは `edit_batch_id` で履歴画面へリンク。
SSE `/api/events` で更新し、リロードしても DB の値で復元する。

### 12.6 その他の画面（骨格のみ）

- **アルバム**（P1-3）: サムネイルグリッド（`/api/artwork/:hash?size=256`。missing は出さない）→
  クリックで表を `album_id` に絞る（一覧へ戻る）
- **操作タブの「アートワーク」**（P1-3 書き側、D-60）: ファイル選択 → `POST /api/artwork/upload` →
  256px のプレビュー（形式・寸法）→ 「選択 N 件の埋め込み画像を差し替え」（`POST /api/artwork/embed`。
  反映待ちの 409 は他の操作と同じ「除外して適用」）。履歴画面の `PICTURE` 値はサムネイルで出す
- **CD**（P2）: ウィザード。検出 → 候補選択 / 手入力 / トラックリスト貼り付け →
  オフセット確認 → 進捗。照会ゼロ件でも完走できる。検出（P2-1）が入るまでは TOC の貼り付け
  （CTDB 形式 / MusicBrainz 形式 / `cdrecord -toc` の出力）を入力源にする（P2-3、D-64。デバッグ用に残す）。
  候補は DiscID 一致を先に出し、exact が 1 件なら選んでおく。選ぶとフォームに写り（アルバム・
  アルバムアーティスト・日付・レーベル・カタログ番号・JAN/UPC・ディスク番号 / 枚数・各トラックの
  タイトル / アーティスト。番号と長さは TOC から）、そこから直せる。候補ゼロ件なら空のフォームに直行、
  「候補を使わず手入力」で空にもできる（P2-4、D-65）。トラックリスト貼り付け欄（行頭の番号・行末の時間を
  外し、`タイトル / アーティスト` で分ける。逆順のチェック、表の貼り付け可）から番号で行に写し、
  行数の違いや未設定の行は警告。「確定」でアルバム名・アルバムアーティスト・各トラック名を検証し
  （空のタイトルは「Track NN で埋める」ボタン）、確定したメタデータをタグ名で見せる（`albumTags` /
  `trackTags`。MUSICBRAINZ_TRACKID は recording、MUSICBRAINZ_RELEASETRACKID は track）。吸い出し（P2-5）は
  この DiscMetadata を受ける。遷移は `lib/cdState.ts` の reducer（TOC の編集と「結果を消す」で結果より下が全部消える）
- **Inbox**（P2-10、D-68）: 左に件（= `[paths].inbox` の音声ファイルのあるディレクトリ）の一覧
  （状態バッジ・ファイル数・コーデック・検出時刻・失敗理由）と「今すぐ確認」（`POST /api/inbox/scan`）、
  右に選んだ件の補正フォーム: アルバムアーティスト / アルバム / 日付 / category（CD の確定フォームと同じ
  `CategoryField`）と、トラックごとの disc / # / タイトル / アーティスト（ファイル名・コーデック・長さは
  表示のみ）。初期値はタグからの提案（`proposal`）、承認済み・失敗の件は保存した下書き。検証はサーバと
  同じ規則（`lib/inbox.ts` の `validateDraft`）で、問題が無いときだけ「承認して配置」が押せる。
  「却下」はファイルを残したまま一覧から外し、「下書きに戻す」で pending に戻る。placed の件は 24 時間
  残り、「アルバムを開く」で表を `album_id` に絞る。inbox ジョブの完了で一覧を取り直す
- **設定**: `config.toml` の閲覧、再スキャン / deep scan / GC dry-run のボタン、
  退避 WAV（`archived_files`）の一覧と復元

---

## 13. 設定

```toml
[server]
listen = "0.0.0.0:8080"        # 待ち受けアドレス。省略時はこの値。ポート公開は compose 側で行う

[paths]
library  = "/library"
derived  = "/derived"
archive  = "/archive"
inbox    = "/inbox"
playlists = "/playlists"
data     = "/data"

[layout]
multi_disc  = "{category}/{albumartist}/{album}/{disc}-{track:02} {title}"
single_disc = "{category}/{albumartist}/{album}/{track:02} {title}"
unsorted    = "_Unsorted/{albumartist}/{album}/{track:02} {title}"

[rip]
device = "/dev/sr0"
drive_offset = "auto"          # auto | 整数
retry_on_mismatch = 2
prefer_ctdb = true

[encode]
derived_codec = "opus"
derived_bitrate = 128
flac_compression = 8

[replaygain]
reference_lufs = -18.0         # 内部表現。書き出し時に変換
write_tags = true

[normalize]
wav_to_flac = true
flac_verify_on_import = true
flac_fix_missing_md5 = true
flac_recompress_all = false   # 圧縮レベル統一のための一括再エンコードは行わない

[scan]
deep_interval_days = 30        # deep scan（tag_hash / audio_md5 全再計算）の間隔。0 で自動実行なし

[inbox]
poll_interval_secs = 60        # Inbox の検出間隔（inotify はコンテナ越しに不安定なのでポーリング）。0 で自動なし

[gc]
retention_days = 30            # 物理削除までの猶予（missing_since / 退避 WAV / Derived 孤児）

[auth]                         # 認証は常に有効。無効化する設定は置かない。
                               # パスワードハッシュは DB の auth 表に持つ。
                               # 初期値は環境変数 SPINDLE_INITIAL_PASSWORD（初回起動時のみ読む）
session_days = 30
trusted_cidrs = []             # 例: ["192.168.1.0/24"]。stream / artwork / tracks/:id / playlist export のみ認証スキップ
trusted_proxies = []           # ここに列挙した proxy からの X-Forwarded-* だけを信用する

[backup]
interval_hours = 24
retention_generations = 14

[export]
autoexport_debounce_sec = 30
fb2k_prefix = "\\\\TRUENAS\\music\\"

[musicbrainz]
user_agent = "spindle/0.1 (contact@example.com)"
rate_limit_per_sec = 1
url = "https://musicbrainz.org/ws/2/"   # 省略可。テストと自前ミラー用

[verify]                       # 遡及照合 / リップ検証の照会先。UA は musicbrainz.user_agent を共用
accuraterip_url = "http://www.accuraterip.com/accuraterip/"
ctdb_url = "http://db.cuetools.net/lookup2.php"

[ytmusic]
enabled = true
metadata_command = ["/usr/local/bin/spindle-ytmusic-meta", "metadata"]   # メタデータプラグイン（D-69）。引数配列
metadata_timeout_secs = 30

[bin]                          # 外部バイナリ。パスで上書き可
ffmpeg = "ffmpeg"
flac = "flac"
opusenc = "opusenc"
cdparanoia = "cd-paranoia"     # libcdio 版（Debian パッケージ cd-paranoia）
cdrdao = "cdrdao"
ytdlp = "yt-dlp"
```

実体は `deploy/config.example.toml`。両者は一致させる。

---

## 14. デプロイ

### TrueNAS Custom App (compose)

```yaml
services:
  spindle:
    image: ghcr.io/akashisn/spindle:latest
    devices:
      - /dev/sr0:/dev/sr0
      - /dev/sg0:/dev/sg0        # SG_IO に必要
    group_add:
      - "24"                     # host の cdrom グループ GID
    device_cgroup_rules:
      - 'b 11:* rmw'             # sr (block)
      - 'c 21:* rmw'             # sg (char)
    user: "1000:1000"            # 既存ライブラリの所有者に合わせる
    volumes:
      - /mnt/ssd/media/Library:/library
      - /mnt/ssd/media/Derived:/derived
      - /mnt/hdd/media/Archive:/archive
      - /mnt/ssd/media/Inbox:/inbox
      - /mnt/ssd/media/Playlists:/playlists
      - /mnt/ssd/apps/spindle:/data
    ports:
      - "8080:8080"
    restart: unless-stopped
```

**注意点:**

- USB 接続だとデバイス再列挙でノード番号が変わり得る（sr0 → sr1）。
  `/dev/disk/by-id/...` を指すか、SATA 接続を推奨
- UID/GID が既存ライブラリの所有者と一致しないとタグ書き込みが全滅する
- udev はコンテナに届かないため、ディスク挿入検知はポーリング

### ZFS データセット

```bash
COMMON="-o casesensitivity=insensitive -o normalization=formD"   # 作成時のみ指定可能。utf8only=on が強制される
zfs create ssd/media
zfs create $COMMON -o recordsize=1M -o compression=lz4 -o atime=off ssd/media/Library
zfs create $COMMON -o recordsize=1M -o compression=lz4 -o atime=off ssd/media/Derived
zfs create $COMMON -o recordsize=1M -o compression=lz4 -o atime=off ssd/media/Inbox
zfs create hdd/media
zfs create $COMMON -o recordsize=1M -o compression=lz4 -o atime=off hdd/media/Archive
zfs create -o recordsize=16K -o compression=lz4 -o atime=off ssd/apps/spindle
```

| dataset | recordsize | snapshot | 備考 |
|---|---|---|---|
| ssd/media/Library | 1M | 毎日 + 一括編集前 | 唯一の正 |
| ssd/media/Derived | 1M | なし | 再生成可能。レプリケーション対象外 |
| hdd/media/Archive | 1M | 週次 | 追記のみ。FLAC 正規化で退避するロスレスの受け皿（D-45） |
| ssd/media/Inbox | 1M | なし | 承認前の一時領域 |
| ssd/apps/spindle | 16K | 毎日 | SQLite。ページサイズに合わせる |

**`casesensitivity` と `normalization` はデータセット作成時のみ指定可能で、
後から変更できない。** このためライブラリは既存データセットの rename ではなく、
新規作成 + ファイルコピーで移行する（D-19）。

- `casesensitivity=insensitive`: Windows の foobar2000 が `Cover.jpg` を、
  spindle が `cover.jpg` を作る事故を ZFS 層で潰す。spindle は自身の生成パスの
  一意性は保証できるが、他クライアントが作るファイルまでは制御できない
- `normalization=formD`: 日本語の濁点・半濁点には NFC（`が` 1 文字）と
  NFD（`か` + 結合濁点）の 2 表現がある。macOS 由来のパスは NFD になりがちで、
  正規化なしだと「見た目が同じで別ファイル」が発生する。目視では気づけない

Inbox は Library と別データセットなので move は実コピーになるが、
1 回あたりアルバム 1 枚（数百 MB〜数 GB）なので実用上の問題はない。
Archive は別プールなので退避も実コピー（追記のみで速度は要らない）。

### 移行手順

手順は `docs/MIGRATION.md`、振り分けの判断は D-45。要点:

- 事前チェックは `scripts/preflight.py`（衝突 / 不正 UTF-8 / 255 バイト超 / symlink / hardlink /
  SMB 禁止名 / 容量）、振り分けは `scripts/migrate_plan.py`（`Opus/` と `Original/` の 1:1 対応を
  原本の形式で Library / Archive に割り、`rsync --files-from` の一覧を出す）。手書きの find / awk は
  置かない
- 旧 `Original/` の ALAC が Library の master（後で FLAC 正規化）、webm 由来は `.opus` が master で
  webm は Archive、旧 `Opus/` のロスレス由来分は移さず Derived を再生成する
- `zfs send/recv` は不可（作成時プロパティが継承される）。**ACL は rsync で引き継げない**
  （NFSv4 ACL と `rsync -A` の POSIX ACL は互換がない）ので TrueNAS の ACL エディタで適用し直す
- 旧データセットは `readonly=on` にして残し、初回スキャンが完走してトラック数が一致し、
  数日間の運用で問題が出ないことを確認してから破棄する

### 環境変数

| 変数 | 用途 |
|---|---|
| `SPINDLE_CONFIG` | `config.toml` のパス（既定 `/data/config.toml`） |
| `SPINDLE_INITIAL_PASSWORD` | 初回起動時の管理パスワード。DB にパスワードが無いときだけ読み、argon2id で保存して以後は無視する。未設定かつ DB にも無ければロックモード（`/health` 以外 503） |

### バックアップ

- SQLite は `backup` ジョブが `VACUUM INTO` でバックアップ（WAL 中の安全なコピー手段。
  読み取りコネクションで走らせ、書き手を止めない）。tmp に書いて `quick_check` → fsync →
  rename（`RENAME_NOREPLACE`。同名の確定済みを上書きしない）→ **`backup/` とその親も fsync**
  してから世代 GC。空き容量が DB サイズ + 64 MiB を
  下回るなら書かずに失敗する（ジョブのバックオフで再試行）
- 周期は `[backup].interval_hours`（既定 24）。スケジューラが起動時と 10 分ごとに
  「`jobs` の最後の終端 `backup` からの経過」で due を判定して投入する（dedup key `backup`）。
  ファイルの mtime は見ない。復元直後は古い記録しか無いので、すぐ 1 世代取れる
- ファイル名は `spindle-<YYYYMMDDTHHMMSSZ>.db`（UTC、固定幅。名前順 = 時刻順）。
  保持世代は `[backup].retention_generations`（既定 14）で、この名前形式のファイルだけを
  新しい順に残す。`apps/spindle` データセットのスナップショットと二重化
- 復元: コンテナ停止 → `spindle.db`（と `-wal` / `-shm`）を差し替え → 起動。起動時スキャンが
  ファイルとの差分を吸収する。未反映だった編集意図（pending）は失われるが、ファイルは
  旧値のままなので壊れない。手順は `docs/OPERATIONS.md`、復元ドリルは `tests/backup.rs`
- DB は「キャッシュ」だが、プレイリスト / 編集履歴 / 検証結果 / ジョブ履歴は DB にしか
  ない（§3）。バックアップは任意ではない

---

## 15. Rust モジュール構成

```
src/
├── main.rs
├── config.rs            config.toml の読み込みと検証（D-34）
├── logging.rs           tracing の初期化
├── db/
│   ├── mod.rs           コネクション管理（write 単一 / read プール）
│   ├── migrations.rs    リポジトリ直下 db/migrations/*.sql の埋め込みと適用
│   ├── tracks.rs        一覧（キーセット）・検索・selection 解決・アルバム一覧（D-39）
│   ├── archive.rs       archived_files 台帳（退避・復元・GC の根拠）
│   ├── categories.rs    統制語彙（canonical key で一意）
│   ├── inbox.rs         inbox_items / inbox_files（承認キューの状態機械。D-68）
│   ├── playlists.rs  jobs.rs  history.rs
├── domain/
│   ├── identity.rs      inode / audio_md5 による同一性解決
│   ├── filter.rs        一覧のフィルタ（JSON）/ ソート / カーソルの検証
│   ├── selection.rs     selection の 2 形、preview のスナップショット store（D-33）
│   ├── pathgen.rs       テンプレート展開・正規化・衝突回避
│   ├── tags.rs          lofty ラッパ、正規化、多値処理
│   ├── replaygain.rs    ebur128、フォーマット別変換
│   └── category.rs      統制語彙、GENRE 写像
├── edit/
│   ├── mod.rs           編集バッチの coordinator（記録・DB 先行更新・反映・overlay 解消・
│   │                    キャンセル・起動時リカバリ。D-24 / D-41）
│   ├── rename.rs        一括リネームの計画・記録・2 phase 反映・album の追随（D-43）
│   ├── normalize.rs     ロスレス → FLAC 正規化の計画・記録・反映・Archive 退避（D-46）
│   ├── md5fill.rs       FLAC の MD5 補填（md5 op。D-59）
│   ├── picture.rs       埋め込み画像の差し替え（`PICTURE` の tags op。画像の読み出し・退避。D-60）
│   └── revert.rs        巻き戻し（対象集合・現在値の比較・逆バッチの記録。D-44）
├── media/
│   ├── fingerprint.rs   STREAMINFO MD5 / デコード PCM MD5 / パケット列ハッシュ
│   ├── decode.rs        symphonia / ffmpeg フォールバック
│   ├── encode.rs        flac（ffmpeg デコード → flac -8）/ opus
│   └── artwork.rs       同梱 / 埋め込み画像の選択、判別、ハッシュアドレスのキャッシュ（P1-3）
├── cd/
│   ├── mod.rs           TrackLayout（サンプル単位のトラック列）、照会用 HTTP クライアント
│   ├── device.rs        ioctl / SG_IO / ポーリング
│   ├── toc.rs           TOC の検証、各種 DiscID 算出、サンプル数からの再構成（§7.3）
│   ├── rip.rs           cd-paranoia、オフセット、分割
│   ├── accuraterip.rs   ARv1/v2 CRC、DB の照会（dBAR-*.bin）、オフセット表
│   ├── ctdb.rs          CRC32、照会（lookup2.php）、パリティファイルの取得
│   ├── repair.rs        CTDB の修復（GF(2^16) RS: シンドローム表、オフセット探索、復号、適用。D-66）
│   ├── crctable.rs      1 回流して任意オフセットのトラック CRC を出す表（累積和と CRC32 combine）
│   ├── verify.rs        DB の応答との照合（オフセット探索、トラックごとの一致、`tracks.verification` の写像）
│   ├── musicbrainz.rs   ws/2/discid の照会（UA、1 req/s、503 の再試行）と候補（リリース × medium）
│   ├── metadata.rs      確定フォームの DiscMetadata（検証、タグ写像 + TRACKTOTAL / MUSICBRAINZ_DISCID。D-65 / D-67）
│   ├── riplog.rs        RipReport と rip.log / disc.cue / disc.toc の描画、同梱ファイルの名前（D-67）
│   └── place.rs         配置（分割 MD5 → flac → pathgen::plan → library 排他 → tmp + RENAME_NOREPLACE →
│                        1 トランザクション登録 → rg / transcode。MD5 で冪等。D-67）
├── import/
│   ├── scanner.rs
│   ├── placement.rs     CD / Inbox 共通の配置（tmp + RENAME_NOREPLACE、album の解決と登録トランザクションでの
│   │                    リリースキー再検証、行の登録 / 採用。D-67 / D-68）
│   ├── inbox.rs         Inbox の走査（件 = ディレクトリ）、タグからの下書き、承認の検証、承認済みの配置
│   │                    （補正をタグに書いて pathgen::plan の宛先へ。source_type = download。D-68）
│   └── ytmusic/         metadata.rs（メタデータプラグインのプロトコル v1 と呼び出し。D-69）、downloader.rs
├── jobs/
│   ├── queue.rs  worker.rs  recovery.rs  scheduler.rs（backup / gc の周期投入。inbox は handlers/inbox.rs）
│   └── handlers/
├── gc/
│   └── mod.rs           物理削除の唯一の経路。plan（判定・dry-run）と execute_*（5 区分。D-56）
├── playlist/
│   ├── dsl.rs           pest 文法 → AST
│   ├── compile.rs       AST → パラメータ化 SQL
│   ├── fb2k.rs          AST → foobar クエリ + ソートパターン
│   └── export.rs        m3u8 / pls / パスマッピング
├── api/
│   ├── mod.rs  tracks.rs  albums.rs  categories.rs  selection.rs  batch.rs  rename.rs  normalize.rs
│   │   history.rs  stream.rs  cd.rs  inbox.rs  events.rs
│   ├── auth.rs          argon2id / セッション Cookie / CSRF / trusted_cidrs のミドルウェア
│   ├── state.rs  error.rs   AppState、`{ "error": code }` 応答
└── web/                 SPA を rust-embed で同梱
```

---

## 16. 実装順

| フェーズ | 内容 | 完了条件 |
|---|---|---|
| **P0** | データセット構築、移行、スキャナ、DB、表 UI、タグ一括編集、リネーム、編集履歴 | 既存ライブラリ全曲が表に出て、一括編集と巻き戻しができる |
| **P1** | ReplayGain、アートワーク、WAV 正規化、プレイリスト（m3u8 出力）、再生、Derived 生成・追随、GC | foobar2000 を開かずに日常運用が回る |
| **P2** | CD 取り込み（TOC → MB → rip → AR/CTDB → エンコード）、遡及照合 | 新規 CD が検証付きで取り込め、既存 FLAC が格付けされる |
| **P3** | ytmusic 移植、ダウンロード後の Derived 投入、偽ハイレゾ検出 | ytmusic CLI を廃止できる |

P0 を先に置くのは、リップの出口（タグ付け・配置・RG）がすべて P0 の成果物であり、
先に作った方が結果的に早いため。

---

## 17. 決定済み事項と残課題

### 決定済み

| 項目 | 決定 | 根拠 |
|---|---|---|
| 名称 | **spindle** | CD を積むスピンドル。バイナリ名として扱いやすい |
| ライブラリ構造 | 役割別 3 層（Library / Derived / Archive） | フォーマット別だと CD の FLAC が保管物かつ再生対象で破綻する |
| Category 軸 | ジャンル別・統制語彙 | GENRE タグとは別フィールド |
| アルバム名 | `{album}` のみ、衝突時のみ年を付与 | |
| Derived | 可逆のみ変換 + 配布ビュー解決 | 非可逆の多重劣化を回避 |
| ビットレート | Opus 128k VBR | 再生成可能なので低リスクな決定 |
| WAV | FLAC へ正規化（MD5 照合付き）。元 WAV は Archive へ退避し GC 待ち | タグ・RG の互換性が低いため。即時削除は禁止事項と矛盾 |
| ジョブ dedup | `queued`/`running` の間だけ一意 | 列 UNIQUE だと完了後に同キーを再投入できない |
| 一括編集 | DB 先行更新 + `edit_ops.pending`（トラック単位）、pending 中の再編集は 409、スキャナは pending の論理値を巻き戻さない | ファイル反映待ちの窓で「ファイルが正」と衝突する |
| 配布ビュー | 音声版一致の Derived のみ。音声が古ければ原本へ | 再エンコード待ちの古い音声を配らない |
| スキャン | 4 相（inventory → 候補 → claim → commit）、`scan_runs` で claim、missing は completed でのみ | 走査途中では「移動元の消失」を判定できない |
| 一括編集の対象 | preview 時のスナップショット（`selection_token`） | 見ていない行を破壊的操作の対象にしない |
| 認証スキップ | `trusted_cidrs` は route allowlist のみ、CSRF は Origin 完全一致 | 全 GET 開放は履歴・パスが漏れる。Host は防御にならない |
| 記法 | foobar 風 DSL → AST → SQL | 生 SQL 保存はスキーマ変更で壊れる |
| 認証 | 単一パスワード + セッション Cookie | 事故対策。破壊的 API を裸で置かない |
| Subsonic | 非対応 | ファイル同期を継続。認証も argon2 単独で閉じる |
| メタデータ | MusicBrainz + 手入力の一級市民化 | 同人・VTuber 盤は未登録が常態 |
| マルチch | Derived 対象外・album RG 除外、個別オプトイン | |
| FLAC 再圧縮 | 一括再エンコードはしない。MD5 未設定のみ補填 | 削減 1% 未満に対し 200GB の書き直しとスナップショット肥大 |
| インポート | Inbox + 承認キュー方式 | 外部ツリーを直接スキャンすると規約とタグ品質が混入する |
| SMB | `casesensitivity=insensitive` / `normalization=formD` | 他クライアントが作るファイルと NFC/NFD 混在の事故を ZFS 層で潰す |
| 移行方式 | 新規データセット作成 + rsync コピー | 上記 2 プロパティは作成時のみ指定可能 |

### 残課題

- [ ] Discogs / VGMdb 連携（P3 以降の任意。国内盤カタログ番号とアートワーク補完）
- [ ] `.fpl` 書き出し（P4 の任意。バイナリ形式の解析コストに見合うか要判断）
- [x] `HAS` 等の演算子の foobar 実機との挙動突き合わせ（2026-09-19。部分一致で一致。D-55）
- [ ] 偽ハイレゾ検出のしきい値設計（P3）
- [ ] 移行後の NFSv4 ACL 再適用（rsync では引き継げない）
- [x] Inbox のポーリング間隔（2026-09-19。`[inbox].poll_interval_secs` 既定 60 秒 + 手動。D-68）
- [x] 一括リネームで album 全体を動かした後、旧ディレクトリに残る同梱ファイル（cover.jpg /
      disc.cue / rip.log 等）の追随と空ディレクトリの扱い（2026-09-19。rename ジョブが commit 後に
      既知の名前を追随させ、空なら rmdir。D-67）

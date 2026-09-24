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
| Derived | Library から生成される配布用の非可逆（`opus` 系統と Apple 向け `aac` 系統。§7.6）。破棄・再生成可能 |
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

### パステンプレート

```toml
[layout]
multi_disc  = "{category}/{albumartist}/{album}/{disc}-{track:02}. {title}"
single_disc = "{category}/{albumartist}/{album}/{track:02}. {title}"
unsorted    = "_Unsorted/{albumartist}/{album}/{track:02}. {title}"
```

- 階層は Artist ではなく **AlbumArtist**。コンピレーションは `Various Artists`
- マルチディスクはサブフォルダを作らず `1-01` 前置き（アルバム = 1ディレクトリを維持）
- 発売年はパスに含めない。DATE / ORIGINALDATE タグは必ず保持する
- パス衝突時のみ自動降格: `{album}` → `{album} ({year})` → `{album} ({edition})`
- **衝突 = マージではない。** 配置前に MusicBrainz Release ID（無ければ album 行）で
  同一リリース判定を行い、異なる場合は必ず別ディレクトリにする。DiscID は 1 枚ごとの値なので
  リリースの鍵にしない（D-67 追記 3）

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
アルバムの行、`Archive/` の退避ファイル、Derived の孤児、アートワークの孤児、却下して「削除」した
Inbox の件のファイル（D-90）を回収する。
1 日 1 回自動、`POST /api/gc` で手動、`GET /api/gc/preview` が dry-run。D-56）。GC は保持期間を過ぎた
終端の**ジョブ行**も消す（done / cancelled は `[gc].jobs_done_days`、failed は `jobs_failed_days`。0 で
消さない。P4-18、D-81）。

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
  古い解析値を Derived や再生に使わない。album の他のトラックの `rg_album_*` は次の album 解析で揃う。
  捨てた行は**同じトランザクションで rg ジョブを積んで解析し直す**（投入単位と dedup は `POST /api/rg` と
  同じ。タグへの書き込みは `[replaygain].write_tags` に従う。aac の Derived は解析が揃ってから作られる。
  新規トラックや未解析の行には自動で積まない。D-47 追記 2、P4-22）
- album gain は `album_id` 単位で、**`albums.album_gain` が true の album だけ**計算・書き出しする（既定
  false。CD 取り込みは true、Inbox の承認画面で選ぶ。D-74、P4-5）。**2ch 以外は album 集計から除外**（判定はデコード結果の
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
  2. mb_release_id が一致する既存 album が**ちょうど 1 つ**で、その旧 rel_dir が
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
[ディスク検出]  CDROM_DRIVE_STATUS ioctl を 2 秒間隔ポーリング（`cd/device.rs`）
   ↓            （udev がコンテナに届かないため）。デバイスを開けなければ no_drive として起動は続ける
[TOC 取得]      CDROMREADTOCHDR / CDROMREADTOCENTRY ioctl（LBA 形式。中身は READ TOC コマンドで、
   ↓            /dev/sg* も外部プロセスも要らない）。DiscOk になってから読めるまで毎周回試し、
   ↓            読めたらディスクが抜かれるまで読み直さない。`GET /api/cd/status` が CTDB 形式の
   ↓            文字列で返し、UI は新しいディスクの TOC が出たら照会を自動で始める
   ↓
[ID 算出]       MusicBrainz DiscID / AccurateRip id1,id2 / FreeDB ID
   ↓            すべて TOC からの整数演算。libdiscid FFI 不要
[メタデータ照会] MusicBrainz → 候補提示 → ユーザ確認・手動補正
   ↓            候補は「リリース × medium」（DiscID を持つ medium は exact、トラック数の合う medium は
   ↓            近似。D-64）。**経路は 3 段で、上の段で候補が残れば下は引かない**（D-64 追記 4）:
   ↓
   ↓              段        引くもの                              次の段へ進む条件
   ↓              discid    ws/2/discid/<id>                      404、または 200 でも候補 0 件
   ↓              ids       ISRC（recording 検索）+ MCN = バーコード  候補 0 件
   ↓                        （release 検索）
   ↓              toc       ws/2/discid/<id>?toc=（TOC 近似）      —
   ↓
   ↓            「候補」はトラック数で絞ったあとの数で数える。DiscID が 200 でも候補 0 件なら次の段へ
   ↓            落とすが、exact は真のままにする（DiscID は登録済みなので登録を勧めない）。ユーザが
   ↓            貼ったリリース URL / MBID は段に関係なく常に足し、toc へ進むかの判定では候補に数える
   ↓            （ids は引く）。同じ medium は 1 件に束ねて経路（matched_by）を付ける（D-64 追記）。
   ↓            TOC 近似はトラック長の近い別の盤を大量に返すので最後の手段。応答の stage / can_widen で
   ↓            画面に段を見せ、「さらに広げて探す」（widen）で明示的に下の段まで引ける。候補を選んだら DiscID の登録リンク
   ↓            ※同人・VTuber・インディーズ国内盤は MusicBrainz 未登録が常態。
   ↓              照会結果ゼロでもウィザードが完走できることを必須要件とする。
   ↓              候補を写す先は 1 つのフォーム（D-65。写す範囲は既定で「全部写す」、「識別用の
   ↓              最小限」を選べる。D-72 追記 2、P4-2 / P4-20）。**CD 画面では直せない**（D-67 追記。
   ↓              補正とトラックリスト貼り付けは Inbox の承認画面）。これが
   ↓              DiscMetadata（album / album_artist / date / label / catalog_number / barcode /
   ↓              disc_no / disc_count / tracks[{ number, title, artist, mb }]、source）になる。
   ↓              トラックリスト貼り付け（通販ページ等からのテキストを行解析して
   ↓              トラック番号・タイトル・アーティストへ割り付け。web/src/lib/tracklist.ts）は
   ↓              **Inbox の承認画面の**一級の入力経路とする（D-67 追記。CD 画面からは外した）。
   ↓              行は TOC の音声トラックと 1:1 で、番号と長さは TOC から
   ↓
[オフセット決定] INQUIRY でドライブ型番取得 → 学習済みの値（drive_offsets）→ AccurateRip の
   ↓            ドライブ表（DriveOffsets.bin を実行時に取得・保存）→ 0（D-83 / 追記 2）
   ↓            `[rip].drive_offset` の整数で上書き可。照合で見つかったずれは PCM に当てて学習する
[吸い出し]      cd-paranoia -e -r -d <dev> -- <first>-<last>[mm:ss.ff] で音声トラック全体を 1 本の PCM に
   ↓            （オフセット 0 で読み、範囲は TOC の長さで明示。`cd::rip::read_disc`。D-83）
   ↓            ※トラック単位で吸うとオフセット補正が境界をまたげない
[オフセット適用 → トラック分割]
   ↓
[CRC 計算]      ARv1 / ARv2 / CTDB CRC32
   ↓            先頭トラック冒頭・末尾トラック終端の除外規則を厳守
[照合]          CTDB を主、AccurateRip を補助
   ↓
[エンコード]    FLAC (master) を **Inbox へ**（D-67 追記。Library へ直行しない）。
   ↓            tmp の PCM は検証完了まで保持
[ログ出力]      rip.log / disc.cue / disc.toc を件のディレクトリへ。MusicBrainz から写した内容と
   ↓            RipReport（照合結果）は サイドカー spindle-inbox.json に書く（D-70 の仕組み）
[承認]          Inbox の承認画面で名前と category を直して「承認して配置」
   ↓            → 配置が Library へ運び、source_type = cd_rip と検証記録を入れる
[後続ジョブ投入] rg（album 単位。配置した album は album gain on。D-74）→ transcode(Derived) → thumbnail
```

Inbox への配置（`src/cd/place.rs` の `place_disc`、D-67 追記、P2-5）: rip ジョブの最終段。
入力は `Toc`、吸い出しを始めたときの `DiscMetadata`（**名前は空でもよい**。`validate` は TOC との
行の対応・日付・ディスク番号・category だけを見る。必須の検証は Inbox の承認が担う）、オフセット
適用済みの tmp の PCM、`RipReport`。PCM を `TrackLayout` で切り、raw のまま `flac -N --verify` で
エンコード（トラックごとの PCM の MD5 と STREAMINFO の MD5 が一致するときだけ成果物）、タグは
下書きの写像（D-65）+ `TRACKTOTAL` / `MUSICBRAINZ_DISCID`。空のタイトルは `Track NN`、空の名前は
タグに書かない。`category` はタグに書かず（パス専用。SPEC §5）サイドカーで渡す。

- **組み立ててから公開する。** Inbox 直下の隠しディレクトリ `.spindle-rip-<DiscID>` に `NN.flac`
  （TOC の番号。名前での対応付けの鍵なので承認で変わる値を入れない）、同梱ファイル、サイドカー
  （`category` と `rip`。§7.8）を置き、`CD/<albumartist - album> [<DiscID>]`（名前が空なら
  `CD/[<DiscID>]`。DiscID は `.` で始まり得るので括弧で包む）へディレクトリごと `RENAME_NOREPLACE`。
  走査は `.` で始まるディレクトリを見ないので、揃う前の盤が件として見えて承認されることはない。
  公開したら `inbox` ジョブを投入する。複数枚組は DiscID が違うのでディスクごとに別の件になる
- **冪等性。** 公開先が既にあり、全トラックの STREAMINFO の MD5 が自分の PCM と一致し、サイドカーの
  記録が同じファイル名を持てば自分の成果物（公開の後に落ちた再実行）として組み立てを飛ばす。違えば
  衝突。前の実行の組み立ての残骸は消して作り直し、公開が衝突したら組み立てたものを消す
- `albums` / `tracks` / 検証記録の登録、パスの計画（`[layout]`）、`library` の排他、`rg` / `transcode` の
  投入は承認後の Inbox の配置（§7.8）が行う

同梱ファイルは 1 枚なら `disc.cue` /
`disc.toc` / `rip.log`、複数枚組は `disc<N>.cue` / `disc<N>.toc` / `rip<N>.log`。`disc.cue` は
EAC 流の複数ファイル cue（ギャップは前トラック末尾。INDEX 00 は書かない）、`disc.toc` は `Toc` から
cdrdao 構文で生成、`rip.log` は先頭行 `spindle rip log v1` の自前形式でドライブ・オフセット・
トラックごとの CRC と照合結果を持つ。トラック表の「ずれ」は、cd-paranoia が読み取り位置のずれ（ドライブの
ジッター）を検出・補正した回数（drift / dropped / duped。`TrackRead.slips`。補正の通知なので回数だけでは誤りを
意味しないが、多発と照合の不一致が併発した）。1 回でもあれば表の下に合計と
意味を、試行ごとの合計（`RipReport.attempt_slips`）を「試行ごとのずれ」に出す。照合が通らずずれがある件は、
Inbox の承認画面の警告にも出る（P2-5 の調査。TASKS）。スキャナはこの rip.log のあるディレクトリで新規に登録する行を
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
- **形式ごとの写像は読み書きで 1 つ。** FLAC / Opus / Ogg は Vorbis Comment に任意キー・多値を
  そのまま書く。MP4（ALAC / AAC）は Vorbis 名を lofty の `ItemKey` に写像して ilst の標準 atom
  （`©nam` `trkn` `disk` `----:com.apple.iTunes:CATALOGNUMBER` 等）に、写像表に無いキーは
  `----:com.apple.iTunes:<KEY>` のフリーフォーム atom に書く（多値は 1 atom の複数値。`iTunNORM` は
  綴りを固定。D-77）。読み側は同じ写像で、フリーフォーム atom を名前を大文字化したキーとして取り込む。
  置き換え・削除は名前の大小文字を無視して既存 atom を消す（外部ツールが `MyKey` で書いた atom を
  `MYKEY` で上書きしても二重化しない）。`trkn` / `disk` は番号と総数が同居するので、片側だけの変更は
  相方を保つ。写像と書き手（`apply_ilst`）は Derived の `aac` 系統（§7.6）と共有する。MP3 / WAV 等は
  lofty の generic `Tag` 経由で、写像できないキーは書けない（読み戻し照合で failed に閉じる）
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

Derived は**系統（variant）**ごとに 1 本ずつ作る。系統は `opus`（Android の同期・Web 再生・配布ビュー）と
`aac`（Mac のミュージック.app へ取り込む Apple 向け）の 2 つで固定（P4-7 で系統化、P4-8 で `aac` の実体。
D-9 追記、D-75）。系統の設定は `config.toml` が正で、起動時に `derived_variants` 表へ写す（投入判定は
どこからでも同じ接続で設定を引ける。D-55 の `fb2k_prefix` と同じ流儀）。設定に無い系統は投入も生成も
しない。

| 項目 | 規則 |
|---|---|
| パス | `Derived/<variant>/` 以下に Library と完全ミラー（拡張子だけ `.opus` / `.m4a`）。`paths.derived` は 1 つ |
| 設定 | `[encode.derived.<variant>]` に `enabled` と `bitrate`（下記）。節を省略したときの `enabled` は **`opus` が true、`aac` が false**（既存の config をそのまま新版で起動しても aac は始まらない。RG 全件解析 → 有効化の順序を崩さない）。`enabled = false` の系統は**凍結**: 新しく作らず、既存の行とファイルは Move / Retag / Encode のどれも行わず、配布ビュー・バッジ・`has_derived` は既存行をそのまま使う。GC は系統を区別せず `Derived/` 全体で「行に無いファイル = 孤児」を回収する（凍結でも行は残るので消えない） |
| 出力仕様の世代 | 系統ごとに 2 つの文字列を設定から作り、行に保存する。**`audio_profile`**（音声に効く設定: codec / bitrate / サンプルレート規則 / RG 焼き込み方式の版。例 `opus:256:v1`、`aac:256:48k:bake1`）と **`tag_profile`**（タグに効く設定: `multi_value_separator` / iTunNORM 規則の版。例 `aac:sep= & :itunnorm0:v1`）。エンコーダの引数や規則を変えるときは版を上げる |
| 判定 | `audio_version` 差分・**行の `audio_profile` が設定と食い違う** → 再エンコード / `tag_version`・埋めた画像（`src_artwork_id`）・RG の解析世代（`src_rg_scanned_at`）・**`tag_profile`** の差分のみ → タグ上書き（**`aac` は RG を音声に焼き込むので RG 世代の差分は再エンコード**）/ パスの差分のみ → rename。優先順はこの順（再エンコードは残りを兼ねる） |
| 投入 | scan ジョブの完了時に食い違う全トラック × 系統、tagwrite / rename の applied、RG 解析の保存（D-51）。ジョブは `transcode`（`(track_id, variant)` 単位、`audio_version` で dedup）で、ハンドラが現在値から必要な処理を決める。P4-1 の共通並列予算の対象 |
| 追随 | Library の移動に追随（Derived を rename）。削除には追随せず（missing は可逆）、`retention_days` 超の回収と孤児（行に無いファイル。`Derived/` 全体）は GC ジョブ |
| マルチch | どの系統も既定で対象外（チャンネル数不明も対象外）。トラック単位の `-ac 2` ダウンミックスは需要が出たら（D-51。実データは全件 2ch） |
| 配布ビュー | `delivery` は **`opus` 系統に固定**（Android の m3u8、`?transcode=opus`、`has_derived`）。`aac` は配布ビューを持たず、プレイリストも出さない（ミュージック.app へはファイルを取り込むだけ。D-75） |

**`opus` 系統**

| 項目 | 規則 |
|---|---|
| 対象 | Library 内の可逆のみ（flac / alac / wav）。opus / aac / mp3 は原本をそのまま配布（D-8） |
| 出力 | `opusenc --vbr --music --bitrate <bitrate>`。既定 **256 kbps**（D-9 追記。可逆 237 GB で約 58 GB） |
| RG | 再解析しない。Library 側の解析値を `R128_*` へ変換して埋める（`REPLAYGAIN_*` は書かない） |
| 画像 | トラック自身の埋め込み画像（`tracks.artwork_id`。D-61）、無ければ album のアートワーク（§7.1）の長辺 768 の WebP を 1 枚だけ埋める（D-51） |

**`aac` 系統**（Apple 向け。D-75）

| 項目 | 規則 |
|---|---|
| 対象 | 可逆（flac / alac / wav）に加え、`lossy_sources = true` なら**非可逆も**（opus / ogg / mp3 / aac → AAC。世代劣化は承知の上で、ミュージック.app が Opus を読めないため。**D-8 の例外**）。原本が AAC でも同じ経路で再エンコードする（stream copy では RG の焼き込みとリサンプルができない。D-75）。`lossy_sources` を true → false にしても既存の非可逆の行とファイルは消さない（対象外 = Skip で凍結と同じ扱い。物理削除は GC のみ） |
| 出力 | ffmpeg **1 パス**（中間 WAV なし。FD を stdin に繋ぐのは opus と同じ）: `-i /dev/stdin -map 0:a:0 -vn -map_metadata -1 -af volume=<gain>dB [-ar 48000] -c:a aac -b:a <bitrate>k -f mp4`（内蔵エンコーダ。既定 256 kbps）。48 kHz 超は 48 kHz へ落とす（`-ar 48000`）、44.1 / 48 は据え置き。`audio_profile` は `aac:<bitrate>:48k:bake1`、`tag_profile` は `aac:sep=<区切り>:itunnorm0:v1` |
| RG | **track gain を音声に焼き込む**（`-af volume=<gain>dB`。gain は `min(rg_track_gain, −20·log10(rg_track_peak))` で**エンコーダ入力を 0 dBTP 以下に抑える**（peak は true peak なので 1.0 超なら減衰側に倒れる。AAC 再符号化後のオーバーシュートまでは保証しない。gain が有限でなければ 0、peak は有限かつ > 0 のときだけ上限を掛ける）。album gain は使わない）。**RG 未解析のトラックは作らず待つ**（「解析済み」= `rg_scanned_at` と `rg_track_gain` / `rg_track_peak` の 3 つが揃っていること。時刻だけ残った行は対象外。rg の保存で投入される。二度エンコードの回避）。RG の解析世代（`src_rg_scanned_at`）の差分は**再エンコード**（album gain の on / off も世代を進めるので、その album の aac は作り直される。track gain しか使わないが値ベースの判定に列を足すより単純で、まれな操作なので許容。D-75）。タグには `iTunNORM` を **0 dB 相当**で書き、端末のサウンドチェック ON でも二重に掛からないようにする。値は 10 個の 8 桁 16 進を空白区切り（先頭にも空白）で、1〜2 値目（基準 1/1000）は `000003E8`、3〜4 値目（同じ量の基準 1/2500 の表現）は `000009C4`、残り 6 値は `00000000`: `" 000003E8 000003E8 000009C4 000009C4 00000000 00000000 00000000 00000000 00000000 00000000"`。`REPLAYGAIN_*` / `R128_*` は書かない |
| タグ | Library のタグを写す（`REPLAYGAIN_*` / `R128_*` / 既存の `ITUNNORM` は落とす）。**同じキーの複数値は出現順に `multi_value_separator`（既定 `" & "`）で 1 値に結合**（ミュージック.app は複数値の 1 つしか見せない。ARTIST / ALBUMARTIST / GENRE / COMPOSER など多値になり得る全フィールド）。写像は Library の tagwrite と同じ（§7.5「形式ごとの写像」）: Vorbis 名を lofty の `ItemKey` に写像して ilst の標準 atom（`©ART` `trkn` `disk` `©gen` 等）に書き、写像できないキーは `----:com.apple.iTunes:<KEY>` のフリーフォーム。`iTunNORM` は内部キーが大文字化されても atom 名を `iTunNORM` に固定する（大小文字を special-case） |
| 画像 | `opus` と同じ選び方で、長辺 768 の **JPEG**（ミュージック.app は `covr` の WebP を読まない）。`thumbs/<hex>/768.jpg` をキャッシュに足す（thumbnail ジョブと同じ変換に形式を足したもの。`-pix_fmt yuvj420p -q:v 2`） |

```toml
[encode]
flac_compression = 8

[encode.derived.opus]
enabled = true
bitrate = 256                   # opusenc --vbr --music --bitrate

[encode.derived.aac]
enabled = true
bitrate = 256                   # ffmpeg -c:a aac -b:a
lossy_sources = true            # 非可逆原本も AAC へ（D-8 の例外。aac 原本も再エンコード）
multi_value_separator = " & "   # 多値フィールドの結合
```

`derived_variants` の `lossy_sources` / `multi_value_separator` は aac 系統の設定（`eligible` が前者を、ハンドラが後者を
表から引く。設定は config が正で、起動時に両系統を写す）。

`derived_files` の主キーは `(track_id, variant)` で、配布ビュー `delivery` は `variant = 'opus'` の行を見る。
系統を持つ前の行（ルート直下・`audio_profile = 'opus:128:v1'`）は、既定の 256k では次の transcode が
`audio_profile` の差分で再エンコードして `opus/` 配下へ置き、旧ファイルは transcode の退避経路で消える。
`bitrate = 128` のまま（`audio_profile` が一致）ならパスの差分だけなので Move で `opus/` 配下へ移る（D-75）。

`has_derived`（DSL / フラグ / バッジ）は **`opus` 系統の行**の有無（= `opus` 系統を生成済みか。音声版が古い間は
配布ビューが原本へ倒れるが、`has_derived` は行の有無のまま）。
`aac` しか無いトラック（非可逆原本）は `has_derived` にならない。`GET /api/tracks` の `derived` は系統ごとに
集約して返す（2 系統あっても行は 1 つ）。

容量逼迫時のみ、トラック単位で `force_transcode` を手動指定可能（**元が 256kbps 以上の場合のみ**）
という `opus` 系統の例外は、需要が出るまで実装しない（D-51。実データの非可逆は Opus が大半で削減にならない）。

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
  DATE / TRACKNUMBER / DISCNUMBER、画像の PICTURE / METADATA_BLOCK_PICTURE、ReplayGain の
  REPLAYGAIN_* / R128_*）や不正なキー・空の値があるものは
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
  初回起動の空 DB でも動く）。トラック番号と配置は Inbox が行う（下記、§7.8、D-70）
- 参照実装: `AkashiSN/spindle-ytmusic-meta`（private。ルール TOML + フィクスチャ + チャンネル定義を同梱した
  Rust のバイナリ。新パターンは Claude がそのリポジトリでルールとフィクスチャを 1 件ずつ足す運用）。
  イメージには同梱せず、static バイナリをホストに置いて compose でマウントする（§14、D-70）

**ダウンローダ**（`src/import/ytmusic/downloader.rs`、`ytdl` ジョブ、P3-3、D-70）。Library には触らず、
**Inbox に置くところまで**。配置・登録・後続ジョブは Inbox（§7.8）が担い、プラグインが判定したものも
人が一度見てから Library に入る。

- **入口**: `POST /api/ytmusic/download { urls }` が URL ごとに `ytdl` ジョブ（並列 1、dedup `ytdl:<url>`）を
  投入。YouTube 画面（1 行 1 URL。§12.6）から呼ぶ
- **手順**（ジョブ 1 件 = URL 1 件。作業領域は `[paths].data/tmp/ytdl/<job_id>/`。全部引数配列、`--` の後に URL）
  1. `yt-dlp --dump-single-json --flat-playlist --no-download -- <url>`（120 秒）。`_type` が playlist なら
     `entries` の `url` ごとに `ytdl` を投入して終わり（展開だけ）。動画なら `id` / `webpage_url` / `uploader`
     （channel）/ `channel`（channel_title）/ `title` / `upload_date` / `duration` を取る。stderr が
     `Unsupported URL` / `is not a valid URL` なら `Fatal`、他の失敗は `Failed`（再試行）
  2. プラグインに問い合わせ（Request は上記。`channel` は `uploader`）。`ok` → 宛先
     `Inbox/youtube/<albumartist>/<album>/`（`sanitize_component`）。`ok: false` の `skip` → ダウンロードせず
     `done`（ログに message）。それ以外の reason（`unmatched` / `unknown_channel` / 未知）→ 宛先
     `Inbox/youtube/_unmatched/<channel>/`（受け皿）。プラグインの故障（`ProviderError`）→ `Fatal`。
     ここで Inbox のファイル名（8.）まで決まる
  3. 取り込み済みチェック: `SOURCE_URL = webpage_url` が Library（`track_tags`）にあれば**やることが無い**ので
     `done`（note に「取り込み済み（Library）: <パス>」。失敗ではない。P4-18、D-81）。Inbox（`inbox_files.tags`）にあり、それが自分の宛先そのもの
     （`rel_path_key` で比べる）なら「置いた後に落ちて走査が先に拾った」再実行の可能性があるので、
     **実ファイルの `SOURCE_URL` を読み直して**同じならダウンロードせずに 8. の仕上げ（サイドカーと投入）
     だけ済ませて `done`。違えば `Fatal`（同名で別の内容）、ファイルが消えていれば普通に置き直す
     （行はキャッシュ、ファイルが正）。Inbox の別の場所なら `done`（「取り込み済み（Inbox）: <パス>」）
  4. `yt-dlp -f "ba[ext=webm]" --no-playlist --write-thumbnail --convert-thumbnails jpg -o <tmp>/%(id)s.%(ext)s -- <url>`
     （`[ytmusic].download_timeout_secs`、既定 900）。webm の音声が無ければ `Fatal`。ネットワーク等の失敗は
     `Failed`（再試行）
  5. `ffmpeg -nostdin -y -i <tmp>/<id>.webm -vn -c:a copy -map_metadata -1 <tmp>/<id>.opus`（再エンコードなし）
  6. lofty でタグ。`ok`: `Track::tags(なし)` の TITLE / ARTIST / ALBUM / ALBUMARTIST / DATE / 追加 tags
     （TRACKNUMBER は書かない。採番は Inbox）+ PICTURE（サムネイル jpg）+ `SOURCE_URL`。`ok: false`: TITLE =
     動画タイトル + PICTURE + `SOURCE_URL` だけ（他は Inbox の警告で人が埋める）
  7. webm を `Archive/youtube/<id>.webm` へ（tmp + `RENAME_NOREPLACE`。既にあれば同じ id なので採用）
  8. `.opus` を宛先へ `<YYYYMMDD> <title> [<id>].opus`（title は `sanitize_component`。名前順 = 公開順 =
     採番順。`RENAME_NOREPLACE`。既にあれば、その `SOURCE_URL` が同じときだけ自分の成果物（置いた後に
     落ちた再実行）として採用し、違えば `Fatal`）。仕上げ: 同じディレクトリの `spindle-inbox.json` を読んで
     このファイルの項を足し tmp + rename で書く → ディレクトリを fsync → `inbox` ジョブを投入（すぐ件が
     出る）。仕上げの失敗は `Failed`（再試行。次の実行は 3. か 8. の採用でここへ戻る）
- **サイドカー `spindle-inbox.json`**（タグに載らない情報を Inbox へ渡す。Inbox の DB には書かない）
  ```jsonc
  { "version": 1,
    "category": "<統制語彙の名前>" | null,          // 件の category（プラグインの判定。最後に書いたものが勝つ）
    "files": { "<ファイル名>": { "source": "youtube", "url": "<webpage_url>", "channel": "<uploader>",
                                 "verdict": "ok" | "unmatched" | "unknown_channel" | "skip" | "<未知の reason>",
                                 "message": "…" | null,
                                 "subscription_id": 3, "position": 17 } } }   // 購読由来のときだけ（P4-16）
  ```
- **失敗の区分**: `Fatal`（再試行なし）は webm の音声なし・プラグインの故障・対応していない URL・宛先の
  同名で別の内容（取り込み済みは失敗でなく `done` + note）。それ以外（yt-dlp / ffmpeg の非ゼロ終了、I/O、仕上げ）は `Failed` で指数
  バックオフ。作業領域はどの終わり方でも消す。**再実行は冪等**: Archive は同じ id なら採用、Inbox は
  `SOURCE_URL` で自分の成果物を見分けて続きから
- `POST /api/ytmusic/download` の URL は http / https でホストがあり空白・制御文字を含まないものだけ受ける
  （ホストは限定しない。対応していなければジョブが `Fatal` で伝える）
- yt-dlp は YouTube の抽出に JS ランタイムを要求する版があるため、runtime イメージに deno を同梱する（§14）

**再生リストの購読と同期**（`playlist_subscriptions`、`playlist_sync` ジョブ、P4-16、D-78）。URL を貼る
運用をなくす。再生リスト 1 本 → Library の album 1 つ（追記先）。`SOURCE_URL`（P4-14 で補填、ytdl が
書く）で「再生リストのどこまで持っているか」が分かる。

- **購読**（`db::subscriptions`、`/api/ytmusic/subscriptions`）: `id` は再利用しない（AUTOINCREMENT。
  ジョブの payload とサイドカーが裸の id を持つので、DELETE 直後に作った購読へ旧ジョブが誤帰属しない）、
  `list_id`（URL の `list=`。YouTube のホストだけ。UNIQUE）、`albumartist` / `album` / `category`（追記先の初期値と表示用）、`album_id`
  （**追記先の同一性**。登録時は NULL。同期か配置が `import::inbox::destination_of`（Inbox の追記先と
  同じ規則）で引けたときに CAS（`album_id IS NULL` のときだけ）で束ねる。album 全体の移動は id を
  維持する（D-32）ので以後は album 行の値が正。album が消えれば SET NULL で再解決。albumartist /
  album / category を PATCH で変えると NULL に戻す）、`align`（番号揃えをするか。既定 on）、`enabled`
  （定期と承認の後続の対象か。手動の同期は enabled に関わらず受ける）、`max_enqueue`（1 回の同期で
  投入する上限。既定 50。誤登録した巨大なリストで数百本落とさない）。同じ追記先の購読は 1 つだけ
  （`target_key` = `sanitize_component` した albumartist / album の canonical key、UNIQUE。`album_id` も
  非 NULL の間 UNIQUE = 束ね同士の競合は DB で片方が失敗する）。Inbox の `youtube/<albumartist>/<album>`
  = 1 購読 = 1 category になるので、サイドカーの category が dir 単位でも他の購読に波及しない
- **同期ジョブ `playlist_sync`**（並列 1、dedup `playlist_sync:<id>`、payload `{ subscription_id }`）。
  順序は **列挙 → 追記先 → 照合 → 番号揃え → ytdl 投入**（購読由来の ytdl は TRACKNUMBER = 位置を書くので、
  先に既存の行を揃えて隙間を空ける）:
  1. `last_attempted_at = now`、latch（`sync_requested_at`）を NULL に。`yt-dlp --flat-playlist
     --dump-single-json`（`[ytmusic].ytdlp_args` 付き）で列挙。entry の位置（1 始まり）が再生リストの位置。
     `entries < playlist_count` なら**古い yt-dlp の取りこぼし**として `Failed`（再試行）で何も投入しない
  2. 追記先: `album_id` があればその album（missing なら `Fatal`）、無ければ引いて束ねる（別の購読が束ねて
     いれば `Fatal`。読んでから束ねるまでに配置が束ねていればそちらが正）。まだ無ければ揃えは無し（最初の
     配置で束ねる）。同期が queued / running の間は PATCH / DELETE を 409 `sync_running` で拒む（走行中の同期が
     古い追記先で揃えたり、消えた購読の album を揃えたり、死んだ id で投入したりしない。検査と UPDATE は同じ書き込み閉包 = 投入と直列）。加えて各段の
     前に購読を読み直し、消えていれば `Fatal`、`updated_at` が進んでいれば `Failed`（直接 DB を書いた場合の
     控え）
  3. entry ごとに正規 URL `https://www.youtube.com/watch?v=<id>` で Library の active 行を**全件**引く
     （`find_source_url` の LIMIT 1 でなく 0 / 1 / 2 件以上を区別）。追記先にあれば「Library」、別の album
     なら「別の album にある」（触らない、投入しない）、Inbox にあれば「取り込み中」（投入しない）
  4. **番号揃え**（`align` が on。`import::ytmusic::playlist::plan_align`）: 対象 = 追記先の active 行で
     `SOURCE_URL` が entry に一致し disc 1（NULL 含む）で単一のもの。目標番号 = 位置。**固定行** = それ
     以外（`SOURCE_URL` 無し・再生リストに無い・`SOURCE_URL` 重複）の disc 1 の行で、その番号は塞がる。
     目標番号が塞がれた対象と disc 2 以降・重複の対象は動かさず「揃えられない」一覧へ（理由付き）。
     **同じ動画が再生リストに複数回ある**ときも表現できない（1 行は 1 番号）ので、その行は固定して「揃え
     られない」（`duplicate_entry`、位置の一覧）に出し、投入は最初の位置でだけ行う。対象
     同士の swap / cycle は可（番号は UNIQUE でなく、rename は一時パス経由）。非公開・削除・未取り込みの
     位置も数えるので、それらの分は番号が飛ぶ。
     **各段の前に追記先の active 行に pending の op が無くなるまで待ち**（2 秒間隔、上限 10 分。超えたら
     `Failed`）、差分は待った後の DB から計算し直す（phase を永続化しない: 落ちて再実行しても、失敗した op は
     overlay がファイルの値に戻るので次回また差分になり、適用済みなら差分ゼロで通る）。ずれた行だけ
     `Editor::prepare_tags`（`TRACKNUMBER`。説明「再生リスト『…』に番号を揃える」）→ 終端待ち → album を
     読み直し「**現在の TRACKNUMBER が目標位置と一致する対象行のうち、ファイル名の先頭の番号が合っていない
     行**」を `plan_rename` → **ファイル名だけ**テンプレートのものにして（ディレクトリは今のまま。テンプレートの
     dir で動かすと category 未推定の album を `_Unsorted` へ移してしまう）`prepare_rename` → 終端待ち
     （今回のバッチの applied 集合には依らない = tags 適用後・rename 前に落ちた境界を再実行で拾う。名前の
     書式だけ違う行は触らない）。計画の時点の衝突（同名のファイルがある等）は続く状態なので失敗にせず
     「改名できない」として報告だけ。どちらも履歴に載り巻き戻せる。Derived（opus / aac）はタグ上書き・移動で
     追随する。子バッチの終端は **batch_id の pending = 0** で待つ（album の active 行で見ると、対象が
     走査で missing になったときに pending の op を見落とす）。**全件 applied でなければ**（pending が残る、
     外部の書き換えによる衝突・失敗・キャンセル、applied ≠ total）揃えは終わっていないので `Failed`（再試行で差分から取り直す）。揃え終わる前の失敗・
     キャンセルでは投入しない（そこまでのバッチ id と件数は結果に残す）
  5. Library / Inbox / 別 album に無く、**取れる** entry を位置順に `max_enqueue` まで `ytdl` 投入（payload
     `{ url, subscription_id, position }`、dedup は今までどおり `ytdl:<url>`）。超えた分は「次回」。
     Duplicate（手動貼り付け・別の購読の投入が走行中）は**満たしたと数えず「別の投入が走行中」**として
     結果に出すだけ（そのジョブには購読の情報が無い。承認されれば次の同期が Library で拾い、失敗すれば
     dedup が空いて購読の情報付きで再投入。同じ動画が複数の購読に出る場合も同じ規則で、先に取り込んだ側の
     album に置かれ他方には「別の album にある」）
  6. **取れない entry**（`title` が無い、旧版の `[Private video]` / `[Deleted video]`）: 投入しないが位置は
     占める。Library にあれば（補填済み、公開だった頃に取り込んだ）何もしない。無ければ「取れない」一覧
     `{ position, id, kind: private | deleted | unknown }`（現行の yt-dlp は `title: null` で来るので区別
     できず `unknown`。旧版の題名から分かるときだけ private / deleted）。後で公開に戻れば次の同期で拾う
  7. `last_result`（結果 JSON: entries / playlist_count / in_library / in_inbox / elsewhere / enqueued /
     running / deferred / unavailable / align{moved, unchanged, blocked, outsiders, unnumbered, tags, renamed,
     rename}、state = done / failed / cancelled）、成功なら `last_synced_at`、`jobs.note` に 1 行。終わる前に
     latch が立っていれば（走行中に承認の後続・手動要求が来た）`Outcome::Requeue`（試行回数を数えず同じ
     ジョブがもう一度走る）
- **ytdl の購読由来**（payload に `subscription_id`）: ALBUMARTIST / ALBUM / category は購読の追記先（束ねて
  あれば album 行の値）、プラグインの判定は TITLE / ARTIST に使う。**skip / 判定不能でも投入**（再生リストは
  人が選んだもの。TITLE = 動画タイトル、ARTIST = albumartist、verdict はサイドカーに残す）。宛先は
  `Inbox/youtube/<albumartist>/<album>/`。`align` が on なら **TRACKNUMBER = 位置**を書く（同期が先に既存の
  行を揃えて隙間を空けている。承認画面の初期値がそのまま正しい番号になり、塞がっていれば承認の既存検査
  （400）で人が直す）。off なら書かず Inbox が max+1 を振る。購読が消えていれば通常の ytdl として振る舞う（skip も通常どおり。
  購読の有無はプラグインを呼ぶ前に引く）
- **承認はそのまま**（D-70。判断は Inbox で人が行う）。配置（`place_item`）はサイドカーを消す前に件の
  `subscription_id` を集め、inbox ジョブが配置後に追記先を束ね（CAS）、latch を立ててから `playlist_sync`
  を投入する（走行中なら Duplicate だが latch は残る）
- **dispatcher**（常駐、30 秒ごと。`[ytmusic].enabled` のとき）: (a) latch の立った購読を投入（enabled に
  関わらず。active なジョブがあれば Duplicate で何もせず、終端になった次の tick で投入される = handler の
  最終確認から終端までの窓に立った latch、Failed / Cancelled で残った latch も必ず回収される）、(b)
  `[ytmusic].sync_interval_hours`（既定 0 = 手動と承認の後続だけ）が 0 でなければ enabled で
  `last_attempted_at`（成否を問わない開始時刻）から interval 経った購読を投入（最終 retry が failed に
  なっても interval までは新しいジョブを作らない）
- yt-dlp を定期的に叩くので、ブロック時の `ytdlp_args`（P4-13）と yt-dlp の更新（P4-12）が前提

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

- **検出**は `inbox` ジョブ（並列 1・固定キー）。周期の**監視**が `[inbox].poll_interval_secs`（既定 60、0 で
  自動なし）ごとに Inbox の指紋（音声ファイルの集合: パス・inode・size・mtime・ctime。非音声と空ディレクトリは
  効かない）を取り、**前回投入時と違うとき**と、配置待ち（`approved`）・期限切れの `placed` があるときだけ
  投入する（変化の無い毎分のジョブ行で一覧を埋めない。投入が Duplicate なら次の周回で投入し直す。起動直後は
  必ず 1 回。P4-18、D-81。inotify は使わない: D-68 追記）。`POST /api/inbox/scan` は常に投入。
  `GET /api/inbox` の `watch` に最後に確認した時刻と間隔（Inbox 画面が「最後に確認: HH:MM:SS」を出す）。
  ジョブは Inbox を歩き、音声ファイルのあるディレクトリを
  1 件（アルバム候補。root 直下の音声は `""` の 1 件）として `inbox_items` / `inbox_files` に写す。
  stat（inode / size / mtime / ctime）が変わったファイルだけタグを読み直す。**正は Inbox のファイル**で、
  行はキャッシュ: ディレクトリが消えれば行も消す（`placed` は 24 時間残して結果を見せる）。`approved` の件で
  ファイルが変わっていたら `pending` に戻す（再承認）
- **却下した件の削除**（D-90）: `rejected` の件に「削除」で `discard_requested_at`（破棄待ち）を入れる。ファイルは
  すぐには消さず、GC が `[gc].retention_days` 経過後に件のディレクトリの直下の**走査が写した音声**（stat が一致
  するもの）・既知の同梱ファイル・サイドカーを消し、ディレクトリが空なら消し、行を消す（サブディレクトリ = 別の件と
  知らないファイルは残す）。それまでは「削除を取り消す」で `rejected` に戻る。`rejected` から出る遷移（下書きに
  戻す）でも破棄待ちは解ける。走査が破棄待ちの件でファイルの変化（足された・差し替えられた）を見たら、人が
  見ていないものを消さないよう破棄待ちを解いて `rejected` のまま理由を `error` に残す（GC も確かめてから消す:
  音声は `inbox_files` と、同梱ファイル・サイドカーは ctime が破棄の要求より後でないかで照合し、合わなければ
  何も消さずに解く。消す途中で stat が変われば止めて解き、消せないものがあれば止めて行と破棄待ちを残し次の GC で
  続ける。D-90）
- **同名の警告**（P4-19、D-70 追記）: 追記先の album に**同じタイトル**の active な行があれば、そのトラックに
  `same_title: [{ track_id, rel_path, duration_ms }]` を付ける（承認は止めない。画面は「⚠ Library に同名:
  <ファイル名>（長さ）」と、件の見出しに「同名 N」）。鍵は NFKD + casefold + 空白の畳み込み（全角・半角の
  揺れは同じ、`(Cover)` / `【… Live ver.】` の注記は**落とさない** = 別曲扱い）。狙いは `SOURCE_URL` の
  補填漏れや別 URL の再アップロードによる二重取り込みで、判断は人が行う（同じ曲名の別テイクは正当なので、
  長さを添えて見分けられるようにする）
- **承認キュー**は `GET /api/inbox`（`items` と監視の状態 `watch: { checked_at, poll_interval_secs }`）。件ごとにタグから作った下書き（`proposal`: albumartist / album / date の
  最頻値、category は GENRE → `genre_category_map`、トラックは TRACKNUMBER / DISCNUMBER / TITLE / ARTIST）と
  不足の警告を返し、UI の Inbox タブで category / albumartist / album / date と各トラックの
  disc_no / track_no / title / artist を補正して `POST /api/inbox/:id/approve { draft }`。検証（album /
  albumartist / 各 title が空でない、`(disc_no, track_no)` が 1 以上で重複なし、rel_path が件のファイルと
  一致）に通らなければ 400 で、メタデータ不足のまま Library に入れない。`reject` / `reopen` で状態を戻す
- **トラックのタグの変更と画像の差し替え**（D-86）: 下書きのトラックは `tags`（キー → 値の配列、`null` で
  そのタグを消す。書くのはここにあるキーだけ）と `picture`（`<mime>:<sha256hex>`。`POST /api/artwork/upload`
  か `/from-caa` で置いた画像）を持てる（どちらも省略可。旧い下書きはそのまま読む）。キーは大文字・空でなく
  `=` と制御文字を含まない（tagops と同じ文字の規則）。上の欄が扱うキー（TITLE / ARTIST / ALBUM /
  ALBUMARTIST / DATE / TRACKNUMBER / DISCNUMBER / DISCTOTAL / PICTURE）と、曲・盤の同一性に使うキー
  （`SOURCE_URL` / `MUSICBRAINZ_*`）は受け付けない。承認は差し替える画像の `artwork` 行が無ければ 400。
  配置は `tags` の変更を補正のタグに足して書き（現在値と同じものは書かない）、`picture` のある曲だけ埋め込み
  画像を全部捨ててその 1 枚（front cover）にする。宛先に自分の成果物（同じ音声。途中で落ちた前回の配置）が
  あれば置き換えずに再利用するが、今回の補正（タグ・画像）がそのファイルに入っていなければ Conflict で failed
  （補正を黙って捨てない。外部の変更も上書きしない）。画像が store から消えていれば配置せず
  failed。入力欄は Enter / Esc / blur のどれで決着しても 1 回だけ確定か取り消しをする（`lib/editSession.ts`）。
  下書きが参照する画像は GC しない（区分 E の「参照」に Inbox の下書きを足す。D-56 / D-86）。`POST /api/inbox/:id/preview { draft }` は音声を読まずに
  配置の計画を引いて置き場所の見込みを返す（承認画面の ④）
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
- **後続**は `rg`（album の属性が on なら album 単位、off なら登録した track ごと。D-74）と `transcode`。WAV / ALAC / AIFF は `[normalize].wav_to_flac` なら `normalize` の
  編集バッチを作って投入する（D-46 の予告）。**アートワークは配置の直後にその album だけ解決する**
  （`scanner::resolve_album_artwork_now`。同梱カバー画像 → 構成トラックの埋め込み画像というスキャンの
  Phase 5 と同じ規則・同じ DB 反映で、thumbnail もここで投入。**`library` の排他を持ったまま**行う（解放して
  からだと並行する scan の予約 → 解決をこちらの古い読み取りで上書きし得る）。始める前に
  `artwork_resolved_at = NULL` で予約するので、決められなくても（同梱カバーの探索や読み取りの I/O 失敗を
  含む）次のスキャンが拾う。P3-4）
- **既存の album への追記**（D-70）: リリースキーは MUSICBRAINZ_ALBUMID の最頻値があれば `mb:`、自分の成果物の
  album があればそれ、**無ければ宛先ディレクトリに active な album があり、その album にも MB キーが
  無ければその album を採用**（`album:<id>`）、それも無ければ件ごとの新規。MB キー同士が違えば従来どおり
  降格か衝突。**CD とそれ以外は混ぜない**（D-67 追記 3）: トラックのタグに `MUSICBRAINZ_DISCID` があるものを
  CD とし、CD の件は CD の album にだけ、しかも album にまだ無い `disc_no` のときだけ採用する（MBID の無い
  複数枚組の 2 枚目が 1 枚目に合流する。同じ番号は同名の別の盤）。CD でない件は CD でない album にだけ採用する。
  同じ判定を再実行の「自分の成果物」と登録のトランザクションでも通し、外れたら `failed`。購読（§7.7）の
  束ね先が CD の album になっていたら同期は失敗する。採用する album は `GET /api/inbox` の `destination`（`{ album_id, album, track_count,
  max_track_no, album_gain }` | null。下書きの category / albumartist / album と件のファイルから引く。件に
  MUSICBRAINZ_ALBUMID があれば別リリースなので null）で見せる
- **album gain**（D-74）: 下書きの `album_gain`（既定 false。承認画面のチェックボックス。追記先があればその
  現在値が初期値）を配置時に album の属性へ書く（追記先の属性も上書きし、off にすれば album の値を消す）
- **ARTIST の多値**（P4-4、D-70）: 提案のトラックは `artist`（ファイルの ARTIST の全値を `"; "` で結合した
  表示文字列。1 値なら同じ）と **`keep_artists`**（ファイルの ARTIST が多値なら true）を持つ。配置は
  `keep_artists` が true なら**現在の ARTIST の個数に関係なく** ARTIST に触れない（`artist` は表示用で、値は
  見ない。承認後にファイルが 1 値に変わっていても、そのまま）。false なら `artist`（空ならアルバムアーティスト）
  の 1 値で置き換える（`;` で分割はしない）。提案と画面が true にするのは多値のときだけ（保存済みの
  下書きを再表示するとき、ファイルが多値でなくなっていれば画面は false に戻す）。`keep_artists` を持たない
  旧下書きは旧規則（`artist` が先頭値のままなら保つ）で解釈する。保つときのパス生成の `{artist}` は
  Library の `artist_display` と同じ `", "` 結合（ARTIST が無ければ `artist` → アルバムアーティストの順）
- **埋め込み画像**（P4-4、D-70）: 走査は各ファイルの `PICTURE`（`"<mime>:<sha256>"`）を、Library の
  `pick_embedded` と同じ規則（front cover 優先、無ければ先頭）で選んだ画像が**先頭**になる順で記録する。
  各ファイルの代表画像はその先頭。件の見出しに出す画像は各ファイルの代表の最頻（同数なら先に現れた
  もの）で、**件の中の多数派を目安に見せる要約**。配置後の album の代表（同梱カバー → 最初のトラックの
  埋め込み、追記先なら既存の代表）と同じとは限らない。忠実なのはトラックごとのサムネイル
  `GET /api/inbox/:id/artwork/:hash` は、件があれば `PICTURE` にその sha256 を持つファイルを走査順に
  開き（openat2。symlink / 境界外 / 消失は次の候補へ）、埋め込み画像の内容の sha256 が一致するものを
  探す。見つからなければ 404（件に無い hash、ファイルが消えた、画像が書き換わった）。見つかれば
  `If-None-Match` が ETag `"<hash>-orig"` に一致すれば 304、でなければ原寸を返す（MIME はタグの値ではなく
  内容の sniff。sniff できなければ 404。`Cache-Control: public, max-age=31536000, immutable`）。
  **304 の判定は実体の照合の後**（DB に `PICTURE` が残っていても実体が無ければ 404）。件の状態は見ない
  （rejected / failed でも実体があれば返す。placed は実体が消えているので普通 404）。セッション必須
  （allowlist 無し）。その他の I/O 失敗は 500。サムネイルは作らない（画面で縮小）。同梱の `cover.jpg`
  等は配置時に埋め込みより優先されるが（§7.1 の規則）、承認画面が見せるのは埋め込み画像だけ
- **採番**: 下書きの提案で TRACKNUMBER の無いファイルは、採用する album の active な `track_no` の最大 + 1 から
  ファイル名順に振る（無ければ 1 から）。承認の検証に「採用する album の active なトラックと `(disc_no, track_no)`
  が重ならない」を加え、配置で失敗する前に 400 で直させる。配置の登録トランザクションでも同じ検証をする
  （承認と配置の間に足された分。外れたら `failed`）
- **サイドカー `spindle-inbox.json`**（§7.7、D-70）: 件のディレクトリにあれば `GET /api/inbox` が読み、
  `category` を提案の category（語彙に同じ canonical key があるときだけ）に、`files` の `verdict` / `message` /
  `url` / `channel` を各トラックに付ける。走査は音声でないので無視し、配置の成功時に消す（Library へ
  持っていかない）。壊れていれば無いものとして扱い、件の `warnings` に載せる。モジュールは `import/sidecar.rs`
  （ダウンローダと CD の吸い出しが共有する）
- **CD の吸い出しの件**（D-67 追記、P2-5）: サイドカーの `rip`（`RipEntry`: CTDB 形式の `toc`、吸い出し開始時の
  `metadata`（`DiscMetadata`。名前は空でもよい）、音声トラック順のファイル名 `files`、ドライブが読んだ `isrcs` /
  `mcn`（P4-21。旧サイドカーには無い）、rip.log の名前 `log`、`report`（`RipReport`））を持つ。提案は `album_gain = true`（D-74。保存した下書きがあればそちら）。配置は
  下書きの各トラックを **basename で `files` の位置へ結びつけ**（`bind_rip`。大小文字・正規化の違いは同じ名前）、
  記録の形（件数が音声トラック数と揃う、名前の重複なし）・件のファイルとの 1 対 1・下書きの `disc_no` が 1 つに
  揃うこと（1 件 = 1 枚）を確かめ、外れたら配置せず `failed`（提案の `warnings` にも出す）。登録は
  `register_item` と同じトランザクションで `source_type = 'cd_rip'`、`album_verifications`（`source = 'rip'`、
  `disc_no` は下書きの値、`job_id` は NULL、`drive_offset` はレポートの `read_offset`（PCM に当てた
  読み取りオフセット。遡及照合の行は NULL。D-83 追記 3）、`log_path` は移した rip.log の Library 相対。移せなければ NULL）/
  `track_verifications` / `tracks.verification`（写像は §7.3 と同じ）。全トラックに `rip` の記録が既にあれば
  書かない（commit の後に落ちて再配置したとき）。サイドカーは読んだ FD の inode / size / mtime / ctime を
  登録の直前と消す前に照合し、変わっていれば登録せず `pending`（登録後なら消さずに残す）
- **承認後にファイルが増えた件**（同じ album への追加ダウンロード）は既存の規則で `pending` に戻る。そのとき
  提案は「保存した下書き（既知のファイルの分）+ 新しいファイルの提案」を merge して返す（補正をやり直させない）

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

### 7.10 偽ハイレゾ検出（任意）

配信で買ったハイレゾ音源には、CD 由来の AccurateRip / CTDB のような品質の裏付けが無い
（`verification` は `unverifiable` のまま）。44.1 kHz / 16 bit のマスターをアップサンプリング・
ビット深度の水増しで「24/96」として売る例があり、移行データにも `Album/Hi-Res/01.m4a`（24/96）と
アルバム直下の 16/44 が別トラックとして両方入っている。どちらを残すかの判断材料として、
可逆かつ **`sample_rate > 48000` または `bit_depth > 16`** のトラックを解析し、結果を表示と
絞り込みに出す。**判定を消費する自動処理は無い**（Derived・配布ビュー・RG・リネームは判定に
依らない。D-71）。非可逆由来の可逆（44.1 kHz の FLAC が 16 kHz で切れている等）は対象外。

```
hirescheck ジョブ（読むだけ。track_id + audio_version、並列 = max(1, CPU コア数 / 2)）
  root の dirfd で開き fstat を行と照合 → media::decode で PCM を流す（PcmSink）
  ├ スペクトル（sample_rate > 48000 のとき）
  │   チャンネルごとに Hann 窓 8192 点 / ホップ 8192 の FFT（rustfft）。パワーはフルスケール正弦波が
  │   0 dB になるよう正規化（|X_k|² × (2 / Σw)²）し、線形のまま累積して有音フレーム数で割る
  │   RMS が -70 dBFS 未満のフレームは無音として捨てる
  │   終了時（有音フレームが 1 以上のとき）:
  │     P[k]        = 平均線形パワー（未平滑）。dB は 10·log10(max(P, 1e-20))（下限 -200 dB）
  │     S[k]        = 各ビンを中心に ±1/6 オクターブ（= 1/3 オクターブ幅）の P を線形平均して dB。
  │                   窓は [0, Nyquist] で切る（端は片側だけ）
  │     floor_db    = max(Nyquist 直下 5% のビンの S の中央値, −150 dB)。完全ゼロの帯域で床が
  │                   −200 dB に張り付き、遠いサイドローブまで拾うのを防ぐ（−150 dB は 24 bit の
  │                   量子化ノイズ（1 ビン約 −186 dB）より上、16 bit のディザの直下）
  │     候補 f_s    = S[k] > floor_db + 10 dB を満たす最高ビンの中心周波数（S は窓の下端
  │                   f_s / 2^(1/6) まで信号を引きずるので、これ自体は上へ最大 1/6 オクターブずれる）。
  │                   該当ビンが無ければ（上端まで信号がある） cutoff_hz = Nyquist、cliff_db = NULL
  │     cutoff_hz   = 平滑化前の P_dB に戻り、[f_s / 2^(1/3), f_s] の範囲で 200 Hz 幅の dB 平均の段差
  │                   mean(P_dB[k−m, k)) − mean(P_dB[k, k+m)) が最大のビンの周波数（推定エッジ）。
  │                   brickwall では段差が真のエッジで最大になり、Hann 窓の漏れの量に依らない
  │                   （合成信号で 22.05 kHz → 22,090 Hz、24 kHz → 24,035 Hz）
  │     cliff_db    = 10·log10(mean(P[cutoff−1 kHz, cutoff)) / mean(P[cutoff, cutoff+1 kHz]))。
  │                   **平滑化前の P で、cutoff と同じ推定エッジを中心に測る**（S で測ると崖を自分で
  │                   ぼかす）。帯域は [0, Nyquist] で切り、どちらかの帯域にビンが無ければ（cutoff が
  │                   Nyquist 直下・1 kHz 未満）NULL。分母が 0 なら +200 dB に飽和
  │   チャンネルが複数なら cutoff が最大のチャンネルを採り、**cliff もその同じチャンネルの値**を対で
  │   採る（片側だけ本物なら本物。cutoff と cliff を別々に最大化しない）
  │   有音フレームが 0 なら cutoff_hz / cliff_db は NULL
  ├ ビット（bit_depth ≤ 24 のとき）
  │   round(sample × 2^(bit_depth−1)) を i32 に戻して全チャンネル・全サンプルを OR
  │   effective_bits = bit_depth − OR の末尾ゼロビット数。OR = 0（全無音）なら NULL
  │   32 bit は f32 で正確でないので NULL
  └ 判定（上から順に最初に当たったもの。計測しなかった側は NULL）
      decode_error : デコード失敗（hires_check_error に stderr / メッセージの先頭 500 文字）
      inconclusive : 計測できたものが無い（cutoff_hz と effective_bits がともに NULL。全無音、
                     32 bit かつ ≤ 48 kHz）
      both         : upsampled かつ padded
      upsampled    : cutoff_hz ≤ [hires].cutoff_hz かつ（cliff_db ≥ [hires].cliff_db または
                     cutoff_hz ≤ [hires].hard_cutoff_hz）。後者は 44.1 kHz の Nyquist + 余裕で、
                     >48 kHz のファイルでそこから上が空なら崖の有無を問わない（D-71 追記）
      padded       : effective_bits ≤ 16
      inconclusive : cutoff_hz ≤ [hires].cutoff_hz だが崖が無い（cliff_db が NULL か閾値未満。
                     自然なロールオフ。本物の可能性あり）
      ok           : いずれでもない
```

カットオフだけで判定しない理由: 本物でも録音や マスタリングの都合で自然に減衰する帯域がある。
SRC のローパスは 1 kHz 以内に段差（実測 12〜21 dB。エッジ直下の音楽の残りと上げた後の床の差で
決まり、30 dB には届かない）を作るので、それを条件に加えて誤検出を避ける。ただし 44.1 kHz の
Nyquist（22.05 kHz）の下で床に沈むものは、段差が無くても本物の >48 kHz 録音ではあり得ない
（実機の 106 本の本物はすべて 25.4 kHz 以上まで伸びていた）ので `hard_cutoff_hz` で拾う。
22.5〜25 kHz の帯（48 kHz マスター）で崖の無いものは `inconclusive` として人が見る。

結果は `tracks.hires_check` に検査時の `audio_version` 付きで記録し（`hires_checked_at` /
`hires_check_version` / `hires_check_error`）、**計測値 `hires_cutoff_hz` / `hires_cliff_db` /
`hires_effective_bits` も残す**（しきい値を変えたときや目視の判断に使う。判定は検査時に確定し、しきい値の変更は既存の
結果を書き換えない。再判定は手動投入）。版が進めば結果は古い扱い（`stale`）。
`[hires].check_on_import` ならスキャン完了時に結果の無い対象を自動で投入する（flaccheck と同型）。
手動は `POST /api/hirescheck { selection }`（対象外・missing は `skipped`）。
一覧の固定フィルタ `hires_unchecked` / `hires_suspect`（`upsampled` / `padded` / `both`）。
DSL は `hirescheck`（文字列）、`cutoff`（数値、Hz）、`cliff`（数値、dB）、`effectivebits`（数値）。

開いた FD の fstat が行と一致しない・missing なら何も書かず `Outcome::Done` で終える（flaccheck と
同じ。API の `skipped` は投入時に対象外だった数で、ジョブの終端とは別）。次のスキャンで版が進めば
再投入される。`record` は `WHERE audio_version = ?` で、検査中に版が進んでいれば書かない。

---

## 8. ジョブシステム

| type | 並列度 | 冪等キー |
|---|---|---|
| `scan` | 1 | 固定 |
| `rip` | **1**（物理ドライブ1台） | discid |
| `verify` | 2 | album_id |
| `rg` | CPU コア数 | album_id |
| `transcode` | CPU コア数 - 1 | track_id + variant + audio_version（variant は P4-7 から。§7.6） |
| `tagwrite` | 4 | track_id + tag_version（`edit_batch_id` でバッチに紐づく） |
| `rename` | 1 | batch_id（バッチ 1 つに 1 ジョブ。2 phase の順序を守るため直列。D-43） |
| `normalize` | 2 | track_id + op_id（同じトラックの直列化は track_locks） |
| `thumbnail` | 4 | artwork_id |
| `flaccheck` | CPU コア数 | track_id + audio_version（版付き。D-57） |
| `hirescheck` | max(1, CPU コア数 / 2) | track_id + audio_version（版付き。§7.10、D-71。rg / transcode / flaccheck と重なる分を抑える） |
| `inbox` | 1 | 固定 |
| `ytdl` | 1 | `ytdl:<url>`（playlist の展開・購読の同期で投入する分も同じ。D-70、D-78） |
| `playlist_sync` | 1 | `playlist_sync:<subscription_id>`（列挙 → 番号揃え → ytdl 投入。§7.7「再生リストの購読と同期」、D-78） |
| `gc` | 1 | 固定（scan と同じ排他 `library` を取れなければ Requeue。D-56） |
| `backup` | 1 | 固定 |

- **CPU 系の共通予算**（`rg` / `transcode` / `flaccheck` / `hirescheck`。D-73、P4-1）: 種別の並列度に加えて
  共有の予算（= CPU コア数）を取ってから走る。種別単独なら今までどおり、複数種別が同時に走るときだけ
  実行中の合計がコア数に収まる。取得順は種別 → 共通で、共通が取れなければ claim せず次の周回で試す
  （ジョブは queued のまま。種別内の順序は変えない）。予算を分け合う種別は 1 件ずつラウンドロビンで
  claim する（次の周は最後に claim した種別の次から。1 種別が予算を独占しない）。`GET /api/jobs` の `cpu_budget`
- 起動時リカバリ: `running` を `queued` へ戻し、`track_locks` / `derived_path_locks` / `job_mutexes` を
  **全件削除**する
  （ロックはプロセス生存中しか意味を持たない）。単一インスタンス前提。同じ DB を
  複数プロセスで開くことは想定しない（compose で replicas を増やさない）
- **終端遷移の書き込みの再試行**（D-76、P4-9）: ハンドラの結果を DB に書く終端トランザクション
  （進捗の flush・ロック解放・`done` / `failed` / `cancelled` / 再キュー）が失敗したら、種別の許可と
  CPU 予算を**持ったまま** 1, 2, 4, 8, 16, 32 秒の間隔で再試行する（6 回、約 1 分。ディスク満杯や
  一時的なロックなら自力で復帰し、仕事の結果を失わない）。使い切ったら `failed` としての記録を 1 度
  試み、それも駄目なら `running` のまま手放す（下の稼働中の回収が拾う）
- **稼働中の回収**（起動時リカバリのループ版。D-76、P4-9）: ワーカーは周回ごとに、DB で `running`
  だが**プロセス内で実行中でない**行（終端を書けなかった行）を `queued` へ戻し（`cancel_requested_at`
  が立っていれば `cancelled`）、その行の `track_locks` / `derived_path_locks` / `job_mutexes` を消す。
  `started_at` は NULL、`attempts` / `run_after` は触らない（起動時リカバリと同じ）。回収した旨を
  `last_error` とログに残す。実行中の登録は claim と同じスケジューラの流れで spawn の前に行い、
  「claim 済みだが未登録」の瞬間を作らない
- `dedup_key` は **`queued` / `running` の間だけ**一意（partial unique index）。
  列 UNIQUE にすると `done` / `failed` 後に同じキー（`scan` の固定キー、同 version の
  手動再試行）を永久に投入できない。キーは `type` を含めて構成する
  （`tagwrite:<track_id>:<tag_version>` 等）
- 失敗は `attempts` をインクリメントし、`run_after` に次回時刻を書いて指数バックオフ
  （再起動を跨いでも待ち時間が保たれる）。`attempts >= max_attempts` で `failed`。再試行しても変わらない
  失敗（`JobError::Fatal`。対応していない URL 等）はバックオフせず直ちに `failed`（D-70）
- 終端の行の片付け（P4-18、D-81）: `DELETE /api/jobs/:id`（failed / done / cancelled だけ。queued / running は
  409）、`DELETE /api/jobs?state=failed`（失敗をまとめて）。上部バーの赤丸は `summary.failed > 0` なので、
  確認済みの失敗を消せば消える。保持期間を過ぎた終端の行は GC が消す（`[gc].jobs_done_days` /
  `jobs_failed_days`）
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
GET    /health                                    認証の外。{ status: "ok" | "locked", version, ytdlp }
                                                  version = ビルド時に焼いた版（CI の git describe --tags --always。
                                                  `spindle --version` と同じ。P4-12、D-79）、ytdlp = 起動時診断で
                                                  取った yt-dlp の版（取れなければ null）
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
POST   /api/rg                                    { selection }。rg ジョブを投入（D-47）。`albums.album_gain` が on の
                                                  album は album 単位（rg:album:<id>）、それ以外は track 単位（rg:track:<id>。D-74）
POST   /api/rg/write                              { selection, description?, skip_pending? }。解析値を
                                                  タグとして書く編集バッチを記録（§6、D-48）。
                                                  preview 段階は無い（値は DB から決まる）
POST   /api/flaccheck                             { selection }。active な FLAC ごとに flaccheck ジョブを
                                                  投入（§7.9、D-57。読むだけで preview は無い）
POST   /api/hirescheck                            { selection }。対象（可逆かつ >48 kHz または >16 bit）ごとに
                                                  hirescheck ジョブを投入（§7.10、D-71。読むだけで preview は無い）
                                                  → 202 { tracks, skipped, duplicates, job_ids } | 409 no_changes
POST   /api/md5fill                               { selection, description?, skip_pending? }。flac_check = md5_missing
                                                  の FLAC に md5 op の編集バッチを記録（§7.9、D-59）
                                                  → 201 { batch_id, affected, skipped, pending_excluded }
                                                  → 409 pending | no_changes | md5_fill_disabled
POST   /api/verify                                { selection }。selection のトラックが属する album ごとに
                                                  verify ジョブ（遡及照合）を投入（§7.3、D-13 / D-63。読むだけ）
                                                  → 202 { albums, duplicates, job_ids }
                                                  → 409 no_changes

GET    /api/albums / :id                         全件（ページングなし）。track_count / duration_ms は active のみ。
                                                  `?filter=`（/api/tracks と同じ JSON）で一致するトラックを持つ album だけ（P4-6）
                                                  /api/tracks の行と /api/tracks/:id には artwork_hash（トラック自身の
                                                  埋め込み画像。無ければ null。D-61）。行に album_gain（D-74）
PATCH  /api/albums/:id                            { album_gain }。album gain の属性（D-74）。true → album 単位の rg を投入、
                                                  false → rg_album_* を NULL にして未書込に戻し、Derived の追随を投入。
                                                  同じ値なら何もしない。200 { album, job_id | null, derived_jobs }。
                                                  404 not_found / 400 bad_request
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
POST   /api/artwork/from-caa                      { release_id }（MBID。大小無視）。Cover Art Archive の front 画像（500px）を
                                                  取り、upload と同じく置く → 201（upload と同じ応答）。画像の無い盤は 404、
                                                  上流の失敗は 502 lookup_failed、未構成は 503 coverart_unavailable（D-86）
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

GET    /api/cd/status                             { state: unknown | no_drive | no_disc | tray_open | not_ready | disc_ok,
                                                  toc: CTDB 形式の文字列 | null, isrcs: [音声トラック順。無ければ null],
                                                  mcn: JAN/UPC | null, error: 直近の失敗 | null, checked_at,
                                                  tracks: [{ number, length_ms }], rip_job: 進行中の rip ジョブ | null,
                                                  drive: { model, offset, offset_source: manual | learned | table | unknown } | null
                                                  （型番はディスクが無くても読む。D-83 追記 2）}
                                                  （P2-1。ポーラの状態で、ドライブは叩かない。TOC は下の lookup に渡す
                                                  文字列と同じ形。ドライブ未配線なら 503 cd_unavailable）
POST   /api/cd/lookup                             { toc, isrcs?, mcn?, release?, refresh? }。TOC 文字列（CTDB 形式 0:13915:…:leadout か
                                                  MusicBrainz 形式 1 12 leadout+150 offset+150…）から各種 DiscID を出し、
                                                  MusicBrainz に照会（P2-3、D-21 / D-64）。isrcs / mcn は status が読んだもの
                                                  （null は捨てる）、release は貼ったリリース URL か MBID。→ 200 { discid,
                                                  mb_toc, accuraterip_id, ctdb_toc_id, exact, candidates: [リリース × medium。
                                                  matched_by: [discid | release | isrc | barcode | toc] を強い順に持ち、その順に並ぶ。
                                                  media: [{ position, format, track_count }]（リリース全体の収録構成）],
                                                  notes: [候補に入れられなかった理由], tracks: [{ number, length_ms }]（TOC の
                                                  音声トラック。手入力フォームの行。D-65）}。同じ入力の照会結果は
                                                  10 分覚えていて MusicBrainz を引き直さない（D-64 追記 3）。
                                                  refresh: true で捨てて引き直す（画面の「MusicBrainz に照会」）。
                                                  400 bad_request（TOC）、502 lookup_failed（届かない・応答が壊れている）、
                                                  503 musicbrainz_unavailable（再試行しても 503 の負荷制限、または未構成）
POST   /api/cd/library                            { toc, release_id? }。いまの盤がライブラリにあるか（§12.6 CD の帯、D-85）。
                                                  DiscID はサーバが TOC から出し、active なトラックの MUSICBRAINZ_DISCID
                                                  タグで引く。当たらず release_id があれば active な album の mb_release_id
                                                  （大文字小文字を区別しない。D-84）で引く。DB だけを読む（MusicBrainz は
                                                  引かない）→ 200 { discid, disc: AlbumRef | null, release: AlbumRef | null }
                                                  （AlbumRef = { album_id, rel_dir, album, albumartist }。複数あれば id の
                                                  小さいもの。disc が当たれば release は引かない）。400 bad_request（TOC）
POST   /api/cd/rip                                { toc, metadata }（P2-5）。metadata は CD 画面の下書き（DiscMetadata。名前は空でもよい）。
                                                  盤がドライブにあり TOC が一致するときだけ rip ジョブを投入 → 202 { job_id }。
                                                  400 bad_toc / bad_metadata、409 disc_mismatch（盤が違う・無い）/ duplicate
                                                  （進行中の吸い出しがある。ドライブは 1 台）、503 cd_unavailable。進捗は SSE の
                                                  job イベントの detail（RipProgress。§7.2）、結果の一行はジョブの note
POST   /api/cd/eject                              トレイを開けて状態を見直す → 204。失敗は 500 eject_failed（理由付き）。
                                                  先に CDROM_LOCKDOOR 0 で扉のロックを外す（外さないとドライブが
                                                  CHECK CONDITION で拒む機種がある）。吸い出し中（rip が running）は 409 ripping

GET    /api/jobs, POST /api/jobs/:id/cancel, POST /api/jobs/:id/retry
DELETE /api/jobs/:id                              終端（failed / done / cancelled）のジョブ行を消す（P4-18。queued / running は 409 not_terminal）
DELETE /api/jobs?state=failed                     失敗をまとめて消す → { "deleted": N }（state=failed 以外は 400）
GET    /api/config                                読み込んだ config.toml の原文 { "path", "text" }（設定画面 §12.6。秘密は config に無い）
GET    /api/archive                                退避台帳 { "items": [ archived_files の行 + "batch_id" ] }（新しい順。復元は batch の巻き戻し）
POST   /api/scan                                  {"kind": "incremental" | "deep"}。scan ジョブを投入
GET    /api/gc/preview                            GC の dry-run（区分ごとの件数・バイト数・先頭 50 件、`jobs: { done, failed }` は掃除するジョブ行の数。何も消さない。D-56）
GET    /api/inbox                                 承認キュー { "items": [{ id, rel_dir, state, detected_at, error, placed_album_id,
                                                  proposal, draft, warnings, destination, tracks: [{ rel_path, codec, lossless,
                                                  sample_rate, bit_depth, channels, duration_ms, tags, source,
                                                  same_title: [{ track_id, rel_path, duration_ms }] }] }],
                                                  watch: { checked_at, poll_interval_secs }, discard_retention_days,
                                                  subscriptions: [{ id, album, synced_at, moved, renamed, tags_batch_id,
                                                  rename_batch_id }] }   # P4-18 / P4-19 / P4-22 / D-90
                                                  （§7.8、D-68。destination と source は D-70: source は spindle-inbox.json の
                                                  項 { source, url, channel, verdict, message, subscription_id?, position? } | null。
                                                  destination.numbers は宛先の既存の番号 [[disc, track], …]（昇順）。subscriptions は
                                                  件のトラックが参照する購読の直近の同期の要約（last_result の align から。1 回で読む））
GET    /api/inbox/:id/artwork/:hash               件のファイルの埋め込み画像（PICTURE の sha256 で実体を照合してから ETag / 304。
                                                  原寸、MIME は sniff、immutable。件に無い / 実体消失 / 不一致は 404。状態非依存。
                                                  セッション必須。§7.8、P4-4）
POST   /api/inbox/scan                            inbox ジョブを投入（202 + job_id。queued / running があれば 409 duplicate）
POST   /api/inbox/:id/approve                     { category, albumartist, album, date, album_gain?, tracks: [{ rel_path, disc_no,
                                                  track_no, title, artist, keep_artists?, tags?, picture? }] }。検証に通らなければ 400、pending / failed 以外は 409 → approved +
                                                  ジョブ投入。album_gain は既定 false（D-74）。tags / picture は §7.8（D-86）。
                                                  差し替える画像の artwork 行が無ければ 400
POST   /api/inbox/:id/preview                     { draft }（approve と同じ形）。音声を読まずに配置の計画を引く →
                                                  200 { rel_dir, paths, error }（決められなければ rel_dir は null で error に
                                                  理由）。自分の成果物の再利用は見ないので見込み（D-86）
POST   /api/inbox/:id/reject, /reopen             rejected へ / pending へ戻す（approved / rejected / failed から）
POST   /api/inbox/:id/discard, /undiscard         却下した件を破棄待ちにする / 取り消す（rejected だけ。消すのは GC。D-90）
POST   /api/ytmusic/download                      { urls: [string] }（1 件以上、各 1〜2048 文字）。URL ごとに ytdl ジョブを投入
POST   /api/ytmusic/lookup                        { urls: [string] }（200 件まで）→ { items: [{ url, kind: video|playlist|other|invalid,
                                                   video_url?, located?: { location: library|inbox, path }, list_id?, subscription?: { id, albumartist, album } }] }
                                                   （YouTube 画面 ① の照合。DB だけ。`list=` 付きは再生リスト。D-87）
POST   /api/ytmusic/playlist                      { url }（再生リスト）→ { list_id, title, entries, unavailable, in_library, in_inbox, new, truncated, subscription? }
                                                   | 400 | 502 ytdlp_failed（yt-dlp で列挙して所在ごとに数える。読むだけ。同時 2 本。D-87）
GET    /api/ytmusic/subscriptions                 → { items: [Subscription] }（albumartist / album 順。P4-16、D-78）
POST   /api/ytmusic/subscriptions                 { url, albumartist, album, category?, align? = true, enabled? = true, max_enqueue? = 50 }
                                                  → 201 Subscription | 400（YouTube の list= 付き URL でない、空、max_enqueue が 1〜1000 外）
                                                  | 409 { error: "duplicate_list" | "duplicate_target" }
PATCH  /api/ytmusic/subscriptions/:id             { albumartist?, album?, category? (null で消す), align?, enabled?, max_enqueue? }
                                                  → 200 Subscription | 404 | 409 { error: "duplicate_target" | "sync_running" }
                                                  追記先を変えると album_id は NULL に戻る。同期が queued / running の間は sync_running
DELETE /api/ytmusic/subscriptions/:id             → 204 | 404 | 409 sync_running（PATCH と同じ。Library のファイルは消えない）
POST   /api/ytmusic/subscriptions/:id/sync        → 202 { job_id } | 404 | 409 { error: "duplicate" }（走行中。latch は残るので終わった後に走る）
                                                  [ytmusic].enabled でなければすべて 404
// Subscription = { id, list_id, url, album_id, albumartist, album, category, align, enabled, max_enqueue,
//                  created_at, updated_at, last_attempted_at, last_synced_at, sync_requested_at,
//                  last_result: { state, error?, synced_at, title?, entries, playlist_count?, album_id?, in_library, in_inbox,
//                                 elsewhere: [{position, id, track_id, rel_path}], enqueued: [position], running: [position],
//                                 deferred, unavailable: [{position, id, kind}], align?: { moved, unchanged, blocked: [{track_id,
//                                 position, current_no, reason}], outsiders, unnumbered, tags?, renamed, rename? } } | null }
                                                  → 202 { job_ids }。`[ytmusic].enabled` でなければ 404（§7.7、D-70）
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
//                                      //   flac_unchecked | flac_error（§7.9）| hires_unchecked | hires_suspect（§7.10）
//     "q": "情緒" }                    // 検索語（3 文字以上 FTS5 / 未満 LIKE）
//   cursor は前ページの next_cursor をそのまま返す不透明文字列（キーセット）。sort が変わったら
//   捨てる（別ソートで発行したカーソルは 400）。
//   total はフィルタに一致する全件数（同じ読み取りスナップショットで数える）
{ "items": [ { "id": 1, "title": "...", "artist_display": "...", "album": "...", "albumartist": "...",
               "track_no": 1, "disc_no": 1, "date": "2024", "category": "J-Pop",
               "duration_ms": 280000, "codec": "flac", "lossless": true,
               "verification": "verified_ctdb", "rg_scanned_at": 1, "rg_written_at": 1,
               "derived": { "opus": { "codec": "opus", "stale_tags": false }, "aac": null },
                                                                       // 系統ごと。無ければ null（§7.6）
               "flac_check": { "status": "ok", "checked_at": 1700000000, "stale": false, "error": null },
                                                                       // 未検査なら null（§7.9）
               "hires_check": { "status": "upsampled", "checked_at": 1700000000, "stale": false,
                                "error": null, "cutoff_hz": 22050, "cliff_db": 48.3, "effective_bits": 16 },
                                                                       // 対象外・未検査なら null（§7.10）
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
//     { "op": "set_rows","key": "SOURCE_URL", "rows": { "<track_id>": "…" | ["…"] } }
//         行ごとに違う値（API 専用。UI には出さない。rows に無い行は変更なし。1〜10,000 行。P4-14）
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

// GET /api/jobs?type=          // type（任意）: 種別で items を絞る（`ytdl` 等。上限も種別内で数える。YouTube 画面）。
                                //   summary / by_type は全件のまま。不明な種別は 400
{ "items": [ { "id", "type", "state", "progress", "done", "total", "attempts", "last_error",
               "run_after", "edit_batch_id", "created_at", "started_at", "finished_at",
               "note",          // 完了時の結果 1 行（ハンドラが返す。ytdl: "Inbox に置いた: <path>" /
                                //   "プラグインが skip: <理由>" / "取り込み済み（Library|Inbox）: <パス>" /
                                //   "再生リストを展開した: N 件を投入、M 件は取り込み済み"。
                                //   playlist_sync: 同期の要約。無ければ null）
               "subject" } ],   // subject = 対象の表示用文字列（track_id → Library のパス、album_id → ディレクトリ、
                                //   batch_id → 説明、transcode は " [<variant>]" 付き、scan は kind、ytdl は url、
                                //   thumbnail は "artwork #id"。行が消えていれば "track #id"、対象の無い種別は null）
                 // 状態ごとに上限付き（`limits`）: 実行中・待ち 1,000（実行中 → 待ち。取り出し順 = priority 降順・
                 // 作成順・id 昇順）→ 完了 300（新しい順）→ 失敗・取り消し 300（新しい順）。1 本の並びで切ると
                 // 待ちが数千件のとき完了・失敗が届かない
  "summary": { "running": 3, "queued": 12, "done": 15300, "failed": 0, "cancelled": 0, "pending_ops": 1204 },
  "by_type": { "transcode": { "queued": 6470, "running": 11, "done": 1089, "failed": 0, "cancelled": 0 }, … },
                 // 種別ごとの全件集計（0 件の種別は無し）
  "concurrency": { "scan": 1, "rg": 12, "transcode": 11, … },   // 種別ごとの並列度（§8）
  "cpu_budget": 12,                                               // CPU 系の共通予算（= コア数。D-73）
  "limits": { "active": 1000, "done": 300, "failed": 300 } }     // items の状態ごとの上限
// POST /api/jobs/:id/cancel  → 202（queued は即 cancelled、running は cancel_requested_at を立てる）
//                            → 404 | 409 { "error": "not_cancellable" }   // 既に終端
// POST /api/jobs/:id/retry   → 202（failed / cancelled を attempts=0 で queued に戻す）
//                            → 404 | 409 { "error": "not_retryable" | "duplicate" }   // D-36
// DELETE /api/jobs/:id       → 204（failed / done / cancelled の行を消す。P4-18）
//                            → 404 | 409 { "error": "not_terminal" }
// DELETE /api/jobs?state=failed → 200 { "deleted": N }。state=failed 以外は 400 { "error": "invalid_state" }

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
  `channels` `category` `added` `duration` `has_derived` `missing` `hirescheck` `cutoff` `cliff`
  `effectivebits`
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
   （`verification` `category` `source_type` `lossless` `added` `has_derived` `missing` `hirescheck`
   `cutoff` `cliff` `effectivebits`）と `MATCHES` は
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

デスクトップブラウザ専用。レスポンシブ・アニメーションは対象外。配色はライト / ダーク
（設定「表示」。既定は OS に従う。P4-10、D-58 追記）。キーボードは Ctrl+A / Delete / Enter / Esc と
一覧のカーソル移動（§12.2「キーボード」）。

### 12.1 骨格（上部バー + 3 ペイン）

```
┌──────────────────────────────────────────────────────────────────────────────────────┐
│ spindle  ライブラリ アルバム CD YouTube Inbox❷ │ ⏮ ▶ ⏭ ■ 0:38 ━━━━━━━━ 3:37 曲名 — アーティスト │ RG track ☐原本 🔊━ │ ☰ │
├──────────────┬───────────────────────────────────────────────────────────────────────┤
│ [検索 _____] │ プロパティ │ 一括編集 │ 操作                         選択 1,204 件 ▴ │ ← 右パネル
│ ▾ All Music  │ Metadata                 │ Location                                   │
│   ▾ J-Pop    │ Artist Name  …           │ File path  …                               │
│     ▸ 藍井   │ …                        │ General …                                  │
│ ▸ Game       ├───────────────────────────────────────────────────────────────────────┤
│              │ 表示 9,099 件                                                     列 │
│ プレイリスト  │ ☐ │ Artist/album │ # │ Title / track artist │ 長さ │ バッジ │        │
│  ♪ 通勤      │ ☑ │ …            │ 1 │ …                    │      │ ✔ RG   │        │
│  ⚙ 未検証    │ … (6 万行、仮想スクロール)                                            │
│ フィルタ      │                                                                     │
│  未検証 重複 │                                                                     │
│ ┌──────────┐ │                                                                     │
│ │  cover   │ │                                                                     │
│ └──────────┘ │                                                                     │
└──────────────┴───────────────────────────────────────────────────────────────────────┘
```

- **上部バー**（ナビとプレイヤーを 1 本に。D-58 追記）: 左に画面の切替、中央に ⏮ ▶ ⏭ ■ とシーク、
  曲名（無ければ Not playing）、右に RG モード / 原本 / 音量、☰（ジョブ / 履歴 / 設定 / ログアウト。
  SSE の接続状態の点もここ）
- **ナビの並び**（P4-20）: `ライブラリ / アルバム / CD / YouTube / Inbox`。ジョブ・履歴・設定は ☰ の中。
  **タブは取り込みの流れ順には並べない。** ものは 入力（CD / YouTube）→ Inbox → ライブラリ と流れるが、
  ライブラリが日常の入口なので先頭（ホーム）に置く。導線は「取り込みタブのすぐ右に Inbox があり、
  そこに赤い数字が増える」ことで示す（CD を取り込み終えたら右隣の Inbox を見る）。
  Inbox のバッジは承認待ちの件数（`GET /api/inbox/summary`）で、失敗があれば赤点も出す。
  ジョブが ☰ に入るので、実行中 + 待ちの件数は ☰ のメニュー内の「ジョブ」に添え、失敗の赤点は
  ☰ のボタン自体に出す（SSE の点と合わせて 2 つまで。3 つ並べると読めない）
- **左サイドバーを出す画面**（P4-20）: ツリーの選択が表の絞り込みに効く画面（ライブラリ / アルバム）だけ。
  CD / YouTube / Inbox / ジョブ / 履歴 / 設定では左カラムごと畳んで全幅にする（ツリーが何にも効かないため。
  左カラム下のアルバムアートも一緒に隠れるが、曲名は上部バーに残る）
- **検索ボックス**は左サイドバーの先頭（スクロールしない）。表の `filter.q`（入力から 250ms 後に
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
| Derived | `derived_files` の `opus` 系統あり（= 配布ビュー）。`stale_tags` は点付き。`aac` 系統はバッジにせずプロパティの Location 列に行を出す（§7.6） | delivery |
| 反映待ち ⏳ | `edit_ops.result = 'pending'` がある | edit_ops |
| conflict ⚠ | このトラックの**最新の op**（`edit_ops` を `id DESC` で 1 件）が `skipped_conflict` | edit_ops |
| 重複 | `duplicate_groups` に属する | view |
| hardlink | `nlink > 1` | tracks |
| missing | `missing_since` あり。行全体をグレー | tracks |

**選択**: クリック / Shift 範囲 / Ctrl 追加 / Ctrl+A（フィルタ結果全件）/ キーボード（下記）。
全件選択は ID 列挙ではなく**選択した時点のフィルタ式**をサーバに渡す（`selection.filter`）。
選択は immutable で、その後に表示フィルタやソートを変えても**選択集合は変わらない**
（表示中の行と選択集合が食い違うことはあり、右パネルは選択集合の件数を出す）。
プレビューでサーバが集合をスナップショットし（`selection_token`）、適用はその集合だけに
効く。右パネル頭に「選択 N 件（うち反映待ち M 件）」、表の上に「表示 K 件」。

**反映待ちの行**は編集不可。右パネルの [適用] は「M 件を除外して適用 / 待つ」の 2 択に
なる（API の 409 をここで吸収する）。

**キーボード**（P4-17、D-80。表にフォーカスがあるとき。クリックした行がカーソルになる）:

| キー | 動き |
|---|---|
| ↓ / ↑ | カーソルを 1 行動かし、その行だけを選択する |
| PageDown / PageUp | 1 画面ぶん動かす（同上） |
| Home / End | 読み込み済みの先頭 / 末尾へ（同上） |
| Shift + 上記 | カーソルを動かし、選択を **anchor からカーソルまでの範囲そのもの**に置き換える（戻れば縮む。クリックの Shift が「足す」のと違う）。anchor が無ければ（Ctrl+A / Esc の直後）移動前のカーソル行が anchor |
| Ctrl + 上記 | カーソルだけ動かす（選択は変えない） |
| Space | カーソル行を選択にトグル（Ctrl+クリックと同じ） |
| Ctrl+A / Esc / Delete | 全件選択 / 解除 / プレイリスト scope で選択を除外（従来どおり） |

カーソルは**読み込み済みの行の中**でだけ動く（未読込の骨組み行には id が無く選択に入れられない）。末尾に
着くと次のページが読まれるので、End を繰り返せば先へ進める。カーソルは選択とは別の見た目上の状態で、
表示フィルタ・ソートを変えても選択集合は変わらない（上記の immutable の規則のまま）。

**インライン編集**: セルをダブルクリック → 1 件のバッチとして同じ経路（プレビュー省略）。

**プロパティタブの Metadata**（D-58 / D-72、P4-3）: 値のダブルクリックで選択全体への `set`（`;` で多値）。
foobar2000 の Properties と同じく、行末の × か右クリックのメニューで**フィールドの削除**（選択全体への
`delete` op。値の無い行は消せない）、表の下の「フィールドを追加」で**新しいキー**に `set`（キーは大文字化、
`=` と制御文字は不可、`PICTURE` 不可、表に既にあるキーはその行の編集を案内、空の値は追加しない）。
どれも 1 op のバッチ（preview → apply、履歴から巻き戻せる）。サーバの変更は無い。

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

**導線**（D-87。ユーザ要望・モックで合意）: 一括編集タブと操作タブは CD / Inbox と同じ番号付きの段を
**横に 4 列**並べる（パネルは表の上で低く横長なので。コンテナ幅が狭ければ 2 列 → 1 列）。

- 一括編集: **① 対象**（選択件数・アルバム・反映待ち。`PanelTargetStep`）→ **② 操作**（上の操作リスト）→
  **③ プレビュー**（変更 / 変更なし / 反映待ち除外の件数。選択・操作・ソートを変えると「古い」になり枠が
  警告色）→ **④ 適用**（説明と「N 曲に適用」。③ が済むまで押せない。済んだらバッチ #、巻き戻しは履歴）
- 操作: **① 対象** → **② 操作を選ぶ**（ファイル / 音量 / 検査 / 画像・プレイリストの群から 1 つ。選んだものは
  localStorage に残す）→ **③ 確かめる / プレビュー**（選んだ操作の説明と「巻き戻せる変更 / ジョブ / 読むだけ /
  すぐ反映」の印。リネーム・正規化はパスのプレビュー、アートワークは画像の選択、album gain はチェックボックス、
  プレイリストは追加先）→ **④ 実行**（見出しは適用 / 投入 / 反映。押せない理由を添える。結果の 1 行と
  反映待ちの「除外して適用」もここ）

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

種別ごとの並列度と待ち行列（待ち / 実行中 / 完了 / 失敗。件数は `by_type` の全件集計。一覧は上限付きなので
画面で数えない）、一覧の各行に**対象**（`subject`。どのファイル / アルバム / バッチかが id だけでは分からない）、
実行中の進捗（`done / total`）、失敗の `last_error` と [再試行] / [キャンセル]、終端の行の [消す]（確認済みの
片付け。失敗・取り消しタブには [失敗をすべて消す]。P4-18）。一覧のタブは
**実行中・待ち / 完了 / 失敗・取り消し / すべて**で、各タブに `summary` の件数を添える（待ち → 実行中 → 完了と
数が移っていくのが分かる）。一覧は状態ごとに切り出す（実行中・待ちはキューの順で 1,000 件、完了と失敗・取り消しは新しい順で
各 300 件。`limits`）。表示中のタブが上限に達していれば「表示は最新 N 件まで」と添える。種別表の下に CPU 系の実行中の合計と共通予算（`cpu_budget`。D-73）。編集バッチ由来のジョブは `edit_batch_id` で履歴画面へリンク。
SSE `/api/events` で更新し、リロードしても DB の値で復元する。

### 12.6 その他の画面（骨格のみ）

- **アルバム**（P1-3）: サムネイルグリッド（`/api/artwork/:hash?size=256`。missing は出さない）→
  クリックで表を `album_id` に絞る（一覧へ戻る）。**表と同じ絞り込み**（ツリーの選択・プレイリスト・検索語・
  編集中のルール）が効き、`GET /api/albums?filter=` で「一致する active なトラックを持つ album」を出す
  （P4-6、D-58 追記）。件数は絞り込み中「全 M 件中 N 件」。ツリーは全件の一覧から組む（絞り込みとは別に取る）
- **操作タブの「アートワーク」**（P1-3 書き側、D-60）: ファイル選択 → `POST /api/artwork/upload` →
  256px のプレビュー（形式・寸法）→ 「選択 N 件の埋め込み画像を差し替え」（`POST /api/artwork/embed`。
  反映待ちの 409 は他の操作と同じ「除外して適用」）。履歴画面の `PICTURE` 値はサムネイルで出す
- **CD**（P2、P4-20）: **ドライブに CD が入っている前提の画面**。左カラム（ツリー）は出さない（§12.1）。
  - **導線**（D-85。ユーザ要望）: 上から番号付きの枠で **① 挿入中の CD → ② 認識したトラック → ③ 候補を
    選ぶ → ④ 取り込む**。各枠の見出しの右に要約（① は「表示のみ」のバッジ、② は曲数、③ は照会の要約
    `lookupHeadline`）と ⓘ。**長い説明は本文に並べず ⓘ のツールチップ**（`components/Hint.tsx`。ホバーと
    キーボードのフォーカスで出る）に畳む（照会の段の順序、写す範囲、CD 以外の媒体、「さらに広げて探す」、
    「どれも違う」、ドライブの型番とオフセット）。③ の候補の一覧は枠の中でスクロールし（高さの上限）、
    ④ を画面の遠くへ押し出さない。④ の横の一行は「『アルバム』として取り込む」/「候補を選ばずに取り込む」
  - **ライブラリにあるかの帯**（D-85）: ツールバーの下。`POST /api/cd/library`（TOC と選択中の候補の
    リリース）で、その盤そのもの（DiscID）が当たれば緑の「✓ この盤はライブラリにある: 『…』（…）」、
    リリースだけ当たれば黄の「同じリリースの album がライブラリにある（この盤は未取り込み）」と
    「ライブラリで開く」（表を album に絞る）。どちらも無ければ出さない。失敗しても画面は止めない
  - **ドライブ**（P2-1）: 画面を開いている間 `GET /api/cd/status` を 2 秒間隔で取り（`useCdDrive`）、
    状態の一行（ディスクなし / トレイが開いている / ディスクあり（N トラック・総時間）/ ドライブが無い
    （理由））と「照会し直す」「取り出す」。新しいディスクの TOC が出たときだけ（同じディスクの間は 1 回。
    `lib/cdDrive.ts` の `newDiscToc`）照会を自動で始める。自動の照会はサーバが覚えている結果を使い、
    ボタンからの照会は `refresh` で引き直す
  - **この画面では編集させない**（P4-20 追記。ユーザ要望）。吸い出したものは Inbox を通すので
    （D-67 追記）、値を直すのは Inbox の承認画面に一本化する。入力欄が 2 か所にあると、どちらで直すのか
    分からなくなる。CD 画面は「何が入っていて、どの盤として取り込むか」を確かめる場所
  - **トラック表**: TOC の音声トラックと 1:1 で、**照会の前から出る**（`GET /api/cd/status` の `tracks`。
    番号と長さはサーバが TOC から出す）。列は `# / タイトル / アーティスト / 長さ`、取り込み中だけ右端に
    `進捗`。**読み取り専用**で、見た目は一覧の表に寄せる（sticky なヘッダ、`--row-h` の行高、行のホバー、
    番号と長さは右寄せ）。名前の入っていない行は**取り込んだときに付く名前**（`Track NN` /
    アルバムアーティスト）を薄字で見せ、候補を選ぶと実名に置き換わる。ISRC は列にせず `#` の
    ツールチップ。コンポーネントは一覧の `TrackTable` とは別（あちらは仮想スクロールと行選択を持つ）
  - **アルバムの要約**（① 挿入中の CD。`CdAlbumSummary`）: 読み取り専用。**すべてラベル付き**の 2 列の
    定義リストで、アルバム（強調）/ アルバムアーティスト / 日付は常に（空は「—」、アルバムは「（候補を
    選んでいない）」）、ディスク番号（複数枚組のときだけ）/ レーベル / カタログ番号 / JAN/UPC は値のある
    ときだけ。名前は Inbox で入れる旨は ⓘ に。
    **`category` は CD 画面に無い**（Inbox の承認時に選ぶ）。トラックリスト貼り付け（P2-4、D-65）も
    CD 画面からは外し、Inbox の承認画面へ移した（§12.6 の Inbox、P2-10）。TOC の貼り付け
    （CTDB 形式 / MusicBrainz 形式 / `cdrecord -toc` の出力）・各種 ID・用語の凡例は「詳細」に残す
    （編集ではなく照会の入力。ドライブの無い環境とデバッグ用。P2-3、D-64）
  - **候補**: 左にジャケット（`GET /api/cd/cover/{release_id}`。無ければ空の枠。D-82）、右に候補の
    ラジオ一覧。MusicBrainz のリリースへのリンク、収録構成（`mediaSummary`。`CD + Blu-ray の 1 枚目` /
    `CD 2 枚組の 1 枚目`）、ディスクとの長さ差（`lengthDiffMs`）で見分ける。CD 以外の medium
    （デジタル配信・DVD・Blu-ray）は既定で畳み、「CD 以外の媒体に当たった候補も表示」で出す
    （`isCdMedium` / `splitByMedium`）。バッジは経路（DiscID 一致 / 指定 / ISRC / バーコード / TOC 近似。
    複数可）、見出しは段と経路（`stage`。DiscID が登録済みのまま下の段に落ちたときは「未登録」と言わない）、
    `notes` は赤字。DiscID 未登録なら「MusicBrainz に DiscID を登録」リンク（`discidSubmissionUrl`）。
    応答の `can_widen` が真なら**「さらに広げて探す」**（`widen`。TOC 近似まで引き直す。D-64 追記 4）
  - **写す範囲**（D-72、P4-2）は**既定で「全部写す」**（D-72 追記 2、P4-20。レーベル・カタログ番号・
    JAN/UPC・`MUSICBRAINZ_RELEASEGROUPID`・各トラックのタイトル / アーティスト・MB id・ISRC まで写す）。
    「識別用の最小限」に切り替えると盤を見分けるのに要るものだけ（アルバム・アルバムアーティスト・
    日付・ディスク番号 / 枚数・`MUSICBRAINZ_ALBUMID`。`MUSICBRAINZ_DISCID` / `TRACKTOTAL` は吸い出し時に
    TOC から付く。トラック行は番号と長さだけの空行）になる。範囲を切り替えると選択中の候補を写し直す。
    候補ゼロ件でも、また「どれも違う（候補を使わない）」を選んでも取り込める（P2-4、D-21 / D-65。
    名前は Inbox で入れる）
  - **「確定」の段は無い**（P4-20）。「取り込む」で吸い出しが始まる（吸い出しは P2-5。それまでは
    ボタンを無効にしておく）。遷移は `lib/cdState.ts` の reducer: フォームは TOC が読めた時点ででき、
    消えるのは別のディスクに替わったとき（`set_disc` / `set_toc` / `reset`）だけ。**照会の開始と
    失敗ではフォームを消さない**（表は照会の前から出ているため）
- **Inbox**（P2-10、D-68）: 左に件（= `[paths].inbox` の音声ファイルのあるディレクトリ）の一覧
  （状態バッジ・ファイル数・コーデック・検出時刻・失敗理由）と「今すぐ確認」（`POST /api/inbox/scan`）、
  右に選んだ件の補正フォーム: アルバムアーティスト / アルバム / 日付 / category
  （`components/CategoryField.tsx`）。**CD で吸い出したものもここへ来る**（D-67 追記）ので、
  盤の名前を直すのもこの画面と、トラックごとの disc / # / タイトル / アーティスト（ファイル名・コーデック・長さは
  表示のみ）。初期値はタグからの提案（`proposal`）、承認済み・失敗の件は保存した下書き。検証はサーバと
  同じ規則（`lib/inbox.ts` の `validateDraft`）で、問題が無いときだけ「承認して配置」が押せる。
  「却下」はファイルを残したまま件を却下にし（一覧には「却下」のバッジで残る）、「下書きに戻す」で pending に戻る。
  却下した件には「削除」（確認ダイアログ → 破棄待ち。一覧に「削除待ち」のバッジ、承認画面に「削除待ち: <期限> 以降の
  GC でファイルを消す」と「削除を取り消す」。期限は `GET /api/inbox` の `discard_retention_days` から。D-90）。placed の件は 24 時間
  残り、「アルバムを開く」で表を `album_id` に絞る。inbox ジョブの完了で一覧を取り直す。
  `destination` があれば「宛先: 既存の『…』（N 曲）に追加。番号は <max+1> から」と出す。**購読から落とした曲**
  （`source.subscription_id` あり）を含む件は番号が再生リストの位置（同期が空けた番号）に入るので、「番号 165 に入る
  （再生リストの位置。空けてある番号）」と出し、その下に購読の直近の同期の要約（「同期（日時）で既存の 14 曲の番号を
  揃え … 165 を空けた（バッチ #14 / #15）。承認しても既存の曲は動かない」。直近の同期で揃え直しが無ければその旨）を
  出す。下書きの番号が `destination.numbers` と重なれば赤で警告する（承認はサーバが 400 で止める。P4-22）
  追記先の曲がどれもディスク番号を持たない（`destination.uses_disc = false`）1 枚分の件（CD の件を除く）は、配置で
  `DISCNUMBER` を書かず件のファイルにあれば消し（宛先に合わせる。DB の `disc_no` も NULL になる）、宛先の文言に
  「ディスク番号は付けない（宛先の曲に無い）」を足し、③ の disc 列を空で見せる（下書きの値は 1 のまま。D-70 追記）
  （配置済みの件は `destination` を引かないので、宛先も同名の警告も出ない。引くと自分が置いた album に当たる）。
  **同名の警告**（P4-19）はタイトル欄の下に「⚠ Library に同名: <ファイル名>（長さ）／この曲 <長さ>」（件の曲の長さも
  並べる。P4-22）、件の一覧に「同名 N」のバッジ（承認は止めない。長さで別テイクと見分ける）。ツールバーに周期監視の「最後に確認: HH:MM:SS（N 秒ごと）」（P4-18）。`source` のあるトラック行は判定バッジ
  （ok / 未判定）を出し、行を開くと `message`（参照実装ならルールの足し方）と URL が読める（D-70）。
  「album gain を計算する」のチェックボックス（既定 off。`destination` があればその現在値が初期値。D-74）。
  **忠実表示**（P4-4、D-70）: ARTIST が多値のファイルの行は元の値をチップで見せ、「ファイルの多値をそのまま
  保つ」のチェック（提案は on。on の間は欄が `"; "` 結合の表示で編集不可、外すと欄が編集できて 1 値で書く
  旨を示す）。見出しの横と**件の一覧の各件の左**（48px。無い件は同じ寸法の空枠。D-85）に件の代表画像
  （各ファイルの代表 = `PICTURE` の先頭、の最頻）、トラック表に小さなサムネイル列
  （`GET /api/inbox/:id/artwork/:hash`。無ければ空）。
  **導線**（D-86。ユーザ要望・モックで合意）: 承認画面は CD 画面と同じ番号付きの段（`components/Step.tsx`）で
  **① 取り込む件**（出どころ CD / YouTube / 手置き・ディレクトリ・検出時刻・追記先・ファイルのタグで足りないもの
  = `warnings`）→ **② アルバム情報** → **③ トラック** → **④ 確認して配置**。見出しに件の代表画像
  （下書きを当てた後の各曲の画像の最頻）・出どころ・状態・「変更 N 件」（`draftChangeCount`）。
  **② アルバム情報**: 先頭に画像の欄（`InboxCover`）、その下にライブラリのプロパティと同じ操作の 2 列の表
  （`InboxAlbumProps`）。Metadata（アルバムアーティスト / アルバム / 日付 / category / album gain）は最初から
  入力欄にせず、行をクリックで選び、ダブルクリック（Enter / F2）で入力欄、Enter で確定、Esc で取り消し、↑↓ で
  行を移る。category は統制語彙の選択欄（`CategoryField` の inline）、album gain はダブルクリックで切り替わる。
  提案（album gain は追記先の現在値）と違う行に「変更」、行のツールチップに元の値。Info（MusicBrainz リリース /
  ディスク・トラック数 / 追記先）は表示のみ。
  **画像の欄**: 全曲が同じ画像なら 1 枚（クリックかドロップで差し替え＝全曲）、画像なしは点線の枠、**曲ごとに
  違う（YouTube）なら既定で曲ごとの画像を保ち**、サムネイルを並べて「画像の無い N 曲に入れる」「全曲を 1 枚に
  そろえる」を明示的な操作にする。CD の件でリリースが決まっていれば「Cover Art Archive から取る」
  （`POST /api/artwork/from-caa`）。画像は `POST /api/artwork/upload` で置き、下書きの `picture` に入れる
  （`hooks/useArtworkUpload`、`lib/inbox.ts` の `pictureState` / `applyPicture` / `resetPictures`）。
  **③ トラック表は全項目**（D-85 / D-86）: ライブラリの表のように列で並べ、枠の中で横スクロールする
  （`InboxTrackGrid`）。セルをクリックで選び、ダブルクリック（Enter / F2）で入力欄、↑↓←→ で直せるセルを移る。
  disc / # / タイトル / アーティストはその行だけ、**画像**はその曲だけ差し替える（ファイルを選ぶかドロップ）、
  アルバム / アルバムアーティスト / 日付は**アルバム単位**（どの行で直しても全行と ② に反映。列を薄く塗る）、
  **ファイルの全タグ**（件のファイルと下書きで足したキーのうち、固定列が出している TITLE / ARTIST / ALBUM /
  ALBUMARTIST / DATE / TRACKNUMBER / DISCNUMBER と PICTURE を除いたもの。ABC 順、多値は `"; "` で結合）は
  その行だけ直し、空にするとそのタグを消す（`setTrackTag`）。表の下の「タグを追加」で新しいキーの列を足す
  （`newTagKeyProblem`）。長さ / codec / ファイル / 判定 / category と、**🔒 の付いた同一性のタグ**
  （`SOURCE_URL` / `MUSICBRAINZ_*`）と DISCTOTAL は直せない。変更したセルは色で示し、ツールチップに元の値。
  ARTIST が多値の行はセルに「多値を保つ」のチェック（D-70）。disc / # / 画像 / タイトルは横スクロールしても
  左端に残す（`stickyColumns`）。列の組み立ては `inboxColumns` / `extraTagKeys`。「列」メニューで
  disc / # / タイトル以外を隠せ、隠した列は localStorage（`inbox.columns.hidden`）に覚える。
  **④ 確認して配置**: 赤（`validateDraft` の問題。あると承認できない）/ 黄（判定できなかった曲・Library に
  同名・画像の無い曲）/ 緑（タグを直す曲数・画像を差し替える曲数）の一覧、配置先の見込み
  （`POST /api/inbox/:id/preview` を下書きの変更から 400ms 後に引く。`hooks/useInboxPreview`）、
  「承認して配置」「却下」「下書きに戻す」。
  **MusicBrainz の引き直し**（P4-21、D-84）: CD の件（`GET /api/inbox` の `rip` がある件）だけ、アルバムの欄の
  下に節を出す。ボタンで `POST /api/cd/lookup { toc, isrcs, mcn, release?, refresh?, widen? }`（CD 画面と同じ）を
  引いて候補を並べ、選ぶと下書きの `release_id` / `release_group_id` を写し、ディスク番号を候補の medium の位置に
  し、空欄の名前（と `Track NN`）だけ埋める（`lib/inbox.ts` の `applyCandidate`）。結果は保存しない。
  **トラックリスト貼り付け**（P2-10、D-65 / D-65 追記 2）: トラック表の下の畳んだ節。テキストを
  `lib/tracklist.ts` で行解析し、`lib/inbox.ts` の `applyTracklist` で**選んだディスクの行へトラック番号で**
  写す（複数枚組のときだけ「写す先」のディスクを選ぶ）。アーティストの無い行は既存の値を保ち、
  アーティストを貼った行は「そのまま保つ」を外す。件に無い番号・同じ番号の行が複数あるもの・行数の違い・
  未設定の行と、解析の警告（飛ばした見出し・番号の重複 / 飛び）を節の中に出す。写した後もフォームで直せる
- **操作タブの「album gain」**（P4-5、D-74）: 「ReplayGain / FLAC」節に、選択行が属する album ごとの
  チェックボックス（`PATCH /api/albums/:id`。20 album を超えたら絞るよう促す）。アルバム画面は無く
  アルバム一覧は表を絞るだけなので、切り替えはここに置く
- **YouTube**（P3-3 → P4-13、D-70 追記）: 上部バーの独立した画面（取り込み元は Inbox / CD / YouTube で
  横並び、結果は Inbox に集まる）。
  **導線**（D-87。ユーザ要望・モックで合意）: 見出しの切り替えで「ダウンロード」と「購読」。ダウンロードは
  番号付きの段 **① URL を貼る**（入力が止まって 400ms で `POST /api/ytmusic/lookup` を引き、行ごとに種類と
  状態（新規 / ライブラリにある / Inbox で承認待ち / YouTube 以外 / 取れない）を表で出す。再生リストは URL
  ごとに 1 回 `POST /api/ytmusic/playlist` で本数と新規の数を出す）→ **② 取り込み方**（行き先・判定・展開・
  画像の説明。未購読の再生リストがあれば「購読にする →」で購読の ① へ URL を渡す）→ **③ ダウンロード**
  （飛ばす行を除いた「N 件をダウンロード」。投入後は今回のジョブ = 返ったジョブと、その再生リストの展開で増えた子（payload の
  `parent_job_id`）を追う）→
  **④ Inbox で承認**（今回のジョブの note から置いた件を並べ、「Inbox で開く」でその件を選んで開く。件が
  まだ一覧に無ければ現れるまで待ち、人が別の件を選んだらやめる）。
  これまでのジョブは折りたたみ。購読は **① 再生リスト**（list_id・題名・本数。購読済みなら止める）→
  **② アルバムとして登録**（題名が取れたら空のアルバム名に入れる）→ **③ 登録して同期**（「登録して今すぐ同期」と
  「登録だけ」。定期同期は既定で無いので、登録だけなら後から一覧の「同期」）、その下に登録済みの一覧。以下は各部の中身。(1) 1 行 1 URL のテキストエリアと「ダウンロード」（`POST /api/ytmusic/
  download`。再生リストは動画ごとに展開し、Library / Inbox に `SOURCE_URL` のある動画は投入しない）、
  (2) ytdl ジョブの一覧（`GET /api/jobs?type=ytdl` を新しい順。URL・結果（待ち / ダウンロード中 / 完了は
  `note` = Inbox に置いた・プラグインが skip・再生リストを展開した N 件 / 失敗の理由）・時刻・取り消し /
  再試行・Inbox に置いた行だけ「Inbox で確認」）、
  (3) 購読（P4-16、D-78。ジョブの表は長くなるので購読の節はその上に置く）: 一覧表（アルバムアーティスト / アルバム（category、束ねた album #）・再生リスト
  （list_id、リンク）・有効・揃える（その場で PATCH）・最終同期・結果 1 行（走行中なら「同期中 / 同期待ち /
  再試行待ち」を `GET /api/jobs?type=playlist_sync` から、無ければ `last_result` の要約）・「同期」「編集」
  「削除」）。結果に詳細があれば「詳細」で行を開く（取れない一覧（非公開 / 削除 / 取れない）・別の album に
  ある・走行中・持ち越し・揃えられない（理由付き）・番号 / 改名のバッチ id と適用 / 衝突 / 失敗）。追加
  フォーム（URL（`list=` が無ければその場で注意）/ アルバムアーティスト / アルバム / category（語彙から
  選択、空 = 未分類）/ 揃える / 上限）。編集はアルバムアーティスト / アルバム / category をその行で。
  削除は確認ダイアログ。SSE job のたびに取り直す。
  `/youtube?url=<URL>` で開くと欄に入れた状態で開く（ブックマークレットの
  受け口。同一 origin の GET なので CORS / CSRF を触らない。http / https 以外は受けない。USERGUIDE §11.3）。
  操作タブの YouTube 節は廃止
- **設定**: 「表示」（配色: OS に従う / ライト / ダーク。localStorage に保存。P4-10、D-58 追記）、
  `config.toml` の閲覧、再スキャン / deep scan / GC dry-run のボタン、
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
multi_disc  = "{category}/{albumartist}/{album}/{disc}-{track:02}. {title}"
single_disc = "{category}/{albumartist}/{album}/{track:02}. {title}"
unsorted    = "_Unsorted/{albumartist}/{album}/{track:02}. {title}"

[rip]
device = "/dev/sr0"
drive_offset = "auto"          # auto | 整数
retry_on_mismatch = 2
prefer_ctdb = true

[encode]
flac_compression = 8

[encode.derived.opus]          # Derived の opus 系統（§7.6、D-75）。Android の同期・Web 再生・配布ビュー
enabled = true
bitrate = 256                  # opusenc --vbr --music --bitrate（D-9 追記）

[encode.derived.aac]           # Derived の aac 系統（§7.6、D-75）。Mac のミュージック.app へ取り込む Apple 向け
enabled = true                 # 節を省略すると false（RG 全件解析 → 有効化の順序のため）
bitrate = 256                  # ffmpeg -c:a aac -b:a
lossy_sources = true           # 非可逆原本も AAC へ（D-8 の例外。aac 原本も再エンコード）
multi_value_separator = " & "  # 多値フィールドの結合

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
poll_interval_secs = 60        # Inbox の確認間隔（変化があったときだけ走査を投入。inotify は使わない: D-68 追記）。0 で自動なし

[gc]
retention_days = 30            # 物理削除までの猶予（missing_since / 退避 WAV / Derived 孤児）
jobs_done_days = 7             # 終端のジョブ行（done / cancelled）を消すまでの日数。0 で消さない（P4-18）
jobs_failed_days = 30          # failed のジョブ行を消すまでの日数。0 で消さない

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
address_family = "auto"                 # auto | ipv6 | ipv4。片方の経路が塞がれている環境で選ぶ
cover_art_url = "https://coverartarchive.org/"   # 省略可。候補のジャケット（D-82）

[verify]                       # 遡及照合 / リップ検証の照会先。UA は musicbrainz.user_agent を共用
accuraterip_url = "http://www.accuraterip.com/accuraterip/"
ctdb_url = "http://db.cuetools.net/lookup2.php"

[ytmusic]
enabled = true
metadata_command = ["/usr/local/bin/spindle-ytmusic-meta", "metadata"]   # メタデータプラグイン（D-69）。引数配列
metadata_timeout_secs = 30
download_timeout_secs = 900    # yt-dlp のダウンロード 1 件の上限（D-70）
ytdlp_args = ["--extractor-args", "youtube:lang=ja"]   # 翻訳タイトルでなく日本語のタイトル・チャンネル名を取る。弾かれたときの口にもなる（P4-13）。UA / Referer は付けない
sync_interval_hours = 0        # 再生リストの購読を定期同期する間隔（時間）。0 = 手動と承認の後続だけ（D-78）

[hires]                        # 偽ハイレゾ検出（§7.10、D-71）
check_on_import = true         # スキャン完了時に未検査の対象（可逆かつ >48 kHz または >16 bit）を自動投入
cutoff_hz = 25000              # カットオフがこれ以下なら「上げただけ」の疑い（44.1k の 22.05 kHz と 48k の 24 kHz を
                               # 窓の漏れ込みの余裕込みで拾う。本物の 96k は 30 kHz 以上まで伸びるのが普通）
cliff_db = 10.0                # カットオフ前後 1 kHz の落差がこれ以上なら SRC の崖とみなす（実 SRC は 12〜21 dB）
hard_cutoff_hz = 22500         # カットオフがこれ以下なら崖に関わらず「上げただけ」（44.1k の Nyquist + 余裕）

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

### イメージの配布（P4-12、D-79）

- 置き場は GHCR `ghcr.io/akashisn/spindle`（public。pull にトークン不要）。`linux/amd64` のみ
- CI（`.github/workflows/ci.yml`）は web → rust → docker → publish の順。docker ジョブ（PR でも走る。
  `contents: read` だけ）が**イメージを 1 回だけビルド**し（`docker/build-push-action` の `load`）、起動確認
  （`--version`、`/health` の `version` / `ytdlp`、ロックモード、ログイン、CSRF、ジョブ API、同梱 SPA）に
  通ったものを `docker save` して成果物に渡す。publish ジョブ（push イベントだけ。書き込み権限 = GHCR /
  Release はここだけ）がそれを skopeo で GHCR へ複製する（二度ビルドしない = 確認した digest と配布する
  digest が同じ。`docker push` は load したイメージで「unknown blob」になる）。タグは `docker/metadata-action`:
  `main` への push → `edge` と `sha-<7 桁>`、`vX.Y.Z` タグ（この形だけ。他の `v*` は CI が拒む）→ `X.Y.Z` /
  `X.Y` / `latest`（+ GitHub Release。自動生成のノートにイメージのタグと digest を添える）。PR はビルドと
  起動確認だけで push しない
- 版は build context に `.git` を入れないので、CI が `--build-arg SPINDLE_VERSION=$(git describe --tags
  --always)` で渡し、`build.rs` が `cargo:rustc-env` で焼く（環境変数 > 作業ツリーの `git describe`（HEAD /
  その ref / packed-refs / index の変化で再計算。`--dirty` は付けない）> `dev`）。OCI ラベル（`org.opencontainers.image.{source,revision,version,created,…}`）は metadata-action
- メタデータプラグイン（`spindle-ytmusic-meta`）はイメージに焼かず実行時マウントのまま（D-70）
- **`edge` は開発用で DB の互換（マイグレーションの前進）以外は約束しない。** 本番は `latest`
  （= 最新の `vX.Y.Z`）か `X.Y` を指す。マイグレーションは起動時に自動で前進のみ = 戻すときは
  バックアップから（OPERATIONS）
- 依存の更新は Renovate（`renovate.json`。GitHub App）: Dockerfile の `FROM`（node / rust / debian /
  deno）、GitHub Actions、Cargo / npm（minor / patch は週 1 のまとめ）、**yt-dlp**（`ARG YTDLP_VERSION`
  を `# renovate:` 注釈の custom manager で github-releases から追い、schedule の例外で 1 件ずつ即時）。
  マージは手動。Dependency Dashboard の issue に保留中の更新と手動実行のチェックボックスが出る。
  マージすれば `edge` が作り直され、本番へは次の `vX.Y.Z` で届く（yt-dlp だけの更新でもパッチ版を切る）
- スカッシュ（`db/migrations` を `0001` に畳む）は最初の `vX.Y.Z` より前に 1 回だけ行った（2026-09-24、D-88）。
  公開イメージで DB を作った後は既存ファイルを書き換えられないので、以後は連番を足すだけ

### TrueNAS Custom App (compose)

```yaml
services:
  spindle:
    image: ghcr.io/akashisn/spindle:latest
    devices:
      - /dev/sr0:/dev/sr0        # CD ドライブ。ioctl / SG_IO とも sr0 に直接通る（/dev/sg* は不要）
    group_add:
      - "24"                     # host の cdrom グループ GID
    device_cgroup_rules:
      - 'b 11:* rmw'             # sr (block)
    user: "1000:1000"            # 既存ライブラリの所有者に合わせる
    volumes:
      - /mnt/ssd/media/Library:/library
      - /mnt/ssd/media/Derived:/derived
      - /mnt/hdd/media/Archive:/archive
      - /mnt/ssd/media/Inbox:/inbox
      - /mnt/ssd/media/Playlists:/playlists
      - /mnt/ssd/apps/spindle:/data
      - /mnt/ssd/apps/spindle/bin/spindle-ytmusic-meta:/usr/local/bin/spindle-ytmusic-meta:ro   # メタデータプラグイン（D-70）
    ports:
      - "8080:8080"
    restart: unless-stopped
```

**注意点:**

- USB 接続だとデバイス再列挙でノード番号が変わり得る（sr0 → sr1）。
  `/dev/disk/by-id/...` を指すか、SATA 接続を推奨
- UID/GID が既存ライブラリの所有者と一致しないとタグ書き込みが全滅する
- udev はコンテナに届かないため、ディスク挿入検知はポーリング
- ホストにドライブが無いと `devices` の行で compose が起動に失敗する。ドライブの無い機体では
  `devices` / `group_add` / `device_cgroup_rules` を消す。アプリ側は `[rip].device` を開けなくても
  起動し、CD 画面に「ドライブが無い」と出す

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
│   ├── subscriptions.rs playlist_subscriptions（再生リストの購読。追記先の CAS 束ねと同期の latch。D-78）
│   ├── playlists.rs  jobs.rs  history.rs
├── domain/
│   ├── identity.rs      inode / audio_md5 による同一性解決
│   ├── filter.rs        一覧のフィルタ（JSON）/ ソート / カーソルの検証
│   ├── selection.rs     selection の 2 形、preview のスナップショット store（D-33）
│   ├── pathgen.rs       テンプレート展開・正規化・衝突回避
│   ├── tags.rs          lofty ラッパ、正規化、多値処理、Derived への書き出し（Opus の VorbisComments / MP4 の ilst + フリーフォーム）
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
│   ├── hires.rs         偽ハイレゾ検出の解析（PcmSink: FFT の累積とサンプル OR → 計測値（cutoff / cliff / 実効ビット）→ 判定。§7.10、D-71）
│   ├── encode.rs        flac（ffmpeg デコード → flac -8）/ opus（ffmpeg デコード → opusenc）/ aac（ffmpeg 1 パス。RG 焼き込み・48 kHz 上限）
│   └── artwork.rs       同梱 / 埋め込み画像の選択、判別、ハッシュアドレスのキャッシュ（P1-3）
├── cd/
│   ├── mod.rs           TrackLayout（サンプル単位のトラック列）、照会用 HTTP クライアント
│   ├── device.rs        ioctl（CDROM_DRIVE_STATUS / READ TOC / LOCKDOOR + EJECT）、SG_IO READ SUB-CHANNEL（ISRC / MCN）、
│   │                    Drive トレイト、DriveMonitor とポーラ
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
│   └── ytmusic/         metadata.rs（メタデータプラグインのプロトコル v1 と呼び出し。D-69）、
│                        downloader.rs（yt-dlp の dump / download、remux、タグ、Archive、Inbox への配置と
│                        spindle-inbox.json。D-70）、sidecar.rs（spindle-inbox.json の読み書き。Inbox と共有）、
│                        playlist.rs（再生リストの列挙の解釈と番号揃えの計画。純粋。D-78）
├── jobs/
│   ├── queue.rs  worker.rs  recovery.rs  scheduler.rs（backup / gc の周期投入。inbox は handlers/inbox.rs、
│   │                        購読の dispatcher は handlers/playlist_sync.rs）
│   └── handlers/        種別ごと（playlist_sync.rs = 購読の同期: 列挙 → 追記先 → 揃え → 投入。D-78）
├── gc/
│   └── mod.rs           物理削除の唯一の経路。plan（判定・dry-run）と execute_*（6 区分。D-56 / D-90）
├── playlist/
│   ├── dsl.rs           pest 文法 → AST
│   ├── compile.rs       AST → パラメータ化 SQL
│   ├── fb2k.rs          AST → foobar クエリ + ソートパターン
│   └── export.rs        m3u8 / pls / パスマッピング
├── api/
│   ├── mod.rs  tracks.rs  albums.rs  categories.rs  selection.rs  batch.rs  rename.rs  normalize.rs
│   │   history.rs  stream.rs  cd.rs  inbox.rs  events.rs  ytmusic.rs  subscriptions.rs
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
| **P3** | ytmusic 移植（メタデータプラグイン + yt-dlp → Inbox）、配置直後のアートワーク解決、偽ハイレゾ検出 | ytmusic CLI を廃止できる |

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
| Derived | 可逆のみ変換 + 配布ビュー解決。Apple 向け `aac` 系統だけ非可逆も変換（D-75） | 非可逆の多重劣化を回避。ミュージック.app は Opus を読めない |
| ビットレート | Opus 256k VBR（当初 128k。D-9 追記）、AAC 256k | 再生成可能なので低リスクな決定 |
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
| 外部メタデータ | MusicBrainz だけ。候補から写す範囲は選べる（CD 画面の既定は「全部写す」。D-72 追記 2）。Discogs / VGMdb は作らない | 表記揺れが激しく値は結局手で直す。外部の価値は盤の識別（D-72） |
| `.fpl` | 非対応（確定） | .m3u8 とクエリ文字列で足りる。非公開バイナリで 1.x / 2.x が違う（D-72） |
| album gain | album ごとの属性（既定 off、CD 取り込みは on、承認画面で選ぶ）。RG の一致判定は完全一致のまま | 育つコレクションに曲を足すたびに全曲の再解析と書き換えになる。運用はシャッフルが主（D-74） |
| ACL の再適用 | 移行後に TrueNAS の ACL エディタで手動（MIGRATION.md §2 のチェックリスト） | rsync は NFSv4 ACL を引き継げない。1 回しか使わないのでスクリプト化しない |

### 残課題

- [x] Discogs / VGMdb 連携（2026-09-20。作らない。D-72）
- [x] `.fpl` 書き出し（2026-09-20。作らない。D-72）
- [x] `HAS` 等の演算子の foobar 実機との挙動突き合わせ（2026-09-19。部分一致で一致。D-55）
- [x] 偽ハイレゾ検出のしきい値設計（2026-09-20。カットオフ ≤ 25 kHz かつ（崖 ≥ 10 dB または ≤ 22.5 kHz）/ 実効 ≤ 16 bit。
      実機 124 本で較正。計測値も保存。§7.10、D-71）
- [x] 移行後の NFSv4 ACL 再適用（2026-09-20。UI で手動。MIGRATION.md §2 のチェックリスト）
- [x] CPU 系ジョブ（rg / transcode / flaccheck / hirescheck）に共通の並列予算（2026-09-20。共通 Semaphore =
      コア数を P4 で実装。D-73）
- [x] Inbox のポーリング間隔（2026-09-19。`[inbox].poll_interval_secs` 既定 60 秒 + 手動。D-68）
- [x] 一括リネームで album 全体を動かした後、旧ディレクトリに残る同梱ファイル（cover.jpg /
      disc.cue / rip.log 等）の追随と空ディレクトリの扱い（2026-09-19。rename ジョブが commit 後に
      既知の名前を追随させ、空なら rmdir。D-67）

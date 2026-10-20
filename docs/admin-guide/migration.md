# 移行手順

既存の音楽ライブラリを spindle のデータセット構成（役割別の Library / Derived / Archive / Inbox。
[SPEC §5](../spec/05-library-layout.md)、[D-1](../decisions/D-001.md)）へ移す手順。振り分けの判断は [D-45](../decisions/D-045.md)。
**`casesensitivity` と `normalization` はデータセット作成時のみ指定可能**で後から変更できない
ため、既存データセットの `zfs rename` による流用はしない（[D-19](../decisions/D-019.md)）。

この手順は作者の旧ライブラリ（`Opus/` と `Original/` が 1:1 で対になった構成。`Original/` に ALAC / webm /
mp3 などの原本、`Opus/` に配布用の Opus）を 2026-09 に移したときのもの。`scripts/migrate_plan.py` の振り分けは
この構成を前提にしている。構成が違うなら、振り分け一覧（rsync の `--files-from`）は自分で作り、
データセット作成・コピーと検証・照合の手順だけを使う。以下の例のパスは [`deploy/compose.yaml`](../../deploy/compose.yaml)
に合わせて、SSD のプール `ssd`、HDD のプール `hdd`、旧ライブラリ `ssd/musics` としている。

| | 例 |
|---|---|
| 旧ライブラリ | `ssd/musics` → `/mnt/ssd/musics/{Opus,Original}` |
| 新データセット | `ssd/media/{Library,Derived,Inbox}`、`ssd/apps/spindle`、**`hdd/media/Archive`** |
| コンテナ | `user: 1000:1000`（`deploy/compose.yaml`） |

作業はホストで行う。`scripts/preflight.py` と `scripts/migrate_plan.py` は標準ライブラリだけで
動くので、`scp` で `/tmp` へ置いて `sudo python3` で実行する。

## 0. 事前チェックと振り分け計画

```bash
# ブロッカー検出（衝突 / 不正 UTF-8 / 255 バイト超 / symlink / hardlink / SMB 禁止名 / 容量）。
# Library へ入る 2 本の木を別々に見る。どちらも exit 0 になるまで進まない
sudo python3 /tmp/preflight.py /mnt/ssd/musics/Opus     --dest /mnt/ssd --plan /tmp/rename-opus.sh
sudo python3 /tmp/preflight.py /mnt/ssd/musics/Original --dest /mnt/ssd --plan /tmp/rename-original.sh

# 振り分け一覧（rsync --files-from 用）。unmatched が 0 で exit 0 になること
sudo python3 /tmp/migrate_plan.py /mnt/ssd/musics --out /tmp/plan
sudo cat /tmp/plan/summary.json          # library_audio_total が初回スキャン後のトラック数
sudo cut -f1,2 /tmp/plan/skip.tsv | sort | uniq -c   # 移さないものの理由を目視
```

`insensitive` + `formD` では、比較キーが一致する 2 ファイルは共存できない。判定は
「NFD 正規化 → casefold」した結果の一致で行う。ブロッカーがあれば `--plan` のリネーム案を
**目視のうえ**実行する（中身が同一の重複ならリネームではなく削除が正しい）。symlink は
実体に置き換え、hardlink は片方を実コピーにするか削除する（[D-26](../decisions/D-026.md)）。

`migrate_plan.py` は `Opus/` と `Original/` の 1:1 対応を前提に、原本の形式で振り分ける
（ALAC → Library、webm → その `.opus` が Library で webm は Archive、mp3 → Library、
AIFF は同名 ALAC があれば Archive、`AAC/` とマーカーは移さない）。対応が取れない stem が
あれば exit 1 で止まるので、`skip.tsv` の `unmatched` を見て手で決める。

## 1. データセット作成

```bash
COMMON="-o casesensitivity=insensitive -o normalization=formD"   # utf8only=on が強制される
sudo zfs create ssd/media
sudo zfs create $COMMON -o recordsize=1M -o compression=lz4 -o atime=off ssd/media/Library
sudo zfs create $COMMON -o recordsize=1M -o compression=lz4 -o atime=off ssd/media/Derived
sudo zfs create $COMMON -o recordsize=1M -o compression=lz4 -o atime=off ssd/media/Inbox
sudo zfs create hdd/media
sudo zfs create $COMMON -o recordsize=1M -o compression=lz4 -o atime=off hdd/media/Archive
sudo zfs create -o recordsize=16K -o compression=lz4 -o atime=off ssd/apps/spindle
sudo mkdir -p /mnt/ssd/media/Playlists/m3u8
```

| dataset | recordsize | snapshot | 備考 |
|---|---|---|---|
| ssd/media/Library | 1M | 毎日 + 一括編集前 | 唯一の正 |
| ssd/media/Derived | 1M | なし | 再生成可能。レプリケーション対象外 |
| hdd/media/Archive | 1M | 週次 | 追記のみ。FLAC 化で退避する原本の受け皿 |
| ssd/media/Inbox | 1M | なし | 承認前の一時領域 |
| ssd/apps/spindle | 16K | 毎日 | SQLite。ページサイズに合わせる |

- `casesensitivity=insensitive`: Windows の foobar2000 が `Cover.jpg` を、spindle が
  `cover.jpg` を作る事故を ZFS 層で潰す
- `normalization=formD`: 日本語の濁点・半濁点には NFC と NFD の 2 表現がある。macOS 由来の
  パスは NFD になりがちで、正規化なしだと「見た目が同じで別ファイル」が発生する

Inbox は Library と別データセットなので move は実コピーになるが、1 回あたりアルバム 1 枚
なので実用上の問題はない。Archive は別プールなので退避も実コピー（追記のみで速度は要らない）。

## 2. コピーと検証

```bash
# 1. 旧ライブラリを固定してから作業する（ブロッカー解消のリネームは readonly の前に済ませる）
sudo zfs set readonly=on ssd/musics
sudo zfs snapshot ssd/musics@pre-migration

# 2. 振り分け一覧に従ってコピー（zfs send/recv は不可。作成時プロパティが継承されてしまう）
R="sudo rsync -aHX --info=progress2 --from0"
$R --files-from=/tmp/plan/library-original.list /mnt/ssd/musics/Original/       /mnt/ssd/media/Library/
$R --files-from=/tmp/plan/library-opus.list     /mnt/ssd/musics/Opus/           /mnt/ssd/media/Library/
$R --files-from=/tmp/plan/archive-original.list /mnt/ssd/musics/Original/       /mnt/hdd/media/Archive/
$R --files-from=/tmp/plan/playlists.list        /mnt/ssd/musics/Opus/Playlists/ /mnt/ssd/media/Playlists/m3u8/

# 3. 検証（チェックサム比較。何も出なければ成功）。-O はディレクトリの時刻を比べない:
#    Original と Opus の 2 本を同じディレクトリへコピーするので、ディレクトリの mtime は必ず違う
V="sudo rsync -aHXnO --checksum --itemize-changes --from0"
$V --files-from=/tmp/plan/library-original.list /mnt/ssd/musics/Original/ /mnt/ssd/media/Library/
$V --files-from=/tmp/plan/library-opus.list     /mnt/ssd/musics/Opus/     /mnt/ssd/media/Library/
$V --files-from=/tmp/plan/archive-original.list /mnt/ssd/musics/Original/ /mnt/hdd/media/Archive/

# 4. 設定とメタデータプラグインを置く（初回起動より前。config.toml が無いと既定値で起動する）
sudo install -m 644 config.toml /mnt/ssd/apps/spindle/config.toml
sudo install -D -m 755 spindle-ytmusic-meta /mnt/ssd/apps/spindle/bin/spindle-ytmusic-meta

# 5. 所有者をコンテナの実行 UID/GID に合わせる
sudo chown -R 1000:1000 /mnt/ssd/media /mnt/hdd/media /mnt/ssd/apps/spindle

# 6. spindle の初回スキャン完了と数日の運用までは旧ライブラリを破棄しない
```

`config.toml` は `deploy/config.example.toml` を元にする。`[encode.derived.aac]` を最初から有効にして
よい（aac は RG の解析が済んだ行から作られる。§4）。プラグインはイメージに入っていない（[D-70](../decisions/D-070.md)）。
`[ytmusic].ytdlp_args` に `["--extractor-args", "youtube:lang=ja"]` を入れる（無いと翻訳タイトルのある動画は英語の
タイトルとチャンネル名が返り、日本語の曲名が取れない）。

コピーと検証が済んだら、[導入](install.md)「カスタムアプリの作り方」でアプリを作って起動する。
起動すると初回スキャン（deep）が走る（目安: 約 9,000 本で 9 分）。

**ACL は rsync で引き継げない。** TrueNAS の SMB データセットは NFSv4 ACL を使うが、
`rsync -A` が扱うのは POSIX ACL であり互換がない。コピー後に TrueNAS の ACL エディタで
新データセットにプリセットを適用し直す（SMB ユーザとコンテナの `1000:1000` の両方が書ける形）。
1 回しか使わないのでスクリプト化はしない（[D-72](../decisions/D-072.md) と同日に決定）。手順:

1. Datasets → `ssd/media` → Permissions → Edit（`hdd/media` も同じ）
2. Owner / Group を `1000` / `1000`、Preset は **Restricted** を基点に、`owner@` と `group@` に
   FULL_CONTROL、SMB で書くユーザ（またはそのグループ）にも FULL_CONTROL の ACE を足す
3. **Apply permissions recursively** と **Apply permissions to child datasets** にチェックして保存
4. 確認: `nfs4xdr_getfacl /mnt/ssd/media/Library` に `owner@:rwxpDdaARWcCos:fd-----:allow` 相当の
   行が出て、コンテナ（`1000`）と SMB ユーザの両方で `touch` できること
   （`sudo -u '#1000' touch /mnt/ssd/media/Inbox/.acl-test && rm` と、SMB クライアントからの新規ファイル）

定期スナップショットも UI で上の表どおりに設定する。

## 3. 移行後の照合

```bash
# Library の音声一覧が計画と一致すること
sudo python3 /tmp/preflight.py /mnt/ssd/media/Library --manifest /tmp/after.tsv --audio-only
tail -n +2 /tmp/after.tsv | wc -l                       # = summary.json の library_audio_total
diff <({ tr '\0' '\n' < /tmp/plan/library-original.list; tr '\0' '\n' < /tmp/plan/library-opus.list; } \
        | grep -Ev '\.(png|jpe?g|webp)$' | sort) \
     <(tail -n +2 /tmp/after.tsv | cut -f1 | sort)
```

初回スキャン完了後、DB のトラック数が `after.tsv` の行数（ヘッダ除く）と一致することを
確認する。音声拡張子の判定は `preflight.py` の `AUDIO_EXT` が唯一の定義で、スキャナの
受け付ける拡張子もこれと揃える。一致しない場合はタグ読み取り失敗を疑う。

あわせて、webm 由来の曲を数件選び、`Archive/` の `.webm` と `Library/` の `.opus` を `ffprobe` で
比べてコーデック（opus）とストリーム長が一致すること（remux であり再エンコードでないこと）を
確認する。これが「webm 由来の曲は Library の `.opus` が master、`Archive/` の webm は生データ」
という前提の根拠になる。ALAC 由来の曲は `.m4a` がそのまま master なので照合は不要。

## 4. 移行後にやること

- `backup` ジョブが 1 世代以上取れていること（`/mnt/ssd/apps/spindle/backup/`。起動時に 1 世代取る）
- 旧 m3u8 を手動プレイリストとして取り込む（プレイリスト画面「Playlists/ の m3u8」、または
  `GET /api/playlists/import` の各 `path` を `POST /api/playlists/import`）。未解決が 0 であること
- ReplayGain を全件解析する（`POST /api/rg {"selection":{"filter":""}}`、または全選択で「ReplayGain を解析」）。
  `[replaygain].write_tags = true` なら解析と同時にタグへ書く（別の「解析値をタグに書く」は要らない）。
  aac の Derived は RG が揃った行から作られるので、解析が終わるまで aac は増えない
- ロスレス → FLAC 正規化（P1-4）で ALAC を FLAC にする（全選択で「正規化をプレビュー」→ 適用）。
  退避した ALAC は `Archive/` に 7 日（`[gc].retention_days`）置かれてから GC される。正規化は `audio_version` を上げないので、
  Derived の生成と並行してよい（作り直しは起きない）。目安: 2 並列で約 7,500 本に約 5 時間
- 一括編集を 1 件行い、巻き戻しが通ること（P0 の完了条件）
- Derived は Library から再生成される（起動時スキャンの後に自動で投入。[SPEC §7.6](../spec/07-pipeline/06-derived.md)）。旧 `Opus/` は
  移していない。目安: 約 9,000 曲で opus + aac に約 130G 使うので、始める前に空きを見る

## 5. パスを `[layout]` の書式に揃える（任意）

旧ライブラリのパスは spindle の書式（`[layout]`。[SPEC §5](../spec/05-library-layout.md)「パステンプレート」）と表記が違うことがある。
リネームは巻き戻せるが数千件のファイル移動になるので、ReplayGain の解析と Derived の生成が終わってから一度だけ行う。

1. **同名の別版に `EDITION` を付ける**。同じアルバム名の別版（Hi-Res 版など）は、`EDITION` が無いと通常版と
   同じパスになり、リネームのプレビューで「降格に必要な値が無い: edition」になる。付ければ通常版は元の名前、
   別版は `… (Hi-Res)` のように分かれる（[D-43](../decisions/D-043.md) 追記）。**付けた後に増分スキャンを 1 回走らせてから
   プレビューする**（album の edition はスキャンの最後に構成トラックの `EDITION` から埋まる。タグを書いただけでは
   album 側が NULL のままで、「同名の別リリースと衝突」が出る）
2. 全選択で「リネームをプレビュー」。作者のライブラリでは約 9,100 曲中 約 5,200 曲が変わり、衝突 0 だった。
   変わるのは表記の違い（アルバム名の `/` を `／` にする、複数枚組のファイル名 `1.01.` → `1-01.`、`[layout]` に無い
   中間の階層が無くなる、など）。**`_Unsorted/` 行きが 0 であること**（category は初回スキャンで直下のフォルダ名から
   付く。[D-92](../decisions/D-092.md)）と、「パスを生成できない」が 0 であることを確かめてから適用する
3. 適用後、Derived は移動に追随し、m3u8 の書き出しは自動で書き直される。foobar2000 など外部の
   プレイヤーのライブラリはパスが変わるので読み直しが要る

## 6. YouTube 由来の曲に `SOURCE_URL` を付ける（任意）

YouTube の再生リストから取り込んでいた曲を、spindle の購読（YouTube 画面の「同期」。[D-78](../decisions/D-078.md)）へ
引き継ぐときの一度きりの手順。旧パイプラインが「再生リスト名 = アルバム名、リスト内の位置 = `TRACKNUMBER`」で
並べていたなら、`scripts/backfill_source_url.py` で既存の行に `SOURCE_URL` を付けてから購読を登録すると、
同期が既存の曲を「持っている」と判定して二重に取り込まない（P4-14 / P4-15）。

1. `scripts/backfill_source_url.py --playlists … --spindle … --out …` で計画 CSV を出し、目視
   （`title-mismatch` / `no-track` / `extra-track` / `kept` と、位置推定の `verified-by-neighbors` を
   確認）→ `--apply` で `SOURCE_URL` を書く（アルバムごとに 1 バッチ。巻き戻し可）。位置推定は区間内の
   入れ替えを検出できないので、CSV で確かめてから `--include-inferred` を付けて書く。既に
   `SOURCE_URL` を持つ行は上書きしない
2. 再生リストに無い曲は、プロパティタブの Metadata で「フィールドを追加」して `SOURCE_URL` を手で付ける
3. YouTube 画面で再生リストを購読に登録し「同期」する。同期は先に既存の行の `TRACKNUMBER` とファイル名を
   再生リストの位置に揃え、`SOURCE_URL` の無いものだけを Inbox に入れる。承認画面で「⚠ Library に同名」が
   出たら、件の長さと Library 側の長さを比べる。**長さが一致するものは同じ音源の二重取り込み**なので承認せず、
   既存行の `SOURCE_URL` を件の動画の URL に付け替える。長さが違えば別テイク（Cover / Live ver.）なのでそのまま承認してよい

## 7. 旧ライブラリの破棄

正式運用で数日間問題が出ないことを確かめてから、旧ライブラリ（と `@pre-migration` スナップショット）を破棄する。
作者の環境では、旧ライブラリは別プールへ退避してから元のデータセットを消した。

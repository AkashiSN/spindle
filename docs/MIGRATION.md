# 移行手順

旧ライブラリ（`ssd/musics`）から spindle のデータセット構成へ移す手順。振り分けの判断は
`docs/DECISIONS.md` D-45、旧構成の実態もそこに書いてある。
**`casesensitivity` と `normalization` はデータセット作成時のみ指定可能**で後から変更できない
ため、既存データセットの `zfs rename` による流用はしない（D-19）。

**運用方針**: spindle が完成するまで `ssd/musics` が正のライブラリで、日常の追加もそちらに行う。
2026-09-16 の移行は本手順のリハーサルで、`ssd/media` / `hdd/media/Archive` / `ssd/apps/spindle` は
開発・検証用の環境。**リリース時にこれらを作り直し、§0 から再度実施する**（`ssd/musics` の
その時点の内容で `migrate_plan.py` を回す）。ACL プリセットと定期スナップショットの設定も
リリース時の再移行で行う。

実機の前提（2026-09-16 時点）:

| | |
|---|---|
| 旧ライブラリ | `ssd/musics` → `/mnt/ssd/musics/{Opus,Original,AAC}`（342G。所有者 `nobody:nogroup`、ファイルは `snishi`） |
| 新データセット | `ssd/media/{Library,Derived,Inbox}`、`ssd/apps/spindle`、**`hdd/media/Archive`** |
| コピー量 | Library 約 229G（ALAC 221G + opus 7.4G）、Archive 5.7G、Playlists 1.7M |
| 空き | `ssd` 435G、`hdd` 36T |
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
実体に置き換え、hardlink は片方を実コピーにするか削除する（D-26）。

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
| hdd/media/Archive | 1M | 週次 | 追記のみ。FLAC 化で退避する ALAC 220G の受け皿 |
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

# 6. spindle 初回スキャン完了と数日の運用までは ssd/musics を破棄しない
```

`config.toml` は `deploy/config.example.toml` を元にする。`[encode.derived.aac]` を最初から有効にして
よい（aac は RG の解析が済んだ行から作られる。§4）。プラグインはイメージに入っていない（D-70）。
`[ytmusic].ytdlp_args` に `["--extractor-args", "youtube:lang=ja"]` を入れる（無いと翻訳タイトルのある動画は英語の
タイトルとチャンネル名が返り、日本語の曲名が取れない）。

コピーと検証が済んだら、`docs/OPERATIONS.md`「カスタムアプリの作り方」でアプリを作って起動する。
起動すると初回スキャン（deep）が走る（2026-09-24 のリハーサルで 9,098 本 9 分）。

**ACL は rsync で引き継げない。** TrueNAS の SMB データセットは NFSv4 ACL を使うが、
`rsync -A` が扱うのは POSIX ACL であり互換がない。コピー後に TrueNAS の ACL エディタで
新データセットにプリセットを適用し直す（SMB ユーザとコンテナの `1000:1000` の両方が書ける形）。
1 回しか使わないのでスクリプト化はしない（D-72 と同日に決定）。手順:

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
  `GET /api/playlists/import` の各 `path` を `POST /api/playlists/import`）。28 本 14,205 行、未解決 0 が期待値
- ReplayGain を全件解析する（`POST /api/rg {"selection":{"filter":""}}`、または全選択で「ReplayGain を解析」）。
  `[replaygain].write_tags = true` なら解析と同時にタグへ書く（別の「解析値をタグに書く」は要らない）。
  aac の Derived は RG が揃った行から作られるので、解析が終わるまで aac は増えない
- ロスレス → FLAC 正規化（P1-4）で ALAC を FLAC にする（全選択で「正規化をプレビュー」→ 適用）。
  退避した ALAC は `Archive/` に 7 日（`[gc].retention_days`）置かれてから GC される。正規化は `audio_version` を上げないので、
  Derived の生成と並行してよい（作り直しは起きない）。2 並列で 7,570 本に約 5 時間
- 一括編集を 1 件行い、巻き戻しが通ること（P0 の完了条件）
- Derived は P1-10 が Library から再生成する（起動時スキャンの後に自動で投入）。旧 `Opus/` の 7,568 本は
  移していない。opus + aac で約 130G 使うので、始める前に `ssd` の空きを見る

## 5. リリース時の再移行と旧データセットの破棄

リリースまで `ssd/musics` は正のライブラリとして使い続ける（`readonly` は外してよい。
`@pre-migration` スナップショットは残す）。リリース時は次の順で作り直す:

0. **マイグレーションをもう一度 `0001_init.sql` へ畳む**（リリースのコミットで。D-88 追記）。D-88 の後に
   足した `0002`〜（2026-09-26 時点で `0002_inbox_discard` / `0003_inbox_cover` / `0004_albums_category_null_index`）を
   `0001_init.sql` に統合し、空 DB へ流した最終スキーマが畳む前と一致すること（D-88 と同じ比べ方）を確かめる。
   リハーサル環境の DB は版が新しすぎて開けなくなるが、次の 1 で消すので構わない。これを入れたイメージで
   最初の `vX.Y.Z` を切り、以下はそのイメージで行う。**タグを切った後は二度と畳まない**
1. リハーサル環境を消す: spindle を止め、`ssd/media`、`hdd/media`、`ssd/apps/spindle` を
   `zfs destroy -r`（Derived / DB / バックアップも開発用なので捨てる）
2. §0 から本手順をやり直す（preflight → migrate_plan → データセット作成 → readonly → snapshot →
   rsync → 検証 → chown → ACL / スナップショット設定 → 起動 → 照合）
3. **移行後の一度きりの手順**（webm 由来の `〜のお歌` アルバム。docs/TASKS.md P4-14 / P4-15）。
   入力（アーティストごとの YouTube 再生リスト URL の TSV と前回の計画 CSV）はリポジトリ外に
   保管してある。旧パイプラインは「再生リスト名 = アルバム名、リスト内の位置 = `TRACKNUMBER`」で
   並べていたので、この順を正とする:
   1. `scripts/backfill_source_url.py --playlists … --spindle … --out …` で計画 CSV を出し、目視
      （`title-mismatch` / `no-track` / `extra-track` / `kept` と、位置推定の `verified-by-neighbors` を
      確認）→ `--apply` で `SOURCE_URL` を書く（アルバムごとに 1 バッチ。巻き戻し可）。位置推定は区間内の
      入れ替えを検出できないので、CSV で確かめてから `--include-inferred` を付けて書く。既に
      `SOURCE_URL` を持つ行は上書きしない
   2. **再生リストが無いアルバム**の `SOURCE_URL` を手で付ける（リポジトリ外の `singles.tsv`。列:
      albumartist / album / TITLE / 動画 URL。1 曲ずつ受領したもの。表のセルのダブルクリックでは
      `SOURCE_URL` を編集できないので、プロパティタブの Metadata で「フィールドを追加」する）。いまは
      `柊マグネタイトの曲` の「再見ロマネスク feat. 花隈千冬」1 曲だけ。前回 1 の後に残った 5 件
      （ヰ世界情緒 メズマライザー、花譜 ゲシュタルト -崩壊Remix- / コバルトメモリーズ、理芽 Touch、
      明透 毎日）は**ユーザが YouTube 側で再生リストに追加済み**なので、1 の再実行で一緒に付く
      （手作業は要らない。前回は追加前に走らせたため残っていた）
   3. YouTube 画面で 9 本の再生リストを購読に登録（アルバムアーティスト / アルバム = 各 `〜のお歌`、
      category = Library の最上位ディレクトリと同じ語彙: `神椿Studio` 5 本 / `深脊界Studio` 3 本 / `Vtuber`
      （HIMEHINA）。語彙は初回スキャンが Library 直下のディレクトリ名から自動で作るので（D-92）、
      先に作る必要はない）し、「同期」（P4-16、D-78）。同期は先に既存の行の `TRACKNUMBER` と
      ファイル名を再生リストの位置に揃え（tags バッチ → rename バッチ。Derived はタグ上書き・移動で追随）、
      `SOURCE_URL` の無いものだけを位置付きで Inbox に投入する → 承認で配置（初期値の番号がそのまま
      位置）→ 配置の後続で自動的にもう一度同期 → 結果が「番号を 0 件揃え」「揃えられない」無しで完了。
      非公開・削除の動画の位置は番号が飛ぶ（「取れない」一覧に出る）。
      承認画面で「⚠ Library に同名」（P4-19）が出たら、件の長さと Library 側の長さを比べる。**長さが一致する
      ものは同じ音源の二重取り込み**なので承認しない。原因は 2 通りある: (a) 同じ音源の再アップロード、
      (b) 既存行の `SOURCE_URL` が別の動画を指している（2026-09-24 のリハーサルの VALIS: 再生リストの位置 224 が
      曲名をタイトルに含む 628 秒のドキュメンタリーで、270 秒の曲の行にその URL が付き、本来の動画を同期が
      別の曲として落とした。手順 1 の補填は位置とタイトルが合っても長さが食い違えば採らないようにしたので、
      今は起きない）。どちらも既存行の `SOURCE_URL` を件の動画の URL に付け替える（プロパティタブの Metadata）と、
      次の同期で「持っている」と判定されて落ちてこない。再生リストに Library へ入れない動画があるなら、
      再生リスト側から外す（除外の仕組みは作らない）。長さが違えば別テイク（Cover / Live ver.）なので
      そのまま承認してよい。**Library のファイルを消す op は無い**ので、
      既に二重に入れてしまったら SMB でファイルを消す → スキャンが `missing_since` を立てる → GC
      （既定 7 日）で行が消える
   1 と 2 は実データでは 1 回しか行わない（以後は購読の同期が日常運用）。リハーサル環境で一度通してから行う
4. **ライブラリ全体を `[layout]` の書式に揃える**（一度きり。2026-09-25 のリハーサルで決定）。旧パイプラインの
   パスは spindle の書式と表記が違うところがある。リネームは巻き戻せるが数千件のファイル移動になるので、
   ほかの一度きりの手順（上の 3）と Derived の生成が終わってから行う:
   1. **同名の別版に `EDITION` を付ける**（`ssd/musics` のファイルには無いので再移行のたびに要る）。
      THE IDOLM@STER CINDERELLA MASTER Solo Series の `IM@S CM Solo - 034 速水奏/Hi-Res/` と
      `IM@S CM Solo - 041 大槻唯/Hi-Res/` の各 1 曲に `EDITION=Hi-Res`（一括編集で「フィールドを追加」）。
      付けないと通常版と同じパスになり、リネームのプレビューで「降格に必要な値が無い: edition」になる。
      付ければ通常版は元の名前、Hi-Res は `… (Hi-Res)` に分かれる（D-43 追記）
   2. 全選択で「リネームをプレビュー」。リハーサルでは 9,100 曲中 約 5,200 曲が変わり、衝突 0 だった。
      変わるのは表記の違いだけ: アルバム名の `/` を旧パイプラインは `-`、spindle は `／` にする
      （`FAKE OFF - 天使と悪魔` → `FAKE OFF ／ 天使と悪魔`）、複数枚組のファイル名 `1.01.` → `1-01.`、
      `Anime/THE IDOLM@STER/<albumartist>/…` のような `[layout]` に無い中間の階層が無くなる。
      **`_Unsorted/` 行きが 0 であること**（category は初回スキャンで直下のフォルダ名から付く。D-92）と、
      「パスを生成できない」が 0 であることを確かめてから適用する
   3. 適用後、Derived は移動に追随し、m3u8 の書き出しは自動で書き直される。foobar2000 など外部の
      プレイヤーのライブラリはパスが変わるので読み直しが要る
5. 正式運用後、数日間問題が出ないことを確認してから `ssd/musics` を破棄する。`AAC/`（48G）は
   この破棄で一緒に消える

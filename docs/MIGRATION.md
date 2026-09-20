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

# 3. 検証（チェックサム比較。何も出なければ成功）
V="sudo rsync -aHXn --checksum --itemize-changes --from0"
$V --files-from=/tmp/plan/library-original.list /mnt/ssd/musics/Original/ /mnt/ssd/media/Library/
$V --files-from=/tmp/plan/library-opus.list     /mnt/ssd/musics/Opus/     /mnt/ssd/media/Library/
$V --files-from=/tmp/plan/archive-original.list /mnt/ssd/musics/Original/ /mnt/hdd/media/Archive/

# 4. 所有者をコンテナの実行 UID/GID に合わせる
sudo chown -R 1000:1000 /mnt/ssd/media /mnt/hdd/media /mnt/ssd/apps/spindle

# 5. spindle 初回スキャン完了と数日の運用までは ssd/musics を破棄しない
```

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

- `backup` ジョブが 1 世代以上取れていること（`/mnt/ssd/apps/spindle/backup/`）
- 一括編集を 1 件行い、巻き戻しが通ること（P0 の完了条件）
- ロスレス → FLAC 正規化（P1-4）で ALAC を FLAC にする。退避した ALAC は `Archive/` に
  30 日置かれてから GC される
- Derived は P1-10 が Library から再生成する。旧 `Opus/` の 7,568 本は移していない

## 5. リリース時の再移行と旧データセットの破棄

リリースまで `ssd/musics` は正のライブラリとして使い続ける（`readonly` は外してよい。
`@pre-migration` スナップショットは残す）。リリース時は次の順で作り直す:

1. リハーサル環境を消す: spindle を止め、`ssd/media`、`hdd/media`、`ssd/apps/spindle` を
   `zfs destroy -r`（Derived / DB / バックアップも開発用なので捨てる）
2. §0 から本手順をやり直す（preflight → migrate_plan → データセット作成 → readonly → snapshot →
   rsync → 検証 → chown → ACL / スナップショット設定 → 起動 → 照合）
3. 正式運用後、数日間問題が出ないことを確認してから `ssd/musics` を破棄する。`AAC/`（48G）は
   この破棄で一緒に消える

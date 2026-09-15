# 移行手順

旧ライブラリから spindle のデータセット構成へ移す手順。
**`casesensitivity` と `normalization` はデータセット作成時のみ指定可能**で
後から変更できないため、既存データセットの `zfs rename` による流用はしない
（`docs/DECISIONS.md` D-19）。

## 0. 事前チェック

```bash
python3 scripts/preflight.py /mnt/tank/music \
  --dest /mnt/tank/media --plan rename.sh
# 照合用マニフェストは Library へ入る部分（旧 Opus/）だけで取る。
# 旧ツリー全体で取ると Original/ と Playlists/ の行、および Opus/ プレフィックスの
# 差で移行後と一致しない。--audio-only で音声ファイル（preflight.py の AUDIO_EXT、
# 大文字拡張子も含む）に限定し、DB のトラック数と同じ集合にする
python3 scripts/preflight.py /mnt/tank/music/Opus --manifest before.tsv --audio-only
```

`insensitive` + `formD` では、比較キーが一致する 2 ファイルは共存できない。
このチェックを飛ばすと、rsync が途中で失敗するか、悪い場合は片方が黙って
上書きされる。判定は「NFD 正規化 → casefold」した結果の一致で行うため、
大小文字と正規化が両方絡む組み合わせも検出できる。

ブロッカー（衝突 / 不正な UTF-8 名 / 255 バイト超 / symlink / hardlink / SMB 禁止名 /
読めないパス / 通常ファイルでないもの / コピー先の容量不足）を解消してから先へ進む。
SMB 禁止名（禁止文字・末尾ドット/スペース・予約名）は spindle のスキャナが対象外にするため、
残したまま移行するとトラック数が一致しない。`--plan` のリネーム案で直す。
symlink は spindle が辿らないため実体に置き換え、hardlink は `(dev, inode)` による同一性を
壊すため片方を実コピーにするか削除する（`docs/DECISIONS.md` D-26）。
`rename.sh` はそのまま実行せず必ず目視すること。中身が同一の重複であれば
リネームではなく削除が正しい。

## 1. データセット作成

### ZFS データセット

```bash
zfs create tank/media

# 作成時のみ指定可能なプロパティ（後から変更不可）
COMMON="-o casesensitivity=insensitive -o normalization=formD"
# normalization を設定すると utf8only=on が強制される

zfs create $COMMON -o recordsize=1M -o compression=lz4 -o atime=off tank/media/Library
zfs create $COMMON -o recordsize=1M -o compression=lz4 -o atime=off tank/media/Derived
zfs create $COMMON -o recordsize=1M -o compression=lz4 -o atime=off tank/media/Archive
zfs create $COMMON -o recordsize=1M -o compression=lz4 -o atime=off tank/media/Inbox
zfs create -o recordsize=16K -o compression=lz4 -o atime=off tank/apps/spindle
```

| dataset | recordsize | snapshot | 備考 |
|---|---|---|---|
| Library | 1M | 毎日 + 一括編集前 | 唯一の正 |
| Derived | 1M | なし | 再生成可能。レプリケーション対象外 |
| Archive | 1M | 週次 | 追記のみ |
| Inbox | 1M | なし | 承認前の一時領域 |
| apps/spindle | 16K | 毎日 | SQLite。ページサイズに合わせる |

**`casesensitivity` と `normalization` はデータセット作成時のみ指定可能で、
後から変更できない。** このためライブラリは既存データセットの rename ではなく、
新規作成 + ファイルコピーで移行する（下記）。

- `casesensitivity=insensitive`: Windows の foobar2000 が `Cover.jpg` を、
  spindle が `cover.jpg` を作る事故を ZFS 層で潰す。spindle は自身の生成パスの
  一意性は保証できるが、他クライアントが作るファイルまでは制御できない
- `normalization=formD`: 日本語の濁点・半濁点には NFC（`が` 1 文字）と
  NFD（`か` + 結合濁点）の 2 表現がある。macOS 由来のパスは NFD になりがちで、
  正規化なしだと「見た目が同じで別ファイル」が発生する。目視では気づけない

Inbox は Library と別データセットなので move は実コピーになるが、
1 回あたりアルバム 1 枚（数百 MB〜数 GB）なので実用上の問題はない。

## 2. コピーと検証

**前提: Library と同容量の空きが必要**（一時的に 2 倍を消費する）。

```bash
# 1. 旧ライブラリを固定してから作業する
zfs set readonly=on tank/music
zfs snapshot tank/music@pre-migration

# 2. 事前チェックは §0 の preflight.py が唯一の手順。exit 0 を確認してから進む
#    （手書きの find / awk による判定は、空白を含むパスや NFD/casefold の複合ケースで
#      誤判定するため置かない）

# 3. コピー（zfs send/recv は不可。作成時プロパティが継承されてしまうため）
rsync -aHX --info=progress2 /mnt/tank/music/Opus/     /mnt/tank/media/Library/
rsync -aHX --info=progress2 /mnt/tank/music/Original/ /mnt/tank/media/Archive/
rsync -aHX --info=progress2 /mnt/tank/music/Playlists/ /mnt/tank/media/Playlists/

# 4. 検証（チェックサム比較。差分が出なければ成功）
rsync -aHXn --checksum --itemize-changes /mnt/tank/music/Opus/ /mnt/tank/media/Library/

# 5. 所有者をコンテナの実行 UID/GID に合わせる
chown -R 1000:1000 /mnt/tank/media

# 6. spindle 初回スキャン完了と全曲の目視確認までは tank/music を破棄しない
```

**ACL は rsync で引き継げない。** TrueNAS の SMB データセットは NFSv4 ACL を
使うが、`rsync -A` が扱うのは POSIX ACL であり互換がない。コピー後に
TrueNAS の ACL エディタで新データセットにプリセットを適用し直すこと。

旧データセットの破棄は、P0 のスキャンが完走し、トラック数が一致し、
数日間の運用で問題が出ないことを確認してから。

## 3. 移行後の照合

```bash
# コピー先で同じマニフェストを取り、差分がないことを確認する。
# before.tsv は /mnt/tank/music/Opus を root に取ったものなので、相対パスが揃う
python3 scripts/preflight.py /mnt/tank/media/Library --manifest after.tsv --audio-only
diff <(cut -f1,2 before.tsv) <(cut -f1,2 after.tsv)
tail -n +2 after.tsv | wc -l        # = 音声ファイル数
```

初回スキャン完了後、DB のトラック数が `after.tsv` の行数（ヘッダ除く）と一致することを
確認する。音声拡張子の判定は `preflight.py` の `AUDIO_EXT` が唯一の定義で、スキャナの
受け付ける拡張子もこれと揃える。一致しない場合はタグ読み取り失敗を疑う。

あわせて、旧 `Opus/` と `Original/` の同じ曲を数件選び、`ffprobe` でコーデックと
ストリーム長が一致すること（remux であり再エンコードでないこと）を確認する。
これが「Library の `.opus` が master、`Archive/` の webm は生データ」という前提の根拠になる。

## 4. 旧データセットの破棄

P0 のスキャンが完走し、トラック数が一致し、数日間の運用で問題が出ないことを
確認してから。それまで `tank/music` は `readonly=on` のまま残す。

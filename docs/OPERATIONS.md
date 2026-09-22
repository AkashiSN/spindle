# 運用手順

移行後の日常運用で、コードを読まずに済ませたい手順をまとめる。移行そのものは
`docs/MIGRATION.md`。

## イメージの更新（TrueNAS カスタムアプリ）

イメージは GHCR `ghcr.io/akashisn/spindle`（public。pull にトークン不要）。タグは `latest`（最新の
リリース `vX.Y.Z`）/ `X.Y` / `X.Y.Z` と、開発用の `edge`（`main` の最新）/ `sha-<7 桁>`。本番は
`latest` か `X.Y` を指す。`edge` は DB の互換を（前進のマイグレーション以外）約束しない。

### カスタムアプリの作り方

TrueNAS の Apps → Discover → Custom App → **Install via YAML** に `deploy/compose.yaml` を写す。
そのまま使えない箇所:

- `devices` / `device_cgroup_rules` / `group_add`: CD ドライブ（`/dev/sr0` と cdrom グループの GID。
  `/dev/sg*` は要らない）。ドライブが無い機体では 3 つとも消す（残すと compose が起動に失敗する。
  アプリ側はデバイスを開けなくても起動し、CD 画面に「ドライブが無い」と出す）
- `user`: 既存ライブラリの所有者の UID:GID に合わせる（違うとタグ書き込みが全滅する）
- `volumes`: Library / Derived / Archive / Inbox / Playlists / data の実パスと、メタデータプラグイン
  （`spindle-ytmusic-meta`。イメージには入っていない）のマウント
- `SPINDLE_INITIAL_PASSWORD`: 初回だけ。初期化が済んだら消す

### 更新

1. 更新前に DB のバックアップがあることを確認する（下記「バックアップ」。自動が動いていれば
   `/data/backup/` の最新で足りる）。マイグレーションは**起動時に自動で前進のみ**で、戻す手段は
   バックアップからの復元だけ
2. TrueNAS の Apps でアプリを選び **Update / Pull image**（同じタグの新しい digest を取る）→ 再作成。
   compose で動かしているなら `docker compose pull && docker compose up -d`
3. `GET /health` の `version`（= `spindle --version`）が期待する版（Release のタグ、または `sha-<7>` の
   sha）になっていること、`ytdlp` が新しくなっていることを確認する。起動ログの「DB を開いた
   schema_version=」で前進したマイグレーションが分かる
4. 戻すとき（新しい版が進めたマイグレーションは旧版が知らないので、DB も一緒に戻す）: **アプリを止める** →
   `/data/spindle.db`（と `-wal` / `-shm`）を退避し、更新前のバックアップから復元（下記「復元」の手順）→
   旧タグ（`X.Y.Z`）を指して再作成 → `/health` の `version` で旧版を確認。順序を違えると、旧版を新 schema の
   DB で起動したり、稼働中の DB を差し替えたりすることになる

### YouTube の取り込みが失敗し始めたら

まずイメージを更新する（yt-dlp が古いと YouTube の抽出が壊れる。新版は Renovate が PR にし、マージ
すると `edge`、次のリリースで `latest` に届く）。`/health` の `ytdlp` で版を確認する。それでも駄目なら
`config.toml` の `[ytmusic].ytdlp_args`（`USERGUIDE.md` §11.3「YouTube」）。

## バックアップ

DB（`/data/spindle.db`）は原則キャッシュで、ファイルから再構築できる（SPEC §3）。
ただし **プレイリスト / 編集履歴 / 検証結果 / ジョブ履歴** は DB にしか無い。
バックアップは任意ではない。

### 自動

`backup` ジョブが `/data/backup/` へ `VACUUM INTO` で一貫したスナップショットを書く。

- 周期は `[backup].interval_hours`（既定 24）。起動時と 10 分ごとに「最後の終端 `backup`
  ジョブからの経過」で判定し、経っていれば投入する。ジョブ一覧（`GET /api/jobs`）に
  `backup` として出る
- ファイル名は `spindle-<YYYYMMDDTHHMMSSZ>.db`（UTC）。WAL / SHM を伴わない単独ファイルで、
  そのまま `spindle.db` に置けば使える
- tmp（`.spindle-….db.tmp`）に書いて `quick_check` → fsync → rename（同名があれば上書きせず
  失敗）→ `backup/` と `/data` の fsync の順で確定する。途中で落ちた tmp は次回の実行が消す
- 空き容量が「DB サイズ + 64 MiB」を下回るなら書かずに失敗する。失敗はジョブの
  バックオフで再試行され、上限で `failed` になる。`failed` はジョブ一覧に残るので
  `last_error` を見る
- 世代は `[backup].retention_generations`（既定 14）件。名前順で新しいものから残し、
  古いものを消す。**この名前形式以外のファイルは触らない**ので、手で取ったコピーを
  `backup/` に置いても GC されない
- `apps/spindle` データセットのスナップショット（毎日）と二重化する。スナップショットは
  `spindle.db` と `backup/` の両方を含む

### 手で取る

アプリを止めずに取るなら、コンテナの外から `VACUUM INTO` を使う。`cp` は WAL 中の
変更を取りこぼすので使わない。

```bash
sqlite3 /mnt/ssd/apps/spindle/spindle.db \
  "VACUUM INTO '/mnt/ssd/apps/spindle/backup/manual-$(date -u +%Y%m%dT%H%M%SZ).db'"
```

## 復元

前提: コンテナを止める。動いたまま差し替えると、開いているコネクションが古い inode を
持ち続けて書き込みが行方不明になる。

```bash
cd /mnt/ssd/apps/spindle
docker compose -f /path/to/compose.yaml stop spindle

# 1. 壊れた DB を退避（WAL / SHM も一緒に。残すと差し替えた DB に古い WAL が適用される）
mkdir -p broken
mv spindle.db broken/ 2>/dev/null
mv spindle.db-wal broken/ 2>/dev/null
mv spindle.db-shm broken/ 2>/dev/null

# 2. 戻す世代を選んで差し替える
ls -1 backup/
cp backup/spindle-20260916T030000Z.db spindle.db
chown 1000:1000 spindle.db          # compose の user に合わせる

# 3. 起動。起動時スキャンがファイルとの差分を吸収する
docker compose -f /path/to/compose.yaml start spindle
```

起動後に確認すること:

- `GET /api/jobs` で起動時 `scan` が `done` になり、`backup` が投入される（復元した DB
  には古い記録しか無いので、すぐ 1 世代取れる）
- トラック数がライブラリの音声ファイル数と一致する（移行時の照合と同じ。
  `docs/MIGRATION.md` §3）
- 編集履歴（`GET /api/history`）とプレイリストがバックアップ時点の内容で見える

### 何が失われるか

バックアップ時点より後の変更のうち **DB にしか無いもの**: プレイリストの編集、
編集履歴、検証結果、ジョブ履歴。

ファイル側は失われない。バックアップ後に反映済みだったタグ編集・リネームはファイルが
新しい値を持っているので、起動時スキャンが「外部変更」として取り込む（ファイルが正）。
その編集の履歴だけが消えるので、巻き戻しはできなくなる。

バックアップ時点で `prepared` / `applying` だった編集バッチは、復元した DB では未反映の
op を抱えたままになる。起動時リカバリが子ジョブを再投入し、事前条件（`tag_version` /
inode）が合えば続きを反映し、合わなければ `skipped_conflict` で止める。ファイルは
旧値のままなので壊れない。

## 復元ドリル

`tests/backup.rs::restore_drill_rescans_to_the_same_state` が同じ手順を自動で通す
（編集履歴 3 バッチ + プレイリスト 2 本 → バックアップ → DB と WAL / SHM を削除 →
差し替え → 再スキャン → トラック数・履歴・プレイリストの一致）。実機で初めて
復元するときも、まず別ディレクトリにコピーして手順を一度通してから本番に当てる。

## ロスレスの FLAC 化（正規化）

Library の WAV / ALAC / AIFF を FLAC に置き換える（SPEC §7.4、D-45 / D-46）。移行で取り込んだ
ALAC 7,572 本が主対象。UI は未着手なので API を直接叩く。変換は `normalize` ジョブ（並列 2）が
1 曲ずつ行い、元ファイルは `Archive/` の同じ相対パスへ退避される（`archived_files` 台帳、
既定 30 日後に GC が回収。それまでは履歴画面の [巻き戻す] で元に戻せる）。

前提: Archive（`/mnt/hdd/media/Archive`）に退避ぶんの空き（ALAC 全件で約 220G）がある。
`config.toml` の `[normalize].wav_to_flac = true`。

```bash
BASE=http://truenas:8080
# 1. ログイン（Cookie を保存）
curl -s -c cookie.txt -H 'Content-Type: application/json' -H 'Sec-Fetch-Site: same-origin' \
  -d '{"password":"…"}' "$BASE/api/auth/login"
# 2. プレビュー。filter は表のフィルタ式（{} で全曲。FLAC / 非可逆は unchanged に数えられるだけ）
curl -s -b cookie.txt -H 'Content-Type: application/json' -H 'Sec-Fetch-Site: same-origin' \
  -d '{"selection":{"filter":"{}"}}' "$BASE/api/normalize/preview" | tee preview.json | \
  jq '{count, changed, unchanged, conflict, pending_excluded}'
# 3. 適用（token は 15 分で期限切れ）
TOKEN=$(jq -r .selection_token preview.json)
curl -s -b cookie.txt -H 'Content-Type: application/json' -H 'Sec-Fetch-Site: same-origin' \
  -d "{\"selection_token\":\"$TOKEN\",\"description\":\"ALAC → FLAC\"}" "$BASE/api/normalize/apply"
# 4. 進み具合はジョブ一覧か履歴画面で（batch_id の applied / conflict / failed）
curl -s -b cookie.txt "$BASE/api/jobs" | jq .summary
```

- 一部だけ試すなら `"selection":{"ids":[…]}`（表で選んだ id）で 1 アルバムぶんから
- `conflict` は宛先（`.flac`）を別のトラックが占有している行。`failed` は PCM MD5 の不一致か
  ビット深度非対応で、元ファイルは無傷のまま残る。履歴画面の op の `error` を見る
- 変換中（op が pending）のトラックはタグ編集・リネームが 409 になる。バッチ単位のキャンセルは
  `POST /api/history/:batch/cancel`（変換中の 1 曲は止め、配置を始めた曲は完了を待つ）
- 変換後の再スキャンで差分が出ないことを確認する（`new` / `moved` / `missing` が 0）

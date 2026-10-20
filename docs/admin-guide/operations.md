# 運用手順

移行後の日常運用で、コードを読まずに済ませたい手順をまとめる。移行そのものは
[移行手順](migration.md)。

## イメージの更新（TrueNAS カスタムアプリ）

イメージは GHCR `ghcr.io/akashisn/spindle`（public。pull にトークン不要）。タグは `latest`（最新の
リリース `vX.Y.Z`）/ `X.Y` / `X.Y.Z`、リリース候補の `X.Y.Z-rc.N`（`vX.Y.Z-rc.N` タグ。`latest` / `X.Y` は
付かない）と、開発用の `edge`（`main` の最新）/ `sha-<7 桁>`。本番は `latest` か `X.Y` を指す。`edge` は DB の互換を（前進のマイグレーション以外）約束しない。

### カスタムアプリの作り方

TrueNAS の Apps → Discover → Custom App → **Install via YAML** に `deploy/compose.yaml` を写す。
そのまま使えない箇所:

- `devices` / `device_cgroup_rules` / `group_add`: CD ドライブ（`/dev/sr0` と cdrom グループの GID。
  `/dev/sg*` は要らない）。ドライブが無い機体では 3 つとも消す（残すと compose が起動に失敗する。
  アプリ側はデバイスを開けなくても起動し、CD 画面に「ドライブが無い」と出す）。
  **ドライブのつなぎ先に注意**: 一部の SATA コントローラでは、複数セクタの READ CD の中のセクタ境界で
  音声が 16 バイト（4 サンプル）ずれ、C2 もエラーも出ないまま照合（AccurateRip / CTDB）が全曲通らない。
  実機（AMD 600 シリーズのチップセット SATA、1022:43f6）で起き、同じドライブを USB-SATA 変換につなぐと
  消えた。PCIe カードの ASMedia ASM1062（1b21:0612）の SATA でも起きない（2026-09-25、TASKS P2-5）。
  照合が一度も通らず rip.log の「ずれ」が毎回数百〜千回出るなら、まずつなぎ先を変える
- `user`: 既存ライブラリの所有者の UID:GID に合わせる（違うとタグ書き込みが全滅する）
- `volumes`: Library / Derived / Archive / Inbox / Playlists / data の実パスと、メタデータプラグイン
  （`spindle-ytmusic-meta`。イメージには入っていない）のマウント
- `SPINDLE_INITIAL_PASSWORD`: 初回だけ。カスタムアプリには `.env` が無いので、`${…:?…}` の行を
  `- SPINDLE_INITIAL_PASSWORD=<生成したパスワード>` の値に書き換える。ログの「SPINDLE_INITIAL_PASSWORD から
  パスワードを初期化した」とログインを確かめたら、アプリの Edit で `environment` ごと消す
- **イメージは作る前に pull しておく。** ホストに同じタグ（`edge` など）の古いイメージが残っていると、
  カスタムアプリはそれをそのまま使う（2026-09-24 のリハーサルで旧 compose の `edge` が起動した）。
  `sudo docker pull ghcr.io/akashisn/spindle:<tag>` を先に打つか、作った直後に `/health` の `version` を見て、
  違えば止める → `/data` の DB を消す → Pull image → 起動し直す

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

### リリース候補（RC）を本番で確かめてから出す

画面の崩れなど実機でしか分からないことがあるので、正式版の前に RC を本番で確かめる。カスタムアプリは
ホストにある `latest` をそのまま使う（上記）ので、RC のイメージにホストの上で `latest` の名前を付け直し、
アプリの設定は変えない。

1. `main` の確かめたいコミットに `vX.Y.Z-rc.N` タグを打って push する。CI が `X.Y.Z-rc.N` のイメージと
   pre-release の Release を作る
2. 更新前のスナップショットを取る（例 `zfs snapshot ssd/apps/spindle@pre-vX.Y.Z-rc.N-<日付>`）
3. `sudo docker pull ghcr.io/akashisn/spindle:X.Y.Z-rc.N && sudo docker tag ghcr.io/akashisn/spindle:X.Y.Z-rc.N ghcr.io/akashisn/spindle:latest && sudo midclt call -j app.redeploy spindle`
4. `/health` の `version` が `vX.Y.Z-rc.N` になったことを見て、画面で変更点を確かめる
5. 問題なければ**同じコミット**に `vX.Y.Z` を打つ。CI が `latest` を公開したら
   `sudo docker pull ghcr.io/akashisn/spindle:latest && sudo midclt call -j app.redeploy spindle`
6. 問題があれば前の版へ戻す: `sudo docker pull ghcr.io/akashisn/spindle:<前の X.Y.Z> && sudo docker tag ghcr.io/akashisn/spindle:<前の X.Y.Z> ghcr.io/akashisn/spindle:latest && sudo midclt call -j app.redeploy spindle`。
   RC がマイグレーションを進めていたら、上の「更新」の 4 と同じく DB も戻す（アプリを止めてから
   スナップショットかバックアップで復元）。直したら `rc.N+1` を打ち直す

### 外から https で使う（リバースプロキシ / Cloudflare Tunnel）

プロキシが spindle へ http で転送するときは、`config.toml` の `[auth].trusted_proxies` にプロキシの接続元を入れる。
入れないと spindle は `X-Forwarded-Proto: https` を信用せず、自分を `http://<host>` とみなすので、ブラウザの
`Origin: https://<host>` と一致せず**ログインなど変更系の要求が CSRF 検査で 403 になる**（Cookie にも Secure が付かない）。
接続元は spindle のログの `ログイン失敗 ip=` などで分かる。実機は cloudflared が host ネットワークから
`172.16.100.24:80` へ転送するので、接続元は NAS 自身の `172.16.100.5`（`trusted_proxies = ["172.16.100.5/32"]`。
2026-09-26）。変えたら再起動する

nginx（Nginx Proxy Manager など）を挟むときは、その接続元も `trusted_proxies` に入れる（実機は
`172.16.100.20`。2026-10-15）。nginx は既定で応答を溜める（`proxy_buffering`）ので、SSE（`/api/events`）が
届かず、右上の点が赤のまま曲が 1 件も出なくなる（画面は SSE が開いてから一覧を取る。D-36）。spindle は
`/api/events` の応答に `X-Accel-Buffering: no` を付けてこれを避けている。それより古い版や、ヘッダを捨てる
プロキシでは、プロキシ側で `proxy_buffering off;` にする

### YouTube の取り込みが失敗し始めたら

まずイメージを更新する（yt-dlp が古いと YouTube の抽出が壊れる。新版は Renovate が PR にし、マージ
すると `edge`、次のリリースで `latest` に届く）。`/health` の `ytdlp` で版を確認する。それでも駄目なら
`config.toml` の `[ytmusic].ytdlp_args`（[操作手順](../user-guide/README.md) §11.3「YouTube」）。

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
  [移行手順](migration.md) §3）
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

### 端末は引き継ぎし直す

復元した DB は、バックアップの時点より後に足した曲の `tracks.id` を知らない。起動時スキャンはその曲に
id を振り直すが、元と同じ id になるとは限らない。端末の写し（Android の manifest、Mac のエージェントの
state）は `track_id` で曲を指すので、バックアップより後に同期した端末をそのまま同期すると、
別の曲への更新・移動になる。Mac ではミュージック.app の track の再生回数・レート・追加日が
別の曲に移る（D-105）。

だから復元したら、**同期する前に**、バックアップより後に同期した端末（普通はすべて）を引き継ぎし直す。
端末の行を消しても、端末上のファイルとミュージック.app の track には触らない。

1. 消す前に、端末の**名前・配信の形式（opus / aac）・送る範囲（全曲 / 印のプレイリスト）**を控える。
   バックアップより後に設定を変えた端末や、バックアップに行の無い端末は、復元した DB の値ではなく実際に
   使っていた設定を入れる。送る範囲が分からなければ「全曲」にする（Library にある曲を端末から消すことが無い。
   後から絞れる）
2. 端末タブで端末を削除し、引き継ぐ
   - **Android**: 「前の登録の保存先（引き継げる）」から、控えた名前・形式・範囲で引き継ぐ（SPEC §7.11「引き継ぎ」）
   - **Mac**: 控えた名前・形式・範囲で新しく行を作り、その pair コードで `spindle-agent pair --adopt <url> <code>`
     を実行する（SPEC §7.11「iPhone」の「引き継ぎ」）
3. 引き継いだ後の最初の差分（Android は端末タブの差分表、Mac は `spindle-agent sync` の表示）を、確定する前に
   確かめる。**予期しない削除があれば確定しない**（Android は同期しない、Mac は `N`）。範囲が狭すぎると、
   引き継いだ曲が削除になり、Mac ではミュージック.app の track ごと消える。範囲を直してから差分を取り直す

曲は `rel_path` で今の DB の id に付け替わり、形式が元と同じなら中身の同じ曲は送り直されない（形式が違うと
送り直しになる。Mac はミュージック.app の track を保ったまま中身を差し替える）。Mac は、バックアップより後に
発行したトークンがそもそも復元で失われているので、どのみち pair し直しになる。

## 復元ドリル

`tests/backup.rs::restore_drill_rescans_to_the_same_state` が同じ手順を自動で通す
（編集履歴 3 バッチ + プレイリスト 2 本 → バックアップ → DB と WAL / SHM を削除 →
差し替え → 再スキャン → トラック数・履歴・プレイリストの一致）。実機で初めて
復元するときも、まず別ディレクトリにコピーして手順を一度通してから本番に当てる。

## ロスレスの FLAC 化（正規化）

Library の WAV / ALAC / AIFF を FLAC に置き換える（SPEC §7.4、D-45 / D-46）。移行で取り込んだ
ALAC 7,572 本が主対象。UI は未着手なので API を直接叩く。変換は `normalize` ジョブ（並列 2）が
1 曲ずつ行い、元ファイルは `Archive/` の同じ相対パスへ退避される（`archived_files` 台帳、
既定 7 日後に GC が回収。それまでは履歴画面の [巻き戻す] で元に戻せる）。

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

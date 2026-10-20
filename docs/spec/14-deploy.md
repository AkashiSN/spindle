## 14. デプロイ

### イメージの配布（P4-12、D-79）

- 置き場は GHCR `ghcr.io/akashisn/spindle`（public。pull にトークン不要）。`linux/amd64` のみ
- CI（`.github/workflows/ci.yml`）は web → rust → docker → publish の順（Mac の `spindle-agent` の
  `agent-macos` も publish の前提。下記）。docker ジョブ（PR でも走る。
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
- adb（Android の platform-tools。版と sha256 を Dockerfile で固定）はイメージに同梱し、spindle のクライアントと
  compose の adb サイドカーのサーバが同じものを使う（D-98）
- **Mac の `spindle-agent`**（P5-4c、D-101）: CI の `agent-macos` ジョブ（`macos-latest`）が clippy・test
  （偽のミュージック.app の `fake` 機能）・release ビルドを行い、`spindle-agent-<git describe>-aarch64-apple-darwin.tar.gz`
  （バイナリと `agent/README.md`）と `.sha256` を成果物にする。publish ジョブはこれにも依存し（edge のイメージも
  macOS ジョブの成功が要る）、`vX.Y.Z` タグでは Release に添付する。macOS（arm64）だけで、
  **署名・公証はしない**（Developer ID が無い。必要になったら入れる）。導入は README の
  `xattr -d com.apple.quarantine` と、初回の Automation（ミュージック.app の操作）の許可
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
    init: true                   # PID 1 を tini にする（取り消しで kill した孫プロセスをゾンビにしない）
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
      - adb-run:/run/adb:ro      # adb サイドカーの Unix ソケット（D-98）。:ro は外さない（下記）
      - /mnt/ssd/apps/spindle/bin/spindle-ytmusic-meta:/usr/local/bin/spindle-ytmusic-meta:ro   # メタデータプラグイン（D-70）
    ports:
      - "8080:8080"
    restart: unless-stopped

  adb:                           # adb サーバだけを動かすサイドカー（D-98）。Android の同期を使わないなら不要
    image: ghcr.io/akashisn/spindle:latest
    network_mode: host
    init: true
    entrypoint: ["/bin/sh", "-c", "umask 000 && exec /usr/local/bin/adb -L localfilesystem:/run/adb/adb.sock nodaemon server"]
    volumes:
      - /dev/bus/usb:/dev/bus/usb
      - adb-run:/run/adb
      - /mnt/ssd/apps/spindle/adb-keys:/root/.android   # USB デバッグの許可の鍵。消すと端末で許可し直す
    device_cgroup_rules:
      - 'c 189:* rmw'            # USB（/dev/bus/usb）
    restart: unless-stopped

volumes:
  adb-run: {}
```

実体は `deploy/compose.yaml`（PID 1 を tini にする `init: true` などのコメントもそこ）。

**注意点:**

- USB 接続だとデバイス再列挙でノード番号が変わり得る（sr0 → sr1）。
  `/dev/disk/by-id/...` を指すか、SATA 接続を推奨
- UID/GID が既存ライブラリの所有者と一致しないとタグ書き込みが全滅する
- udev はコンテナに届かないため、ディスク挿入検知はポーリング
- ホストにドライブが無いと `devices` の行で compose が起動に失敗する。ドライブの無い機体では
  `devices` / `group_add` / `device_cgroup_rules` を消す。アプリ側は `[rip].device` を開けなくても
  起動し、CD 画面に「ドライブが無い」と出す

**adb サイドカー**（Android の同期。§7.11、D-98）:

- **`network_mode: host`**: 独自のネットワーク名前空間では、挿し直した端末を拾わない（USB のホットプラグの
  通知 = netlink の uevent が届かない。実機で確認）。代わりに **TCP は開かず**、名前付きボリューム `adb-run` 上の
  Unix ソケット（`-L localfilesystem:/run/adb/adb.sock`）だけで待ち受ける（`-a` で 5037 を開くと LAN から端末を
  操作できてしまう）。`umask 000` はサイドカー（root）が作るソケットに spindle（`user: 1000:1000`）が繋げるようにするため
- **spindle 側はソケットのボリュームを `:ro` でマウントする**: `localfilesystem:` を指定しても adb クライアントは
  「サーバに繋がらなければ同じパスで自前のサーバを起こす」。サイドカーが落ちている間にそれが起きると、戻った
  サイドカーとサーバが 2 つになり、端末の監視が USB を持たない方に繋がったままになる。読み取り専用なら connect()
  は通り、自前のサーバの待ち受けの作成だけが `Read-only file system` で失敗する（未接続として扱われる）
- **鍵の置き場**: USB デバッグの許可の鍵（`/root/.android`）を `/mnt/ssd/apps/spindle/adb-keys` に残す。消すと
  端末で許可し直しになる。USB は `/dev/bus/usb` の bind と `c 189:* rmw`
- spindle 側は `[devices].adb_server = "localfilesystem:/run/adb/adb.sock"`（§13）。サイドカーを置かない構成では
  空にする（Android の同期だけが無効になる）。spindle 本体の権限は変えない

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

手順は [移行手順](../admin-guide/migration.md)、振り分けの判断は D-45。要点:

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
  旧値のままなので壊れない。手順は [運用手順](../admin-guide/operations.md)、復元ドリルは `tests/backup.rs`
- DB は「キャッシュ」だが、プレイリスト / 編集履歴 / 検証結果 / ジョブ履歴 / 端末の設定・印・計画・
  エージェントのトークンは DB にしかない（§3）。バックアップは任意ではない
- 古いバックアップへ戻すと、それより後に発行したエージェントのトークン（`devices.agent_selector` と `agent_secret_hash`）は
  失われ、その端末は pair し直しになる（`devices.uuid` も同様に、後から足した端末は消える）
- 古いバックアップへ戻すと `tracks.id` の採番も巻き戻る。バックアップより後に足した曲には起動時スキャンが
  id を振り直し、別の曲に当たり得るので、バックアップより後に同期した端末は**同期する前に**引き継ぎし直す
  （端末の名前・形式・送る範囲を控えて行を消し、同じ設定で Android は「前の登録の保存先」、Mac は新しい行で
  `pair --adopt`。範囲が分からなければ「全曲」。最初の差分に予期しない削除があれば確定しない。
  [運用手順](../admin-guide/operations.md)「端末は引き継ぎし直す」、D-105）。
  GC が最大 id の行を消さないこと（§6「論理削除」）は DB の中の再利用だけを防ぐ

---


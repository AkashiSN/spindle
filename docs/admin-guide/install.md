# 導入

spindle を TrueNAS SCALE のカスタムアプリ（Docker Compose）として動かすまでの手順。
既存の音楽ライブラリを持ち込むときは、先に [移行手順](migration.md) でデータセットを作ってファイルを移す。
動かし始めた後の更新・バックアップは [運用手順](operations.md)。

## 前提

- TrueNAS SCALE（Docker で動くカスタムアプリ）。ほかの Linux の Docker Compose でも動くが、手順は TrueNAS を前提に書く
- ZFS のデータセット。Library / Derived / Archive / Inbox は `casesensitivity=insensitive` +
  `normalization=formD` で**作成時に**作る（後から変えられない。[D-18](../decisions/D-018.md) / [D-19](../decisions/D-019.md)）。
  作り方は [移行手順](migration.md)「1. データセット作成」、置き場所の役割は [SPEC §5](../spec/05-library-layout.md)
- CD を取り込むなら光学ドライブ（`/dev/sr0`）。Android へ同期するなら USB でつないだ端末

## イメージ

イメージは GHCR `ghcr.io/akashisn/spindle`（public。pull にトークン不要）。

| タグ | 中身 |
|---|---|
| `latest` / `X.Y` / `X.Y.Z` | リリース（`vX.Y.Z`）。普段はこれを指す |
| `X.Y.Z-rc.N` | リリース候補。`latest` / `X.Y` は付かない |
| `edge` / `sha-<7 桁>` | `main` の最新。開発用で、DB の互換は前進のマイグレーション以外は約束しない |

外部バイナリ（`ffmpeg` `flac` `opusenc` `cd-paranoia` `cdrdao` `yt-dlp` `adb`）はイメージに入っている。
YouTube のメタデータプラグイン（`spindle-ytmusic-meta`）は入っていないので、使うなら別に置く（[D-70](../decisions/D-070.md)）。

## カスタムアプリの作り方

TrueNAS の Apps → Discover → Custom App → **Install via YAML** に [`deploy/compose.yaml`](../../deploy/compose.yaml) を写す。
そのまま使えない箇所:

- `devices` / `device_cgroup_rules` / `group_add`: CD ドライブ（`/dev/sr0` と cdrom グループの GID。
  `/dev/sg*` は要らない）。ドライブが無い機体では 3 つとも消す（残すと compose が起動に失敗する。
  アプリ側はデバイスを開けなくても起動し、CD 画面に「ドライブが無い」と出す）。
  **ドライブのつなぎ先に注意**: 一部の SATA コントローラでは、複数セクタの READ CD の中のセクタ境界で
  音声が 16 バイト（4 サンプル）ずれ、C2 もエラーも出ないまま照合（AccurateRip / CTDB）が全曲通らない。
  AMD 600 シリーズのチップセット SATA（1022:43f6）で起き、同じドライブを USB-SATA 変換につなぐと
  消えた。PCIe カードの ASMedia ASM1062（1b21:0612）の SATA でも起きない（2026-09-25、P2-5）。
  照合が一度も通らず rip.log の「ずれ」が毎回数百〜千回出るなら、まずつなぎ先を変える
- `user`: 既存ライブラリの所有者の UID:GID に合わせる（違うとタグ書き込みが全滅する）
- `volumes`: Library / Derived / Archive / Inbox / Playlists / data の実パスと、メタデータプラグイン
  （`spindle-ytmusic-meta`）のマウント
- `adb` サービス: Android へ同期しないなら消してよい（spindle の `adb-run` のマウントも一緒に消し、
  `config.toml` の `[devices].adb_server` を空にする。[SPEC §13](../spec/13-config.md)）
- `SPINDLE_INITIAL_PASSWORD`: 初回だけ。カスタムアプリには `.env` が無いので、`${…:?…}` の行を
  `- SPINDLE_INITIAL_PASSWORD=<生成したパスワード>` の値に書き換える（下記「初回起動」）
- **イメージは作る前に pull しておく。** ホストに同じタグ（`edge` など）の古いイメージが残っていると、
  カスタムアプリはそれをそのまま使う。`sudo docker pull ghcr.io/akashisn/spindle:<tag>` を先に打つか、
  作った直後に `/health` の `version` を見て、違えば止める → `/data` の DB を消す → Pull image → 起動し直す

## 設定

`SPINDLE_CONFIG`（既定 `/data/config.toml`）で設定ファイルを指す。例は
[`deploy/config.example.toml`](../../deploy/config.example.toml)、各項目の意味は [SPEC §13](../spec/13-config.md)。
初回起動の前に `/data/config.toml` を置く（無いと既定値で起動する）。起動時に設定を厳格に検証し、
不正なら起動しない（[D-34](../decisions/D-034.md)）。

- YouTube から取り込むなら `[ytmusic].ytdlp_args` に `["--extractor-args", "youtube:lang=ja"]` を入れる
  （無いと翻訳タイトルのある動画は英語のタイトルとチャンネル名が返り、日本語の曲名が取れない）
- リバースプロキシ越しに https で使うなら `[auth].trusted_proxies` が要る（[運用手順](operations.md)
  「外から https で使う」）

## 初回起動

1. 初回起動では環境変数 `SPINDLE_INITIAL_PASSWORD` を読んで argon2id で DB に保存し、以後は無視する。
   環境変数も DB もパスワードが無いとロックモード（`/health` が `{"status":"locked"}`、それ以外は 503）で起動する
2. ログに「SPINDLE_INITIAL_PASSWORD から パスワードを初期化した」と出て、ログインできることを確かめたら、
   アプリの Edit で `environment` ごと消す（平文を compose に残さないため。消しても DB のパスワードで起動する）
3. ブラウザで `http://<ホスト>:8080/` を開いてログインする。起動時にライブラリのスキャン（初回は deep）が
   自動で走り、ジョブ画面の `scan` が終わると全曲が表に出る（目安: 約 9,000 曲で 9 分）
4. 動いている版は `GET /health` の `version`（`spindle --version` と同じ）と `ytdlp` で確かめられる

画面の使い方は [操作手順](../user-guide/README.md)。

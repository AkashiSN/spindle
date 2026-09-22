# spindle

TrueNAS 上で動作する単一コンテナの音楽ライブラリ管理アプリ。
CD リッピング（AccurateRip / CTDB 照合付き）、メタデータ一括編集、ReplayGain、
プレイリスト管理、簡易再生を提供する。Windows 版 foobar2000 の実用機能を代替する。

- 規模: 300GB / 1〜6 万トラック / 単一ユーザ / LAN 内のみ
- 言語: Rust（バックエンド）+ React/TypeScript（フロント）
- 配布: 単一バイナリ（SPA を `rust-embed` で同梱）+ Docker イメージ

## ドキュメント

| 読む人 | ファイル | 内容 |
|---|---|---|
| 使う人 | `docs/USERGUIDE.md` | **画面の操作手順。** ログインから一括編集・巻き戻し・取り込み（Inbox / CD / YouTube）まで |
| 使う人 | `docs/OPERATIONS.md` | 運用手順。イメージの更新、バックアップと復元、正規化 |
| 使う人 | `docs/MIGRATION.md` | 旧ライブラリからの移行手順 |
| 使う人 | `docs/DSL.md` | スマートプレイリストの文法 |
| 作る人 | `CLAUDE.md` | **実装時に最初に読む。** 不変条件・規約・禁止事項 |
| 作る人 | `docs/SPEC.md` | 全体仕様。データモデル、パイプライン、API、UI、デプロイ |
| 作る人 | `docs/DECISIONS.md` | 設計判断とその理由。却下案も記録 |
| 作る人 | `docs/TASKS.md` | 実装タスクと受け入れ条件、進捗 |
| 作る人 | `db/migrations/*.sql` | スキーマ（連番で前進のみ。既存ファイルは書き換えない） |
| 作る人 | `scripts/preflight.py` | 移行前チェック（標準ライブラリのみ） |
| 作る人 | `scripts/migrate_plan.py` | 移行の振り分け計画。`rsync --files-from` の一覧を生成（標準ライブラリのみ） |

## 現在の状態

P0（ライブラリ基盤）〜 P3（ytmusic 統合）は完了し、P4 の改善を進めている。CD の実ドライブ制御と
吸い出し（P2-1 / P2-5）だけが未実装。実機ではリハーサル環境（9,098 トラック）で稼働中で、
`ssd/musics` が引き続き正、リリース時に再移行する。詳細と受け入れ条件は `docs/TASKS.md`。

## 起動

イメージは GHCR にある: `ghcr.io/akashisn/spindle:latest`（最新のリリース `vX.Y.Z`。`X.Y` も可）と
`edge`（`main` の最新。開発用で、DB の互換は前進のマイグレーション以外は約束しない）。
`deploy/compose.yaml` を TrueNAS のカスタムアプリ（compose）として写す。作り方・更新・戻し方は
`docs/OPERATIONS.md`「イメージの更新」。動いている版は `GET /health` の `version`（`spindle --version`
と同じ）と `ytdlp` で確認できる。

`SPINDLE_CONFIG`（既定 `/data/config.toml`）で設定を指す。設定の例は `deploy/config.example.toml`。
初回起動では環境変数 `SPINDLE_INITIAL_PASSWORD` を読んで argon2id で DB に保存し、以後は無視する。
環境変数も DB もパスワードが無いとロックモード（`/health` が `{"status":"locked"}`、
それ以外は 503）で起動する。**初期化が済んだら compose から `SPINDLE_INITIAL_PASSWORD` の
行を消してよい**（平文を残さないため。消しても DB のパスワードで起動する）。

起動後の使い方は `docs/USERGUIDE.md`。

## 開発

```bash
cargo build && cargo test               # バックエンド
cargo clippy -- -D warnings && cargo fmt --check

cd web && npm install && npm run dev      # フロント単体（API は SPINDLE_BACKEND へ中継。既定 127.0.0.1:8080）
cd web && npm run build                   # 同梱用ビルド（tsc -b + vite）
cd web && npx vitest run && npm run lint  # 純粋ロジックのテストと oxlint

docker build -f deploy/Dockerfile -t spindle .
```

外部バイナリ（`ffmpeg` `flac` `opusenc` `cd-paranoia` `cdrdao` `yt-dlp`）はイメージに同梱される。
ローカルで動かすときは PATH に置くか `config.toml` の `[bin]` で指す。CD ドライブとネットワークを
要するテストは `#[ignore]` 付きで、`cargo test -- --ignored` で個別に走らせる。

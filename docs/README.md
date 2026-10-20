# spindle のドキュメント

読む人ごとにディレクトリを分けてある。

| 読む人 | 場所 | 内容 |
|---|---|---|
| 使う人 | [user-guide/](user-guide/README.md) | 画面の操作手順。ログインから一括編集・巻き戻し・取り込み・端末への同期まで |
| 使う人 | [user-guide/dsl.md](user-guide/dsl.md) | スマートプレイリストの文法（リファレンス） |
| 使う人 | [agent/README.md](../agent/README.md) | Mac 用 `spindle-agent`（iPhone への同期） |
| 動かす人 | [admin-guide/install.md](admin-guide/install.md) | 導入。TrueNAS のカスタムアプリの作り方、初期パスワード、設定 |
| 動かす人 | [admin-guide/operations.md](admin-guide/operations.md) | 運用。イメージの更新、バックアップと復元、正規化 |
| 動かす人 | [admin-guide/migration.md](admin-guide/migration.md) | 既存の音楽ライブラリからの移行 |
| 作る人 | [spec/](spec/README.md) | 仕様。データモデル、パイプライン、API、UI、設定、デプロイ |
| 作る人 | [decisions/](decisions/README.md) | 設計判断とその理由（却下した案も含む）。D-1〜 |
| 作る人 | [../CONTRIBUTING.md](../CONTRIBUTING.md) | 開発環境、コマンド、変更の進め方 |
| 作る人 | [../AGENTS.md](../AGENTS.md) | 実装の不変条件・規約・禁止事項（AI エージェントもここから読む） |
| 作る人 | [development/](development/README.md) | 作業中の設計書・計画の置き場と、タスクの履歴 |

## 参照の読み方

コードのコメントや文書の中の参照は、次の場所を指す。

- `SPEC §7.5`、`§12.3` → [spec/](spec/README.md) の目次で同じ番号の章・節
- `D-62` → [decisions/D-062.md](decisions/D-062.md)（番号はゼロ詰め 3 桁）
- `P1-6` などのタスク番号 → [development/tasks-history.md](development/tasks-history.md)

## 旧パスとの対応

2026-10 にドキュメントを再編した。古いコミットやマイグレーション SQL のコメントに残る旧パスは、次の場所に移っている。

| 旧パス | 新しい場所 |
|---|---|
| `docs/SPEC.md` | [spec/](spec/README.md)（§ 番号はそのまま） |
| `docs/DECISIONS.md` | [decisions/](decisions/README.md)（D 番号はそのまま） |
| `docs/TASKS.md` | [development/tasks-history.md](development/tasks-history.md) |
| `docs/USERGUIDE.md` | [user-guide/README.md](user-guide/README.md) |
| `docs/DSL.md` | [user-guide/dsl.md](user-guide/dsl.md) |
| `docs/OPERATIONS.md` | [admin-guide/operations.md](admin-guide/operations.md) |
| `docs/MIGRATION.md` | [admin-guide/migration.md](admin-guide/migration.md) |
| `docs/superpowers/specs/*`、`docs/superpowers/plans/*`（リポジトリに入れていなかった作業用の設計書。`db/migrations/0002_devices.sql` などが参照する） | 要点を [spec/](spec/README.md) と [decisions/](decisions/README.md) に移した。端末への配信の設計（`2026-09-29-device-delivery-design.md`）の ①〜⑥ は [§7.11](spec/07-pipeline/11-device-delivery.md) と [§6](spec/06-data-model.md)「端末への配信の表」、[§12.6](spec/12-ui/06-other-screens.md)「端末」、[D-95](decisions/D-095.md) 以降 |

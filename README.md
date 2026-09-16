# spindle

TrueNAS 上で動作する単一コンテナの音楽ライブラリ管理アプリ。
CD リッピング（AccurateRip / CTDB 照合付き）、メタデータ一括編集、ReplayGain、
プレイリスト管理、簡易再生を提供する。Windows 版 foobar2000 の実用機能を代替する。

- 規模: 300GB / 1〜6 万トラック / 単一ユーザ / LAN 内のみ
- 言語: Rust（バックエンド）+ React/TypeScript（フロント）
- 配布: 単一バイナリ（SPA を `rust-embed` で同梱）+ Docker イメージ

## ドキュメント

| ファイル | 内容 |
|---|---|
| `CLAUDE.md` | **実装時に最初に読む。** 不変条件・規約・禁止事項 |
| `docs/SPEC.md` | 全体仕様。データモデル、パイプライン、API、UI、デプロイ |
| `docs/DECISIONS.md` | 設計判断とその理由。却下案も記録 |
| `docs/TASKS.md` | 実装タスクと受け入れ条件 |
| `docs/DSL.md` | スマートプレイリストの文法と AST/SQL 変換 |
| `docs/MIGRATION.md` | 旧ライブラリからの移行手順 |
| `db/migrations/0001_init.sql` | 初期スキーマ（SQLite 3.46 で適用・FTS 動作を検証済み） |
| `scripts/preflight.py` | 移行前チェック（標準ライブラリのみ） |

## 現在の状態

P0-2（DB 層とマイグレーション）まで完了。`docs/TASKS.md` の P0-3 から続ける。

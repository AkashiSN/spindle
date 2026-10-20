# 開発への参加

spindle は作者が自宅の TrueNAS で使うために作っている個人プロジェクトです。Issue や PR は歓迎しますが、
設計の方針（下記の不変条件）に合わないものは取り込めないことがあります。大きな変更は、先に Issue で相談してください。

## 最初に読むもの

- [AGENTS.md](AGENTS.md): 実装の**不変条件・禁止事項・コーディング規約・よくある実装ミス**。人も AI エージェントもここから読む
- [docs/spec/](docs/spec/README.md): 仕様。変更する機能の章を読む
- [docs/decisions/](docs/decisions/README.md): 設計判断とその理由。記録された判断を覆すときは、理由を添えて先に相談する

## 開発環境

- Rust（stable）、Node.js（`web/` の SPA）
- 外部バイナリ `ffmpeg` `flac` `opusenc` `cd-paranoia` `cdrdao` `yt-dlp` `adb`。Docker イメージには同梱される。
  ローカルで動かすときは PATH に置くか `config.toml` の `[bin]` で指す。無いと該当するテストは skip する
- 本番と同じ ffmpeg で試すなら CI と同じ `rust:1-bookworm` のコンテナを使う

```bash
cargo build && cargo test                   # バックエンド
cargo clippy -- -D warnings && cargo fmt --check

cd web && npm install && npm run dev        # フロント単体（API は SPINDLE_BACKEND へ中継。既定 127.0.0.1:8080）
cd web && npm run build                     # 同梱用ビルド（tsc -b + vite）
cd web && npx vitest run && npm run lint    # 純粋ロジックのテストと oxlint

docker build -f deploy/Dockerfile -t spindle .
```

CD ドライブとネットワークを要するテストは `#[ignore]` 付きで、`cargo test -- --ignored` で個別に走らせる。
Mac 用の `spindle-agent` は `agent/`（[agent/README.md](agent/README.md)）。

## 変更の進め方

1. **設計**: 振る舞いが変わる変更は、先に設計を書く。設計書と実装計画は作業中だけ
   [docs/development/](docs/development/README.md) に置く
2. **テストを先に**: 受け入れ条件をテストにしてから実装する。同一性解決・パス生成・DSL パース・
   AccurateRip CRC のように、壊れても画面上は正常に見える部分は単体テスト必須
3. **ドキュメント**: 実装が済んだら、設計書の要点を [docs/spec/](docs/spec/README.md) へ、判断と理由を
   [docs/decisions/](docs/decisions/README.md)（新しい番号の `D-NNN.md`）へ移し、設計書と計画は消す。
   画面の操作が変わるなら [docs/user-guide/](docs/user-guide/README.md) も直す
4. **PR**: `cargo fmt` / `cargo clippy -- -D warnings` / `cargo test` / フロントのテストと lint を通す。
   CI（[.github/workflows/ci.yml](.github/workflows/ci.yml)）が同じものを走らせる
5. **リリース**: `main` のコミットに `vX.Y.Z-rc.N` を打って本番で確かめ、問題なければ同じコミットに `vX.Y.Z` を打つ。
   CI がイメージと GitHub Release を作る。[CHANGELOG.md](CHANGELOG.md) に版の要点を書く

スキーマ変更は `db/migrations/` に新しい連番ファイルを足す。既存のマイグレーションは書き換えない。

## ドキュメントの置き場所

[docs/README.md](docs/README.md) が地図。使う人向けは `docs/user-guide/`、動かす人向けは `docs/admin-guide/`、
作る人向けは `docs/spec/` と `docs/decisions/`。文章は日本語で書く。

## ライセンス

コントリビューションは [MIT License](LICENSE) で提供されるものとします。

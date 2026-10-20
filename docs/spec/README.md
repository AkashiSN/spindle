# spindle — 設計仕様

> ステータス: ドラフト v0.2 / 主要な設計判断は確定済み
>
> CD を積むスピンドルから。バイナリ名・コマンド名も `spindle`。

仕様は章ごとのファイルに分かれている。§7（パイプライン）と §12（UI）は節ごとに分けてある。
コードやほかの文書の「SPEC §7.5」は、この表の §7.5 の行を指す。設計判断の理由は
[設計判断の記録](../decisions/README.md) を見る。

仕様と実装が食い違ったら、実装を確かめたうえで仕様を直す。新しい判断を伴うなら
[設計判断の記録](../decisions/README.md) にも足す。

## 目次

- [§1. 目的とスコープ](01-scope.md)
- [§2. 用語](02-glossary.md)
- [§3. 基本原則](03-principles.md)
- [§4. アーキテクチャ](04-architecture.md)
- [§5. ライブラリレイアウト](05-library-layout.md)
- [§6. データモデル](06-data-model.md)
- [§7. パイプライン](07-pipeline/README.md)
- [§8. ジョブシステム](08-jobs.md)
- [§9. HTTP API](09-http-api.md)
- [§10. プレイリストとエクスポート](10-playlists-export.md)
- [§11. 再生とトランスコード](11-playback.md)
- [§12. UI](12-ui/README.md)
- [§13. 設定](13-config.md)
- [§14. デプロイ](14-deploy.md)
- [§15. Rust モジュール構成](15-rust-modules.md)
- [§16. 実装順](16-implementation-order.md)
- [§17. 決定済み事項と残課題](17-open-issues.md)

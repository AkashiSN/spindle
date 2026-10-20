# 2. 用語

| 用語 | 定義 |
|---|---|
| Library | 唯一の正となるメディアツリー。1トラック=1ファイル |
| Derived | Library から生成される配布用の非可逆（`opus` 系統と Apple 向け `aac` 系統。[§7.6](07-pipeline/06-derived.md)）。破棄・再生成可能 |
| Archive | アプリが再生成できない生データ（YouTube webm 等）。追記のみ |
| 配布ビュー (delivery) | 可逆なら Derived、非可逆なら Library を指す解決規則 |
| Category | パス最上位階層。統制語彙。GENRE タグとは別概念 |
| master | そのトラックについて手元にある最高品質のファイル（= Library の実体） |
| 端末 (device) | 配信先。Android（ADB 直結）か iPhone（Mac のエージェント経由）の 1 台。系統（`opus` / `aac`）と対象（全曲 / プレイリスト）を持つ（[§7.11](07-pipeline/11-device-delivery.md)、[D-95](../decisions/D-095.md)） |
| マニフェスト（あるべき状態） | 端末に置くべき曲とプレイリストの一覧。保存せず毎回 Library から計算する。端末側の実態が正（[D-95](../decisions/D-095.md)） |
| 写し（端末上の曲） | 端末（Mac の `~/Music/spindle` を含む）に置いた曲の複製。Library から再生成できるので、物理削除してよい（[D-95](../decisions/D-095.md)） |
| 差分 | マニフェストと端末の実態の違い。曲ごとに add / update / move / update_move / delete に分け、確認してから送る（[§7.11](07-pipeline/11-device-delivery.md)） |
| 計画 | 差分から作る、端末へ反映する操作の列。差分を見てからボタンで実行する（自動では送らない。[D-95](../decisions/D-095.md)） |
| エージェント (spindle-agent) | Mac で動く常駐しない CLI。spindle から計画を受け取り、ミュージック.app へ反映して結果を報告する（[§7.11](07-pipeline/11-device-delivery.md)、[D-95](../decisions/D-095.md)） |

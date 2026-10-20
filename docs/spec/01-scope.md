# 1. 目的とスコープ

TrueNAS 上で動作する単一コンテナのWebアプリケーション。CDリッピング、メディアライブラリ管理、
メタデータ一括編集、ReplayGain、プレイリスト管理、簡易再生、端末への配信を提供する。
端末への配信は、Android へは USB の ADB で直接、iPhone へは Mac の `spindle-agent` が
ミュージック.app へ反映する（[D-95](../decisions/D-095.md)。[§7.11](07-pipeline/11-device-delivery.md)）。
Windows版 foobar2000 の実用機能を代替し、既存CLIツール `ytmusic` を統合する。

## 非目標

- 高度な再生機能（ギャップレス、DSP チェーン、クロスフェード、ASIO/WASAPI 相当）
- マルチユーザ、権限管理、公開インターネットへの露出
- モバイルアプリ（ブラウザで足りる）
- 音楽配信サーバ（Subsonic API 互換等）— **非対応で確定。**
- DSD / SACD ISO の取り扱い

## 規模想定

- 総容量 300GB、1〜6万トラック
- 単一ユーザ、LAN 内アクセスのみ
- 常時稼働（NAS）

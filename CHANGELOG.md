# 変更履歴

リリースごとの主な変更。形式は [Keep a Changelog](https://keepachangelog.com/ja/1.1.0/) に倣い、版は
[Semantic Versioning](https://semver.org/lang/ja/) に従う（1.0 までは minor で機能を足す）。リリース候補（`-rc.N`）は載せない。
各版のイメージと差分は [GitHub Releases](https://github.com/AkashiSN/spindle/releases) にある。括弧の D-n は
[設計判断](docs/decisions/README.md)、P-n は [タスクの履歴](docs/development/tasks-history.md) の番号。

## [Unreleased]

### Changed

- ドキュメントを読者別（`docs/user-guide/` `docs/admin-guide/` `docs/spec/` `docs/decisions/`）に再編し、
  仕様は章ごと、設計判断は 1 判断 1 ファイルに分けた。README・CONTRIBUTING・CHANGELOG・LICENSE を足した

## [0.6.0] - 2026-10-19

### Added

- スマホ用の画面 `/m/`。ライブラリを眺める・探す・試し聴きする・プレイリストを作る・YouTube の購読を同期する（D-115）

## [0.5.0] - 2026-10-18

### Added

- Inbox の YouTube 由来の件を、メタデータプラグインで再判定する。判定したプラグインの版を記録する（D-114）

### Fixed

- adb サイドカーの鍵の待ちで tokio のワーカーを塞がない

## [0.4.4] - 2026-10-17

### Added

- Android の同期の進捗を端末タブに出す（D-109）
- MP3 の写像できないキーを ID3v2 の TXXX に読み書きする（D-110）
- Inbox の承認に種類と除外の列、タグのキーに前方一致の候補（D-113）

### Changed

- Derived のジョブは行った処理を結果列に出す（D-111）
- Derived のバッジを O（opus）と A（aac）に分ける（D-112）
- スマートプレイリストのルールの編集中は position のソートを既定にする

## [0.4.3] - 2026-10-16

### Added

- `spindle-agent sync` の進捗を Mac の端末と端末タブに出す（D-108）
- プレイリストの行に同期の状況を出す

### Fixed

- Mac の同期中に「同期が途中です」と強制破棄を出さない

## [0.4.2] - 2026-10-15

### Fixed

- 選曲の見積もりで、送る元のハッシュが無い曲を推定して容量に足す（D-107）
- SSE の応答に `X-Accel-Buffering: no` を付け、nginx 越しでもイベントが届くようにする

## [0.4.1] - 2026-10-15

### Added

- リリース候補（`vX.Y.Z-rc.N`）のイメージを出せるようにする

### Fixed

- 記録画面をスクロールできるようにする

## [0.4.0] - 2026-10-15

### Added

- 「記録」画面（編集履歴・退避ファイル・ログ）と、ジョブの結果の表示

## [0.3.1] - 2026-10-14

### Fixed

- CTDB の 404 を登録なしとして扱う

## [0.3.0] - 2026-10-14

### Added

- DB を失った後に、Android の保存先を新しい登録へ引き継ぐ（P5-7）
- DB を失った後に、Mac の保存先を引き継ぐ `spindle-agent pair --adopt`（P5-8）
- 印のついたプレイリストの削除の規則と、終端した同期の計画の GC（P5-6）

### Fixed

- GC が `tracks` の最大 id の行を消さない（id の再利用を防ぐ）

## [0.2.0] - 2026-10-02

### Added

- 端末への配信（D-95）。端末タブで端末ごとに選曲し、Android へは USB の ADB で直接（adb サイドカー）、
  iPhone へは Mac の `spindle-agent` がミュージック.app を経由して同期する（P5-1〜P5-5）
- ReplayGain を解析後に自動でタグへ書き、Derived はその後に作る
- 遡及照合の結果を操作タブとプロパティに出す

## [0.1.3] - 2026-09-26

### Fixed

- Inbox で配置済みの件が「手置き」「変更 N 件」と出るのを直す

## [0.1.2] - 2026-09-26

### Added

- CD の取り込みで品番（と JAN）を手で入れて版を識別し、照会とタグに使う（D-94）

### Fixed

- compose に `init: true` を足し、取り消しで kill した孫プロセスのゾンビを回収させる

## [0.1.1] - 2026-09-26

### Fixed

- ALAC のプロセス内デコードの範囲を ffmpeg の edit list の扱いに揃える（D-89）
- リバースプロキシ越しでのログインの CSRF 判定（`trusted_proxies`）を運用手順に書く

## [0.1.0] - 2026-09-26

最初のリリース。P0〜P4 の主要機能。

### Added

- ライブラリの走査と同一性解決（`(dev, inode)` → `audio_md5` → `rel_path`）、論理削除と GC（P0、P1-11）
- foobar2000 風の 3 ペイン UI、トラック一覧の絞り込み・並べ替え、一括タグ編集・リネーム・バッチ単位の巻き戻し（P0）
- ReplayGain の解析と書き込み、ロスレスの FLAC 正規化、FLAC 健全性チェック（P1）
- Derived（Opus / AAC）の生成と追随、Range 再生（P1-9、P1-10）
- 手動・スマートプレイリスト（foobar2000 風の DSL）、m3u8 の書き出し・取り込み（P1-6〜P1-8）
- CD 取り込み: TOC と DiscID、MusicBrainz 照会、吸い出し、AccurateRip / CTDB 照合、遡及照合、Inbox 経由の配置（P2）
- YouTube（ytmusic）統合: 再生リストの購読と同期、タイトル解析、Inbox への取り込み（P3、P4）
- 認証（argon2id、CSRF 検査）、DB の自動バックアップ、GHCR へのイメージ配布（P0、P4-12）

[Unreleased]: https://github.com/AkashiSN/spindle/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/AkashiSN/spindle/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/AkashiSN/spindle/compare/v0.4.4...v0.5.0
[0.4.4]: https://github.com/AkashiSN/spindle/compare/v0.4.3...v0.4.4
[0.4.3]: https://github.com/AkashiSN/spindle/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/AkashiSN/spindle/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/AkashiSN/spindle/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/AkashiSN/spindle/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/AkashiSN/spindle/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/AkashiSN/spindle/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/AkashiSN/spindle/compare/v0.1.3...v0.2.0
[0.1.3]: https://github.com/AkashiSN/spindle/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/AkashiSN/spindle/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/AkashiSN/spindle/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/AkashiSN/spindle/releases/tag/v0.1.0

# spindle

TrueNAS 上で動く、自宅向けの音楽ライブラリ管理アプリ。Windows 版 foobar2000 でやっていたことを、
ブラウザから NAS の上で行えるようにする。本体は 1 コンテナで、Android へ同期するなら adb のサイドカーを足す。

- **CD の取り込み**: 吸い出した音声を AccurateRip / CTDB と照合し、正確に読めたことを確かめてから FLAC で保管する。
  照合ログの無い既存の FLAC も後から照合できる（遡及照合）
- **メタデータの一括編集**: 数千曲のタグ編集・リネームをプレビューしてから適用し、**バッチ単位で巻き戻せる**
- **ReplayGain**: EBU R128 で解析してタグに書く（Opus は R128 形式）
- **プレイリスト**: 手動のものと、foobar2000 風のクエリで書くスマートプレイリスト。m3u8 で書き出す
- **配布用の変換**: 原本から Opus / AAC の配布用ファイルを作り、タグの変更や移動に追随させる
- **YouTube の取り込み**: 再生リストを購読し、新しい曲を承認キュー（Inbox）経由でライブラリに入れる
- **端末への同期**: 端末ごとに選んだ曲とプレイリストを、Android へは USB の ADB で直接、iPhone へは Mac の
  ミュージック.app を経由して同期する
- **再生**: ブラウザで試し聴きする。外出先向けにスマホ用の画面（`/m/`）もある

## 設計の考え方

- **ファイルが正、DB はキャッシュ。** DB を消してもファイルから作り直せる。SMB 越しに foobar2000 などが
  ファイルを書き換えても、spindle はファイルの内容を採る
- **パスは識別子ではない。** リネームは日常の操作なので、曲の同一性は inode と音声のハッシュで追う
- **ライブラリへの破壊的な操作は巻き戻せる。** タグ書き込み・リネーム・削除は旧値を記録してから行う。削除は論理削除で、
  実際に消すのは猶予期間（既定 7 日）の後（消した後は戻せない）。端末上の写しは Library から作り直せる複製なので対象外
- **非可逆から非可逆へ変換しない。** 配布用のファイルは可逆の原本から作り、非可逆の原本はそのまま配る。例外は
  Opus を読めない Apple の機器向けの AAC で、これだけは非可逆の原本からも変換する

設計の背景と却下した案は [設計判断の記録](docs/decisions/README.md) に 100 件以上残してある。

## 現状

v0.6.0（2026-10）。作者の自宅の TrueNAS で、約 9,000 曲のライブラリを実際に運用している。

- 想定: 単一ユーザ、LAN 内（外からはリバースプロキシ越し）、300 GB・1〜6 万曲
- 言語: Rust（バックエンド）+ React / TypeScript（フロント）。SPA を同梱した単一バイナリと Docker イメージで配る
- 非対応: マルチユーザ、Subsonic API などの配信サーバ、ギャップレスや DSP などの高度な再生、DSD

変更の履歴は [CHANGELOG.md](CHANGELOG.md)。

## 動かす

```bash
docker pull ghcr.io/akashisn/spindle:latest
```

[`deploy/compose.yaml`](deploy/compose.yaml) を TrueNAS のカスタムアプリとして登録して起動する。データセットの作り方、
初期パスワード、設定は [導入](docs/admin-guide/install.md) にある。起動後の使い方は [操作手順](docs/user-guide/README.md)。

## ドキュメント

| 読む人 | 文書 |
|---|---|
| 使う人 | [操作手順](docs/user-guide/README.md)、[スマートプレイリストの文法](docs/user-guide/dsl.md)、[Mac 用 spindle-agent](agent/README.md) |
| 動かす人 | [導入](docs/admin-guide/install.md)、[運用手順](docs/admin-guide/operations.md)、[移行手順](docs/admin-guide/migration.md) |
| 作る人 | [仕様](docs/spec/README.md)、[設計判断](docs/decisions/README.md)、[開発への参加](CONTRIBUTING.md)、[AGENTS.md](AGENTS.md) |

全体の地図は [docs/README.md](docs/README.md)。

## ライセンス

[MIT](LICENSE)

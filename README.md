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
| `docs/OPERATIONS.md` | 運用手順。バックアップと復元 |
| `db/migrations/0001_init.sql` | 初期スキーマ（SQLite 3.46 で適用・FTS 動作を検証済み） |
| `scripts/preflight.py` | 移行前チェック（標準ライブラリのみ） |
| `scripts/migrate_plan.py` | 移行の振り分け計画。`rsync --files-from` の一覧を生成（標準ライブラリのみ） |

## 現在の状態

P0 完了（P0-14 で実機へ移行のリハーサル済み。9,098 トラック。`ssd/musics` が引き続き正で、リリース時に再移行する）。`docs/TASKS.md` の P1 から続ける。移行中に見つかった後続課題は TASKS の P0-14 末尾。

## 起動

`SPINDLE_CONFIG`（既定 `/data/config.toml`）で設定を指す。初回起動では環境変数
`SPINDLE_INITIAL_PASSWORD` を読んで argon2id で DB に保存し、以後は無視する。
環境変数も DB もパスワードが無いとロックモード（`/health` が `{"status":"locked"}`、
それ以外は 503）で起動する。**初期化が済んだら compose から `SPINDLE_INITIAL_PASSWORD` の
行を消してよい**（平文を残さないため。消しても DB のパスワードで起動する）。

## YouTube の取り込み

上部バーの「YouTube」画面に動画か再生リストの URL を 1 行 1 つ貼ると、音声を Inbox に置く
（承認は Inbox）。再生リストは動画ごとに展開し、取り込み済み（`SOURCE_URL` が一致）の動画は飛ばす
ので、同じ再生リストを何度貼っても新しいものだけが落ちる。

URL を貼る手間を減らすブックマークレット（ブックマークの URL 欄に貼る。`<spindle>` は
`http://truenas:8080` のような spindle の URL）:

- **この動画を spindle へ**: いま見ているページの URL を spindle の YouTube 画面に入れて開く
  ```
  javascript:void(open('http://<spindle>/youtube?url='+encodeURIComponent(location.href)))
  ```
- **このページの動画リンクを全部集める**: チャンネルの動画一覧・検索結果・再生リストのページで
  動画の URL を集め、改行区切りでクリップボードへ（spindle の欄に貼る）
  ```
  javascript:(()=>{const s=new Set([...document.querySelectorAll('a[href*="/watch?v="]')].map(a=>new URL(a.href).searchParams.get('v')).filter(Boolean));navigator.clipboard.writeText([...s].map(v=>'https://www.youtube.com/watch?v='+v).join('\n')).then(()=>alert(s.size+' 件をコピーした'))})()
  ```

YouTube に弾かれるようになったら、まず yt-dlp を新しくする（イメージの更新）。それでも駄目なら
`config.toml` の `[ytmusic].ytdlp_args` に `--extractor-args` や `--cookies` を渡す（`sh -c` は使わない
ので配列。UA / Referer は付けない）。


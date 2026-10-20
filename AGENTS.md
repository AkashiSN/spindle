# CLAUDE.md

spindle の実装作業を始める前に、このファイルを最初に読むこと。

## このプロジェクトは何か

TrueNAS 上で動く単一コンテナの音楽ライブラリ管理アプリ。CD リッピング、
メタデータ一括編集、ReplayGain、プレイリスト、簡易再生、端末（Android / iPhone）への配信。
P0〜P5 は実装済みで、v0.6.0 を本番で運用している。ドキュメントの地図は `docs/README.md`。

- 仕様: `docs/spec/`（章ごと。`SPEC §7.5` は `docs/spec/README.md` の目次で引く）
- 判断の理由: `docs/decisions/`（`D-62` は `docs/decisions/D-062.md`）
- 過去のタスクと受け入れ条件: `docs/development/tasks-history.md`（履歴。新しいタスクは書かない）

読む順序: 依頼に関係する `docs/spec/` の章・節 → そこから参照される `docs/decisions/` で背景を確認。

## 絶対に守る不変条件

これを破る実装は、動いていても差し戻す。

1. **ファイルが正、DB はキャッシュ。**
   DB を消してもファイルから再構築できること。外部（foobar2000 等）が
   ファイルを書き換えていたら、DB ではなくファイルの内容を採用する。
2. **パスは識別子ではない。**
   同一性の解決は `(dev, inode)` → `audio_md5` → `rel_path` の順。
   リネームは日常操作であり、パスを主キー扱いする実装は必ず壊れる。
3. **音声とタグは別に版管理する。**
   タグだけの変更で `audio_version` を上げてはならない。上げると数千件の
   一括編集のたびに Derived の再エンコードが走り、数時間失われる。
4. **破壊的操作はバッチ単位で巻き戻せること。**
   タグ書き込み・リネーム・削除は必ず `edit_batches` / `edits` に旧値を
   記録してから実行する。
5. **ファイルを排他ロックしない。**
   SMB 経由で他プレイヤーが同じファイルを触る。ロックではなく再スキャンで調停する。
6. **ジョブは冪等。**
   コンテナ再起動・電源断・キャンセルから安全に再開できること。
   起動時に `running` を `queued` へ戻すリカバリを必ず通す。

## 禁止事項

- **ユーザデータの物理削除。** 削除は `missing_since` による論理削除。
  物理削除は GC ジョブ（既定 7 日経過後。`[gc].retention_days`）のみが行う。
  - 明示の例外: 端末上の写しと Mac の `~/Music/spindle` は Library から
    再生成できる複製なので、これに当たらない（消えても次の同期で戻る）。端末への
    書き込みは `edit_batches` に載せない（巻き戻しは Library を巻き戻して同期し直す）。
    D-95。
- **生 SQL 文字列の組み立て。** スマートプレイリストを含め、値は必ず
  バインドパラメータ。列名はホワイトリスト経由でのみ解決する。
- **圧縮レベルを揃えるための FLAC 一括再エンコード。** 削減は 1% 未満で、
  200GB の書き直しとスナップショット肥大に見合わない。
- **非可逆 → 非可逆の再エンコード。** 世代劣化する。配布ビューは原本を返す。
  - 明示の例外: Apple 向けの `aac` 系統だけは非可逆の原本も AAC に変換する（`lossy_sources`。
    ミュージック.app は Opus を読めない）。D-75。
- **仕様にない判断を黙って入れること。** 迷ったら実装を止めて質問する。
  決めた場合は `docs/decisions/` に追記案を出す。
- **`db/migrations/*.sql` の既存ファイルの書き換え。** スキーマ変更は
  必ず新しい連番ファイルを追加する。

## 技術スタック

| 用途 | クレート | 注意 |
|---|---|---|
| HTTP | `axum`, `tokio`, `tower-http` | |
| DB | `rusqlite`（bundled 機能）| 書き込みは単一コネクション、読みはプール。`spawn_blocking` 必須 |
| タグ | `lofty` | FLAC / Opus / MP4 / WAV を一貫 API で読み書き |
| デコード | `symphonia` | **Opus 非対応。** Opus は ffmpeg 経由 |
| ラウドネス | `ebur128` | libebur128 の純 Rust 移植 |
| 正規表現 | `fancy-regex` | ytmusic パーサに後方参照・先読みがある。`regex` では不可 |
| 文法 | `pest` | プレイリスト DSL |
| HTTP client | `reqwest` | MusicBrainz は UA 必須・1req/s |
| 同梱 | `rust-embed` | SPA |
| エラー | `anyhow`（アプリ）/ `thiserror`（ライブラリ層） | |
| ログ | `tracing`, `tracing-subscriber` | |
| FFT | `rustfft` | 偽ハイレゾ検出（P3、任意） |

バージョンは `cargo add` で解決すること。この表に固定値は書かない。

外部バイナリ: `ffmpeg` `flac` `opusenc` `cd-paranoia` `cdrdao` `yt-dlp` `adb`。
いずれも `std::process::Command` で呼ぶ。パスは設定で上書き可能にする。

## コーディング規約

- `cargo fmt` / `cargo clippy -- -D warnings` が通ること
- 非テストコードで `unwrap()` / `expect()` / `panic!()` を使わない。
  唯一の例外は起動時の設定検証（そこで落ちるのは正しい）
- 外部プロセスの呼び出しは必ずタイムアウトと終了コード検査を伴う。
  stderr は握り潰さずログに出す。`sh -c` は使わず引数配列で起動し、パスは `--` の後
  （非対応ツールなら `./` を前置）に置く
- パスは `Path` / `PathBuf` で扱う。`String` にしてから結合しない。
  ファイルは root の dirfd から `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` で開き、
  パス文字列を結合して `File::open` しない（SPEC §5「パスの表現と境界」）
- パスの比較・一意性判定は `rel_path_key`（casefold + NFD）で行う。`rel_path` の
  文字列比較は ZFS の insensitive + formD と一致しない
- 時刻は UNIX epoch 秒（`i64`）で統一。DB もそれに合わせている
- ユーザ向け文字列は日本語、ログとコードコメントも日本語で良い

## テスト方針

- **同一性解決・パス生成・DSL パース・AccurateRip CRC は単体テスト必須。**
  これらは壊れても画面上は正常に見えるため、テストがないと発見できない
- ytmusic パーサは `tests/fixtures/ytmusic_titles.json`（83 件）を
  Python 版と共有する。移植中の差分検出に使う
- DB を触るテストは `:memory:` に `db/migrations/*.sql` を流して構築する
- CD ドライブ・ネットワークを必要とするテストは `#[ignore]` を付け、
  CI では走らせない

## よくある実装ミス

- タグ書き込みで tmp + rename した後、DB の `(dev, inode)` を更新し忘れる。
  次回スキャンで全件が「新規トラック」として重複登録される
- Opus の ReplayGain を -18 LUFS 基準で書いてしまう。**Opus のみ -23 LUFS 基準**の
  Q7.8 固定小数（`R128_*`）。内部表現は -18 LUFS で統一し、書き出し時に変換する
- `rel_path` の UNIQUE 制約を一括リネームで踏む。一時パス経由の 2 段階更新にする
- FTS5 の trigram は 3 文字未満を引けない。短い検索語は LIKE にフォールバック
- album gain を 2ch 以外を含めて計算する。マルチチャンネルは集計から除外する
- FTS5 の external content で `tracks` に無い列を索引に入れる。列値取得と `rebuild` が
  `no such column` で落ちる。索引列はすべて `tracks` の実列にする
- `PRAGMA journal_mode` / `synchronous` をマイグレーション SQL に書く。
  トランザクション内では変更できない。コネクション初期化で設定する
- 同一トラックの複数フィールドをフィールドごとに tmp + rename する。1 件目で inode が
  変わり 2 件目以降の事前条件が外れる。反映単位はトラック（`edit_ops`）
- `audio_md5` 一致を無条件に「移動」と判定する。コピー元が残っている・重複収録で
  別トラックを黙ってマージしてしまう。候補が 1 行かつ移動元が消えている場合だけ移動。
  しかも「消えている」は走査途中には判定できない。inventory を全部取ってから判定する
- CSRF 検証で `Origin || Host` と書く。クロスサイト POST でも Host は送信先なので通る。
  `Origin` があれば完全一致、無ければ `Sec-Fetch-Site`。Host は使わない
- `dev` を安定した識別子として扱う。ZFS の dev 番号はホスト再起動で変わる。同一性解決は
  inode + size + mtime + ctime が揃えば dev 違いを「変更なし」とし、編集の事前条件と行と FD の
  照合は dev を見ない（D-62）
- 非可逆ファイルの `size` 変化を音声の変化とみなす。タグ書き換えでコンテナサイズは
  普通に変わる。音声版はパケット列のハッシュ（`audio_fp`）で判定する
- テストで `jobs` 行を直接 `running` にしてワーカーを動かす。実行中表に無い `running` は
  稼働中の回収で `queued` に戻される（D-76）。ロック保持の模擬は本物のハンドラで握るか、
  ワーカーを止めてから行う

## コマンド

```bash
cargo build && cargo test
cargo clippy -- -D warnings
cargo fmt --check

cd web && npm install && npm run dev      # フロント単体（API は SPINDLE_BACKEND へ中継。既定 127.0.0.1:8080）
cd web && npm run build                   # 同梱用ビルド（tsc -b + vite）
cd web && npx vitest run && npm run lint  # 純粋ロジックのテストと oxlint

python3 scripts/preflight.py <旧ライブラリ> --dest <コピー先>       # 移行前チェック（docs/admin-guide/migration.md）
python3 scripts/migrate_plan.py <旧ライブラリ> --out /tmp/plan     # 振り分け一覧
docker build -f deploy/Dockerfile -t spindle .
```

## 作業の進め方

1. 設計を書き、ユーザの了承を得る。設計書と実装計画は**作業中だけ** `docs/development/` に置く
   （`YYYY-MM-DD-<topic>-design.md` / `YYYY-MM-DD-<topic>.md`）。superpowers などのスキルが既定で
   書き出す `docs/superpowers/` には置かない
2. 受け入れ条件を満たすテストを先に書く
3. 実装し、`cargo clippy -- -D warnings` と `cargo test`（フロントは `npx vitest run` と `npm run lint`）を通す
4. 設計書の要点を `docs/spec/` の該当する章へ、判断とその理由を `docs/decisions/` へ移し
   （新しい判断は次の番号の `D-NNN.md` を作り、`docs/decisions/README.md` の一覧に足す）、
   設計書と計画を `docs/development/` から消す。利用者の操作が変わるなら `docs/user-guide/` も直す
5. レビュー（codex）→ PR → CI。リリースは `vX.Y.Z-rc.N` を本番で確かめてから `vX.Y.Z`
   （`docs/admin-guide/operations.md`「リリース候補（RC）を本番で確かめてから出す」）。`CHANGELOG.md` に版の要点を書く

依頼の範囲を超えて実装を先回りしない。

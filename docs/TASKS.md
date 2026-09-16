# 実装タスク

各タスクは独立して着手でき、受け入れ条件を満たせば完了とする。
上から順に実施する。依存が明記されているものはそれを先に終わらせる。

---

## P0 — ライブラリ基盤

完了条件: **既存ライブラリの全曲が表に出て、一括編集と巻き戻しができる。**

順序の意図: 認証とジョブ基盤を先に置く。破壊的 API は認証のない状態で一度も
存在させない。スキャナは進捗 SSE とリカバリを前提にするので、ジョブ基盤の後。
タグ編集・リネーム・論理削除は同じ編集履歴機構（`edit_batches` / `edits`）に乗せ、
巻き戻しは 3 種すべてを対象にする。

### P0-1 プロジェクト初期化

- [x] Cargo ワークスペース、`web/` に Vite + React + TypeScript
- [x] `tracing` によるログ、`config.toml` の読み込みと検証
- [x] `axum` でヘルスチェックを返す
- [x] `db/migrations/` をバイナリへ埋め込む（Dockerfile の COPY 範囲に含める）
- [x] `cargo clippy -- -D warnings` が通る CI

受け入れ: `docker build` が通り、コンテナが起動して `/health` が 200 を返す。

### P0-2 DB 層とマイグレーション

- [x] `db/migrations/*.sql` を連番で適用する仕組み、`schema_version` 管理。
      **1 ファイル = 1 トランザクション**で適用する
- [x] コネクション初期化で `journal_mode=WAL` / `synchronous=NORMAL` / `foreign_keys=ON` を
      **トランザクション外**で設定する（SQL ファイルには PRAGMA を書かない）
- [x] 書き込み単一コネクション + 読み取りプール、`spawn_blocking` でのラップ
- [x] `:memory:` にマイグレーションを流すテストヘルパ
- [x] FTS5 の挙動テスト: insert / 索引列 update / delete / `albums.album` 改名 →
      `tracks.album` 同期 → 旧語で引けず新語で引ける / `rebuild` / `integrity-check`

受け入れ: `0001_init.sql` が**トランザクション内で**適用でき、再起動しても二重適用されない。
適用後のコネクションで `PRAGMA journal_mode` が `wal`、`foreign_keys` が 1 を返す。
FTS の列値取得（`SELECT title, album FROM tracks_fts WHERE ... MATCH`）が通る。
CHECK 制約で不正な列挙値（`state='bogus'` 等）が拒否される。

依存: P0-1

### P0-3 認証

- [x] `SPINDLE_INITIAL_PASSWORD` を初回起動時のみ読んで argon2id で DB へ。無ければロックモード
- [x] セッション Cookie（HttpOnly / SameSite=Lax）、トークンは DB にハッシュ保存、期限切れ掃除
- [x] 変更系リクエストの CSRF 検証: `Origin` があれば完全一致必須、無ければ `Sec-Fetch-Site`。
      **`Host` は使わない。** ログイン失敗の IP 単位レート制限
- [x] `trusted_cidrs` は route allowlist（stream / artwork / tracks/:id / playlist export）のみ
      認証スキップ。判定は socket アドレス、`X-Forwarded-*` は `trusted_proxies` からのみ
- [x] ロックモード: SPA 配信を含め `/health` 以外 503、`/health` は `{"status":"locked"}`
- [x] `/health` 以外の全ルート（SSE / stream / artwork 含む）を同じミドルウェア配下に置く

受け入れ: 未認証で API を叩くと 401。CORS プリフライトが通らない。
trusted CIDR から stream は通り、一覧 GET と POST は 401。
Origin が別サイトの POST はセッション付きでも 403。Origin なし・`Sec-Fetch-Site: cross-site` も 403。
Host だけが一致する（Origin が別）POST が 403 になることをテストで固定する。
環境変数も DB もパスワードが無い起動で `/health` 以外（SPA 含む）が 503。

依存: P0-2

### P0-4 ジョブシステム

- [x] キュー、型別の並列度、`dedup_key` による二重投入防止
      （`queued` / `running` の間だけ一意。`done` 後は同キーを再投入できる）
- [x] 起動時リカバリ: `running` → `queued`、`track_locks` 全削除
- [x] 指数バックオフ（`run_after` に永続化）、`max_attempts` 超過で `failed`
- [x] 協調キャンセル（`cancel_requested_at`）、外部プロセスの子グループ kill と tmp 掃除
- [x] 進捗の SSE 配信と DB 永続化
- [x] `GET /api/jobs`（`summary` 込み）、`POST /api/jobs/:id/cancel` / `retry`。
      ジョブ画面（SPEC §12.5）は P0-8 の骨格に載せる
- [x] `track_locks` による同一トラックの直列化。複数ロックは `track_id` 昇順取得、
      取れなければ全解放して再キュー
- [x] 版付きジョブの stale 判定（payload の版 < 現在値なら no-op で `done`）

受け入れ: 実行中にプロセスを kill → 再起動で中断ジョブが再開され、ロック表が空になる。
同じジョブを二重投入しても 1 つしか走らない。完了後に同キーで再投入できる。
失敗ジョブの次回実行時刻が再起動を跨いで保たれる。

依存: P0-2

### P0-5 同一性解決とパス安全層

- [x] 相対パスの検証（絶対 / `..` / 空 / NUL / `\\` を拒否）と canonical key `casefold(NFD())`
- [x] root dirfd 基準の open / rename / tmp 作成（`rustix` `openat2` with
      `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`、作成は `O_EXCL`、rename は `RENAME_NOREPLACE`）。
      全ファイル操作はこの層を通す。`openat2` 不可なら起動時に失敗
- [x] 外部コマンド起動の共通ラッパ（引数配列、`--` / `./` 前置、タイムアウト、終了コード、
      stderr ログ、プロセスグループ kill）
- [x] `(dev, inode)` → `audio_md5` → `rel_path_key` の順の解決関数。**inventory 全体を入力に取り**、
      各段で候補を検証（inventory 内で inode が一意、未 claim、`nlink = 1`、size/mtime の一致、
      md5 候補は 1 行かつ旧 key が inventory に無い）。取り合いは `rel_path_key` 昇順
- [x] FLAC の STREAMINFO から PCM MD5 を読む（デコード不要）
- [x] ALAC / WAV は `symphonia` でデコードして算出
- [x] 非可逆の `audio_fp`（エンコード済みパケット列の SHA-256。Opus を含め symphonia で demux、
      デコードも外部プロセスも使わない。D-37）
- [x] `tag_hash`（正規化タグ集合の SHA-256）の算出

受け入れ: 単体テストで以下が通ること。
(a) タグ書き換えでも同一と判定 (b) 別ディレクトリへ移動しても同一と判定
(c) MD5 未設定の FLAC を「MD5 なし」として扱い、落ちない
(d) コピー元が残っている場合は移動でなく新規 + `duplicate_groups` に現れる
(e) 同一 md5 の候補が 2 行あるとき自動マージしない
(f) inode 再利用（削除後に別内容のファイルが同じ inode）を別トラックと判定
(g) hardlink（`nlink > 1`）は inode 段を飛ばす
(h) 2 ファイルのパス入れ替え（swap rename）と 3 ファイルの循環が正しく追随する
(i) `../x`、`/abs`、symlink 経由の open が拒否される。`-x.flac` を外部コマンドへ安全に渡せる
(j) `B/x.flac` と `b/x.flac`（大小文字）、`が` の NFC と NFD が同じ key になる。
    `Ｂ`（全角）は `B` と**別の** key になる（NFKC はしない）
(k) 訪問順を変えた inventory（コピー先を先に stat）でも判定結果が同じ
(l) 非可逆ファイルのタグだけを書き換えて size が変わっても `audio_fp` が同じ
(m) 対象 TrueNAS 上で corpus（大小文字・NFC/NFD・ß・トルコ語 I）を作り、`O_EXCL` の結果と
    key の同値判定の差を記録する（差があれば D-31 の限界として文書化。`#[ignore]`、
    `tests/zfs_corpus.rs`。**移行前（P0-14）に対象 NAS で一度実行して結果を D-37 に追記する**）

依存: P0-2

### P0-6 スキャナ

- [x] **4 相スキャン**（SPEC §7.1）: inventory 固定 → 候補生成 → 決定的 claim →
      単一トランザクション commit。`scan_runs` 行と `tracks.seen_run_id` で claim を管理
- [x] `Library/` の再帰走査（symlink は辿らず一覧に出す。SMB 禁止名は対象外として一覧に出す）。
      `(dev, inode, size, mtime_ns, ctime_ns)` 変化なしなら `seen_at` / `seen_run_id` 更新のみ
- [x] タグ読み取り（`lofty`）、`track_tags` への格納、キャッシュ列（`album` 含む）の更新。
      **版は `tag_hash` / 音声フィンガープリントの実差分があるときだけ進める**（SPEC §6 遷移表）
- [x] path / key の更新は予約済み一時 key 経由の 2 段階（swap / 循環）
- [x] `missing_since` は run が `completed` の finalize でだけ立てる。再発見した行は復活
- [x] アルバム（= ディレクトリ）の再構成。**ディレクトリ rename で album id を維持**
      （MBID / DiscID が 1 件一致 → 構成トラック過半数の順。候補複数なら寄せない。SPEC §7.1）、
      構成 0 の album は `missing_since`。`categories` の自動推定
- [x] deep scan（全件の `tag_hash` と可逆の `audio_md5` を再計算）。`[scan].deep_interval_days` と手動
- [x] 取り残された `.spindle-tmp-*` の回収
- [x] 走査で見つからない行に `missing_since` を立てる
- [x] **`edit_ops.result = 'pending'` のあるトラックは、その op が所有する論理フィールドを
      再評価しない。** 物理的な所在（`rel_path` / `size` / `mtime_ns`）は pending 中も追随する
- [x] 並列度 = CPU コア数。進捗を SSE で配信
- [x] FTS5 の同期（トリガ経由。`seen_at` 更新で FTS 書き込みが走らないこと）

受け入れ: 1 万件規模の合成ツリーで完走し、2 回目の走査が 1 回目より大幅に速い。
ファイルを外部で移動 → 再走査で重複が増えないこと。
アルバムディレクトリを外部で rename → 同じ `album_id` が維持され、2 ディレクトリの swap でも壊れない。
mtime を保存して上書き（`touch -r`）したファイルの変更を検出する。
同じタグを再保存したファイルで `tag_version` が動かない。
走査を途中で kill（`failed`）しても `missing_since` が立たない。
missing だった行を同じパスに戻すと復活する。
性能の測定条件を固定する: 合成ツリー 1 万件（FLAC 8 割 / Opus 2 割、アルバム 12 曲）、
2 回目は warm cache、対象 NAS または CI の参照マシンを明記
（`tests/scanner.rs` の `perf_10k_synthetic_tree`（`#[ignore]`、`--release`）。結果は D-38）。

依存: P0-4, P0-5

### P0-7 トラック一覧 API

- [x] `GET /api/tracks` カーソルページング（キーセット）、ソート（ホワイトリスト）、フィルタ
      （JSON、ホワイトリスト。D-39）。SPEC §9 のレスポンス形
      （`pending_batch_id` / `conflict_batch_id` / `duplicate_group` / `hardlink` / `derived` を
      1 クエリで返す。pending / 最新 op は LEFT JOIN、重複は相関 EXISTS）
- [x] `selection` の 2 形（`ids` / `filter + exclude_ids`）を解決する共通関数
      （`db::tracks::resolve_selection`）
- [x] preview で selection をスナップショットし `selection_token`（TTL 15 分）を返す。
      apply は token の集合だけを対象にする（D-33）。`api::selection::snapshot` / `lookup`、
      `SelectionStore`（プロセス内、上限 16 件）。エンドポイント本体は P0-10 で載せる
- [x] バッジ用の JOIN（最新 op、重複）を `EXPLAIN QUERY PLAN` で確認し、
      temp B-tree が出ないことを固定（`tests/tracks_query.rs`。無フィルタの全ソートキー × 昇降 ×
      1 / 2 ページ目と `total` の COUNT）。フィルタ付きソートの temp B-tree は許容（D-39）
- [x] `GET /api/search` FTS5 trigram、3 文字未満は LIKE フォールバック（一覧と同じレスポンス形）
- [x] `GET /api/albums` / `:id`
- [x] `GET /api/events`（SSE: job / batch / library）。scan 完了時に `library`（200 件以下は
      `ids`、超えたら `bulk`）を流す。`GET /api/tracks/:id`（CIDR 経由は限定フィールド）も併せて実装

受け入れ: 6 万件 + 履歴 10 万 op + 重複 5% の合成 DB で、100 件取得が warm cache で 100ms 以内
（バッジ用の JOIN と `total` 込み。対象 NAS または参照マシンを明記）。日本語の部分一致が引ける。
フィルタ形の selection が 6 万件で ID 列挙なしに解決できる。
preview 後にスキャンで行が増えても apply の対象が増えない。
計測: `tests/tracks_perf.rs`（`#[ignore]`、`cargo test --release --test tracks_perf -- --ignored --nocapture`）。
2026-09-16、TrueNAS ホスト（AMD Ryzen 5 7600、DB は tmpfs）で最悪 53ms（FTS で全行に当たる語）、
既定ソート 0.75ms。詳細は D-39

依存: P0-3, P0-6

### P0-8 表 UI（SPEC §12）

- [x] 3 ペイン骨格: 上部ナビ / 左サイドバー（ツリー・プレイリスト・固定フィルタ）/ 表 /
      右パネル（折りたたみ・幅永続化）/ 下部バー（ジョブ要約。再生は P1-9）。ログイン画面と
      401 → ログインへの復帰、他画面は骨格のプレースホルダ
- [x] TanStack Table + Virtual による仮想スクロール（行はカーソル順に積み、表には窓だけ渡す。D-40）
- [x] 列の表示切替・並べ替え（ヘッダのドラッグ）・幅の永続化（localStorage）
- [x] 範囲選択（Shift / Ctrl）、Ctrl+A はフィルタ形 selection。**選択は immutable**（選択時の
      フィルタを保持。表示フィルタを変えても集合は変わらない）。選択件数・反映待ち件数・表示件数
- [x] バッジ列（検証 / 可逆 / RG / Derived+stale / 反映待ち / conflict / 重複 / hardlink / missing）
- [x] 反映待ちの行はグレーで編集不可（`aria-disabled`。編集 UI 自体は P0-10）
- [x] SSE `library` イベントで表示中ページを無効化・再取得（`ids` は該当時のみ、`bulk` は無条件）、
      `job` / `batch` で下部バーを更新。選択はイベントで変えない
- [x] conflict バッジは「最新 op が skipped_conflict」で判定（サーバの `conflict_batch_id` をそのまま使う）

受け入れ: 6 万行でスクロールが 60fps を保つ（Chrome 最新安定版、参照マシン、DevTools の
Performance で 5 秒間のフレーム落ちを計測）。選択状態がソート変更後も維持される。
ツリー・プレイリスト・フィルタのどれを選んでも同じ表コンポーネントが集合を差し替えるだけ。
計測: 2026-09-16、TrueNAS ホスト（Ryzen 5 7600）の headless Chromium で 6 万件、5 秒の連続スクロールで
60.2 fps・20ms 超のフレーム 0。ソート変更・Ctrl+A・表示フィルタ変更後の選択維持と、スキャンによる
SSE 再取得を同じセッションで確認。詳細と Chrome 安定版での再計測の扱いは D-40。
純粋ロジックは `cd web && npx vitest run`（20 件）

依存: P0-7

### P0-9 編集履歴の記録機構

タグ編集・リネーム・論理削除の共通土台。API はまだ持たない。

- [x] `edit_batches` / `edit_ops` / `edits` への記録（旧値・新値は JSON、
      事前条件 dev / inode / size / mtime_ns / **ctime_ns** / **tag_hash** / rel_path は op に持つ）
      （`db::history`、`edit::Editor::prepare_tags`）
- [x] 事前条件の確認は open した FD の fstat + 同じ FD からのタグ読みで行い、tmp はその FD の
      親ディレクトリに作る（`fsroot::fstat` / `RootDir::replace_file`）
- [x] **ファイル反映の単位は op（トラック）。** 同一トラックの全フィールドを 1 回の
      tmp+rename で書き、配下の edits を同時に確定。`tag_version` はトラックごとに 1 回
      （`domain::tags::write_tag_changes`、`edit::Editor::apply_op`、`jobs::handlers::tagwrite`）
- [x] pending の op があるトラックへの新規 op を拒否（DB の partial UNIQUE + `EditError::Pending`。
      API の 409 は P0-10）
- [x] バッチ状態機械 `prepared → applying → applied | partial | failed | cancelled`。
      終端状態は子ジョブ・op の結果から集計（`db::history::aggregate_batch`、SSE `batch`）
- [x] 事前条件の再確認と `skipped_conflict` 判定（op 単位。1 フィールドでも不一致なら op 全体）。
      外部 rename（rel_path のみ不一致）は tags op なら追随（rename が進める ctime はその場合だけ
      許容。D-41）、宛先 key の占有はスキャナが解決済み。rename op は P0-11
- [x] **overlay の解消**: applied 以外の終端になる op は同じトランザクションで DB をファイルの
      現在値へ戻す（読めなければ記録値。D-41）
- [x] 起動時リカバリ: `prepared` / `applying` のバッチの track ジョブ再投入、rename 済み op の
      「ファイルの全フィールドが新値と一致すれば applied」確定（`Editor::recover` + `apply_op`）
- [x] バッチ単位のキャンセル（子ジョブへ `cancel_requested_at`、未着手 op は failed）
      （`Editor::cancel_batch`。API は P0-12）

受け入れ: 単体テストで、同じ op の適用を何度繰り返しても結果と `tag_version` が
変わらない。同一トラックの 3 フィールド編集が 1 回の rename で反映され、op が applied になる。
事前条件不一致の op がファイルを触らない。`touch -r` で mtime を保った in-place 更新が
ctime で検出される。pending 中の同一トラックへの op 追加が拒否される。
cancel / conflict になった op のトラックが、その直後にファイルの値で読める（overlay 解消）。

依存: P0-4, P0-5

### P0-10 タグ一括編集

- [x] 操作: 固定値代入 / フィールド参照（`%albumartist%`）/ 正規表現置換 /
      トラック番号連番 / タグ削除（`domain::tagops`。JSON 形は SPEC §9、D-42）
- [x] 右パネル「一括編集」タブ: 操作リストの組み立て、プレビュー結果を表のセルに差分表示
      （旧→新、変更なしは薄く）、操作リストは選択を変えても残る
      （`web/src/components/BatchEditPanel.tsx`、`hooks/useBatchEdit.ts`、`lib/tagops.ts` / `lib/preview.ts`）
- [x] インライン編集（セルのダブルクリック → 1 件バッチ、プレビュー省略）
- [x] `POST /api/tracks/batch/preview` による dry-run（`changed / unchanged / pending_excluded`）
- [x] 対象に pending の op があれば 409（件数と track_id を返す）。`skip_pending` で除外して続行。
      UI は「M 件を除外して適用 / 待つ」の 2 択
- [x] 1 トランザクションで `edit_ops` / `edits` 記録 → DB 更新 → track 単位の `tagwrite`
      ジョブ投入（`jobs.edit_batch_id` で紐づけ）（`Editor::prepare_tags_with`）
- [x] ファイル書き込みは tmp + fsync + rename。**成功時に `(dev, inode, mtime_ns)` を更新**（P0-9）
- [x] 書き込み前に事前条件を再確認し、外部変更があればスキップして報告（P0-9）
- [x] `tag_version` の差分で Derived タグ上書きジョブを投入（`derived_files` が無ければ no-op。
      `transcode` の `kind = "retag"`。ハンドラは P1-10。D-42）

受け入れ: 1000 件の一括編集後、再スキャンで重複が発生しない。
外部で書き換えたファイルが上書きされない。
**tagwrite が完了する前にスキャンを走らせても編集が DB から消えない。**
tagwrite の途中で kill → 再起動で残りが反映され、二重に版が上がらない。

依存: P0-8, P0-9

### P0-11 パス生成とリネーム

- [x] テンプレート展開（`{category}/{albumartist}/{album}/...`）（`domain::pathgen::Template`。
      `[layout]` は設定の読み込み時に検証。single / multi / unsorted の選択は D-43）
- [x] ファイル名の置換テーブル、SMB 制約、255 バイト切り詰め（`pathgen::sanitize_component`、
      パス全体 240 UTF-16 単位。D-43）
- [x] 衝突時の降格（`{album}` → `{album} ({year})` → `{edition}`）（`pathgen::plan`。降格しても
      解決しなければ conflict。既にそのディレクトリにいるリリースは降格しない）
- [x] 同一リリース判定（MB Release ID / DiscID）。**同名 ≠ 同一リリース**（`mb:` → `disc:` →
      `album:<id>` のキー。album 行が違えば別リリース）
- [x] 一括リネームは coordinator の **2 phase**: phase 1 で全 op の source を一時名へ退避、
      phase 2 で `ordinal` 順に最終名へ（`RENAME_NOREPLACE`）。`rename` ジョブは**バッチ 1 つに
      1 つ**・並列 1。衝突判定は `rel_path_key`、DB の overlay は prepare 時の 2 段階更新、
      一時名は `spindle-rename-<op_id>.<ext>`（`edit::rename`、`jobs::handlers::rename`。D-43）
- [x] phase 境界でのクラッシュ復旧: 再投入されたジョブが op ごとの所在（最終名 / 一時名 / source）を
      inode で判定して続きを行う（`Editor::recover` がバッチジョブを再投入）
- [x] `edit_ops(kind='rename')` への記録（`expected_rel_path` 含む）と、ファイル rename 後の
      `(dev, inode)` 追随。外部 rename と衝突した op は `skipped_conflict`（スキャナは一時名 /
      最終名を「作業中」として扱う）
- [x] album の追随（album 全体の移動は id を維持して `rel_dir` を書き換え、部分移動は新規 album。
      overlay と overlay 解消の両方。D-43）
- [x] `POST /api/rename/preview` / `POST /api/rename/apply`（SPEC §9。UI は未着手）

受け入れ: 単体テストで置換テーブル・切り詰め・衝突降格を網羅。
既存の ytmusic 出力と同じパスが再現できる。
大小文字だけ違う 2 つのファイル名への一括リネームが衝突として検出される。
A↔B の swap と 3 件の循環リネームが完了し、phase 1 直後に kill しても再起動で完了する。
（`tests/pathgen.rs` / `tests/rename.rs` / `tests/rename_api.rs`）

未決: album 全体を動かした後の旧ディレクトリに残る同梱ファイル（cover.jpg / disc.cue / rip.log）の
追随。rename op はトラックのパスだけを所有する（D-43）

依存: P0-6, P0-9

### P0-12 編集履歴 API と巻き戻し

- [x] `GET /api/history`、バッチ単位の一覧（state / affected / applied / conflict / failed /
      reverts_batch_id / reverted_by）。`GET /api/history/:id` で op 一覧と edits
      （`api::history`、`db::history::list_batches` / `get_batch_summary`）
- [x] 履歴画面（SPEC §12.4）: 一覧、行を開いて op と conflict の現在値、[巻き戻す] は終端状態のみ、
      戻し済みは「#N で戻し済み」、↩ で逆バッチの関係を表示
      （`web/src/components/HistoryView.tsx`、`hooks/useHistory.ts`、`lib/history.ts`）
- [x] `POST /api/history/:batch/revert` で DB とファイルの両方を戻す。
      **tag / rename / delete の 3 種すべて**が対象（`edit::revert`。tags は `prepare_tags_tx`、
      rename は `prepare_rename_tx` に乗り、delete は DB だけなので即終端。D-44）
- [x] 対象は終端状態のバッチのみ（`prepared` / `applying` は 409。先にキャンセル）
- [x] 対象集合 = 元バッチの `applied` op − 既存逆バッチで `applied` 済みの op。空なら 409
      `already_reverted`
- [x] 逆バッチを作り `reverts_batch_id` で元を指す。元バッチの `reverted_at` は
      **逆バッチが終端になり対象を全件 applied にしたとき**だけ立てる
      （`history::aggregate_batch` → `mark_reverted_if_covered`）
- [x] 現在値が元バッチの新値と 1 フィールドでも違う op は `skipped_conflict`（op 単位）。
      戻そうとした変更は edits に残す
- [x] `POST /api/history/:batch/cancel`

受け入れ: 1000 件の一括編集を戻して元のタグに一致する。
1000 件の一括リネームを戻して元のパスに一致する。
戻した後にさらに戻す（= やり直し = 逆バッチの revert）ができ、全件戻し済みのバッチの再 revert は拒否される。
巻き戻し対象の 1 件を外部で書き換えておくと、その 1 件だけ conflict になり他は戻る。
そのとき元バッチの `reverted_at` は立たず、外部変更を直してから再 revert すると残り 1 件だけが対象になる。
`partial` のバッチを戻すと applied だった op だけが戻り、skipped だった op は触られない。
（`tests/revert.rs` / `tests/history_api.rs` / `web/src/lib/history.test.ts`）

依存: P0-10, P0-11

### P0-13 バックアップと復元

DB にしか存在しないもの（編集履歴 / プレイリスト / 検証結果 / ジョブ履歴）を守る。
実データを載せる前に必須。

- [x] `backup` ジョブ: `VACUUM INTO` で `data/backup/` へ（`[backup].interval_hours`）。tmp に書いて
      fsync → rename → 親ディレクトリ fsync、容量不足で中断、世代 GC（`[backup].retention_generations`）
- [x] 復元手順を `docs/OPERATIONS.md` に書く
      （停止 → ファイル差し替え → 起動 → 起動時スキャンで差分吸収）
- [x] 復元ドリルのテスト: バックアップから復元した DB で再スキャンし、
      トラック数・プレイリスト・履歴件数が一致する（`tests/backup.rs`）

受け入れ: バックアップを取って DB を消し、復元 → スキャンで元の状態に戻る
（fixture: 編集履歴 3 バッチとプレイリスト 2 本を含む DB。履歴とプレイリストが復元後も一致）。

依存: P0-4, P0-6, P0-9

### P0-14 移行の実施

- [x] `scripts/preflight.py` でブロッカーを解消（hardlink・symlink・衝突・不正 UTF-8・255 バイト超）。
      実機 `ssd/musics` は `Opus/` `Original/` とも exit 0（ブロッカーなし、全 NFC）
- [x] 実機の構成を確認し、振り分けを決めて `docs/DECISIONS.md` D-45 に記録
      （`Original/` は大半が ALAC ロスレス → Library の master。`AAC/` は移さない。
      Archive は `hdd`。Library のロスレスは後で FLAC に統一）
- [x] `scripts/migrate_plan.py`: `Opus/` と `Original/` の 1:1 対応から `rsync --files-from` の一覧を
      生成（Library 音声 9,098、Archive 1,514、unmatched 0）
- [x] データセット作成 → rsync → 検証（2026-09-16。checksum 差分 0）。**この移行はリハーサル**で、
      spindle 完成まで `ssd/musics` が正。リリース時に `ssd/media` 等を作り直して再移行する
      （MIGRATION.md §5）。ACL プリセットと定期スナップショットの設定はその時に行う
- [x] 初回スキャンを完走させ、`--audio-only` マニフェストの行数（= `summary.json` の
      `library_audio_total`）とトラック数を突き合わせる（9,098 = 9,098 = 9,098。errors 0、
      duplicate 10 は A ver / B ver・アルバムとシングルの同一 PCM で正当）
- [x] webm 由来の標本 5 件で `Archive/` の webm と `Library/` の `.opus` の Opus パケット列
      MD5（`ffmpeg -c copy -f data`）が一致。「webm 由来は Library の .opus が master」を固定
- [x] バックアップ 1 世代（`spindle-20260916T150022Z.db`）、一括編集 1 件（COMMENT 付与）と
      巻き戻しが applied。ファイルも元に戻った

受け入れ: 旧ライブラリの音声ファイル数（振り分け後）と DB のトラック数が一致する。
バックアップが 1 世代以上取れている。移行後に 1 件の一括編集と巻き戻しが通る。

移行中に見つかった後続課題（P0-14 の範囲外。優先度は要判断）:

- 初回 deep scan が遅い。`Scanner` の Phase 2（同一性解決）が `audio_md5` を 1 スレッドで直列に
  計算し、Phase 3 の並列読みはそのキャッシュを使うだけ。空の DB では md5 で突き合わせる既存行が
  無いので Phase 2 では計算せず Phase 3 に回せる（ALAC 7,572 本で約 75 分 → 並列度分だけ短縮）
- Phase 2 の間は進捗（done / total）が出ない。UI では 1 時間以上「running」のまま見える
- `symphonia` が 1 ファイルごとに INFO / WARN（`skipped 4 bytes of junk`、`stream is seekable`）を
  出す。既定のログフィルタで `symphonia=error` に落とす
- ホストに `/dev/sr0` が無いと `deploy/compose.yaml` の `devices` で起動に失敗する。CD ドライブは
  P2 まで無いので、移行時は devices を外した compose（`/root/spindle-migration/compose.yaml`）で
  起動した。P2 で `devices` を optional にするか、compose を 2 段にする

依存: P0-12, P0-13

---

## P1 — 日常運用

完了条件: **foobar2000 を開かずに日常運用が回る。**

- [ ] **P1-1** ReplayGain スキャン（`ebur128`、album は `album_id` 単位、
      2ch 以外は集計から除外）
- [ ] **P1-2** RG タグ書き込み（Opus のみ -23 LUFS 基準の Q7.8:
      `round((G18 - 5.0) * 256)` を符号付き 16bit に飽和。SPEC §6 のテストベクトルを
      単体テストに置く。`rg_scanned_at` と `rg_written_at` を分離）
- [ ] **P1-3** アートワーク（埋め込み / `cover.jpg` 両対応、抽出・一括差し替え、
      WebP サムネイル生成とキャッシュ。アルバムグリッド画面 → クリックで表を `album_id` に絞る）
- [ ] **P1-4** ロスレス → FLAC 正規化（WAV / ALAC / AIFF。D-45。変換前後の PCM MD5 照合。
      不一致なら中止。一致時は元ファイルを `Archive/` へ move し `edit_ops(kind='archive')` と
      `archived_files` 台帳に記録。**即時削除しない**。`audio_version` は据え置き。
      移行で取り込んだ ALAC 7,572 本が主対象）
- [ ] **P1-5** FLAC 健全性チェック（`flac -t`、MD5 未設定の補填。
      補填時は `audio_version` 据え置き）
- [ ] **P1-6** プレイリスト（手動、並べ替え、m3u8 書き出し）
- [ ] **P1-7** スマートプレイリスト（`docs/DSL.md`。pest → AST → SQL）
- [ ] **P1-8** エクスポートプロファイル（foobar / android / internal、
      foobar Autoplaylist クエリ生成）。**依存: P1-10**（`delivery` プロファイルが Derived を前提）。
      タグ鮮度が必要な export / 同期は `stale_tags` の件数を明示するか追随ジョブの完了を待つ
- [ ] **P1-9** 再生（Range 対応、ALAC は既定で Opus 変換、
      `canPlayType()` によるクライアント能力判定。下部バー左側の再生 UI: 再生・停止・
      シーク・音量・RG 適用切替）
- [ ] **P1-10** Derived 自動生成と追随（`audio_version` / `tag_version` 差分判定、
      Library の移動・削除への追随。`delivery` ビューの版一致フォールバックの結合テスト）。
      P1-8 の `delivery` プロファイルと Android 同期がこれを前提にするため P3 から前倒し
- [ ] **P1-11** GC ジョブ（`missing_since` 30 日超の行、`Archive/` へ退避した WAV、
      Derived の孤児。**物理削除を行う唯一の経路**。dry-run と削除件数のログを必須にする）

---

## P2 — CD 取り込み

完了条件: **新規 CD が検証付きで取り込め、既存 FLAC が格付けされる。**

- [ ] **P2-1** ドライブ制御（デバイス割当、`CDROM_DRIVE_STATUS` ポーリング、eject）
- [ ] **P2-2** TOC 取得と各種 DiscID 算出（MusicBrainz / AccurateRip / FreeDB）
- [ ] **P2-3** MusicBrainz 照会（UA 必須、1req/s）と候補選択 UI
- [ ] **P2-4** **照会ゼロ件でも完走できる手入力経路**とトラックリスト貼り付け
- [ ] **P2-5** 吸い出し（全ディスクを 1 本の PCM として取得 → オフセット適用 → 分割）
- [ ] **P2-6** ARv1/v2 CRC と CTDB CRC32（先頭・末尾トラックの除外規則に注意）
- [ ] **P2-7** CTDB 照会・修復適用、AccurateRip は補助
- [ ] **P2-8** エンコードと配置、`rip.log` / `disc.cue` / `disc.toc` の出力
- [ ] **P2-9** 遡及照合（44.1/16/2ch かつサンプル数が 588 の倍数のときのみ）
- [ ] **P2-10** Inbox 取り込み（ステージング → 承認キュー → 配置）

---

## P3 — ytmusic 統合

完了条件: **ytmusic CLI を廃止できる。**

- [ ] **P3-1** タイトルパーサ移植（`fancy-regex`、規則は TOML 外出し、
      83 件のフィクスチャを Python 版と共有）
- [ ] **P3-2** チャンネル定義とカテゴリ写像（`config.toml` 互換維持）
- [ ] **P3-3** ダウンローダ（yt-dlp を subprocess）
- [ ] **P3-4** ytmusic ダウンロード後の Derived 投入（生成本体は P1-10、GC は P1-11）
- [ ] **P3-5** 偽ハイレゾ検出（`rustfft`、任意機能）

---

## 着手前に確認が必要な残課題

- Discogs / VGMdb 連携の要否（P3 以降）
- `.fpl` 書き出しの要否（P4、非推奨）
- `HAS` 演算子の foobar 実機との挙動突き合わせ（P1-8 実装時）
- Inbox のポーリング間隔
- 一括リネーム後の旧ディレクトリに残る同梱ファイル（cover.jpg / disc.cue / rip.log）と
  空ディレクトリの扱い（P0-11 では動かさない。D-43）

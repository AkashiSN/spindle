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
追随。rename op はトラックのパスだけを所有する（D-43）。Library に同梱ファイルを置き始める P2-8 で決める

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

着手順の目安（依存関係と P0-14 の状況から。2026-09-17 時点）:
P1-4（ALAC 7,572 本の FLAC 化。D-45 で決めた方針で、Archive 退避の台帳と GC の前提）→
P1-1 / P1-2（FLAC 化後に一括で RG）→ P1-10（旧 Opus を捨てたので Derived が空。P1-8 の前提）→
P1-9 / P1-3 → P1-6 / P1-7（`Playlists/m3u8` の 28 本を取り込む）→ P1-8 / P1-11 / P1-5。
実データでの検証は P0-14 のリハーサル環境（`ssh truenas`、`/root/spindle-migration/`、
`/mnt/ssd/media/Library` 9,098 トラック）で行う。`ssd/musics` は正なので触らない。

- [x] **P1-0** 初回 deep scan の高速化（P0-14 の後続課題。リリース時の再移行でも効く。D-50）:
      `Scanner` Phase 2 の直列 `audio_md5` を、既存行が md5 で突き合わせを要するときだけ計算するか
      Phase 3 の並列読みへ回す。Phase 2 中も進捗を出す。`symphonia` / `lofty` のファイルごとの
      WARN を既定フィルタで落とす（ALAC 7,572 本で 75 分 → 並列度分だけ短縮が目安）
      - [x] `identity::resolve`: 段 1 は行が md5 を持つときだけ、段 2 は移動候補があるときだけ md5 を要求。
            `identity::md5_requests` で要求されうる集合を先に求める（初回・移動なしは 0 件）
      - [x] `Scanner::compute_md5s`: 要求分を Phase 3 と同じ並列度で計算し、`resolve` と Phase 3 に渡す。
            `Progress` に相（`ScanPhase::Md5` / `Read`）を持たせて Phase 2 も進捗を出す
      - [x] `logging::default_filter`: `lofty` / `symphonia*` を error に

      受け入れ: `tests/identity.rs`（初回・移動元が残っている・段 1 で claim 済みなら md5 を要求しない、
      行が md5 を持たない inode 再利用は要求しない、移動候補があるときだけ未決分を要求、`md5_requests` が
      `resolve` の要求を含む）、`tests/scanner.rs`（初回と変更なしの増分で `Md5` 相が出ない、コピー + 削除で
      `Md5` 相が未決 2 本で出て移動として解決）、`tests/logging.rs`

      計測（2026-09-17、リハーサル環境 9,098 トラック / ALAC 7,570 本、12 コア）: 空 DB からの初回
      deep scan **552 秒**（P0-14 時点の 3,583 秒 → 約 6.5 倍速）、既存 DB への deep scan 554 秒、
      変更なしの増分 1 秒未満。Phase 5（アートワーク 721 album）は 1 秒未満。errors 0
- [x] **P1-1** ReplayGain スキャン（`ebur128`、album は `album_id` 単位、
      2ch 以外は集計から除外。D-47）
      - [x] `domain::replaygain`: `LoudnessMeter`（積分ラウドネス + true peak、フレーム端数の持ち越し）、
            `album_loudness`（構成トラックの状態をまとめてゲートし直す）、無音は gain 0
      - [x] `media::decode`: symphonia（FLAC / ALAC / WAV / AIFF / MP3 / AAC / Vorbis）→ Opus は
            OpusHead の情報 + ffmpeg `f32le`、WavPack / APE は lofty の属性 + ffmpeg。
            `ExternalCommand::stdout_channel` で stdout をチャンクのまま受ける
      - [x] `jobs::handlers::rg`: `{"album_id"}` / `{"track_id"}`、構成トラックを id 昇順に全件ロック、
            2ch だけ album 集計、all-or-nothing、`rg_scanned_at` のみ更新
      - [x] `POST /api/rg { selection }`（`api::rg`）。UI の起動導線は P1-12 の操作タブ

      受け入れ: `tests/replaygain.rs`（正弦波の LUFS / peak、album の電力平均、無音）、`tests/decode.rs`
      （FLAC / Opus / WavPack が同じ形で流れる、キャンセル、失敗）、`tests/rg_job.rs`（track / album の
      値、6ch の除外、missing の除外、album 無し、失敗で何も書かない、cancel）、`tests/rg_api.rs`

      `audio_version` が上がったときの `rg_scanned_at` の扱い（D-47）は P1-13 で解決
- [x] **P1-2** RG タグ書き込み（Opus のみ -23 LUFS 基準の Q7.8:
      `round((G18 - 5.0) * 256)` を符号付き 16bit に飽和。SPEC §6 のテストベクトルを
      単体テストに置く。`rg_scanned_at` と `rg_written_at` を分離。D-48）
      - [x] `domain::replaygain`: `opus_r128` / `rg2_gain_db` / `tag_changes`（形式ごとの固定キー集合。
            値の無いキーは削除）/ `file_matches`
      - [x] `Editor::prepare_rg_write`: 解析済みトラックを tags op の編集バッチとして記録（旧値・overlay・
            tagwrite・巻き戻しはタグ編集と共通）。DB のタグが既に一致する行は `rg_written_at` だけ立てる
      - [x] `rg_written_at` はファイルの現在値から判定（`db::replaygain::sync_written_at`。applied の追随・
            overlay の解消・巻き戻し・手編集で自動的に立つ / 消える）
      - [x] `POST /api/rg/write { selection, description?, skip_pending? }`（`api::rg::write`。missing は対象外）、
            フィルタ `rg_unwritten`。UI の起動導線は P1-12 の操作タブ

      受け入れ: `tests/replaygain.rs`（SPEC §6 のテストベクトル、飽和、書式、形式ごとのキー集合、
      `file_matches`）、`tests/rg_write.rs`（FLAC / Opus / MP4 への書き込みと `rg_written_at`、一致済みは
      バッチ無し、未解析の除外、pending、巻き戻し・手編集・conflict で NULL、再解析後の再書き込み）、
      `tests/rg_write_api.rs`

      スキャナが外部のタグ変更を取り込んだときの `rg_written_at` の判定（D-48）は P1-13 で解決
- [x] **P1-3** アートワーク（読みは埋め込み / 同梱画像の両対応、書きは埋め込み統一で一括差し替え、
      WebP サムネイル生成とキャッシュ。アルバムグリッド画面 → クリックで表を `album_id` に絞る）
      - [x] 読み側（D-49）: `media::artwork`（同梱画像の名前の優先順、ヘッダでの判別、埋め込みの選択、
            `ArtworkStore` = `<data>/thumbs/<hex>/orig.<ext>` + `<size>.webp`）
      - [x] スキャンの Phase 5（`Scanner::with_artwork`）: Phase 4 が新旧 album の再解決を予約
            （`artwork_resolved_at = NULL`）し、Phase 5 は予約 + 同梱画像の stat 変化 + 原画像の欠損だけを
            解決し直す（マイグレーション 0004）。決められない album は状態を動かさない。原画像を
            キャッシュへ置き `thumbnail` ジョブを投入。Phase 5 の cancel / 失敗は run を戻さない
      - [x] `jobs::handlers::thumbnail`: ffmpeg で 256 / 768 の WebP（長辺、拡大なし、tmp + rename、冪等）
      - [x] `GET /api/artwork/:hash?size=`（`api::artwork`。未生成なら原画像へ倒す）、
            `GET /api/albums` の `artwork_hash`
      - [x] UI: アルバムグリッド（`AlbumGrid`）→ クリックで一覧を `album_id` に絞る
      - [x] 書き側（D-60。埋め込み統一。cover ファイルは書かず抽出も作らない）: `POST /api/artwork/upload`
            （ヘッダで判別、32 MiB、`ArtworkStore` + `artwork` 行 + thumbnail）→ `POST /api/artwork/embed`
            （`Editor::prepare_picture`: `PICTURE` を `[<mime>:<hex>]` にする tags op。全画像を捨てて 1 枚）。
            `stage_tags` は書く前に旧画像を store へ退避、`write_tag_changes` に `pictures` を足す。
            GC 区分 E は `edits` の `PICTURE` 値が参照する画像を残し、行にも 24 時間の猶予。applied で
            album を `mark_unresolved` して増分スキャンを投入。UI は操作タブの「アートワーク」節と
            履歴の `PICTURE` サムネイル

      受け入れ（書き側）: `tests/picture_write.rs`（FLAC / Opus / MP4 への差し替えと `tag_version` +1、旧画像の
      退避、同じ画像は差分なし、巻き戻しで旧画像が戻る、外部変更で conflict、キャッシュ欠損で failed、
      AlreadyMatches、album の予約）、`tests/artwork_api.rs`（upload の形式判定・上限・400、embed の 404 / 409）、
      `tests/gc.rs`（`edits` が参照する画像は残す、行の 24 時間の猶予）、`web/src/lib/operations.test.ts`、
      `web/src/lib/artwork.test.ts`（`PICTURE` 値の分解、アップロードの要約）。ローカル起動で upload →
      差し替え → 履歴のサムネイル → 巻き戻しを agent-browser で確認済み（2026-09-18）

      受け入れ: `tests/artwork.rs`（名前の優先順、判別、埋め込みの選択、キャッシュの配置）、
      `tests/artwork_scan.rs`（同梱 > 埋め込み、最初のトラック、無し → NULL、名前の優先順、同梱画像の
      差し替え / 削除の検出、未解決 album の解決、deep、同じ画像の共有、読めない同梱画像、missing、
      画像付きトラックの移動で新旧 album を解決、commit 後の cancel で run は completed のまま次回再開、
      キャッシュ書き込み失敗 / 読めないトラックで状態を動かさない、原画像の欠損を incremental で復旧）、
      `tests/thumbnail_job.rs`（寸法・アスペクト比・拡大なし・冪等・原画像なし）、`tests/artwork_api.rs`
- [x] **P1-3c** トラック単位のアートワーク（D-61。トラックごとに画像が違う album 向け）:
      - [x] マイグレーション 0012 `tracks.artwork_id`。`TrackContent.picture`（Unread / Absent / Found）を
            スキャナ Phase 3 と tagwrite の読み戻しが埋め、`insert_track` / `update_content` が記録する。
            サムネイルが無い画像は Phase 4 が thumbnail ジョブを投入
      - [x] Derived は `COALESCE(tracks.artwork_id, albums.artwork_id)` を埋める（D-51 の改訂）
      - [x] GC 区分 E の参照に `tracks.artwork_id` を足す
      - [x] `TrackRow` / `GET /api/tracks/:id` の `artwork_hash`、左下のアートワークはトラック自身 → album

      受け入れ: `tests/artwork_scan.rs`（トラックごとの画像、front cover 優先、無しは NULL、同じ画像は 1 行、
      store 無しは NULL のまま → deep で埋まる、外部差し替えで更新）、`tests/picture_write.rs`（差し替え・
      巻き戻しで追随）、`tests/derived*.rs`（トラック自身 → album の順）、`tests/gc.rs`（参照の保護）、
      `tests/tracks_api.rs`（`artwork_hash`）、`tests/migrations.rs`（0012）、`web/src/lib/artwork.test.ts`。
      ローカル起動でトラックごとに違う画像の 2 曲について、左下の表示と Derived の埋め込み（ffprobe で
      300×300 / 400×300）がそれぞれ自身の画像になることを確認済み（2026-09-18）

      計測（2026-09-18、リハーサル環境 9,098 トラック / 721 album、12 コア）: 0011 / 0012 適用後の deep scan
      **568 秒**（P1-0 の 552 秒 + 3%）で全行に `artwork_id` が付き、`artwork` 2,241 行（+1,520。thumbs 2.1 GB）、
      thumbnail 1,523 本・transcode 再タグ 35 本、errors 0。トラックごとに画像が違う album は 15
      （神椿系「〜のお歌」9 dir が主。すべて Opus で Derived は無い）。UI で自身の画像が出ることを確認
- [x] **P1-4** ロスレス → FLAC 正規化（WAV / ALAC / AIFF。D-45 / D-46。変換前後の PCM MD5 照合。
      不一致なら中止。一致時は元ファイルを `Archive/` へ move し `edit_ops(kind='archive')` と
      `archived_files` 台帳に記録。**即時削除しない**。`audio_version` は据え置き。
      移行で取り込んだ ALAC 7,572 本が主対象）
      - [x] `POST /api/normalize/preview` / `apply`（selection。rename と同型。`api::normalize`。
            UI は P1-12 の操作タブ。`[normalize].wav_to_flac = false` で 409）
      - [x] `edit::normalize`: `edit_ops(kind='archive')` + `edits(rel_path / codec)`、DB は先行更新
            しない。track 単位の `normalize` ジョブ（並列 2、`jobs::handlers::normalize`）
      - [x] `media::encode::FlacEncoder`: ffmpeg デコード（root の FD を `/dev/stdin` で渡す）→
            `flac -8 --verify`。STREAMINFO MD5 を symphonia の PCM MD5 と照合。タグ・画像は lofty で
            写す（`tags::read_transfer_tags` / `write_flac_tags`）。`tag_hash` が変わったときだけ
            `tag_version++`
      - [x] 破壊フェーズ: 元を一時名 `spindle-normalize-<op_id>.<ext>` へ退避（inode 確認）→ Archive へ
            実コピー + SHA-256 の読み戻し照合（`edits.source_sha256` に記録）→ unlink 直前に同じ FD の
            stat とバイト列を再照合 → unlink。conflict は元パスへ戻して生成物を消す。台帳 `db::archive`
      - [x] 巻き戻し（`revert_archive`）: Archive からコピーで復元、FLAC は Archive へ move して
            `reason='restore'`（マイグレーション 0003）。redo は同じ台帳行を `held` に戻す
      - [x] 冪等（宛先 / 一時名 / Archive の実体と記録した SHA-256 から続きを判定）、cancel（Library を
            触る前まで）、スキャナが作業中のパス（元・一時名・宛先）を避け、inventory 後に確定された
            宛先を `Identity::New` で挿入しない（`PendingOp::target_rel_path`）
      - [x] symphonia の `aiff` feature を有効化（AIFF の PCM MD5 が計算できていなかった）

      受け入れ: `tests/normalize.rs` / `tests/normalize_api.rs` / `tests/encode.rs`。WAV / ALAC / AIFF が
      FLAC になり `audio_md5` / `audio_version` が変わらない。MD5 不一致（ffmpeg を差し替えた模擬）で
      元ファイルが無傷のまま failed。事前条件不一致・宛先の占有が conflict。revert → redo が通り
      Archive の実体が減らない。配置後 / 退避後 / unlink 後のクラッシュから再投入で完了。Archive のコピー
      中・unlink 直前の in-place 更新と元パスの差し替えでユーザデータを失わない。Archive のコピーが
      壊れた状態からの復旧は conflict で何も消さない。I/O 失敗は生成物を消して再試行できる。作業中に
      走ったスキャンが宛先を新規登録せず、inventory 後に確定されても走査が失敗しない。

      閉じた未決（2026-09-18）: 20 bit などの ffmpeg PCM エンコーダが無いビット深度は failed のまま。
      実データは 16 bit 7,447 / 24 bit 123 で 20 bit は 0 本、ALAC の正規化は全件完了済み、今後入るのは
      主に CD（16 bit）。failed はデータを壊さないので、出てきたら raw → `flac --bps` の経路を足す
- [x] **P1-5** FLAC 健全性チェック（`flac -t`、MD5 未設定の補填。
      補填時は `audio_version` 据え置き）。D-57。**補填は P1-5b に切り出し**（実データに FLAC が無く、
      今後の FLAC は自前の `flac -8 --verify` と CD リップで MD5 が付く）
      - [x] マイグレーション 0010: `tracks.flac_check` / `flac_checked_at` / `flac_check_version` /
            `flac_check_error`
      - [x] `jobs::handlers::flaccheck`: 版付き（`flaccheck:<id>:<audio_version>`、stale ゲート、
            `track_locks`）。root で開いた FD を fstat して行と照合 → STREAMINFO の MD5 → `flac -t -`
            （FD を stdin、タイムアウト、終了コード、stderr）→ 版を再確認して 1 UPDATE。ファイルは書かない
      - [x] `db::flaccheck`: `enqueue_selection` / `enqueue_all_unchecked`（スキャン完了時、
            `[normalize].flac_verify_on_import`）
      - [x] `POST /api/flaccheck { selection }`、一覧の `flac_check { status, checked_at, stale, error }`、
            フィルタ `flac_unchecked` / `flac_error`
      - [x] UI: バッジ（`decode_error` は赤、`md5_missing` は薄く、古い結果は点付き）とサイドバーの
            固定フィルタ。起動導線は P1-12 の操作タブ

      受け入れ: `tests/flaccheck_job.rs`（ok / md5_missing（ファイルを触らない）/ decode_error（stderr）/
      非 FLAC と missing は no-op / stale 版と差し替えは書かない / flac 無しで失敗 /
      `enqueue_all_unchecked` の対象 / scan 完了時の自動投入と 2 回目は投入しない）、
      `tests/flaccheck_api.rs`（投入件数・skip・重複・行の値・フィルタ・stale・409・401）、
      `web/src/lib/badges.test.ts`

      P1-5b（MD5 補填）と UI の起動導線（P1-12）は実施済み
- [x] **P1-5b** FLAC の MD5 補填（`flac_fix_missing_md5`）。`md5_missing` のトラックを選んで、デコードした
      PCM MD5 を STREAMINFO に書く編集バッチ（`edit_ops.kind` に `md5` を足すマイグレーション 0011、旧値 = 全ゼロ
      を `edits` に残して巻き戻し可、`audio_version` 据え置き、inode / mtime は追随）。D-57 / D-59。
      `POST /api/md5fill`、操作タブの「MD5 を補填」。受け入れ: `tests/migrations.rs`（0011 で参照行が残る）、
      `tests/fingerprint.rs`（MD5 の位置）、`tests/md5fill.rs`（補填 / 対象外 / 巻き戻し / conflict）、
      `tests/md5fill_api.rs`（409 の各コード）
- [x] **P1-6** プレイリスト（手動、並べ替え、m3u8 書き出し。D-53）
      - [x] `db::playlists`: CRUD（名前の一意性は `name_key` = canonical key。マイグレーション 0006）、
            項目の追加（同じトラックは 1 回）・除外・移動（`before` の直前 / 末尾）、`position` の振り直し、
            書き出し記録、`export_profiles` の読み出し
      - [x] `sort=position`（`filter.playlist_id` と組でだけ有効。`playlist_items` を JOIN して主キー順、
            カーソルページング可）
      - [x] `playlist::export`（m3u8 の生成とパス写像: relative は `../../`、absolute は prefix、区切りの
            置換）、`playlist::import`（行の正規化 → `rel_path_key` 完全一致 → stem 一致、active 優先、
            複数なら曖昧として未解決。root 名の無い絶対パスは未解決）
      - [x] API: `GET/POST /api/playlists`、`GET/PATCH/DELETE /:id`、`POST/DELETE /:id/items`、
            `POST /:id/items/move`、`GET /:id/export?profile=`（本文。CIDR allowlist）/ `POST`
            （`Playlists/<profile>/<name>.m3u8` に tmp + rename、`playlist_exports` に記録）、
            `GET/POST /api/playlists/import`（Playlists root 下の m3u8 の一覧 / 取り込み）
      - [x] UI: サイドバーのプレイリスト区画（一覧・作成・改名・削除・書き出し・取り込み）、表の行を
            プレイリストへドラッグで追加、右パネルの「プレイリストへ追加」（Ctrl+A のフィルタ形も可）、
            プレイリスト scope では position 順、行のドラッグで並べ替え、「除外」ボタンと Delete キー

      受け入れ: `tests/playlist_export.rs`（3 プロファイルの写像、EXTINF、BOM なし）、
      `tests/playlist_import.rs`（BOM / `#` 行 / `\` / `../` / root 名 / UNC・ドライブレター / 完全一致 vs
      stem / active 優先 / 曖昧 / 未解決 / 重複）、`tests/playlists_db.rs`（作成・重複（大小文字 / NFC-NFD）、
      追加の順と skip、除外、移動、改名、削除の
      CASCADE、件数と記録、`export_tracks` の missing 除外と delivery）、`tests/tracks_query.rs`（position
      順とカーソル、playlist_id 必須、temp B-tree なし、selection の position 順）、
      `tests/playlists_api.rs`（CRUD、名前の検証、項目、export の GET / POST / CIDR / 503、import）、
      `web/src/lib/playlists.test.ts`（scope とソートの連動、ドラッグの payload、並べ替えの移動先）

      取り込み（2026-09-17、リハーサル環境）: `Playlists/m3u8/` の 28 本を `POST /api/playlists/import` で
      全件取り込み。14,205 行すべてが解決（未解決 0、重複 0。旧 `../<Category>/…/<n>. Title.opus` 行が
      stem 一致で ALAC / Opus の Library 行に当たった）。`00_Anime` の android 書き出し 4,741 件も確認
      （`Playlists/android/00_Anime.m3u8`、`../../Derived/…`）

      自動再書き出しは P1-7（D-54 の `playlist::autoexport`）で実施済み。スマートの表示（`kind='smart'` は
      ⚙ で出すだけ）
- [x] **P1-7** スマートプレイリスト（`docs/DSL.md`。pest → AST → SQL。D-54）
      - [x] `playlist::dsl`: `dsl.pest` の文法（キーワードは大小文字無視、値は引用可、`NOT` > `AND` > `OR`）→
            AST（DSL.md の JSON 形で `rule_ast` に保存、原文は `rule_source`）、構文エラーは行・桁付き
      - [x] `playlist::compile`: ホワイトリストの列解決、任意タグは `track_tags` の EXISTS、値は全てバインド、
            `regexp()` を全コネクションに登録、`missing` を参照しない限り active 限定、型に合わない演算子は
            実行前にエラー。`evaluate` で並び付きの id 列
      - [x] マイグレーション 0007: `tracks.added_at`（`added` フィールド。スキャナが INSERT 時に設定）
      - [x] `filter.dsl`（一覧のプレビュー。WHERE だけ。D-39）
      - [x] API: `POST /api/playlists { name, rule }` / `PATCH { rule }`（再評価）、`POST /api/playlists/preview`、
            `POST /:id/refresh`、smart への項目操作は 409
      - [x] `playlist::autoexport`: library / 終端 batch / 完了 job をデバウンスして全 smart を再評価、
            `auto_export = 1` で記録のあるプレイリストを記録済みプロファイルへ再書き出し（起動時にも 1 回）
      - [x] UI: サイドバー「＋⚙」→ 中央のルール編集（250ms で検証・件数、表は `filter.dsl` で追随、
            Ctrl+Enter で保存）、smart 行の「ルールを編集」「再評価」、smart は ⚙ 表示でドロップ・並べ替え不可

      受け入れ: `tests/dsl.rs`（優先順位・括弧・引用・大小文字・全演算子・PRESENT/MISSING・ORDER/LIMIT・
      エラー位置・AST の往復）、`tests/dsl_compile.rs`（`:memory:` での評価: NOCASE、暗黙 missing、HAS /
      MATCHES / presence、任意タグの多値、数値と duration、拡張フィールド、date / added、ORDER / LIMIT /
      random、型エラー、値がバインドされる）、`tests/tracks_query.rs`（`filter.dsl`）、
      `tests/smart_playlists.rs`（作成で materialize、400 の位置、preview、refresh / ルール差し替え、
      項目操作の 409、autoexport のデバウンスと再書き出し・`auto_export = 0`）

      foobar Autoplaylist クエリ変換は P1-8 で実施済み、`HAS` の語境界は実機で確認済み（D-55）。
      UI の並び替え（smart は ORDER BY で決まるので表のソート変更は表示だけ）
- [x] **P1-8** エクスポートプロファイル（foobar / android / internal、
      foobar Autoplaylist クエリ生成）。**依存: P1-10**（`delivery` プロファイルが Derived を前提）。
      タグ鮮度が必要な export / 同期は `stale_tags` の件数を明示するか追随ジョブの完了を待つ。D-55
      - [x] `playlist::fb2k`: AST → `{ query, sort, notes }`（写像表、後置 PRESENT / MISSING、date の
            AFTER / BEFORE、引用規則、変換不能な葉の脱落と親の畳み込み、ORDER BY の分離）
      - [x] `GET /api/playlists/:id/fb2k_query`（smart のみ。manual は 409）
      - [x] UI: smart のメニュー「foobar クエリ」→ コピーボタン付きダイアログ（`Fb2kQueryDialog`）
      - [x] `export_tracks` が `delivery` のタグ追随待ちを数え、`POST …/export` の応答と UI の通知に
            `stale_tags`。自動再書き出しはログ
      - [x] `[export].fb2k_prefix` を正として起動時に `export_profiles.foobar.path_prefix` を揃える。
            プロファイル CRUD と `.pls` は作らない

      受け入れ: `tests/fb2k.rs`（写像表全件、技術フィールド、spindle 固有の脱落、後置 PRESENT、
      AFTER / BEFORE、MATCHES、引用規則と `"` の警告、括弧、脱落による親の畳み込み、sort と DESC、
      random / LIMIT、DSL.md の例）、`tests/smart_playlists.rs`（fb2k_query の 200 / 409 / 404 / 401）、
      `tests/playlists_db.rs`（`stale_tags` の集計、prefix の同期）、`tests/playlists_api.rs`
      （android の `stale_tags`、internal は 0）、`web/src/lib/playlists.test.ts`（通知文）

      foobar 実機との突き合わせ（2026-09-19）: `HAS` は部分一致で spindle と同じ、技術情報フィールドの
      `PRESENT` / `MISSING` も効く。二重引用符は挙動を変えず、単一引用符は一致しなくなる（D-55）。
      変換器の変更は不要
- [x] **P1-9** 再生（Range 対応、ALAC は既定で Opus 変換、
      `canPlayType()` によるクライアント能力判定。下部バー左側の再生 UI: 再生・停止・
      シーク・音量・RG の off / track / album）。D-52
      - [x] `api::stream`: `GET /api/stream/:id`（原本、Range / HEAD / ETag、開いた FD を行と照合して
            不一致は 409 `stale`）、`?transcode=opus`（`delivery` が Derived を指せば直送、無ければ ffmpeg で
            Ogg/Opus を chunked、`start=` で `-ss`。非可逆は変換しない）
      - [x] `GET /api/tracks` の行に `rg`（解析値。未解析なら null）
      - [x] UI: 行先頭の ▶（ホバー表示、その行から表示順に連続再生）、下部バーの再生
            （`<audio>` + Web Audio の GainNode で RG、音量、シーク、原本 / Derived の切替）、
            `canPlayType()` による URL の選択（`lib/playback`）
      - [x] RG の album モード（`album_gain` が無ければ track に倒す。`playback.test.ts`）
      - [x] テスト: `tests/stream_api.rs`（Range / HEAD / ETag / 416 / stale / missing / CIDR / MIME /
            Derived 直送 / ffmpeg フォールバックと `start=` / ffmpeg 無し 503）、
            `web/src/lib/playback.test.ts`

      閉じた未決（2026-09-18。D-52）: Safari 向けの AAC 変換（Safari 18.4+ は Ogg Opus をネイティブ
      再生する。実機の Safari で確認済み）。ハイレゾのサンプルレート変換。可逆は既定で Derived
      の Opus（48 kHz）を再生するので、96 kHz（64 本）がそのまま流れるのは「原本」を選んだときだけ。原本を
      選んだ人にリサンプルを掛けるのは逆なので作らない
- [x] **P1-10** Derived 自動生成と追随（`audio_version` / `tag_version` 差分判定、
      Library の移動・削除への追随。`delivery` ビューの版一致フォールバックの結合テスト）。
      P1-8 の `delivery` プロファイルと Android 同期がこれを前提にするため P3 から前倒し。D-51
      - [x] マイグレーション 0005: `derived_files.src_artwork_id` / `src_rg_scanned_at`
            （タグ版に乗らない 2 つの世代）
      - [x] `domain::derived`: 期待パス（拡張子を `.opus` に）、対象判定（可逆・active・1ch / 2ch）、
            `plan`（Skip / Encode / Move / Retag / MoveAndRetag / UpToDate）、Opus に書くタグ集合
            （`TransferTags` + DB の RG を `R128_*` へ + album のカバー 1 枚）
      - [x] `media::encode::OpusEncoder`: ffmpeg → WAV → `opusenc --vbr --music`。
            `domain::tags::write_opus_tags` でタグと画像
      - [x] `db::derived`: 行の読み書き、`enqueue_if_stale`、scan 完了時の一括投入
            （`enqueue_all_stale`）、占有行の明け渡し
      - [x] `jobs::handlers::transcode`: 現在値に揃える（no-op / 再エンコード / rename / retag）。
            Library の FD を DB の行と照合、期待パスの排他予約（`derived_path_locks`）と占有の確定
            （`claim_path`）を物理書き込みの前に、宛先の tmp + `replace_file`、cancel、冪等。画像は album の `768.webp`（キャッシュに
            無ければ生成、原画像も無ければ画像なしで `src_artwork_id = NULL`）。起動時の取り残し回収
            （`sweep_tmp`）
      - [x] 投入契機: scan ジョブ完了、tagwrite applied（`enqueue_derived_retag` を置換）、
            rename applied、RG 保存
      - [x] `delivery` ビューの結合テスト（生成 → Derived、`audio_version++` → Library、
            `tag_version++` → `stale_tags`、retag → 解消）

      受け入れ: `tests/derived.rs`（純粋な判定）、`tests/derived_db.rs`（投入判定）、
      `tests/opus_encode.rs`、`tests/transcode_job.rs`（初回・no-op・retag・再エンコード・stale・
      外部移動・消失・カバー差し替え・RG 後追い・FD 不一致・占有・占有確定後の復活との競合・
      同じ期待パスの並走・retag 中の claim 待ち・swap / 3 件循環の追随（音声差し替え付き・実体無しを
      含む）・move 失敗時の予約解放・
      画像キャッシュ欠損からの復旧・cancel・取り残し回収）、
      `tests/derived_sync.rs`
      （4 つの契機と `delivery` の end-to-end）。UI の起動導線は無し（scan 完了時の自動投入で足りる）

      計測（2026-09-17、リハーサル環境 9,098 トラック / 可逆は ALAC 7,570 本、12 コア、並列 11）:
      起動時スキャンの完了で 7,570 件を投入 → **39 分**で全件 done（failed 0、警告 0、tmp の残り 0）。
      Derived は 30 GB（全件に album の WebP カバー入り）。`delivery` は可逆 7,570 が Derived、
      非可逆 1,528 が Library 原本

      閉じた未決（2026-09-18。D-51）: マルチチャンネルのダウンミックス（全 9,098 トラックが 2ch）、
      非可逆の `force_transcode`（非可逆は Opus 1,512 / MP3 14 / AAC 2。Opus は変換の意味が無く残りは
      16 本）、Derived 側の `cover.jpg` ミラー（全件に WebP を埋め込み済み。Library は D-49 で同梱ファイルを
      書かない方針なので Derived にだけ書くと方針が割れる）。需要が出たら再開
- [x] **P1-11** GC ジョブ（`missing_since` 30 日超の行、`Archive/` へ退避した WAV、
      Derived の孤児。**物理削除を行う唯一の経路**。dry-run と削除件数のログを必須にする）。D-56
      - [x] `gc::plan`（読み取りのみ）: A missing トラック（`stat` で実体が無いことを再確認）、B missing
            アルバム（構成 0）、C `archived_files` の `held` で期限超、D Derived の孤児（`.spindle-tmp-*` と
            24 時間以内は除外）、E 参照の無い `artwork` 行と行の無い `thumbs/<hex>/`
      - [x] `gc::execute_*`: A → B → E(行) を 1 トランザクション（条件を再確認）→ C（`held` と期限を
            再確認しトラックをロックしてから unlink → CAS で `deleted`）→ D（行が無ければ transcode と
            同じ `derived_path_locks` の予約を取り、unlink 直前に inode / mtime を照合、空ディレクトリも
            消す。マイグレーション 0008）→ E(dir)（行が無く猶予超を再確認）。
            失敗はログして続行、区分ごとの件数・バイト数を `info!`
      - [x] `jobs::handlers::gc`: scan と同じ名前付き排他 `library`（`job_mutexes`。マイグレーション
            0009、`JobContext::lock_mutex`）を取れなければ `Requeue`。`jobs::scheduler` に backup と共通の
            周期投入を切り出し、1 日 1 回自動投入
      - [x] `GET /api/gc/preview`（dry-run。`plan` を同期で返す）、`POST /api/gc`。UI は作らない
      - [x] `RootDir::remove_dir`

      受け入れ: `tests/gc.rs`（期限前後と実体の有無、CASCADE と履歴の残存、アルバムの構成判定、Archive
      の状態遷移と unlink 失敗、Derived の孤児・tmp・猶予・同じ実行で消える missing の Derived・空
      ディレクトリ・一覧後の差し替え、アートワークの行と dir、scan 中の待ちと dedup、計画後の
      restored / 期限延長 / 他ジョブのロック / 復活 / claim / 参照の出現 / dir の更新で消えないこと、
      GC の予約中は transcode が claim できず残骸は奪えること、mutex を持つ相手がいれば待つこと、
      scan と gc を同時に 5 回投入して両方が完走すること）、
      `tests/gc_api.rs`
      （preview が何も消さない、POST の 202 / 409 / 401、root 無しの 503）

      設定画面からの起動は P1-12 (e) で実施済み。Library の同梱ファイルの回収（D-43）は P2-8 で決める

- [x] **P1-13** スキャナの ReplayGain 追随（D-47 / D-48 の未決）: 外部で音声が差し替わった行は解析値を
      捨てる（`reset_analysis`。tagwrite の overlay 解消も同じ）、外部のタグ変更を取り込んだ行は
      `rg_written_at` を判定し直す（`sync_written_at`。`Scanner::with_replaygain_reference`）。
      受け入れ: `tests/rg_write.rs`（音声差し替えで NULL・タグだけの変更は据え置き、RG タグの削除 /
      一致する書き込みで `rg_written_at` が動く、tagwrite の conflict で読んだ音声差し替え）
- [x] **P1-12** UI の再構成と起動導線（foobar2000 のレイアウトに寄せる。D-58）。P1 の完了条件
      「foobar2000 を開かずに日常運用が回る」に対して、API だけで UI が無い機能（リネーム / 正規化 /
      RG / FLAC 検査 / GC）の起動導線と、ジョブ・設定画面を揃える
      - [x] (a) レイアウト: ヘッダ → プレイヤーバー（下部バーを上へ）→ 左（ツリー + プレイリスト +
            固定フィルタ / アルバムアート）・右（プロパティ領域 / 表）。境界はドラッグで可変・永続化。
            ツリーの表示形式（パターン。組み込み 4 + ユーザ定義）、`filter.album_ids`、既定列
      - [x] (b) プロパティタブ（Metadata / Location / General、共通値、ダブルクリック編集）と
            `GET /api/tracks/:id` の `detail`
      - [x] (c) 操作タブ（リネーム / 正規化の preview → 適用、RG 解析 / 書き込み、FLAC 検査、
            プレイリストへ追加）
      - [x] (d) ジョブ画面（SPEC §12.5）。`GET /api/jobs` に `concurrency`（種別ごとの並列度）を追加
      - [x] (e) 設定画面（SPEC §12.6）: `GET /api/config`、再スキャン / deep scan、GC preview → 実行、
            `GET /api/archive`

      受け入れ: `web/src/lib/tree.test.ts`（パターンのパース・ツリーの構築・`album_ids`）、
      `web/src/lib/properties.test.ts`（共通値の畳み込み）、`web/src/lib/operations.test.ts`（件数の
      メッセージと 409 の日本語化）、`web/src/lib/jobs.test.ts`（種別集計・絞り込み）、`tests/jobs.rs`
      （`concurrency`）、`web/src/lib/settings.test.ts`（GC preview の表）、`tests/tracks_query.rs`（`album_ids`）、
      `tests/tracks_api.rs`（`detail`）、`tests/config_api.rs`、`tests/archive_api.rs`。
      各段階で clippy / test / build / lint を通し、(a) はスクショで確認する

---

## P2 — CD 取り込み

完了条件: **新規 CD が検証付きで取り込め、既存 FLAC が格付けされる。**

- [ ] **P2-1** ドライブ制御（デバイス割当、`CDROM_DRIVE_STATUS` ポーリング、eject）
- [ ] **P2-2** TOC 取得と各種 DiscID 算出（MusicBrainz / AccurateRip / FreeDB）
  - [ ] TOC 取得（`cdrdao read-toc` / SG_IO READ TOC → `Toc`）。ドライブが要る
  - [x] ID 算出（`src/cd/toc.rs`。ドライブ不要）: `Toc`（LBA、データトラックのフラグ、リードアウト）から
        MusicBrainz DiscID と `?toc=` 文字列、FreeDB ID、AccurateRip id1 / id2、CTDB の TOC 文字列と TOCID、
        CRC 用の `TrackLayout`。§7.3 の `from_audio_sample_counts`（588 の倍数でなければ拒否）。
        Enhanced CD は音声部分の終端をデータトラック開始 − 11400 とし、MB / CTDB はデータトラックを
        数えない。AccurateRip は CUETools 式（id1 / id2 は音声だけ、リードアウトは実値、FreeDB は
        データトラックも数える）を既定とし、データトラックを落とした `audio_session()` から作る
        libdiscid 式の ID も AccurateRip DB に別キーとしてあることを実サーバで確認した。
        受け入れ: `tests/cd_toc.rs`（MB ドキュメントの 6 トラック例と CD-Extra 例、Nevermind、
        Hybrid Theory JP Enhanced CD。DiscID は MB ドキュメント / `ws/2` で、AccurateRip ID は
        実サーバの `dBAR-*.bin` で照合した値。再構成、不正な TOC の拒否、u32 境界）
- [x] **P2-3** MusicBrainz 照会（UA 必須、1req/s）と候補選択 UI。D-64。`src/cd/musicbrainz.rs`
      （`MusicBrainzClient`: DiscID → 404 なら `?toc=` の fuzzy、間隔待ち、503 の再試行。`parse_lookup`:
      リリース × medium の候補、exact / 近似）、`Toc::parse`（CTDB 形式 / MusicBrainz 形式）、
      `POST /api/cd/lookup { toc }`、設定 `[musicbrainz].url`。UI は CD タブ（`CdView` / `useCdLookup` /
      `lib/cd.ts`）: ドライブが無い間は TOC の貼り付け（`cdrecord -toc` の出力も可）→ 候補一覧（DiscID 一致 /
      近似、要約、曲数、長さ）→ 選択でトラック対応。受け入れ: `tests/cd_musicbrainz.rs`（実応答フィクスチャ
      tests/fixtures/mb/ の解釈: exact / Enhanced CD / fuzzy / 0 件 / joinphrase / フォールバック、ローカル HTTP で
      DiscID → toc の順・UA・inc・503 の再試行・間隔）、`tests/cd_lookup_api.rs`、`tests/cd_toc.rs`（文字列の往復）、
      `web/src/lib/cd.test.ts`。残り: 検出（P2-1）で入力源を差し替える
- [x] **P2-4** **照会ゼロ件でも完走できる手入力経路**とトラックリスト貼り付け。D-65。候補も手入力も
      同じフォーム（`DiscDraft`。`lib/cd.ts` の `draftFromCandidate` / `emptyDraft`）に写して直し、確定で
      `DiscMetadata`（`validateDraft` / `finalizeDraft`。P2-5 / P2-8 の入力）。行は TOC の音声トラックと 1:1
      （`POST /api/cd/lookup` の応答に `tracks: [{ number, length_ms }]`。`Toc::audio_track_sectors`）。
      貼り付けの行解析は `web/src/lib/tracklist.ts`（番号・時間・アーティストの区切り・表・見出し）で、
      番号で行に写す（`applyTracklist`。行数の違い・TOC に無い番号・未設定の行を警告）。UI は `CdView` の
      フォーム（候補ゼロ件なら空のフォームに直行、「候補を使わず手入力」、「空のタイトルを Track NN で埋める」、
      確定後はタグ名で表示。遷移は `lib/cdState.ts` の reducer）。受け入れ: `web/src/lib/tracklist.test.ts`
      （番号の形 10 種と全角、年 / 100 以上は番号にしない、番号の重複・飛びの警告、見出し、時間の形と全角、
      区切りの優先と端・ぶら下がり、artistFirst、タブ区切り）、`web/src/lib/cd.test.ts`（候補の写し・空フォーム・
      貼り付けの適用と警告・検証・埋め・確定・タグ名の写像）、`web/src/lib/cdState.test.ts`（照会 → 選択 / 手入力 →
      編集・貼り付け → 確定 → TOC 編集 / reset で下の段が消える）、
      `tests/cd_toc.rs` / `tests/cd_lookup_api.rs`（`tracks`）。残り: 検出（P2-1）で TOC の入力源を差し替え、
      吸い出し（P2-5）で `DiscMetadata` を受ける
- [ ] **P2-5** 吸い出し（全ディスクを 1 本の PCM として取得 → オフセット適用 → 分割）
- [x] **P2-6** ARv1/v2 CRC と CTDB CRC32（先頭・末尾トラックの除外規則に注意）。`src/cd/`
      （`TrackLayout` にサンプルを順に流すストリーミング計算。吸い出し PCM と既存 FLAC のデコード結果の
      両方に同じ形で使う。80 分のディスクで 0.3 秒）。定義は CUETools の `AccurateRip.cs` / `CDRepair.cs`
      に合わせた: ARv1 は先頭トラックの頭 5×588−1、末尾トラックの尻 5×588 を除外（非対称）、
      ARv2 は 64 bit 積の上位も加算、AccurateRip DB のプレス違い検出に使う crc450 も併せて算出。
      CTDB は zlib CRC32 で、ディスクは頭 10 セクタと尻 (10 セクタ + 総数 mod 5880)、トラックは先頭の頭と
      末尾の尻に同じ除外。受け入れ: `tests/cd_crc.rs`（手計算できる閉じた式での除外規則、v2 の上位加算、
      語の詰め方、crc450、細切れ push の一致、奇数長の持ち越しと L 余りの拒否、サンプル数の過不足）と、CUETools の定義から独立に書いた
      Python 実装（`scripts/gen_cd_crc_fixture.py` → `tests/fixtures/cd_crc_reference.json`）との突き合わせ
- [x] **P2-7** CTDB 照会・修復適用、AccurateRip は補助。照会は P2-9 で済み。修復は `src/cd/repair.rs`
      （D-66。CUETools `CDRepair` / `RsDecode` / `Parity2Syndrome` の定義: GF(2^16) 0x1100B、stride 11760 語、
      `SyndromeSampler` → `SyndromeTable`（80 分 3〜5 秒）、`find_offset`（列 0、±2939）、`plan`（BM → Chien →
      Forney、列あたり npar/2 個まで、直した後の CRC が合うときだけ）、`RepairApplier`（2 回目の走査で XOR）、
      `decode_entry_syndrome`（`syndrome` / 旧 `parity` 属性）、`DbSyndromes::parse`（面順）)と
      `CtdbClient::fetch_syndromes`（`hasparity` を Range で先頭 npar 面）。受け入れ: `tests/cd_repair.rs`
      （表を使わない GF 演算との一致、直接の定義との一致、ずらした列の再計算との一致、LFSR パリティ →
      シンドローム、実応答の `syndrome` 属性、面順、列ごとの能力内の修復とオフセット付き修復、能力超え、
      CRC 不一致、範囲外の誤り、乱数ストレス、本番 stride で 1 セクタ丸ごと）、`tests/cd_lookup.rs`
      （Range の 206 / 200、列 0 の検証、npar 超え、404。実サーバは `#[ignore]`）。残り: 吸い出し（P2-5）で
      2 回の走査に配線し、直せなければ再リップ / `mismatch`
- [x] **P2-8** エンコードと配置、`rip.log` / `disc.cue` / `disc.toc` の出力。D-67。
      `src/cd/place.rs`（`place_disc`: `DiscMetadata` の検証 → トラックごとの PCM MD5 → `pathgen::plan`
      （category 無しは `unsorted`、複数枚組は `multi_disc`。2 枚目以降は宛先の同名 album に合流）→
      raw PCM を `flac -N --verify --skip/--until` で tmp へ（STREAMINFO の MD5 が PCM と一致するときだけ）
      → lofty でタグ → `library` の排他 → tmp + `RENAME_NOREPLACE` で配置 → 1 トランザクションで `albums` /
      `tracks`（`source_type = cd_rip`、`verification`）/ `track_tags` / `album_verifications`（`source = rip`）/
      `track_verifications` → `rg` と `transcode` を投入。再実行は MD5 で自分の成果物を見分ける）、
      `src/cd/metadata.rs`（web の `DiscMetadata` と同じ形 + `category`。検証とタグ写像）、
      `src/cd/riplog.rs`（`RipReport`、`rip.log` / `disc.cue` / `disc.toc` の描画。複数枚組は `disc<N>.*` /
      `rip<N>.log`）、`GET/POST /api/categories` と確定フォームの category（`useCategories`）。
      D-43 の残課題: rename ジョブが commit 後に、album 全体の移動で active な行が無くなった旧ディレクトリの
      既知の同梱ファイルを宛先へ移し、空なら rmdir（`edit::rename::follow_companions`）。スキャナは spindle の
      rip.log（先頭行の署名）があるディレクトリの新規行を `cd_rip` にする。verify.log は `data/verify` のまま。
      受け入れ: `tests/cd_place.rs`（配置・タグ・同梱 3 ファイル・DB 行・後続ジョブ・次のスキャンで不変、
      not_attempted、複数枚組の合流、別リリースの `({year})` 降格、同じ盤の再実行の冪等性と別音声の衝突、
      配置後に落ちてスキャナが拾った行の採用、排他中の Busy、PCM 長 / メタデータの拒否）、
      `tests/cd_metadata.rs`、`tests/cd_riplog.rs`、`tests/scanner.rs`（rip.log → cd_rip）、`tests/rename.rs`
      （同梱ファイルの追随・衝突・巻き戻し）、`tests/categories_api.rs`、`web/src/lib/cd.test.ts`。
      残り: P2-5 で `PlaceEnv` を `AppState` から組み立てて `place_disc` を配線する（`Busy` は Requeue、
      `Conflict` は最終失敗で tmp の PCM を残す）。`POST /api/cd/rip` も P2-5
- [x] **P2-9** 遡及照合（44.1/16/2ch かつサンプル数が 588 の倍数のときのみ）。D-63。
      `verify` ジョブ（album 単位、並列 2。`src/jobs/handlers/verify.rs`）: ディスクごとに STREAMINFO の
      サンプル数から TOC を再構成 → デコードして CRC 表（`src/cd/crctable.rs`。1 回流して ±2939 の
      全オフセットの CRC が出る）→ CTDB / AccurateRip に照会（`CtdbClient` / `AccurateRipClient`。
      **P2-7 の照会部分はここで実装済み**、残りは修復適用）→ オフセットを探して照合
      （`src/cd/verify.rs`）→ `album_verifications`（手法 × ディスク。migration 0013 で `disc_no` と、
      再実行の冪等キー `job_id`）/ `track_verifications` / `tracks.verification` と
      `data/verify/<album_id>.log`（1 トランザクション。`audio_version` と fstat の再照合が通るときだけ）。
      `POST /api/verify { selection }` と操作タブの「遡及照合」、設定 `[verify]`（照会先の URL）。
      受け入れ: `tests/cd_crctable.rs`（ずらした列への直接計算と全オフセットで一致、crc32 combine）、
      `tests/cd_lookup.rs`（実サーバから保存した bin / XML の解釈、ローカル HTTP でパス・クエリ・
      UA・404）、`tests/cd_verify.rs`（合成エントリでのオフセット検出、壊れたトラック、候補の絞り込み）、
      `tests/verify_job.rs`（ffmpeg で作った FLAC で verified / offset / AR のみ / mismatch / not_found /
      unverifiable / 不完全 / 複数ディスク / 照会失敗 / 再照合の履歴 / 照合中の版更新・差し替え・
      キャンセル / 原子性 / ログ確定失敗の巻き戻し / commit 後の再実行の冪等性）、`tests/verify_api.rs`
- [x] **P2-10** Inbox 取り込み（ステージング → 承認キュー → 配置）。D-68。
      `src/import/inbox.rs`（`scan_inbox`: `[paths].inbox` を走査して音声のあるディレクトリを 1 件として
      `inbox_items` / `inbox_files` に登録（root 直下は rel_dir ""）。ファイルが変われば読み直して pending に
      戻し、消えた件は行ごと消す。`proposal`: タグから下書き（albumartist / album / date / category は
      genre 写像、トラックは DISCNUMBER / TRACKNUMBER / TITLE / ARTIST、無ければファイル名順）。
      `InboxDraft::problems`: 承認の検証。`place_item`: 承認済みの件を `library` の排他の下で配置
      （MUSICBRAINZ_ALBUMID → 自分の成果物の album のキー → `inbox:<id>` のリリースキーで `pathgen::plan` →
      tmp + `RENAME_NOREPLACE`、補正はファイルのタグに書く → 1 トランザクションで `albums` / `tracks`
      （`source_type = download`）/ `track_tags` → Inbox 側を消す → `rg` / `transcode`、wav / alac / aiff は
      `[normalize].wav_to_flac` なら normalize バッチも投入。再実行は音声の指紋で自分の成果物を見分ける）、
      `src/import/placement.rs`（CD の配置と共通の tmp + rename / album の解決 / 行の登録。リリースキーは
      登録トランザクションで再検証）、`src/jobs/handlers/inbox.rs`（`inbox` ジョブ = 走査 + 承認済みの配置。
      `[inbox].poll_interval_secs`（既定 60、0 で自動なし）の周期投入と「今すぐ確認」）、`db/migrations/0014`、
      `src/db/inbox.rs`、`src/api/inbox.rs`（`GET /api/inbox`、`POST /api/inbox/scan`、
      `POST /api/inbox/{id}/approve|reject|reopen`）、web の Inbox タブ（`lib/inbox.ts` / `useInbox` /
      `InboxView`: 件の一覧とアルバム単位 + トラック単位の補正フォーム、placed からアルバムへ）。
      受け入れ: `tests/inbox_job.rs`（検出、変更の読み直しと承認の取り消し、消えた件、placed の期限切れ、
      補正付きの配置と同梱ファイル・DB 行・後続ジョブ・Inbox の消費、wav の normalize 投入の有無、
      衝突 → failed と後始末、排他が取れないときの再投入、配置中の変更、再実行の冪等性、placing のまま
      落ちた件の回復、コピー前の差し替えの検出、登録前 / 登録後に落ちた後の完了、placed のディレクトリに残った音声、normalize の投入と登録の原子性）、
      `tests/inbox_draft.rs`、`tests/inbox_db.rs`、`tests/inbox_api.rs`、`web/src/lib/inbox.test.ts`

---

## P3 — ytmusic 統合

完了条件: **ytmusic CLI を廃止できる。**

- [x] **P3-1** タイトルパーサ → **メタデータプラグインのプロトコル v1**。D-69。タイトルの慣習は spindle に
      置かず、外部コマンド（`[ytmusic].metadata_command`。参照実装 `AkashiSN/spindle-ytmusic-meta`。Python 版
      ytmusic のテスト 98 件から生成したフィクスチャで一致を保証）に JSON で問い合わせる。
      `src/import/ytmusic/metadata.rs`（`Request` / `Response` / `Track`、`MetadataProvider::resolve`:
      引数配列で起動 → stdin に Request → stdout の Response を検証。タイムアウト・非ゼロ終了・不正な JSON・
      必須の値の欠落は `ProviderError`、`ok: false` は `Outcome::Declined { reason, message }`）。
      受け入れ: `tests/ytmusic_metadata.rs`（偽のプラグインで: 往復と stdin の内容、`ok: false` の各 reason、
      非ゼロ終了、不正な JSON、プロトコル違い、空の必須値、タイムアウト、起動失敗）、`tests/config.rs`、
      `tests/docker_context.rs`（`include_str!` / rust-embed の埋め込み元が Dockerfile の build stage に
      COPY されている）。`ExternalCommand::stdin_bytes`（stdin へ書いて閉じる）を追加
- [x] **P3-2** チャンネル定義とカテゴリ写像 → チャンネル定義はプラグイン側（D-69）。spindle は `Track` を
      タグ（`Track::tags(track_no)`: TITLE / ARTIST 多値 / ALBUM / ALBUMARTIST / DATE / TRACKNUMBER + 追加タグ）と
      `pathgen::TrackFields`（`Track::track_fields`）に写し、`category` は `db::categories::ensure` で無ければ
      語彙に追加する。受け入れ: `tests/ytmusic_metadata.rs`（写像）、`tests/categories_api.rs`（ensure）
- [x] **P3-3** ダウンローダ（yt-dlp を subprocess）→ **Inbox に置くところまで**（D-70。配置・採番・後続は Inbox）
      - [x] ジョブ基盤: `JobError::Fatal`（バックオフせず `failed`）、`JobType::Ytdl`（並列 1）
      - [x] `[ytmusic].download_timeout_secs`、起動時診断（`metadata_command[0]` と yt-dlp の実行可否を警告）
      - [x] サイドカー `spindle-inbox.json` の読み書き（`import/ytmusic/sidecar.rs`。merge は tmp + rename）
      - [x] Inbox: 既存 album の採用（MB キー無し同士）、TRACKNUMBER 無しの採番（max + 1 から名前順）、承認と
            登録トランザクションでの `(disc_no, track_no)` の重複検証、`destination` / `source` の応答、
            サイドカーの category 提案と配置成功時の削除、pending に戻った件の下書き merge
      - [x] `downloader.rs` + `handlers/ytdl.rs`: dump（playlist 展開）→ SOURCE_URL の重複 → プラグイン →
            download → remux → タグ（PICTURE / SOURCE_URL）→ Archive/youtube/<id>.webm → Inbox + サイドカー → inbox 投入
      - [x] `POST /api/ytmusic/download`、操作タブの「YouTube」節、Inbox タブの宛先表示と判定バッジ / message
      - [x] Dockerfile に deno（yt-dlp の JS ランタイム）。compose のプラグインのマウント例
      - [x] 受け入れ: `tests/ytmusic_download.rs`（偽 yt-dlp（bash）+ 偽プラグインで ok / unmatched / skip /
            playlist / 重複 / Fatal と Failed の区別、dump の解釈、ファイル名。実 yt-dlp の通しは `#[ignore]`）、
            `tests/ytmusic_api.rs`、`tests/ytmusic_sidecar.rs`、`tests/inbox_job.rs`（追記・番号の再検証・
            サイドカーの削除）、`tests/inbox_api.rs`（destination / source / 採番 / 400 / merge）、
            `tests/inbox_draft.rs`、`tests/jobs.rs`（Fatal）、`tests/config.rs`（起動時診断）
- [x] **P3-4** ~~ytmusic ダウンロード後の Derived 投入~~ → **配置直後のアートワーク解決**に縮小。Opus 原本に
      Derived は無く（非可逆 → 非可逆禁止）、rg は Inbox の配置で投入済み。残っていた「次のスキャンまで画像が
      出ない」を、配置の直後にその album だけ解決して thumbnail を投入する形で埋めた
      （`scanner::resolve_album_artwork_now`、`PlaceItemEnv.artwork`。D-68 追記）。受け入れ:
      `tests/inbox_job.rs`（埋め込み画像から解決して thumbnail 投入、画像なしは「なし」で解決）
- [ ] **P3-5** 偽ハイレゾ検出（`rustfft`、任意機能。SPEC §7.10、D-71。表示と絞り込みだけで、判定を消費する
      自動処理は無い）
      - [ ] `media/hires.rs`: `HiresSink`（PcmSink。Hann 8192 / ホップ 8192 の FFT をチャンネルごとに累積、
            無音フレーム除外、サンプルの OR）→ `Measurement { cutoff_hz, cliff_db, effective_bits }` →
            `[hires]` のしきい値で `Status`。合成信号の単体テスト（brickwall → upsampled、緩やかな
            ロールオフ → inconclusive、全帯域 → ok、下位 8 bit ゼロ → padded、無音 → inconclusive）
      - [ ] `db/migrations/0016_hires_check.sql`: `tracks.hires_check*` + `hires_cutoff_hz` / `hires_effective_bits`、
            `jobs.type` に `hirescheck`（0015 と同じ表の作り直し）。`db/hires.rs`（Status / Target / record /
            enqueue_all_unchecked / dedup_key）
      - [ ] `jobs/handlers/hirescheck.rs`（並列 = CPU コア数、stale ゲート、track_locks、fstat 照合 → decode →
            版付き record）。`[hires]` 設定。スキャン commit での自動投入。`POST /api/hirescheck`
      - [ ] `tracks` の行に `hires_check`、`Flag::HiresUnchecked` / `HiresSuspect`、DSL の `hirescheck` /
            `cutoff` / `effectivebits`
      - [ ] UI: バッジ、プロパティ（判定 + 計測値）、フィルタ、操作タブの再投入
      - [ ] 受け入れ: `tests/hires_check.rs`（合成 FLAC でジョブを通す。差し替え済みの Skipped、版が進んだ no-op、
            対象条件）、`tests/hirescheck_api.rs`、`tests/config.rs`、`web/src/lib/*.test.ts`

---

## 着手前に確認が必要な残課題

- Discogs / VGMdb 連携の要否（P3 以降）
- `.fpl` 書き出しの要否（P4、非推奨）
- ~~Inbox のポーリング間隔~~（P2-10 で決めた。60 秒 + 手動。D-68）
- ~~一括リネーム後の旧ディレクトリに残る同梱ファイル（cover.jpg / disc.cue / rip.log）と
  空ディレクトリの扱い~~（P2-8 で決めた。D-67）

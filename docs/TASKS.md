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
- [x] `POST /api/rename/preview` / `POST /api/rename/apply`（SPEC §9。UI は P1-12 の操作タブ）

受け入れ: 単体テストで置換テーブル・切り詰め・衝突降格を網羅。
既存の ytmusic 出力と同じパスが再現できる。
大小文字だけ違う 2 つのファイル名への一括リネームが衝突として検出される。
A↔B の swap と 3 件の循環リネームが完了し、phase 1 直後に kill しても再起動で完了する。
（`tests/pathgen.rs` / `tests/rename.rs` / `tests/rename_api.rs`）

~~未決: album 全体を動かした後の旧ディレクトリに残る同梱ファイルの追随~~（P2-8 で決めた。
`edit::rename::follow_companions`、D-67）

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
  起動した。P2-1 でアプリ側は「デバイスが無くても起動して no_drive を出す」にし、compose は
  ドライブの無い機体で 3 行を消す運用（OPERATIONS.md）にした。compose で optional にする手段は無い

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

### P1-0 初回 deep scan の高速化

P0-14 の後続課題。リリース時の再移行でも効く（D-50）。
`Scanner` Phase 2 の直列 `audio_md5` を、既存行が md5 で突き合わせを要するときだけ計算するか
Phase 3 の並列読みへ回す。Phase 2 中も進捗を出す。`symphonia` / `lofty` のファイルごとの
WARN を既定フィルタで落とす（ALAC 7,572 本で 75 分 → 並列度分だけ短縮が目安）。

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

### P1-1 ReplayGain スキャン

`ebur128`、album は `album_id` 単位、2ch 以外は集計から除外（D-47）。

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

### P1-2 RG タグ書き込み

Opus のみ -23 LUFS 基準の Q7.8: `round((G18 - 5.0) * 256)` を符号付き 16bit に飽和。SPEC §6 の
テストベクトルを単体テストに置く。`rg_scanned_at` と `rg_written_at` を分離（D-48）。

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

### P1-3 アートワーク

読みは埋め込み / 同梱画像の両対応、書きは埋め込み統一で一括差し替え、WebP サムネイル生成と
キャッシュ。アルバムグリッド画面 → クリックで表を `album_id` に絞る。

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

受け入れ: `tests/artwork.rs`（名前の優先順、判別、埋め込みの選択、キャッシュの配置）、
`tests/artwork_scan.rs`（同梱 > 埋め込み、最初のトラック、無し → NULL、名前の優先順、同梱画像の
差し替え / 削除の検出、未解決 album の解決、deep、同じ画像の共有、読めない同梱画像、missing、
画像付きトラックの移動で新旧 album を解決、commit 後の cancel で run は completed のまま次回再開、
キャッシュ書き込み失敗 / 読めないトラックで状態を動かさない、原画像の欠損を incremental で復旧）、
`tests/thumbnail_job.rs`（寸法・アスペクト比・拡大なし・冪等・原画像なし）、`tests/artwork_api.rs`

受け入れ（書き側）: `tests/picture_write.rs`（FLAC / Opus / MP4 への差し替えと `tag_version` +1、旧画像の
退避、同じ画像は差分なし、巻き戻しで旧画像が戻る、外部変更で conflict、キャッシュ欠損で failed、
AlreadyMatches、album の予約）、`tests/artwork_api.rs`（upload の形式判定・上限・400、embed の 404 / 409）、
`tests/gc.rs`（`edits` が参照する画像は残す、行の 24 時間の猶予）、`web/src/lib/operations.test.ts`、
`web/src/lib/artwork.test.ts`（`PICTURE` 値の分解、アップロードの要約）。ローカル起動で upload →
差し替え → 履歴のサムネイル → 巻き戻しを agent-browser で確認済み（2026-09-18）

### P1-3c トラック単位のアートワーク

D-61。トラックごとに画像が違う album 向け。

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

### P1-4 ロスレス → FLAC 正規化

WAV / ALAC / AIFF（D-45 / D-46）。変換前後の PCM MD5 照合。不一致なら中止。一致時は元ファイルを
`Archive/` へ move し `edit_ops(kind='archive')` と `archived_files` 台帳に記録。**即時削除しない**。
`audio_version` は据え置き。移行で取り込んだ ALAC 7,572 本が主対象。

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

### P1-5 FLAC 健全性チェック

`flac -t`、MD5 未設定の補填。補填時は `audio_version` 据え置き（D-57）。**補填は P1-5b に切り出し**
（実データに FLAC が無く、今後の FLAC は自前の `flac -8 --verify` と CD リップで MD5 が付く）。

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

### P1-5b FLAC の MD5 補填

`flac_fix_missing_md5`。`md5_missing` のトラックを選んで、デコードした PCM MD5 を STREAMINFO に書く
編集バッチ（D-57 / D-59）。

- [x] `edit_ops.kind` に `md5` を足すマイグレーション 0011。旧値 = 全ゼロを `edits` に残して巻き戻し可、
      `audio_version` 据え置き、inode / mtime は追随
- [x] `POST /api/md5fill`、操作タブの「MD5 を補填」

受け入れ: `tests/migrations.rs`（0011 で参照行が残る）、`tests/fingerprint.rs`（MD5 の位置）、
`tests/md5fill.rs`（補填 / 対象外 / 巻き戻し / conflict）、`tests/md5fill_api.rs`（409 の各コード）

### P1-6 プレイリスト

手動、並べ替え、m3u8 書き出し（D-53）。

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

### P1-7 スマートプレイリスト

`docs/DSL.md`。pest → AST → SQL（D-54）。

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

### P1-8 エクスポートプロファイル

foobar / android / internal、foobar Autoplaylist クエリ生成（D-55）。**依存: P1-10**（`delivery`
プロファイルが Derived を前提）。タグ鮮度が必要な export / 同期は `stale_tags` の件数を明示するか
追随ジョブの完了を待つ。

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

### P1-9 再生

Range 対応、ALAC は既定で Opus 変換、`canPlayType()` によるクライアント能力判定。下部バー左側の
再生 UI: 再生・停止・シーク・音量・RG の off / track / album（D-52）。

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

### P1-10 Derived 自動生成と追随

`audio_version` / `tag_version` 差分判定、Library の移動・削除への追随。`delivery` ビューの版一致
フォールバックの結合テスト。P1-8 の `delivery` プロファイルと Android 同期がこれを前提にするため
P3 から前倒し（D-51）。

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

### P1-11 GC ジョブ

`missing_since` が保持期間（`[gc].retention_days`、既定 7 日。2026-09-25 に 30 から変更）を超えた行、`Archive/` へ退避した WAV、Derived の孤児。**物理削除を行う唯一の経路**。
dry-run と削除件数のログを必須にする（D-56）。

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

### P1-13 スキャナの ReplayGain 追随

D-47 / D-48 の未決。

- [x] 外部で音声が差し替わった行は解析値を捨てる（`reset_analysis`。tagwrite の overlay 解消も同じ）
- [x] 外部のタグ変更を取り込んだ行は `rg_written_at` を判定し直す（`sync_written_at`。
      `Scanner::with_replaygain_reference`）

受け入れ: `tests/rg_write.rs`（音声差し替えで NULL・タグだけの変更は据え置き、RG タグの削除 /
一致する書き込みで `rg_written_at` が動く、tagwrite の conflict で読んだ音声差し替え）

### P1-12 UI の再構成と起動導線

foobar2000 のレイアウトに寄せる（D-58）。P1 の完了条件「foobar2000 を開かずに日常運用が回る」に
対して、API だけで UI が無い機能（リネーム / 正規化 / RG / FLAC 検査 / GC）の起動導線と、ジョブ・
設定画面を揃える。

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

### P2-1 ドライブ制御

- [x] デバイス割当、`CDROM_DRIVE_STATUS` ポーリング、eject（`src/cd/device.rs`。`Drive` トレイト +
      `LinuxDrive`（ioctl）+ `DriveMonitor`（ポーラの状態）+ `spawn_poller`（2 秒。main で `[rip].device` から
      配線。デバイスが無くても起動は止めず no_drive）。`GET /api/cd/status` / `POST /api/cd/eject`。
      UI は `useCdDrive`（CD 画面を開いている間 2 秒間隔）→ 新しいディスクの TOC が出たら `lookupToc` で
      自動照会、状態の一行と「取り出す」（`CdView`）
- 実機（Pioneer BDR-209M、kernel 6.12）で分かったこと: CDROMEJECT は先に `CDROM_LOCKDOOR 0` を叩かないと
  ドライブが CHECK CONDITION で拒み、しかも戻り値が 2（SCSI status。負でない）なので成功に見える。
  unlock を前置し、戻り値 0 以外を失敗にした。`cdrdao read-toc` はサブチャネル解析込みで 9 分の
  ディスクに 1 分近くかかるので状態表示には使わない（ioctl の READ TOC は 40 ms）。SG_IO は `/dev/sr0`
  に直接通る（cdrdao が `/dev/sg` 無しで動いた）ので compose から `/dev/sg0` と `c 21:*` を外した

受け入れ: `tests/cd_device.rs`（READ TOC エントリ → `Toc`、ポーラの遷移: DiscOk で 1 回だけ読む・抜かれたら
捨てて次のディスクで読み直す・読めなければ理由を持って次周回に再試行・開けなければ no_drive。実ドライブは
`#[ignore]`: `sudo -u ubuntu -g cdrom target/debug/deps/cd_device-* --ignored`）、`tests/cd_status_api.rs`
（フェイクのドライブで status / eject / 503 / CSRF）、`web/src/lib/cdDrive.test.ts`（状態の一行、新しい
ディスクの判定）。実機で ディスクあり → `/api/cd/status` → lookup（MB の fuzzy 候補）→ eject を確認

### P2-2 TOC 取得と各種 DiscID 算出

MusicBrainz / AccurateRip / FreeDB。

- [x] TOC 取得（`CDROMREADTOCHDR` / `CDROMREADTOCENTRY` ioctl → `toc_from_entries` → `Toc`。P2-1 と同じ
      `src/cd/device.rs`。cdrdao / SG_IO は使わない）
- [x] ID 算出（`src/cd/toc.rs`。ドライブ不要）: `Toc`（LBA、データトラックのフラグ、リードアウト）から
      MusicBrainz DiscID と `?toc=` 文字列、FreeDB ID、AccurateRip id1 / id2、CTDB の TOC 文字列と TOCID、
      CRC 用の `TrackLayout`。§7.3 の `from_audio_sample_counts`（588 の倍数でなければ拒否）。
      Enhanced CD は音声部分の終端をデータトラック開始 − 11400 とし、MB / CTDB はデータトラックを
      数えない。AccurateRip は CUETools 式（id1 / id2 は音声だけ、リードアウトは実値、FreeDB は
      データトラックも数える）を既定とし、データトラックを落とした `audio_session()` から作る
      libdiscid 式の ID も AccurateRip DB に別キーとしてあることを実サーバで確認した

受け入れ（ID 算出）: `tests/cd_toc.rs`（MB ドキュメントの 6 トラック例と CD-Extra 例、Nevermind、
Hybrid Theory JP Enhanced CD。DiscID は MB ドキュメント / `ws/2` で、AccurateRip ID は
実サーバの `dBAR-*.bin` で照合した値。再構成、不正な TOC の拒否、u32 境界）

### P2-3 MusicBrainz 照会と候補選択 UI

UA 必須、1req/s（D-64）。

- [x] `src/cd/musicbrainz.rs`（`MusicBrainzClient`: DiscID → 404 なら `?toc=` の fuzzy、間隔待ち、503 の
      再試行。`parse_lookup`: リリース × medium の候補、exact / 近似）、`Toc::parse`（CTDB 形式 /
      MusicBrainz 形式）、`POST /api/cd/lookup { toc }`、設定 `[musicbrainz].url`
- [x] UI は CD タブ（`CdView` / `useCdLookup` / `lib/cd.ts`）: ドライブが無い間は TOC の貼り付け
      （`cdrecord -toc` の出力も可）→ 候補一覧（DiscID 一致 / 近似、要約、曲数、長さ）→ 選択でトラック対応

- [x] DiscID 以外の識別経路（D-64 追記。2026-09-22）: ドライブが TOC と同じ回に ISRC / MCN を読み
      （`cd/device.rs` の `read_ids`、SG_IO READ SUB-CHANNEL。`GET /api/cd/status` の `isrcs` / `mcn`）、
      `POST /api/cd/lookup { toc, isrcs?, mcn?, release? }` が TOC 近似 + ISRC 検索 + バーコード検索 + 指定
      リリースを束ねる（`MusicBrainzClient::lookup` / `DiscQuery` / `merge_candidates` / `MatchedBy`、
      `parse_release_ref`）。UI は候補のバッジを経路に、リリース URL / MBID の入力欄、`notes`、DiscID の
      登録リンク（`discidSubmissionUrl`）

受け入れ: `tests/cd_musicbrainz.rs`（実応答フィクスチャ tests/fixtures/mb/ の解釈: exact / Enhanced CD /
fuzzy / 0 件 / joinphrase / フォールバック / ISRC 検索 / バーコード検索 / リリース取得 / URL の解釈 / 束ねと
並び、ローカル HTTP で DiscID → toc の順・UA・inc・503 の再試行・間隔・複数経路の要求順と重複取得なし・
指定リリースの notes・exact 時の省略）、`tests/cd_lookup_api.rs`（ISRC + 指定リリースで当たる、null の
ISRC は捨てる）、`tests/cd_device.rs`（READ SUB-CHANNEL の解釈、TOC と一緒に 1 回だけ読む、読めなくても
TOC は成立。実機は `#[ignore]`）、`tests/cd_toc.rs`（文字列の往復）、`web/src/lib/cd.test.ts`（バッジ・
見出し・登録 URL）。実機（嵐「Five」。DiscID・トラック長とも未登録）で ISRC 経路から 2 盤が候補に出るのを確認

- [x] 接続の経路と診断（D-64 追記 2。2026-09-22）: `[musicbrainz].address_family`（auto / ipv6 / ipv4。
      `cd::select_addrs` + reqwest の DNS リゾルバ差し替え）、応答前に落ちた要求の 1 回の張り直し、
      `cd::error_chain` で原因の末端までログと API 本文に出す。**この回線は MetaBrainz の IPv4 が塞がれて
      いる**（TCP は通るが TLS で切られる。IPv6 は通る）ので実機の config.toml は `address_family = "ipv6"`

- [x] 画面をドライブ前提に（2026-09-22。ユーザ要望）: 状態の一行（トラック数・総時間）と「照会し直す」
      「取り出す」を主役にし、TOC の貼り付け・各種 ID・リリース URL 指定・用語の凡例は「詳細」へ。候補に
      MusicBrainz へのリンク・収録構成（`mediaSummary`。候補に `media` を追加）・ディスクとの長さ差を出し、
      CD 以外の medium（デジタル配信 / DVD / Blu-ray）は既定で畳む
- [x] 照会結果のキャッシュ（D-64 追記 3。2026-09-22）: 入力ごとに 10 分・8 件（`DiscQuery::cache_key`。
      失敗は覚えない）、`POST /api/cd/lookup` の `refresh` で引き直す。UI はボタンからの照会だけ refresh

受け入れ（接続）: `tests/config.rs`（既定 auto、ipv6 / ipv4、未知の値は拒否）、`tests/cd_musicbrainz.rs`
（`error_chain` の連結、`select_addrs` の絞り込みと空のときの扱い、最初の接続を閉じるサーバで 1 回張り直して
通る、キャッシュの再利用 / 入力違い / refresh / 期限切れ / 失敗は覚えない）、`tests/cd_lookup_api.rs`
（2 回目は上流を引かない、refresh は引く）

- [x] 照会の段階化（D-64 追記 4。2026-09-23。P4-20）: TOC 近似は DiscID / ISRC / バーコードで 1 件も
      候補が残らなかったときだけ引く。`widen` で明示的に広げられる

残り: なし（検出は P2-1 で差し替え済み）

### P2-4 照会ゼロ件でも完走できる手入力経路とトラックリスト貼り付け

D-65。候補を写す先は 1 つの下書き（`DiscDraft`。`lib/cd.ts` の `draftFromCandidate` / `emptyDraft`）。

**現在形**（P4-20 / D-65 追記 / D-67 追記で上書き済み。以下は履歴として残す）:
- CD 画面でできるのは**候補を選ぶ / 写す範囲を変える /「どれも違う」**だけ。**補正と
  トラックリスト貼り付けは Inbox の承認画面へ移管**した（取り込んだものは Inbox を通る）
- 「確定」の段と「空のタイトルを Track NN で埋める」ボタンは廃止。空のタイトルは表のプレースホルダで
  見せ、取り込むときに `Track NN` が入る
- **P2-5 に渡す契約は名前が空でもよい**（`DiscMetadata::validate` の必須検証は Library へ置くとき
  のもの。D-67 追記）

- [x] 行は TOC の音声トラックと 1:1（`POST /api/cd/lookup` の応答に `tracks: [{ number, length_ms }]`。
      `Toc::audio_track_sectors`）
- [x] 貼り付けの行解析は `web/src/lib/tracklist.ts`（番号・時間・アーティストの区切り・表・見出し）で、
      番号で行に写す（`applyTracklist`。行数の違い・TOC に無い番号・未設定の行を警告。P2-10 で Inbox 版に移した）
- [x] UI は CD 画面（候補ゼロ件でも、「どれも違う（候補を使わない）」でも取り込める。遷移は
      `lib/cdState.ts` の reducer）。P4-20 で表が主役になり、P4-20 追記で読み取り専用の
      `CdAlbumSummary` / `CdTrackTable` になった

受け入れ: `web/src/lib/tracklist.test.ts`（番号の形 10 種と全角、年 / 100 以上は番号にしない、番号の
重複・飛びの警告、見出し、時間の形と全角、区切りの優先と端・ぶら下がり、artistFirst、タブ区切り）、
`web/src/lib/cd.test.ts`（候補の写し・空フォーム・貼り付けの適用と警告・検証・`Track NN` の埋め・
タグ名の写像）、`web/src/lib/cdState.test.ts`（ディスク検出 → 照会 → 選択 /「どれも違う」、TOC 編集 / reset で
下の段が消える、照会中もフォームが消えない）、`tests/cd_toc.rs` / `tests/cd_lookup_api.rs`（`tracks`）

残り: なし（空名を許す契約は P2-5 で実装済み（D-67 追記）、トラックリスト貼り付けは P2-10 で Inbox の
承認画面へ移した（D-65 追記 2）。検出は P2-1 / P4-20 で差し替え済み）

### P2-5 吸い出し

**実装済み（2026-09-23）。残りは実機での照合成功の確認だけ**（下の「引き継ぎ」）。CD 取り込みは
CD 画面の「取り込む」→ rip ジョブ → Inbox → 承認 → Library まで通しで動く。

#### いまの状態（引き継ぎ。別セッションはここから読む）

- **コード**: `src/cd/rip.rs`（`read_disc` / `rip_disc` / `choose_offset` / `shift_pcm`）、
  `src/cd/place.rs`（Inbox の隠しディレクトリで組み立てて公開）、`src/cd/driveoffsets.rs`（AccurateRip の
  ドライブ表）、`src/jobs/handlers/rip.rs`、`src/api/cd.rs`（`POST /api/cd/rip`、status の `rip_job` /
  `drive`）、`src/import/sidecar.rs`（`RipEntry`）、`src/import/inbox.rs`（`bind_rip` と検証記録の登録）、
  web は `hooks/useCdRip.ts` / `lib/cdRip.ts` / `components/CdView.tsx`
- **判断**: DECISIONS の D-67 追記 2（Inbox 側・サイドカー・DiscID を入れない）、D-83 と追記・追記 2
  （オフセットは 設定 → 学習済み → AccurateRip のドライブ表 → 0。照合で見つけたずれを当てて学習）
- **レビュー**: codex レビュー済み（すべて LGTM）。コミットは `5df2dc5`〜`ad70ad3`（main、未 push）
- **テスト**: `cargo test` 1339 件、web 301 件。実ドライブのテストは `#[ignore]`
  （`sudo -u ubuntu -g cdrom target/debug/deps/<test>-* --ignored --exact <名前> --nocapture`。
  **`--exact` を付ける**。付けないと `real_drive_eject_opens_tray` まで走る）
- **実機で確認済み**（BDR-209M、2 トラックの盤 `0:20144:40290`）: 名前の無い盤を `POST /api/cd/rip` から
  Inbox まで通した（読み取り 2 回 → 修復は直せず → mismatch のまま `CD/[<DiscID>]` に pending、album gain
  on、SSE の `detail` が read / verify / repair / encode / place の順）。INQUIRY の型番は
  `PIONEER BD-RW   BDR-209M`、AccurateRip のドライブ表で **+667**（status が `offset_source: table`）
- **その盤は CTDB（信頼度 30、同じ TOC）とどのオフセットでも一致せず、2 回の吸い出しで中身が違った**
  （傷か読みの不安定）。**照合が通るところは実機でまだ見ていない**

- **照合が通らない原因の調査（2026-09-24）**: 「Five」は新品で、CTDB にも同じ TOC で載っている（信頼度 30）。
  原因は盤ではなく**ドライブの読み取りのずれ（ジッター）**。同じ範囲を 2 回読むと、読み取り要求の継ぎ目ごとに
  ±4 サンプルずれる。cd-paranoia（libcdio 2.2.0）を通さず SG_IO の READ CD（0xBE）で直接読んでも同じ。
  要求の大きさ（8 / 26 セクタ。上限 55）でも変わらず、速度指定（`CDROM_SELECT_SPEED` / `-S`）はドライブに
  無視される（約 15 倍速のまま）。paranoia は jitter 補正を 60 秒あたり 1,300〜2,400 回行い、`drift` / `dropped`
  （どちらも libcdio では「ずれを検出して補正した」通知。codex 指摘）も出す。それでも補正後の出力どうしが
  トラック内で最大数百サンプル違う（同じ範囲の 2 回の読み取りで最大 280 サンプル）ので、少なくとも一方はずれて
  いる → CRC がどのオフセットでも合わず、
  CTDB のパリティのオフセットも合わない（ログの「パリティとオフセットが合わないので修復しない」）。
  `cd-paranoia -A` の診断は「Drive tests OK with Paranoia」（キャッシュ 137 セクタ、先読み 119）。
  TrueNAS 側の spindle の状態ポーリングを止めても変わらない（ドライブの共有は原因ではない）。
- [x] **ずれの記録（対策案 A。2026-09-24）**: `read_disc` が paranoia の drift / dropped / duped（コード 7 / 10 / 11）を
  トラックごとに数える（`TrackRead.slips`）。試行ごとの合計は `RipReport.attempt_slips`、rip.log のトラック表に
  「ずれ」列と合計・「試行ごとのずれ」、照合が通らずずれがある件は承認画面の警告（`slip_warning`）。
  **吸い直しの判断は変えていない**（照合できる盤は今までどおり不一致で吸い直す。照合できない盤で drift の回を
  読み直すかは観測の後に決める）。
  **実機の観測（同日、「Five」）**: 試行ごとのずれは 450 / 344 / 627 回、最後の回はトラック 1 で 553 回・
  トラック 2 で 74 回。どのトラックも毎回数十〜数百回起きるので、案 B（通ったトラックを残し、通らないトラック
  だけ読み直す）でも照合が通る見込みは低い。**このドライブでは読み方の工夫より、ドライブ側（PureRead の設定・
  別のドライブ）の切り分けが先**。承認画面の警告が出ることも実機で確認した
- **未判定（2026-09-24。別の盤が無いので保留）**: ドライブの故障・個体差・設定のどれかは分かっていない。
  BDR-209M は EAC の判定で accurate stream 対応・精度上位とされる型番なので、この振る舞いは型番としては普通では
  ない。一方で読み取りエラー・C2 は出ず音も正常で、「壊れている」症状でもない。候補: (1) PureRead のモード
  （Standard / Master / Perfect。ドライブ本体に保存され、Windows の Pioneer ユーティリティでしか変えられない）、
  (2) この個体かファームウェア 1.30 の癖、(3) キャッシュ（137 セクタ）と cdparanoia の相性（ただし paranoia を
  通さない READ CD でもずれるので、これだけでは説明がつかない）。
  次に確かめること: 別の CD をこのドライブで 2 回読んで比べる（ずれればドライブ側）、この盤を別のドライブ
  （Windows + EAC）で読んで照合が通るか、PureRead の設定。比較の手順は scratchpad にあった `readcd.py`（SG_IO で
  READ CD を直接発行して同じ範囲を 2 回読み、8 KB の窓でずれ幅を測る）と同じことを `cd-paranoia -Z -r` でも行える:
  `cd-paranoia -Z -r -d /dev/sr0 -- "2[0:00.00]-2[1:00.00]" a.raw` を 2 回 → `cmp`
- **影響**: spindle はずれた吸い出しを「検証済み」にはせず mismatch として Inbox に置く（誤って正しいと言うことは
  無い）。ただしこのドライブでは照合が通らないので、P2 の完了条件「新規 CD が検証付きで取り込め」を実機で満たせて
  いない。補正後の出力が読むたびに数〜数百サンプル（最大 6 ms 程度）違うので、どの回もビット単位で正しいとは
  言えない（どれが正しいかは照合でしか分からない）

#### 引き継ぎ: 残り

- [ ] **実機での照合成功の確認**（ユーザに CTDB / AccurateRip に載っている傷の無い盤を入れてもらう）。
      期待: 表の +667 で吸い（`offset_source: table`）、照合が 1 回で通り（`verified_ctdb` か
      `verified_ar`、試行 1）、`drive_offsets` に 667 を学習し、次の盤から `learned` になる。
      手順: ローカルでサーバを立てる（`SPINDLE_CONFIG` に scratchpad の config、`sudo -u ubuntu -g cdrom` で
      起動、ポートは空いているもの）→ ログイン → `POST /api/cd/rip { toc, metadata }`（名前は空でよい）→
      `GET /api/jobs?type=rip` の note と Inbox のサイドカー（`spindle-inbox.json` の `rip.report`）を見る。
      rip.log の「読み取りオフセット」と照合欄も確認。終わったら**自分で起動したプロセスだけ**止める
      （`pkill -f` はシェル自身に当たるので使わない。PID を控える）
- [x] Inbox で名前を入れて承認し、Library の `tracks.source_type = cd_rip` /
      `album_verifications`（`source = rip`、`log_path`）/ `tracks.verification` を見る
      （済: 2026-09-23 に同じ 2 トラックの盤で。吸い出しは表の +667・試行 3 回・CTDB mismatch / AR not_found →
      承認 → `tracks` 2 行が `cd_rip` / `mismatch`、`album_verifications` が ctdb / accuraterip の 2 行で
      `source = rip`・`log_path` は移動後の Library 相対、`track_verifications` 4 行に CRC。
      このとき `album_verifications.drive_offset` が NULL だったので、`read_offset` を書くよう直した（D-83 追記 3）。
      **照合が通る盤での確認（上の項目）は、この盤しか無いため残り**）
- ~~残課題（判断待ち）: DiscID の食い違い~~（D-67 追記 3 で解消）

#### やること

- [x] 全ディスクを 1 本の PCM として取得 → オフセット適用 → 分割（`cd::rip::read_disc`: `cd-paranoia -e -r`、
      範囲は TOC から `first-last[mm:ss.ff]`。常にオフセット 0 で読み、`shift_pcm` で当てる。D-83）
- [x] `POST /api/cd/rip` と rip ジョブ。CD 画面の「取り込む」を有効にする
      （`jobs/handlers/rip.rs`、payload `{ toc, metadata }`、dedup `rip`。API は盤がドライブにあり TOC が一致する
      ときだけ 202、進行中なら 409 `duplicate`。`GET /api/cd/status` の `rip_job` で開き直しても追う。
      完了の一行は `jobs.note`（「Inbox に置いた: …」）。画面は `useCdRip`）
- [x] 2 回の走査に CTDB の修復を配線する（P2-7 の残り。直せなければ再リップ / `mismatch`）
      （`cd::rip::rip_disc`: 照合 → ずれを当てる → CTDB のパリティで修復 → 吸い直し（比べる相手が
      あるときだけ、`retry_on_mismatch` 回）→ それでも駄目なら mismatch のまま Inbox へ）
- [x] `place_disc` を **Inbox 経由**に作り直す（D-67 追記）: FLAC と rip.log / disc.cue / disc.toc を件の
      ディレクトリへ、MusicBrainz から写した内容と `RipReport` をサイドカーへ、`inbox_items` に 1 件。
      PCM の切り方・エンコード（`flac -N --verify`）・タグの写像・同梱ファイルは既存のものをそのまま使う
      （隠しディレクトリ `.spindle-rip-<DiscID>` で組み立てて `CD/<名前> [<DiscID>]` へ公開し、`inbox` ジョブを
      投入。件は走査で出る。D-67 追記 2、SPEC §7.2）
- [x] Inbox の承認と配置が、サイドカーの `RipReport` から `album_verifications`（`source = 'rip'`）/
      `track_verifications` / `tracks.verification` と `source_type = 'cd_rip'` を入れる。
      **対応付けはファイルの basename**（配列順を信じない。Inbox では番号もタイトルも直せる）。
      登録は `register_item` と同じトランザクション、`log_path` は移動後の Library 相対
      （`import::inbox::bind_rip` / `register_item`。D-67 追記 2）
- [x] サイドカー v1 に `RipReport` を入れる形を足す（`import/sidecar.rs` の `RipEntry`。モジュールを
      ytmusic から移した）。CD の件は `album_gain = true` で提案する（D-74。web の初期値も提案に従う）
- [x] **リップの開始は空の名前を許す**（D-67 追記）。`DiscMetadata::validate` から名前の必須を外した
      （`EmptyAlbum` / `EmptyAlbumArtist` / `EmptyTitle` を削除。空のタイトルは `with_placeholder_titles` で
      `Track NN`）。web の `validateDraft` も同じ規則に直した（`POST /api/cd/rip` の前に使う）
- [x] 吸い出し中、トラック単位の進捗を SSE の `job` イベントで流す（`JobEvent.detail` に
      `RipProgress { phase: read|verify|repair|encode|place, attempt, disc_no, track_no, done, total }`。
      read はセクタ、verify / repair はバイト、encode はトラック。表の欄と状態の一行は `lib/cdRip.ts`:
      読み取りは読み終えた行「読んだ」・読んでいる行「読み取り中」、照合 / 修復は全行、エンコードは終えた行
      「FLAC 済」・次の行「エンコード中」、配置は全行「Inbox へ」）
      （`{ phase: read|verify|encode|place, disc_no, track_no, done, total }`）。CD 画面のトラック表の
      右端に出る（器は P4-20 の `CdTrackTable` の `RipProgress`）。**相ごとの `track_no` の進み方と、
      表の「完了」の表現はここで確定する**（read は 1 本の PCM なのでトラック単位に分かれない）
- [x] 実装後に **SPEC §7.2 の「Library へ直接置いていたときの記述」を現在形に置き換える**（配置の段落は
      置き換えた。吸い出し・修復・再リップの段は rip ジョブの実装で見直す）

#### 受け入れ（必ず見る）

- **候補ゼロ件・アルバム名もアーティストも空のまま Inbox まで完走する**（CD 画面から編集を外したので、
  この経路が塞がっていると「どれも違う」盤を取り込めない）（済: 2026-09-23 に実機（BDR-209M、2 トラックの
  盤）で `POST /api/cd/rip` → 読み取り 2 回（CTDB 不一致で吸い直し、修復は直せず）→ エンコード → Inbox の
  `CD/[<DiscID>]` に pending、album gain on、名前の警告付き。SSE の `detail` は read / verify / repair /
  encode / place の順に届いた。**照合が通る盤でのオフセット学習の実機確認は残り**）
- サイドカーの basename 対応が 1 対 1 でないとき / CRC の件数や `disc_no` が合わないときは配置しない
  （済: `tests/inbox_draft.rs` の `bind_rip_*`、`tests/inbox_job.rs` の `cd_item_is_not_placed_*`、
  `tests/inbox_sidecar.rs` の `rip_entry_*`）
- 承認で番号やタイトルを直しても、検証記録が正しいトラックに付く（済: `tests/inbox_job.rs` の
  `cd_item_is_placed_with_verification_bound_by_file_name`）
- 既存の `tests/cd_place.rs` は Library 直行前提なので書き換わる。実ドライブのテストは `#[ignore]`
  （済: `tests/cd_place.rs` を Inbox 前提に書き直した。`nameless_disc_completes_to_inbox_and_through_approval`
  が 1 つ目の受け入れを Inbox の配置まで通す。吸い出し本体から通すのは rip ジョブで）

### P2-6 ARv1/v2 CRC と CTDB CRC32

先頭・末尾トラックの除外規則に注意。`src/cd/`（`TrackLayout` にサンプルを順に流すストリーミング計算。
吸い出し PCM と既存 FLAC のデコード結果の両方に同じ形で使う。80 分のディスクで 0.3 秒）。

- [x] 定義は CUETools の `AccurateRip.cs` / `CDRepair.cs` に合わせた: ARv1 は先頭トラックの頭 5×588−1、
      末尾トラックの尻 5×588 を除外（非対称）、ARv2 は 64 bit 積の上位も加算、AccurateRip DB のプレス違い
      検出に使う crc450 も併せて算出
- [x] CTDB は zlib CRC32 で、ディスクは頭 10 セクタと尻 (10 セクタ + 総数 mod 5880)、トラックは先頭の頭と
      末尾の尻に同じ除外

受け入れ: `tests/cd_crc.rs`（手計算できる閉じた式での除外規則、v2 の上位加算、語の詰め方、crc450、
細切れ push の一致、奇数長の持ち越しと L 余りの拒否、サンプル数の過不足）と、CUETools の定義から独立に
書いた Python 実装（`scripts/gen_cd_crc_fixture.py` → `tests/fixtures/cd_crc_reference.json`）との
突き合わせ

### P2-7 CTDB 照会・修復適用

AccurateRip は補助。照会は P2-9 で済み。

- [x] 修復は `src/cd/repair.rs`（D-66。CUETools `CDRepair` / `RsDecode` / `Parity2Syndrome` の定義:
      GF(2^16) 0x1100B、stride 11760 語、`SyndromeSampler` → `SyndromeTable`（80 分 3〜5 秒）、
      `find_offset`（列 0、±2939）、`plan`（BM → Chien → Forney、列あたり npar/2 個まで、直した後の CRC が
      合うときだけ）、`RepairApplier`（2 回目の走査で XOR）、`decode_entry_syndrome`（`syndrome` / 旧
      `parity` 属性）、`DbSyndromes::parse`（面順））
- [x] `CtdbClient::fetch_syndromes`（`hasparity` を Range で先頭 npar 面）

受け入れ: `tests/cd_repair.rs`（表を使わない GF 演算との一致、直接の定義との一致、ずらした列の再計算との
一致、LFSR パリティ → シンドローム、実応答の `syndrome` 属性、面順、列ごとの能力内の修復とオフセット
付き修復、能力超え、CRC 不一致、範囲外の誤り、乱数ストレス、本番 stride で 1 セクタ丸ごと）、
`tests/cd_lookup.rs`（Range の 206 / 200、列 0 の検証、npar 超え、404。実サーバは `#[ignore]`）

残り: なし（P2-5 の `cd::rip::rip_disc` で 2 回の走査に配線した。直せなければ吸い直し、それでも駄目なら
`mismatch` のまま Inbox へ）

### P2-8 エンコードと配置

`rip.log` / `disc.cue` / `disc.toc` の出力（D-67）。

- [x] `src/cd/place.rs`（`place_disc`: `DiscMetadata` の検証 → トラックごとの PCM MD5 → `pathgen::plan`
      （category 無しは `unsorted`、複数枚組は `multi_disc`。2 枚目以降は宛先の同名 album に合流）→
      raw PCM を `flac -N --verify --skip/--until` で tmp へ（STREAMINFO の MD5 が PCM と一致するときだけ）
      → lofty でタグ → `library` の排他 → tmp + `RENAME_NOREPLACE` で配置 → 1 トランザクションで `albums` /
      `tracks`（`source_type = cd_rip`、`verification`）/ `track_tags` / `album_verifications`（`source = rip`）/
      `track_verifications` → `rg` と `transcode` を投入。再実行は MD5 で自分の成果物を見分ける）
- [x] `src/cd/metadata.rs`（web の `DiscMetadata` と同じ形 + `category`。検証とタグ写像）
- [x] `src/cd/riplog.rs`（`RipReport`、`rip.log` / `disc.cue` / `disc.toc` の描画。複数枚組は `disc<N>.*` /
      `rip<N>.log`）
- [x] `GET/POST /api/categories` と確定フォームの category（`useCategories`）
- [x] D-43 の残課題: rename ジョブが commit 後に、album 全体の移動で active な行が無くなった旧ディレクトリの
      既知の同梱ファイルを宛先へ移し、空なら rmdir（`edit::rename::follow_companions`）
- [x] スキャナは spindle の rip.log（先頭行の署名）があるディレクトリの新規行を `cd_rip` にする。
      verify.log は `data/verify` のまま

受け入れ: `tests/cd_place.rs`（配置・タグ・同梱 3 ファイル・DB 行・後続ジョブ・次のスキャンで不変、
not_attempted、複数枚組の合流、別リリースの `({year})` 降格、同じ盤の再実行の冪等性と別音声の衝突、
配置後に落ちてスキャナが拾った行の採用、排他中の Busy、PCM 長 / メタデータの拒否）、
`tests/cd_metadata.rs`、`tests/cd_riplog.rs`、`tests/scanner.rs`（rip.log → cd_rip）、`tests/rename.rs`
（同梱ファイルの追随・衝突・巻き戻し）、`tests/categories_api.rs`、`web/src/lib/cd.test.ts`

残り: なし（P2-5 で `place_disc` を **Inbox 経由**に作り直して rip ジョブから配線した。Library への配置・
DB 行・後続ジョブは Inbox の承認と配置（P2-10）が行う。`POST /api/cd/rip` も P2-5。D-67 追記 2）

### P2-9 遡及照合

44.1/16/2ch かつサンプル数が 588 の倍数のときのみ（D-63）。

- [x] `verify` ジョブ（album 単位、並列 2。`src/jobs/handlers/verify.rs`）: ディスクごとに STREAMINFO の
      サンプル数から TOC を再構成 → デコードして CRC 表（`src/cd/crctable.rs`。1 回流して ±2939 の
      全オフセットの CRC が出る）→ CTDB / AccurateRip に照会（`CtdbClient` / `AccurateRipClient`。
      **P2-7 の照会部分はここで実装済み**、残りは修復適用）→ オフセットを探して照合
      （`src/cd/verify.rs`）→ `album_verifications`（手法 × ディスク。migration 0013 で `disc_no` と、
      再実行の冪等キー `job_id`）/ `track_verifications` / `tracks.verification` と
      `data/verify/<album_id>.log`（1 トランザクション。`audio_version` と fstat の再照合が通るときだけ）
- [x] `POST /api/verify { selection }` と操作タブの「遡及照合」、設定 `[verify]`（照会先の URL）

受け入れ: `tests/cd_crctable.rs`（ずらした列への直接計算と全オフセットで一致、crc32 combine）、
`tests/cd_lookup.rs`（実サーバから保存した bin / XML の解釈、ローカル HTTP でパス・クエリ・
UA・404）、`tests/cd_verify.rs`（合成エントリでのオフセット検出、壊れたトラック、候補の絞り込み）、
`tests/verify_job.rs`（ffmpeg で作った FLAC で verified / offset / AR のみ / mismatch / not_found /
unverifiable / 不完全 / 複数ディスク / 照会失敗 / 再照合の履歴 / 照合中の版更新・差し替え・
キャンセル / 原子性 / ログ確定失敗の巻き戻し / commit 後の再実行の冪等性）、`tests/verify_api.rs`

### P2-10 Inbox 取り込み

ステージング → 承認キュー → 配置（D-68）。

- [x] `src/import/inbox.rs`（`scan_inbox`: `[paths].inbox` を走査して音声のあるディレクトリを 1 件として
      `inbox_items` / `inbox_files` に登録（root 直下は rel_dir ""）。ファイルが変われば読み直して pending に
      戻し、消えた件は行ごと消す。`proposal`: タグから下書き（albumartist / album / date / category は
      genre 写像、トラックは DISCNUMBER / TRACKNUMBER / TITLE / ARTIST、無ければファイル名順）。
      `InboxDraft::problems`: 承認の検証。`place_item`: 承認済みの件を `library` の排他の下で配置
      （MUSICBRAINZ_ALBUMID → 自分の成果物の album のキー → `inbox:<id>` のリリースキーで `pathgen::plan` →
      tmp + `RENAME_NOREPLACE`、補正はファイルのタグに書く → 1 トランザクションで `albums` / `tracks`
      （`source_type = download`）/ `track_tags` → Inbox 側を消す → `rg` / `transcode`、wav / alac / aiff は
      `[normalize].wav_to_flac` なら normalize バッチも投入。再実行は音声の指紋で自分の成果物を見分ける）
- [x] `src/import/placement.rs`（CD の配置と共通の tmp + rename / album の解決 / 行の登録。リリースキーは
      登録トランザクションで再検証）
- [x] `src/jobs/handlers/inbox.rs`（`inbox` ジョブ = 走査 + 承認済みの配置。`[inbox].poll_interval_secs`
      （既定 60、0 で自動なし）の周期投入と「今すぐ確認」）、旧 `db/migrations/0014`（D-88 で `0001_init.sql` へ統合）、`src/db/inbox.rs`
- [x] `src/api/inbox.rs`（`GET /api/inbox`、`POST /api/inbox/scan`、`POST /api/inbox/{id}/approve|reject|reopen`）
- [x] web の Inbox タブ（`lib/inbox.ts` / `useInbox` / `InboxView`: 件の一覧とアルバム単位 + トラック単位の
      補正フォーム、placed からアルバムへ）

受け入れ: `tests/inbox_job.rs`（検出、変更の読み直しと承認の取り消し、消えた件、placed の期限切れ、
補正付きの配置と同梱ファイル・DB 行・後続ジョブ・Inbox の消費、wav の normalize 投入の有無、
衝突 → failed と後始末、排他が取れないときの再投入、配置中の変更、再実行の冪等性、placing のまま
落ちた件の回復、コピー前の差し替えの検出、登録前 / 登録後に落ちた後の完了、placed のディレクトリに残った音声、normalize の投入と登録の原子性）、
`tests/inbox_draft.rs`、`tests/inbox_db.rs`、`tests/inbox_api.rs`、`web/src/lib/inbox.test.ts`

- [x] トラックリスト貼り付けを承認画面へ移す（P4-20 追記で CD 画面から外したもの。2026-09-23。D-65 追記 2）:
      トラック表の下の畳んだ節。`lib/tracklist.ts` で解析し、`lib/inbox.ts` の `applyTracklist` で選んだディスクの
      行へ `track_no` で写す（複数枚組のときだけ写す先を選ぶ。アーティストを貼った行は「そのまま保つ」を外す）。
      CD 側の `lib/cd.ts` の `applyTracklist` は消した

受け入れ（貼り付け）: `web/src/lib/inbox.test.ts` の `applyTracklist`（番号で写す・並び順に依らない・
アーティストの無い行は保つ・件に無い番号と行数の違いと未設定の行の警告・複数枚組は選んだディスクだけ・
番号の重複する行には写さない・多値の「そのまま保つ」を外す）。ローカルのサーバで 3 曲の件に貼り付け →
警告の表示 → 承認で貼ったタイトルの名前で配置されるのを確認

---

## P3 — ytmusic 統合

完了条件: **ytmusic CLI を廃止できる。**

### P3-1 タイトルパーサ → メタデータプラグインのプロトコル v1

D-69。タイトルの慣習は spindle に置かず、外部コマンド（`[ytmusic].metadata_command`。参照実装
`AkashiSN/spindle-ytmusic-meta`。Python 版 ytmusic のテスト 98 件から生成したフィクスチャで一致を
保証）に JSON で問い合わせる。

- [x] `src/import/ytmusic/metadata.rs`（`Request` / `Response` / `Track`、`MetadataProvider::resolve`:
      引数配列で起動 → stdin に Request → stdout の Response を検証。タイムアウト・非ゼロ終了・不正な JSON・
      必須の値の欠落は `ProviderError`、`ok: false` は `Outcome::Declined { reason, message }`）
- [x] `ExternalCommand::stdin_bytes`（stdin へ書いて閉じる）を追加

受け入れ: `tests/ytmusic_metadata.rs`（偽のプラグインで: 往復と stdin の内容、`ok: false` の各 reason、
非ゼロ終了、不正な JSON、プロトコル違い、空の必須値、タイムアウト、起動失敗）、`tests/config.rs`、
`tests/docker_context.rs`（`include_str!` / rust-embed の埋め込み元が Dockerfile の build stage に
COPY されている）

### P3-2 チャンネル定義とカテゴリ写像

チャンネル定義はプラグイン側（D-69）。

- [x] spindle は `Track` をタグ（`Track::tags(track_no)`: TITLE / ARTIST 多値 / ALBUM / ALBUMARTIST / DATE /
      TRACKNUMBER + 追加タグ）と `pathgen::TrackFields`（`Track::track_fields`）に写し、`category` は
      `db::categories::ensure` で無ければ語彙に追加する

受け入れ: `tests/ytmusic_metadata.rs`（写像）、`tests/categories_api.rs`（ensure）

### P3-3 ダウンローダ

yt-dlp を subprocess で呼び、**Inbox に置くところまで**（D-70。配置・採番・後続は Inbox）。

- [x] ジョブ基盤: `JobError::Fatal`（バックオフせず `failed`）、`JobType::Ytdl`（並列 1）
- [x] `[ytmusic].download_timeout_secs`、起動時診断（`metadata_command[0]` と yt-dlp の実行可否を警告）
- [x] サイドカー `spindle-inbox.json` の読み書き（`import/sidecar.rs`。merge は tmp + rename）
- [x] Inbox: 既存 album の採用（MB キー無し同士）、TRACKNUMBER 無しの採番（max + 1 から名前順）、承認と
      登録トランザクションでの `(disc_no, track_no)` の重複検証、`destination` / `source` の応答、
      サイドカーの category 提案と配置成功時の削除、pending に戻った件の下書き merge
- [x] `downloader.rs` + `handlers/ytdl.rs`: dump（playlist 展開）→ SOURCE_URL の重複 → プラグイン →
      download → remux → タグ（PICTURE / SOURCE_URL）→ Archive/youtube/<id>.webm → Inbox + サイドカー → inbox 投入
- [x] `POST /api/ytmusic/download`、操作タブの「YouTube」節、Inbox タブの宛先表示と判定バッジ / message
- [x] Dockerfile に deno（yt-dlp の JS ランタイム）。compose のプラグインのマウント例

受け入れ: `tests/ytmusic_download.rs`（偽 yt-dlp（bash）+ 偽プラグインで ok / unmatched / skip /
playlist / 重複 / Fatal と Failed の区別、dump の解釈、ファイル名。実 yt-dlp の通しは `#[ignore]`）、
`tests/ytmusic_api.rs`、`tests/inbox_sidecar.rs`、`tests/inbox_job.rs`（追記・番号の再検証・
サイドカーの削除）、`tests/inbox_api.rs`（destination / source / 採番 / 400 / merge）、
`tests/inbox_draft.rs`、`tests/jobs.rs`（Fatal）、`tests/config.rs`（起動時診断）

### P3-4 配置直後のアートワーク解決

~~ytmusic ダウンロード後の Derived 投入~~ から縮小。Opus 原本に Derived は無く（非可逆 → 非可逆禁止）、
rg は Inbox の配置で投入済み。

- [x] 残っていた「次のスキャンまで画像が出ない」を、配置の直後にその album だけ解決して thumbnail を
      投入する形で埋めた（`scanner::resolve_album_artwork_now`、`PlaceItemEnv.artwork`。D-68 追記）

受け入れ: `tests/inbox_job.rs`（埋め込み画像から解決して thumbnail 投入、画像なしは「なし」で解決）

### P3-5 偽ハイレゾ検出

`rustfft`、任意機能（SPEC §7.10、D-71）。表示と絞り込みだけで、判定を消費する自動処理は無い。

- [x] `media/hires.rs`: `HiresSink`（PcmSink。Hann 8192 / ホップ 8192 の FFT をチャンネルごとに累積、
      無音フレーム除外、サンプルの OR）→ `Measurement { cutoff_hz, cliff_db, effective_bits }`（候補は
      1/3 オクターブ平滑化、エッジは平滑化前の段差最大、崖は平滑化前。境界は SPEC §7.10）→ `[hires]` の
      しきい値で `Verdict`
- [x] 旧 `db/migrations/0016_hires_check.sql`（D-88 で `0001_init.sql` へ統合）: `tracks.hires_check*` + `hires_cutoff_hz` / `hires_cliff_db` /
      `hires_effective_bits`、`jobs.type` に `hirescheck`（0015 と同じ表の作り直し）。`db/hires.rs`（Status /
      Target / record / enqueue_all_unchecked / enqueue_selection）
- [x] `jobs/handlers/hirescheck.rs`（並列 = max(1, コア数 / 2)、`version_field` で stale ゲートと track_locks、
      fstat 照合 → decode → デコード後の再照合 → 版付き record。デコード失敗は `decode_error`）。`[hires]` 設定
      （`tests/config.rs`）。スキャン commit での自動投入
- [x] `tracks` の行に `hires_check`、`Flag::HiresUnchecked` / `HiresSuspect`、DSL の `hirescheck` / `cutoff` /
      `cliff`（`Kind::Float`）/ `effectivebits`、fb2k は変換不能。`POST /api/hirescheck`
- [x] UI: H バッジ（疑い / inconclusive / エラー、stale は •）、プロパティ「Hi-Res check」（判定 + 計測値）、
      フィルタ、操作タブ「偽ハイレゾを検出」、ジョブ名

受け入れ: `tests/hires_analysis.rs`（逆 FFT の合成信号で: 96 kHz の 22.05 kHz と 24 kHz の brickwall →
upsampled でエッジ ± 数百 Hz、帯域外が完全ゼロでも有限、緩やかなロールオフ → inconclusive、全帯域 →
ok（cutoff = Nyquist）、下位 8 bit ゼロ → padded / both、44.1k はスペクトルなし、全無音・32 bit →
inconclusive で計測値 NULL、2ch は cutoff 最大の ch と対の cliff、判定の優先順位）、`tests/migrations.rs`、
`tests/hires_db.rs`、`tests/jobs.rs`、`tests/hires_job.rs`（合成 24/96 FLAC で ok / padded、44.1k の 24 bit は
スペクトルなし、対象外・差し替え済み・版が進んだ件は記録なし、壊れたファイルは decode_error、スキャンの
自動投入と再投入なし、ffmpeg 経路（WavPack）の整数スケール）、`tests/dsl_compile.rs`、`tests/fb2k.rs`、
`tests/hirescheck_api.rs`、`web/src/lib/{badges,properties,operations}.test.ts`

---

## P4（予定）

完了条件は未定。P0〜P3 の残課題から決めたものだけを置く。設計は着手時に行う。

### P4-1 CPU 系ジョブの共通並列予算

D-73。2026-09-20。

- [x] `worker.rs` に共有 Semaphore（= コア数）、`rg` / `transcode` / `flaccheck` / `hirescheck` が種別の
      上限に加えて取る
- [x] `GET /api/jobs` に `cpu_budget`（`concurrency` の兄弟。D-73 追記）

受け入れ: `tests/jobs.rs`（コア数 2 で rg + flaccheck の同時投入で実行中の合計が 2 を超えない、4 種を
固めて投入しても先頭 6 本に 4 種が揃う（ラウンドロビン）、thumbnail は縛られない）、
`web/src/lib/jobs.test.ts`（脚注）

### P4-2 CD 確定フォームの「候補から写す範囲」

D-72。2026-09-21。

- [x] 「識別用の最小限」（ALBUM / ALBUMARTIST / DATE / DISCNUMBER / DISCTOTAL / TRACKTOTAL / MB id）と
      「全部写す」（+ LABEL / CATALOGNUMBER / BARCODE / トラックのタイトル・アーティスト・ISRC）。
      `web/src/lib/cdState.ts` の reducer に 1 アクション
- [x] **既定は P4-20 で「全部写す」に変えた**（D-72 追記 2）。表が主役になり、候補を選んだら
      トラック名が入るのが期待される動きになったため

受け入れ: `web/src/lib/cdState.test.ts`

### P4-3 プロパティタブのフィールド削除 / 追加

D-72。2026-09-21。

- [x] 右クリック（または行末の ×）で `delete` op、「フィールドを追加」で新しいキーに `set` op。どちらも
      選択全体への一括編集（preview → apply）

受け入れ: `web/src/lib/properties.test.ts`（コンポーネントテストの基盤が無いので、画面は実機で確認）

### P4-4 Inbox 承認画面の忠実表示

D-70 追記。2026-09-20。

- [x] ARTIST の全値を `;` 区切りで見せ、`keep_artists` で「そのまま保つ / 1 値で書く」を明示
- [x] Inbox のファイルの埋め込み画像を返す `GET /api/inbox/:id/artwork/:hash`（`PICTURE` のハッシュで実体を
      照合、Library の artwork と同じ ETag / キャッシュ）と承認画面のサムネイル

受け入れ: `tests/inbox_api.rs`、`tests/inbox_job.rs`、`web/src/lib/inbox.test.ts`

### P4-5 album gain を album ごとの属性に

D-74。2026-09-20。

- [x] 旧 `db/migrations/0017_album_gain.sql`（D-88 で `0001_init.sql` へ統合）（`albums.album_gain` 既定 0、既存の `tracks.rg_album_*` を NULL）
- [x] rg の投入経路（`POST /api/rg`、承認後、CD 配置後。スキャンは投入しない）は属性で album / track 単位を
      選ぶ。`cd/place.rs` は true で作る
- [x] 承認画面のチェックボックス（既定 off、追記先 album の現在値が初期値）
- [x] 切り替え（`PATCH /api/albums/:id { album_gain }` → true なら album 単位の rg を投入、false なら
      `rg_album_*` を NULL にして未書込に）。切り替えは操作タブ（アルバム画面は無い）
- [x] 書き出しは false なら album のキーを書かず、あれば消す
- [x] web は `lib/albumGain.ts` / `lib/inbox.ts`

受け入れ: `tests/migrations.rs`、`tests/rg_job.rs`（track 単位の投入と album の集計なし）、`tests/rg_api.rs`、
`tests/rg_db.rs`（属性の読み書きと投入単位）、`tests/cd_place.rs`、`tests/inbox_api.rs`、
`tests/inbox_job.rs`（下書きの album_gain）、`tests/albums_api.rs`

### P4-6 アルバム一覧をトラック一覧と同じフィルタで絞る

D-58 追記。2026-09-20。

- [x] `GET /api/albums?filter=`（トラック一覧と同じ JSON フィルタ。指定があれば一致する active なトラックを
      1 本以上持つ album だけ。`WHERE a.id IN (SELECT t.album_id FROM tracks t … WHERE <db/tracks.rs の既存の
      WHERE>)` で、ツリーの `album_ids` / `category` / `playlist_id`（静的・スマート）/ `flags` / `dsl` / `q` を
      そのまま再利用）
- [x] web は `useAlbums(filterParam)` でツリー・プレイリスト・検索語の変更で取り直す

受け入れ: `tests/albums_api.rs`（album_ids / category / 静的プレイリスト / スマートプレイリスト / q で絞れる、
フィルタ無しは全件、不正なフィルタは 400）

### P4-7 Derived の系統化と Opus 256k

D-9 追記、D-51 追記、D-75、SPEC §7.6。2026-09-20。

- [x] 新しい連番のマイグレーションで `delivery` ビューを DROP → `derived_files` を `(track_id, variant)` 主キー +
      `audio_profile` / `tag_profile` で作り直し（既存行は `variant = 'opus'`、`audio_profile = 'opus:128:v1'`、
      `tag_profile = 'opus:v1'`）→ ビューを `variant = 'opus'` で再作成
- [x] `[encode]` を `[encode.derived.opus] { enabled, bitrate }` に改め（`derived_codec` / `derived_bitrate` は
      廃止。deploy/config.example.toml と SPEC §13 を揃える）
- [x] `domain/derived.rs` の `plan()` に variant と `audio_profile`（違えば Encode）/ `tag_profile`（違えば
      Retag）、`enabled = false` の凍結（Skip）を足す。パスは `Derived/opus/…`
- [x] `transcode` の dedup キーとロックに variant、scan 完了時の投入は enabled な系統ごと
- [x] `playlist/compile.rs` の `has_derived` と `db/tracks.rs` の `Flag` / `derived` 集約を `variant = 'opus'`
      に限定
- [x] `GET /api/tracks` の `derived` を系統ごとの形に（web の `TrackRow.derived`、D バッジは opus のまま、
      プロパティ Location 列に系統ごとの行）

受け入れ: `tests/migrations.rs`（ビュー DROP → 作り直し → 再作成の順、既存行の variant / profile、`delivery` が
opus だけを指す）、`tests/derived_plan.rs`（`audio_profile` 差分で Encode、`tag_profile` 差分で Retag、
enabled=false で Skip、旧ルート直下の行は 128k → 256k なら Encode で `opus/` へ、profile 一致でパスだけ
違えば Move）、`tests/transcode_job.rs`（Encode 後の旧パスの退避と削除、Move で `opus/` 配下へ）、
`tests/dsl_compile.rs`（`has_derived` が opus 限定）、`tests/config.rs`、`tests/tracks_api.rs`（2 系統あっても
行は 1 つで `derived` に両方）、`web/src/lib/properties.test.ts`

### P4-8 Apple 向け `aac` 系統

D-75、D-8 追記、SPEC §7.6。2026-09-20。

- [x] `[encode.derived.aac] { enabled（節省略時 false）, bitrate, lossy_sources, multi_value_separator }`、
      0019 で `derived_variants` に `lossy_sources` / `multi_value_separator`（`sync_variants(&DerivedConfig)`
      が両系統を写す）
- [x] `media/encode.rs` に `AacEncoder`（ffmpeg 1 パス: `-af volume=<gain>dB [-ar 48000] -c:a aac -b:a <k>k
      -f mp4`。aac 原本も同じ経路）
- [x] `domain/derived.rs`: `Target.rg_ready`（時刻 + gain + peak）、`eligible(&VariantSettings, &Target)`
      （`aac` は `lossy_sources` で非可逆も、RG 未解析は待つ）、`aac` の RG 世代差分 → Encode、
      `bake_gain_db`（true peak で頭打ち、非有限は 0 / 上限なし）、`aac_tags`（多値を区切りで結合、RG 系と
      既存 ITUNNORM を落として `iTunNORM` 0 dB）
- [x] `domain/tags.rs::write_mp4_tags`（ilst 標準 atom + `----:com.apple.iTunes:<KEY>` フリーフォーム +
      `covr`）。画像は `ThumbFormat::Jpeg` の 768（`thumbs/<hex>/768.jpg`）
- [x] transcode ハンドラは両エンコーダを持ち系統で分岐（予約・占有・配置・退避・drift は共通）。
      Dockerfile の変更なし

受け入れ: `tests/migrations.rs`（0019）、`tests/config.rs`、`tests/derived_db.rs`（sync と RG 待ちの投入）、
`tests/derived.rs`（非可逆の対象化、RG 未解析で Skip、RG 世代で Encode、`tag_profile` で Retag、
`bake_gain_db` の境界）、`tests/aac_tags.rs`（結合、iTunNORM、RG キー無し、`ffprobe` でフリーフォーム atom を
外部観測）、`tests/aac_encode.rs`（ebur128 で gain 0 / 負 / 正、96k → 48k、44.1k / 48k 据え置き、非可逆
3 形式、cancel）、`tests/artwork.rs`（JPEG 768）、`tests/transcode_job.rs`（可逆 → AAC の読み戻し・焼き込み・
peak 上限・96k → 48k・opus / aac 原本の再エンコード・RG 世代の作り直し・`lossy_sources` off の据え置き・
タグ上書き・区切り変更の Retag・両系統の共存）

### P4-9 終端を書けなかった `running` の稼働中回収

SPEC §8、D-76。ジョブの終端状態（done / failed / requeue）を DB に書けなかったとき（2026-09-20 の実機で
ディスク満杯により 88 本）、`src/jobs/worker.rs::execute` のフォールバック（failed の記録）も同じ理由で
失敗し、行が `running` のまま次回起動のリカバリまで残る（ロックも残る）。候補 (a) / (b) の両方を入れるのが
妥当。

- [x] (a) 終端書き込みをバックオフ付きで再試行（数十秒〜数分。一時的な満杯・ロックなら自力で復帰）
- [x] (b) ワーカーのループで `Jobs.running`（実行中の token 表）に無い `running` 行を定期的に queued へ戻し、
      `track_locks` / `derived_path_locks` / `job_mutexes` も解放する（起動時リカバリの稼働中版。
      単一インスタンス前提なので安全）
- [x] docker のログも満杯で落ちるので、回収したことは次に書けたときにログとジョブの `last_error` に残す

受け入れ: `tests/jobs.rs`（終端書き込みを失敗させる DB フック or 読み取り専用化で `running` を作り、
回収で queued に戻りロックが消える。実行中の本物の running は戻さない）、SPEC §8 に回収の記述

### P4-10 ダークテーマ

D-58 追記、SPEC §12 / §12.6。2026-09-21。

- [x] `index.css` の直書き色 59 箇所を `:root` の変数に集約（バッジは `--badge-<hue>-bg` / `-fg` の対）、
      ダークは `:root[data-theme='dark']` の 1 ブロックで全変数を差し替え（`color-scheme` も）
- [x] `lib/theme.ts`（`resolveTheme`: 保存値 > OS、`load` / `saveThemePref`）、`hooks/useTheme.ts`
      （`prefers-color-scheme` の変化を購読して `<html data-theme>` に書く）、`index.html` のインライン
      スクリプトで初回描画前に同じ規則で付ける（白飛び防止）
- [x] 切り替えは設定画面の「表示」節（OS に従う / ライト / ダーク）

受け入れ: `web/src/lib/theme.test.ts`（解決・保存・不正値、`index.css?raw` を読んで直書き色が 2 ブロック
以外に無いこと、両ブロックの変数集合が一致すること。`vitest.config.ts` に `css: true`）、Vite の開発
サーバから実機 API に中継して一覧（選択行・凡例・プロパティ）/ アルバム / Inbox / CD / ジョブ / 履歴
（partial の failed 行）/ 設定をダークで目視、ラジオでライト → OS → ダークの切り替えとライトの回帰なしを
確認

### P4-11 Library の MP4（ALAC / AAC）に任意キーを読み書きする

D-77、SPEC §7.5「形式ごとの写像」。2026-09-21。旧 Library 経路（`write_tag_changes` の `FileType::Mp4` →
lofty の generic `Tag` → `apply_generic`）は `ItemKey` の写像表に無いキー（`SPINDLETEST` 等の独自キー。
`CATALOGNUMBER` / `LABEL` / `BARCODE` / MB id は lofty 0.25 が `----:com.apple.iTunes:*` に写像済み）を
書けず、読み戻しの照合で安全側に失敗していた。読み側（`Ilst` → `Tag`）もフリーフォーム atom を捨てていた。

- [x] `domain::tags` に `mp4_target`（写像は 1 か所。`ITUNNORM_KEY` を `derived.rs` から移して共有）と
      `apply_ilst`（標準 atom は lofty の `Tag` → `Ilst` 変換に載せ、`trkn` / `disk` の相方を保つ。
      フリーフォームは大小文字無視で消してから 1 atom の複数値で書く）を置き、Library の MP4 分岐と
      Derived の `write_mp4_tags` が共有する
- [x] 読みは `collect_mp4`（`split_tag` の残りから `com.apple.iTunes` のフリーフォームを大文字化したキーで
      取り込む）

受け入れ: `tests/tags_write.rs`（独自キー・多値・`iTunNORM` の綴り・削除、大小文字違いの既存 atom の
置き換え、標準キーは標準 atom のまま = ffprobe、`TRACKTOTAL` だけ変えても `TRACKNUMBER` が残る）、
`tests/tags_read.rs`（フリーフォームの多値・小文字名・`iTunNORM`、標準 atom と同名は標準が勝つ）、
`tests/edits.rs`（ALAC に `set` → `delete` が applied）、実機で ALAC の 1 曲に追加 → 削除

### P4-12 CI / CD の整理と Docker イメージの配布

2026-09-21。SPEC §14「イメージの配布」、D-79。

- [x] (1) 置き場は GHCR（`ghcr.io/akashisn/spindle`）。パッケージは public。プラグイン
      （`spindle-ytmusic-meta`）は D-70 どおり焼かず、実行時マウントのまま
- [x] (2) タグ: `main` への push で `edge` と `sha-<7 桁>`、`vX.Y.Z` タグで `X.Y.Z` / `X.Y` / `latest`。
      PR は build と起動確認だけで push しない
- [x] (3) CI の docker ジョブで作ったイメージを**そのまま** push する（起動確認に通ったものと同じ digest。
      `docker push` は load したイメージで「unknown blob」になるので skopeo で daemon から複製）
- [x] (4) platform は `linux/amd64` のみ
- [x] (5) イメージに版を焼く: `org.opencontainers.image.{source,revision,version,created}` のラベルと、
      `spindle --version` / `GET /health` の `version`
- [x] (6) README「起動」と docs/OPERATIONS.md にカスタムアプリの作り方と更新手順。リハーサル環境は GHCR の
      `edge` を使い、`build.sh` は開発中の未コミット確認用に残す
- [x] (7) yt-dlp の更新は Dependabot / 自作ワークフローでなく **Renovate**（`renovate.json`。yt-dlp は
      `# renovate:` 注釈の custom manager）。`/health` に yt-dlp の版
- [x] (8) `v0.1.0` はまだ切らず `edge` までを完了とする（最初のタグはリリース時。スカッシュは (9) で実施済み）。
      ユーザ側の作業: GHCR のパッケージを public に、Renovate の GitHub App をインストール
- [x] (9) スカッシュ: リリース時の再移行の前に `db/migrations` を `0001_init.sql` 1 本へ畳んだ（2026-09-24、D-88）。
      最終スキーマは 24 本を流した結果と一致（`sqlite_sequence` の空行だけ差）。アップグレード試験は空 DB の
      スキーマ試験へ移した。最初のタグはリリース時

受け入れ（確認済み）: `main` への push で `edge` / `sha-<7>` が push され、リハーサル環境の compose を
`edge` に切り替えて `compose pull` → `/health` が `{"status":"ok","version":"a10fb0d","ytdlp":"2026.08.19"}`。
`docker pull ghcr.io/akashisn/spindle:edge` して `/health` が 200 かつ `version` に sha が入る（CI の
起動確認に `version` の検査を足す）、`vX.Y.Z` タグで `latest` が動く（最初のタグはリリース時）、PR では
push されない、実機を `compose pull` で更新して `/health` の `version` が一致する、README / OPERATIONS の
更新手順

着手時の設計メモ: いまは `.github/workflows/ci.yml`
が web（lint / build）→ rust（fmt / clippy / test）→ docker（build + `/health` 等の起動確認）まで行うが、
イメージはどこにも push しておらず、`deploy/compose.yaml` の `ghcr.io/akashisn/spindle:latest` は存在
しない。実機は `git archive` → ホストで `docker build` → `spindle:local` の手作業（`/root/spindle-migration/
build.sh`）。決めること: (1) 置き場は GHCR（`ghcr.io/akashisn/spindle`。compose が既に指している）。
パッケージを public にして pull にトークンを不要にする。プラグイン（`spindle-ytmusic-meta`）は
D-70 どおり焼かず、実行時マウントのまま。(2) タグ: `main` への push で `edge` と `sha-<7 桁>`、`vX.Y.Z`
タグで `X.Y.Z` / `X.Y` / `latest`。PR は build と起動確認だけで push しない。(3) CI の docker ジョブで
作ったイメージを**そのまま** push する（起動確認に通ったものと同じ digest。二度ビルドしない。
`docker/build-push-action` の `load` + `push` か、`docker/metadata-action` でタグを組んで最後に push）。
buildx の GHA キャッシュは今のまま。(4) platform は `linux/amd64` のみ（TrueNAS。arm64 は Rust の
クロスビルドが遅く需要も無い。要るときに足す）。(5) イメージに版を焼く: `org.opencontainers.image.
{source,revision,version,created}` のラベルと、`spindle --version` / `GET /health` の `version`
（`git describe` か sha。`build.rs` か `vergen` で埋める。実機の「いまどのコミットが動いているか」を
docker のログではなく `/health` で答えられるように）。(6) 本番は TrueNAS のカスタムアプリ（compose 相当）として動かす
ので、更新は TrueNAS の UI でイメージを pull し直す操作になる。自動配備は作らず、README「起動」と
docs/OPERATIONS.md に「カスタムアプリの作り方（`deploy/compose.yaml` を写す。デバイス・GID・
ボリューム・プラグインのマウント）と更新手順（`latest` / `X.Y` のどれを指すか、pull → 再作成、
`/health` の `version` で確認、DB のマイグレーションは起動時に自動で前進のみ = 戻すときは
バックアップから）」を書く。リハーサル環境は GHCR の `edge` を使うようにし、`build.sh` は開発中の
未コミット確認用に残す。
(7) **yt-dlp の更新**を仕組みにする（YouTube の抽出は yt-dlp が古いと壊れる。いまは Dockerfile の
`ARG YTDLP_VERSION=2026.08.19` 固定を手で上げている）: 週 1 の `schedule` で yt-dlp の最新リリースを
GitHub API から取り、`ARG` を書き換える PR を自動で作る（Dependabot は `ADD https://…` の版を追えない）
→ CI が通ればマージして `edge` を作り直す。本番へは次の `vX.Y.Z` で届く（yt-dlp だけの更新でもパッチ版を
切る。カスタムアプリ側の更新は手動なので、OPERATIONS に「YouTube の取り込みが失敗し始めたらまず
イメージを更新する」と書く）。あわせてベースイメージ（`debian:bookworm-slim` / `node` /
`rust` / `denoland/deno`）と GitHub Actions は Dependabot（`docker` / `github-actions`）で追う。
`/health` に yt-dlp の版（`yt-dlp --version` を起動時診断で取る）を出し、実機で確認できるように。(8) `vX.Y.Z` の GitHub Release（自動生成のノート）を作るかは任意。Release には
イメージのタグと digest だけ書く。**スカッシュ（`db/migrations` を `0001` に畳む）はこの前提**: 公開
イメージを誰かが pull して DB を作った後は既存ファイルを書き換えられないので、畳むなら最初の
`vX.Y.Z` より前（リリース時の再移行で DB を作り直すとき）に 1 回だけ行い、D-xx に記録する。
`edge` は開発用で DB の互換を約束しない旨を README に書く。

### P4-13 YouTube の導線を独立した画面に

D-70 追記、SPEC §12.6。2026-09-21。それまでは右パネル「操作」タブの中の `YouTube` 節（URL 欄 +
ダウンロード）で、選択したトラックへの操作と混ざって見つけにくく、投入した後の行方（ジョブ → Inbox）も
自分で探す必要があった。上部バーを `一覧 / アルバム / Inbox / CD / YouTube / ジョブ` にして `YouTube`
画面を置く（取り込み元は Inbox / CD / YouTube で横並び、結果は Inbox に集まる。SPEC §7.7、D-70）。

- [x] (1) URL 欄（1 行 1 つ。動画 / playlist。playlist は entries ごとに展開される）と「ダウンロード」。
      再生リストの展開時に `SOURCE_URL` のある動画は投入しない = 実機で再生リストを貼ると新しいものだけ
      落ちる。entries の URL は id から正規形を組む
- [x] (2) その下に ytdl ジョブの一覧（`GET /api/jobs?type=ytdl`。上限は種別内。全種別共通の上限だと
      transcode の完了 300 件に押し出される。URL・状態・失敗理由・完了した件は「Inbox で確認」で件へ飛ぶ。
      SSE で追随）。完了の中身は 0020 の `jobs.note`（`Outcome::DoneWith`）で区別する
- [x] (2') **購読の節**（P4-16 の UI。再生リスト URL ↔ 追記先 album の一覧、登録 / 削除 / 有効・無効、
      「今すぐ同期」、最終同期時刻と結果。P4-13 の時点では API が無ければ枠だけ置いて P4-16 で埋める）
- [x] (3) 「操作」タブの YouTube 節は消す（`lib/operations.ts` の `startYoutube` と `youtubeStartedMessage`
      を画面側へ）
- [x] (4) **URL の受け渡しを楽にする**: SPA のルート `/youtube?url=<URL>` で URL 欄を埋めて開く（同一 origin
      の GET なので CORS / CSRF の問題が無い。未ログインならログイン後にそのまま）。「いま見ている動画を
      spindle へ」のブックマークレット（`javascript:open('http://<spindle>/youtube?url='+encodeURIComponent(location.href))`）
      と、「このページの動画リンクを全部集めてクリップボードへ」のブックマークレット（チャンネルの動画
      一覧 / 検索結果 / playlist ページから `a[href*="/watch?v="]` を集めて改行区切りに）を README に載せる
      （2026-09-22 に `docs/USERGUIDE.md` §11.3 へ移した）
- [x] (5) `[bin].ytdlp` を引数付きにできるように（`ytdlp_args = ["--extractor-args", "youtube:player_client=…"]`
      か `ytdlp = ["yt-dlp", …]` の配列。`sh -c` は使わない）。ブロック時に `--extractor-args` / `--cookies` を
      設定で渡す口。UA / Referer は付けない（yt-dlp の YouTube 抽出は player client の偽装で innertube を
      叩くので、ブラウザ UA を上書きすると食い違って弾かれる。yt-dlp の公式見解）
- [x] codex のレビューで修正

受け入れ: `web/src/lib/youtube.test.ts`（URL 行の解析は `parseUrlLines` を移す、`?url=` の取り出し、ジョブ行の
整形）、`tests/jobs_api.rs`（`type=ytdl` で絞れる）、`tests/config.rs`（引数配列）、実機でブックマークレット →
画面が開いて URL が入る → ダウンロード → 一覧に出て Inbox へ飛べる

### P4-14 既存の webm 由来トラックへの `SOURCE_URL` 補填

一度きり。D-70 の重複防止は `SOURCE_URL` を見るが、移行で取り込んだ webm 由来の Opus 1,512 本（実機。P3 で
取り込んだ 1 本を除く）には無く、旧 `Original/*.webm` のメタデータにも id は無い（`encoder=google/video-file`
のみ）。手掛かりはユーザが YouTube 側で保守してきた**アーティストごとの再生リスト**: 旧パイプラインは
「再生リスト名 = アルバム名（`花譜のお歌` 等 10 アルバム）、再生リスト内の位置 = `TRACKNUMBER`」で並べて
おり（実機で確認: 各アルバムが 1 から連番、`花譜のお歌` は 244 曲で 1〜246 = 2 つ欠番）、非公開になった
動画もリストには `[Private video]` として id と位置が残る。再生リストの URL 一覧と計画 CSV はリポジトリ外に
保管する（公開リポジトリに置かない。リリース時の再移行で `SOURCE_URL` は消えるので、同じ入力で再適用する）。

- [x] `set_rows` op（D-42 追記）: `ops` に `{"op":"set_rows","key":…,"rows":[{"id":…,"value":[…]}]}` の行ごとの
      値を足す（tagops の 1 op、preview で差分が見える、巻き戻しは 1 回）。書き込みは通常の tagwrite
      （tmp + rename、`tag_version` +1）なので Derived の opus / aac がタグ上書きで追随する（1,512 × 2 本。
      音声は変えない）
- [x] `scripts/backfill_source_url.py`（標準ライブラリのみ。yt-dlp と spindle の API を使う）+
      `scripts/test_backfill_source_url.py`: (1) 再生リスト URL ごとに `yt-dlp --flat-playlist
      --dump-single-json` を取り（非公開のリストは cookie が要るので、一時的に限定公開にするか `--cookies` を
      渡す）、(2) 位置 i+1 と `TRACKNUMBER`、リスト名と `ALBUM` で Library の行を引き、タイトルが取れる
      entry はメタデータプラグイン（`spindle-ytmusic-meta`）で判定した title と Library の `TITLE` を照合して
      `verified`、`[Private video]` / `[Deleted video]` は `position-only`、番号に行が無い・タイトル不一致は
      `unmatched` として **計画 CSV を出す**（track_id / rel_path / id / 動画タイトル / 判定）、(3) 人が CSV を
      見てから `--apply` で `SOURCE_URL = https://www.youtube.com/watch?v=<id>` の編集バッチを投入する
- [x] 対応付けは 位置 + タイトル → 未割り当て行からタイトルで救済（一致の長い行 → 近い位置を優先）→
      両隣が同じずれで対応する区間は位置推定、の 3 段。位置推定は区間内の入れ替えを検出できないので既定では
      書かず `--include-inferred` で明示、部分一致は ASCII 英数字の境界を要求、既に `SOURCE_URL` を持つ行は
      上書きしない。codex のレビューで修正
- [x] 2026-09-21 に実機で適用済み: 9 バッチ #10〜#18、1,508 件 applied / conflict 0、Derived の aac 1,508 本が
      タグ追随。残り 6 件のうち 5 件はユーザが再生リストに追加 → 再実行のバッチ #19〜#23 で付いた。
      再生リストが無い `柊マグネタイトの曲` の 1 曲だけリポジトリ外の `singles.tsv` に URL を保管し、
      リリース時の再移行で手で付ける（MIGRATION §5-3-2）

受け入れ: `tests/tagops.rs`（`set_rows` の解析と適用。無い id は無視、値の検証は `set` と同じ）、
スクリプトの単体テスト（位置と番号の対応、欠番、Private の扱い。yt-dlp の JSON は fixtures）、実機で
1 アルバムを dry-run → CSV 確認 → apply → `SOURCE_URL` が付き、同じ URL の再ダウンロードが
「取り込み済み（Library）」で拒否される

実機で分かったこと: 開発機の古い yt-dlp（2026.06）は再生リストの continuation を黙って取りこぼす
（`entries < playlist_count` で中止する検出を入れた。`--ytdlp "ssh … docker exec … yt-dlp"` で実機の版を
使える）、再生リストから消えた動画の後ろは位置が 1 ずれる、初期の動画は英題で Library の邦題と照合できない

着手時に検討した案: 1 トラックずつ値が違う `set` をどう 1 バッチにするか（いまの `POST /api/tracks/batch`
は選択全体に同じ op。案 a: トラックごとに preview → apply で 1,512 バッチ（履歴が汚れる）、案 b: `ops` に
行ごとの値を足す `set_rows`。こちらが筋 → 採用）

### P4-16 再生リストの購読と同期

2026-09-21。P4-15「番号を再生リストの順に揃える」を吸収。SPEC §7.7「再生リストの購読と同期」、D-78。
`SOURCE_URL`（P4-14）で「再生リストのどこまで持っているか」が分かるので、URL を貼る運用をなくす。

- [x] (1) **購読**: マイグレーションで `playlist_subscriptions`（`list_id` / URL / 追記先 `album_id`（無ければ
      `albumartist` + `album` で作る）/ `enabled` / `last_synced_at` / 最終結果）。API は
      `GET / POST / PATCH / DELETE /api/ytmusic/subscriptions` と `POST /api/ytmusic/subscriptions/:id/sync`
      （設計時の `/api/playlists/subscriptions` から変更）。UI は P4-13 の YouTube 画面の購読の節
- [x] (2) **同期ジョブ** `playlist_sync`（購読ごとに 1 本。手動 + `[ytmusic].sync_interval_hours`（既定 0 =
      手動のみ）で定期）: `yt-dlp --flat-playlist --dump-single-json` で列挙（`entries < playlist_count` なら
      「古い yt-dlp の取りこぼし」として失敗させ、何も投入しない）→ 各 entry の `webpage_url` 正規形を
      Library（`track_tags`）/ Inbox（`inbox_files.tags`）/ 投入済み ytdl ジョブ（dedup_key）と突き合わせ →
      **無いものだけ** ytdl ジョブを投入（payload に `subscription_id` / 宛先 `album_id` / 再生リストの位置）。
      再生リストから消えた動画には何もしない（ファイルが正）。1 回の同期で投入する上限（既定 50）を
      設けて、誤登録した巨大なリストで数百本落とさない
- [x] (2') **非公開・削除の動画**（`[Private video]` / `[Deleted video]`。`--flat-playlist` でも id と位置は
      取れる）: ダウンロードはできないので投入しないが、**位置は占め続ける**ものとして扱う。Library に
      あれば（補填済み、または公開だった頃に取り込んだ）`SOURCE_URL` で一致するので何もしない。Library に
      無ければ「取れない」として購読の結果に一覧で出す（id・位置・非公開 / 削除の別）。後で公開に戻れば
      次の同期で普通の未取り込みとして拾う。番号揃え (4) もこれらの位置を数える（Library に無い非公開の分は番号が飛ぶ。
      既存の `花譜のお歌` が 244 曲で 1〜246 なのと同じ規則）。「非公開 / 削除の別」は現行 yt-dlp では
      出力から付かないので `kind: private | deleted | unknown` の hint に読み替え
- [x] (3) **承認はそのまま**（D-70。判断は Inbox で人が行う）。宛先 album と位置が分かっているので、承認画面の
      初期値（アルバムアーティスト / アルバム / category）は購読から埋め、プラグインの判定は補助にする
- [x] (4) **番号揃え**: 配置（Inbox 承認）の後、同期ジョブ（または承認の後続）が「再生リストの位置 ↔ 現在の `TRACKNUMBER`」の
      ずれを `SOURCE_URL` で計算し（目標番号 = 再生リストの位置。非公開・削除・未取り込みの位置も数えるので、
      それらの分は番号が飛ぶ）、ずれている行だけ `TRACKNUMBER` を `set_rows` の tags バッチで書き、続けて
      rename バッチでファイル名（`{track:02}. {title}`）を追随させる（どちらも履歴に載り巻き戻せる。Derived の
      opus / aac はタグ上書き・移動で追随）。`SOURCE_URL` の無い行と再生リストに無い行は触らず、ずれの一覧を
      購読の結果に出す。「番号揃えをしない」購読も選べる（既定は揃える）
- [x] (5) yt-dlp を定期的に叩くので、ブロック時の `--extractor-args` / `--cookies` の口（P4-13 の `[bin].ytdlp`
      引数化）と yt-dlp の更新（P4-12 (7)）が前提
- [x] 設計は codex レビューで 4 点の P1 を直した: 順序は列挙 → 揃え → 投入、phase 非永続の再計算（rename の
      候補は番号の合った行の全部）、Duplicate は「走行中」扱い、latch + Requeue + dispatcher、`album_id` の
      CAS 束ね
- [x] 実装レビューで足した規則: 購読 id は AUTOINCREMENT（0022）、同期が active の間は PATCH / DELETE を 409、
      重複 entry は固定、子バッチは全件 applied を要求、改名はファイル名だけ

受け入れ: `tests/migrations.rs`（0020）、`tests/playlist_sync.rs`（列挙は fixture の JSON を返す偽 yt-dlp
で: 無いものだけ投入、Inbox / 投入済みと重複しない、取りこぼしで失敗、上限、非公開は投入せず Library に
無ければ「取れない」一覧に出る、公開に戻ったら拾う）、`tests/playlist_align.rs`（位置と番号のずれ →
set_rows + rename バッチ。非公開・未取り込みの位置は番号が飛ぶ、`SOURCE_URL` 無しは触らない、
一致なら変更なし）、`tests/inbox_api.rs`（購読由来の件の初期値）、`web/src/lib/subscriptions.test.ts`、
リハーサル環境で 9 本を登録 → 同期 → 未取り込みの 3 本だけ Inbox に来る → 承認 → 番号とファイル名が
再生リストの順に揃う → 再同期で「変更なし」

確認済み（リハーサル環境）: 9 本を登録 → 同期（明透 14 件ずらし + 2 本投入）→ 承認 → 後続の自動同期で
変更なし

### P4-17 一覧のキーボード操作

2026-09-22。SPEC §12.2「キーボード」、D-80。

- [x] 表にフォーカスがあるとき ↓ / ↑ / PageDown / PageUp / Home / End でカーソル行を動かし、素の移動はその
      行だけを選択、**Shift + 移動は anchor からカーソルまでの範囲そのもの**（縮む）、Ctrl + 移動はカーソル
      だけ、Space はカーソル行のトグル。クリックもカーソルを置く
- [x] カーソルは読み込み済みの行の中でだけ動く（未読込の骨組み行は id が無い。末尾に着くと次のページが
      読まれるので End を繰り返せば進む）
- [x] 仮想化は sticky ヘッダぶんの `scrollMargin` / `scrollPaddingStart` で `scrollToIndex` がヘッダの下に行を
      出す

受け入れ: `web/src/lib/keynav.test.ts`（端で止まる、Page は 1 画面、カーソル無しの起点、読み込み済みの外へ
出ない）、`web/src/lib/selection.test.ts`（`rangeSelect`: 縮む、飛び地が消える、filter 形は ids 形へ、anchor
が無ければ移動前のカーソル行）、dev サーバ + 実機 API でブラウザ確認（ヘッダ直下 / 下端に揃う、Ctrl+A →
Shift+↑、Esc → Shift+↓）

### P4-18 ジョブ一覧の衛生

2026-09-22。D-81、D-68 追記。

- [x] (1) Inbox の周期監視をジョブの外へ: 指紋（`import::inbox::fingerprint`）が前回投入時と違うとき・
      配置待ち・期限切れの placed・起動直後だけ投入（`jobs::handlers::inbox::spawn_watcher`。Duplicate なら
      次の周回で投入し直す）。`GET /api/inbox` の `watch` を Inbox 画面が「最後に確認」で出す
- [x] (2) ytdl の「取り込み済み」は `done` + note
- [x] (3) `DELETE /api/jobs/:id`（終端だけ）と `DELETE /api/jobs?state=failed`、ジョブ画面の [消す] /
      [失敗をすべて消す]
- [x] (4) GC が `[gc].jobs_done_days`（7）/ `jobs_failed_days`（30）を過ぎた終端の行を消す（preview に `jobs`）

受け入れ: `tests/inbox_job.rs`（指紋は音声だけで決まる、監視は変化・配置待ち・期限切れ・起動直後だけ
投入し Duplicate を取りこぼさない）、`tests/inbox_api.rs`（`watch`）、`tests/ytmusic_download.rs`
（取り込み済み → done + note）、`tests/jobs.rs`（終端の削除、queued は 409、failed の一括、done は 400）、
`tests/gc.rs`（保持期間で消す、0 は消さない、gc 自身は残る）、`tests/gc_api.rs`（preview の `jobs`）、
`tests/config.rs`（既定と 0）、web の `canRemove` / `watchLabel`

### P4-19 Inbox の同名の警告

2026-09-22。D-70 追記、SPEC §7.8 / §9 / §12.6。`SOURCE_URL` の補填漏れ・別 URL の再アップロードによる
二重取り込みを人が気づけるようにするもので、リリース後の再移行（MIGRATION §5-3）の取りこぼし対策。

- [x] 追記先の album に同じタイトル鍵（`import::inbox::title_key` = NFKD + casefold + 空白の畳み込み。注記は
      落とさない）の active な行があれば `GET /api/inbox` の `tracks[].same_title` に返す
- [x] 承認画面がタイトル欄の下に「⚠ Library に同名: <ファイル名>（長さ）」、件の一覧に「同名 N」を出す
      （**承認は止めない**）

受け入れ: `src/import/inbox.rs` の `title_key` の単体テスト（全角・半角と空白は同じ、`(Cover)` /
`【Live ver.】` は別）、`tests/inbox_api.rs`（同名あり / `(Cover)` は出ない / missing は数えない / 追記先が
無ければ空）、`web/src/lib/inbox.test.ts`（`sameTitleLabel` / `sameTitleCount`）、実機のデータで誤警告の量を
測る（同一 album 内 27 グループ / 119 行。YouTube 由来 6 グループ）

### P4-20 CD 画面の作り直しと取り込みの導線

2026-09-23。ユーザ要望。SPEC §7.2 / §12.1 / §12.6、D-64 追記 4、D-82。

- [x] MusicBrainz の照会を 3 段の打ち切りに（`discid` → `ids` → `toc`）。`POST /api/cd/lookup` に
      `widen`、応答に `stage` / `can_widen`
- [x] `GET /api/cd/status` に TOC の音声トラック（照会の前から表を出す）
- [x] `GET /api/cd/cover/{release_id}`（Cover Art Archive の中継。D-82）
- [x] `GET /api/inbox/summary`（上部バーのバッジ用。一覧を読まない固定 SQL の集計）
- [x] CD 画面: トラック表を主役に（タイトルは `Track NN` のプレースホルダ）、候補は左にジャケットを
      付けて下へ、「確定」段を廃止（`CdView` を 4 ファイルに分割）
- [x] 候補から写す範囲の既定を「全部写す」に（D-72 追記 2。2026-09-23）
- [x] **CD 画面から編集を外し、読み取り専用のライブラリ風の表に**（2026-09-23。ユーザ要望）。
      値を直すのは Inbox の承認画面に一本化する（D-67 追記。取り込み先を Inbox にする実装は P2-5）。
      `CdAlbumFields` → `CdAlbumSummary`、`CategoryField` は `components/CategoryField.tsx` へ、
      cdState から編集系のアクションと paste の state を削除
- [x] 上部バー: `ライブラリ / アルバム / CD / YouTube / Inbox`、☰ に `ジョブ / 履歴 / 設定`。
      Inbox に承認待ちの赤バッジ。左カラムはツリーが効く画面（ライブラリ / アルバム）だけ

受け入れ: `tests/cd_musicbrainz.rs`（段の打ち切り・落ち方・`widen`・キャッシュの鍵）、
`tests/cd_lookup_api.rs`（`stage` / `can_widen` / `widen`）、`tests/cd_status_api.rs`（`tracks`）、
`tests/cd_cover_api.rs`（中継・404・MBID でない id・未構成・上流異常は 502・リダイレクトの上限と
ダウングレード）、`tests/inbox_api.rs`（要約）、`web/src/lib/cd.test.ts`（`Track NN` の埋め、見出し）、
`web/src/lib/cdState.test.ts`（`set_disc`、照会中もフォームが消えない、busy が残らない）、
`web/src/lib/views.test.ts`（並びと左カラム）

**完了**（2026-09-23。codex approve 済み）。実機（TrueNAS）で確認したこと:
- 照会の前からトラック表が出る、候補を選ぶと名前が入る、ジャケットが出る（500x500）
- 嵐「Five」で `stage=ids` / 候補 2 件。`widen` で 28 件（内訳 ISRC 2 / TOC 近似 26）= 以前ノイズだった
  26 件が「さらに広げて探す」の裏に隠れた
- タブの並び、CD 画面に左カラムが無いこと、入力欄が 0 個であること

**実機でしか出なかった不具合**: ジャケットが IPv6 固定では取れなかった（coverartarchive.org から
archive.org へ飛ぶ二段構えで、飛び先に AAAA が無い）。`CoverArtClient` は族を引数で受けず `Auto`
固定にした（D-82 追記）。ユニットテストはローカルの HTTP サーバを模していたので素通りしていた。

残り: なし（トラックごとの進捗・Inbox 経由の取り込み・「取り込む」ボタンは P2-5 で入れた。実機で
「取り込む」→ Inbox に件が出るまでを確認済み（P2-5 の受け入れ））

### P4-21 候補の無いまま取り込んだ CD の MusicBrainz 引き直し

候補ゼロや「どれも違う」で Inbox に置いた CD は MBID を持たない。後から MusicBrainz に DiscID を登録した・
リリースを見つけたときに、Inbox の承認画面から引き直して候補を選べるようにする（2026-09-23 のユーザ要望。
D-84）。YouTube の件は対象外（曲の身元は `SOURCE_URL`。D-70）

- [x] 吸い出しでドライブの ISRC / MCN をサイドカーに残す（`POST /api/cd/rip` がドライブの状態から payload の
      `ids` に載せ、`rip_disc` → `PlaceInput.ids` → `RipEntry.isrcs` / `mcn`。旧サイドカー・旧 payload は空で読める）
- [x] `GET /api/inbox` の件に `rip: { toc, isrcs, mcn }`（CD の件の照会の材料。`import::inbox::RipLookup`）
- [x] 下書き（`InboxDraft`）に `release_id` / `release_group_id`。提案はファイルのタグの最頻値、保存した下書きが
      勝つ（旧下書きは提案）。形は MBID（UUID。大文字も通す。値はスキャナと揃えるため正規化しない）でなければ
      承認できない。配置でタグ（`MUSICBRAINZ_ALBUMID` /
      `MUSICBRAINZ_RELEASEGROUPID`）に書き、リリースキー `mb:` と album の `mb_release_id` になる（同じリリースの
      2 枚目は 1 枚目に合流）
- [x] 承認画面の「MusicBrainz」節（`InboxView` の `MbLookup`）: ボタンで CD 画面と同じ `POST /api/cd/lookup` を
      引き（10 分のキャッシュと 1 req/s はサーバ側。ジョブにも保存にもしない）、候補を選ぶと `applyCandidate`
      （ID は常に、ディスク番号は候補の medium、名前は空欄と `Track NN` だけ埋める）。リリース URL / MBID の指定、
      「さらに広げて探す」。選択の解除は置かない（null はファイルのタグにフォールバックするので外れない）

受け入れ: `tests/inbox_sidecar.rs`（`isrcs` / `mcn` のキーと旧サイドカー）、`tests/cd_status_api.rs`（payload の
`ids`）、`tests/cd_rip_job.rs`（サイドカーに残る）、`tests/inbox_api.rs`（`rip` と提案の `release_id`）、
`tests/inbox_draft.rs`（MBID の形、提案と merge）、`tests/inbox_job.rs`（タグと `mb_release_id`、同じ MBID の
2 枚目の合流）、`web/src/lib/inbox.test.ts`（`applyCandidate`・`draftFrom`・`validateDraft`）。ローカルのサーバで
実機の盤（嵐「Five」、DiscID 未登録）の件を ISRC の経路で引き直し → 候補 2 件 → 選んで承認 → タグと
`mb_release_id` が `f1223d63-…` になるのを確認

### P4-22 リリース前: 2 回目のクリーンリハーサルで見つかった修正

2026-09-24〜25 のリハーサル（MIGRATION §0〜§5 をカスタムアプリで通し）で見つかったもの。リリースの再移行の前に
入れる。リハーサル中に直したもの（`backfill_source_url.py` の同名曲、MIGRATION / OPERATIONS の抜け、D-89 の
ALAC 末尾の長さ 0 のサンプル）は済み

- [x] Inbox の承認画面: 購読由来の件の番号の表示。今は宛先の文言が常に「番号は <max+1> から」で、購読由来の件
      （`source.subscription_id` / `position` あり）は実際には再生リストの位置（空けてある番号）に入るのに 180 から
      と読める（明透: 178 曲・1〜179 で 165 が空き、件は 165 なのに「180 から」）。購読由来なら「番号 165 に入る
      （再生リストの位置。空き番号）」、そうでなければ従来どおり
- [x] Inbox の承認画面: 「⚠ Library に同名」に**この曲の長さも並べる**（今は Library 側の長さだけで、件の長さは③の
      表の右端の列。並べないと二重取り込みか別テイクかを判断できない）
- [x] Inbox の承認画面: 購読由来の件に、同期が番号を空けた経緯を出す（「同期で既存の 14 曲を 1 つ後ろへずらして
      165 を空けた（バッチ #14 / #15）」）。今は YouTube 画面の購読の詳細か履歴画面を見に行くしかない。
      実装: `GET /api/inbox` に参照される購読の直近の同期の要約（`subscriptions`）と宛先の既存の番号
      （`destination.numbers`）。要約は**直近の**同期なので、件を落とした後に同期し直していれば「揃え直しは無かった」
      と出る（番号が空いていることは宛先の番号との照合で示す）。`web/src/lib/inbox.test.ts`、`tests/inbox_api.rs`
- [x] `backfill_source_url.py`: 位置どおりの一致（手順 1 の verified）でも、長さが両方分かって食い違えば採らない
      （位置推定の手順 3 も同じ）。
      VALIS の 224 番（270.7 秒の「彷徨フォーエバー Live ver.」）が、位置 224 の別動画（628 秒の
      「BACKSTAGE DOCUMENTARY」。タイトルに曲名を含む）の URL を付けられ、本来の動画（位置 225 の
      `XTVG_tEhKAw`）を同期が別の曲として落としてきた（PCM MD5 が完全一致の二重取り込み）。前回のリハーサルで
      「再アップロードによる既知の二重取り込み」としていたものはこれ。MIGRATION §5-3-3 の該当の記述も直す
- [x] 再生リストに Library へ入れない動画（上の 628 秒のドキュメンタリー）があるときは、**再生リスト側から外す運用**
      とする（2026-09-25 ユーザの判断。購読は人が選んだ再生リストなので、除外リストの仕組みは作らない）。
      リハーサル環境は外した後に 224 番の `SOURCE_URL` を `XTVG_tEhKAw` に付け替え（バッチ 21）、再同期で
      224 件中 224・変更 0。SOURCE_URL の無い状態からの計画でも 224 番は `XTVG_tEhKAw` に verified で付く
- [x] Inbox の却下した件のファイルを片付ける（2026-09-25 ユーザの決定。D-90。`tests/inbox_discard.rs`、
      `tests/inbox_api.rs` の discard）。今は却下してもファイルが Inbox に残り続け、
      人が SMB で消すしかない。二段にする: (1) 却下（従来どおり。ファイルは残し「下書きに戻す」で戻せる）、
      (2) 却下した行の「削除」で破棄待ちにする（`inbox_items` に破棄要求の時刻。ファイルはまだ消さない。一覧は
      「却下」の絞り込みで見える）→ GC が `[gc].retention_days`（既定 7。2026-09-25 に 30 から変更）経過後にファイルと行を消す。それまでは
      「削除を取り消す」で却下に戻せる。物理削除は GC だけ（CLAUDE.md の禁止事項）を保つ。実装時に D-xx を起こす
      （スキーマは新しい連番のマイグレーション。GC の区分と対象ファイルの範囲 = 件のディレクトリの中だけ、
      破棄待ちの間に同じディレクトリへ新しいファイルが来たときの扱い、を決める）
- [x] メタデータプラグイン（spindle-ytmusic-meta、別リポジトリ）を実際の YouTube のタイトルで直す。9 本の再生リストの
      実タイトル 1,512 件で ok 734 / unmatched 521 / unknown_channel 257 だった。ルール・フィクスチャ・channels.toml の
      キーが旧パイプラインのファイル名（`/` → `⧸` に置換済み）由来で、実メタデータの `/` と末尾の `｜from 神椿` に
      合わない（362 件はこれだけで救える）、`存流 -ᴀʀᴜ-` 等のチャンネル定義が無い、ルールが本当に無い書式が約 300 件。
      正解データは補填の計画（`plan.csv` の動画タイトル ↔ Library の TITLE。リポジトリ外）。直したら musl の static
      バイナリを `/mnt/ssd/apps/spindle/bin/` に置き直す（2026-09-25 着手）
      → 済み（2026-09-25、spindle-ytmusic-meta `4d0aa02`）: 入力の `/`・`／` と `｜from 神椿`・`【神椿】` の正規化、チャンネル
      定義の追加（存流ほか）、ルール 25 → 51、フィクスチャ 98 → 170。実タイトル 1,513 件（`lang=ja`）で ok 98.9%・TITLE 一致
      90.0%・ARTIST 一致 95.4%（直す前は 38.9% / 25.2% / 25.6%）。`[ytmusic].ytdlp_args` に `youtube:lang=ja` が要る（無いと
      翻訳タイトルの英語が返る）。実機で明透「再会」・理芽「Tropical Therapy」を落とし直し、手直し無しで承認 → 配置 → 再同期で
      9 本とも差分 0 を確認
- [x] Inbox の既存 album への追記で `DISCNUMBER` を宛先に合わせる（D-70 追記。`omits_disc`）。YouTube の件は提案が disc 1 固定で、配置が
      `DISCNUMBER=1` を書くため、ディスク番号の無い既存 album（明透 178 曲・理芽 233 曲など。Library 全体で 6,659 曲が
      disc なし）に追記すると、並べ替え（disc → track）で追記した曲だけが末尾に回った（2026-09-25 実機。バッチ 22 で 2 曲の
      `DISCNUMBER` を消して復旧）。宛先の album の曲が全部 disc なしなら書かない / 提案も空にする。CD の件（複数枚）は従来どおり
- [x] 同じディレクトリでファイル名の書式が混ざる（旧パイプラインの `83. タイトル` と、spindle の `165 タイトル`）→
      2026-09-25 ユーザの決定: `[layout]` を `{track:02}. {title}` にして既存の多数派に合わせ、混ざった分は album ごとの
      リネームで揃える（D-43 追記。config.example / SPEC を更新。実機の config.toml とリネームは配備で行う）
- [x] `audio_version` が上がった行の RG を**自動で解析し直す**（2026-09-25 ユーザの決定。D-47 追記 2）。スキャナの
      外部の差し替え（deep scan での `audio_md5` の変化を含む）と tagwrite の overlay 解消で値を捨てるとき、同じ
      トランザクションで rg ジョブを積む（`db::replaygain::reset_and_reanalyze`。投入単位と dedup は `POST /api/rg` と
      同じ）。リハーサルでは D-89 で 7 本の `audio_md5` が変わったとき手で `POST /api/rg` した。`tests/rg_write.rs`
      （track 単位・album gain on の album 単位・タグだけの変更では積まない・tagwrite の経路）
- [x] CD の取り込みの表の画像を承認画面の初期値に入れる（2026-09-25 ユーザの決定。D-91）。CD 画面では Cover Art
      Archive のジャケットが見えていたのに、吸い出したファイルには画像が無く、承認画面で「Cover Art Archive から取る」を
      押さないと入らなかった（実機の嵐「Five」）。inbox ジョブの走査の後、サイドカーの `rip.metadata.release_id` が
      ある承認前の取り込みに一度だけ front を取り（マイグレーション 0003 の `inbox_items.caa_picture` / `caa_tries`）、
      提案の全曲の `picture` に入れる（保存した下書きが勝つ）。404・リリースなしは 1 回で打ち止め、上流の失敗は
      もう 1 回だけ。承認画面は「Cover Art Archive の表の画像（…自動で取った）」と「画像を外す」。`tests/inbox_cover.rs`
      （取得 → 提案・一度だけ・GC しない、404、失敗の再試行と上限、リリースなし / CD でないは取りに行かない、下書きが
      勝つ、inbox ジョブの配線と既存の取り込み）、vitest の `usesCaaPicture`
- [x] Library 直下のディレクトリ名をスキャナが自動で category の語彙にする（2026-09-25 ユーザの決定。D-92）。作り直した
      DB は語彙が空で、実機の 721 album 中 707 が category なし（Inbox / 購読 / CD の選択肢に Anime 等が出ない、
      一括リネームが 7,505 曲を `_Unsorted/` へ移す提案をした）。スキャンで直下の名前を登録し album に付け、
      NULL のまま残った album も次のスキャンで埋める（人が付けた値は変えない）。`_Unsorted` は登録しない。
      受け入れ: `tests/scanner.rs`（空の語彙からの初回スキャン、既存 DB の NULL の埋め戻しと人の値の保護・
      `changed_ids`、大小・NFD 違いの重複なし、`[layout].unsorted` の先頭、Phase 3 中の API の追加、部分索引の
      EXPLAIN QUERY PLAN）、`tests/categories_api.rs`（使われていない語彙だけ削除）、`web/src/lib/categories.test.ts`。
      codex の指摘で、語彙の読み直し・`library` イベントでの一覧の取り直し・使われていない語彙の削除（設定画面）・
      部分索引（0004）を足した

---

## 着手前に確認が必要な残課題

- ~~Inbox 経由で置いた CD の album は `albums.discid` が NULL だが、DB 再構築後はスキャナがタグの
  `MUSICBRAINZ_DISCID` から復元する~~（2026-09-23。`albums.discid` を落とし、リリースの同一性を
  MBID → album 行にした。CD とそれ以外は Inbox の追記先で混ぜない。D-67 追記 3、マイグレーション 0024）

- ~~Discogs / VGMdb 連携の要否~~（2026-09-20。作らない。D-72）
- ~~`.fpl` 書き出しの要否~~（2026-09-20。作らない。D-72）
- ~~Inbox のポーリング間隔~~（P2-10 で決めた。60 秒 + 手動。D-68）
- ~~一括リネーム後の旧ディレクトリに残る同梱ファイル（cover.jpg / disc.cue / rip.log）と
  空ディレクトリの扱い~~（P2-8 で決めた。D-67）
- ~~Library の ALAC（m4a）に任意キーを書けない~~（2026-09-21 に P4-3 の実機確認で観測 → P4-11 に昇格）
- ~~Inbox の承認画面で「Library に同名の曲がある」警告~~（2026-09-22 に P4-19 で実装。判定は
  「追記先の album の中で同じタイトル鍵」に絞った。albumartist 単位だと 720 行が該当して無視されるため）

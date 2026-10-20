## 8. ジョブシステム

| type | 並列度 | 冪等キー |
|---|---|---|
| `scan` | 1 | 固定 |
| `rip` | **1**（物理ドライブ1台） | discid |
| `verify` | 2 | album_id |
| `rg` | CPU コア数 | album_id |
| `rgwrite` | 1 | 固定（`rgwrite`。開始時にキーを外すので、実行中の投入は次のジョブになる）。`rg_write_due` の行を album ごとの編集バッチにする（D-97） |
| `transcode` | CPU コア数 - 1 | track_id + variant + audio_version（variant は P4-7 から。§7.6） |
| `tagwrite` | 4 | track_id + tag_version（`edit_batch_id` でバッチに紐づく） |
| `rename` | 1 | batch_id（バッチ 1 つに 1 ジョブ。2 phase の順序を守るため直列。D-43） |
| `normalize` | 2 | track_id + op_id（同じトラックの直列化は track_locks） |
| `thumbnail` | 4 | artwork_id |
| `flaccheck` | CPU コア数 | track_id + audio_version（版付き。D-57） |
| `hirescheck` | max(1, CPU コア数 / 2) | track_id + audio_version（版付き。§7.10、D-71。rg / transcode / flaccheck と重なる分を抑える） |
| `inbox` | 1 | 固定 |
| `ytdl` | 1 | `ytdl:<url>`（playlist の展開・購読の同期で投入する分も同じ。D-70、D-78） |
| `playlist_sync` | 1 | `playlist_sync:<subscription_id>`（列挙 → 番号揃え → ytdl 投入。§7.7「再生リストの購読と同期」、D-78） |
| `inbox_rejudge` | 1 | `inbox_rejudge:<item_id>`（Inbox の件の再判定。`max_attempts` 1。§7.8、D-114） |
| `source_hash` | 2（CPU 系の共通予算も取る） | `source_hash:<track_id>:<source>`（source = `master` / `opus` / `aac`。送る元の SHA-256。§7.11、D-95） |
| `device_scan` | 2（同じ端末は排他。下記） | `device_scan:<device_id>`（回復してキャッシュを置き換える。未接続なら待たずに終わる。§7.11） |
| `device_sync` | 2（同上） | `device_sync:<device_id>`（payload に `plan_id`。最大 3 回。確定した計画を実行。§7.11「同期」） |
| `device_verify` | 2（同上） | `device_verify:<device_id>`（全曲の sha256。§7.11「内容の検証」） |
| `gc` | 1 | 固定（scan と同じ排他 `library` を取れなければ Requeue。D-56） |
| `backup` | 1 | 固定 |

- **CPU 系の共通予算**（`rg` / `transcode` / `flaccheck` / `hirescheck` / `source_hash`。D-73、P4-1）:
  種別の並列度に加えて共有の予算（= CPU コア数）を取ってから走る。種別単独なら今までどおり、複数種別が
  同時に走るときだけ実行中の合計がコア数に収まる。取得順は種別 → 共通で、共通が取れなければ claim せず次の周回で試す
  （ジョブは queued のまま。種別内の順序は変えない）。予算を分け合う種別は 1 件ずつラウンドロビンで
  claim する（次の周は最後に claim した種別の次から。1 種別が予算を独占しない）。`GET /api/jobs` の `cpu_budget`
- **端末ジョブ**（`device_scan` / `device_sync` / `device_verify`。D-98）: 端末ごとの排他（`job_mutexes` の
  `device:<id>` とプロセス内のロック）を取ってから、最初に端末側の回復を行う（別の端末なら並べてよい）。
  ロックが取れなければ 15 秒後、未接続なら `device_sync` / `device_verify` は attempts を数えずに 300 秒後へ
  回し（`RequeueAfter`）、`adb track-devices` がその端末を見たら `run_after` を前倒しする。`device_scan` は
  未接続なら待たずに `DoneWith("未接続")` で終わる（次の接続でまた投入される）。`generation` の不一致と空き不足は
  再試行しても変わらないので直ちに `failed`。端末が削除されていれば何もせずに終わる。詳細は §7.11
- 起動時リカバリ: `running` を `queued` へ戻し、`track_locks` / `derived_path_locks` / `job_mutexes` を
  **全件削除**する
  （ロックはプロセス生存中しか意味を持たない）。単一インスタンス前提。同じ DB を
  複数プロセスで開くことは想定しない（compose で replicas を増やさない）
- **終端遷移の書き込みの再試行**（D-76、P4-9）: ハンドラの結果を DB に書く終端トランザクション
  （進捗の flush・ロック解放・`done` / `failed` / `cancelled` / 再キュー）が失敗したら、種別の許可と
  CPU 予算を**持ったまま** 1, 2, 4, 8, 16, 32 秒の間隔で再試行する（6 回、約 1 分。ディスク満杯や
  一時的なロックなら自力で復帰し、仕事の結果を失わない）。使い切ったら `failed` としての記録を 1 度
  試み、それも駄目なら `running` のまま手放す（下の稼働中の回収が拾う）
- **稼働中の回収**（起動時リカバリのループ版。D-76、P4-9）: ワーカーは周回ごとに、DB で `running`
  だが**プロセス内で実行中でない**行（終端を書けなかった行）を `queued` へ戻し（`cancel_requested_at`
  が立っていれば `cancelled`）、その行の `track_locks` / `derived_path_locks` / `job_mutexes` を消す。
  `started_at` は NULL、`attempts` / `run_after` は触らない（起動時リカバリと同じ）。回収した旨を
  `last_error` とログに残す。実行中の登録は claim と同じスケジューラの流れで spawn の前に行い、
  「claim 済みだが未登録」の瞬間を作らない
- `dedup_key` は **`queued` / `running` の間だけ**一意（partial unique index）。
  列 UNIQUE にすると `done` / `failed` 後に同じキー（`scan` の固定キー、同 version の
  手動再試行）を永久に投入できない。キーは `type` を含めて構成する
  （`tagwrite:<track_id>:<tag_version>` 等）
- 失敗は `attempts` をインクリメントし、`run_after` に次回時刻を書いて指数バックオフ
  （再起動を跨いでも待ち時間が保たれる）。`attempts >= max_attempts` で `failed`。再試行しても変わらない
  失敗（`JobError::Fatal`。対応していない URL 等）はバックオフせず直ちに `failed`（D-70）
- 終端の行の片付け（P4-18、D-81）: `DELETE /api/jobs/:id`（failed / done / cancelled だけ。queued / running は
  409）、`DELETE /api/jobs?state=failed`（失敗をまとめて）。上部バーの赤丸は `summary.failed > 0` なので、
  確認済みの失敗を消せば消える。保持期間を過ぎた終端の行は GC が消す（`[gc].jobs_done_days` /
  `jobs_failed_days`）
- キャンセルは `cancel_requested_at` を立てる協調方式。ハンドラは進捗更新のたびに
  確認して自発的に止め、`cancelled` へ遷移する。外部プロセス（ffmpeg 等）は
  子プロセスグループごと kill し、tmp の成果物を消す
- 進捗は SSE で配信。DB にも永続化してリロードに耐える
- 同一トラックに対する競合ジョブは `dedup_key` と track 単位の
  advisory lock テーブルで直列化。複数トラックを掴むジョブ（album 単位の `rg` 等）は
  `track_id` 昇順に取得し、1 つでも取れなければ全解放して再キュー（デッドロック回避）
- 版を持つジョブ（`tagwrite` / `transcode`）は開始直前に payload の版と現在値を比較し、
  古ければ no-op で `done`（§7.5 stale ジョブ）
- **編集バッチは coordinator + 子ジョブ。** `tagwrite` ジョブは track 単位、`rename` ジョブは
  バッチ単位で投入し `jobs.edit_batch_id` でバッチに紐づける。バッチ自体はジョブではなく、
  終端状態は子ジョブ・op の結果から集計する（最後に終端になった子ジョブが集計する）。
  起動時リカバリ・キャンセルはバッチ配下の子ジョブに対して行う

---


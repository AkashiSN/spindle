# 6. データモデル

`db/migrations/0001_init.sql` を参照。主要な設計判断のみ以下に記す。

## 同一性解決の優先順位

1. `(dev, inode)` — 外部の rename や in-place のタグ書き換えでは inode は変わらない。
   spindle 自身の tagwrite（tmp + rename、[§7.5](07-pipeline/05-tagwrite.md)）は inode を変えるので、成功トランザクションで
   DB を追随させる
2. `audio_md5` — FLAC は STREAMINFO に非圧縮音声の MD5 を持つ。タグ変更で不変。
   ALAC/WAV はデコードして算出。非可逆は算出しない（識別子としては使わない）
3. `rel_path_key` 一致
4. いずれも当たらなければ新規トラック

**どの識別子も「トラック実体の一意 ID」ではない**ので、各段で候補を検証する（[D-30](../decisions/D-030.md)）。
そのために走査は**多相**で行い、判定は inventory 全体が揃ってから下す（[§7.1](07-pipeline/01-scan.md)）:

| 段 | 候補の採用条件 | 外れる例 |
|---|---|---|
| inode | inventory 内でその `(dev, inode)` を持つパスが **1 つだけ**（2 つ以上なら hardlink とみなし、全候補で inode 段を無効化）、候補行が未 claim、`nlink = 1`、かつ `size` か `mtime_ns` のどちらかが一致。両方違えば `audio_md5` を計算して一致を要求 | inode 再利用（削除後に別ファイルが同じ inode を得る）、hardlink |
| inode（dev の付け替え） | 同じ `dev` の候補が無いとき、**別の `dev` に同じ `inode` を持つ行がちょうど 1 つ**あり、`size` / `mtime_ns` / `ctime_ns` が**すべて**一致。同じ実体とみなし「変更なし」で `dev` だけ現在値へ直す（[D-62](../decisions/D-062.md)） | 属性が 1 つでも違う（inode 再利用と区別できない）、候補が複数 |
| audio_md5 | 候補が**ちょうど 1 行**で、その行の `rel_path_key` が **inventory 全体に存在しない**（= 移動元が消えている。走査途中の未訪問ではなく、全 stat が終わった後に判定する） | コピー元が残っている、ベスト盤との重複、無音トラック |
| rel_path_key | 候補が未 claim | — |

- claim は `scan_runs.id` 単位（`tracks.seen_run_id`）。`seen_at` は時刻で、claim 判定には使わない
  （epoch 秒では同秒の再実行と区別できない）
- 複数のパスが同じ候補行を取り合ったら `rel_path_key` の昇順で先のものが取り、後は次の段へ落ちる。
  順序が固定なので並列に stat しても結果は決定的
- audio_md5 の候補が複数、または元パスが inventory にまだ存在する場合は**移動ではなく新規トラック**
  として登録する。自動マージはしない。同一 `audio_md5` の active 行が 2 つ以上あるものは
  `duplicate_groups` ビューに現れ、UI がバッジと一覧で示す（[D-29](../decisions/D-029.md)）
- `nlink > 1`、または inventory 内で同じ inode を複数パスが持つファイルは **inode 段と md5 段を飛ばし
  `rel_path_key` だけで解決**し、警告バッジを出す。移行時は preflight のブロッカー（[D-26](../decisions/D-026.md)）
- **復活**: どの段で採用されても、候補行に `missing_since` があれば NULL に戻す
- **missing の確定**: `scan_runs.state = 'completed'`（root を開けて walk がエラーなく完了）の finalize
  でだけ、その run で claim されなかった active 行に `missing_since` を立てる。途中失敗・キャンセルの
  run では立てない（SMB 一時切断で全曲が missing になる事故を防ぐ）

## 変更検出と版の遷移

音声の同一性を見るフィンガープリントは形式で分ける:

| 形式 | 音声フィンガープリント | 用途 |
|---|---|---|
| FLAC | STREAMINFO の MD5（`audio_md5`） | 同一性（移動検出）と音声版 |
| ALAC / WAV | デコードした PCM の MD5（`audio_md5`） | 同上 |
| 非可逆（opus / aac / mp3 / ogg） | エンコード済みパケット列の SHA-256（`audio_fp`。symphonia で demux のみ、デコードしない。Opus も同様） | **音声版のみ**。同一性には使わない |

| 観測 | 判定 | 版 |
|---|---|---|
| `(dev, inode, size, mtime_ns, ctime_ns)` すべて一致 | 変更なし | `seen_at` / `seen_run_id` のみ |
| `dev` だけ違う（inode 段の「dev の付け替え」で採用） | 変更なし | `dev` を現在値へ直し、`seen_at` / `seen_run_id` |
| いずれか変化 → タグを読み `tag_hash` を再計算、音声フィンガープリントを再計算 | どちらも同じ | 版は動かさない |
| 〃 | `tag_hash` のみ変化 | `tag_version++` |
| 〃 | 音声フィンガープリントのみ変化 | `audio_version++` |
| 〃 | 両方変化 | 両方 `++` |
| spindle 自身の tagwrite / rename / RG 書き込み / MD5 補填（[§7.9](07-pipeline/09-flac-check.md)） | 音声は不変と**既知** | フィンガープリントを再計算せず `audio_version` 据え置き（tagwrite は `tag_version++` 済み） |

非可逆のタグ書き換えはコンテナサイズを普通に変えるので、`size` の変化を音声の変化と
みなしてはならない（不変条件 3）。パケット列のハッシュは demux だけで済み、外部変更が
あったファイルにしか走らない。

`ctime_ns` を含めるのは、mtime を保存して書き戻すタグツールや `cp -p` を検出するため。
ZFS のスナップショット rollback は ctime も戻すので検出できない。これは **deep scan**
（全ファイルの `tag_hash` と音声フィンガープリントを再計算。`[scan].deep_interval_days` 既定 30 日、
UI から手動実行可）で吸収する。rollback を行ったら deep scan を手動で回すこと（運用手順）。

`tag_version` は「同じ値の書き直し」で進めない。tagwrite の再実行や外部ツールによる
同値の再保存で Derived の追随が空回りしないため。

## 出自の3属性（直交）

| 属性 | 値 |
|---|---|
| `source_type` | cd_rip / download / youtube / unknown |
| `lossless` | コーデックから自動判定 |
| `verification` | verified_ar / verified_ctdb / mismatch / **unverifiable** / not_attempted |

`unverifiable` はハイレゾ・配信音源など TOC が存在しない音源。
「未検証」とは意味が異なるため UI でも別バッジとする。

## 版管理

- `audio_version`: 音声バイト列が変わったら ++ → Derived 再エンコードが必要
- `tag_version`: タグのみ変更で ++ → Derived はタグ上書きのみ

`derived_files` に生成時の両版番号を記録し、差分で必要な処理を判定する。
数千件の一括タグ編集後に再エンコードが走らないための中核機構。

配布ビュー `delivery` は Derived の存在だけでなく**版の一致**を見る:

| 状態 | 配布するもの |
|---|---|
| `src_audio_version = audio_version` かつ `src_tag_version = tag_version` | Derived |
| 音声版は一致、タグ版のみ不一致 | Derived（`stale_tags = 1`。タグ上書きジョブが追随する） |
| 音声版が不一致（再エンコード待ち） | **Library 原本へフォールバック** |

音声が古い Derived を配ると、修復・差し替え前の音声を正常品として配ることになる。
タグだけ古い場合に原本へ落とさないのは、容量の大きい可逆を配布してしまうため。

## 全文検索（FTS5）

`tracks_fts` は external content 方式（`content='tracks'`）。索引対象の
`title / artist_display / album / albumartist` は**すべて `tracks` に実在する列**で
なければならない（列値の取得と `rebuild` が content 表を直接読むため）。
このため `tracks.album` を `albums.album` の複製キャッシュとして持ち、トリガで同期する。
FTS の更新トリガは索引対象列の `UPDATE OF` にだけ張る。`seen_at` 更新のたびに
削除・再挿入が走ると、スキャンの最速パスが FTS 書き込みで律速される。

## 論理削除

`missing_since` による論理削除。SMB 一時切断やスキャン中のマウント欠落で行を物理削除すると、
プレイリストと編集履歴が巻き添えになる。既定 7 日経過後に GC（`gc` ジョブ。missing トラック・
アルバムの行、`Archive/` の退避ファイル、Derived の孤児、アートワークの孤児、却下して「削除」した
Inbox の取り込みのファイル（[D-90](../decisions/D-090.md)）を回収する。
1 日 1 回自動、`POST /api/gc` で手動、`GET /api/gc/preview` が dry-run。[D-56](../decisions/D-056.md)）。GC は保持期間を過ぎた
終端の**ジョブ行**も消す（done / cancelled は `[gc].jobs_done_days`、failed は `jobs_failed_days`。0 で
消さない。P4-18、[D-81](../decisions/D-081.md)）。端末の同期の計画（`device_sync_plans`）も、終端（`completed` / `abandoned`）して
`[gc].retention_days` を過ぎたものを消す。端末ごとに最後の終端 1 件と open な計画は残す（[D-102](../decisions/D-102.md)）。
GC は `tracks` の**最大 id の行を消さない**（missing で期限を過ぎていても一覧・削除の両方から外す）。
`tracks.id` は `AUTOINCREMENT` ではなく、SQLite は次の行に `max(rowid)+1` を振るので、最大 id の行を
消すと次の曲が同じ id を得て、端末の写し（`device_items`・Mac の state）が別の曲を指す。残した行は
後で新しい曲が入れば最大でなくなり、次の GC で消える（[D-105](../decisions/D-105.md)）。

## ReplayGain の内部表現

- 内部は **RG 2.0 / -18 LUFS 基準の dB 値**で一元管理
- 書き出し時にフォーマットごとへ変換:
  - Opus: `R128_TRACK_GAIN` / `R128_ALBUM_GAIN`（Q7.8 固定小数、**-23 LUFS 基準**）
  - FLAC / Ogg: `REPLAYGAIN_TRACK_GAIN` 等の Vorbis Comment
  - MP4 (AAC/ALAC): RG 互換タグ
- Opus への変換式（`G18` = 内部の dB 値 = `-18 - 測定 LUFS`）:
  `R128 値 = round((G18 - 5.0) * 256)`、符号付き 16bit に飽和（-32768..32767）。
  基準が 5 dB 低いので**必ず減算**する。テストベクトル: 測定 -18 LUFS → `G18 = 0`
  → `R128 = -1280`。測定 -23 LUFS → `G18 = +5` → `R128 = 0`。
  `OpusHead` の output gain は 0 のまま触らない（合成すると再解析時に二重に掛かる）
- `rg_scanned_at` と `rg_written_at` を分離。数万件のスキャン後に書き込みが中断しても
  再スキャンなしで書き込みのみ再開できる
- **解析が済むと自動でタグに書く**（`[replaygain].write_tags = true` のとき。[D-97](../decisions/D-097.md)）。書くべき行は
  `tracks.rg_write_due` の印で持つ（rg の保存で確認が成り立たなくなった行と album gain の off で立ち、書き込みの
  バッチを記録したときに下りる。巻き戻し・外部の変更では立たないので勝手に書き直さない。印が立った後に外部が
  RG のキーを書き換えたら走査で下ろす）。Library 全体で 1 本の `rgwrite` ジョブが印の付いた active な行を album
  ごとに 1 つの編集バッチにする（同じ album の rg が残っている album と反映待ちの編集がある album は後回しにし、
  キー付きの後継を 1 本だけ積む）。`write_tags = false` はタグに一切書かない（自動も手動も）
- `rg_written_at` は `rg_scanned_at` より小さくしない（`rg_scanned_at` は「前の値 + 1」へ進んで壁時計より先に
  ありうる。同じ秒の確認を未書き込みに見せない。[D-97](../decisions/D-097.md)）
- **タグ書き込みは通常の編集バッチ**（`POST /api/rg/write { selection }` → tags op。旧値の
  記録・overlay・巻き戻しはタグ編集と同じ）。書くキーは形式ごとに固定で、値の無いキーは消す
  （Opus: `R128_TRACK_GAIN` / `R128_ALBUM_GAIN` を書き `REPLAYGAIN_*` を消す。他形式:
  `REPLAYGAIN_TRACK_GAIN` `+0.00 dB` / `REPLAYGAIN_TRACK_PEAK` `0.000000`（線形、true peak）/
  `REPLAYGAIN_ALBUM_*`。album の値が無いトラックは album のキーを消す。[D-48](../decisions/D-048.md)）
- **`rg_written_at` は「ファイルの RG タグが解析値と一致していると確認した時刻」**。書き込み
  バッチの applied だけでなく、DB をファイルの現在値へ揃えるすべての経路（overlay の解消・
  外部変更の追随・巻き戻し）で判定し直し、一致しなければ NULL に戻す。再解析で値が変わった
  行も NULL にする（秒単位の時刻では同じ秒の再解析を `<` で検出できない）。DB のタグが既に
  一致している行は op にせず `rg_written_at` だけ立てる。スキャナも外部のタグ変更を取り込んだ
  行（`tag_hash` が変わった行）で判定し直す（[D-48](../decisions/D-048.md)）
- **外部で音声が差し替わった（`audio_version` が進んだ）行の解析値は捨てる**（`rg_*` と
  `rg_scanned_at` / `rg_written_at` を NULL。スキャナと tagwrite の overlay 解消の両方。[D-47](../decisions/D-047.md)）。
  古い解析値を Derived や再生に使わない。album の他のトラックの `rg_album_*` は次の album 解析で揃う。
  捨てた行は**同じトランザクションで rg ジョブを積んで解析し直す**（投入単位と dedup は `POST /api/rg` と
  同じ。タグへの書き込みは `[replaygain].write_tags` に従う。Derived は解析と書き込みが揃ってから作られる。
  [D-47](../decisions/D-047.md) 追記 2、P4-22）
- **スキャンで新規に登録したトラックにも rg を積む**（album の照合の後に投入単位を決める。既存の未解析の行は
  拾わない。Inbox を通さずに置かれた曲も漏れなく解析 → 書き込み → Derived に乗せるため。[D-97](../decisions/D-097.md)）
- album gain は `album_id` 単位で、**`albums.album_gain` が true の album だけ**計算・書き出しする（既定
  false。CD 取り込みは true、Inbox の承認画面で選ぶ。[D-74](../decisions/D-074.md)、P4-5）。**2ch 以外は album 集計から除外**（判定はデコード結果の
  チャンネル数。除外されたトラックの album の値は NULL）。構成トラックが 1 本でも
  デコードできなければ album 全体を書かない（[D-47](../decisions/D-047.md)）
- 無音（絶対ゲート以下、積分ラウドネス `-inf`）の gain は 0 dB

## 端末への配信の表（§7.11）

`db/migrations/0002_devices.sql`（P5-1、[D-95](../decisions/D-095.md)）。端末に何があるかの正は**端末側**（Android は
`<root>/.spindle/manifest.json`・ジャーナル・実ファイル、iPhone は Mac のエージェントの
state とミュージック.app。spindle へは報告で届く）で、DB の曲とプレイリストの表はそのキャッシュ（不変条件
1 を端末にも当てはめる）。

| 表 | 中身 | 正 / キャッシュ |
|---|---|---|
| `devices` | 端末 1 台 1 行。`uuid`（128 bit 乱数。端末側の manifest / Mac の `.spindle-device` との照合に使い、再利用しない）、`name` / `name_key`（`canonical_key`。一意）、`transport`（`adb` / `agent`）、`variant`（`opus` / `aac`）、`selection`（`all` / `playlists`）、`generation`（下記）、adb は `adb_serial`（一意）/ `adb_volume`（`emulated` か SD の UUID）/ `adb_root`（ボリューム内の相対パス。登録時は `Music/spindle`）、agent はトークン（`agent_selector` とシークレットの SHA-256）とワンタイムコード（`pair_selector`、argon2id の `pair_code_hash`、期限、試行回数。[D-99](../decisions/D-099.md)）、`last_synced_at`（adb は同期の完了、agent は報告の受理） | DB にしか無い（端末の設定） |
| `device_playlists` | 端末 × プレイリストの印 | DB にしか無い |
| `device_items` | 端末に反映済みの曲: `track_id`、`dest_path`（root 相対）/ `dest_path_key`（端末内で一意）、配信トークン、`size`、`sha256`、`synced_at`。`track_id` は FK にしない（GC で `tracks` の行が消えても削除の差分を出すため） | キャッシュ |
| `device_playlist_state` | 反映済みのプレイリスト: `dest_path`（adb は `Playlists/<名前>.m3u8`、agent はプレイリスト名そのもの。[D-99](../decisions/D-099.md)）とトークン。`playlist_id` は FK にしない | キャッシュ |
| `device_errors` | 最後の同期・報告で出た項目（`track` / `playlist`）ごとのエラーの理由。差分表の理由に出す | キャッシュ（同期の完了・エージェントの報告と破棄で全置換。差分の計算だけの `device_scan` は前回の分を残す） |
| `source_hashes` | 送る元（`master` / `opus` / `aac`）ごとの SHA-256 と、取ったときの意味トークン・物理同一性（inode・size・mtime_ns・ctime_ns。dev は持たない。[D-62](../decisions/D-062.md)） | キャッシュ（`source_hash` ジョブで取り直せる） |
| `device_sync_plans` | 確定した計画。`plan` は不変の JSON（版 `v`・`generation`・`plan_token` と、曲の操作（`op_id`・種類・`track_id`・from・to・トークン・size・sha256）とプレイリストの操作（`op_id`・種類・`playlist_id`・from・to・トークン）の列）、`state`（`open` / `completed` / `abandoned`）、`job_id`（adb）、`report_digest`（agent の報告の再送の照合）。端末ごとに `open` は 1 つまで（部分 UNIQUE 索引） | DB にしか無い |

- `devices.generation` は端末の選曲に効く設定（`selection` / `variant` / 印）を API で変えるたびに 1 進める
  （名前の変更では進めない。印のついたプレイリストの削除でも、open な作業が無ければ進める。[D-102](../decisions/D-102.md)）。計画とジョブと報告はそれを持ち、食い違えば実行・受理しない（[§7.11](07-pipeline/11-device-delivery.md)「計画」）
- `device_items` / `device_playlist_state` は端末側の正本を読んだ時点で**全置換**する（adb は回復の後と同期の
  完了、agent は報告・破棄の受理）。部分的な更新はしない
- 端末の行と計画・キャッシュが消えるのは端末の削除（`ON DELETE CASCADE`。端末上のファイルには触らない）。
  印はそれに加えてプレイリストの削除でも消える（`device_playlists.playlist_id` も CASCADE）。印のついた
  プレイリストの削除は印の変更と同じ規則で、印のある端末に open な作業（open な計画か待ち・実行中の
  `device_sync`）があれば 409 `open_plan`、通れば印のある端末の `generation` を進める（同じトランザクション。
  [D-102](../decisions/D-102.md)）。`source_hashes` は `tracks` の行の削除で消える。GC は終端した計画のうち、閉じてから
  `[gc].retention_days` を過ぎたものを消す。端末ごとに最後の終端 1 件（報告の再送を 200 で受ける保険）と
  open な計画は残す（[D-102](../decisions/D-102.md)）
- 同じマイグレーションで `playlists.evaluated_at`（スマートプレイリストを評価した時刻。差分画面に出す）を
  足し、`jobs.type` に `source_hash` / `device_scan` / `device_sync` / `device_verify` を足した
  （`playlist_sync` も同時）。0003 の `inbox_item_tracks` は Inbox の件で配置した曲で、件の「配置 → RG →
  系統 → 端末」の段に使う（[D-95](../decisions/D-095.md) の P5-2 追記）
- **キャッシュ表だけを失い、`devices` の行と認証情報が残った場合**は、曲とプレイリストのキャッシュは次の回復・
  報告で戻る。**DB 全体を失うと** `devices` の行・`devices.uuid`・エージェントのトークンも失われ、端末側の
  manifest / `.spindle-device` と照合できず、回復・報告を受ける行も無い。**Android** は端末タブの「前の登録の
  保存先（引き継げる）」で、manifest の uuid の行を作り、曲の `rel_path` で `track_id` を付け替える
  （[§7.11](07-pipeline/11-device-delivery.md)「引き継ぎ」、[D-103](../decisions/D-103.md)）。
  **Mac** は端末タブで新しく作った行の pair コードで `spindle-agent pair --adopt` を実行し、行の uuid を
  エージェントの state の uuid へ差し替えてから、state の曲の `rel_path` で `track_id` を付け替える
  （[§7.11](07-pipeline/11-device-delivery.md)「iPhone」の「引き継ぎ」、[D-104](../decisions/D-104.md)）。ミュージック.app の track（再生回数・レート・追加日）はそのまま残る。
  Android でバックアップを復元して行の uuid が
  端末の manifest と食い違うときは、端末を削除（端末上のファイルには触らない）してから引き継ぐ

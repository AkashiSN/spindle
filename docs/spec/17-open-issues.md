# 17. 決定済み事項と残課題

## 決定済み

| 項目 | 決定 | 根拠 |
|---|---|---|
| 名称 | **spindle** | CD を積むスピンドル。バイナリ名として扱いやすい |
| ライブラリ構造 | 役割別 3 層（Library / Derived / Archive） | フォーマット別だと CD の FLAC が保管物かつ再生対象で破綻する |
| Category 軸 | ジャンル別・統制語彙 | GENRE タグとは別フィールド |
| アルバム名 | `{album}` のみ、衝突時のみ年を付与 | |
| Derived | 可逆のみ変換 + 配布ビュー解決。Apple 向け `aac` 系統だけ非可逆も変換（[D-75](../decisions/D-075.md)） | 非可逆の多重劣化を回避。ミュージック.app は Opus を読めない |
| ビットレート | Opus 256k VBR（当初 128k。[D-9](../decisions/D-009.md) 追記）、AAC 256k | 再生成可能なので低リスクな決定 |
| WAV | FLAC へ正規化（MD5 照合付き）。元 WAV は Archive へ退避し GC 待ち | タグ・RG の互換性が低いため。即時削除は禁止事項と矛盾 |
| ジョブ dedup | `queued`/`running` の間だけ一意 | 列 UNIQUE だと完了後に同キーを再投入できない |
| 一括編集 | DB 先行更新 + `edit_ops.pending`（トラック単位）、pending 中の再編集は 409、スキャナは pending の論理値を巻き戻さない | ファイル反映待ちの窓で「ファイルが正」と衝突する |
| 配布ビュー | 音声版一致の Derived のみ。音声が古ければ原本へ | 再エンコード待ちの古い音声を配らない |
| スキャン | 4 相（inventory → 候補 → claim → commit）、`scan_runs` で claim、missing は completed でのみ | 走査途中では「移動元の消失」を判定できない |
| 一括編集の対象 | preview 時のスナップショット（`selection_token`） | 見ていない行を破壊的操作の対象にしない |
| 認証スキップ | `trusted_cidrs` は route allowlist のみ、CSRF は Origin 完全一致 | 全 GET 開放は履歴・パスが漏れる。Host は防御にならない |
| 記法 | foobar 風 DSL → AST → SQL | 生 SQL 保存はスキーマ変更で壊れる |
| 認証 | 単一パスワード + セッション Cookie | 事故対策。破壊的 API を裸で置かない |
| Subsonic | 非対応 | ファイル同期を継続。認証も argon2 単独で閉じる |
| メタデータ | MusicBrainz + 手入力の一級市民化 | 同人・VTuber 盤は未登録が常態 |
| マルチch | Derived 対象外・album RG 除外、個別オプトイン | |
| FLAC 再圧縮 | 一括再エンコードはしない。MD5 未設定のみ補填 | 削減 1% 未満に対し 200GB の書き直しとスナップショット肥大 |
| インポート | Inbox + 承認キュー方式 | 外部ツリーを直接スキャンすると規約とタグ品質が混入する |
| SMB | `casesensitivity=insensitive` / `normalization=formD` | 他クライアントが作るファイルと NFC/NFD 混在の事故を ZFS 層で潰す |
| 移行方式 | 新規データセット作成 + rsync コピー | 上記 2 プロパティは作成時のみ指定可能 |
| 外部メタデータ | MusicBrainz だけ。候補から写す範囲は選べる（CD 画面の既定は「全部写す」。[D-72](../decisions/D-072.md) 追記 2）。Discogs / VGMdb は作らない | 表記揺れが激しく値は結局手で直す。外部の価値は盤の識別（[D-72](../decisions/D-072.md)） |
| `.fpl` | 非対応（確定） | .m3u8 とクエリ文字列で足りる。非公開バイナリで 1.x / 2.x が違う（[D-72](../decisions/D-072.md)） |
| album gain | album ごとの属性（既定 off、CD 取り込みは on、承認画面で選ぶ）。RG の一致判定は完全一致のまま | 育つコレクションに曲を足すたびに全曲の再解析と書き換えになる。運用はシャッフルが主（[D-74](../decisions/D-074.md)） |
| ACL の再適用 | 移行後に TrueNAS の ACL エディタで手動（[移行手順](../admin-guide/migration.md) [§2](02-glossary.md) のチェックリスト） | rsync は NFSv4 ACL を引き継げない。1 回しか使わないのでスクリプト化しない |
| 端末への配信 | spindle の管轄に入れる。Android は ADB 直結（host ネットワークの adb サイドカーが Unix ソケットで待ち受ける）、iPhone は Mac の `spindle-agent` がミュージック.app を中継し Finder 同期に任せる。端末ごとのマニフェストを毎回計算し、差分を確かめて確定した計画だけを実行する（自動では送らない）。端末の写しは Library から再生成できる複製なので物理削除と `edit_batches` の対象外を明示の例外にする（[§7.11](07-pipeline/11-device-delivery.md)、[D-95](../decisions/D-095.md) / [D-98](../decisions/D-098.md)〜[D-101](../decisions/D-101.md)） | 端末に何が入っていて何が古いかが spindle から見えなかった。Linux から純正ミュージックへ直接書く手段が無い。配信ツリーの実体化は Derived と同量をもう一度持つ。差分の確認はユーザが選んだ |

## 残課題

- [x] 印のついたプレイリストの削除（2026-10-09。印の変更と同じ規則: open な作業中は 409 `open_plan`、
      通れば `generation` を進める。[D-102](../decisions/D-102.md)）
- [x] 終端した計画の回収（2026-10-09。GC が `[gc].retention_days` 後に消す。端末ごとに最後の終端 1 件は残す。
      [D-102](../decisions/D-102.md)）
- [x] Discogs / VGMdb 連携（2026-09-20。作らない。[D-72](../decisions/D-072.md)）
- [x] `.fpl` 書き出し（2026-09-20。作らない。[D-72](../decisions/D-072.md)）
- [x] `HAS` 等の演算子の foobar 実機との挙動突き合わせ（2026-09-19。部分一致で一致。[D-55](../decisions/D-055.md)）
- [x] 偽ハイレゾ検出のしきい値設計（2026-09-20。カットオフ ≤ 25 kHz かつ（崖 ≥ 10 dB または ≤ 22.5 kHz）/ 実効 ≤ 16 bit。
      実機 124 本で較正。計測値も保存。[§7.10](07-pipeline/10-fake-hires.md)、[D-71](../decisions/D-071.md)）
- [x] 移行後の NFSv4 ACL 再適用（2026-09-20。UI で手動。[移行手順](../admin-guide/migration.md) [§2](02-glossary.md) のチェックリスト）
- [x] CPU 系ジョブ（rg / transcode / flaccheck / hirescheck）に共通の並列予算（2026-09-20。共通 Semaphore =
      コア数を P4 で実装。[D-73](../decisions/D-073.md)）
- [x] Inbox のポーリング間隔（2026-09-19。`[inbox].poll_interval_secs` 既定 60 秒 + 手動。[D-68](../decisions/D-068.md)）
- [x] 一括リネームで album 全体を動かした後、旧ディレクトリに残る同梱ファイル（cover.jpg /
      disc.cue / rip.log 等）の追随と空ディレクトリの扱い（2026-09-19。rename ジョブが commit 後に
      既知の名前を追随させ、空なら rmdir。[D-67](../decisions/D-067.md)）
- [x] 端末への配信の実現性の確認（2026-09-30 / 10-01。Poweramp の再スキャンは `ACTION_SCAN_DIRS` を API レシーバへ
      明示で送り extras を付けない（[D-98](../decisions/D-098.md)）。ミュージック.app の track の `location` は文字列の代入で付け替えられ、
      移動でも track と再生回数を保てる（[D-101](../decisions/D-101.md)））
- [ ] ADB の転送の速さ: shell v2 の stdin 自体は約 38MB/s だが、1 曲ごとの意図・`sync`・sha256 の往復込みの実測は
      約 6.5MB/s で、全曲（約 9,000 曲）の初回はおよそ 2 時間。意図の追記や `sync` をまとめる高速化は持ち越し（[D-98](../decisions/D-098.md)）
- [x] DB を失ったときの既存の保存先の引き継ぎ（Android。2026-10-13。P5-7）: 端末タブの「前の登録の保存先（引き継げる）」。
      manifest の uuid の行を作り、曲の `rel_path` で `track_id` を付け替える。manifest に `rel_path` / `source` を
      足し、`ADOPTED_TOKEN` の近道で送り直さずにトークンだけ書き換える（[§6](06-data-model.md)、[§7.11](07-pipeline/11-device-delivery.md)「引き継ぎ」。[D-103](../decisions/D-103.md)）
- [x] 引き継ぎ（Mac の `pair --adopt`。2026-10-13。P5-8）: 新しい行の pair コードで `pair --adopt` を実行すると、
      pair が行の uuid を state の uuid へ差し替え、`POST /api/agent/adopt` が state の曲を `rel_path` で付け替えて
      キャッシュへ直接書く。state と manifest に `rel_path` / `source` を足し、`ADOPTED_TOKEN` の近道で送り直さずに
      トークンだけ書き換える。ミュージック.app の track（再生回数）は保たれる。pending が残っていれば止める
      （[§6](06-data-model.md)、[§7.11](07-pipeline/11-device-delivery.md)「iPhone」の「引き継ぎ」。[D-104](../decisions/D-104.md)）
- [x] id の再利用（2026-10-14。[D-105](../decisions/D-105.md)）: `tracks.id` / `playlists.id` は `INTEGER PRIMARY KEY` で `AUTOINCREMENT` ではないので、
      最大 id の行を GC が消すと次の挿入が同じ id を得る。端末に写しのある曲の id が別の曲に再利用されると、
      差分は `UpdateMove` になり、Android は収束するが、Mac ではミュージック.app の track（再生回数）が
      別の曲に引き継がれる。7 日以上同期しない場合に限る（[D-102](../decisions/D-102.md)）。方針は **GC が `tracks` の最大 id の行を消さない**
      （SQLite は `max(rowid)+1` を振るので再利用が起きない）。「token・sha256・パスがすべて違う写しを Delete + Add
      にする」は却下（Library の一括のタグ修正と改名は日常操作で、そのたびに Mac の再生回数が消える）。
      プレイリストの id の再利用は受け入れる（Update = 改名として収束し、失う情報が無い。[D-103](../decisions/D-103.md)）

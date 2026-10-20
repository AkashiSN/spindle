# 7.11 端末への配信

Library の曲とプレイリストを端末へ写す（P5、[D-95](../../decisions/D-095.md)）。Android（Poweramp）へは spindle が USB の
ADB で直接書き、iPhone へは Mac の `spindle-agent` がミュージック.app へ反映し、その先は Finder
同期に任せる（Linux から純正ミュージックへ直接書く手段が無いため）。**自動では送らない**:
端末ごとに差分を出し、利用者が確かめてから計画として確定し、確定した計画だけを実行する。表は
[§6](../06-data-model.md)「端末への配信の表」、ジョブは [§8](../08-jobs.md)、API は [§9](../09-http-api.md)、端末タブは [§12](../12-ui/README.md)、設定は [§13](../13-config.md) の `[devices]`、adb
サイドカーは [§14](../14-deploy.md)。送る系統は端末ごとに選ぶ（既定は Android が `opus`、iPhone が `aac`。[§7.6](06-derived.md)）。

```
CD ─┐                                                        ┌─ device_sync ─ adb ─ Android（Poweramp）
    ├→ Inbox → 承認・配置 → Library → RG → Derived ─┬─ opus ─┘
YT ─┘                                               └─ aac ─→ spindle-agent（Mac）→ ミュージック.app → iPhone
                    ↑ 端末ごとのマニフェスト（あるべき状態）を spindle が毎回計算する
```

| | Android（`transport = 'adb'`） | iPhone（`transport = 'agent'`） |
|---|---|---|
| 運び手 | spindle の `device_scan` / `device_sync` / `device_verify` ジョブ。adb サーバは host ネットワークのサイドカーで、Unix ソケットだけで待ち受ける（[D-98](../../decisions/D-098.md)） | Mac の `spindle-agent`（常駐しない CLI）。`/api/agent/*` を Bearer トークンで叩く（[D-99](../../decisions/D-099.md)） |
| 端末側の正本 | `<root>/.spindle/manifest.json` とジャーナル、実ファイル | エージェントの `state.json` とミュージック.app の track。spindle へは報告で届く |
| 差分の確認と確定 | 端末タブの差分表 →「同期」 | `spindle-agent sync` が差分を表示 → `y/N` → 確定 |
| キャッシュへの取り込み | 回復の後（各ジョブの最初・破棄）と同期の完了 | 報告・破棄の受理 |
| 接続の検出 | `adb track-devices` の常駐監視。つながると `device_scan`（回復と差分の計算だけ）を投入し、待っている同期・検証を前倒しする | 無い（利用者が Mac で `sync` を実行する） |
| 端末タブの「反映済み」 | 端末上 | **Mac に反映済み**（`~/Music/spindle` とミュージック.app まで。Finder 同期の結果は spindle から見えない） |

**マニフェスト（あるべき状態）**

保存せず、差分を見るたびに DB から計算し直す（[D-78](../../decisions/D-078.md) と同じ流儀。`domain::device` の純粋関数、
`db::devices::compute` が 1 つの読み取りトランザクションで入力を読む）。曲ごとに **desired / hold / remove**
のどれかに入れる。全端末の計算結果（スナップショット）は `Db` が書き込みの通番で無効化するキャッシュとして
持ち、表示だけの用途（一覧・詳細の行の端末の状態、Inbox の段、端末一覧の件数）は計算から 2 秒以内なら
使い回す。差分・`plan_token`・確定・DSL の未反映の集合は通番の一致を要する（[D-95](../../decisions/D-095.md) の P5-2 追記）。

| 項目 | 規則 |
|---|---|
| 選曲 | `selection = 'all'` は全曲、`'playlists'` は印（`device_playlists`）の付いたプレイリストの曲の和集合（スマートは materialize 済みの結果。[D-54](../../decisions/D-054.md)）。どちらも missing と、チャンネル数が 1・2 以外（不明を含む）の曲は対象外（= remove）。印の付いたプレイリストは `all` でも端末へ書き出す |
| 循環の禁止 | `on_device` / `device_pending`（端末の状態を引く DSL のフィールド）を使うスマートプレイリストには印を付けられない（400）。印の付いたプレイリストのルールの保存でも同じ検査をする（同期のたびに選曲が変わるため。[D-95](../../decisions/D-095.md) の P5-2 追記） |
| 評価待ち | スマートプレイリストの再評価は debounce される（[D-54](../../decisions/D-054.md)）ので、印の付いたスマートプレイリストがある `playlists` の端末は、評価待ちの間は確定を 409 `pending_reevaluation` で断る。差分画面には各プレイリストの評価時刻（`playlists.evaluated_at`）と評価待ちの印を、エージェントの manifest には評価待ちの印を出す |
| 送る元 | 系統（`devices.variant`）の Derived の行があり、`src_audio_version = audio_version`、かつ系統が有効なら `audio_profile` が設定と一致 → **Derived**（タグ版だけ古い `stale_tags` でも送る。タグの上書きでトークンが変わり、更新として送り直す）。系統が `opus` で原本が非可逆 → **Library の原本**（[D-8](../../decisions/D-008.md)。`aac` は非可逆原本も Derived に入るので原本は送らない）。それ以外 → **待ち**（理由は「Derived 未生成」、`aac` で RG が揃わなければ「RG 未解析」、音声版か profile が違えば「Derived の音声が古い」）。系統が設定に無い・凍結中は行の profile をそのまま有効とみなす（[D-75](../../decisions/D-075.md) の凍結と同じ） |
| 送る元のハッシュ | `source_hashes` に送る元の**意味トークンが同じ行**があるときだけ送れる。原本はさらに行の物理同一性（inode・size・mtime_ns・ctime_ns）が `tracks` の現在の値と一致すること（外部ツールが同じタグで再保存した原本を「変化なし」としないため）。無ければ「待ち: ハッシュ計算中」で、差分を計算した側が `source_hash` ジョブを投入する（下記） |
| 端末上のパス | Library の `rel_path` と同じ相対パスで、拡張子だけ送る元に合わせる（`J-Pop/YOASOBI/THE BOOK/1-01 群青.opus`）。Derived と原本が 1 つの root に並ぶので、m3u8 を root 相対で書ける |
| パスの検査 | 端末の root を前置きした長さが 240 UTF-16 単位（[§5](../05-library-layout.md)）を超えれば「エラー: パスが長すぎる」。前置きは adb が `/storage/<volume>/<root>/`（内部ストレージは `/storage/emulated/0/<root>/`）、agent が `Music/spindle/`。別の曲と `dest_path_key`（`canonical_key` = casefold + NFD）が重なれば**どちらも**「エラー: パス衝突」（黙って片方を勝たせない。Library のリネームで解消する）。行き先を「パスを変えない管理下の曲」（保留中の曲を含む）が占めていれば、その曲も「パス衝突」で保留にし、不動点まで繰り返す（入れ替えの片側が保留になれば、もう片側も保留。[D-95](../../decisions/D-095.md) の P5-1 追記） |
| プレイリスト | 印の付いたプレイリストを、衝突を解いた後の desired と hold から組み立てる。載せるのは**同期の後に端末に実在する曲**だけ: desired は今回のパス、hold で既存の写しがある曲は `device_items` のパス。adb は `Playlists/<名前>.m3u8`（`#EXTM3U` と `../<root 相対パス>` の行、UTF-8）、agent はミュージック.app の「spindle」フォルダ直下の同名のプレイリスト。名前の `canonical_key` が重なる・予約名（`.spindle`）と同じものはエラーで、端末上の既存のものに触らない |

| 集合 | 条件 | 端末上の既存の写し |
|---|---|---|
| **desired** | 選曲に入り、送る元とハッシュが揃い、エラーが無い | 追加 / 更新 / 移動 / 更新 + 移動 / 変化なし |
| **hold** | 選曲に入るが、待ち（上の理由）かエラー（パス衝突・パス長） | **触らない**（古い写しがあればそのまま残る） |
| **remove** | 選曲から外れた・missing・マルチチャンネル | 削除 |

**トークン**は構成要素の正準 JSON（キー昇順・空白なし・スキーマ版 `v` を含む）の SHA-256 で、
解析しない不透明な値:

- **意味トークン**: 送る元の版。Derived は行に記録した版（`variant`・`src_audio_version`・
  `src_tag_version`・`src_artwork_id`・`src_rg_scanned_at`・`audio_profile`・`tag_profile`）、原本は
  `audio_version`・`tag_version`。`source_hashes` がどの版のハッシュかを示す
- **配信トークン**: 意味トークン + sha256。端末の中身の同一性と、エージェントのファイル取得の強い
  ETag はこれで判定する（版が進まないまま外部ツールがバイト列を変えても、更新として送り直す）
- プレイリストのトークン: `playlist_id`・名前・生成したバイト列の sha256

**差分**は desired と `device_items` を `track_id` で突き合わせる。トークンもパスも同じなら変化なし、
トークンだけ違えば**更新**、パスだけ違えば**移動**、両方違えば**更新 + 移動**、`device_items` に
無ければ**追加**、remove に入る既存の写しが**削除**。hold は件数と理由だけを出す。プレイリストは
`device_playlist_state` と突き合わせて追加 / 更新（中身か置き場所が違う。改名を含む）/ 削除。
各曲の端末の状態（`synced` / `pending` / `waiting` / `error` / `removing`）は一覧・詳細と DSL の
`device_pending` に使う（[D-95](../../decisions/D-095.md) の P5-2 追記）。

**`source_hash` ジョブ**（曲 × 送る元の単位、dedup `source_hash:<track_id>:<source>`、
CPU の共通並列予算の対象で並列度 2。[D-73](../../decisions/D-073.md)）: 送る元を root の dirfd から `openat2` で開き、開いた
FD の物理同一性を記録してから全体を読んでハッシュし、読み終えた後の `fstat` が開いた時と同じで、
意味トークンも変わっていなければ保存する（途中で差し替わったら捨てて再試行）。原本は開いた FD が
`tracks` の物理同一性と食い違えば読まず、増分スキャンを投入して「スキャン待ち」で終える（同じ物理同一性の
間は再投入しない）。Derived の実ファイルが無ければ（[D-51](../../decisions/D-051.md) の drift）`derived_files` の行を消して
transcode を投入する。投入は差分の計算（端末一覧と差分）のたびに、未完了と抑制中のものを除いて 1 つの
SAVEPOINT で行う（[D-95](../../decisions/D-095.md) の P5-1・P5-2 追記）。初回は全曲分が走る。

**配信時の照合**（ADB の転送・エージェントのファイル取得の両方）: 送る元を開いた FD の物理同一性が
`source_hashes` の行と一致しなければ送らない（ADB はその曲を項目のエラーにし、エージェントへは
412 `source_changed`。どちらも行を捨てて次の差分で取り直させる）。ADB は送りながら spindle
側でもハッシュを取り、期待値と違えば同じく取り直させる。Derived の差し替えや Library の外部更新と
競合しても、**開いた FD の中身とハッシュの組**がずれない。

**計画**

- **確定**: 差分の応答は、`generation` と各操作（種類・track_id / playlist_id・from・to・トークン）を正準
  JSON にした `plan_token` を持つ。確定の要求はそれを添え、サーバが計算し直した差分の
  `plan_token` と一致したときだけ、計画を `device_sync_plans` に**不変のまま**保存する（違えば
  409 `plan_changed`。画面・エージェントは差分を取り直して再確認させる）。差分を見てからボタンを押すまでに
  増えた削除を黙って実行しないため。adb は確定と `device_sync` の投入を 1 つのトランザクションで行う。
  agent は最初の副作用の前にエージェントが確定する
- **端末ごとに open な計画は 1 つ**。同じ `plan_token` の確定は同じ計画を返し（冪等）、違えば
  409 `open_plan_exists`。open な計画か待ち・実行中の `device_sync` がある間は、名前以外の PATCH・
  印の変更（PUT）・pair コードの発行を 409 `open_plan` にする（`generation` を動かさないため）。
  印のついたプレイリストの削除も同じ規則で、印のある端末のどれかに open な作業があれば 409 `open_plan`
  （端末名を message に出す）。通れば印のある端末の `generation` を削除と同じトランザクションで進める
  （[D-102](../../decisions/D-102.md)）。プレイリストの中身・名前・ルールの変更は止めない（次の差分で反映される）
- **開始済みの計画の再開**: 実行するのは計画の部分集合だけ。今の状態で満たされている操作は済みとして
  飛ばし、今の差分にも**同じ操作・同じトークン・同じパス**で残っているものだけを実行する。差分から消えた・
  変わった操作と、計画に無い操作（とくに新しい削除）は実行せず、次の差分に回す。パス変更は入れ替え・
  循環の成分ごとに全員残すか全員外す。例外は始まったパス変更のバッチ（下記）と、
  旧パスの削除の後で切れたプレイリストの改名（追加として続ける。[D-95](../../decisions/D-095.md) の P5-3a 追記）
- **終端**: adb はジョブの成功で `completed`。失敗・キャンセル・未接続では `open` のまま残り、
  端末タブが「続きを実行」（同じ計画で同期を投入し直す）と「破棄」を出す。agent は報告の受理で
  `completed`、エージェントの `abandon` で `abandoned`。終端した計画への報告は受けない（同じ報告の
  再送だけは `report_digest` の一致で成功を返す。[D-99](../../decisions/D-099.md)）。終端した計画は GC が保持期間後に消すので、それより
  遅れて届いた再送は 404 `no_plan` になる（エージェントは `plan_id` を外して止まり、次の sync が報告だけを
  取り直して回復する。端末ごとの最後の終端 1 件は残すので、通常の再送は影響を受けない。[D-102](../../decisions/D-102.md)）
- **端末が戻らないときの逃げ道**（[D-98](../../decisions/D-098.md)）: 破棄に `force` を付けると、端末につながず回復もせずに計画を
  `abandoned` にし、待ちの同期を取り消す（実行中の同期・端末のロック中は 409 `busy`）。adb で安全なのは、
  次につないだときの回復が計画と無関係に端末のジャーナルから封印済みバッチを完遂・取り消すため。iPhone の
  Mac が失われたときもこれで閉じる。端末の削除も open な計画があっても通す（端末のジョブが実行中か
  ロック中なら 409 `busy`。待ちのジョブは取り消す）。印のついたプレイリストの削除が `open_plan` で止まるときも、これで開く

**Android（ADB）**

- **接続**: adb のサブコマンドは `shell`（shell v2）だけを使う。pty を割り当てなければ stdin /
  stdout がバイナリ安全で終了コードも返る（`exec-in` / `exec-out` は終了コードを返さない）。
  ジャーナルの行と転送する中身は stdin で流し、端末側のスクリプトに埋め込むパスは
  1 か所の関数で単引用符クォートする（`sh -c` はローカルでは使わない）。未接続は adb クライアントの文言か
  `get-state` で判定する（[D-95](../../decisions/D-095.md) の P5-3a 追記、[D-98](../../decisions/D-098.md)）
- **端末のファイルシステムの性質**（実機で確認。[D-98](../../decisions/D-098.md)）: 大小文字だけの改名は `mv` が成功を返して
  何もしない（内部共有ストレージ・SD とも）ので、パス変更は必ず `.spindle/moving` の一時名を経由する。
  ディレクトリ名の大小だけの変更は `mkdir -p` が既存を使うので、端末上の表示は旧い大小のまま残る（キーで
  比べるので差分は出ない）。正規化は内部共有ストレージが区別せず（NFD の名前で NFC のファイルに当たる）、
  SD（exFAT）は区別する。キーは casefold + NFD なので安全側（別名が衝突として保留になり得るだけ）
- **登録**（端末タブの「未登録の Android」）: 保存先は内部共有ストレージか SD カードの
  `Music/spindle`。**root が無いか空のときだけ**登録でき、`.spindle/manifest.json` を作る（空でなければ
  409 `not_empty`。既存のファイルの扱いを推測しない。片付けるのは、中身が `.spindle` の下だけで
  **回復に要る物が無い**（`.spindle/moving` の下にファイルが無く、ジャーナルが無いか空）前の登録の失敗の残りで、
  manifest が無い・読めない・空のものだけ。それ以外は 409 `not_empty`。曲かプレイリストを指す manifest は捨てず 409 `adoptable` で
  引き継ぎを案内する。対象は `volume` が登録先と同じで、uuid が DB に無いものに限る）。
  保存先は PATCH で変えない（削除して登録し直す）。Poweramp の
  スキャン範囲はそのフォルダ設定なので、保存先はその下に置く
- **引き継ぎ**（DB 全損後。`POST /api/devices` の `adopt: true`。[D-103](../../decisions/D-103.md)）: 未登録の一覧は、root の manifest が
  読めて、`volume` が一致し、uuid が DB に無く（32 桁の小文字 16 進）、曲かプレイリストを指す
  （空の manifest は引き継げない）ボリュームを `adoptable` にする。プレビュー
  （`GET /api/devices/adb/adopt-preview`）は書かずに付け替える曲・外れる曲・プレイリスト・管理外の件数と
  `library_busy` を返す（ジャーナルは回復しないので件数は目安）。確定は `register_lock` の下で、
  Library の `scan` ジョブが待ち・実行中なら 409 `library_busy`（DB の作り直し途中だと大半が外れる）→
  manifest を読む（無い・読めない・`volume` 違い・空・uuid の形が不正・回復で manifest の不整合が
  見つかったときは 409 `not_adoptable`、uuid が DB にあれば 409 `uuid_registered`。端末との通信の失敗は
  既存のエラー応答の 502 `device_failed` / 409 `not_connected`）→
  `scan` が待ち・実行中でないことと remap の入力（Library の曲とプレイリスト）を 1 つの読み取り
  トランザクションで読む（スキャン中なら 409 `library_busy`。別々に読むと間に始まったスキャンの途中の曲で
  remap し得る）→ 回復（`recover`。DB は見ない）→ remap → `compact` で manifest を書き直す →
  1 トランザクションで manifest の uuid の行を作り、付け替えたプレイリストに印を付け直す →
  `device_scan` を投入する。`generation` は作成時の既定のまま（印の復元で進めない）。印を付け直すのは、
  端末に m3u8 があるのはそのプレイリストに印が付いていたからで、付けないと `selection=playlists` の端末の
  最初の差分が全削除になるため。remap は曲を manifest の `rel_path` の `canonical_key` で今の tracks
  （missing を除く）から引いて `track_id` を付け替え、プレイリストは `Playlists/<名前>.m3u8` の名前で付け替える。
  1 つのキーに複数の曲、1 曲に複数の項目が当たるものは採らない。**採らなかった項目は manifest から外して管理外に
  する（ファイルは消さない）**。曲のトークンは `ADOPTED_TOKEN`（`"adopted"`）、プレイリストは `STALE_TOKEN`
  （回復が先に `STALE_TOKEN` にした曲は変えない）。remap は冪等で、途中で落ちてもやり直せば同じ結果になる。
  Android 以外の transport は 400。Mac は `pair --adopt` で、同じ付け替えの規則をサーバで計算する
  （下の「iPhone」の「引き継ぎ」、[D-104](../../decisions/D-104.md)）
- **端末側の正本**: manifest は `format`・`device_uuid`・`volume` と、曲（track_id・path・配信トークン・
  size・sha256・`rel_path`・`source`）とプレイリスト（playlist_id・path・トークン）の一覧。知らない `format`、id とパスの重複、
  root 外のパス、10 万件超は読まずに止める。`device_uuid` と `volume` が端末の行と揃わなければ止める（別の
  端末のカード・カードの差し替え）。**manifest に無いファイルには触らない**（手で置いたものは
  「管理外」として件数を出し、置き先を塞いでいれば「管理外のファイルと衝突」でその曲を保留にする）。曲の `rel_path` は送った元の Library の
  `tracks.rel_path`、`source` は送る元の種類（`SourceKind::as_str()` の `master` / `opus` / `aac`）で、
  どちらも必須（引き継ぎの付け替えの鍵。ジャーナルの曲の意図にも任意で載せ、回復で取り込む）。`format` は 1
  のまま、この 2 項目の無い旧 manifest は読めない（リリース前の変更。保存先を消して登録し直す）
- **ジャーナル**（`.spindle/journal`、1 行 1 レコードの NDJSON）: 各操作は**意図を耐久化してから**副作用を
  起こす。追記 → `sync` の後に副作用、ファイルの置き換えは同じディレクトリの tmp → `sync` → `mv -f` →
  `sync`、圧縮は manifest を書き直して確定させてからジャーナルを空にする（逆だと回復できない）。
  バッチ以外の意図は `put` と `rm` だけ。完了は次の意図と同じ追記に載せ、manifest の書き直しは完了
  50 件ごとと同期の最後に行う（1 曲ごとに 2 回 `sync` すると初回の全曲転送が数時間延びる。[D-95](../../decisions/D-095.md) の P5-3a
  追記）
- **パス変更（移動・更新 + 移動）は計画の全件を 1 つのバッチ**にし、大域的な 2 段で進める（入れ替え・
  循環・大小文字だけの改名で上書きしないため。Library の一括リネームと同じ考え方）: メンバー全員を記録して
  `sealed` → 更新 + 移動の新しい内容を一時ファイルへ転送して照合（`prepared`）→ 全員の旧パスを空ける
  （`vacating` → `vacated`）→ 全員の行き先を埋める（`placing` → `done`）。一時ファイルは
  `.spindle/moving/<batch_id>-<op_id>`（`.new`）。`prepared` 以前は副作用が一時ファイルだけなので破棄・
  縮小できる（先に `batch_abort` を耐久化）。
  **`vacating` 以降はキャンセルも `generation` の不一致も効かせず、記録した内容のまま完遂する**（一部だけ
  止めると入れ替えが壊れる）
- **回復**（各ジョブの最初と破棄。DB は見ない）: manifest とジャーナルを読み、完了の無い `put` は置き先の
  sha256 が意図と一致すれば反映済みとして取り込む。封印済みで `vacating` 以降のバッチは、メンバーの位置を
  ファイルの在否で判定して前進で完遂し（`from` にも `staging` にも無いメンバーは手で消されたとみなし、
  manifest から外す）、`prepared` 以前のバッチは破棄する。実ファイルが無い・サイズが違う曲は manifest から
  外さずトークンを空（`STALE_TOKEN`）にする（外すと管理外になって上書きも削除もできない。次の差分で「更新」に
  戻る）。ただし実ファイルを見失った曲は、`.spindle/moving/` に残ったファイルと sha256 で照合できれば元の
  パスへ戻す。最後に manifest を書き直してジャーナルを空にし、その結果で `device_items` /
  `device_playlist_state` を全置換する（[D-95](../../decisions/D-095.md) の P5-3a 追記）
- **同期**（`device_sync`、payload は端末と計画の id、dedup は端末ごと、最大 3 回）:
  端末ごとの排他（`job_mutexes` の `device:<id>` とプロセス内のロック）を取り、計画がまだ
  open かをロックの後で確かめ直し、回復してから再開の規則で実行する計画の部分集合を決める。順序は
  **削除 → パス変更のバッチ → 更新 → 追加 → プレイリスト**（先に空きを作る。プレイリストは旧パスの
  削除を全部先に行ってから書く）。始める前に、計画の順に見た追加量の最大値 + プレイリストと manifest の
  大きさ + 64 MiB を空き（`stat -f`）と比べ、足りなければ送らず失敗する。転送は送る元の FD を stdin に
  繋いで端末の `<to>.spindle-tmp` へ書き、端末の `sha256sum` で照合してから `mv -f` する。`vacating` より
  前の書き込みの直前にキャンセルと `generation` を確かめる（不一致は失敗で、再試行しない）。完了で manifest
  からキャッシュを、この同期のエラーから `device_errors` を全置換し、計画を `completed` にする（同期の途中で
  計画が閉じられていたら、キャッシュも `last_synced_at` も進めずに失敗）
  - **進捗**（[D-109](../../decisions/D-109.md)）: 項目の始めと終わりに `progress` を呼ぶ。件数は `JobEvent` の `done` / `total`、
    SSE の `job` イベントの detail は `{"kind":"device_sync","phase","current","bytes_done","bytes_total","rate"}`
    （`phase` は `delete` / `move` / `send` / `playlist`、`current` は送り先のファイル名（削除は元のファイル名）、プレイリストは
    そのファイル名）。`bytes_total` は端末へ中身を送る曲（`Add` / `Update` / `UpdateMove`）の `size` の和で、
    引き継ぎの照合で済む曲と純粋な `Move` は含めない。`bytes_done` は曲を 1 曲処理し終えるごとに増やし
    （曲の途中では増えない）、保留・失敗になった曲の `size` も処理し終えた量として足す。パス変更のバッチは
    失敗した曲を外してやり直すので、バッチの中では送り終えた量の最大値を見せて減らさず、バッチを終えたら
    全体を確定させる。バッチの仕上げ（空ける・置く）でもメンバーごとに `current`（送り先のファイル名）を出し、
    置くたびに件数を 1 進め、終えたら件数を「バッチの開始時 + バッチの項目数」に確定させる。`rate` は直近 5 秒の標本（基準は 5 秒前以前で最新の標本、無ければ 0）からの
    バイト差 ÷ 経過秒で、`JobControl` が持つ。detail は SSE にだけ流し DB には書かない（DB への進捗の書き込みは
    `JobContext` が 250 ms に間引く）
- **引き継いだ写しの近道**: `Update` でパスが同じ（`from` 無し）、book の項目のトークンが `ADOPTED_TOKEN`、
  `sha256` と `size` が操作と一致する曲は、転送もジャーナルも省いてトークンだけを操作のものに書き換える
  （信頼の水準は日常の差分と同じ。manifest の `sha256` は転送時に端末上で測って照合した値）。ただし送る元は
  通常の転送と同じく開いて identity を照合し、変わっていれば項目エラー + ハッシュの取り直し、開けなければ
  項目エラーにしてトークンを進めない。この曲は転送量の
  見積もりと空き容量の確認から外し、差分では op `adopted`（UI は「引き継ぎの照合」）として分ける。回復や
  `device_verify` が付けた `STALE_TOKEN` が優先されるので、壊れた写しは送り直す。同期の終わり（最後の
  `compact` の前）に、パスのキーが今の desired と一致する全項目の `rel_path` / `source` を今の値へ書き直す
  （移動の操作が出ない `rel_path` の変化があっても新しく保つため）
- **Poweramp**: 同期の最後に `am broadcast -a com.maxmpz.audioplayer.ACTION_SCAN_DIRS` を API
  レシーバへ明示（`-n`）で送る。extras は付けない（パスを指定する extra は無く、
  `eraseTags` は全タグ消去の副作用がある）。Poweramp が無ければ何もせず、失敗は警告に留めて同期は成功扱い
  （[D-98](../../decisions/D-098.md)）
- **内容の検証**（`device_verify`）: 日常の差分は**サイズまでしか見ない**（全曲の
  sha256 を毎回取ると数十分かかる）ので、同じサイズの破損や端末上の書き換えは日常の差分では見つからない。
  端末タブの「内容を検証」が全曲の `sha256sum` を取り、食い違った・無くなった曲を `STALE_TOKEN` にして
  manifest を書き直し、`device_items` も全置換する（次の差分で「更新」になる）。
  回復済み（ジャーナルが空）の正本にだけ行う
- **未接続**: 同期・検証は attempts を数えずに 300 秒後へ回し（`RequeueAfter`）、
  `track-devices` がその端末の `device` を見たら前倒しする。`device_scan` は待たずに終わる（次の接続で
  投入される）。端末のロックが取れなければ 15 秒後に回す。`[devices].adb_server` が空なら ADB の操作は
  503 `adb_disabled`（強制破棄と端末の削除は動く。[D-98](../../decisions/D-098.md)）

**iPhone（Mac の spindle-agent）**

エージェントはリポジトリ内の別クレート `agent/`（ワイヤ型は serde だけに依存する
`agent-proto` で本体と共有）。導入・前提・困ったときは `agent/README.md`、エンジンの判断は [D-100](../../decisions/D-100.md)、
ミュージック.app と配布の判断は [D-101](../../decisions/D-101.md)。

- **コマンド**: `pair <URL> <コード> [--insecure-http] [--adopt]`（端末タブで発行した 10 分・1 回限り・
  5 回まで試せるワンタイムコードでトークンを受け取る。発行すると旧トークンは失効。`--adopt` は下の
  「引き継ぎ」）、`sync`、`status`（引き継ぎが途中なら `adopt_pending` を出す）、
  `resolve`、`abandon`。多重起動は state のディレクトリの `lock`（`flock`）で防ぐ。平文の http は
  `--insecure-http` なしでは拒む
- **保存先の所有権**: ローカルの root は `~/Music/spindle`（起動時にシンボリックリンクを解いた
  実パスにする。ミュージック.app が track の場所をそう持つため。root の中のリンクは辿らない）。
  pair のときに**無いか空のときだけ**使い、`.spindle-device` に `device_uuid` と nonce を書いて
  sync のたびに照合する。ミュージック.app 側はエージェントが作った「spindle」フォルダで、衝突しない一時名
  `spindle-setup-<nonce>` で作ってから改名する（既存の同名フォルダは採らずに止める）。
  **管理対象は state と `pending_ops` / `pending_batches` にある persistent ID とパスだけ**で、管理外の
  ファイル・同名のプレイリストは上書きも削除もせず「管理外と衝突」で保留する
- **前提の検査**（副作用の前）: ミュージック.app の「ファイルを［ミュージック］フォルダにコピー」は
  **オフ**が前提。外から読めないので、`add` が返した track の location が期待パスと違うことで検出し、
  その track を消して止める。root かその祖先がミュージックのメディアフォルダ（直下に
  `Automatically Add to Music.localized` か `.Media Preferences.plist` がある）なら pair も
  sync も止める（メディアフォルダの中の曲は削除でファイルまで消え、コピー設定が ON でも複製されず、
  前提が崩れる。[D-101](../../decisions/D-101.md)）
- **sync の順序**: 前提の検査 → root の印と pair の初期化の続き → `pending_batches` と
  `pending_ops` の回復 → 途中の引き継ぎ（`adopt_pending`）の続き → manifest の取得 → **再発見**（ミュージック.app の file track のうち location が
  root の下を指すものを state と突き合わせる）→ open な計画の照合 → 差分の表示と `y/N` → 確定 → 反映 →
  報告。バッチの途中は location が `.moving/` を指すので、回復を再発見より先に行う。
  open な計画を再開するのは、自分が `y/N` を経て確定し state に `plan_id` を記録した計画だけ（それ以外は
  実行せずに報告して閉じる）。回復か再発見で state が変われば、何も実行しない計画を確定して報告だけを
  行い、表示する差分をエージェントの実状態に合わせる（[D-100](../../decisions/D-100.md)）。差分が空なら確認せずに終わり、
  評価待ちなら差分を表示して反映しない。確定が 409 `plan_changed` なら manifest を取り直して表示し直す（3 回まで。
  変わり続ければ止める）。Ctrl-C は扱わない（次の `sync` の回復が続きを行う）
- **受け取り**: `GET /api/agent/files/:track_id` に常に `If-Match: "<配信トークン>"`（強い ETag）を付け、
  続きは同じ `If-Match` に `Range` を足す（`If-Range` は不一致で全文が返り、途中のファイルと混ざりうるので
  使わない）。版が変わっていれば 412 で、その操作は次の差分に回す。受信が切れたら同じ `If-Match` + `Range` で
  1 回の実行の中で 3 回まで続きを取る（実行をまたいだ再開はしない。回復が一時ファイルを消す）。受け取ったら size と sha256 を確かめ、
  同じディレクトリの tmp → fsync → **上書きしない rename**（macOS は `renameatx_np(RENAME_EXCL)`。
  対応しない FS では止める）→ 親の fsync
- **ミュージック.app への反映**は JXA（`osascript -l JavaScript`。trait のメソッド 1 回 = 起動 1 回、既定
  120 秒で打ち切り）。追加は `add`、更新は同じパスへ差し替えて `refresh`（再生回数を保つ）、削除は
  track を消してからファイルを消す。パス変更は ADB と同じバッチと相で、旧ファイルを
  `.moving/<batch_id>-<op_id>` へ動かして location を付け替える（track は残す）。
  プレイリストの中身は一時プレイリスト `.tmp-<op_id>` を作って埋め、旧を消して改名する（途中で落ちても空や
  重複のものが本名で残らない）。state の書き込みは tmp → fsync → rename → 親の fsync で、
  1 つの遷移で変わるもの（曲の更新・バッチの相・`pending_*` からの除去）は 1 回の書き込みに含める
- **外で変わった写し**: size・sha256 が state と違う曲は track を残してトークンを空にし（ADB の
  `STALE_TOKEN` と同じ）、ミュージック.app から track が消えた曲は state から外して自分のファイルも消す
  （次の差分で追加になる）。コピー設定 ON のまま `add` の直後に落ちたときの複製は件数にかかわらず自動では
  消さず、候補を表示して `resolve <op_id> --delete-track <persistent_id>` か `--no-copy-created` を明示で
  選ばせる
- **state の曲の出どころ**: state（`state.json`）の曲は persistent ID・パス・トークン・size・sha256・
  inode / mtime に加えて、`rel_path`（送った元の Library の `tracks.rel_path`）と `source`（送る元の種類。
  `master` / `opus` / `aac`）を必須で持つ（引き継ぎの付け替えの鍵。§7.11「端末側の正本」の manifest と同じ）。
  値は manifest（`GET /api/agent/manifest` の `items[].rel_path` / `source`）の同じ `track_id` の項目から取り、
  manifest に無い曲の追加・更新は項目のエラー（「出どころが無い（manifest に無い曲）」）にする。曲の
  `pending_ops` には任意で、パス変更のバッチのメンバーには必須で持たせ、回復で写す。`STATE_VERSION` と
  `PROTO_VERSION` は 1 のまま、この 2 項目の無い旧 state は読めない（リリース前の変更。作り直す）
- **進捗**（[D-108](../../decisions/D-108.md)）: `spindle-agent sync` は確認の後の実行中（削除 → パス変更 → 更新 → 追加 → プレイリスト）の
  進捗を、Mac の端末とサーバに出す。件数は 1 件を扱い終えるたびに進め（見送り・エラーも数える）、バイト数は
  受け取りが要る項目の `size` の和に対する受信量（失敗・見送りは残りを足して合計に届かせる）。速度は直近 5 秒の
  移動平均、残り時間は残りバイト ÷ 速度。サーバへは `POST /api/agent/plans/:id/progress` を 2 秒に 1 回まで送る
  （件数が変わらなくても速度と生存のために送る。項目の始めにも送る機会を作り、CLI では別スレッドの心拍が、
  1 項目が長く掛かる間も前回の送信から 2 秒空けば最新の値を送り直す。間隔・失敗の数・打ち切りはエンジンと共有し、
  前の計画の送信の結果は次の計画に記さない。速度は送る時点で計り直す。最後の 1 回は心拍の送信の終わりを待って送る）。**サーバはメモリだけに持つ**（端末ごとに 1 件。再起動で消え、次の
  送信で戻る）。計画が閉じる（報告・破棄・強制破棄）と消して `device_progress`（`progress: null`）を流す。
  端末の応答の `sync_progress` は、保持している進捗の `plan_id` が今の open な計画と一致するときだけ出す。
  受理後に計画を確かめ直し、閉じていたら消して null を流す（閉じる側の消去との競合）。端末の削除でも消す。
  送信の間隔は前回の送信が終わってから数える。送信の失敗（通信エラー・タイムアウト・5xx）は同期を止めず、
  その回を捨てて間隔を 2 秒・4 秒・8 秒…（上限 60 秒）と延ばし、3 回続けて失敗したらその計画では以後送らない
  （詰まったサーバのタイムアウトで受信を止めないため）。404 / 409 を受けたらその計画では以後送らない（古いサーバの
  404 もこれ）。次の計画（再開した計画の後の新しい計画）ではまた送る。終わりの要約（`完了: …`）は最後まで実行した
  ときだけ出し、途中で止めた（`Stop`）ときは進捗の行を消して止めた理由だけを出す。
  Android（adb）はこの経路を使わず、ジョブの `job` イベントの detail で流す（上の「Android（ADB）」の「同期」の進捗、[D-109](../../decisions/D-109.md)）
  - **Mac の端末の表示**: 標準エラーが TTY なら 1 行を `\r` で上書きする（0.2 秒に 1 回まで）。形は
    `[12/92] 85.3 MB / 634.8 MB  12.4 MB/s  残り 0:42  受信中: <今の項目>`（バイト数は受け取りが要る項目が
    あるときだけ）。段の動詞は 削除中 / 移動中 / 受信中 / 追加中（ミュージック.app へ）/ プレイリスト、今の項目は
    `dest_path`（プレイリストは名前）。端末の幅（取れなければ `COLUMNS`、それも無ければ 80 桁）に収まるよう
    今の項目を前から省き（`…` を前置）、それでも足りなければ残り時間・速度・バイト数の順に省く。全角は幅 2 で
    数える。TTY でなければ上書きせず、10 秒か 10 件ごとに 1 行出す。ほかのメッセージは進捗の行を消してから出す。
    終わりの要約は `完了: 92 件（634.8 MB）を 0:51 で（平均 12.4 MB/s）`（受信が無ければバイト数と速度を省く）。
    バイトは 1024 で割った KB / MB / GB（Web の表示と揃える）
- **引き継ぎ**（`pair --adopt <URL> <コード>`。DB 全損後。[D-104](../../decisions/D-104.md)）: DB を作り直して端末タブで作った新しい
  行の pair コードで、この Mac の保存先とミュージック.app の track をそのまま引き継ぐ。
  1. 前提（コードを使う前）: 通常の pair と同じ検査（コピー設定・メディアフォルダ・Automation・トークンの
     置き場）、前の登録の state（初期化が済み、サーバの情報がある）があること（無ければ「引き継ぐ前の登録が
     ありません」で止める）、root の `.spindle-device` が state の uuid と一致すること、ローカルの回復
     （サーバを使わない）で `pending_ops` / `pending_batches` が空になること（残れば止める）
  2. `POST /api/agent/pair` に `adopt_uuid`（state の uuid）を付ける。サーバは 32 桁の小文字 16 進でなければ
     コードに触らずに 400 `bad_request`。コードの消費と同じトランザクションで、行が端末の状態を一度も持って
     いない（`last_synced_at` が NULL、計画が 1 件も無い、`device_items` と `device_playlist_state` が空）うえ
     `adopt_uuid` が他の行に無いときだけ行の uuid を差し替える。行の uuid が既に `adopt_uuid` なら、報告も
     計画も無い（`last_synced_at` が NULL、計画が 1 件も無い）ときだけ差し替え済みのやり直しとして通す
     （前の adopt が `device_items` などを書いた後で落ちたやり直しを含む）。報告か計画があれば DB を失って
     いない端末なので断る。それ以外は 409 `not_adoptable` で、コードは消費せず、予約した試行の枠も返す
     （断られても 5 回の試行に数えない）
  3. エージェントは応答の uuid が state と同じことを確かめ、サーバの URL・名前と `adopt_pending = true` を
     保存してから、トークンを保存する（逆順だと間で落ちたとき、新しいトークンと付け替える前の state が
     組み合わさる。この順なら次の `sync` は古いトークンで止まり、`pair --adopt` のやり直しで続く）。続く adopt
     はこの新しいトークンで送る
  4. `POST /api/agent/adopt` に state の曲（`track_id`・パス・`rel_path`・`source`・size・sha256・
     トークンが `STALE_TOKEN` か）とプレイリスト（`playlist_id`・名前）を送り、付け替え表（外れたものは
     `to: null`）を受け取る。付け替えは Android と同じ規則（曲は `rel_path`、プレイリストは名前の
     `canonical_key`。曖昧なものは採らない）。サーバはそのまま `device_items`（トークンは `stale` なら
     `STALE_TOKEN`、それ以外は `ADOPTED_TOKEN`）・`device_playlist_state`（`dest_path` は名前、トークンは
     `STALE_TOKEN`）を全置換し、付け替えたプレイリストに印を付け足し（既存の印は外さない）、`device_errors` を空にする。
     `generation` と `last_synced_at` は進めない。冪等で、同じ入力を送り直しても同じ結果になる
  5. エージェントは state の `tracks` / `playlists` のキーを付け替え、曲のトークンを `ADOPTED_TOKEN`
     （`STALE_TOKEN` の曲はそのまま）、プレイリストのトークンを `STALE_TOKEN`（サーバと同じ。最初の同期で
     中身を同じ名前のまま書き直す）にし、`plan_id` を消し、`needs_report` と `adopt_pending` を下ろして
     1 回で保存する。**外れた曲・プレイリストは state から外すだけで、ファイルもミュージック.app の track も
     消さない**（管理外になる）
  3 と 5 の間で落ちたら、次の `sync` が回復の後・manifest の取得の前に 4 と 5 をやり直す（`pair --adopt` の
  打ち直しでもよい。サーバの差し替えはやり直しを受け付ける）。`--adopt` 無しの pair で uuid が state と
  食い違ったときは今どおり止め、文言で `pair --adopt` を案内する（同じ端末の pair し直しと区別できないので、
  コードを使う前には検出しない）
- **引き継いだ曲の近道**: `Update`（`from` 無し）で、state の曲のトークンが `ADOPTED_TOKEN`、パスの鍵・
  size・sha256 が計画の項目と一致すれば、受け取りもミュージック.app の操作もせず、トークン（と `rel_path` /
  `source`）だけを書き換える。報告はそのトークンで行う（計画に含まれるので検証を通る）。受け取らないので
  実行前の空き容量の確認からも外す。信頼の水準は日常の差分と同じ（state の sha256 は再発見の inode /
  mtime のキャッシュと読み直しで保たれている）
- **報告**（`POST /api/agent/report`）: `generation`・`plan_id`・再発見で確かめた state 全体・
  失敗した項目を送る。サーバは計画がその端末の open な計画で、`generation` が一致し、各項目が「現在の
  desired」「既存の `device_items` と同じ値」「その計画で許された遷移の結果」のどれかに完全一致すること
  （トークンが空の曲は `(track_id, パスの鍵)` の一致だけ）、件数（10 万）とパスの形を確かめ、
  **1 件でも外れれば全体を 400 で拒否**する。通れば 1 つのトランザクションでキャッシュと `device_errors` を
  全置換し、計画を `completed` にして `report_digest` を記録する（[D-99](../../decisions/D-099.md)）。`abandon` も同じ検証を通り、
  `pending_ops` と `pending_batches` が空のときだけ受ける
- **トークンの保管**は macOS の Keychain（サービス `spindle-agent`、アカウント `token`）。ssh
  越しでは使えないので、pair はコードを使う前に保存できるかを確かめる。`SPINDLE_AGENT_SECRETS=file` で
  state のディレクトリのファイル（0600）に置ける。サーバはトークンのシークレットを
  SHA-256 で持つ（256 bit の乱数なので総当たりに耐え、ファイル取得のたびに argon2id を払わない。[D-99](../../decisions/D-099.md)）
- **配布**: macOS（arm64）のバイナリは CI で作り、**署名・公証せず** tar.gz を GitHub
  Release に置く（README に `xattr -d com.apple.quarantine` と Automation の許可を書く。[D-101](../../decisions/D-101.md)）

**不変条件との関係（明示する例外）**

（DB にしかない端末の設定・印・計画・トークンの扱いは [§3](../03-principles.md)・[§14](../14-deploy.md)。）

CLAUDE.md の不変条件と禁止事項は Library（ユーザデータ）を対象にしている。端末上の写しと Mac の
`~/Music/spindle` は **Library から再生成できる複製**なので、次を例外とする（[D-95](../../decisions/D-095.md)）。

- **端末の写しの削除は物理削除でよい**（`missing_since` の猶予を持たない）。Library の曲が
  missing になれば次の同期で端末から消え、missing が解ければ追加で戻る。ただし spindle が消すのは端末側の
  正本（manifest / state）にある管理下のものだけで、手で置いたファイルには触らない
- **端末への書き込みは `edit_batches` / `edits` に載せない**。巻き戻しは「Library を巻き戻して同期し
  直す」。中断からの安全は、意図を先に耐久化する端末側のジャーナル（Android）と state の
  `pending_*`（Mac）、計画の再開の規則で保つ（不変条件 6）
- **「ファイルが正」は端末にも当てはめる**: 端末側の正本と実ファイルが正で、`device_items` などは
  キャッシュ。食い違えば端末側を採る（回復・再発見・内容の検証の結果で全置換する）
- **生 SQL の禁止は変わらない**。DSL の `on_device` / `device_pending` は端末名を値としてバインドし、
  未反映の集合は JSON 配列をバインドして `json_each` で展開する（[D-95](../../decisions/D-095.md) の P5-2 追記）

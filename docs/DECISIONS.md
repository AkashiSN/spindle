# 設計判断の記録

実装中に「なぜこうなっているのか」を調べ直さなくて済むように、
判断とその理由、および却下した案を残す。
**ここに記録された判断を実装時に覆さないこと。** 覆すべき理由が見つかった場合は
実装を止めて確認する。

---

## D-1 ライブラリ構造は役割別（フォーマット別ではない）

**決定**: `Library`（正）/ `Derived`（生成物）/ `Archive`（生データ）/ `Inbox`（一時）

**理由**: 旧構成の `Opus/` と `Original/` は実質「配布用」と「保管用」の
役割分担だった。CD の FLAC は保管物であると同時に再生対象でもあるため、
フォーマットで切るとどちらに置くべきか決まらない。

**却下**: 同一ディレクトリにフォーマット混在（他プレイヤーから重複に見える）。
CD イメージを `Archive` に二重保持（300GB の重複。`disc.cue` + `disc.toc` +
per-track FLAC があればイメージを再構成でき、検証可能性は失われない）。

---

## D-2 ファイルが正、DB はキャッシュ

**決定**: DB を消してもファイルから再構築できること。外部変更はファイルが勝つ。

**理由**: SMB 経由で foobar2000 や他プレイヤーが同じファイルを読み書きする。
DB だけを真実にすると、外部から触られた瞬間に破綻する。

**例外**: プレイリスト、編集履歴、検証結果、ジョブ履歴は DB にしか存在しない。
個別にバックアップする。

---

## D-3 同一性は inode と audio_md5 で解決する

**決定**: `(dev, inode)` → `audio_md5` → `rel_path` の優先順位。

**理由**: リネームとタグ編集が日常操作であり、パスもバイト列も変わる。
ZFS では rename とタグ書き換えで inode が不変。FLAC の STREAMINFO には
非圧縮音声の MD5 があり、タグ変更で不変かつデコード不要で読める。

**既知の限界**: 非可逆音源には強い識別子がないため `(size, mtime, path)` の
ヒューリスティックに落ちる。これは割り切る。

**補足**: どの識別子も実体の一意 ID ではない。各段での候補の検証規則は D-30、
重複音声の扱いは D-29、hardlink は D-26。

---

## D-4 音声とタグを別に版管理する

**決定**: `audio_version` と `tag_version` を分ける。

**理由**: これがないと、数千件の一括タグ編集のたびに Derived の再エンコードが
走り数時間失われる。タグだけの変更は Derived 側もタグ上書きで済む。

---

## D-5 タグ書き込みは tmp + rename

**決定**: 一時ファイルに書いて fsync してから rename する。

**理由**: 電源断でファイルが壊れないことを優先する。

**代償**: inode が変わるため、書き込み成功時に DB の `(dev, inode)` を
必ず更新する必要がある。忘れると次回スキャンで全件が新規トラックになる。

---

## D-6 Category と GENRE タグは別フィールド

**決定**: パス決定には統制語彙の `category` を使い、`GENRE` タグとは分離する。

**理由**: (a) GENRE は多値を取り得るがパスは単一値 (b) MusicBrainz 由来の
ジャンル文字列は表記揺れが激しくディレクトリ名が乱立する
(c) ジャンルは後から変わり、パス直結だと数千ファイルが動く。

語彙にマッチしないものは `_Unsorted/` に置き、取り込みを止めない。

---

## D-7 アルバムフォルダ名に年を入れない

**決定**: `{album}` のみ。衝突時のみ `{album} ({year})` へ降格。

**注意**: DATE / ORIGINALDATE タグは必ず保持する（パスに出さないだけ）。
配置前に MusicBrainz Release ID または DiscID で同一リリース判定を行う。
**同名 ≠ 同一リリース**であり、パス衝突をマージ扱いにしてはならない。

---

## D-8 Derived は可逆のみ、配布ビューで解決

**決定**: `delivery_path = 可逆 ? Derived : Library`（`delivery` ビュー）。

**理由**: 非可逆音源を再エンコードすると世代劣化する。Derived にコピーすると
二重管理になる。ビューで解決すれば m3u8 生成側は Derived の有無を意識しない。

**例外**: 容量逼迫時のみトラック単位で `force_transcode` を許可。
ただし元が 256kbps 以上の場合のみ。

---

## D-9 Opus 128kbps VBR

**決定**: Derived は Opus 128k VBR。

**理由**: 事実上透過で、可逆 200GB 分でも約 25GB。96k はポータブルでは十分だが
有線イヤホンで差が出る境界。160k 以上は体感差がほぼなく容量だけ増える。

**補足**: Derived は再生成可能なので、この決定は低リスクであり後から変えられる。

---

## D-10 WAV は FLAC へ正規化する

**決定**: 取り込み時に可逆変換し、PCM MD5 の一致を確認してから元 WAV を
`Archive/` へ退避する。物理削除は GC ジョブ（既定 30 日後、`archived_files` 台帳を
根拠に）のみが行う。

**理由**: WAV は RIFF INFO / ID3 のどちらを使うかがソフトごとに異なり、
ReplayGain タグの互換性も低い。**音声情報は**可逆変換なので失われない。

**意図して捨てるもの**: PCM MD5 が保証するのは音声サンプルの同一性だけで、
RIFF INFO / ID3 / 未知チャンク / コンテナのバイト列は FLAC から再生成できない。
保持期間後にこれらを不可逆に捨てることを受け入れる。WAV の付随メタデータに
価値があるライブラリなら `[gc]` の保持期間を延ばすか、GC を止めて無期限保持にする。

**却下**: MD5 一致確認後の即時削除（初版の案）。「ユーザデータの物理削除は GC のみ」
という禁止事項、および「破壊的操作はバッチ単位で巻き戻せる」不変条件と矛盾する。
情報が失われないのは事実だが、例外を 1 つ作ると原則の検査が実装ごとの判断になる。
代償は一時的な容量 2 倍（GC までの 30 日間）で、設計レビュー（2026-09-15）で指摘。

---

## D-11 FLAC の圧縮レベル統一はしない

**決定**: `flac -8` は新規生成物（CD リップ、WAV 正規化）にのみ適用。

**理由**: `flac -8` は圧縮率だけの違いでデコード結果は完全に同一。
削減は 0.5〜1.5% に対し、200GB の書き直しと、ZFS スナップショットが旧ブロックを
掴むことによる実効使用量の倍増が発生する。

**ただし**: STREAMINFO の MD5 が未設定の FLAC は再エンコードで補填する。
未設定だと同一性解決の第 2 手段が使えず、遡及照合も不可能。

---

## D-12 CD は全ディスクを 1 本の PCM として吸う

**決定**: トラック単位ではなく `cd-paranoia '1-' -` で一括取得し、
オフセット適用後に分割する。

**理由**: 読み取りオフセット補正がトラック境界をまたぐため、
トラック単位で吸うと補正が正しく適用できない。

---

## D-13 CTDB を主、AccurateRip を補助

**決定**: 照合は CTDB を優先する。

**理由**: CTDB は公開 HTTP API があり、確認だけでなくパリティによる修復データも
返る。AccurateRip の DB 利用は本来 Illustrate 社の許諾前提というグレーさがある。

**重要**: 不一致は「不良」を意味しない。ドライブオフセット差、ギャップ処理差、
隠しトラック、データトラックの存在で普通に外れる。`mismatch` は「要確認」として
扱い、警告色で表示しない。

---

## D-14 検証状態と可逆性は別軸

**決定**: `source_type` / `lossless` / `verification` の 3 属性は直交。

**理由**: ハイレゾや配信音源はそもそも TOC が存在せず照合対象外。
これは `unverifiable` であって「未検証」ではない。1 つのフラグで表すと
いつまでも気持ち悪いリストが残る。

---

## D-15 Subsonic API 互換は実装しない

**決定**: Android へは Derived + m3u8 のファイル同期を継続。

**理由**: 実装すれば既存クライアントが使えるが、Subsonic の認証は
パスワードの salt+md5 を送る方式で、argon2 ハッシュのみを保存する設計と
両立しない（別クレデンシャル保管が必要になる）。

---

## D-16 スマートプレイリストは DSL → AST → SQL

**決定**: foobar 風文法を pest でパースし、AST(JSON) を DB に保存。
実行時にパラメータ化 SQL へ変換する。

**却下**: 生 SQL を保存する案（スキーマ変更で全ルールが壊れ、
インジェクション面も抱える）。UI の条件ビルダのみの案（foobar に慣れた
ユーザには遅く、複雑な条件が組めない）。

`rule_source`（原文）と `rule_ast` を両方保存するのは、AST から DSL を
逆生成すると整形が変わりユーザの記述意図が失われるため。

---

## D-17 認証は LAN 限定でも入れる

**決定**: 単一パスワード + argon2id + セッション Cookie。

**理由**: 攻撃者対策というより事故対策。この API は 300GB のライブラリを
リネーム・削除・タグ上書きできる。認証がないと、別タブのスクリプト、
家族の端末、うっかり有効にしたポートフォワードがそのまま破壊的操作に届く。

---

## D-18 SMB は insensitive + formD

**決定**: `casesensitivity=insensitive`, `normalization=formD`。

**理由**: (a) Windows の foobar2000 が `Cover.jpg` を、spindle が `cover.jpg` を
作る事故を ZFS 層で潰す。spindle は自身の生成パスの一意性は保証できるが、
他クライアントが作るファイルは制御できない
(b) 日本語の濁点には NFC / NFD の 2 表現があり、macOS 由来のパスは NFD に
なりがち。正規化なしだと「見た目が同じで別ファイル」が発生し、目視で気づけない。

---

## D-19 移行は新規データセット作成 + rsync

**決定**: 既存データセットの `zfs rename` による流用はしない。

**理由**: `casesensitivity` と `normalization` は**作成時のみ指定可能**で
後から変更できない。rename で流用すると Library だけ `sensitive` のまま残る。

**代償**: 300GB の実コピーと、一時的に 2 倍の空き容量が必要。
`zfs send/recv` も不可（作成時プロパティが継承されてしまう）。

**注意**: NFSv4 ACL は rsync で引き継げない（`rsync -A` は POSIX ACL）。
コピー後に TrueNAS の ACL エディタで適用し直す。

---

## D-20 インポートは Inbox + 承認キュー

**決定**: スキャン対象ディレクトリを増やす方式は採らない。

**理由**: 外部ディレクトリを直接スキャン対象にすると、そのツリーのパス規約と
タグ品質がそのまま Library に混入する。承認キューがないと `_Unsorted` が
際限なく育つ。

---

## D-21 メタデータは MusicBrainz + 手入力の一級市民化

**決定**: 照会結果ゼロでも CD 取り込みウィザードが完走できることを必須要件とする。
トラックリスト貼り付け（テキストを行解析して割り付け）を実装する。

**理由**: 同人・VTuber・インディーズの国内盤は MusicBrainz 未登録が常態。
プロバイダを増やすより、手入力経路の品質を上げる方が効果が大きい。

---

## D-22 マルチチャンネルは切り捨て

**決定**: 2ch 以外は Derived 対象外、album RG の集計からも除外。
トラック単位でステレオダウンミックスをオプトイン可能。

**理由**: 5.1 が混ざると album gain の集計でも Opus 変換でも例外処理になる。
DSD / SACD ISO は非対応。

---

## D-23 ジョブの dedup は未完了の間だけ効かせる

**決定**: `jobs.dedup_key` は `queued` / `running` の行に対する partial unique index。
列 UNIQUE にしない。バックオフの待ち時間は `run_after` 列に永続化する。

**理由**: 列 UNIQUE だと `done` / `failed` / `cancelled` 後に同じキーを永久に投入できない。
`scan` / `inbox` の固定キーは一生 1 回しか作れず、同じ `tag_version` の手動再試行も不能。
バックオフをメモリだけで持つと、再起動直後に失敗ジョブが一斉再実行される。

**却下**: 完了時に `dedup_key` を NULL に書き換える案。履歴から「どのキーで走ったか」が
消え、二重実行の事後調査ができなくなる。

---

## D-24 一括編集は DB 先行更新 + pending 記録、スキャナは pending を巻き戻さない

**決定**: 編集バッチは 1 トランザクションで「旧値・新値・事前条件（dev / inode / mtime /
rel_path）を `edit_ops` / `edits` に記録 → DB 更新 → track 単位の tagwrite ジョブ投入」を
行い、ファイル反映は非同期。**ファイル反映の単位はトラック（op）**で、同一トラックの
全差分を 1 回の tmp+rename で書く。DB の値は確定値ではなく**書き込み意図の
オーバーレイ**と定義し、UI に pending を表示する。
`pending` の op があるトラックは、スキャナがその op の所有する論理フィールドを
再評価しない（物理的な所在は追随する）。**pending 中の再編集は 409 で拒否**する。
巻き戻しは終端状態のバッチに限り、実際に applied になった op（既存の逆バッチで戻し済みの
ものを除く）だけを反転する。**結果は op 単位**で、1 フィールドでも現在値が元バッチの新値と
違えば op 全体を conflict にする（フィールド単位の部分適用はしない）。元バッチの
`reverted_at` は逆バッチが対象を全件 applied にしたときだけ立てる。
op が applied 以外の終端になるときは、同じトランザクションで DB 値をファイルの現在値へ
戻す（overlay の解消。次回スキャン任せにしない）。事前条件には `ctime_ns` と `tag_hash`
を含める。

**理由**: 「ファイルが正」と「DB を先に更新して UI へ即時反映」は、反映待ちの窓で
衝突する。この窓でスキャンが走ると編集が DB から巻き戻され、後続の tagwrite と
食い違う（設計レビューで指摘）。pending を一級の状態として持てば、スキャナ・tagwrite・
巻き戻し・起動時リカバリが同じ表を見て判断でき、クラッシュ点ごとの結果が決まる。

**却下**: ファイル書き込み後に DB を確定する案。6 万件の一括編集で UI が数分間
古いまま残り、その間の再編集が前の編集を見ない状態で行われる。

**却下（P0 では）**: pending 中の再編集を許し後続バッチが先行 intent を統合する案
（`superseded`）。先行バッチの集計・スキャナ抑止の解除・リカバリの定義が必要で、
P0 の価値に対して複雑すぎる。`edit_ops.result` に値だけ予約し、必要になったら足す。
409 の代償は「反映中の数分間はそのトラックを触れない」ことで、UI がバッジで示す。

**却下**: フィールド単位で tmp+rename する案。1 件目で inode が変わり 2 件目以降の
事前条件が必ず外れる（再レビューで指摘）。

---

## D-25 配布ビューは版が一致する Derived だけを返す

**決定**: `delivery` は `derived_files.src_audio_version = tracks.audio_version` の
ときだけ Derived を返し、音声版が古ければ Library 原本へフォールバックする。
タグ版だけ古い場合は Derived を返し `stale_tags = 1` を付ける。

**理由**: 存在だけで採用すると、音声差し替え（修復・再リップ）後の再エンコード待ちの間、
古い音声を正常品として配ってしまう。タグ版まで一致を要求すると、一括タグ編集のたびに
全曲が一時的に可逆原本の配布へ落ちて Android 同期が肥大する。

---

## D-26 hardlink は移行ブロッカー、Library 内では作らない

**決定**: `preflight.py` は `nlink > 1` のファイルを「解消必須」に分類する。
移行後に Library 内で見つかった場合（`nlink > 1`、または同じ走査で同一 inode を複数パスが
持つ場合）は **inode 段も md5 段も飛ばし `rel_path_key` だけで解決**し、警告バッジを出す。
spindle 自身は hardlink を作らない。

**理由**: hardlink は「1 トラック = 1 ファイル」と `(dev, inode)` 同一性の両方を壊す。
同じ inode を 2 パスが共有すると、片方へのタグ書き込みが他方を黙って書き換える。

**却下**: 各パスを別トラックとして登録する案。二重書き込みのリスクを受け入れる
ことになり、原則 4（巻き戻し）が破れる。

---

## D-27 trusted_cidrs は route allowlist だけ認証をスキップする

**決定**: `trusted_cidrs` からの接続は **route allowlist**（`/api/stream/:id`、
`/api/artwork/:hash`、`/api/tracks/:id` の限定フィールド、`/api/playlists/:id/export`）だけ
認証なしで通す。一覧・検索・SSE・履歴・ジョブ・設定はメソッドが GET でもセッション必須。
変更系は CIDR 内でもセッション Cookie を要求し、`Origin` があれば完全一致、無ければ
`Sec-Fetch-Site` で判定する（`Host` は使わない）。接続元の判定は socket のアドレスで、
`X-Forwarded-*` は `trusted_proxies` からのものだけ採用する。

**理由**: Cookie なしで破壊 API が通る経路があると、SameSite も CORS も CSRF の防御に
ならない（別サイトの JS から LAN IP への単純 POST は送信され、応答が読めないだけ）。
一方、他プレイヤーや curl からのストリーム参照にセッションを要求すると運用が面倒。
読み取りだけ許すのが両立点。

**却下**: 設定項目の削除（ストリーム参照に毎回セッションが要る）。
全 API で維持 + カスタムヘッダ必須（curl から使うたびにヘッダを付ける手間、実装増）。
メソッドベース（全 GET を開ける）: 履歴・ジョブのエラー文・rel_path・設定が LAN 全体に
見える。他プレイヤーの用途に SSE や履歴は要らない。
`Origin || Host` の論理 OR: クロスサイト POST でも Host は送信先になるので防御にならない。

---

## D-28 初期パスワードは環境変数で与える

**決定**: `SPINDLE_INITIAL_PASSWORD` を、DB にパスワードが無い初回起動時だけ読み、
argon2id でハッシュして保存する。以後は無視する。環境変数も DB も無ければ
ロックモード（`/health` 以外 503）で起動し、先着で設定できる画面は置かない。

**理由**: 「最初にアクセスした人がパスワードを決める」画面は、起動直後の窓が無防備になる。
LAN 内・単一ユーザでもコンテナの再作成のたびにその窓が開くので、明示的に与える方が
事故が少ない。

**代償**: compose.yaml に平文が残り得る。設定後に行を消す運用を README に書く。
サンプルは `${SPINDLE_INITIAL_PASSWORD:?...}` 形式にし、既知の既定値（`change-me` 等）を
書かない。既定値があると、そのまま起動した利用者が既知パスワードで初期化される。
ロックモードでは SPA の配信も止める（セットアップ画面が無いので出しても操作不能）。

**却下**: 起動ログに one-time token を出す案（`docker logs` を見る手順が要り、
TrueNAS の UI からだと一手間）。

---

## D-29 同一音声の重複は自動マージしない

**決定**: `audio_md5` が一致する active トラックが複数あっても別トラックとして扱う。
スキャンで「移動」と判定するのは候補がちょうど 1 行で、かつ移動元が消えている場合だけ。
重複は `duplicate_groups` ビューで UI に見せ、統合するかはユーザが決める。

**理由**: `audio_md5` は音声内容の識別子であってトラック実体の ID ではない。
コピー元が残っている・ベスト盤に同一マスターが収録されている・無音トラック、で
普通に衝突する。自動マージすると片方のタグ・プレイリスト所属が黙って消える。

---

## D-30 同一性の各段で候補を検証する

**決定**: 走査は **4 相**（inventory 固定 → 候補生成 → 決定的 claim → 単一トランザクションで
commit）で、判定は inventory 全体が揃ってから行う。(1) inode 段は inventory 内でその inode を
持つパスが 1 つだけ、`nlink = 1`、未 claim、`size` か `mtime_ns` の一致を要求し、両方違えば
`audio_md5` の一致を要求する。(2) audio_md5 段は候補がちょうど 1 行で、その旧 `rel_path_key`
が inventory に無いことを要求する。(3) rel_path_key 段は未 claim を要求する。取り合いは
`rel_path_key` 昇順で決着する。claim は `scan_runs.id`（`tracks.seen_run_id`）で管理し、
`missing_since` は run が `completed` になった finalize でだけ立てる。DB の path / key 更新は
予約済み一時 key 経由の 2 段階で行う（swap / 循環で UNIQUE を踏まない）。

**却下**: 走査しながら逐次判定する方式。コピー先を先に訪問すると、未訪問のコピー元を
「消失」と誤認して移動扱いにし、その後コピー元を新規登録する。並列走査では結果が順序依存。
`scan_generation` = 開始時刻を `seen_at` に兼用する案（epoch 秒では同秒の再実行と区別不能）。

**理由**: inode は削除後に再利用され、別ファイルが同じ inode を得る。並列走査では
2 パスが同じ行を取り合う。検証なしの一致採用は「見た目は正常だが別の曲に紐づく」
最悪の壊れ方をする。

**変更検出**: 最速パスの比較に `ctime_ns` を含める（mtime を保存する上書きの検出）。
版は `tag_hash` と音声フィンガープリントの**実差分**があるときだけ進める。音声
フィンガープリントは可逆が `audio_md5`、非可逆が**エンコード済みパケット列の SHA-256**
（`audio_fp`、demux のみ）。非可逆の `size` 変化を音声の変化とみなす案は却下: タグ書き換えで
コンテナサイズは普通に変わり、不変条件 3 に反する。spindle 自身の tagwrite / rename /
RG 書き込み / MD5 補填は音声不変が既知なので再計算せず `audio_version` を据え置く。
ZFS rollback は ctime も戻すので検出できず、`deep scan`（既定 30 日、手動可）で吸収する。

---

## D-31 パスは canonical key で比較し、dirfd 基準で開く

**決定**: `rel_path` / `rel_dir` に対して `casefold(NFD(...))` の canonical key 列を持ち
UNIQUE にする。ファイルは root の dirfd から `openat2(RESOLVE_BENEATH |
RESOLVE_NO_SYMLINKS)` で開き、symlink は辿らない。一時ファイルは対象と同じ
ディレクトリに `O_EXCL` で作る。外部コマンドは引数配列 + `--`（非対応なら `./` 前置）。

**理由**: ZFS は `insensitive` + `formD`（D-18）だが SQLite の UNIQUE は BINARY 比較で、
同じファイルを指す 2 つの文字列を別と見る。canonicalize → prefix 比較は symlink の
差し替えに負ける。先頭 `-` のファイル名は外部ツールにオプションとして解釈される。

**限界**: `casefold(NFD())` は spindle 側の保守的な同値規則で、OpenZFS の `u8_textprep`
と同一の保証はない（ß、トルコ語 I、合字、Unicode 版差）。DB の key は事前判定に留め、
最終判定は `O_EXCL` / `RENAME_NOREPLACE` の失敗で行う。`openat2` が無い環境は起動時に
落とす（フォールバックを設けない。非 Linux 開発機は結合テストを skip）。

**却下**: SQLite の `COLLATE NOCASE`（ASCII しか畳まない）。全角・半角の同一視
（NFKC の領域で、ZFS の formD とも別物。同一視しない）。

---

## D-32 album の同一性は構成トラックと MBID / DiscID で引く

**決定**: ディレクトリ rename 後の album は、`mb_release_id` / `discid` が**ちょうど 1 件**
一致し旧 rel_dir が消えているもの → 構成トラックの過半数が属していた album（旧 rel_dir が
消えているもの）の順で既存行を引き当て、`rel_dir` を書き換えて id を維持する。候補が
複数なら自動では寄せない。分割は移った側が新規、統合は最多の album が id を維持し、
構成 0 になった album は `albums.missing_since` を立てて行を残す（削除すると
`album_verifications` が CASCADE で消える）。`rel_dir` の更新もトラックと同じ 2 段階。

**理由**: album を `rel_dir` で作り直すと `album_verifications` / `artwork_id` /
プレイリストのアルバム参照が失われる。「パスは識別子ではない」は album にも適用する。

---

## D-33 一括編集の対象はプレビュー時のスナップショットに固定する

**決定**: `POST /api/tracks/batch/preview` でサーバが selection を解決し、対象 `track_id` と
各行の `tag_version` / 事前条件を短期保存して `selection_token`（TTL 15 分）を返す。
`PATCH /api/tracks/batch` はこの token の集合だけを対象にする。スナップショット後に
`tag_version` が変わった行は conflict になる。

**理由**: フィルタ形の selection を preview と apply で別々に評価すると、その間のスキャンで
増えた（ユーザが一度も見ていない）行まで破壊的操作の対象になる。「プレビューで見たもの
だけに効く」は一括編集の基本保証。ID を HTTP で列挙しない要件とは両立する。

**却下**: apply 時にフィルタ式 + `scan_run_id` を再送して一致を要求する案。スキャンが
走るたびに 409 になり、6 万件の適用中に必ず踏む。

## D-34 起動時に設定を厳格に検証し、不正なら起動しない

**決定**: `config.toml` は未知キーをエラーにし、値の範囲（`flac_compression` 0..=8、
`gc.retention_days` ≥ 1、`reference_lufs` は有限の負数、`flac_recompress_all = false` 固定、
`musicbrainz.rate_limit_per_sec = 1` 固定など）を起動時に検証する。`[paths]` の各ルートは
絶対パスで、**互いに同じでも入れ子でもなく**（Derived が Library の下にあるとスキャナが
Derived を原本として拾う）、**ディレクトリとして存在**しなければならない。同一性は字面に
加えて `(dev, inode)` でも見る（symlink / bind mount の別名を弾く）。入れ子は字面に加えて
symlink 解決後（`canonicalize`）のパスでも見る。いずれかを満たさなければプロセスは起動せず、
理由をログに出して終了する。
待ち受けアドレスは `[server].listen`（既定 `0.0.0.0:8080`、省略可）で持つ。

保証の範囲: 検出できるのは「無い」「同じ」「symlink 解決後まで含む入れ子」まで。
**bind mount で Library の子ディレクトリを別パスに見せた入れ子**は `canonicalize` にも
`(dev, inode)` にも現れないので検出しない（`/proc/self/mountinfo` の解析が要り、compose の
volumes でそれを書く事故は考えにくい）。**空の別ディレクトリが誤ってマウントされている**
ケースも区別できない（それはスキャナ側の "全曲 missing" 閾値の話で、P0-6 以降で扱う）。

**理由**: マウント忘れで空の `/library` を走査すると、次の finalize で全曲に `missing_since`
が立つ。復旧は可能（再発見で復活する）だが、GC の猶予やバッジの意味が壊れる。typo で
「設定したつもり」になるのも同種の事故で、黙って既定値に倒すより起動時に落ちる方が安い。
listen をハードコードしないのは、開発機で複数インスタンスを並べるため。ポート公開は
compose 側の責務なので、既定値はコンテナ内で使う値に固定する。

**却下**: 存在しないルートを自動作成する案。ホスト側のマウント漏れを隠してしまう。
`[paths]` の存在確認を warn に留める案。全曲 missing の後始末の方が高くつく。

## D-35 認証まわりの固定値と境界

**決定**: ログインのレート制限は同一 IP から 15 分に 10 回で、超えると 429（設定には
出さない）。枠は argon2id の検証に入る**前に原子的に予約**する（並行送信で上限を超えさせない）。
表はハード上限 1024 IP で、超えたら期限切れを掃除し、それでも一杯なら最古の窓を追い出す。
`X-Forwarded-For` は trusted proxy から受け取ったチェーンを右から辿り、trusted proxy の hop を
飛ばした最初の IP をクライアントとする（それより左は自己申告）。壊れた値はクライアント不明。期限切れセッションの掃除は起動時とログイン成功時に行い、周期タスクは置かない。
SPA の静的配信（`/api` 以外の GET）はセッション不要で通す（ログイン画面を出すため）。
`POST /api/auth/login` も CSRF 検証の対象にする。`GET /api/auth/session` はセッションが
無ければ 401（SPA はこれでログイン画面へ切り替える）。`OPTIONS` は変更系でも SPA でも
ないのでセッション必須扱いになり、結果として CORS プリフライトは常に失敗する。

**理由**: 単一ユーザ・LAN 内なので制限値を調整する場面が無く、設定項目を増やすだけ損。
セッションはログインでしか増えないので、ログイン時の掃除で溜まらない。curl からログイン
したい場合は `Sec-Fetch-Site: none` を付ければ通る（trusted_cidrs の用途は stream 参照で、
ログインは想定しない）。

**却下**: 周期的な掃除タスク（P0-4 のジョブ基盤に載せる方が筋だが、それまで待つ理由が
ない程度の量しか溜まらない）。

---

## D-36 ジョブ基盤の固定値と境界

**決定**: 仕様（SPEC §8）が定めていない値と境界を次のとおり固定する。設定には出さない。

- バックオフは失敗 n 回目で `10 × 2^(n-1)` 秒、上限 1 時間。`attempts` は**失敗のたびに**
  進める（claim 時ではない）。`attempts >= max_attempts` で `failed`。既定の `max_attempts`
  はスキーマの DEFAULT と同じ 5
- 手動 `retry` は `failed` / `cancelled` だけを対象にし、`attempts` を 0、`run_after` /
  `cancel_requested_at` / 進捗を NULL に戻す。`last_error` は前回の理由として残す。同じ
  `dedup_key` の未完了ジョブがあれば 409 `duplicate`。`done` / `queued` / `running` の retry は
  409 `not_retryable`
- `cancel` は `queued` なら即 `cancelled`（ハンドラを経ない）、`running` なら
  `cancel_requested_at` を立ててプロセス内の CancellationToken も倒す。終端は 409
  `not_cancellable`。どちらも 202 で返す（実際の遷移は SSE で観測する）
- **cancel 要求は再実行より優先する。** `cancel_requested_at` が立った行は、
  (a) 起動時リカバリで `running` / `queued` とも `cancelled` に送る、
  (b) ワーカーが claim の前に `queued` を掃除して `cancelled` に送る（バックオフ中の cancel）、
  (c) ハンドラが token を見ずに失敗 / 再キューを返しても `cancelled` にする（試行回数は数えない）。
  例外は**完了**で、ハンドラが `Done` を返せば cancel が後から来ていても `done`（仕事は済んで
  いる。API は 202 を返しているが、最終状態は SSE / 一覧で分かる）
- 進捗は SSE には毎回流し、DB には 250ms に 1 回（および `done >= total` のとき）だけ書く。
  未書き込みの最後の値は**終端遷移と同じトランザクション**で書く。DB の `cancel_requested_at`
  は永続化のたびに読み、立っていれば token を倒す（API 経由でない要求も拾う）
- **版付きジョブ（tagwrite / transcode）は基盤が payload の `track_id` をロックしてから、同じ
  トランザクションで stale 判定する。** ロック待ちの間に版が進んだジョブを実行しないための
  順序。同じトランザクションで先にトラックの存在を見て、**無ければロックを取らずに no-op
  `done`**（`track_locks` は `tracks` への FK なので、消えたトラックはロックできない）。
  ロックが取れなければ試行回数を数えずに再キュー、版が古ければロックを返して no-op `done`。
  ハンドラは既にロックを持った状態で始まり、追加のロックを取った後は `JobContext::is_stale()`
  で再確認できる。版付き種別の payload に整数の `track_id` と版が無いものは**投入時に拒否**し、
  DB に直接入った不正行は再試行せずに `failed` にする（ゲートを迂回して実行しない）
- `track_locks` が取れずに `Outcome::Requeue` で戻したジョブは `run_after = now + 1` 秒で
  再度対象になる。試行回数は数えない。ロックはジョブの終端遷移と同じトランザクションで
  全解放する
- ハンドラの panic はそのジョブの失敗（バックオフ対象）に変換し、ワーカーは止めない。
  終端遷移の書き込み自体が失敗（DB 障害）したときも `running` に固着させず、失敗として
  記録を試み、それも駄目なら次回起動のリカバリに任せる
- **停止は共有の CancellationToken 1 つで行う。** シグナルハンドラがそれを倒し、HTTP サーバの
  graceful shutdown・ワーカー・SSE ストリームが同時に止まる（SSE を閉じないと axum が接続の
  終了を待ち続け、コンテナの stop timeout で SIGKILL される）。ワーカーはループの先頭と
  各 claim の直前で token を見て新規 claim を止め、claim の DB 往復中に停止が来た分は起動せずに
  `queued` へ戻す。実行中タスクは**破棄**する。DB 上は `running` のまま残り、次回起動のリカバリで `queued` に
  戻る（ジョブは冪等なのでこれで良い）
- `GET /api/jobs` は未完了を先頭に最大 1000 件で、一覧と `summary` は同じ読み取り
  スナップショットで取る。SSE の keep-alive は 15 秒間隔、broadcast の容量は 1024。
  **購読者が遅れて取りこぼしたときは、読み飛ばした後続イベントより前に `event: resync` を
  流す。** クライアントは resync を受けたら一覧を再取得する。接続直後の取りこぼしを避けるため、
  クライアントは**SSE を開いてから**一覧を取得する（逆順だと開く前のイベントを失う）
- 外部プロセスは `process_group(0)` で自分のグループのリーダーとして起動し、キャンセル時は
  グループへ SIGTERM → **`killpg(pgid, 0)` でグループが空になったかを見て** 2 秒待ち、残っていれば
  グループへ SIGKILL。leader の終了だけでは判定しない（TERM を無視する孫が残る）。待つ間も
  leader を `try_wait` で回収し続ける（zombie はグループの一員として数えられ、消えたことに
  ならない）。グループに 1 つでもプロセスが残る限りその pgid は再利用されない（pgid として
  参照中の pid は割り当てられない）ので、`killpg` での判定と送信は leader 回収後も安全。
  wait を経ずに drop された（panic / 停止時の abort）ときはグループへ SIGKILL。tmp は `TempGuard`（drop で削除）で持ち、
  キャンセル・失敗・panic のどの経路でも残さない

**理由**: 単一ユーザ・LAN 内で調整する場面が無く、設定項目を増やすだけ損。バックオフの
基数を 10 秒にしたのは NAS の一時的な I/O 詰まりを跨ぐには十分で、UI の「失敗」表示が
1 時間以上遅れることもないため。進捗の間引きは 1 万件スキャンで進捗のたびに WAL へ
書かないため。停止時に実行中を待たないのは、コンテナの stop timeout（既定 10 秒）内に
数分単位のジョブが終わる保証が無く、待っても SIGKILL されるだけだから。cancel を再実行より
優先するのは、ユーザが止めたジョブがバックオフ後や再起動後に勝手に動き出すのが破壊的
操作（tagwrite / rename）では取り返しがつかないため。

**却下**: claim 時に `attempts` を進める案（プロセス kill の繰り返しで `failed` に達する
ジョブが出るが、その状況は電源断であってジョブの失敗ではない）。停止時に実行中ジョブへ
キャンセル要求を出す案（`cancelled` は「ユーザが止めた」の意味であり、再起動で再開すべき
ものと区別がつかなくなる）。Lagged を SSE の切断で伝える案（再接続のたびにイベントを
失う窓ができ、resync 1 行を流す方が単純で確実）。

---

## D-37 パス安全層・同一性解決・フィンガープリントの固定値と境界

**決定**: 仕様（SPEC §5 / §6）が定めていない値と境界を次のとおり固定する。設定には出さない。

- **canonical key は `NFD → full casefold → NFD`。** casefold の結果は NFD とは限らない
  （U+1F88 → U+1F00 U+03B9 のように分解が必要な文字がある）ので末尾で NFD を再適用し、
  `key(key(x)) == key(x)` を保つ。casefold は Unicode の full case folding（`caseless`）で、
  `ß` と `ss`、`ﬁ` と `fi` は同じ key になる。これは spindle 側の**保守的な**同値規則で、ZFS が
  別名とみなす組を同名と見ることはあっても逆はない想定。差の実測は `tests/zfs_corpus.rs`
  （`#[ignore]`）で対象 NAS 上で行い、結果をここに追記する
- 相対パスの要素は、`/` 区切り・先頭 `/` なし・`.` / `..` なし・NUL / `\` なしに加え、
  SMB / exFAT 禁止文字 `< > : " | ? *` と制御文字を含まず、末尾がドット・スペースでなく、
  Windows 予約名（CON PRN AUX NUL COM1–9 LPT1–9。**拡張子付きも予約**、大小文字無視）でなく、
  255 バイト以下。パス全体の長さ上限（240 UTF-16 単位）は検証ではなく**生成側**（P0-11）で
  切り詰める。先頭 `-` のファイル名は正当で、外部コマンドへ渡す側で無害化する
- root は `O_DIRECTORY` で開いた dirfd を保持し、以後は `openat2(RESOLVE_BENEATH |
  RESOLVE_NO_SYMLINKS)` だけで解決する。**`O_NOFOLLOW` は付けない**（`O_PATH` と組むと末尾の
  symlink 自体が開けてしまい、親ディレクトリの解決で symlink を通す穴になる。全段の拒否は
  `RESOLVE_NO_SYMLINKS` の ELOOP に任せる）。stat は親を `O_PATH` で開いてから
  `statat(AT_SYMLINK_NOFOLLOW)`、rename / unlink も親 dirfd 基準。tmp は
  `.spindle-tmp-<16 hex>` を `O_EXCL` で作り、EEXIST なら 8 回まで引き直す。
  起動時に全 root（library / derived / archive / inbox / playlists）を開き、
  `openat2` が ENOSYS ならそこで落ちる。非 Linux では `RootDir` は空実装で `open` が常に
  `Openat2Unsupported` を返す（crate はビルドでき、実ファイルの結合テストは
  `#![cfg(target_os = "linux")]` で Linux の CI だけが走らせる）
- 外部コマンドの既定タイムアウトは 600 秒（エンコード等は呼び出し側で伸ばす）。stderr は
  **読みながら**末尾 16 KiB だけを保持し（全量を溜めない。長時間の ffmpeg でもメモリは有界）、
  失敗時は warn、成功時は debug でログに出す。leader が終了した時点でプロセスグループに
  子孫が残っていれば（leader が fork して先に exit した）グループごと掃除してから返す。
  放置すると継承した stdout / stderr パイプの EOF が来ず呼び出し側が固まるか、孫が漏れる。`--` 方式では最初のパスの前に
  `--` を 1 度だけ置くので**オプションはパスより前に並べる**。`./` 方式は相対パスで先頭 `-` の
  ものだけ前置し、絶対パスは触らない。タイムアウトはキャンセルと同じ経路（プロセスグループ
  SIGTERM → 2 秒 → SIGKILL、D-36）で止め、エラー型で区別する
- 同一性解決は純関数（DB を触らない）で、inode 段 → md5 段 → path 段の順に**各段を全エントリ
  について** `rel_path_key` 昇順で回す。エントリごとに 3 段を回す方式は、rename 後の旧パスに
  別ファイルが置かれたとき、先に来た旧パスの path 一致が後のエントリの inode 一致から行を
  奪うので採らない。DB 側で同じ `(dev, inode)` を複数行が持つ（過去の hardlink）場合は曖昧
  として inode 段を飛ばす。同じ md5 の新パスが複数あり旧パスが消えている場合は key 昇順の
  先頭だけが移動、残りは新規（duplicate_groups に出る）。`changed` は
  `(dev, inode, size, mtime_ns, ctime_ns)` のいずれかの差で、md5 段の採用は常に `changed`
- `decoded_pcm_md5`（ALAC / WAV）は FLAC エンコーダが STREAMINFO に書くのと同じ流儀
  （チャンネルインターリーブ、リトルエンディアン、bps を 8 の倍数に切り上げたバイト数）で
  取るので、同じ PCM の WAV / ALAC / FLAC は同じ `audio_md5` になる（WAV → FLAC 正規化と
  ALAC からの移行で同一性が保たれる）。ALAC の bit depth は demuxer から来ないので magic
  cookie（`extra_data`、`frma` / `alac` atom 前置あり）の 6 バイト目から読む
- **非可逆の `audio_fp` は Opus を含めて symphonia の demuxer で取る。** symphonia が非対応
  なのは Opus の**デコード**であり、Ogg の Opus パケットは取り出せる。ffmpeg 経由にすると
  外部プロセスと ffmpeg の版差（remux 時のヘッダ生成）に結果が依存する。ハッシュはパケットの
  データをそのまま連結した SHA-256（長さ前置なし。demuxer の packetization が変わっても
  バイト列が同じなら同じ値）。MP3 / Opus / AAC(MP4) / Vorbis でタグ書き換え後に不変であることを
  テストで固定する
- `tag_hash` はキーを大文字化、値を NFC、キー順に整列（同一キーの多値は入力順を保持）した
  `(key, value)` 列を、それぞれ 8 バイト LE の長さ前置で連結した SHA-256。埋め込み画像は
  `PICTURE` キーに `<mime>:<画像バイト列の SHA-256 hex>` として入れる（カバー差し替えも
  `tag_version` を進め、Derived のタグ追随対象になる）

**理由**: 設定に出しても調整する場面が無い。key の規則を保守的にするのは、DB の UNIQUE が
FS より緩い（別と見て同名を作ろうとする）方が、FS より厳しい（同じと見て正当な名前を拒む）
より危険なため（前者は `O_EXCL` / `RENAME_NOREPLACE` が最終防衛線になるが、黙って上書きに
近い挙動に見える）。PCM MD5 を FLAC と揃えるのは、正規化・移行で `audio_md5` が変わると
Derived の再エンコードと重複検出の両方が空回りするため。

**却下**: `char::to_lowercase` による簡易 casefold（`ß` などが畳まれず、ZFS の挙動との差が
増える方向）。SQLite の `COLLATE NOCASE`（D-31）。パケット列のハッシュに長さを前置する案
（packetization 差で値が変わる）。`bits_per_sample` が無い ALAC をバッファ幅（32 ビット）で
ハッシュする案（FLAC と一致しなくなる）。

---

## D-38 スキャナの固定値と境界

**決定**: 仕様（SPEC §7.1 / §6）が定めていない値と境界を次のとおり固定する。設定には出さない。

- **スキャンの起動経路**は `POST /api/scan`（`{"kind": "incremental" | "deep"}`、dedup key は固定の
  `scan`）と、**起動時に incremental を 1 件投入**する（停止中の外部変更を拾う。既に queued /
  running なら dedup で何もしない）。incremental は、完了した deep scan が
  `[scan].deep_interval_days` より古い（または一度も無い）とき deep に**昇格**する。`0` なら
  昇格しない。deep は全既存行を「変更あり」として扱い、`tag_hash` と音声フィンガープリントを
  再計算する。**実差分があれば版を進める**（deep は stat に見えない変更 — ZFS rollback など —
  を拾うためのもので、「再計算だから据え置く」ではない）
- 走査対象は拡張子で決める（flac / opus / m4a・mp4・aac / mp3 / wav / ogg・oga / wv / ape /
  aiff・aif）。`.` で始まるファイルは黙って飛ばす（macOS の `._x`）。symlink と、SMB / exFAT
  制約に反する名前（`RelPath` が拒否するもの、UTF-8 でない名前）は**対象外として一覧に出す**。
  対象外のディレクトリは配下ごと飛ばす。walk の途中でディレクトリを読めなければ run は
  `failed`（`completed` は「root を開けて walk がエラーなく完了」なので、途中欠落を missing に
  しない）。音声として読めないファイル（壊れた FLAC 等）は `errors` に数え、既存行なら物理属性
  だけ更新し、新規なら登録しない。run 自体は `completed` にする
- **`.spindle-tmp-*` の回収**は mtime が 1 時間より古いものだけ（並走中の tagwrite の tmp を
  消さないための猶予）
- **`artist_display`** は多値 ARTIST を `", "` で連結（foobar2000 の `%artist%` と同じ見え方）。
  `albumartist` キャッシュ列は ALBUMARTIST が無ければ ARTIST の先頭値で代用する。`track_no` /
  `disc_no` は先頭の整数（`"3/12"` → 3）
- **`category_id` の推定**（album 単位）: 先頭ディレクトリ名が `categories.name` と canonical key で
  一致すればそれ → 構成トラックの GENRE を `genre_category_map` で引く → どちらも無ければ NULL
  （`_Unsorted` への配置はパス生成側 P0-11 の判断）。album のメタデータ（albumartist / album /
  date / original_date / mb_release_id / discid / disc_count）は構成トラックの**最頻値**
  （同数なら文字列順で先）。`edition` は P0-6 では設定しない
- **album 照合の候補順**は MBID / DiscID が 1 件一致 → 構成トラックの過半数が直前まで属していた
  album → そのディレクトリに既にある album → 新規。既存 album を別のディレクトリへ寄せてよいのは
  「旧 rel_dir が inventory に無い」か「**旧 rel_dir の現在の構成の過半数が別の album に属して
  いる**」ときで、後者が 2 ディレクトリの swap を id 維持で解く鍵になる（仕様の「旧 rel_dir が
  inventory に無ければ」の字義どおりだと swap は両方新規になる）。ディレクトリを別の album に
  明け渡した album が誰にも claim されなかったときは `rel_dir` を予約 key（`\0displaced:<id>`）へ
  退避して UNIQUE を避け、構成 0 なので finalize で missing になる。変更のないディレクトリ
  （最速パスだけ）は照合を省く
- パスの 2 段階更新の一時 key は `\0track:<id>` / `\0album:<id>`（NUL は `RelPath` が拒否するので
  実 key と衝突しない）。外部 rename の**宛先 key を今回 claim されなかった別の行**（missing 行を
  含む）が占有していれば、同じトランザクションでその行の key を `\0vacated:<id>` へ明け渡させる。
  行は残り、finalize で missing になる（SPEC §7.1）。inventory 内で canonical key が重複する
  ファイル（case-sensitive な FS で大小文字だけ違う 2 ファイル）は後から見つかった方を対象外
  （`DuplicateKey`）にして UNIQUE を踏まない
- **pending op は commit トランザクションの中で読み直す。** Phase 2 のスナップショットは長い
  Phase 3 の間に編集バッチ（DB 先行更新 + pending）に追い越され得るので、スナップショットの
  値で判断すると編集意図をファイル値で巻き戻す（D-24 違反）。さらに **rel_path と物理属性が
  スナップショットから変わっている行（tagwrite の tmp + rename や rename ジョブが Phase 3 中に
  完了した）は、今回の inventory と読み取りが古いので何も適用せず `seen` だけ更新する**
  （`overtaken` として報告。次回スキャンで整合する）。追い越された行は album 照合の
  ディレクトリ集計にも入れない（所在も所属も DB の現在値が正しい）。scan は track_locks を
  取らないので tagwrite / rename と並走する前提
- pending の rename op があるトラックの `rel_path` は op が所有する（overlay で先に変わっていて
  よい）。外部移動の判定は DB の `rel_path` ではなく **`edit_ops.expected_rel_path`（記録時点の
  物理パス）** と inventory のパスを key で比べ、違えば `skipped_conflict`。同じなら物理属性と
  タグ（rename op はタグを所有しない）は通常どおり追随する
- 音声の変化は同種のフィンガープリントの差に加え、**種類が変わった**（同じパスで ALAC ⇔ AAC
  など可逆 ⇔ 非可逆の差し替え）ときも `audio_version` を進める。旧値が片方しか無いので同種比較
  では見えない
- root 直下へ移ったトラックは `album_id` / `album` を NULL にする（album = ディレクトリ。root は
  album を持たない）
- pending の `tags` op があるトラックはタグ・キャッシュ列・`tag_hash`・`tag_version` を触らない。
  pending の `rename` op があるトラックで外部 rename を検出したら `rel_path` を据え置き、op を
  `skipped_conflict`（error に新パス）にする。物理属性はどちらも追随する（D-24）
- Phase 3（タグ読込 + フィンガープリント）の並列度は CPU コア数（`available_parallelism`）。
  進捗は Phase 3 の件数で報告し、DB への永続化は `JobContext::progress` の間引きに任せる
- **性能の参照値**（`tests/scanner.rs::perf_10k_synthetic_tree`、`--release`、1 万件 = FLAC 8 割 /
  Opus 2 割 / アルバム 12 曲、2 回目は warm cache）: 2026-09-16、TrueNAS ホスト（AMD Ryzen 5 7600、
  12 論理コア、`/tmp` = tmpfs、DB も tmpfs）で **1 回目 2.87 秒（new=10000）、2 回目 0.15 秒
  （unchanged=10000）**。ZFS 上の実ライブラリでは 1 回目はタグ読込の I/O が支配的になるが、
  2 回目は stat だけなので同程度に収まる想定

**理由**: 起動時スキャンは「DB はキャッシュ」の原則の実装で、停止中の外部変更を UI を開く前に
拾う。tmp の猶予 1 時間は tagwrite が 1 ファイルに要する時間の上限を大きく超える値で、短すぎて
書き込み中の tmp を消す事故を避ける。swap の扱いを字義より広げたのは、「パスは識別子ではない」
を album にも適用する D-32 の意図に沿うため。

**却下**: ffmpeg で全ファイルの音声属性を取る案（lofty の properties で足りる。外部プロセス
1 万回は遅い）。`.spindle-tmp-*` を無条件に回収する案（並走する tagwrite の tmp を消す）。

## D-39 トラック一覧 API の固定値と境界

**決定**: 仕様（SPEC §9 / §12）が定めていない値と境界を次のとおり固定する。設定には出さない。

- **フィルタ式は URL エンコードした JSON 1 文字列**で、キーはホワイトリスト（未知キーは 400）:
  `category`（`categories.name`）/ `albumartist`（`tracks.albumartist` 完全一致）/ `album_id` /
  `playlist_id` / `flags`（`unverified` `duplicate` `missing` `no_rg` `pending` `conflict` `hardlink`。
  複数は AND）/ `q`（検索語）。サイドバーの 3 区画（ツリー・プレイリスト・固定フィルタ）と
  検索ボックスをそのまま表せる。`selection.filter` も同じ文字列。P1-7 のスマートプレイリスト
  DSL は同じ JSON に `dsl` キーとして足す（DSL を一覧のフィルタ式に採用して P1-7 を前倒しする案は
  「タスクをまたいで先回りしない」に反し、プレイリスト所属も表せないので却下）
- `unverified` は `verification = 'not_attempted'` だけ。`unverifiable`（TOC が無い音源）と
  `mismatch` は別の意味を持つバッジなので混ぜない（SPEC §6）
- **ソートはホワイトリスト**（`album`（既定。albumartist, album_id, disc_no, track_no）/ `title` /
  `artist` / `album_title` / `albumartist` / `date` / `duration` / `codec` / `rel_path` / `id`。
  `-` 前置で降順）。**キーセットページング**で、カーソルは「発行時のソート `s` / 最終行のソートキー値
  `k` / `id`」の JSON オブジェクトを base64url にした不透明文字列。ソート列は NULL を許すが行値比較は
  NULL で不定になるので、
  ソート式は `coalesce(col, '' | 0 | -1)` に畳み、同じ式の**式索引**を `0002_tracks_sort_indexes.sql`
  に持つ（NULL は昇順で先頭、降順で末尾）。SQLite は式索引に行値比較の範囲最適化を掛けないため、
  述語は「先頭キーの `>=` / `<=`（索引の入口）AND 全キー + id の行値比較（絞り込み）」の 2 段にする。
  **カーソルは発行時のソート（向き込み）を持ち**、デコード時にキー数と値の型（TEXT / INTEGER）を
  ソートキーの型並びと照合する。型が違う値をバインドすると SQLite の storage class 順序で比較が
  常に真になり先頭ページが再掲されるため、キー数だけの照合では足りない。**リクエストの `sort` と
  発行時のソートが違う（向き違いを含む）カーソルは `Query::from_params` で 400**、DB 層に直接
  届いた不一致は空ページ（二重防御。エラーにしない）。`limit` は 1..=1000、既定 100
- **バッジ列は行ごとの索引検索で引く**: pending は `edit_ops` の partial UNIQUE を LEFT JOIN、
  最新 op は `max(id)` の相関サブクエリを LEFT JOIN、重複は `idx_tracks_md5` への相関 EXISTS。
  `duplicate_groups` ビューを LEFT JOIN すると毎回 GROUP BY の実体化と自動索引（temp B-tree）が
  走るので使わない（SPEC §9 の「LEFT JOIN で引く」は「1 クエリで返す」の意味に読み替える）。
  `total` は同じ読み取りトランザクションで `count(*)` を取る
- フィルタ `pending` / `conflict` は行ごとの相関サブクエリではなく **`edit_ops` 側から集合を作って
  `IN` で引く**（該当行は少数で、全行を歩くと 6 万回の索引検索になる。計測で 124ms → 0.2ms）。
  `duplicate` は相関 EXISTS のまま（ビューの `IN` は 2 倍遅い）
- **フィルタ付きソートの temp B-tree は許容する。** 索引の無い組み合わせ（`category` で絞って
  `album` 順など）では planner が絞り込み側の索引を選んで ORDER BY を temp B-tree で解く。
  絞り込んだ集合のソートなので 100ms 以内に収まる（下の計測）。無フィルタの全ソートキーと
  バッジ用 JOIN、`total` に temp B-tree が出ないことはテストで固定する
- 検索は **3 文字以上（文字数。バイト数ではない）で FTS5 trigram**、未満は 4 列（title /
  artist_display / album / albumartist）の `LIKE ... ESCAPE '\'`。FTS には入力全体を 1 フレーズ
  （`"` は `""`）として渡し、演算子・構文文字を解釈させない。`GET /api/search?q=` は一覧と同じ
  レスポンス形（`filter.q` を差し替えるだけ。表の集合を差し替える SPEC §12.1 の作りに合わせる）
- **`selection_token` の保存はプロセス内メモリ**（TTL 15 分、上限 16 件で最古を追い出す）。
  DB に置かないのは、再起動で消えてよい短期の状態で、apply 側が 409 `preview_stale` で preview から
  やり直せるから。token は 32 バイトの乱数（base64url）で、**乱数源が読めなければ発行しない**
  （時刻などで代用しない）。apply は `take`（期限確認と削除を同じロックの中で行う）で token を
  原子的に消費し、同じ token の並行 apply は 1 つだけ通る。スナップショットは `track_id` /
  `tag_version` / `audio_version` / 事前条件（dev / inode / size / mtime_ns / ctime_ns / tag_hash /
  rel_path）と preview 時の `ops` を持つ。`ids` 形の解決は `json_each` で 1 パラメータに畳む
  （数万個のプレースホルダを並べない）
- `GET /api/tracks/:id` を trusted_cidrs から**セッション無し**で引いたときの限定フィールドは
  id / title / artist_display / album / albumartist / track_no / disc_no / date / duration_ms /
  codec / lossless。パス・編集状態（pending / conflict）・重複・missing・検証結果は含めない（D-27）
- `GET /api/albums` はページングしない（数千件。ツリーの構築に全件を使う）。`track_count` /
  `duration_ms` は active なトラックだけを数える
- SSE `library` は **scan ジョブの完了時に 1 回**流す。変更行（新規・更新・移動・復活・明け渡し・
  missing 確定・pending rename の conflict 化・読めなかったファイルの物理属性更新）が 200 件以下なら
  `ids`、超えたら `bulk`、0 件なら流さない。基準は「表の表示値（バッジ含む）が変わった行」。
  commit 後に流すのでクライアントが再取得すれば新しい値が見える
- **性能の参照値**（`tests/tracks_perf.rs`、`--release`、6 万件 / 5,000 album / 履歴 10 万 op /
  重複 5% / pending 50 件、warm cache、5 回の中央値）: 2026-09-16、TrueNAS ホスト（AMD Ryzen 5 7600、
  12 論理コア、DB は tmpfs）で **既定ソート 1 ページ目 0.75ms、title 昇順 1.2ms、599 ページ目
  1.2ms、`category` 絞り込み 12ms、`duplicate` 32ms、`conflict` 19ms、`no_rg` + `-date` 21ms、
  FTS で全行に当たる語 53ms（最悪）、LIKE 13ms**。フィルタ形 selection の全件解決は 27ms
  （ID 列挙なし）。すべて `total` とバッジ列込み

**理由**: 一覧は P0-8 の表が 6 万行を仮想スクロールで流す土台なので、ページ取得の遅さがそのまま
体感になる。キーセットにしたのは OFFSET が深いページで線形に遅くなるため（599 ページ目でも
1.2ms）。フィルタ形式を JSON にしたのはパーサを持たずにホワイトリストで SQL を閉じられ、
サイドバーの操作と 1 対 1 に対応するから。

**却下**: OFFSET ページング。`duplicate_groups` ビューの LEFT JOIN（実体化のコストが毎回かかる）。
token を DB に保存する案（短期状態に永続化は過剰。P0-9 の `edit_batches` が apply 後の正）。

## D-40 表 UI の固定値と境界

**決定**: 仕様（SPEC §12）が定めていない値と境界を次のとおり固定する。設定には出さない。

- **行の読み込みはカーソル順に積む。** `GET /api/tracks` はキーセットなので N ページ目を単独では
  取れない。1 ページ 500 件で先頭から順に取り、仮想スクロールが未読込の index を描くときにその
  index まで続けて取る（末尾へのジャンプは全ページを順に読む。6 万件 = 120 リクエストで 0.8 秒）。
  未読込の行は骨組みで描く。TanStack Table には**表示中の窓の行だけ**を渡す（6 万件の Row
  オブジェクトをページのたびに作り直さない）。仮想化の count は `total`
- **無効化（SSE `library` / `batch` / `resync`）は「表示に必要な件数まで先頭から取り直し、揃ってから
  差し替える」。** 1 ページずつ差し替えると途中で行数が縮んでスクロール位置が飛ぶ。表示範囲より
  後ろの行は捨て、スクロールで再び要求されたら読む。`library` の `ids` は読み込み済みの行に含まれる
  id があるときだけ、`bulk` は無条件。`batch` も取り直す（反映待ち / conflict バッジが変わる）。
  ソート・フィルタの変更は積んだ行を全部捨てて最初から
- **一覧は SSE を開いてから取る**（D-36）。`onopen` を初回取得のトリガにする。**再接続の `onopen`
  では表示範囲・ジョブ要約・アルバムを全部取り直す**（サーバの SSE は broadcast の購読で event id
  による再送が無く、切断中の job / batch / library は `resync` としても届かない）。SSE の `onerror`
  ではセッションを確かめる（`GET /api/auth/session`、5 秒に 1 回）。401 で閉じられた SSE は
  `apiFetch` の 401 境界を通らないので、これがログイン画面へ戻る経路になる
- **表の集合 = サイドバーの scope + 上部ナビの検索語 `q`。** ツリー（category / albumartist /
  album_id）とプレイリストは scope を**置き換え**、固定フィルタ（`flags`）は**トグルで AND**（ツリーの
  絞り込みと組み合わせられる）。検索語は 250ms 遅らせて反映する。どれを変えても同じ表コンポーネント
  の filter が変わるだけ
- **filter 形の選択を持ったまま表示フィルタを変えた後は、Ctrl / Shift による除外・解除を無視する**
  （素のクリックで ids 形に置き換えるのは常に効く）。表示中の行が選択集合に入っているかを client で
  判定できないので、除外 id を増やすと「total − 除外数」の件数が合わなくなる。Ctrl+A の時点で
  1 ページ目が届いておらず total が無かったときは、同じフィルタの total が届いた描画で一度だけ確定する
- **選択は immutable な値として表と別に持つ**（`lib/selection.ts`）。ids 形（クリック / Shift 範囲 /
  Ctrl トグル）と filter 形（Ctrl+A。選択した時点の filter 文字列 + 除外 id）。Shift の範囲は表示中の
  行順で解決し、anchor がソート・フィルタで表示から消えていれば素のクリック扱い。filter 形での Ctrl
  クリックは除外、Shift は除外の解除、素のクリックは ids 形に戻す。表示フィルタ・ソート・SSE の
  どれでも選択は変えない
- **filter 形の件数は「Ctrl+A 時点のサーバの total − 除外数」**、「うち反映待ち」は選択時のフィルタに
  `pending` を足して `GET /api/tracks?limit=1` の `total` で数える（除外した行の反映待ちは読み込み済み
  で判定できる分だけ差し引く）。集合は immutable でも中の行の pending はバッチの進行で変わるので、
  選択時に加えて **batch / library（ids の該当有無に関わらず）/ resync / SSE 再接続**のたびに数え直し、
  応答は世代で守る（`lib/pendingCount.ts`）。ハイライトは**表示フィルタが選択時のフィルタと同じときだけ**出す
  （違うときは表示中の行が集合に入っているかを client では判定できない。件数は右パネルで見せる）
- 列の表示・順・幅は TanStack Table の状態を localStorage（`spindle:columns.*`）に持つ。
  右パネルの折りたたみ・幅（220〜640px）・タブ、も同様。読めない・壊れている値は既定に倒す。
  `rel_path` 列は既定で非表示。`#` 列はトラック番号（2 枚目以降のディスクだけ `2-03` と前置）
- **反映待ちの行**（`pending_batch_id` あり）は `aria-disabled` + グレー背景、missing の行は
  打ち消し線。編集 UI 自体は P0-10 なので「編集不可」はまだ見え方だけ
- 401 はどの API でもログイン画面へ戻す（`GET /api/auth/session` で初回判定）。ログイン以外の画面
  （アルバム / CD / ジョブ / 履歴 / 設定）は骨格のプレースホルダ
- 純粋ロジック（filter の正規化、選択、ページローダ、バッジの条件、書式）は vitest で固定する
  （`cd web && npx vitest run`）。React コンポーネントの単体テストは持たない（ブラウザで確認する）
- **性能の参照値**（2026-09-16、TrueNAS ホスト AMD Ryzen 5 7600、headless Chromium via agent-browser、
  6 万件の合成行 + 実ファイル 9 件、release ビルドの同梱 SPA、1280×577）: 6 行/フレームの連続
  スクロール 5 秒で **60.2 fps、フレーム間隔 p95 16.7ms、20ms 超のフレーム 0**、30 行/フレームでも
  60.3 fps / 0 落ち。末尾へのジャンプ（120 ページ順読み）0.8 秒。ソート変更後・Ctrl+A・除外・
  表示フィルタ変更後に選択が維持されること、スキャンで行が増えたときに SSE でリロードなしに
  `total` が 60,009 → 60,010 になることを同じセッションで確認した。Chrome 安定版の DevTools
  Performance での再計測は実機の UI 確認時に行う

**理由**: 6 万行を表に出す土台なので、行データの持ち方（順に積む・窓だけ渡す）とスクロール位置を
保つ無効化が体感を決める。選択を表と別の immutable な値にしたのは、Ctrl+A の集合を ID で
持たない要件（SPEC §12.2）と「表示を変えても選択は変わらない」を同じ型で満たすため。

**却下**: react-query 等のデータ取得ライブラリ（キーセット + 順読みの都合を吸収できず、依存が
増えるだけ）。TanStack Table の rowSelection（ID 列挙前提で filter 形を表せない）。無効化で表示中の
ページだけをそのカーソルで取り直す案（行が増減するとページ境界がずれて重複・欠落する）。

---

## D-41 編集履歴の記録機構の固定値と境界

**決定**: 仕様（SPEC §7.5 / §8）が定めていない値と境界を次のとおり固定する。設定には出さない。

- **外部 rename の追随と ctime。** Linux の rename は対象 inode の ctime を進めるので、事前条件に
  `ctime_ns` を含めたままでは「rel_path のみ不一致（外部 rename）は tags op なら追随」（SPEC §7.5）が
  成立しない。**DB の `rel_path` が記録時点の `expected_rel_path` と違う（= スキャナが `(dev, inode)` で
  外部 rename を追随した）ときに限り、`ctime_ns` だけの不一致は rename によるものとみなして続行する。**
  dev / inode / size / mtime_ns / tag_hash のどれかも違えば conflict。rel_path が同じなら ctime の
  不一致は従来どおり conflict（`touch -r` を伴う in-place 更新の検出）。宛先 key の占有はスキャナが
  移動を記録する時点で解決済み（`rel_path_key` UNIQUE）なので、tagwrite 側では再判定しない
- **事前条件の不一致でも、ファイルの全フィールドが op の新値と一致すれば `applied` として確定する。**
  クラッシュ前に rename まで済んでいた op の確定（SPEC §7.5 リカバリ）を、起動時の特別処理ではなく
  通常の `apply_op` の経路で行う。再投入されたジョブが同じ判定を通るので、リカバリの手順は
  「pending の op に track ジョブを再投入する」だけになる
- **overlay の解消はファイルの現在値が原則、読めないときだけ記録値。** ファイルが消えている・
  壊れていて読めない op は `skipped_conflict` にし、`edits.old_value` と事前条件の物理属性
  （dev / inode / size / mtime_ns / ctime_ns / tag_hash）へ戻す。物理属性を記録時点へ戻すのは、その後
  ファイルが外部で変わっていれば次回スキャンが差分として拾い直せるようにするため（`rel_path` は
  戻さない。所在は常にスキャナが追随する）
- **キャンセルは書き込みロックの外でファイルを読む。** 1 つ目のトランザクションで子ジョブに
  cancel 要求を立て（queued は即 `cancelled`）、pending の op のうちジョブが `running` でないものを
  集める。その後 op ごとにファイルを開いて現在値を読み、`failed`（error = `cancelled`）に閉じて
  overlay を解消する。running のジョブはそのまま完了し、そのジョブが集計する。queued のジョブは
  1 つ目のトランザクションで `cancelled` になっているので、その後に claim されることはない
- **バッチの `cancelled` は「error = 'cancelled' の failed op が 1 件以上ある」で判定する。** cancel を
  要求しても全 op が既に進行中で完了した場合は `applied` / `partial` になる（キャンセルは何もしていない）
- **tagwrite ジョブはファイル操作を始める前だけ cancel を見る。** 始めた後は完了まで走り、`Done` を
  返す（進行中の op は完了を待つ。SPEC §7.5）。手を付ける前に cancel 要求があれば op を `failed`
  （`cancelled`）に閉じて `Cancelled`
- **最終試行の失敗では op を `failed` に閉じる。** バックオフの上限（`max_attempts`）に達する失敗を
  ハンドラが検知し、overlay を解消してから `Failed` を返す。op を pending のまま残すと再起動まで
  そのトラックを編集できず、再起動後は同じ失敗を繰り返す
- **起動時リカバリは `jobs::recovery::run` の後・ワーカー起動の前。** prepared / applying のバッチの
  pending op について、対応するジョブが `cancelled` なら op を閉じ、ジョブが無い・`done`・`failed` なら
  現在の `tag_version` で track ジョブを再投入する（dedup key `tagwrite:<track_id>:<tag_version>` で
  二重にはならない）。pending が無いのに開いているバッチは集計する
- **prepare は値が変わらないトラックを op にしない。** 全フィールドが現在値と同じトラックは
  `unchanged` に数えるだけで `tag_version` も動かさない。全件が unchanged ならバッチを作らず
  `NoChanges`。同じトラックが入力に 2 回あれば拒否する。値はキー大文字化・NFC 正規化してから
  比較・記録する（`edits.new_value` と反映後の読み戻しが同じ表記になる）
- **書いた内容は同じ FD から読み戻して `tag_hash` を確定し、編集キーが意図どおりでなければ反映
  しない。** lofty が書いた結果をスキャナと同じ関数で読み、編集した全キーが `edits.new_value` と
  一致することを確かめてから rename する。一致しなければ（その形式で表現できない値）tmp を捨てて
  op を `failed` に閉じる（再試行しても直らない）。hash と fstat（rename 後）を DB に書く。
  rename は ctime を進めるので rename の後に fstat する。rename 後に親ディレクトリを fsync する
- **rename の直前に宛先を開き直して stat を再確認する。** 事前条件の確認から rename までの窓
  （コピー + タグ書き込み。大きいファイルでは秒単位）で外部が書き換えていれば、tmp を捨てて
  `skipped_conflict`（overlay はその時点のファイルから解消）。dev / inode / size / mtime_ns /
  ctime_ns が確認時点の FD と同じなら内容も同じ（書けば ctime が進む）。再確認と rename の間の
  窓は排他ロックを持たない以上は残り（不変条件 5）、そこはスキャンが調停する
- **tmp には元ファイルの mode / 所有者 / xattr を写す。** mode（`fchmod`）は必須で、失敗したら
  反映しない。所有者（`fchown`）と xattr（`user.*` / `system.*` を含む全部。TrueNAS SCALE の
  NFSv4 ACL は xattr として見える）は best-effort で、権限や FS の都合で写せなければ警告して続行する
  （タグ編集のためにアクセス制御を変えない。それでも写せない属性は運用でディレクトリ継承に任せる）
- **外部の実体を採用する経路では音声属性とフィンガープリントも確定する。** 事前条件不一致で
  `applied`（全フィールドが新値）/ `skipped_conflict` になるとき、および overlay 解消・cancel で
  ファイルを読むときは、スキャナと同じ規則（`import::scanner::read_fingerprint` /
  `audio_changed`）で `audio_md5` / `audio_fp` を比べ、変わっていれば `audio_version` を進め、
  codec / 音声属性も置き換える。物理属性を新ファイルに揃えると次回スキャンには「変更なし」と
  見えるので、ここで確定しないと音声の差し替えを deep scan まで拾えない。自分の tagwrite の
  結果（音声不変が既知。SPEC §6）では計算しない
- **generic 形式（ID3 / APE 等）は全タグブロックに同じ変更を当てる。** 読み側は複数ブロック
  （ID3v2 + ID3v1 など）を **primary 優先で集約し、副ブロックからは primary に無いキーだけを
  補う**（連結すると同値が多値として二重になる。P0-6 の読み側もこの規則に揃えた）。
  書き側が primary だけ書くと副ブロックの旧値が残るので、全ブロックに当てる
- **generic Tag に写像できないキーは書かない。** MP4 / ID3 / APE 等では lofty の `ItemKey` に
  写像できるキー（Vorbis Comment 名）だけを書き、写像できないキーは警告して落とす（読み側も
  名前を持たない項目を落とす。P0-6）
- **`tracks.album`（albums の複製）と album メタは prepare では更新しない。** ALBUM タグの編集は
  `track_tags` とキャッシュ列 `title / artist_display / albumartist / track_no / disc_no / date` に
  反映され、album 側は tagwrite 後の次回スキャン（物理属性が変わるので dirty group になる）で
  再計算される。UI からの編集（P0-10）で即時性が要るなら、そこで album メタの再計算を足す

**理由**: 事前条件は「ユーザが見たものと同じファイルか」を保証するための道具であり、rename が
進める ctime を conflict にすると日常操作（フォルダ整理中の一括編集）が全部 conflict になる。
タグ内容は同じ FD の `tag_hash` で確認しているので、rename 追随時に ctime を緩めても書き込みの安全性は
落ちない。overlay の解消をファイル読みで行うのは「ファイルが正」を守るためで、記録値に戻すのは
読めないときの次善策にとどめる。キャンセルで 6 万ファイルを書き込みロックの中で読まない。

**却下**: 起動時に rename 済み op を専用の手順で確定する案（`apply_op` の経路と二重になり、
片方だけ直す事故が起きる）。キャンセルで op を記録値へ戻す案（pending 中の外部変更を DB が
見失い、物理属性が一致したままだと次回スキャンも拾わない）。pending のまま失敗し続ける op を
残す案（そのトラックが永久に 409 になる）。

---

## D-42 タグ一括編集の固定値と境界

**決定**: 仕様（SPEC §9 / §12.3）が定めていない値と境界を次のとおり固定する。設定には出さない。

- **操作の JSON 形**は SPEC §9 のとおり（`set` / `ref` / `replace` / `number` / `delete`）。キーは
  大文字化・値は NFC に正規化してから評価する。`PICTURE` は対象外（アートワークは P1-3）。
  操作は 1〜64 件。`set` の空（空文字・空配列）は削除と同義。`ref` の `%key%` は同じトラックの
  **先頭値**で展開し、無ければ空（結果が空なら削除）。`replace` は各値に全一致置換で `$1` が使え、
  空になった値は落ちる。正規表現は `fancy-regex`（先読み・後方参照可）でバックトラック上限
  100,000。`number` は `start + 位置`、`pad` 桁で 0 埋め（0〜6。範囲外は 400）
- **評価は preview と apply で同じ純粋関数**（`domain::tagops::apply_ops`）を、DB の `track_tags`
  に対して行う（ファイルは読まない。DB はファイルのキャッシュで、同じ `tag_version` なら同じ
  タグ集合）。apply は snapshot の各行に対して**再評価**し、現在値との差分だけを `edits` にする。
  preview 後に `tag_version` が進んだ行は評価せず `skipped_conflict` の op として記録だけする
  （`affected` に含む。DB も版も触らない）。**op の事前条件は preview 時の snapshot の値**
  （dev / inode / size / mtime_ns / ctime_ns / tag_hash / rel_path。D-33）で、apply 時の DB 値では
  ない。preview の後にタグはそのままで音声だけ差し替えられた（`tag_version` は据え置き）ファイルは
  tagwrite の事前条件確認が conflict にし、外部 rename の追随は D-41 の規則（rel_path 差 + ctime
  のみ）に乗る
- **preview は selection の解決・pending・各行のタグを 1 つの読み取りトランザクション**で読む
  （WAL のスナップショット）。途中でスキャンが commit しても、token の `tag_version` と表示した
  差分の世代が行ごとに混ざらない
- **連番の位置は「解決した selection を要求の `sort` で並べたときの位置」**で、反映待ちの行も
  位置を消費する（preview と apply で番号がずれない）。`sort` は一覧と同じ文字列で、省略時は
  既定ソート。preview の `items` は値が変わる行だけ返す（6 万件全部の差分を運ばない。変更なしは
  件数だけ）。反映待ちの行は評価せず `pending_excluded` に数える
- **apply は token を処理の間だけ占有する**（`SelectionStore::claim`）。同じ token の並行 apply は
  占有中なので `preview_stale`。409（`pending` / `preview_stale`（ops 不一致）/ `no_changes`）では
  `release` して同じ token でやり直せるようにし、201 で `finish`（消費）する。pending の事前確認と
  Editor の再確認（同じトランザクション）の間に別バッチが入っても、Editor の `Pending` を同じ
  409 に変換して token を残す。ops の照合は正規化後の canonical JSON の完全一致。token 不明・
  期限切れは `preview_stale`
- **`no_changes`**: snapshot 全行が変更なし（または反映待ちで除外）のときは 409。バッチは作らない
- **Derived の追随**は applied になった op のトランザクションで、`derived_files` の
  `src_tag_version` が現在の `tag_version` と違うときだけ `transcode` ジョブ
  （payload `{ track_id, audio_version, tag_version, kind: "retag" }`、dedup key
  `transcode:<track_id>:<audio_version>`）を投入する。P0 では `derived_files` が空なので何も
  起きない。ハンドラと `kind` の解釈は P1-10 が決める（`retag` = タグ上書きだけ、既定 = 再エンコード）
- **UI**: 操作リストは `useBatchEdit` が持ち、選択を変えても残る。プレビュー結果は
  「選択・操作・ソート」の組（`previewKey`）に紐づき、どれかが変わると古くなる（表の差分は消え、
  [適用] はプレビューし直すまで押せない）。差分は表の列 → タグキーの写像（Title / Artist /
  Album / AlbumArtist / Date / #）があるセルにだけ 旧→新 で重ね、選択集合にあって変更の無い行は
  薄く描く。`pending` の 409 は「M 件を除外して適用 / 待つ」で、この確認もプレビューの key に
  紐づく（選択・操作・ソートを変えたら消える）。`preview_stale` はプレビューのやり直しを促す。**インライン編集**はダブルクリックしたセルの列を `set` 1 件の操作にして
  同じ preview → apply を続けて呼ぶ（ユーザにはプレビューを見せない）。`#` 列は `2-03` 形式で
  DISCNUMBER + TRACKNUMBER、それ以外は TRACKNUMBER だけ。反映待ちの行はダブルクリックしても
  開かない。Artist 列の多値は `, ` で連結表示されているので、インライン編集では 1 値になる
  （多値は一括編集の `set` に配列で渡す。UI の入力は 1 値）
- **`tracks.album`（albums の複製）は編集で変わらない**（D-41）。ALBUM を編集した行の Album 列は
  次回スキャン後に追随する

**理由**: 評価をサーバの純粋関数に閉じることで、preview に見せた値と apply で書く値が一致し、
クライアントは表示だけを持てばよい。再評価にしたのは、preview 結果（数万行 × 変更）を token に
抱えて apply まで持ち歩くより、`tag_version` が同じなら同じ結果になる性質を使う方が単純で、
版が進んだ行の検出（conflict）も同じ比較で済むため。連番が反映待ちの行を飛ばさないのは、
preview で見た番号と apply の番号を一致させるため（飛ばすと「除外して適用」で全部ずれる）。

**却下**: クライアントで操作を評価して新値を送る案（正規表現の方言が JS と Rust で違い、
preview と apply の一致を保証できない）。preview の全行差分を返す案（6 万行で数十 MB）。
409 で token を消費する案（「除外して適用」のたびに preview からやり直しになる）。

---

## D-43 パス生成と一括リネームの固定値と境界

**決定**: 仕様（SPEC §5 / §7.5 / §8 / §9）が定めていない値と境界を次のとおり固定する。設定には
出さない（テンプレートは `[layout]` の 3 本だけ。リクエストでは変えられない）。

- **テンプレート**（`domain::pathgen::Template`）: プレースホルダは `category` / `albumartist` /
  `artist` / `album` / `title` / `disc` / `track` / `year` / `edition`。`{track:02}` のように `:0N`
  （N ≥ 1）で 0 埋め幅を指定できるのは整数フィールド（`disc` / `track`）だけ。`{track:2}` や
  `{title:02}` は書式エラー。未知の名前・閉じていない `{` とともに設定の読み込み時に落とす（fail-fast）。拡張子はテンプレートに含めない（形式はファイルが決める）。
  ytmusic の `<Cat>/<Artist>/<Album>/<track>. <title>` は `{category}/{albumartist}/{album}/{track}. {title}`
  で再現できる
- **テンプレートの選択**: album に category が無ければ `unsorted`、album が複数ディスク
  （`albums.disc_count > 1`、または active な構成トラックの `disc_no` の最大が 2 以上）なら
  `multi_disc`、それ以外は `single_disc`。`{category}` は `albums.category_id` の名前
- **値のフォールバック**: `albumartist` → `artist_display` → `Unknown Artist`、`album` →
  `Unknown Album`、`title` → 現在のファイル名（拡張子なし）、`track` → 0、`disc` → 1、
  `year` は album の `date`（無ければトラックの `date`）の先頭 4 桁が数字のときだけ
- **置換テーブル**は ytmusic の `FILENAME_REPLACE_TITLE` + `EXTRA` を全要素に適用する（ytmusic は
  albumartist に `:` と `*` だけ当てていたが、表を 1 本にする。SPEC §5 の表のとおり）。表に無い
  SMB / exFAT 禁止文字は同じ流儀で全角にする（`/`→`／` `\`→`＼` `|`→`｜` `"`→`＂`）。
  制御文字は落とす。末尾のドット・スペースは削る。Windows の予約名は `_` を後置する
  （`CON` → `CON_`、`con.txt` → `con_.txt`）。空になった要素は `_`。タグ値そのものは変えない
- **切り詰め**: 各要素 255 バイト、パス全体 240 UTF-16 単位。要素はその要素の末尾を、全体は
  ファイル名の stem を `…`（U+2026）付きで切る。拡張子は保つ。省略記号の前の末尾スペース・
  ドットは削る。切り詰めた結果に元の文字が 1 つも残らない（ディレクトリだけで上限を超える、拡張子が
  長すぎる、先頭が多バイト / サロゲートで予算に入らない）ときは生成エラー（計画では conflict）に
  する。上限を満たさないパスは返さない
- **衝突降格の単位はリリース**: `mb:<MUSICBRAINZ_ALBUMID>` → `disc:<DISCID>` → `album:<album_id>` の
  順で決めるキーが同じなら同一リリース。同じ宛先ディレクトリに 2 つ以上のリリースが来たら、
  そのディレクトリに**既にいる**リリース（選択外の占有者、または選択内で現在そこにいるもの）が
  1 つだけでそれと同じなら合流（降格なし）、それ以外は `{album} ({year})` → `{album} ({edition})` に
  降格する。年 / edition が無い、または降格しても衝突するものは `conflict`（マージにはしない。D-7）
- **ファイル名の衝突**は `rel_path_key` で判定する（選択内の重複、選択外の active 行の占有）。
  選択内の現在 key は占有とみなさない（swap / 循環が通る）。missing 行が宛先 key を持っていれば
  prepare で `\0vacated:<id>` へ明け渡させ（スキャナと同じ規則）、ファイルが残っていれば
  `RENAME_NOREPLACE` が最終判定になる。自分自身の大小文字だけの変更は許可する
- **バッチ 1 つに `rename` ジョブ 1 つ**（payload `{ batch_id }`、dedup key
  `rename:batch:<batch_id>`、並列 1）。SPEC §8 の「track 単位」を rename では採らない: 2 phase は
  バッチ全体の順序を要し、track ジョブに分けると phase の境界を DB で管理する必要が出る。
  ジョブは開始時にバッチの全トラックをロックする（取れなければ再キュー）
- **DB の overlay は prepare の時点**で `rel_path` を新値にする（2 段階更新。D-24 と同じ
  「DB 先行更新 + pending」）。`expected_rel_path` が記録時点の物理パス
- **一時名は `spindle-rename-<op_id>.<ext>`**（source と同じディレクトリ、隠しファイルにしない）。
  `.spindle-tmp-*` はスキャナが 1 時間で回収するので使えず、隠すとスキャナが対象外にして
  トラックを missing にする。スキャナは pending の rename op があるトラックの所在が
  source / 一時名 / 最終名（DB の `rel_path`）のいずれかなら衝突にしない（`edit::in_progress_keys`）。
  どれでもなければ op を `skipped_conflict` にし、**同じトランザクションで `rel_path` を実在パスへ
  追随させる**（rename ジョブは pending の op しか見ないので、overlay の最終名を残すと次回スキャンまで
  DB が実在と食い違う）
- **phase 1 の事前条件は stat だけ**（dev / inode / size / mtime_ns / ctime_ns）。タグは読まない
  （内容は変えないので `tag_hash` の再計算は不要。in-place のタグ書き換えは ctime で見える）。
  preview 時の `tag_hash` が apply 時の DB 値と違う行は prepare で `skipped_conflict` にする
  （計画の前提が崩れている。D-33）
- **op の終端は commit で一括**（1 トランザクション）。各 op の所在（最終名 / 一時名 / source /
  不明）をファイルから決め、`rel_path` を所在へ揃え（2 段階更新）、物理属性を追随する。所在が
  不明（ファイルが無い。一時名が外部に消された等）なら記録時点の `expected_rel_path` と物理属性へ
  戻す（次回スキャンが inode で追随するか missing にする）。ただしその key を別の行（swap の相手の
  実在パスを含む）が持っていれば戻せないので `\0vacated:<id>` のままにする（所在が分かっている行が
  正。UNIQUE を踏んで commit が永久に失敗する経路を作らない）。順序は **所在不明の行を先に予約 key へ
  退避 → 所在の分かった行を実在パスへ（バッチ外の占有者は明け渡し）→ 所在不明の行を空いていれば
  記録時点のパスへ**（所在不明の行の overlay が、相手の戻り先と重なることがある）。phase 2 で宛先が取られていた op は
  source へ戻して `skipped_conflict`、戻せなければ一時名のまま `skipped_conflict`（DB の `rel_path`
  は一時名。警告を出す）。rename の直後は毎回親ディレクトリを fsync する（phase 1 / 巻き戻し /
  phase 2 のどこで電源断しても rename が永続化されている）
- **cancel は phase 2 に入るまで**: phase 1 の各 op の前と phase 1 完了直後に確認し、退避済みを
  source へ戻して全 op を `failed('cancelled')` に閉じる。phase 2 に入ったら最後まで進める
  （一部だけ戻すと swap / 循環が解けない）。ジョブが走っていないバッチの cancel / リカバリ /
  最終失敗は `close_rename_ops` がバッチの pending を一度に閉じる（rename op はパスが互いに
  絡むので op 単位に閉じない）
- **クラッシュ復旧は所在の再判定**: 再投入されたジョブが op ごとに最終名 → 一時名 → source の
  順に inode で所在を判定し、続きを行う。起動時の特別処理は無い（D-41 と同じ）
- **album は coordinator が追随する**（「ディレクトリ = album」を DB でも保つ）: overlay と
  overlay 解消の両方で、宛先ディレクトリに album 行があれば合流（missing なら復活）、無ければ
  ある album の active な構成トラック全部が同じ宛先へ動くとき（album 全体の移動）は `rel_dir` を
  書き換えて id を維持（D-32）、それ以外は新規 album（メタは構成トラックのキャッシュ列の最頻値、
  category は先頭ディレクトリ名が語彙に一致すればそれ、無ければ旧 album から引き継ぐ）。宛先の
  album 行が missing でこちらが album 全体の移動なら、その行を `\0displaced:<id>` へ退かせて
  id を維持する。構成 0 になった旧 album は `missing_since`。スキャナは変更のないディレクトリの
  照合を省く（D-38）ので、ここで揃えないと album が古いまま残る
- **同梱ファイル（cover.jpg / disc.cue / rip.log 等）は動かさない。** rename op はトラック 1 本の
  パスだけを所有し、巻き戻しの対象もそれだけ。album 全体を動かした後の旧ディレクトリに残る
  同梱ファイルの扱い（追随・GC）は未決（残課題）
- **API**（SPEC §9）: preview は選択を固定して token を返し、行ごとに `old` / `new`（衝突なら
  null と `reason`）。apply は token の集合で計画を取り直し、`affected` / `conflict` を返す。
  409 の規則は tags と同じ（`pending` / `preview_stale` / `no_changes`）

**理由**: 2 phase をバッチ単位のジョブにすると、phase の境界がプロセス内の制御フローだけで済み、
クラッシュ復旧は「所在をファイルから判定する」1 本になる。一時名を可視にするのは、スキャンが
リネームと並走する前提（不変条件 5）で pending 中のトラックを missing にしないため。album を
coordinator で追随させるのは、リネーム直後の一覧（category / albumartist / album のツリー）が
次回 deep scan まで古いままになるのを避けるため。

**却下**: track 単位の rename ジョブ（phase 境界の管理が DB に要る）。`.spindle-tmp-*` を一時名に
使う案（スキャナが回収する）。phase 2 の途中で cancel を効かせる案（swap の片側だけ戻せない）。
同名の album をマージする案（D-7）。

---

## D-44 巻き戻しの固定値と境界

**決定**: 仕様（SPEC §7.5「巻き戻し」/ §9 / §12.4）が定めていない値と境界を次のとおり固定する。

- **逆バッチは通常のバッチ**で、`reverts_batch_id` だけが違う。tags は `prepare_tags_tx`、rename は
  `prepare_rename_tx` に乗る（DB 先行更新 + tagwrite / rename ジョブ、事前条件は記録時点の DB 値、
  pending の 409、キャンセル、リカバリはすべて同じ経路）。`delete` は `missing_since` を戻すだけの
  DB 操作なので、同じトランザクションで op を applied にして即終端にする（構成トラックが戻った album の
  `missing_since` も外す）。`archive`（ロスレス正規化）は `prepare_normalize_tx` に乗る（D-46）
- **対象集合はトラック単位**で引く: 元バッチの `applied` op − 逆バッチ群（`reverts_batch_id` = 元）で
  `applied` になった op の `track_id`。1 バッチ 1 トラック 1 op なので op と track は 1:1
- **現在値の比較は DB の値**（`track_tags` / `rel_path` / `missing_since`）で行う。DB はファイルの
  キャッシュで、スキャン済みの外部変更はここで conflict になり、未スキャンの外部変更は tagwrite /
  rename の事前条件確認（tag_hash / stat）が conflict にする。どちらも op 単位の `skipped_conflict`
- **計画時点の conflict にも edits を残す**（戻そうとした変更 = 現在値 → 元の値）。履歴画面が
  「何を戻そうとして、現在値が何だったか」を出せる。preview 後に版が進んだ行（tags の
  `expected_tag_version` 不一致）は評価しないので edits は空
- **`reverted_at` は `aggregate_batch` が立てる**: 逆バッチが終端になった時点で、元バッチの applied op が
  逆バッチ群で全件 applied になっていれば元バッチに `reverted_at`。partial なら立てず、再 revert は
  残りだけが対象。やり直し（redo）= 逆バッチの revert で、同じ規則で逆バッチに `reverted_at` が立つ。
  元バッチの `reverted_at` は redo でも消さない（「#39 で戻し済み」の事実は変わらない。さらに戻すなら
  redo バッチを revert する）
- **一覧の `reverted_by`** は `reverted_at` が立っているときだけ、applied op を持つ逆バッチの最大 id。
  一覧は新しい順で 1000 件まで（履歴はユーザ操作 1 回 = 1 行）
- **`description` は省略可**で、無ければ UI が「(#N の巻き戻し)」と表示する（DB には入れない）
- **UI**（`HistoryView`）: 一覧は画面を開いたときに取り、開いている間は SSE `batch` で取り直す
  （250ms で間引く）。開いている行の詳細も一緒に取り直す。[巻き戻す] は終端かつ applied の op があり
  `reverted_at` が無いときだけ。逆バッチの行は「巻き戻す (=やり直し)」。[キャンセル] は
  prepared / applying のみ。conflict の op は「ファイルを再読込した現在値」として `current` を出す

**理由**: 巻き戻しを既存のバッチ機構にそのまま乗せることで、pending / conflict / cancel / リカバリの
規則が 1 本になる。対象集合をトラック単位で引くのは、部分的に戻った後の再 revert（残りだけ）と
redo を同じ式で扱うため。

**却下**: 逆バッチを専用の状態機械にする案（同じ規則を二重に持つ）。元バッチの `reverted_at` を redo で
消す案（履歴の事実が書き換わり「#N で戻し済み」の表示が揺れる）。

---

## D-45 移行の振り分けと Library のロスレス形式（実機合わせ）

**決定**:

- **実機**: プールは `tank` ではなく `ssd`（NVMe、928G）と `hdd`（43.6T）。旧ライブラリは
  `ssd/musics`（342G、既に `insensitive` + `formD` + `utf8only=on`）で、最上位は `Opus/`
  `Original/` `AAC/`。`Playlists/` は `Opus/Playlists` と `Original/Playlists` の中にある
- **`Original/` は「生データ」ではなく大半がロスレス master**。内訳は ALAC 7,572（うち 24/96 が 4）、
  webm 1,512（YouTube 生データ）、mp3 14、AIFF 2（同名の ALAC あり）。`Opus/` はその 1:1 の
  非可逆版。`AAC/` は iTunes 用に FDK-AAC 256k + ReplayGain 適用済みで音声を書き換えた配布物
- **振り分け**（`scripts/migrate_plan.py` が一覧を生成し、`docs/MIGRATION.md` の rsync が使う）:

  | Original の原本 | Library（master） | Derived | Archive | 捨てる |
  |---|---|---|---|---|
  | ALAC | `.m4a` そのまま | 再生成（旧 `.opus` は移さない） | — | — |
  | webm | 対応する `Opus/*.opus`（remux） | — | `.webm` | — |
  | mp3 | `.mp3`（非可逆原本） | — | — | 対応する `.opus`（非可逆→非可逆） |
  | AIFF + 同名 ALAC | ALAC（整数 PCM） | — | `.aiff` | — |
  | `AAC/`、`.exclude` / `.noimage`、`.fpl` / `gen.sh` | — | — | — | 移さない |

  Library の音声は 9,098 本。Hi-Res の 24/96（`Album/Hi-Res/01.m4a`）とアルバム直下の 16/44 は
  `audio_md5` が違う別トラックとして両方 Library に入れる（整理は表 UI で後から）。
  プレイリストは `Opus/Playlists/*.m3u8`（28 本）だけを `Playlists/m3u8/` へ
- **Library のロスレスは FLAC に統一する。** 移行時は ALAC のまま取り込み、変換は spindle の
  ジョブで行う: P1-4 の WAV → FLAC 正規化を「ロスレス → FLAC 正規化」（WAV / ALAC / AIFF）に
  広げ、PCM MD5 照合 → 一致時のみ元ファイルを `Archive/` へ退避 → 台帳 → 30 日後 GC の同じ機構に
  乗せる。`audio_md5` は同じ PCM なら形式に依らず一致する（D-37）ので同一性は保たれ、
  `audio_version` は上げない（Derived の再生成は起きない）
- **Apple 向け（ALAC / AAC）は Derived に置かず、将来のエクスポート・同期プロファイルで
  オンザフライ変換する。** D-9（Derived は Opus のみ）は変えない
- **データセットの置き場**: `ssd/media/{Library,Derived,Inbox}` と `ssd/apps/spindle` は SSD、
  **`Archive` は `hdd/media/Archive`**（`/mnt/hdd/media/Archive`）
- **DSD**（現状 0 本）: 買ったら原本 `.dsf` / `.dff` は Archive、Library には PCM 変換した FLAC
  （24/176.4 または 24/88.2）を master として置く。DSD → PCM は不可逆なので Archive の原本は
  消さない。FLAC は PCM 専用で DSD を格納できない

**理由**:

- SPEC の master の定義は「手元にある最高品質 = Library の実体」。`Original/` を丸ごと Archive に
  送る旧手順では 7,572 曲のロスレスが再生も編集もできない場所に埋もれ、非可逆の Opus が master に
  なる。D-1 の役割別構成をそのまま適用すれば、ロスレスが Library、生データ（webm）が Archive、
  Opus は Derived になる
- FLAC を選ぶのは、ブラウザ再生が全種で直送できる（ALAC は Safari 以外で毎回 Opus 変換、§11）、
  Vorbis comment で複数値タグが素直（旧 `AAC/Readme.md` の「`; ` を `、` に置換」は MP4 ilst の
  制約の回避策だった）、STREAMINFO の MD5 で `flac -t` の健全性検査ができる（P1-5）、CD リップ
  （P2）と WAV 正規化（P1-4）も FLAC なので Library のロスレスが 1 形式になる、から
- 移行前に ffmpeg で一括変換しないのは、ilst → Vorbis comment のタグ移し替え（複数値・ディスク
  番号・コンピレーション・アートワーク）が不完全で 7,572 曲の検品ができないこと、変換後は
  旧ツリーとの rsync checksum 照合が効かなくなること。ジョブなら lofty でタグを移し、失敗すれば
  戻せる
- Archive を hdd に置くのは、FLAC 化のとき 220G の ALAC が Archive へ退避され、SSD だけだと
  一時的に 520G を超えて空き（435G）に収まらないため。Archive は追記のみで速度が要らない
- 旧 `Opus/` を Derived に流用しないのは、旧パスがテンプレート（`01 Title` 形式）と一致せず、
  `derived_files` に紐づかない孤児として GC 対象になるだけだから。再生成は CPU 時間だけで済む

**却下**: ALAC のまま Library に残す（Safari 以外の再生が全件変換、複数値タグの制約が続く）。
`AAC/` を Archive に置く（音声を書き換えた非可逆で原本性が無い）。`Original/Playlists` の fpl を
移す（foobar 専用。P1-6 の取り込みでパスは書き換えるので m3u8 だけで足りる）。

## D-46 ロスレス正規化の固定値と境界

**決定**: 仕様（SPEC §7.4 / §8 / §9、D-10 / D-45）が定めていない値と境界を次のとおり固定する。

- **起動は selection API**（`POST /api/normalize/preview` → `/apply`。rename と同型で
  `selection_token` を使う）。スキャンは WAV / ALAC / AIFF を見つけても自動投入しない。移行で
  取り込んだ ALAC 7,572 本（220G）の変換は数時間の CPU と Archive への実コピーを伴うので、
  いつ・どの範囲を変換するかはユーザが決める。Inbox 取り込み（P2-10）の後続ジョブとしての
  自動投入はそのときに足す。`[normalize].wav_to_flac = false` なら API は 409 `normalize_disabled`
- **編集バッチの機構に乗せる**: `edit_batches` + `edit_ops(kind='archive')` + `edits`
  （`rel_path` 旧→新、`codec` 旧→新）。宛先は拡張子を `.flac` に置き換えた同じパス。
  ジョブは track 単位の `normalize`（dedup `normalize:<track_id>:<op_id>`、並列 2。op ごとに一意に
  しないと、巻き戻し直後の新しい op がまだ `running` の前のジョブに相乗りして実行されない）で、
  ハンドラが自分で
  `track_locks` を取る。pending の op があるトラックは再編集できない（409）ので、変換中にタグ編集や
  リネームが割り込むことはない
- **DB は先行更新しない**（tags / rename の overlay と違う）。変換が終わるまで表は元ファイルの実体
  （ALAC / WAV）を示す。エンコードが失敗すれば何も変わらない。コーデックを先に FLAC と見せると
  失敗時に戻す overlay 解消が要り、表示も嘘になる
- **照合は独立した 2 つのデコーダ**: 元の PCM MD5 は symphonia（`decoded_pcm_md5`、スキャンと同じ
  計算）、エンコード側は ffmpeg でデコードした WAV を `flac -8 --verify` に通した STREAMINFO の
  MD5。両者が一致するときだけ置く。どちらかが元ファイルを読み違えれば不一致になり、op は
  `failed`（元ファイルは無傷、生成物は捨てる）。ALAC を ffmpeg に渡すときは `/dev/stdin` に
  root から開いた FD を繋ぐ（MP4 は moov が末尾にあると pipe では読めない。パス文字列は渡さない）
- **タグは lofty で写す**: 読み側が Vorbis 名へ揃えた項目と画像の実体を、生成した FLAC の
  VorbisComments と PICTURE ブロックにそのまま書く。写した結果の `tag_hash` が元と違うときだけ
  `tag_version` を進める（MP4 ilst で表現していたキーが落ちた等。同値なら進めない）。
  `audio_md5` は同じ PCM なので変わらず、`audio_version` も据え置く
- **破壊フェーズは 3 段で、unlink の直前まで元ファイルを照合し続ける**（排他ロックは使わない。
  不変条件 5）。(1) 宛先の FLAC を置き、元ファイルを同じディレクトリの一時名
  `spindle-normalize-<op_id>.<ext>` へ `RENAME_NOREPLACE` で退避する。rename した実体の inode が
  自分の FD と違えば（stat と rename の間に差し替えられた）戻して conflict。以後に元パスへ現れる
  外部のファイル（tmp + rename）は自分の inode と分離され、触らない。(2) Archive へ実コピー
  （別プールなので rename できない）。コピーしながら取った SHA-256 が最初の値と違えば（同じ inode
  への in-place 更新）conflict、書いた tmp を読み戻して一致しなければ I/O 失敗として再試行。
  rename の前に **元ファイルのコンテナ全体の SHA-256 を `edits(key='source_sha256')` に記録**する。
  (3) unlink の直前に同じ FD の stat とバイト列、一時名にあるのがその FD の実体（dev / inode）
  であること、宛先にあるのが期待した音声（MD5）であることをもう一度照合し、外れていれば
  conflict。**unlink が不可逆な確定点**で、その後の失敗（dir の fsync 等）では宛先も Archive も
  消さず再試行に回す（反映済み経路で確定する）。
  確定点では同じ blocking 関数の中で一時名の実体（dev / inode）を再確認してから unlink する。
  conflict では元を元パスへ戻し、自分が置いた宛先と Archive の未確定コピーを消す（ユーザデータは
  元の inode に無傷で残っている）。消すのは**今そのパスにあるのが自分の置いた実体で、置いてから
  変わっていない（置いた直後の stat と dev / inode / size / mtime / ctime が一致し、内容の SHA-256 も
  書いたときと一致する。カーネルの時刻は粗い粒度なので stat だけでは同じ tick の変更を見落とす）
  ときだけ**で、
  外部が差し替え・上書きしていれば触らない（パスは識別子ではない）。消すときも unlink をパスに
  対して直接は行わず、同じディレクトリの隔離名（`.spindle-undo-…`。隠しファイルだが
  `.spindle-tmp-` ではないので、スキャナの取り残し回収の対象に**ならない**）へ原子的に rename して
  パスから切り離し、外した実体（dev / inode・size / mtime・内容）を照合する。一致した自分の生成物
  だけを `.spindle-tmp-` の名前へ移してから unlink する（消す前に落ちても 1 時間後に回収される）。
  照合に外れれば元の場所へ戻す（塞がっていれば回収パス。どちらも失敗すれば隔離名のまま残り、
  自動では消えない）。一時名の実体が差し替えられて
  いれば、開いている FD の内容を元パスへ、元パスも塞がっていれば同じディレクトリの回収パス
  `spindle-recovery-<op_id>-<random>.<ext>` へ書き出す（所在は op の error に残る）。元をどこにも
  残せなかったときは、同じ音声を持つ自分の生成物（宛先の FLAC・Archive のコピー）を 1 つも消さず、
  実在する複製だけを op の error に記す。I/O 失敗（再試行）では生成物
  だけ消し、元は一時名に残す（戻す rename は ctime を進めて次の試行の事前条件が外れるため。
  一時名にある元は「rename による ctime の差」として許容する）。op を閉じるとき（最終失敗・
  キャンセル）に、一時名にあるのが記録した inode なら元パスへ戻す。
  台帳 `archived_files` は `rel_path`（Archive 相対 = 元の Library 相対）で UNIQUE、
  `eligible_after = now + [gc].retention_days`
- **巻き戻しは同じ op 種別で向きを逆にする**（`rel_path` 新→旧、`codec` 新→旧。D-44 の
  「未実装」を解消）。元ファイルは Archive から Library へ**コピー**で戻し、Archive の実体は消さない
  （Archive は追記のみ。台帳は `restored`）。Library にあった FLAC は Archive へ move し、台帳に
  `reason='restore'` の行を足す（GC の対象。マイグレーション 0003 で CHECK を広げた）。
  やり直し（逆バッチの revert）は WAV を再び Archive へ move するが、同じパスの行が既にあるので
  作り直さず `held` に戻して期限を更新する。復元元が GC 済み（`deleted`）や台帳に無い op は
  `skipped_conflict`
- **冪等性の判定はファイルの実体**: 再投入されたジョブは Library/<旧>（無ければ一時名）の有無、
  Library/<新> の音声 MD5（期待値と一致するときだけ自分の成果物）、Archive/<旧> のバイト列（元と
  一致するときだけ退避済み）から続きを行う。<旧> も一時名も無く、<新> の音声 MD5 が DB と一致し、
  Archive/<旧> の SHA-256 が記録した `source_sha256` と一致するときだけ「反映済み」として DB を
  確定する。記録が無い・一致しないときは何も消さず `skipped_conflict`（宛先も Archive の実体も
  残す）。宛先に別のファイルがあれば `skipped_conflict`、Archive の同じパスに別のファイルがあれば
  **Library を触る前に** `failed`。置いた宛先の読み直しに失敗したときは terminal にせず再試行
  （反映済み経路で確定する）
- **スキャナは作業中のパスを避ける**: pending の archive op の元（`expected_rel_path`）・一時名・
  宛先（`edits.rel_path` の新値）の key にあるエントリは新規登録も移動も missing 判定もせず、
  seen だけにする。DB を先行更新しないので、ジョブが宛先を置いてから DB を確定するまでの窓で
  スキャンが宛先を「新規トラック」に登録すると、確定時に `rel_path` の UNIQUE を踏む。
  inventory を取った後・commit の前にジョブが DB を確定した（op はもう pending でない）場合は、
  `Identity::New` の挿入直前に `rel_path_key` の占有を読み直し、占有されていれば挿入せず
  seen だけにする（走査全体を UNIQUE 違反で失敗させない）
- **cancel は Library を触る前まで**（変換中の ffmpeg / flac は子プロセスごと止め、tmp を消す）。
  配置を始めたら最後まで進める。外部の変更は事前条件（stat + tag_hash）で `skipped_conflict`、
  変換の間に元ファイルが更新されていれば配置の直前に同じ FD の fstat で検出する
- **UI は未着手**（rename と同じ。curl で起動できる）。設定画面の「退避ファイルの一覧と復元」
  （SPEC §12.6）は台帳 API と合わせて後で載せる

**理由**: 破壊的操作（Library からファイルを外す）は必ずバッチとして巻き戻せる必要があり
（不変条件 4）、既存の編集バッチの機構に乗せれば pending / conflict / cancel / リカバリ / 履歴 /
巻き戻しの規則が 1 本になる。DB を先行更新しないのは、変換の成否がファイルを読むまで分からず、
失敗が普通に起きる（MD5 不一致、ビット深度非対応）ため。2 つのデコーダで照合するのは、
「可逆変換だから情報は失われない」を実装が検査できる形にするため。

**却下**: スキャンで自動投入（220G の変換が起動直後に勝手に走る）。ffmpeg だけで FLAC まで作る
（`flac -8` の参照エンコーダを外すうえ、照合が単一デコーダの自己一致になる）。元ファイルを
Archive から move で戻す（Archive の追記のみの原則に例外が増える）。DB を先行更新して codec=flac
を先に見せる（失敗時の overlay 解消が要る）。

---

## D-47 ReplayGain 解析の起動と固定値（実装合わせ）

**決定**（P1-1）:

- **起動は `POST /api/rg { selection }`**（selection は rename / normalize と同じ `ids` / `filter`
  の 2 形）。含まれる active なトラックの `album_id` ごとに `rg` ジョブ（dedup `rg:album:<id>`）を
  投入し、`album_id` を持たないトラックは track 単位（`rg:track:<id>`）。既に queued / running の
  album は `duplicates` に数えて投入しない。preview 段階は無い（DB にしか書かない）。全曲は
  `filter = {"flags":["no_rg"]}` で指定する。UI の起動ボタンは未着手（curl で起動できる）
- **album の集計はデコード結果が 2ch のトラックだけ**（SPEC §6 / D-22 のとおり mono も除外）。
  判定は DB の `channels` 列ではなく実際にデコードしたチャンネル数（ファイルが正）。集計に
  入らないトラックは track の値だけ持ち、`rg_album_gain` / `rg_album_peak` は NULL。track 単位の
  ジョブも album の値を持たない
- **開いた FD を DB の行と照合してから解析する**: root から開いた FD の fstat（dev / inode /
  size / mtime / ctime）が行と一致しなければ失敗（パスは識別子ではない。同名で差し替えられて
  いれば別トラックの音声をこの行の値として保存してしまう）。デコード後にも同じ FD を fstat し直し、
  解析中の in-place 更新を検出する。外部のタグ書き換えでも size / mtime は動くので、その場合は
  再スキャンで行が更新された後の再試行で通る
- **album は all-or-nothing**: 構成トラックが 1 本でもデコードできなければジョブを失敗にし、
  何も書かない。一部だけ書くと album gain が揃わないまま次の再解析まで残る。書き込みの
  トランザクション内で構成（active な構成トラックの id と stat・rel_path）を読み直し、解析した
  行の集合と違えば（scanner が missing にした・別 album へ移した・新しいトラックが加わった・
  同じ行を別実体へ追随させた）何も書かずに失敗する。
  失敗したジョブは `last_error` に該当パスを持ち、`/api/jobs/:id/retry` で再投入できる
- **無音（絶対ゲート -70 LUFS 以下）は gain 0 dB**（補正なし）。積分ラウドネスが `-inf` になる
  ので `reference - lufs` が定義できない。peak は測定値のまま
- **デコードは symphonia を優先し、持たない形式だけ ffmpeg**（Opus は demux まで symphonia、
  PCM は ffmpeg の `f32le` を stdout のチャンクで受ける。WavPack / APE は lofty の属性 + ffmpeg）。
  ffmpeg の stdout はメモリに溜めない（`ExternalCommand::stdout_channel`。受け手が消えたら子を
  グループごと止める。SIGPIPE を無視する子でもタイムアウトまで待たない）
- **`rg_scanned_at` だけ更新**し、`audio_version` / `tag_version` は動かさない（`rg_written_at`
  は値が変われば NULL。D-48）。走査中に missing になった行は書かない

**理由**: rg ジョブは SPEC §8 で album_id 単位・CPU コア数並列と決まっているが、起動の入口が
§9 に無かった。selection で受ければ UI の選択・フィルタとそのまま繋がり、移行直後の「全曲」も
`no_rg` フラグで表現できる。2ch 判定をデコード結果で行うのは、`channels` 列が lofty 由来の
キャッシュであり、不変条件 1 に従うと解析時に読んだ実体を採用すべきため。

**却下**: scan 完了時の自動投入（移行直後に 9,098 曲の解析が勝手に始まる。normalize と同じ理由）。
失敗トラックを飛ばして album を書く（嘘の album gain が残る）。ffmpeg で全部デコードする
（外部プロセス起動と f32 のパイプが 9,000 回。symphonia で済む形式は済ませる）。

**未決**: 外部で音声が差し替わって `audio_version` が上がったとき `rg_scanned_at` を NULL に
戻すか（現状は残る。scanner に足すなら別タスク）。

## D-48 ReplayGain のタグ書き込みは通常の編集バッチに乗せる（実装合わせ）

**決定**（P1-2）:

- **`POST /api/rg/write { selection, description?, skip_pending? }`** で、selection の解析済み
  トラックについて DB の `rg_*` を形式ごとのタグに変換した **tags op の編集バッチ**を記録する
  （`Editor::prepare_rg_write`）。旧値の記録・DB 先行更新（overlay）・track 単位の tagwrite・
  事前条件・巻き戻し・キャンセル・リカバリはタグ編集と共通で、専用のジョブ種別も op 種別も持たない。
  preview 段階は無い（値は DB から一意に決まり、ユーザが選ぶものが無い）。反映待ちがあれば 409
  `pending`（`skip_pending` で除外）、書く行も一致済みの行も無ければ 409 `no_changes`、
  `[replaygain].write_tags = false` なら 409 `rg_write_disabled`
- **書くキーは形式ごとに固定し、値の無いキーは消す。** Opus は `R128_TRACK_GAIN` /
  `R128_ALBUM_GAIN`（Q7.8、-23 LUFS 基準。SPEC §6 の式）だけを書き、`REPLAYGAIN_*` 4 キーは
  消す（RFC 7845 §5.2.1 は Opus に `REPLAYGAIN_*` を使わないとしている。両方あると基準の違う
  値をプレイヤーが拾う）。他形式は `REPLAYGAIN_TRACK_GAIN`（`+0.00 dB`、小数 2 桁、-18 LUFS
  基準）/ `REPLAYGAIN_TRACK_PEAK`（線形の true peak、小数 6 桁。1.0 を超えうる）/
  `REPLAYGAIN_ALBUM_GAIN` / `REPLAYGAIN_ALBUM_PEAK` を書き、`R128_*` は触らない（Opus 専用で
  generic Tag に写像できない）。album の値を持たないトラック（album 無し・2ch 以外）は album
  のキーを消す。MP4 / MP3 / APE 等へは lofty の写像（`----:com.apple.iTunes:replaygain_*`、
  `TXXX:REPLAYGAIN_*`）で書く。`OpusHead` の output gain は触らない（SPEC §6）
- **`rg_written_at` はフラグではなく「ファイルの RG タグが解析値と一致していると確認した時刻」。**
  書き込み op の applied に限らず、DB をファイルの現在値へ揃える経路（`sync_track_to_file`:
  applied の追随、overlay の解消、外部変更の採用）のすべてで `file_matches` を判定し直し、
  一致すれば `now`、一致しなければ NULL にする。DB のタグが既に変換結果と一致している行
  （再解析で同じ値になった、前回の書き込みが済んでいる）は op にせず `rg_written_at` だけ立てる
  （DB のタグはファイルのキャッシュなので、pending が無ければファイルも一致している）。
  巻き戻しやユーザの手編集で RG のキーが解析値と違う値になれば自動的に NULL に戻る。
  **再解析（`db::replaygain::store`）で値が 1 つでも変わった行も NULL にする**（時刻が秒単位
  なので、確認と再解析が同じ秒に起きると `rg_written_at < rg_scanned_at` では検出できない）。
  値が全て同じで確認が有効なら確認時刻を `now` へ進める（同じ値の再解析で未書き込みに落とさない）
- 一覧のフィルタに **`rg_unwritten`**（`rg_scanned_at IS NOT NULL AND (rg_written_at IS NULL OR
  rg_written_at < rg_scanned_at)`）を足す。移行直後の「解析済み全曲を書く」は
  `filter = {"flags":["rg_unwritten"]}` で表現する。UI の起動ボタンは未着手

**理由**: 不変条件 4（タグ書き込みは旧値を記録してから）と 3（タグだけの変更で `audio_version` を
上げない）は編集バッチがそのまま満たす。専用ジョブにすると事前条件・overlay・巻き戻しを二重に
持つことになる。`rg_written_at` を op の属性（書き込みバッチかどうかのフラグ）にすると、巻き戻しや
手編集でファイルの RG タグが変わっても「書き込み済み」のまま残り、UI の半透明表示が嘘になる。
ファイルの内容から判定すれば不変条件 1（ファイルが正）と整合する。

**却下**: 値だけ書いて他のキーを残す（Opus に `REPLAYGAIN_*` と `R128_*` が併存する、mono に
なった曲の古い album gain が残る）。書き込み専用の op 種別 / ジョブ種別。preview → apply の
2 段階（選ぶものが無い）。

**未決**: スキャナが外部のタグ変更を取り込んだときの `rg_written_at` の判定（現状はスキャンでは
触らない。外部ツールが RG タグを消しても `rg_written_at` は残る。P1-0 か scanner の別タスクで
`sync_written_at` を呼ぶ）。`tag_version` が進むので Derived の追随（P1-10）が RG タグを
`R128_*` へ変換して埋める（SPEC §7.6）のは P1-10 側の責務。

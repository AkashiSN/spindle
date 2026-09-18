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

## D-49 アートワークの解決とキャッシュ（読み側。実装合わせ）

**決定**（P1-3 の読み側）:

- **album のアートワークは同梱カバー画像 → 最初のトラックの埋め込み画像の順。** 同梱画像は
  `cover` / `folder` / `front` × `jpg` / `jpeg` / `png` / `webp` を名前 → 拡張子の優先順で 1 つ
  （大文字小文字は区別しない。ZFS insensitive と同じ）。埋め込みは構成トラックを
  `disc_no` / `track_no` / `rel_path` 順に見て最初に見つかる 1 枚（front cover を優先、無ければ
  ファイル内の最初）。形式と寸法はバイト列のヘッダ（`imagesize`）で判別する
- **原画像は元の形式のまま、サムネイルだけ WebP。** `artwork` 表は画像の SHA-256 で一意化し、
  原画像を `<data>/thumbs/<hex>/orig.<ext>` へコピーする（同じ画像は 1 回だけ。Library には
  何も書かない）。`thumbnail` ジョブ（dedup `thumbnail:<artwork_id>`、並列 4）が ffmpeg で
  `256.webp` / `768.webp`（長辺、拡大なし、quality 82）を tmp + rename で作る
- **解決はスキャンの Phase 5（commit の後）で、予約された album だけ。** Phase 4 は行が変わった
  トラックの**現在の album と直前まで属していた album**（分割・移動で構成を失った側）の
  `artwork_resolved_at` を同じトランザクションで NULL にして再解決を予約する。予約は DB に残るので、
  Phase 5 が cancel・失敗・プロセス停止で終わっても次のスキャンで続きを行う（ジョブの冪等性）。
  Phase 5 の対象はこの予約に加えて、同梱画像の有無・stat（`albums.cover_inode / cover_size /
  cover_mtime_ns / cover_ctime_ns`。トラックではないので最速パスでは拾えない。マイグレーション
  0004）が前回と違う album と、参照中の原画像がキャッシュに無い・`artwork.bytes` と長さが違う album
  （thumbnail ジョブも原画像の欠損で参照 album を予約する）。同じ長さの破損は incremental では
  見えないが、`put_original` が既存ファイルの SHA-256 を照合して不一致なら置き直すので deep で直る。
  deep は Phase 4 の同じトランザクションで active な album 全件を予約する（commit 直後・Phase 5 の
  前に止まっても残る）。Phase 5 も候補を全件、解決を始める前に予約し（`artwork_resolved_at = NULL`）、
  成功した album だけが予約を消す。決められなかった album（deep だけを理由に対象になったものを含む）と、cancel・
  DB エラー・panic・プロセス停止で処理できなかった album は予約のまま残り、次の incremental で続きになる。マイグレーション直後の既存 album と、アートワーク無しで
  走ったスキャナの結果も NULL なので次のスキャンで解決する。画像の読み取りは並列、DB 更新は
  1 トランザクション
- **決められない album は状態を動かさない。** キャッシュへ置けない、同梱画像の I/O 失敗、読んで
  いる間に同梱画像が変わった（読み前後の stat 不一致）、構成トラックを 1 本でも開けない・タグを
  読めない（先頭が読めなければ「画像なし」も「後続が最初」も確定できない）ときは、旧画像も
  `artwork_resolved_at` も据え置いて次のスキャンでやり直す。同梱画像が画像として認識できない
  （magic だけ一致して寸法が読めない・0・16,384 px 超）ときだけは埋め込みへ倒し、stat は記録する
  （変わるまで読み直さない）。同梱画像は 32 MiB を上限とし（それ以上は画像とみなさない）、読み取りも
  上限 + 1 で打ち切る
- **Phase 4 の commit 後の Phase 5 は run の状態を戻さない。** missing の確定と `scan_runs = completed`
  は commit 済みなので、Phase 5 の cancel は「続きは次回」、失敗は `ScanReport.artwork_error`
  （警告ログ）にとどめ、scan ジョブは done にする。予約が残っているので取りこぼさない
- **`GET /api/artwork/:hash?size=`** は `size` があれば WebP、無ければ原画像を元の MIME で返す。
  サムネイルが未生成なら原画像へ倒す。倒した応答は `no-cache`（ETag で再検証。後で WebP に
  置き換わる）、それ以外は `immutable`。`GET /api/albums` に `artwork_hash`（hex）を足す
- UI のアルバムグリッドは missing の album を出さず、クリックで一覧を `album_id` に絞る

**理由**: foobar2000 や SMB クライアントは同梱の cover.jpg を見るので、それを優先すると spindle と
他のプレイヤーで同じ絵になる。ハッシュアドレスにすると同じ画像を持つ数千 album でも原画像と
サムネイルは 1 組で済み、URL が不変なのでブラウザキャッシュを最大にできる。原画像を Library から
都度読む案は、埋め込み画像の取り出しに毎回 lofty のパースが要り、cover.jpg の差し替え後に旧画像を
出せない（将来の巻き戻しに使えない）。ffmpeg は既に必須の外部バイナリで libwebp を内蔵する。

**却下**: 埋め込み優先（ディレクトリ単位で他のクライアントが見る絵と食い違う）。`image` クレート
（WebP は可逆のみでサムネイルが大きい）。`webp` クレート（Docker ビルドに C 依存が増える）。
スキャンのたびに全 album の同梱画像をハッシュする（9,000 album × 数百 KB を毎回読む）。
サムネイル未生成時の 404（グリッドに壊れた画像が出る）。

**却下（追加）**: Phase 5 の対象を「この run で行が変わったトラックの現在の album」から都度計算する
（構成を失った旧 album が対象から漏れ、Phase 5 が途中で止まると次回 incremental では track が
unchanged なので二度と再試行されない）。Phase 5 の cancel / 失敗で run を cancelled / failed に
上書きする（missing_since が立っているのに run が failed という矛盾。SPEC §7.1）。

**未決**: 書き側（埋め込み → cover.jpg の抽出、画像アップロードによる一括差し替え）は別の作業で
設計する（cover.jpg の書き換えを編集バッチでどう巻き戻すか、埋め込みを全トラックに書くか）。
参照が無くなった `artwork` 行と `thumbs/` の回収は GC（P1-11）。スキャナが外部の埋め込み画像の
変更を検出するのは tag_hash 経由（`PICTURE` 疑似キー）なので、画像だけ差し替えられた場合も
Phase 5 に入る。

## D-50 同一性解決の audio_md5 は要求される分だけ並列で計算する（実装合わせ）

**決定**（P1-0）:

- `identity::resolve` が md5 を要求する条件を絞る。段 1（inode 一致で `size` も `mtime` も違う）は
  **行が `audio_md5` を持つときだけ**要求する（持たなければ照合できない）。段 2 は**移動候補**
  （`audio_md5` を持ち、段 1 で claim されず、旧 `rel_path_key` が inventory に無い行）が 1 つも無ければ
  md5 を要求しない（計算しても照合相手が無い）。初回スキャン（既存行なし）と移動の無い増分スキャンでは
  要求が 0 件になる
- 要求されうるエントリの集合を `identity::md5_requests`（md5 が無い前提で `resolve` を走らせて要求を
  記録する。実際の `resolve` の要求を含む）で先に求め、スキャナが Phase 3 と同じ並列度で計算してから
  キャッシュを引くコールバックで `resolve` を呼ぶ。結果は Phase 3 のフィンガープリント計算にも渡す
  （同じファイルを二度デコードしない）
- 進捗コールバックに相を持たせる（`Progress = Fn(ScanPhase, done, total)`。`Md5` は要求があるときだけ、
  `Read` は Phase 3）。ジョブの進捗（`done / total`）は相を区別せず最新値を出す
- ログの既定フィルタ（`RUST_LOG` 未設定時）で `lofty` と `symphonia*` を `error` に落とす。ファイルごとの
  WARN（非対応のメタデータ・軽微な破損）は初回 deep scan で数千行になり、読めなかった結果は spindle 側の
  ログと `ScanReport.errors` で分かる

**理由**: P0-14 のリハーサル（ALAC 7,572 本）で初回 deep scan が 75 分かかった主因は、Phase 2 が全エントリの
ALAC / WAV / AIFF を**直列に**フルデコードしていたこと。しかも初回は既存行が無いので、その md5 は何とも
照合されずに捨てられていた（Phase 3 が並列に計算し直す）。要求を絞ると初回・通常の増分で Phase 2 の
デコードが消え、移動があるときだけ未決エントリ分を並列に計算する。判定の結果は変えない
（`md5_requests` は `resolve` の要求の上位集合なので、`resolve` が引くキャッシュに欠けは無い）。

**却下**: Phase 2 の md5 を丸ごと Phase 3 へ回す（段 2 の移動判定に md5 が要るので、判定を Phase 3 の
後に遅らせる大改造になる）。段 2 の要求を移動候補の md5 と一致しうるエントリに更に絞る（md5 を計算する
前に一致は分からない。候補の有無で 0 件にできれば十分）。

**計測**（2026-09-17、リハーサル環境 9,098 トラック / ALAC 7,570 本、12 コア）: 空 DB からの初回
deep scan 3,583 秒 → **552 秒**。既存 DB への deep scan 554 秒（Phase 2 の要求 0 件、Phase 3 が
並列にフルデコード）。変更なしの増分は 1 秒未満。

**未決**: deep scan は Phase 3 で全エントリのフィンガープリントを計算し直すので、可逆のフルデコードは
残る（deep の定義そのもの。並列度分だけ短縮）。P1-4 の正規化で ALAC が FLAC になれば、deep でも
STREAMINFO の MD5 を読むだけになる。

## D-51 Derived の生成と追随はトラック単位の `transcode` ジョブが現在値に揃える

**決定**（P1-10）:

- **`transcode` ジョブ（payload `{track_id, audio_version, tag_version}`、dedup
  `transcode:<track_id>:<audio_version>`、並列 cpus-1、基盤が `audio_version` で stale 判定して track
  lock を取る。SPEC §8）は「そのトラックの Derived を現在の状態に揃える」ハンドラ。** D-42 の
  payload `kind` は解釈しない（廃止）。ハンドラは毎回現在値から必要な処理を判定する
  （`domain::derived::plan`）: 非可逆 / missing / 1ch・2ch 以外（チャンネル数不明を含む。属性を読めなかった
  ファイルはマルチチャンネルかもしれない）は no-op（Derived を作らない。D-8 / D-22）。
  `derived_files` が無い・`src_audio_version` が違えば再エンコード。音声版が一致していて `rel_path`
  が期待値（Library の `rel_path` の拡張子を `.opus` に替えたもの）と違えば Derived を rename
  （元が無ければ再エンコード）。`src_tag_version` / `src_artwork_id` / `src_rg_scanned_at` のどれかが
  違えばタグ・画像だけ上書き（Derived が無ければ再エンコード）。全部一致なら no-op。
  Library の FD は開いた直後とエンコード後に fstat して DB の行と照合し、違えば失敗（再スキャン
  後の再試行で通る。パスは識別子ではない）
- **投入契機は 4 つ**で、すべて `db::derived::enqueue_if_stale`（上の判定に当たるときだけ投入）に
  集約する。(1) **scan ジョブの完了時**に対象を 1 クエリで取り出して一括投入
  （`enqueue_all_stale`。初回は可逆全曲、以後は差分。外部の移動・音声差し替え・カバー差し替え・
  実行中に dedup で落ちた分もここで拾う）。(2) tagwrite が applied になったトランザクション
  （D-42 の `enqueue_derived_retag` を置き換え）。(3) rename が applied になったトランザクション。
  (4) RG 解析値の保存と同じトランザクション。**タグ版に乗らない 2 つの世代を `derived_files` に
  持つ**（マイグレーション 0005）: `src_artwork_id`（埋めた画像。NULL = 画像なし）と
  `src_rg_scanned_at`（埋めた `R128_*` の元になった `tracks.rg_scanned_at`。NULL = 未解析で書いた）。
  これが無いと RG の解析やカバーの差し替えが Derived に届くのは次のタグ編集まで。`src_artwork_id` に
  記録するのは album の `artwork_id` ではなく**実際に埋めた**画像の id（原画像がキャッシュに無くて
  画像なしで書いたら NULL。album の id を書くと、スキャンが原画像を復旧しても同じ SHA-256 → 同じ id
  なので不一致が出ず、画像なしのまま固定される）。`rg_scanned_at` は値が変わる再解析では前回より
  必ず大きくする（同じ秒でも世代が進む。`db::replaygain::store`）
- **エンコードは `media::encode::OpusEncoder`（`FlacEncoder` と同型）。** Library の FD を
  `/dev/stdin` で ffmpeg に渡して `<data>/tmp` の WAV へ（ビット深度に合う `pcm_s*le`、無ければ
  `pcm_s24le`）→ `opusenc --bitrate <derived_bitrate> --vbr --music --discard-comments
  --discard-pictures` → lofty で Vorbis Comment と画像を書く → Derived の宛先ディレクトリの tmp
  （`create_tmp`）へコピー → `replace_file`。`derived_files` の upsert はファイル配置後
- **タグは Library ファイルの `TransferTags` をそのまま写し、RG だけ DB の解析値から `R128_*` へ変換して
  差し替える**（`REPLAYGAIN_*` は消す。SPEC §7.6「再解析しない」。解析前なら RG タグは書かない）。
  **画像は album の `artwork_id` の `768.webp`**（P1-3 のキャッシュ。無ければ thumbnail ジョブと同じ
  変換で作る。原画像も無ければ画像なしで作り、次のスキャンが原画像を復旧したときに
  `src_artwork_id` の不一致で書き直される）を `image/webp` の front cover 1 枚として埋める。
  トラック自身の埋め込み画像は使わない
- **Derived の削除は行わない。** missing は可逆（SMB 切断）なので Derived を残し、
  `retention_days` 超の回収と孤児ファイルの回収は GC（P1-11）。期待パスを**別トラックの**
  `derived_files` 行が占有しているとき（A を消して B を A のパスへ移した: B は inode で追随し、A は
  key を明け渡して missing になるが、A の Derived の行は残る）は、占有側が missing ならその行を消して
  上書きする（Derived は再生成物でユーザデータではない）。占有側が生きていれば failed（バックオフで
  再試行。A が生きたまま別のパスへ移り、その跡地に B が来た場合で、A の transcode が先に Derived を
  動かせば次の試行で通る。占有側のファイルは占有側のロックを持たないので触らない）。**占有の確定は
  物理的な書き込みの前に 1 トランザクションで行う**（`claim_path`: 読んで missing なら行を消す、まで
  同じトランザクション）。scanner は track lock を取らないので、確認と書き込みの間に A が別のパスで
  復活しうる。先に行を消しておけば、復活した A は「Derived 無し」として自分の期待パスに作り直すだけで、
  B が置いたファイルを A のパスへ動かすことはない。**期待パスの canonical key は `derived_path_locks`
  （マイグレーション 0005）で排他予約してから触る**（encode / move / retag の全経路。別のジョブが
  持っていれば試行回数を数えずに再キュー、終了時に解放。持ち主が `running` でなくなれば無効、
  起動時リカバリで全件消す）。`track_locks` は track 単位なので、`x.flac` と `x.wav` のように別の
  Library パスが同じ期待パスに写る 2 本の並走や、先に走っている retag が後から同じパスを置き換える
  ことを防げない。予約があれば、並走は直列化されて負けた方が「生きているトラックが持っている」で
  失敗し（勝者のファイルは上書きされない）、retag 中の宛先を別のトラックが claim することもない
- **Library の swap / 循環 rename は自己退避で解く。** 期待パスを**生きている**トラックが持っていて、
  それがその相手の期待パスでもない（相手も追随待ち）ときは、自分の Derived を同じディレクトリの
  一時名 `<名前>.moving-<track_id>` へ退避して行のパスもそこへ向け（音声版が古くても退避する。次の
  試行の Encode が置き換えて退避ファイルを消す。実体が無ければ行を消して key を明け渡し、次の試行は
  Encode）、相手の transcode が queued / running なら再キュー（試行回数を数えない）、いなければ失敗
  （バックオフ。次の scan で両方投入される）。
  相手はそれで自分の元パスへ移れ、自分は次の試行で相手が空けたパスへ移る（rename バッチの 2 段階と
  同じ考え方。相手のファイルは相手のロックを持たないので触らない）。相手の期待パスでもあるとき
  （`x.flac` / `x.wav`）だけが本当の衝突で、失敗にする。退避名は起動時回収の対象外（行が指している）で、
  クラッシュで行が追随しなかった退避ファイルは GC が孤児として回収する
- **SIGKILL / 電源断で残った作業ファイルは起動時に回収する**（`transcode::sweep_tmp`。ワーカー起動前）。
  `<data>/tmp` の `spindle-transcode-*` / `spindle-normalize-*` と Derived の `.spindle-tmp-*`。単一
  インスタンスで起動前は何も走っていないので年齢を見ずに消す。Derived は scan の対象外なので
  scanner の猶予付き回収は効かない
- `delivery` ビューは変えない（D-25）

**理由**: ハンドラが「現在値に揃える」1 種類だと、retag / move / 再エンコードの区別を投入側が
正しく判定する必要がなく、投入が重複しても（dedup は `audio_version` 単位）最初の 1 本で全部
片づく。冪等性（電源断・再投入）も同じ判定で済む。scan 完了時の一括投入にしたのは、旧 `Opus/` を
捨てた移行直後は Derived が空で（D-45）、手動の起動を待つ理由がないから。画像を album の WebP
サムネイルにするのは、Android 側に cover.jpg のミラーが無く埋め込みが要る一方、原画像（数百 KB〜
数 MB）を 7,500 本に埋めると Derived が数 GB 太るため。長辺 768 の WebP は数十 KB で足りる。

**却下**: `kind = "retag"` / `"move"` / `"encode"` を投入側で決める（判定が 2 か所になり、投入後に
状態が変わると誤る）。Derived を 1 本の同期ジョブで全曲処理する（並列度と stale 判定を自前で持つ
ことになり、SPEC §8 の `transcode` と二重になる）。トラック自身の埋め込み画像を写す（同梱
cover.jpg 優先の D-49 と食い違い、原寸のまま太る）。missing になった時点で Derived を消す
（SMB 切断のたびに再エンコードが走る）。手動 API のみ（移行直後の 7,500 本を誰も投入しない）。

**計測**（2026-09-17、リハーサル環境 ALAC 7,570 本、12 コア、`transcode` 並列 11）: 起動時スキャンの
完了で 7,570 件を一括投入し、39 分で全件完了（failed 0）。Derived 30 GB。投入自体は 1 トランザクションで
0.14 秒。エンコード中のロードアベレージは 13 前後で、他のコンテナと同居する NAS では並列度を設定で
落とせるようにする余地がある（SPEC §8 の cpus-1 固定のまま）。

**未決**: マルチチャンネルのダウンミックスと非可逆の `force_transcode`（SPEC §7.6。どちらも
スキーマに列が無く、需要が出てから）。`transcode` の並列度の設定化。UI の起動導線と進捗表示（P1-1 / P1-2 と同様に後回し）。
Derived 側の `cover.jpg` ミラー（P1-8 のエクスポートで要るなら）。transcode が**実行中**に scan が
同じトラックの版を進めた場合、その scan の一括投入は dedup で落ちる（ハンドラが記録するのは
読み始めた時点の版なので不一致は残り、次の scan で投入される）。

## D-52 再生は原本か Derived を Range で直送し、オンザフライ変換は Derived が無いときだけ

**決定**（P1-9）:

- **`GET /api/stream/:id`** は Library の原本を Range 対応で返す（単一範囲、206 / 416、
  `Accept-Ranges: bytes`、HEAD、`ETag` は `(inode, size, mtime_ns)`、`Content-Type` は codec から）。
  ファイルは `RootDir::open_file`（dirfd 基準）で開き、**開いた FD の fstat が DB の行と一致する**
  ことを確かめてから返す（パスは識別子ではない。同名で差し替えられていれば 409 `stale`。次の
  スキャンで直る）。missing は 404
- **`?transcode=opus`** は `delivery` ビューが Derived を指す（音声版が一致）なら **Derived の Opus を
  同じ Range 対応で直送**する。SPEC §11 の「変換結果はキャッシュしない（Derived と役割が重複）」は、
  Derived がそのキャッシュだという意味。Derived が無い・音声版が古いときだけ ffmpeg を stdout
  パイプで起動して Ogg/Opus（libopus 128k）を chunked で返す。このときは Range に応えず、
  `?start=<秒>` を `-ss` に渡してその位置から返す。子プロセスはクライアントの切断（本文の drop）で
  group ごと kill する。上限はトラック長 + 猶予（60 秒）で、本文のストリームが `Sleep` も poll する
  ので stdout が止まったままでも期限で起きて打ち切る（本文はエラーで終わり、子は kill）。stdout の
  EOF 後は終了コードを検査し、非ゼロなら本文をエラーで終える（200 の途中で切れる形になるが成功と
  区別できる）。malformed な Range（数値でない・桁あふれ・逆順・複数範囲）は無視して 200、構文は
  正しいが範囲外のときだけ 416。`If-None-Match` は `*` と弱比較（`W/`）と並記を扱い、304 にも
  `ETag` / `Cache-Control` を付ける
- **クライアント能力は再生時に URL で選ぶ。** 起動時に `canPlayType()` で codec ごとの可否を
  持ち、再生できない codec なら `?transcode=opus`。サーバへ能力を通知して保存する形（SPEC §11）は
  取らない（状態を持たないほうが単純で、同じ結果になる）。**可逆はブラウザが再生できても既定で
  Derived の Opus を使う**（帯域。設定「可逆は原本を再生」を localStorage に持ち、原本を選ぶと
  Range 直送）
- **RG はクライアントで掛ける。** `GET /api/tracks` の行に `rg { track_gain, track_peak, album_gain,
  album_peak }`（内部表現 -18 LUFS 基準の dB、未解析なら null）を足し、UI は Web Audio の
  `GainNode` で `10^(gain/20)` を掛ける。peak でクリップしないよう `min(10^(gain/20), 1/peak)`。
  P1-9 は track gain のみ。トグルは localStorage
- **UI**: 表の行先頭に ▶（ホバーで表示）を置き、その行から**表示中の順序で連続再生**する。
  下部バー左に ▶ / ‖、シーク、経過 / 総時間、音量、RG トグル、曲名。`transcode=opus` を要求しても
  Derived の直送か ffmpeg の chunked かはクライアントには応答からしか分からないので、`loadedmetadata`
  で duration が有限なら `<audio>` のネイティブシーク（Range）、無限 / NaN なら chunked として
  `start=` で読み直して表示時刻をオフセットする。metadata 前のシークは保留して確定後に適用する。
  次曲は表の現在の順序で決め、末尾で未読なら読んでから続け、フィルタ・ソートの変更で現在曲が表から
  消えたらその曲で止まる。ユーザの play / stop は自動継続の待ちを必ず打ち切る

**理由**: P1-10 で可逆全曲の Derived が揃うので、再生の大半は静的ファイルの Range 配信で済み、
ffmpeg の起動はライブラリに加わった直後の短い期間に限られる。ffmpeg の chunked 出力はシークが
できない（`-ss` の再起動で代替）ので、Range が効く Derived を優先する。RG をクライアントで掛けるのは、
サーバ側でゲインを掛けると原本の直送ができなくなり、Derived にも `R128_*` が入っていてブラウザは
それを読まないため。

**却下**: 変換結果のサーバ側キャッシュ（Derived と二重）。サーバへの能力通知（セッションに状態を
持つ理由が無い）。`<audio>` の `volume` で RG（0..1 なので正のゲインを掛けられない）。

**未決**: Safari（Opus 不可）向けの AAC 変換（`transcode=aac`。ffmpeg の aac エンコーダで足せる）。
album gain モード。ハイレゾのサンプルレート変換（原本を選んだときは 96 kHz をそのまま送る）。

## D-53 手動プレイリストと m3u8 の書き出し・取り込みの固定値と境界

**決定**（P1-6。仕様 SPEC §9 / §10 / §12 が定めていない値と境界）:

- **1 プレイリストに同じトラックは 1 回だけ。** 追加・取り込みで既にある・入力内で重複する
  トラックは黙って飛ばし、件数（`skipped` / `duplicates`）だけ返す。`playlist_items` の主キーは
  `(playlist_id, position)` で重複を許す形だが、表の行 = トラックの同一性（選択・一括編集・
  `sort=position` の 1:1 JOIN）を崩さないために使わない。`position` は 0 から連続で、追加・除外・
  移動のたびに全行を振り直す（高々数千件）
- **並びは表と共通。** 項目一覧の API は作らず、`GET /api/tracks?filter={"playlist_id":N}&sort=position`
  で取る。`position` は `filter.playlist_id` と組でだけ有効（単独は 400）。`playlist_items` を JOIN して
  `(playlist_id, position)` の主キー順で読むので temp B-tree は出ない。UI はプレイリストに入ったら
  `position` 昇順、出たら既定ソートへ戻す。並べ替え（行のドラッグ）は `position` 昇順のときだけ
- **移動は「集合を `before` の直前（null なら末尾）へ」。** 移動する側は現在の相対順を保つ。
  `before` が集合の中・プレイリストに無いときは 400
- **書き出し先は `Playlists/<profile>/<name>.m3u8`。** foobar / android / internal の 3 プロファイルが
  同名で衝突しないよう、プロファイル名のディレクトリに分ける。`Playlists/<profile>/` は `Library/` と
  同じ深さなので相対パスは SPEC §10 どおり `../../Library/…` / `../../Derived/…`。旧ライブラリから
  移した `Playlists/m3u8/` は取り込み元として残す（書き出しはそこへ戻さない）。書き出しは tmp + rename、
  `playlist_exports (playlist_id, profile_id)` に `out_path` と時刻を記録する（改名後の書き出しは
  新しい名前のファイルで、古いファイルは消さない。GC の対象にもしない）
- **プレイリスト名はそのままファイル名になる**ので、`RelPath` の 1 要素と同じ規則（`/` `\` NUL 不可、
  SMB / exFAT の禁止文字・予約名・末尾ドット）で名前単体と `<name>.m3u8` の両方を検証し（長さの
  255 バイトは拡張子込み）、前後の空白は落とす。**一意性は `name_key`（`canonical_key` = casefold +
  NFD。マイグレーション 0006）で判定する。** ZFS の insensitive + formD では `Foo.m3u8` と `foo.m3u8`、
  NFC と NFD の同名が同じ実体なので、`name` の BINARY UNIQUE だけでは別プレイリストの書き出しが
  上書きし合う。違反は 409 `duplicate`。既存行の backfill はマイグレーション実行時に登録する SQL 関数
  `spindle_canonical_key()`（Rust の `canonical_key` そのもの。`lower()` は ASCII しか畳まない）で行い、
  同じ key の行が既にあれば id 最小の 1 本を残して後続を空いている `<name> (n)`（n = 2, 3, …。他の
  全行の key と二次衝突しないもの、`.m3u8` 込みで 255 バイトに収まるよう名前側を削る）に改名する
  （消さない。SQL では書けないので `db::migrations::post_sql(6)` が同じトランザクションで行い、
  UNIQUE INDEX もそこで作る）
- **m3u8 は `#EXTM3U` + `#EXTINF:<秒>,<Artist - Title>` 付き、UTF-8 BOM なし、LF。** missing の
  トラックは書かず件数（`skipped_missing`）で返す。`delivery` プロファイルは `delivery` ビューの
  `path`（音声版が一致する Derived、無ければ原本）
- **`GET /api/playlists/:id/export?profile=` は本文を返し、`POST` が書く。** GET は trusted CIDR の
  allowlist（D-27）に入っているので、他プレイヤーが curl で取れる。POST と import は Playlists root が
  無ければ 503
- **取り込みは Playlists root 下のファイルから**（`GET /api/playlists/import` が `.m3u8` / `.m3u` を
  深さ 4 まで列挙、`POST { path, name? }` で 1 本を手動プレイリストにする）。行の解決は
  `\` → `/` にしてから、絶対パス（`/…`、UNC `//host/share/…`、ドライブレター `C:/…`）なら
  `/Library/` `/Derived/` のうち文字列中で最も早く現れるものの直後から取り、**無ければ root の外として
  解決しない**（偶然同じ
  rel_path があっても誤一致させない）。相対なら先頭の `./` `../` を剥がし先頭の root 名を落とす。
  root 相対にしてから `rel_path_key` の完全一致 → 拡張子を除いた stem の一致（旧 `Opus/` の `.opus` 行が
  今の `.m4a` / `.flac` に当たる）。stem の候補が複数なら active を優先し、**その中でまだ複数なら曖昧と
  して解決しない**（同じ stem の `.flac` と `.m4a` が両方 active のような状態でどちらかを黙って選ばない）。
  解決できない行は応答の `unresolved` に返すだけで DB には残さない。名前の既定はファイル名の stem
- **自動再書き出し（ライブラリ変更をトリガにデバウンス）は P1-7 で**（スマートプレイリストが
  前提。手動は明示の書き出しだけ）。`auto_export` 列は PATCH で切り替えられるが今は何も駆動しない

**理由**: 旧ライブラリの `Playlists/m3u8/` 28 本は foobar 由来で、`../Anime/…/1.01. Crow Song.opus` の
ように旧 `Opus/` 相対かつ拡張子が今の Library と違う。パスの完全一致だけでは 1 本も当たらないので
stem で当てる。プロファイル別のディレクトリにするのは、SPEC の `Playlists/m3u8/` 1 段だと 3 つの
出力先が同名で上書きし合うため。

**却下**: 同じトラックの複数回収録（表の行の同一性が崩れる）。項目一覧の専用 API（表の集合は
サイドバーの scope で差し替えるだけという SPEC §12.1 の原則に反する）。取り込みを画面からの
ファイルアップロードにする（ファイルは既に Playlists root にある。SMB を開かずに済む）。

## D-54 スマートプレイリストは評価結果を materialize し、常駐タスクが再評価・再書き出しする

**決定**（P1-7。仕様 SPEC §9 / §10、docs/DSL.md、D-16 / D-39 が定めていない値と境界）:

- **評価結果は `playlist_items` に書く（materialize）。** ルール（`rule_ast`）が正で、項目はその
  キャッシュ。ORDER BY / LIMIT を適用した並びをそのまま `position` にするので、表の
  `playlist_id` scope・`sort=position`・m3u8 の書き出しは手動プレイリストと同じ経路で動き、
  LIMIT / ORDER BY と表のキーセットページングが衝突しない。smart への項目の追加・除外・移動は
  409 `smart`、manual へのルール差し替え・再評価は 409 `manual`
- **`added`（初回スキャン日時）は `tracks.added_at`**（マイグレーション 0007。既存行は `seen_at` で
  backfill。スキャナが INSERT 時に設定し、復活でも変えない）
- **プレビューは id 無しの `POST /api/playlists/preview { rule }`**（構文・型の検証と評価件数、AST）。
  SPEC §9 の `POST /api/playlists/:id/preview` は「保存前のルールを試す」用途に合わないので
  置き換える。表でのプレビューは D-39 どおり `filter.dsl`（WHERE だけ。ORDER BY / LIMIT は無視し、
  `Filter::parse` で構文・型を検証して 400）。`POST /api/playlists/:id/refresh` で手動再評価
- **自動再評価・再書き出しはプロセス内の常駐タスク**（`playlist::autoexport`）。`jobs` 表の
  `type` CHECK が固定でジョブ種別を足せず、ルールが正なので永続化する理由も無い。`library`
  イベント・終端の `batch` イベント・完了した job を合図に `[export].autoexport_debounce_sec`
  だけ待ち（その間の合図はまとめる）、全 smart を再評価して並びが変わったものだけ書き直し、
  `auto_export = 1` で書き出し記録のあるプレイリスト（手動も）を記録済みプロファイルへ書き直す。
  起動時にも 1 回走る。`auto_export = 0` は再書き出しの対象外（再評価はする）。項目が書き換わった
  プレイリストは SSE `playlist { playlist_ids }` で知らせ、UI は一覧の件数と（表示中なら）表を取り直す
  （UI は元の `library` イベントを即時に処理済みで、デバウンス後の書き換えを知らないため）
- **プレイリストへの書き込み API は 1 トランザクション。** 複合 PATCH（name / auto_export / rule）は
  どれかが通らなければ何も変えず、ルールの差し替え・作成は評価と materialize まで含めて原子的
  （評価の実行時失敗のうち `regexp()` 由来 — バックトラック上限など。SQLite は UDF のエラーを文字列で
  しか返さないので接頭辞 `regexp: ` で見分ける — は 400 `rule_failed` で巻き戻す。それ以外の SQLite
  障害は 500 のまま）。
  `rewrite_items` は SAVEPOINT で囲み、外側のトランザクションの有無によらず原子的
- **キーワードには語境界がある**（`ISLAND` を `IS` + `LAND` と読まない。`)AND` や `IS"x"` の記号の
  隣接は通る）。`LIMIT` は 1..=i64::MAX、`added` の日付は暦日として妥当な 1..=9999 年だけ
- **SQL 生成の固定値**: `IS` は `COLLATE NOCASE`、`HAS` は `LIKE`（メタ文字はエスケープ。どちらも
  ASCII の大小だけ畳む。日本語の casefold まではしない）、`MATCHES` は全コネクションに登録した
  `regexp(pattern, text)`（fancy-regex。スレッドごとにコンパイル済みをキャッシュ）、`GREATER` /
  `LESS` は数値フィールドと `date`（辞書順）/ `added`（`YYYY-MM-DD` は UTC 0 時か epoch 秒）だけ、
  `duration` は秒（`IS` は丸めた秒）。真偽（`lossless` `has_derived` `missing`）は `IS true / false`
  だけ。任意タグは `EXISTS (track_tags …)` で多値のどれかが一致すれば成立。`NOT` は `coalesce(…, 0)`
  で NULL を偽に固定。ORDER BY は NULL を末尾、文字列は NOCASE、`t.id` でタイブレーク
- キャッシュ列以外のフィールド名は `track_tags` のキーとして大文字化して引く。存在しないタグ名は
  単に一致しない（エラーにしない。foobar と同じ）

**理由**: スマートを仮想（毎回 SQL に展開）にすると、表のキーセットページングと LIMIT / ORDER BY
が両立せず、書き出しも別経路になる。materialize なら P1-6 の機構をそのまま使え、評価コストも
ライブラリ変更のたびに 1 回で済む（9,098 曲で数十 ms）。

**却下**: 仮想スマート（上記）。ジョブとしての再評価（CHECK 制約の変更に表の作り直しが要る）。
`IS` を canonical key で比較（タグの値に key 列が無く、全行の関数評価になる）。

**未決**: `HAS` の語境界（DSL.md、実機で突き合わせ）。foobar クエリへの変換は D-55。

## D-55 foobar クエリ変換は変換できない項を落として notes に出し、プロファイルは固定

**決定**（P1-8。仕様 SPEC §10「エクスポート形式」「foobar クエリへの変換」、docs/DSL.md が定めて
いない値と境界）:

- **変換は AST → `{ query, sort, notes }` の純粋関数**（`playlist::fb2k`）。`GET /api/playlists/:id/fb2k_query`
  が保存済みの `rule_ast` を変換して返す（smart のみ。manual は 409 `manual`）。SPEC の `.txt` 出力は
  作らず、UI のダイアログからクリップボードへコピーする
- **写像表**: `albumartist` → `%album artist%`、技術情報は生の値を返す `%__…%`（`%__codec%`
  `%__samplerate%` `%__bitrate%` `%__channels%` `%__bitspersample%`。`%channels%` は mono / stereo の
  表示文字列になるので比較・ソートに使えない）、`duration` → 特殊フィールド `%length_seconds%`
  （技術情報でないので `PRESENT` / `MISSING` は変換不能）、その他の標準タグ・任意タグは同名。
  spindle 固有（`verification` `category` `source_type` `lossless` `added` `has_derived` `missing`）と
  `MATCHES` は変換不能
- **変換不能な項は落とし、`notes` に原文（spindle の記法）と理由を残す。** 落ちて空になった `AND` /
  `OR` と、子が落ちた `NOT` も落とす。全部落ちたら `query` は空で、その旨も `notes` に出す。
  `OR` の 1 項が落ちると結果は狭まるが、黙って別の条件に置き換えるよりは `notes` で見せて手で
  直してもらう方が安全
- **演算子**: `PRESENT` / `MISSING` は foobar の後置形、`date` の `GREATER` / `LESS` は `AFTER` /
  `BEFORE`。値は空白・括弧・`"` を含むか予約語と同じなら二重引用符で囲む。`"` を含む値は foobar 側で
  エスケープできないので `notes` に警告。複合式の子は常に括弧で囲み、foobar 側の優先順位に頼らない
- **`ORDER BY` は `sort`（ソートパターン）へ分離。** ソートパターンは title-format の出力を文字列で
  比べるので、spindle が数値順にするフィールド（`tracknumber` `discnumber` `samplerate` `bitrate`
  `channels` `bitdepth` `duration`）は `$num(…,10)` でゼロ埋めする（比較には付けない）。降順・
  `random`・`LIMIT` は表せないので `notes`
- **`delivery` の書き出しは `stale_tags`（タグ追随待ちの Derived）を件数で返す**（`POST …/export` の
  応答。自動再書き出しはログ）。追随ジョブの完了を待つ方式は取らない（書き出しがジョブ待ちで止まる
  方が運用上困る。追随が終われば次の合図で書き直される）
- **エクスポートプロファイルは 3 つ固定で CRUD の API は持たない。** 実運用で変えたいのは foobar の
  UNC prefix だけなので `[export].fb2k_prefix` を正とし、起動時に `export_profiles` の `foobar` 行の
  `path_prefix` を揃える（`db::playlists::sync_foobar_prefix`）。`.pls` も作らない（`format` の CHECK に
  列挙だけ残す）

**理由**: Autoplaylist はファイルで渡せないので、ユーザが貼れる 2 本の文字列と「何が落ちたか」が
あれば足りる。技術フィールドまで写すのは、`%codec% IS flac` のような条件がライブラリの整理で
普通に出てくるため。

**却下**: 変換不能な項を `%path% HAS` などへ近似する（`category` はディレクトリ名と一致するが
部分一致で誤ヒットする）。プロファイル CRUD（3 つ以外の出力先が無い）。`.txt` の書き出し
（Playlists root に置いても foobar はそこから読まない）。

**未決**: foobar 実機との突き合わせ。`HAS` の語境界（spindle は `LIKE %v%` の部分一致）と、
`%__…%` の技術情報フィールドに対する `PRESENT` / `MISSING` の挙動（ヘルプの記述に従って写して
いるが未確認）。

## D-56 GC は 5 区分を 1 本のジョブで回収し、dry-run は同じ計画関数を API が同期で返す

**決定**（P1-11。仕様 SPEC §6「論理削除」/ §8 / §13 `[gc]`、D-45 / D-46 / D-49 / D-51 / D-53 が
GC に委ねた事項の固定値と境界）:

- **物理削除を行う唯一の経路は `gc` ジョブ**（並列 1、dedup key `gc`、payload `{}`）。対象は 5 区分:
  | 区分 | 対象 | 判定 | 物理削除 |
  |---|---|---|---|
  | A | missing トラック | `missing_since <= now - retention` **かつ Library に実体が無い**（`stat` で再確認。あれば次のスキャンが復活させるので飛ばす。symlink 等 NotFound 以外のエラーも飛ばす） | 行のみ。CASCADE で `playlist_items` / `track_tags` / `derived_files` / RG / verifications |
  | B | missing アルバム | `missing_since <= now - retention` かつ構成トラック 0（A の削除後に判定） | 行のみ |
  | C | Archive の退避ファイル | `archived_files.state='held'` かつ `eligible_after <= now` | `Archive/<rel_path>` を unlink → `state='deleted'`。実体が既に無ければ warn して `deleted`。unlink がそれ以外で失敗したら `held` のまま次回 |
  | D | Derived の孤児 | `Derived/` 配下の実体で `derived_files` に `rel_path_key` が無いもの（A で行が消えた Derived もここで拾う）。`.spindle-tmp-*` と **mtime が 24 時間以内**の実体は除外 | unlink。空になったディレクトリも消す（root は残す） |
  | E | アートワーク孤児 | `artwork` 行: `albums.artwork_id` から参照されない。`thumbs/<hex>/`: 行の無い hex（**mtime が 24 時間以内**の dir は除外。スキャン Phase 5 が原画像を置いてから行を入れるまでの間を守る） | 行を DELETE、dir を再帰削除 |
  触らないもの: Archive 内の台帳に無いファイル、`Playlists/` の古い書き出し（D-53）、Library の
  同梱ファイル（D-43 の残課題）、`data/tmp`（起動時回収の領分）
- **判定（`gc::plan`）と実行（`gc::execute`）を分け、dry-run は `GET /api/gc/preview` が `plan` を同期で
  呼んで返す**（区分ごとの件数・バイト数と先頭 50 件のパス）。ジョブの結果を返す列が `jobs` に
  無く、「ジョブとしての dry-run」は結果をログでしか見られない。`POST /api/gc` はジョブの投入
  （未完了があれば 409 `duplicate`）
- **scan と GC は同じ名前付き排他 `library`（`job_mutexes`。マイグレーション 0009）を取り、取れた
  側だけが走る。** 取れなければ `Outcome::Requeue`（1 秒後に再試行）。check-then-requeue を両側に
  置くだけでは同時に claim されたとき両方が譲り合い続けるので、勝者を DB で一意に決める。排他は
  `track_locks` と同じくジョブの終端で自動的に解放され、持ち主が running でなくなれば奪える。
  起動時リカバリで全件消す。A の行削除がスキャン Phase 4 の commit と、E(dir) の削除が Phase 5 の
  原画像の配置と競合しないため
- **実行順と原子性**: A → B → E(行) を 1 トランザクション（削除の SQL でも `missing_since` /
  参照の無さを再確認する）→ C → D → E(dir)。ファイル削除は 1 件ずつ、失敗はログして続行し、
  区分ごとの件数・バイト数・失敗数を `info!` で出す（**削除件数のログは必須**）。24 時間の猶予は
  in-flight の書き込みと手作業で置いた実体の保護。Derived のサブディレクトリが読めなければその下
  だけ飛ばして続ける（root が読めなければ計画できず失敗）
- **計画と実行の間に状態が変わりうるので、物理削除の直前に 1 件ずつ再確認する**（計画は数秒〜
  数分前の読み取り。その間に巻き戻し・redo・スキャンの復活・transcode の claim・Phase 5 が動く）:
  - C: 行がまだ `held` で `eligible_after <= now`、`rel_path` が計画時と同じことを確認し、その台帳の
    トラック行が残っていれば**トラックのロック**（`track_locks`）を GC のジョブで取ってから unlink
    する（巻き戻しの normalize ジョブと同時に走らない。取れなければ今回は飛ばす）。`deleted` への
    遷移は `UPDATE … WHERE state = 'held'` の CAS。クラッシュで実体だけ消えて `held` が残った行は
    次回 NotFound として `deleted` に進む
  - D: その key を指す `derived_files` 行が無いことを確認して **transcode と同じ排他予約**
    （`derived_path_locks`。`track_id` は NULL。マイグレーション 0008 で NULL 可にした）を GC の
    ジョブで取ってから `stat` し直し、一覧時と inode / mtime が同じときだけ unlink、件ごとに解放する。
    transcode は予約無しに宛先へ書かない（D-51）ので、GC が持っている間は置き換えられない。running
    なジョブの予約があれば飛ばし、終わったジョブの残骸は奪う（`lock_path` の規則）
  - E(dir): その hex の `artwork` 行が無く（E(行) が参照の出現で skip されたとき・スキャンが行を
    入れたとき）、dir の mtime がまだ猶予を過ぎていることを確認してから消す
- **冪等**: 途中で落ちても次回が続きを拾う（行が消えて実体が残った Derived は D、`held` のまま実体が
  無い Archive は C で `deleted` に進む）。キャンセルは区分の境界と件ごとの進捗で見る
- **起動契機は backup と同じスケジューラ**（起動時と 10 分ごとに「最後の終端 `gc` から 24 時間」で
  due 判定）+ 手動 `POST /api/gc`。間隔は定数（`[gc]` に設定を足さない。`retention_days` が 30 日
  なので 1 日 1 回で足りる）
- **UI は作らない**（SPEC §12.6 の設定画面は骨格のみ。API とジョブで受け入れを満たす）

**理由**: 編集履歴（`edit_ops` / `archived_files` の `track_id`）は意図的に FK にしていないので、
行を消しても巻き戻しの根拠は残る。Derived は transcode が物理書き込みの前に `derived_files` の
行で期待パスを占有する（D-51）ので「行の無い実体」= 孤児と判定できる。

**却下**: ジョブとしての dry-run（結果の置き場が無い）。Archive の台帳に無いファイルの回収
（Archive は追記のみでスキャン対象外。何が置かれたか分からないものは消さない）。区分ごとに別
ジョブ（順序に依存がある: A → D、E 行 → E dir）。

**未決**: 設定画面からの起動（SPEC §12.6）。Library の同梱ファイルの回収（D-43）。

## D-57 FLAC 健全性チェックは検査結果を版付きで記録し、MD5 の補填は別タスクにする

**決定**（P1-5。仕様 SPEC §7.9 / §8 / §13 `[normalize]` が定めていない値と境界）:

- **`flaccheck` ジョブは読むだけ。** payload `{ track_id, audio_version }`（版付き。dedup key
  `flaccheck:<id>:<ver>`。ワーカーの stale ゲートで古い版は no-op、`track_locks` で同じトラックの
  書き手と直列化）、並列 CPU コア数。対象は `codec = 'flac'` で active。それ以外は no-op で `done`
- **手順**: root で開いた FD を `fstat` して DB の `(dev, inode)` と照合（不一致は書かずに `done`。
  次のスキャンで版が進めば再投入される）→ STREAMINFO の MD5 を読む（`media::fingerprint`。全ゼロは
  未設定）→ `flac -t -`（FD を stdin で渡す。タイムアウトは長さに比例、終了コード検査、stderr は
  ログ）→ 1 UPDATE で記録（`WHERE audio_version = ?` で版を再確認）
- **結果は `tracks` の列**（マイグレーション 0010）: `flac_check`（`ok` / `md5_missing` /
  `decode_error`）、`flac_checked_at`、`flac_check_version`（検査時の `audio_version`。現在値と違えば
  結果は古い。スキャナは触らない）、`flac_check_error`（stderr の末尾）。判定は `flac -t` の終了
  コード ≠ 0 → `decode_error`、0 で MD5 全ゼロ → `md5_missing`、0 で MD5 あり → `ok`
- **起動契機**: `POST /api/flaccheck { selection }`（RG と同型。selection の active な FLAC を
  track 単位で投入）と、`[normalize].flac_verify_on_import = true` ならスキャン完了時に
  「結果が無い・版が古い」FLAC を全件投入（Derived の追随と同じ場所）
- **一覧**: 行に `flac_check { status, checked_at, stale }`。固定フィルタ `flac_unchecked`（FLAC で
  結果が無いか古い）と `flac_error`（`decode_error`）。UI はバッジとサイドバーのフィルタだけで、
  起動ボタンは RG と同じく未着手
- **MD5 の補填（`flac_fix_missing_md5`）は P1-5b に切り出す。** 方式は再エンコードではなく
  **STREAMINFO の MD5 16 バイトだけを tmp + rename で書き換える**（デコードして PCM MD5 を計算し
  埋める。圧縮も音声も変わらず `audio_version` 据え置き、inode / mtime は追随）。編集バッチに
  乗せ（`edit_ops.kind` に `md5` を足すマイグレーションが要る）、旧値 = 全ゼロを `edits` に残して
  巻き戻せるようにする。今は実データに FLAC が無く、今後の FLAC は自前の `flac -8 --verify` と
  CD リップで MD5 が付くので急がない

**理由**: SPEC の「再エンコードして補填」は MD5 を付けることが目的で、`flac -8` の再圧縮は禁止事項
（圧縮レベルを揃える一括再エンコード）と紙一重。16 バイトの書き換えで目的を満たせる。検査は
読むだけなので編集履歴に乗せず、版で古さを表す。

**却下**: 検査結果を履歴表にする（一覧のフィルタとバッジには列が素直）。`flac -t` の stderr から
MD5 未設定を読む（版差で文言が変わる。STREAMINFO を直接読めばよい）。normalize の archive op を
FLAC → FLAC に広げて補填する（2,000 行の壊れやすい経路を同じパスへの置き換えに拡張するより、
専用の小さな op の方が安全）。

**未決**: P1-5b（補填）。UI の起動導線（RG と一緒に一括編集パネルへ）。

## D-58 UI は foobar2000 の配置に寄せ、ツリーの表示形式はパターンで定義し、絞り込みは album_ids で行う

**決定**（P1-12。SPEC §12 が定めていない配置・形式と、起動導線の置き場）:

- **配置は foobar2000（macOS、DUI）のユーザのレイアウトに寄せる。** ヘッダ（画面切替・検索・ジョブ要約）
  → プレイヤーバー（曲名 / ◀◀ ▶ ▶▶ / シーク / 音量 / RG）→ 本体は左右分割。左はツリー + プレイリスト +
  固定フィルタ、その下にアルバムアート（選択行のアルバム、無ければ再生中）。右は上にプロパティ領域
  （タブ: プロパティ / 一括編集 / 操作）、下に表。左右・上下の境界はドラッグで可変、localStorage に
  永続化。配色は現状のライトのまま（ダークは別タスク。CSS 変数は `:root` に集約済み）。
  表の既定列は `Playing | Artist/album | # | Title / artist | Duration`
- **ツリーの表示形式は foobar の Album List と同じくパターン文字列で定義する。** `|` が階層、
  `%field%` が差し込み、`[ … ]` は中のフィールドが全部空なら省く（角括弧は条件記号で表示しない。
  foobar の title formatting と同じ）。フィールドはアルバム単位の値（`category` `albumartist` `album`
  `date` `year` `original_date` `edition` `disc_count` `rel_dir` `folder`）に限る（`GET /api/albums` の
  行から組む。トラックのタグは無い）。`%folder%` だけは特別で、パターン全体がそれだけのときに
  `rel_dir` の階層をそのまま展開する（深さはアルバムごとに違う。他のフィールドや文字と混ぜると
  構文エラー）。Library 直下（`rel_dir` が空）のアルバムは「（なし）」の 1 ノードに載せる（消すと
  All Music の件数と合わない）。組み込み 5 形式はプリセット: by folder structure `%folder%`（既定。
  リハーサル DB はカテゴリ未推定で by category が 1 本になるため）/ by category
  `%category%|%albumartist%|%album%` / by artist `%albumartist%|%album%` / by album
  `%album% — %albumartist%` / by year `%year%|%album%`。ユーザ定義は名前 + パターンで localStorage に
  保存。空になった階層は「（なし）」で末尾、並びは日本語 collator、件数はトラック数の合計
- **ノードの絞り込みは配下の album id の集合。** `GET /api/tracks` の filter に `album_ids`（配列）を
  足し、SQL は `t.album_id IN (SELECT value FROM json_each(?))` で 1 パラメータにバインドする（件数の
  上限なし。ホワイトリスト経由）。組み込み形式もこの経路に統一し、ルートは filter 無し。selection の
  filter 形にもそのまま乗る
- **プロパティタブは Selection Properties の再現。** Metadata（標準タグ + 任意タグ全部）と Location /
  General（ファイル・技術情報・RG・検証・FLAC 検査・Derived）。複数選択は共通値、異なれば
  `<複数の値>`（先頭 50 件で判定）。Metadata の値のダブルクリック編集は選択全体への 1 op の一括編集
  （インライン編集と同じ preview → apply）。`GET /api/tracks/:id` のセッション向け応答に `detail`
  （tags / size / mtime / sample_rate / bit_depth / channels / bitrate / audio_md5 / original_codec /
  added_at）を足す。CIDR 経由の限定フィールドは変えない
- **起動導線は操作タブ。** リネーム / 正規化（preview の old → new と衝突理由を一覧 → 適用。pending の
  409 は既存の 2 択）、RG 解析 / RG 書き込み / FLAC 検査（投入件数・重複・skip を通知）、
  プレイリストへ追加。既存 API のみ。ジョブの完了は行の値（RG / FLAC 検査 / Derived）を変えるが
  `library` イベントは scan だけが流す（SPEC §9）ので、クライアントは `job` の done / failed で
  表示ページとプロパティの詳細を取り直す（一括 transcode で数千回にならないよう 3 秒に 1 回に
  間引く）
- **ジョブ画面（SPEC §12.5）と設定画面（§12.6）を骨格から実装する。** ジョブ画面は種別ごとの並列度
  （`GET /api/jobs` に `concurrency` を足す）と待ち行列の表、一覧（実行中・待ち / 失敗・取り消し /
  すべて、種別で絞る）、`edit_batch_id` から履歴画面のそのバッチを開く。プロパティ領域は一覧のとき
  だけ出す（ジョブ / 履歴 / 設定では表の代わりに画面が入るので選択の意味が無い）。設定画面の
  `GET /api/config` は `Config` が保持する原文（`source`）をそのまま返し、画面からは書き換えない
  （変更はファイルを編集して再起動）。`GET /api/archive` は台帳に `batch_id`（退避した op のバッチ）を
  添え、保持中の行は「バッチ #n を巻き戻す」で履歴画面のそのバッチを開く（復元の API は別に作らない。
  SPEC §7.4 のとおり巻き戻しが復元）。 設定は `GET /api/config`
  （読み込んだ `config.toml` の原文。秘密は config に無い）、再スキャン / deep scan、GC の preview →
  実行、`GET /api/archive`（`archived_files` の一覧。復元は対応バッチの履歴から巻き戻し）

**理由**: ユーザが日常的に使っている foobar の配置に寄せれば、移行後の操作の学び直しが要らない。
ツリーの形式をパターンにすると組み込みとユーザ定義が同じコードで済み、絞り込みを album_ids に
統一すると形式ごとのフィルタの写像が要らない。

**却下**: ノードごとに `category` / `albumartist` の固定フィルタへ写す（合成階層に写せない）。
ツリーのパターンにトラックのタグを許す（アルバム単位で持っていない。必要なら `GET /api/albums` の
拡張で足す）。ダークテーマ（別タスク）。

**未決**: `by genre`（albums にジャンルが無い）。ダークテーマ。

## D-59 MD5 の補填は md5 op の編集バッチにし、tagwrite ジョブで反映する

**決定**（P1-5b。D-57 が「別タスク」とした補填の具体化）:

- **op は `kind = 'md5'`、edits は `key = 'audio_md5'`**（値は 32 桁の hex。全ゼロ = 未設定）。
  `old_value` は記録時の STREAMINFO の値（補填では全ゼロ）、`new_value` は記録時 null で、反映時に
  デコードして計算した値を書き戻す。巻き戻しは逆向きの md5 op（old = 計算値、new = 全ゼロ。
  `new_value` があれば計算せずそれを書く）。DB は先行更新しない（overlay 無し。archive op と同じ）
- **反映は STREAMINFO の 16 バイトだけを tmp + rename で書き換える。** 補填は 2 段階: デコードして
  計算した値をまず `edits.new_value` へ耐久化し（別トランザクション）、それから書く。rename 後・
  DB 確定前にプロセスが落ちても、再実行で「事前条件は外れているがファイルは新値」と分かり applied に
  確定できる（冪等・巻き戻し可）。事前条件は実体（dev / inode / size / mtime / ctime）と記録時の
  MD5 値。どちらか外れていて新値でもなければ `skipped_conflict`（外部で補填・差し替え済み。
  ファイルが正で、DB を現在値に揃える）。FLAC として読めない・デコードできないは
  `failed`（再試行しても直らない）。同じトランザクションで `audio_md5`（全ゼロなら NULL）と
  `flac_check`（値あり → `ok`、全ゼロ → `md5_missing`）を揃え、`audio_version` / `tag_version` は
  据え置く（音声もタグも変わらない）
- **ジョブは tagwrite を再利用する。** 契約（そのトラックの pending op をファイルへ反映する。
  `track_locks`・stale ゲート・再試行・最終失敗で op を閉じる）が同じ。`Editor::apply_op` が
  op の kind で分岐する。新しい job type を足すと `jobs` 表（4 表から参照）も作り直しになる。
  payload は `{ track_id, op_id, batch_id, unversioned: true }` で dedup key は
  `tagwrite:<id>:md5:<op_id>`。`unversioned` は jobs 層の明示的な契約（`db::jobs::UNVERSIONED_KEY`）で、
  版付き種別でも stale 判定をせず `track_locks` だけ取る（md5 op は tags overlay を持たず版も進めない
  ので、補填が queued の間に外部のタグ変更で `tag_version` が進んでも捨ててはいけない）。key を op ごとに
  するのは、tags と同じ key だと元ジョブが running のうちに巻き戻したとき逆 op のジョブが Duplicate に
  なって走らないため
- **マイグレーション 0011 は `edit_ops` を作り直す。** SQLite は CHECK を ALTER できず、`edit_ops` は
  `edits`（CASCADE）と `archived_files`（SET NULL）から参照されているので、FK を切らずに DROP すると
  履歴が消える。runner に「FK を切って適用する版」（`FOREIGN_KEYS_OFF`）を持たせ、トランザクションの
  外で `foreign_keys=OFF` → 適用 → commit 前に `foreign_key_check` → `ON` に戻す（SQLite の 12 手順）
- **API は `POST /api/md5fill { selection, description?, skip_pending? }`**（RG 書き込みと同型。preview は
  無い）。対象は selection の active な FLAC で `flac_check = 'md5_missing'`。`[normalize].flac_fix_missing_md5 = false`
  なら 409 `md5_fill_disabled`。UI は操作タブの「MD5 を補填」

**理由**: 補填は既存の編集バッチの性質（事前条件 → tmp + rename → DB 追随、巻き戻し）をそのまま
使えるので、専用の小さな op にするのが最も安全。

**却下**: `kind = 'tags'` に特別なキーで載せる（履歴の種別が嘘になる）。新 job type `md5fill`
（`jobs` 表の作り直しを伴う）。反映時に `flac -t` で再検査する（全部デコードして計算した時点で
検査と同じことをしている）。


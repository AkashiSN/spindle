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

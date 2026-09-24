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
配置前に MusicBrainz Release ID または DiscID で同一リリース判定を行う（→ D-67 追記 3 で DiscID を外した。
DiscID は 1 枚ごとの値で、複数枚組のリリースの鍵にならない）。
**同名 ≠ 同一リリース**であり、パス衝突をマージ扱いにしてはならない。

---

## D-8 Derived は可逆のみ、配布ビューで解決

**決定**: `delivery_path = 可逆 ? Derived : Library`（`delivery` ビュー）。

**理由**: 非可逆音源を再エンコードすると世代劣化する。Derived にコピーすると
二重管理になる。ビューで解決すれば m3u8 生成側は Derived の有無を意識しない。

**例外**: 容量逼迫時のみトラック単位で `force_transcode` を許可。
ただし元が 256kbps 以上の場合のみ。

**追記（2026-09-20。P4-8）**: Apple 向けの `aac` 系統（D-75）だけは非可逆原本（opus / ogg / mp3）も
AAC へ変換する。ミュージック.app が Opus を読めず、配布ビューで原本へ倒しても Apple 側で再生できない
ため、世代劣化を承知で全曲を揃える。`opus` 系統と配布ビュー（Android / Web）はこの決定のまま。
原本が AAC でも同じ経路で再エンコードする（焼き込みのため）。

---

## D-9 Opus 128kbps VBR

**決定**: Derived は Opus 128k VBR。

**理由**: 事実上透過で、可逆 200GB 分でも約 25GB。96k はポータブルでは十分だが
有線イヤホンで差が出る境界。160k 以上は体感差がほぼなく容量だけ増える。

**補足**: Derived は再生成可能なので、この決定は低リスクであり後から変えられる。

**追記（2026-09-20。P4-7）**: `opus` 系統の既定を **256 kbps** に上げる（`--vbr --music` は当初から）。
可逆 237 GB（7,570 本）で 29 GB → 約 58 GB。あわせて系統ごとの設定 `[encode.derived.<variant>]` に
改め、**行の `audio_profile`（codec / bitrate / サンプルレート規則 / 焼き込み方式の版を並べた文字列）が
設定と食い違えば再エンコード**の判定を足す（これが無いと設定を変えても音声版が同じ既存の Derived は
作り直されない。D-75）。既存の 128k（`opus:128:v1`）はこの判定で全件作り直す。

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
（→ D-67 追記 3 で `discid` の列ごと外し、`mb_release_id` だけになった）
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
  記録を試み、それも駄目なら次回起動のリカバリに任せる（**追記 2026-09-20**: 記録の前に
  バックオフ付きで再試行し、駄目なら稼働中の回収に任せる。D-76）
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
  date / original_date / mb_release_id / disc_count）は構成トラックの**最頻値**（`discid` は D-67 追記 3 で外した）
  （同数なら文字列順で先）。`edition` は P0-6 では設定しない
- **album 照合の候補順**は MBID が 1 件一致（DiscID は D-67 追記 3 で外した）→ 構成トラックの過半数が直前まで属していた
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
  100,000。`number` は `start + 位置`、`pad` 桁で 0 埋め（0〜6。範囲外は 400）。
  **`set_rows`**（2026-09-21、P4-14）は `rows: { "<track_id>": 値 }` で行ごとに違う固定値を書く。
  値の規則は `set` と同じ、`rows` に無い行は触らない（preview で「変更なし」）、1〜10,000 行。
  一括編集の UI には出さず、スクリプトからの補填（`scripts/backfill_source_url.py` の `SOURCE_URL`）に
  使う。行ごとに別バッチにすると履歴が汚れ巻き戻しも 1 件ずつになるので、1 op で済ませる
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
- **衝突降格の単位はリリース**: `mb:<MUSICBRAINZ_ALBUMID>` → `album:<album_id>`（`disc:<DISCID>` は D-67 追記 3 で外した）の
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
  同梱ファイルの扱い（追随・GC）は未決（残課題）。埋め込み統一（D-49 / D-60）で Library の同梱
  ファイルは実データで png 1 本しか無く、Library に `rip.log` / `disc.cue` を置き始める P2-8 で決める
  → D-67 で決めた: rename ジョブが commit 後に、album 全体の移動で active な行が無くなった旧ディレクトリの
  既知の名前のファイルを宛先へ移し、空なら rmdir する（op としては記録しない）
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

**追記（P1-13）**: 外部で音声が差し替わって `audio_version` が進んだ行は、スキャナ（Phase 4 の
`apply_content`）と tagwrite の overlay 解消（`sync_track_to_file`）の両方で解析値を捨てる
（`db::replaygain::reset_analysis`: `rg_*` と `rg_scanned_at` / `rg_written_at` を NULL）。古い値を
Derived や再生に使わないため。album の他のトラックの `rg_album_*` は次の album 解析で揃う。

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

**追記（P1-13）**: スキャナは外部のタグ変更を取り込んだ行（`tag_hash` が変わった。pending の tags op
がある行は overlay を守るので除く）で `sync_written_at` を呼び、外部ツールが RG タグを消せば
`rg_written_at` を NULL に、解析値と一致する値を書けば `now` にする（`Scanner::with_replaygain_reference`
で基準を受ける）。`tag_version` が進むので Derived の追随（P1-10）が RG タグを `R128_*` へ変換して
埋めるのは P1-10 側の責務。

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

**未決**: 書き側は D-60（埋め込み統一。cover ファイルは書かず、抽出も作らない）。
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
  **画像はトラック自身の `artwork_id`、無ければ album の `artwork_id` の `768.webp`**（D-61 で改訂。
  P1-3 のキャッシュ。無ければ thumbnail ジョブと同じ変換で作る。原画像も無ければ画像なしで作り、
  次のスキャンが原画像を復旧したときに `src_artwork_id` の不一致で書き直される）を `image/webp` の
  front cover 1 枚として埋める
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

**追記（2026-09-20。P4-7 / P4-8）**: D-75 が次を上書きする。`transcode` の処理単位は
`(track_id, variant)` で dedup キーは `transcode:<track_id>:<variant>:<audio_version>`（SPEC §8）、
`derived_files` は系統ごとに 1 行、`aac` 系統は非可逆も対象（D-8 追記）で
RG の解析世代の差分は Retag でなく Encode、`delivery` は `opus` 系統に固定、`has_derived` は `opus` の
行の有無。判定の入力に `audio_profile` / `tag_profile`（設定の世代）が加わる。それ以外（判定を
ハンドラが現在値から下す、退避経路、投入経路）はこの決定のまま。

**閉じた未決**（2026-09-18、リハーサル環境 9,098 トラックの実データで需要を確認。出てきたら再開）:
- マルチチャンネルのダウンミックス（SPEC §7.6）: 全トラックが 2ch。DSD / SACD も非対応方針で、
  マルチチャンネルが入る経路が無い。作るなら `tracks` に列を足し、対象判定と `-ac 2`、RG 集計の扱いを決める
- 非可逆の `force_transcode`（SPEC §7.6、D-15）: 非可逆は Opus 1,512 / MP3 14 / AAC 2。Opus は変換の
  意味が無く、残りは 16 本で容量逼迫の解決にならない
- Derived 側の `cover.jpg` ミラー: 全件に WebP を埋め込み済み（トラック単位の画像は D-61）。Library は
  D-49 で同梱ファイルを書かない方針なので、Derived にだけ書くと方針が割れる。埋め込みを読まない
  Android プレイヤーが出てきたら再検討

**未決**: `transcode` の並列度の設定化。進捗表示（ジョブ画面の件数で代替）。transcode が**実行中**に
scan が同じトラックの版を進めた場合、その scan の一括投入は dedup で落ちる（ハンドラが記録するのは
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
  モードは off / track / album の 3 択（foobar2000 と同じ。localStorage `player.rgMode`）。album は
  `album_gain` / `album_peak` を使い、`album_gain` が無い（単独曲・未集計）行は track に倒す
  （foobar の "album, fallback to track"）。peak も同様に `album_peak` → `track_peak`
- **UI**: 表の行先頭に ▶（ホバーで表示）を置き、その行から**表示中の順序で連続再生**する。
  下部バー左に ▶ / ‖、シーク、経過 / 総時間、音量、RG モード（off / track / album）、曲名。`transcode=opus` を要求しても
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

**閉じた未決**（2026-09-18）: Safari 向けの AAC 変換（`transcode=aac`）。Safari 18.4（2025-03。
iOS / iPadOS / macOS）以降は Ogg Opus をネイティブ再生でき、FLAC / ALAC はもともと再生できる。
実機の Safari で Derived の Opus と Opus 原本の再生を確認したので作らない。Opus 不可のブラウザが
必要になったら ffmpeg の aac エンコーダ → ADTS で足せる。
ハイレゾのサンプルレート変換。可逆は既定で Derived の Opus（48 kHz）を
再生するので、96 kHz（実データで 64 本）がそのまま流れるのは「原本」を選んだときだけで、ブラウザは
そのまま再生できる。原本を選んだ人にリサンプルを掛けるのは逆なので作らない。SPEC §11 の
「クライアント設定で変換可」は既定の Derived 再生がその役割。

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

`HAS` の語境界は foobar 実機と突き合わせ済み（部分一致で一致。D-55）。foobar クエリへの変換は D-55。

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

**実機との突き合わせ**（2026-09-19、Windows の foobar2000 で確認。変換器の変更は不要）:
- `%title% HAS ab` は **部分一致**（`xaby` に当たる）。spindle の `LIKE %v%` と同じ
- 値の二重引用符は挙動を変えない（`HAS "ab"` は `HAS ab` と同じ）。**単一引用符は文字通りの値になり
  一致しなくなる**ので、変換器は値を囲むのに `"` だけを使う（`'` で囲まない）
- `%__bitspersample% MISSING` / `PRESENT` は技術情報フィールドにも効く（非可逆 / 可逆の件数と一致）
- `%codec% IS flac` も期待どおり

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
  | E | アートワーク孤児 | `artwork` 行: `albums.artwork_id` から参照されず、`edits` の `PICTURE` 値（旧 / 新）にも現れない（D-60。巻き戻しに要る）。`thumbs/<hex>/` の mtime が 24 時間以内なら行も残す（アップロード直後・参照前を守る）。`thumbs/<hex>/`: 行の無い hex（**mtime が 24 時間以内**の dir は除外。スキャン Phase 5 が原画像を置いてから行を入れるまでの間を守る） | 行を DELETE、dir を再帰削除 |
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

**追記**（2026-09-20、上部の圧縮）: 本体の上に 4 本（上部ナビ / プレイヤー / 右パネルのタブ / 「選択 N 件」）
あって狭かったので 2 本にした。上部ナビとプレイヤーを 1 本の上部バーに統合し、ジョブ要約のテキスト
（実行中 · 反映待ち · 失敗）は「ジョブ」ボタンの件数バッジ + 失敗の赤点 + ツールチップに畳む（画面切替の
「ジョブ」と同じ内容を 2 か所に出していた）。たまにしか開かない履歴 / 設定とログアウトは ☰ メニューへ。
検索ボックスは上部バーから左サイドバーの先頭（スクロールしない）へ（foobar2000 の「ツリーの下」は
サイドバー全体がスクロールする spindle では隠れるので先頭にした）。右パネルは「選択 N 件」をタブ行の
右端に寄せ、フィルタ形の選択の説明はツールチップに移した。

**追記**（2026-09-20、P4-6）: アルバム画面は `GET /api/albums` の全件を出していてツリーの選択・プレイリスト・
検索語を見ていなかった。プレイリストは album の情報だけでは絞れない（中のトラックが属する album が要る）ので、
クライアントで絞るのではなく `GET /api/albums?filter=` にトラック一覧と同じ JSON フィルタを受け、「一致する
active なトラックを 1 本以上持つ album」を返す。アルバム画面は「いまのトラック一覧と同じ絞り込みのアルバム」に
なる（検索語でも絞られる）。WHERE の生成はトラック一覧と共有し、列名はホワイトリスト・値はバインドのまま。
実装で決めた点: `filter` が空・`{}` は全件（active なトラックの有無で絞らない。フィルタ無しと同じ）、指定が
あれば `a.id IN (SELECT t.album_id FROM tracks t WHERE t.missing_since IS NULL AND (<filter_where>))`（`missing`
フラグと組み合わせると空になるが、「表に出るトラックの album」の定義に従う）。web はツリー用の全件と画面用の
絞り込みを別に持ち（`useAlbums(enabled, filterParam, wantFiltered)`）、絞り込みはアルバム画面を表示中で
`filterParam` が空でないときだけ取る（ツリーのクリックごとに数千件を取り直さない）。応答は最新の要求だけを
採用し、結果は「どのフィルタに対する完了か」（成功 / 失敗とも）を持つ。現在のフィルタの完了が無い間（応答待ち）
と失敗時は前のフィルタの結果を出さず全件を出し、失敗は全件の成否とは別に見せる。同じフィルタの取り直し
（library / job イベント）の間は前回の結果を出し、届いたら差し替える。

**追記（2026-09-21。P4-10 ダークテーマ）**: 配色をライト / ダークで切り替える。直書きの色はすべて
`index.css` の `:root` の変数に集約し（バッジは色相ごとに `--badge-<hue>-bg` / `-fg` の対、影・モーダル
背景・行の状態色・欠落・スケルトンも変数）、ダークは `:root[data-theme='dark']` の 1 ブロックで同じ変数を
全部差し替える（`color-scheme` も切り替え、フォーム部品とスクロールバーを追随させる）。セレクタ側は変数しか
見ないことを `web/src/lib/theme.test.ts` が固定する（直書きの色が 2 ブロック以外に無い、両ブロックの変数
集合が一致）。設定は `'light' / 'dark' / 'system'`（既定 `system` = OS の `prefers-color-scheme` に追随し
変化も購読）で localStorage の `spindle:theme` に置き（`lib/theme.ts`、`hooks/useTheme.ts`）、解決結果を
`<html data-theme>` に書く。初回描画の白飛びを防ぐため `index.html` のインラインスクリプトが React より先に
同じ規則で `data-theme` を付ける。切り替えは設定画面の「表示」節（ラジオ 3 つ）。上部バーには置かない
（☰ は履歴 / 設定 / ログアウトで足りる）。`@media (prefers-color-scheme)` を CSS に書かない（保存値が OS より
優先で、判定は JS 側の 1 か所に置く）。ライトの値は従来と同じだが 2 箇所だけ揃えた: 行の再生ボタン
（`.td-sel .play-row`）は未定義変数のフォールバック `#888` だったのを `--muted`（#6b7280）に、編集セルの
不正値の枠は単発の `#d33` を `--error` に。影の 0.15 も `--shadow`（0.12）に統一。ダークの淡色（欠落行・
未検証バッジ・欠落バッジの文字）は本文 13px / バッジ 11px で、選択行（`--accent-bg`）や反映待ち + 選択
（`--pending-selected-bg`）の背景に載っても 4.5:1 を切らないよう #c4c9d1 / #9ca3af にし、反映待ち + 選択の
背景は `--muted` の文字が 4.5:1 を保つ #1c3358 にした（codex のレビューで計測。欠落 / 反映待ちの行は
選択できるので、状態の組み合わせで確認する）。

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


## D-60 アートワークの書き側は埋め込み統一。差し替えは `PICTURE` の tags op で、cover ファイルは書かない

**決定**（P1-3 書き側。D-49 の「未決」の具体化）:

- **spindle は同梱の cover ファイルを書かない。画像はトラックへの埋め込みに統一する。** ライブラリは
  既に全 album が埋め込みで（リハーサル環境 721 album 全部が `origin='embedded'`、同梱画像 0）、
  album で共通の画像と、トラックごとに画像が違う album（神椿系）が混在しているため、埋め込みに
  統一している。cover ファイルを併用すると正が 2 つになり（cover を置いた後の埋め込みが古いまま
  残る、トラックごとに画像が違う album に誰かが cover.jpg を置くと D-49 の規則でそれが勝つ）、
  読み側の規則も書き側の機構も 2 系統になる。読み側の同梱画像優先（D-49）は外部が置いたものを拾う
  ためだけに残す。埋め込み → cover ファイルの「抽出」は作らない
- **差し替えは `kind='tags'` の op で、キー `PICTURE` を変更する。** 値は `tag_hash` と同じ
  `<mime>:<sha256hex>` の配列（無しは null）。`track_tags` は既にこの形で `PICTURE` を持つ
  （スキャナが `TagSet` ごと保存する）ので、旧値・overlay・`tag_version`・差分・書き戻し確認
  （`file_matches_new_values`）・巻き戻し（`revert_tags`）は既存のまま効く。新しい op 種別も
  マイグレーションも要らない。ユーザの JSON タグ操作（`tagops::parse_ops`）は `PICTURE` を拒否した
  ままで、画像 op は `Editor::prepare_picture(description, track_ids, sha256)` だけが作る
  （`prepare_tags_with` に「`PICTURE` を `[<mime>:<hex>]` に置く」評価器を渡す）
- **差し替えは「全画像を捨てて、上げた 1 枚を front cover として入れる」。** 裏ジャケ等の別種別を
  残す案は、lofty の画像種別が形式ごとに揺れる（MP4 は種別を持たない）ので確実に動かず、
  ライブラリの実態も 1 枚運用なので採らない。既に同じ 1 枚だけを持つ行は差分なし（unchanged）
- **画像の実体は `ArtworkStore`（`thumbs/<hex>/orig.<ext>`）と `artwork` 行で持つ。** アップロード
  （`POST /api/artwork/upload`。生のバイト列、`Content-Type: image/*`、上限 32 MiB = `MAX_COVER_BYTES`。
  形式と寸法はヘッダで判別し JPEG / PNG / WebP 以外は 400）が新画像を置き、`thumbnail` ジョブを
  投入する（UI のプレビューと album の解決に使う）。**tagwrite は書く前に、捨てる旧画像を同じ
  store へ退避する**（`stage_tags` が `PICTURE` の変更を含む op で `read_parts` により実体ごと読む。
  退避できなければ `failed` で何も書かない。`artwork` 行は `Staged::Written` で持ち帰り、finish_op
  と同じトランザクションで upsert）。新画像のバイト列が store に無ければ `failed`（「画像が
  キャッシュに無い」）。`AlreadyMatches`（外部が先に同じ画像を書いていた）は退避対象が無いので、
  その op の巻き戻しは「画像がキャッシュに無い」で failed になり得る（正直にそう記録する）
- **`write_tag_changes` は `pictures: Option<Vec<Picture>>` を取り、`Some` なら形式ごとに全画像を
  除去して挿入する**（FLAC は PICTURE ブロック、Opus / Vorbis は VorbisComments、MP4 / MP3 等は
  generic Tag）。`None` なら画像に触らない（既存のタグ編集はこれ）
- **GC 区分 E は `edits` が参照する画像を回収しない。** `key='PICTURE'` の旧 / 新値に現れる hex の
  `artwork` 行と `thumbs/<hex>/` は、履歴が残る限り巻き戻しに要る（画像は数百 KB なので履歴と同じ
  寿命で良い）。加えて、アップロード直後・参照前の行が消えないよう、E(行) にも dir と同じ
  **24 時間の猶予**を付ける（`thumbs/<hex>/` の mtime が猶予内なら行も消さない）。既にある画像の
  再アップロードは `put_original` が dir を touch して猶予を数え直し、E(行) は計画の後・削除の直前にも
  書き込みコネクションを持ったまま猶予を再確認する（アップロードは touch → 同じ writer で upsert の
  順なので、touch が先なら残り、削除が先なら upsert が行を作り直す）
- **applied のとき、そのトラックの album を `mark_unresolved` して増分スキャンを投入する**（dedup。
  既に走っていれば予約が残り次回で拾う）。Phase 5 が D-49 の規則で album の絵を解決し直す。Derived は
  `tag_version` が進むので既存の追随で再タグが走り、album の絵が変われば `src_artwork_id` の不一致で
  もう 1 回走る（二重だが正しい）。トラック自身の画像を Derived に埋める・再生画面に出すのは
  P1-3c（`tracks.artwork_id`、D-51 の改訂）
- **API は `POST /api/artwork/embed { selection, sha256, description?, skip_pending? }`**（RG 書き込み・
  MD5 補填と同型。preview は無い）。対象は selection の active 全行（形式は問わない）。`sha256` の
  `artwork` 行か原画像が無ければ 404 `artwork_not_found`。UI は操作タブの「アートワーク」節
  （ファイル選択 → アップロード → 256px のプレビュー → 「選択 N 件の埋め込み画像を差し替え」）。
  履歴画面の `PICTURE` 値はサムネイルで出す

**理由**: 埋め込み統一なら書き側は既存のタグ編集バッチに 1 キー足すだけで、事前条件・overlay・
巻き戻し・リカバリ・stale ゲートの全部が流用できる。cover ファイルの実利は「差し替えが軽い
（FLAC を書き直さない）」と「Explorer のフォルダサムネイル」だけで、差し替えは日常操作ではない
（たまに 1 album 数百 MB の書き直しなら許容。CLAUDE.md が禁じるのは全件再エンコードの規模）。

**却下**: cover ファイルの op（`kind='cover'`）を album の先頭トラックを「担い手」にして記録する
（ディレクトリ単位の巻き戻し、cover と埋め込みの二重管理、GC との調停が丸ごと増える）。
`edit_ops` に `album_id` を足して `track_id` を nullable にする（`edit_ops` の作り直しに加え、
ロック・pending 一意性・履歴画面・リカバリの全部が「トラック or album」の分岐を持つ）。
`kind='picture'` の専用 op（`tags` op と同じ事前条件・overlay・巻き戻しを二重に持つことになる。
md5 op が専用なのは overlay も版も持たないから）。front cover だけ置き換えて他の種別を残す（上記）。
古い画像を退避せず巻き戻し不可にする（不変条件 4）。

**未決**: P1-3c（トラック単位のアートワーク）。アップロード画像の再エンコード（大きすぎる画像を
縮めて埋める）は要望があれば。

## D-61 トラックは自身の埋め込み画像を持ち、Derived と再生表示はそれを優先する

**決定**（P1-3c。D-60 の未決。トラックごとに画像が違う album 向け）:

- **`tracks.artwork_id`（マイグレーション 0012。`artwork` への FK、`ON DELETE SET NULL`）はそのトラック
  自身の埋め込み画像**（`pick_embedded`: front cover → 無ければ最初の 1 枚）。無ければ NULL。埋めるのは
  ファイルの画像を実体ごと読む 2 か所: スキャナ Phase 3（`read_entry` が `read_audio_file_with_pictures`
  で読み、選んだ 1 枚を `ArtworkStore` へ置く。既に同じ長さの原画像があれば stat だけで済ませる）と
  tagwrite（`stage_tags` の書き戻し確認と外部変更の読み取り。store は `Editor::with_artwork`）。
  `TrackContent.picture` は `Unread` / `Absent` / `Found` / `Failed` の 4 値で、`insert_track` /
  `update_content` が同じトランザクションで反映する: `Unread`（読んでいない・store が無い）は何も
  触らない、`Absent` は `artwork_id = NULL`、`Found` は `artwork` 行を upsert して `artwork_id` に
  する（どちらも `artwork_dirty = 0`）、`Failed`（キャッシュへ置けない）は `artwork_id` を据え置いて
  `artwork_dirty = 1`（次項）。normalize の経路は `Unread`（画像は変わらない）
- **キャッシュへ置けない（store の I/O 失敗）は「画像なし」と区別し、`tracks.artwork_dirty`（0012）で
  再試行する。** `register_track_picture` は `Err` を返し、`TrackContent.picture` は `Failed`: `artwork_id`
  は据え置き、`artwork_dirty = 1` を立てる。スキャナは Phase 2 で `artwork_dirty` の行を（物理属性が
  同じでも）読み直す対象に含め、`Found` / `Absent` を記録できたら 0 に戻す。物理属性に依らない印なのは、
  deep で変更なしの行を検証して失敗した場合や、tagwrite が conflict / cancel で外部の画像を読んで
  物理属性を現在値に揃えた場合は、stat では再試行の契機が残らないため。`Absent` に潰すと DB が NULL の
  まま固定される
- **deep scan はトラックの画像もハッシュを照合して置き直す。** 増分は同じ長さの原画像があれば stat だけ
  （同じ画像を持つ数千トラックで実体を読み直さない）だが、同じ長さの破損は album の絵に採用されない
  画像（同梱 cover のある album の 2 曲目以降）だと Phase 5 の `put_original` に届かないので、deep で
  `register_track_picture(verify = true)` が直す
- **tagwrite で画像が変わったときの追随は `sync_track_to_file` に集約する。** ファイルの現在値を DB に
  揃えた直後に `artwork_id` の前後を比べ、変わっていれば album の再解決を予約して増分スキャンを投入し、
  サムネイルが無ければ thumbnail ジョブを投入する。Applied / AlreadyMatches / Conflict / Failed / cancel
  （`close_op`）の全経路が通る（外部の画像変更を conflict で読んだときも物理属性を現在値に揃えるので、
  ここで予約しないと次の増分スキャンには「変更なし」と見えて album の絵が古いまま固定される）。
  Phase 4 が pending の tags op のために `update_content` を飛ばした行も、その op の反映・conflict・
  cancel のいずれかでここを通る
- **既存行は deep scan 1 回で埋まる。** 変更なしの増分は最速パスでファイルを読まないので NULL のまま。
  NULL の間は Derived も表示も album の絵へ倒れるので壊れない。専用のバックフィルジョブは作らない
  （リハーサル環境はリリース時に再移行する）
- **album の絵（D-49）は変えない。** グリッドと album 単位の表示は同梱画像 → 最初のトラックの埋め込み
  のまま。Phase 5 が `tracks.artwork_id` でファイルの再読みを省く簡略化は別の改善
- **Derived に埋める画像は `COALESCE(tracks.artwork_id, albums.artwork_id)`**（D-51 の「album の
  `artwork_id`」を改訂）。`load_target` / `enqueue_all_stale` のクエリだけ変わり、`plan` と
  `src_artwork_id`（実際に埋めた id）の意味はそのまま。album 共通の album は同じハッシュ → 同じ id
  なので再タグは走らない
- **GC 区分 E の「参照されている」に `tracks.artwork_id` を足す。** 忘れると FK SET NULL で消えた
  `artwork_id` が増分では復旧しない
- **API / UI**: `TrackRow` と `GET /api/tracks/:id` に `artwork_hash`（トラック自身。無ければ null）。
  左下のアートワークは選択行（先頭）または再生中トラックの自身の画像 → 無ければ album の画像。
  `AlbumGrid` は変えない

**理由**: 埋め込み統一（D-60）のライブラリでは、トラックの絵はそのトラックの埋め込みが正。album 単位の
解決（D-49）だけだと、トラックごとに画像が違う album で全曲が 1 曲目の絵になる（再生表示と Derived）。
スキャナはもともと Phase 3 で画像を含むタグを読んでいるので、選んだ 1 枚をハッシュアドレスの store へ
置くだけで済み、同じ画像を持つ数千トラックでも実体は 1 つ。

**却下**: Phase 5 でトラックごとに画像を解決し直す（Phase 5 は album 単位の予約で回っており、トラック
全件を再読みすることになる）。`track_tags` の `PICTURE` 値から `artwork_id` を導く（値はハッシュだけで
実体が store に無いことがあり、front cover の種別も持たない）。バックフィルジョブ（deep scan で足りる）。
Derived で album の絵を優先する（トラックごとに違う album で目的を果たせない）。

**未決**: Phase 5 の簡略化（上記）。

## D-62 dev 番号は識別子の一部として永続化しない（dev の付け替えは「変更なし」）

**決定**（2026-09-19。TrueNAS 実機のホスト再起動で起動時スキャンが全件を読み直した事故から）:

- **同一性解決の inode 段に「dev の付け替え」を足す。** 同じ `(dev, inode)` の候補が無いとき、別の
  dev に同じ inode を持つ行が**ちょうど 1 つ**あり、`size` / `mtime_ns` / `ctime_ns` が**すべて**一致
  すれば同じ実体とみなす。`changed = false`（タグもフィンガープリントも読み直さない）で、commit の
  最速パスが `dev` だけを現在値へ直す（`update_physical`）。属性が 1 つでも違えば採らない（inode 再利用
  と区別できないので、次の段 = md5 / path へ落ちて読み直す）。候補が複数なら曖昧として採らない
- **編集の事前条件と「行と FD の照合」から `dev` を外す。** `edit_ops.expected_dev` は記録するが、
  tags / rename / md5 op の照合、rename と normalize の所在判定（`same_inode`）、flaccheck / RG /
  再生（`/api/stream`）の `matches` は inode / size / mtime_ns / ctime_ns（op はさらに `tag_hash` /
  MD5 値）で判定する。列とスナップショットの `dev` は残す（inode 段の最速パスの索引 `(dev, inode)`
  はそのまま）

**理由**: Linux の `st_dev` はマウント時に割り当てる番号で、ZFS データセットはホスト再起動で変わる
（実機で `ssd/media` が 79 → 80）。SPEC は `(dev, inode)` を安定したものとして書いていたため、
再起動後の起動時スキャンで全行が inode 段を外れ、path 段で `physically_changed = true` となって
9,098 件全部のタグ読みと可逆デコード（`lossless_md5`）が走った。データは壊れない（`audio_md5` は
一致するので版は動かない）が、再起動のたびに CPU 全コアで約 10 分・ライブラリ全読み（約 200 GB）を
払う。編集の事前条件も同じ前提だったので、バッチ準備と反映の間に再起動が入ると全 op が
`skipped_conflict` になるはずだった。dev が守るのは「別ファイルシステムの同じ inode 番号」だけで、
Library の root は 1 データセットであり、size / mtime / ctime（op は tag_hash も）を併せれば
取り違えの余地は実質無い。

**却下**: 起動時に dev の対応表を作って DB を一括更新する（root 配下に複数データセットがあると
対応が一意に決まらない。行ごとの照合で十分）。`dev` 列を落とす（inode 段の索引として有用で、hardlink
判定にも使う。走査中は安定している）。dev 不一致を「変更あり」のまま md5 だけで確認する（可逆の
デコードが要り、避けたいコストそのもの）。

**未決**: なし。

## D-63 遡及照合はオフセットを探し、verify.log は Library に置かない

**決定**（2026-09-19。P2-9）:

- **オフセットは最初から探す。** AccurateRip / CTDB に登録されている CRC は他人のドライブで
  吸ったもので、読み取りオフセットの補正が違えば同じ盤でも全サンプルが数十個ずれる。CUETools と
  同じ ±(5×588−1) サンプルの範囲で探し、「一致したトラック数 → 一致した信頼度の和 → 0 に近い」の
  順に 1 つ選んで `album_verifications.detected_offset` に残す。吸い出しのオフセットはディスクで
  1 つなので、トラックごとに別のオフセットは採らない。探索のために 1 オフセットごとに
  デコードし直すのではなく、1 回流す間にトラック境界 ±2939 の近傍で累積和（AccurateRip v1 は
  Σ(n × 語) で位置に線形）と CRC32 の途中状態（zlib の combine で部分列の CRC が出る）を記録し、
  任意のオフセットの CRC を O(1) で出す（`cd/crctable.rs`）。AccurateRip v2 は線形でないので
  オフセット 0 だけ比べる（CUETools も同じ）。CTDB の照合はパリティではなく trackcrcs の総当たり
  （CUETools のフォールバック経路と同じ。パリティは P2-7 の修復で扱う）
- **verify.log は `data/verify/<album_id>.log`。** SPEC §5 はアルバムディレクトリに置く前提だったが、
  Library に同梱ファイルを置き始めると一括リネーム後の旧ディレクトリへの追随（D-43 の残課題、
  P2-8 で決める）が先に問題になる。アプリのデータ領域に album_id で置き、
  `album_verifications.log_path` で参照する。アルバムの全ディスクぶんを毎回書き直す
- **AccurateRip の ID は CUETools / dBpoweramp 式を既定にする。** Enhanced CD（末尾がデータ
  トラック）で id1 / id2 は音声トラックだけを足すがリードアウトは実際の値、FreeDB ID はデータ
  トラックも数える。データトラックを落として −11400 で切る libdiscid 式の ID も DB に別キーとして
  存在する（Hybrid Theory JP 盤で両方を実サーバで確認）が、エントリ数は CUETools 式のほうが多い。
  必要なら `Toc::audio_session()` から作れる
- 照会に失敗したらジョブを失敗させて再試行し、何も記録しない。「照会できなかった」を
  `not_found` や `mismatch` に混ぜない
- **記録は 1 トランザクションで、ジョブ単位に冪等。** 照合を始めたときの `audio_version` と
  記録直前の fstat（root からパスで開き直す）の両方が一致するときだけ書き、ディスク × 手法の行と
  トラックの行と `tracks.verification` を 1 トランザクションで書く。verify.log の rename も
  トランザクションの中（rename に失敗すれば DB も巻き戻す）。commit の後・done の前に落ちて
  起動時リカバリで同じジョブが再実行されても、`album_verifications.job_id`（`(job_id, disc_no, method)`
  で一意）で既存行を見つけて何もしない（rename は commit の前なのでログも確定済み。読み直して
  別の結果で上書きしない）

**理由**: 遡及照合の目的は「既存 FLAC の格付け」で、オフセット 0 だけでは補正なしのリップや
プレス違いが全部 `mismatch`（要確認）になり、格付けとして役に立たない。CUETools が実用になって
いるのは探索があるからで、累積和を使えば探索のコストはデコード 1 回 + 数百万回の整数演算で済む。

**却下**: オフセット 0 だけで始めて実機の一致率を見てから決める（探索の実装コストが小さく、
先送りする理由が無かった）。verify.log をアルバムディレクトリに置く（P2-8 の同梱ファイルの決定を
待つ。決まったらそちらへ寄せてもよい）。AccurateRip の crc450 でオフセットを検出する
（CUETools の手法だが、累積和があれば全オフセットの完全な CRC を直接比べられる。crc450 は
計算だけ残してある）。

**未決**: ~~verify.log の置き場は P2-8 で同梱ファイルの扱いが決まったら見直す~~（D-67 で `data/verify` のままに確定）。

## D-64 MusicBrainz の候補は「リリース × medium」、TOC の入力源は差し替え可能にする

**決定**（2026-09-19。P2-3）:

- **照会は DiscID → TOC の 2 段。** `ws/2/discid/<DiscID>` を引き、404 なら同じエンドポイントに
  `?toc=`（MusicBrainz 形式の TOC）を付けて fuzzy に引く。MB 側がプレス違い・登録漏れの DiscID に
  対して TOC の近いリリースを返す。fuzzy の結果は `exact = false` として区別する
- **候補の単位はリリース × medium。** リリースの media のうち、自分の DiscID を `discs` に持つ
  medium は exact、そうでなければ音声トラック数が同じ medium を近似の候補にする（複数枚組で
  トラック数が同じディスクが複数あれば、それぞれが候補になる）。exact を先に並べ、MB の順は保つ。
  fuzzy の応答にも自分の DiscID を持つ medium が混ざることがあり、それは exact
- **`inc=recordings artist-credits labels release-groups isrcs`。** トラックのタイトル・アーティスト
  表記（joinphrase で繋ぐ。トラック固有が無ければリリースのもの）・長さ・recording / track の id・
  ISRC、リリースの日付・国・レーベルとカタログ番号・バーコード・注記・release-group id を候補に
  持たせる。タグの写像（P2-8 の配置で書く）はこの候補から決める
- **UA 必須・1 req/s はクライアント内で守る。** 直前の要求時刻を持ち、間隔が空くまで待つ
  （ロックを持ったまま待つので並行する照会も直列）。503（負荷制限）は 1 度だけ間隔ぶん待って
  再試行し、それでも 503 なら API は 503 `musicbrainz_unavailable`（一時的。届かない・壊れた応答の
  502 `lookup_failed` とは分ける）。両方の要求に `cdstubs=no` を付ける（未登録 DiscID に CD stub が
  あると 200 で別の形が返り、404 → fuzzy に進めない）。`[musicbrainz].url` で照会先を差し替えられる
  （テストと自前ミラー）
- **TOC の入力源は差し替え可能にし、ドライブが無い間は貼り付け。** `POST /api/cd/lookup { toc }` は
  TOC 文字列（CTDB 形式か MusicBrainz 形式。`Toc::parse`）を受け、ドライブからの TOC
  （P2-1 / P2-2 の `GET /api/cd/status`）も同じ文字列で渡す。CD 画面は今は貼り付け欄
  （`cdrecord -toc` の出力も LBA を拾って受け付ける）を入力源にし、P2-1 で検出に差し替える。
  貼り付けはデバッグ用に残す

**理由**: 候補をリリース単位にすると複数枚組でどの medium かが決まらず、トラック対応も
確認できない。DiscID だけで引くとプレス違いの盤（同じ内容で TOC が数セクタ違う）が 0 件になり、
D-21 の「手入力」に落ちる場面が増える。TOC の入力源を API の境界で文字列に揃えておけば、
ドライブの実装（P2-1）を待たずに照会と候補選択の全経路をテストでき、届いた後は検出を差し替える
だけで済む。

**却下**: libdiscid の FFI（TOC からの整数演算で足りる。P2-2）。CD stub（MB の cdstubs）を
候補に含める（品質が低く、手入力経路があるので不要）。cover art の取得（P2-8 の配置で
Cover Art Archive を引くか決める）。

**未決**: fuzzy の候補が多いときの並べ方（今は MB の順）。手入力と候補の「補正」は D-65 で決めた。

**追記（2026-09-22。P2-3 拡張。DiscID 以外の識別経路）**: 実盤（嵐「Five」の初回盤）が MusicBrainz に
登録されているのに DiscID もトラック長も未登録で、TOC の fuzzy では原理的に出なかった。識別を 5 経路にする:
(1) DiscID（exact）、(2) ユーザが貼ったリリース URL / MBID（`ws/2/release/<id>`。どんな盤でも当てられる保険）、
(3) ディスクの Q サブチャネルから読んだ ISRC（`ws/2/recording?query=isrc:A OR isrc:B…` を 1 回 → 一致数の
多いリリースから上限 5 件を取得）、(4) MCN = JAN / UPC（`ws/2/release?query=barcode:` → 上限 3 件）、
(5) TOC の fuzzy。ISRC / MCN はドライブが TOC と同じ回に SG_IO の READ SUB-CHANNEL で読み
（`GET /api/cd/status` の `isrcs` / `mcn`）、UI が照会に添える。DiscID で当たれば (3)(4) は引かない（(2) だけ
足す）。同じリリース × medium は 1 件に束ね、出てきた経路を `matched_by`（discid > release > isrc >
barcode > toc）で持って、その順に並べる。fuzzy の応答に混ざった DiscID 持ち（上記）は discid 経路として扱う。
指定リリースが読めない・トラック数の合う medium が無いときは `notes` で返し、照会全体は失敗にしない。
候補を選んだ後（DiscID 未登録のとき）は libdiscid と同じ形の登録 URL（`cdtoc/attach?id=&tracks=&toc=`）を
UI に出し、登録は本人がブラウザで行う。上の「未決」の並べ方はこれで閉じる（経路の強さ、同じ経路の中は
MB の順）。

**追記 2（2026-09-22。接続の経路と診断）**: この回線の **IPv4 から MetaBrainz のインフラ全体
（musicbrainz.org / beta.musicbrainz.org / coverartarchive.org、443 と 80）へ繋がらない**ことが分かった。
TCP は受理されるが TLS ハンドシェイクの途中で切られる（`unexpected eof`。TLS 1.2 を強制しても、鍵交換の
曲線を絞っても同じ）。同じ回線から github.com の IPv4 は通り、MetaBrainz の **IPv6 は通る**（TrueNAS ホスト・
開発コンテナ・spindle のコンテナすべてで確認。Docker の bridge は ULA + NAT66 で外へ出られる）。レート制限
（503 + JSON）とは別物で、先方のエッジがこの公開 IPv4 を落としている。対処:
- `[musicbrainz].address_family = "auto" | "ipv6" | "ipv4"`（既定 auto）。`Auto` 以外なら reqwest の DNS 解決を
  その族に絞る（`cd::select_addrs` / `FamilyResolver`）。reqwest に族を選ぶ設定は無いのでリゾルバを差し替える。
  指定した族のアドレスが 1 つも無ければ、黙ってもう一方へ倒さず「IPv6 のアドレスが無い」と分かる失敗にする
  （塞がっている族へ落ちると原因の分からない TLS エラーになるだけで、1 回の解決で片方しか返らない事故も
  見えなくなる。接続の失敗は下の張り直しで 1 度やり直すので、一過性の解決漏れはそこで復帰する）
- 応答が返る前に落ちた要求（TLS の失敗・idle なコネクションの再利用・一過性の切断）は **1 度だけ張り直す**。
  タイムアウトは対象外（待ち直しても同じで、時間が倍になるだけ）
- ログと API の本文は `cd::error_chain` で原因の末端まで出す。reqwest の `Display` は
  「error sending request for url (…)」で止まり、TLS の失敗・タイムアウト・接続断の区別が消えていた
- **照会結果は入力（TOC + ISRC + MCN + 指定リリース）ごとに 10 分覚える**（追記 3。`MusicBrainzClient` の中。
  8 件まで、古いものから捨てる）。1 枚の照会で最大 13 本の要求が飛ぶので、画面を開き直すたびに引き直すと
  1 req/s の規約に対して重い。失敗は覚えない（押し直せばまた引きにいく）。画面の「MusicBrainz に照会」
  ボタンは `refresh: true` で捨てて引き直す（DiscID を登録した直後などに効く）。ディスク検出からの自動照会は
  付けない
- MusicBrainz の `ws/2` は読み取りに認証が不要で、**OAuth トークンを取ってもレート上限も IP 遮断も変わらない**
  （トークンは投稿・ユーザデータ用）。MetaBrainz アカウントが効くのはローカルミラー（Live Data Feed の
  レプリケーショントークン）を立てる場合で、そのときは `[musicbrainz].url` を差し替える

**追記 4**（2026-09-23。P4-20）: 経路を**段階にして打ち切る**。`discid` → `ids`（ISRC 検索 +
バーコード検索）→ `toc`（TOC 近似）の順で、上の段で候補が 1 件でも残れば下の段は引かない。
候補の数はトラック数で絞ったあとで数える（ISRC が当たっても曲数の合う medium が無ければ 0 件で、
次の段に進む）。**DiscID が 200 でも候補が 0 件なら次の段へ落とす**（登録済みの DiscID でも medium の
トラック数が合わなければ候補にならず、そこで止めると行き止まりになる）。ただし `exact` は真のままに
して、DiscID の登録を勧めない。ユーザが貼ったリリース指定は段に関係なく常に足し、`toc` に進むかの
判定では「残った候補」に数える（ただし `ids` は引く。複数経路を束ねる D-64 本体の動きを壊さない）。

**`ids` の候補は常に TOC 近似より強い、と決め打ちしている。** ISRC は録音単位なので、1 曲だけ一致する
コンピレーションでも曲数さえ合えば候補に残り、TOC 近似を止める。正解を自動で取り逃すことはあり得るが、
逃げ道は「さらに広げて探す」（`can_widen`）で残す。一致した ISRC の数などの確度を持って比べる設計は
採らない（値を経路ごとに持ち回る仕組みが要り、割に合わない）。

理由: 実機（嵐「Five」）で、ISRC 経路に正解があるのに TOC 近似が全く別の盤を大量に返し、
候補一覧が読めなくなっていた。TOC 近似は「トラック長が近い」だけの弱い一致で、DiscID が未登録で
ISRC も無い盤のための最後の手段。応答の `stage` と `can_widen` で画面に段を見せ、「さらに広げて
探す」（`widen`）で明示的に下の段まで引ける。`widen` はキャッシュの鍵に入れる（広げた結果が
普通の照会に返らないように）。

---

## D-65 手入力は「候補を土台に編集する 1 つのフォーム」、貼り付けの行解析は決め打ち

**決定**（P2-4、D-21 の具体化）:

- **候補も手入力も同じフォーム（`DiscDraft`）に収束させ、確定で `DiscMetadata` にする。** 候補を
  選ぶとフォームに写り（アルバム・アルバムアーティスト・日付・レーベル / カタログ番号 / バーコード・
  ディスク番号 / 枚数・各トラックのタイトル / アーティスト、MusicBrainz の release / recording / track
  id と ISRC）、そこから直せる。候補ゼロ件なら空のフォームに直行し、候補がどれも違えば「候補を使わず
  手入力」で空にする。どちらも同じ `DiscMetadata`（`source: musicbrainz | manual`）なので、吸い出し
  （P2-5）と配置（P2-8）の入力は 1 経路。候補を写し直すと編集中の内容は消える（画面で断る）
- **行は TOC の音声トラックと 1:1。** 番号と長さは TOC から取り、編集できない。候補のトラックは
  位置順に当て、足りなければ空行、余れば捨てる。`POST /api/cd/lookup` の応答に `tracks: [{ number,
  length_ms }]` を足し、候補が無くても行数と長さが出る
- **確定の条件はアルバム名・アルバムアーティスト・各トラックのタイトル。** 日付は `YYYY[-MM[-DD]]`
  か空。トラックのアーティストが空ならアルバムアーティストで埋める。タイトルの分からない盤のために
  「空のタイトルを `Track NN` で埋める」ボタンを置く（黙って埋めない）
  → **P4-20 で覆した**（下の追記を見ること）
- **貼り付けの行解析は web（TypeScript）に置き、規則は決め打ち。** 貼るたびに往復せず即座にプレビュー
  でき、間違いはフォームで直す前提なので、推測して当てに行かない。規則: 行頭の番号（`1.` `01` `1)`
  `[01]` `Track 1:` `M-1` `#1`、全角。1..=99 だけで、年や `100 Years` は番号にしない）、行末の時間
  （`4:32` `(3:05)` `1:02:03`。秒は 00..59）、アーティストの区切り（`/` と `／` > `|` と `｜` >
  ` - ` ` – ` ` — `。既定は「タイトル / アーティスト」で、チェック 1 つで逆順。区切りは**アーティスト側の
  端**で切り、タイトル内の ` - Remix - ` を残す。端にぶら下がった区切り（`Title／`、`A - B -`）は
  貼り付けの残骸として落とし、空白無しの `-Intro-` は残す）、タブがあれば表の
  貼り付け（数字だけの列は番号、時間の列は長さ、残りがタイトル・アーティスト）、空行と `Disc 1` /
  `トラックリスト` / `収録曲` の見出しは飛ばす。番号の無い行は前の行 + 1。**番号で行に対応付ける**ので、
  貼り付けの行数が TOC と違っても写せるところは写し、行数の違い・TOC に無い番号・未設定の行を警告に
  出す。アーティストの無い行は既存の値を保つ（候補の表記を貼り付けで消さない）。番号と時間の判定は
  全角の数字・コロンも受けるが、タイトルの全角は正規化しない
- **画面の遷移は純粋な reducer（`web/src/lib/cdState.ts`）で、段が上から消える。** toc → result →
  selected / draft → confirmed の順に段があり、上の段が変わると下は全部消える（TOC の編集と「結果を消す」は
  結果と貼り付けの本文ごと、照会のやり直しと候補の選び直しは選択より下。同じディスクの照会のやり直しでは
  貼り付けの本文を保つ）。確定後は候補の選び直しと手入力を受け付けず、「編集に戻る」で確定だけ外す。
  hook（`useCdLookup`）は非同期の照会と dispatch だけにして、遷移は vitest で固定する
- **確定画面はタグ名で見せ、写像を関数（`albumTags` / `trackTags`）で固定する。** P2-8 の配置はこの
  写像で書く。MusicBrainz の id は recording が `MUSICBRAINZ_TRACKID`、リリース内の track が
  `MUSICBRAINZ_RELEASETRACKID`（Picard の写像。取り違えやすい）。写像は `[キー, 値の列]` で、ISRC のような
  多値は Vorbis コメントの同じキーの反復として持つ（`;` で 1 値に繋がない）。`TRACKTOTAL` と
  `MUSICBRAINZ_DISCID` は TOC から吸い出し時に付ける

**理由**: 同人・VTuber・インディーズの国内盤は MusicBrainz にあっても誤字や表記揺れがあり、
「候補は選ぶだけ、手入力は最初から全部」だと誤字 1 つで全部書き直しになる。候補と手入力を別の形に
すると P2-8 のタグ写像が 2 経路になり、MusicBrainz の id を持ち越すかどうかも経路ごとに決めることになる。
貼り付けの解析は「そこそこ当たって、外れたらすぐ直せる」ことが価値で、通販ページの形は無限にあるので
規則を増やして当てに行くより、プレビューとフォームで直す方が速い。

**却下**: 解析を Rust の API にする（貼るたびに往復。純粋関数なので vitest で固定化できる）。
空のタイトルを確定時に黙って `Track NN` にする（黙った既定はタグに残る。ボタンで明示させる）
→ **P4-20 で覆した**。プレースホルダで埋まる値が常に見えているので「黙って」ではない。
アーティストの区切りを「最初の出現」で固定する（`Title - Remix - Artist` が壊れる）。全角の英数字・
記号をタイトルごと正規化する（`第２章` や `！` が変わる。番号と時間の判定だけ全角を受ける）。
画面の遷移を hook の useState に散らす（テストできず、TOC 編集で確定が残るような取りこぼしが見えない）。

**未決**: 貼り付けの行数が TOC と違うときに「上から順に当てる」選択肢（今は番号で対応付けるだけ）。
複数枚組の貼り付け（`Disc 2` 以降を別のディスクとして扱う。今は見出しを飛ばして番号の重複を警告）。


**追記**（2026-09-23。P4-20。上の「確定の条件」と「遷移」を覆す）:

- **空のタイトルは確定を止めない。** 表では `Track NN` を**プレースホルダとして見せ**（値は空）、
  取り込みの直前に空のままなら `Track NN` を採る（`defaultTitle` / `finalizeDraft`）。
  「空のタイトルを `Track NN` で埋める」ボタンは要らなくなったので消した。
  確定の条件はアルバム名・アルバムアーティスト・日付の形・ディスク番号だけ
- **「確定」の段そのものを廃止した。** 表を直接編集して「取り込む」1 発にしたので、`confirmed` /
  `draftErrors` と `confirm` / `unconfirm` / `fill_titles` は無い。「編集に戻る」も無い
- **フォームは TOC が読めた時点で必ず存在する**（表は照会の前から出る）。消えるのは別のディスクに
  替わったときだけで、照会の開始と失敗では消さない
- **CD 画面からは直せない**（2026-09-23 追記。ユーザ要望）。候補を選ぶ / 写す範囲を変える /
  「どれも違う」の 3 つだけで、補正とトラックリスト貼り付けは **Inbox の承認画面**に移す
  （取り込んだものは Inbox を通る。D-67 追記）。入力欄が 2 か所にあると、どちらで直すのか
  分からなくなる。貼り付けは P2-10 で Inbox の承認画面へ移した（下の追記 2）

理由: 分からないトラックに既定の名前が「最初から見えている」方が、ボタンを押させるより分かりやすい
（ユーザ要望）。画面が「表を直して取り込む」1 本になり、確定を挟む理由が無くなった。黙って埋めるのを
避けたかった元の意図は、プレースホルダで**埋まる値が常に見えている**ことで満たしている。

**追記 2**（2026-09-23。P2-10。貼り付けを Inbox の承認画面へ移した）:

- **写し先は Inbox の下書き（`InboxDraft`）の、選んだ 1 枚のディスクの行。** Inbox の件は TOC ではなく
  ファイルなので、対応付けは行の `track_no`（ファイルの並びではない）。件に複数のディスクがあるときだけ
  「写す先」を選ばせる（上の未決の「`Disc 2` 以降を別のディスクとして扱う」は決めず、1 回の貼り付けは
  1 枚ぶん）。同じ番号の行が複数ある（下書きの番号が重複している）ときは、どれに写すか決められないので
  写さずに警告する
- **アーティストを貼った行は多値の「そのまま保つ」（D-70）を外す。** 保ったままだと貼った値が
  書かれず、画面も結合表示のまま変わらない。貼った以上は 1 値で書く意図とみなす。アーティストの無い行は
  従来どおり既存の値（と保つ設定）を保つ
- CD 画面側の `lib/cd.ts` の `applyTracklist` は消した（Inbox 版は `lib/inbox.ts`）

---

## D-66 CTDB の修復は CUETools の定義どおりに、CRC が合うときだけ適用する

**決定**（P2-7）:

- **符号は CUETools `CDRepair` そのもの。** 16 bit 語を stride = 11760 語（10 セクタ）の行に並べ、
  列ごとに GF(2^16)（生成多項式 0x1100B）の Reed-Solomon。データ行は先頭 1 行（leadin）と末尾
  1 行 + 端数（leadout）を除いた K = 2N/stride − 2 行で、CTDB のディスク CRC の範囲と一致する。
  シンドローム S_i(c) = Σ_r d_r·α^{i(K−r)} を直接 Horner で取る（CUETools は LFSR でパリティを作って
  から変換するが、パリティファイルが持つのはシンドロームなので、変換を挟まない方が短い）
- **パリティファイルは面順のシンドローム。** `hasparity` の URL に npar 面 × 11760 語（LE）。XML の
  `syndrome` 属性は列 0 で、旧い `parity` 属性（列 0 のパリティ 8 語）は `Parity2Syndrome` の式で
  同じ形にする。取得は Range で先頭 npar 面だけ（CUETools は 4 → 8 → 16 と広げる。npar 8 で 188 KB）。
  列 0 が属性と合わなければ拒む
- **オフセットは列 0 だけで探す**（`syndrome` 属性があれば通信なしで決まる）。ずらした列は隣の列に
  leadin / leadout の 1 語を足し引きして出る（`GetSyndrome` と同じ）。探索範囲は ±2939（CrcTable と
  同じ。CUETools は ±5879）。完全一致を優先し、無ければ列 0 の誤りが npar/2 未満で復号できる
  うち最少のもの
- **直した後のディスク CRC がエントリの値に一致するときだけ計画を採用する。** 誤りパターンの
  CRC への寄与は線形なので、計画の段階で `crc32_combine` により適用前に求まる（CUETools と同じ）。
  RS が「直せた」と言っても CRC が合わなければ誤訂正として捨てる。適用は 2 回目の走査で語を XOR
  し、そこでもデータ行の CRC を取って返す
- **計画と適用を分ける。** 吸い出し（P2-5）は tmp の PCM を 2 回読む: 1 回目で CRC 表とシンドローム表、
  2 回目で適用。ディスク全体（最大 800 MB）を載せない。修復の対象は自分の吸い出し結果だけで、
  既存 FLAC（§7.3 の遡及照合）は直さない（音声の書き換えは版管理と巻き戻しの対象で、別の判断が要る）

**理由**: 傷は 1 セクタ（1176 語）単位で来て、1176 の列に 1 語ずつ散るので、列あたり npar/2 個
（4 か 8）の訂正能力で数セクタぶんが直る。これが CTDB を主にする理由（D-13）そのもの。CRC の
ゲートを外すと、能力を超えた誤りを RS が別の符号語に「直して」しまったものが通る。

**却下**: パリティを LFSR で作って変換する（CUETools 互換の内部表現だが、ファイルにあるのは
シンドローム）。オフセット探索に CRC 表を使う（誤りがあると CRC は一致しないので探せない）。
遡及照合の FLAC を直す（版管理と巻き戻しの範囲で別途決める）。

**未決**: オフセット探索を ±5879 に広げる（CrcTable の範囲も一緒に広げる必要がある）。
80 分 3〜5 秒のシンドローム計算を SIMD で速める（吸い出しは 10 分かかるので今は要らない）。
CTDB への提出（自分のシンドロームを面順にした `DbSyndromes::from_table` は用意してある）。

---

## D-67 CD の配置は rip ジョブが直接登録し、同梱ファイルは album 全体の移動に追随する

**決定**（2026-09-19。P2-8。D-43 / D-63 / D-64 / D-65 の残課題を閉じる）:

- **配置は `cd::place::place_disc` の 1 関数で、rip ジョブ（P2-5）の最終段として呼ぶ。** 入力は
  `Toc`、`DiscMetadata`（D-65 の確定フォームと同じ形 + `category`）、`TrackLayout`、オフセット適用済みの
  tmp の PCM（s16le / 2ch / 44.1k）、`RipReport`（ドライブ・オフセットと出所・試行回数・トラックごとの
  再読み / C2 数・AccurateRip / CTDB の `MethodResult`・修復の適用数。P2-5 が埋める）。`POST /api/cd/rip`
  と吸い出しは P2-5 で、P2-8 はテストから `place_disc` を直接呼んで固定する
- **エンコードは raw PCM を `flac -8 --verify` に直接渡す**（ffmpeg を挟まない。正規化の `FlacEncoder`
  はデコードが要るので経路が違う）。トラックごとに PCM の MD5 を取り、STREAMINFO の MD5 と一致する
  ときだけ成果物にする。タグは D-65 の写像（`album_tags` / `track_tags` を Rust に移植）+ `TRACKTOTAL`
  （TOC の音声トラック数）+ `MUSICBRAINZ_DISCID` を lofty で書く。`category` はタグに書かない（パス専用。
  SPEC §5）
- **パスは `pathgen::plan` を再利用する**（`[layout]`、category 無しなら `unsorted`、`disc_count > 1` なら
  `multi_disc`。降格と衝突は D-43 の規則のまま）。リリースキーは `mb:<release_id>` → 無ければ、宛先
  ディレクトリに albumartist と album が一致する複数枚組（`disc_count > 1` か構成トラックの `disc_no` の
  最大が 2 以上）の album があり、入力も複数枚組で、その `disc_no` のトラックがまだ無ければ**その album に
  合流**（複数枚組の手入力で 2 枚目が `({year})` に降格しない）→ それ以外は `disc:<DiscID>` の新規リリース
  （Library へ直接置いていたときの規則。D-67 追記 2 で Inbox 経由になり、追記 3 で `disc:` のキーは無くなった）。
  `release_id` があるときは合流を探さない（別リリースなら降格し、降格先が album になる。合流先を持ったまま
  降格すると「ディレクトリ = album」が壊れる）。計画はエンコードの前に一度（衝突を早く知る）と、
  `library` の排他を取った後にもう一度行い、後者を確定にする（エンコードの間に scan / rename が album を
  動かし得る）。rename は `library` の排他を取らない（track_locks だけ。D-38 の並走前提）ので、登録の
  トランザクション（writer で直列）でも全経路を再検証する: 合流先は `rel_dir_key` が計画の宛先と一致し
  active であること、宛先の既存 album はリリースキーが計画と同じであること（違えば衝突。missing の
  album は同じキーなら復活、違えば `\0displaced:` へ退かせて新規）、同じパスの行は `audio_md5` が
  一致すること。検証に失敗したら、この呼び出しで**新しく置いた**ファイルだけを消し（既存の成果物と
  他人のファイルは触らない）、空になったディレクトリを消してから Conflict にする
- **配置は `job_mutexes` の `library` を取ってから**（scan / gc と同じ。取れなければ Requeue）。
  トラックと同梱ファイルを tmp → fsync → `RENAME_NOREPLACE` → dir fsync で置き、1 トランザクションで
  `albums`（無ければ作成。合流なら触らない）/ `tracks`（スキャナの `track_content` / `insert_track`。
  `source_type = 'cd_rip'`、`verification`）/ `track_tags` / `album_verifications`（`source = 'rip'`、
  手法ごと、`log_path` は Library 相対の rip.log）/ `track_verifications` を書く。`tracks.verification`
  の写像は verify ジョブと同じ（CTDB 一致 → `verified_ctdb` → AR 一致 → `verified_ar` → 候補あり不一致 →
  `mismatch` → 候補なし → `not_attempted`）。後続は `rg`（album）と `transcode`（`derived::enqueue_if_stale`）。
  カバー画像は無いので thumbnail は出ない（Cover Art Archive は引かない。`/api/artwork/embed` で後から
  埋め込む）
- **冪等性は MD5 で判定する。** 宛先に既にファイルがあれば STREAMINFO の MD5 が自分の PCM の MD5 と
  一致するときだけ自分の成果物とみなして配置を飛ばす。DB に同じ `rel_path` の行が既にあり `audio_md5`
  も一致すれば（配置の後に落ちてスキャナが先に拾った）挿入せずその行を採用し、出自と検証だけ書く。
  どちらでもなければ conflict で失敗し、Library は触らない
- **同梱ファイルの名前は 1 枚なら `disc.cue` / `disc.toc` / `rip.log`、複数枚組は `disc<N>.cue` /
  `disc<N>.toc` / `rip<N>.log`**（N = `disc_no`、0 埋めなし）。形式:
  - `rip.log`: 先頭行 `spindle rip log v1`（スキャナの判定キー）。ドライブ / デバイス / 読み取り
    オフセットと出所 / エンコーダ / 各種 DiscID と TOC 文字列 / アルバム情報 / トラック表（LBA・長さ・
    再読み・C2・CRC32・ARv1・ARv2・CTDB CRC・照合結果・ファイル名）/ 手法ごとの結論と修復 / 総合結果
  - `disc.cue`: EAC 流の複数ファイル cue（`REM DISCID` / `REM DATE` / `CATALOG`（バーコードが 13 桁の
    とき）/ `TITLE` / `PERFORMER`、トラックごとに `FILE "…" WAVE` / `TRACK NN AUDIO` / `TITLE` /
    `PERFORMER` / `ISRC` / `INDEX 01 00:00:00`）。ギャップは前トラック末尾に付く形（INDEX 00 は TOC から
    分からない）。データトラックは `REM` で記す
  - `disc.toc`: `Toc` から cdrdao 構文（`CD_DA` / `CATALOG` / `CD_TEXT` / `TRACK AUDIO` +
    `FILE "…" 0 MM:SS:FF`）で生成。実ドライブの `cdrdao read-toc`（P2-2）も `Toc` にしてから同じ形で
    書く（形式を 1 つに）
- **同梱ファイルは album 全体の移動に追随する（D-43 の残課題）。** rename ジョブは phase 2 の後の
  commit トランザクションの中（`tx.commit()` の前）で、applied の op の旧ディレクトリのうち宛先が 1 つで
  active な行が残らないものについて、**既知の名前**（`cover` / `folder` / `front` × 画像拡張子、
  `disc*.cue` / `disc*.toc` / `rip*.log`）の通常ファイルを新ディレクトリへ `RENAME_NOREPLACE` で移し
  （衝突は残して警告）、旧ディレクトリが空なら `rmdir`（`ENOTEMPTY` は無視）。commit の前に動かすのは、
  トラックの実体は phase 2 で既に宛先にあって commit はそれを記録するだけなので、途中で落ちても再実行が
  同じ判定に至り（旧ディレクトリに同梱ファイルはもう無い）、commit の後に落ちて追随だけが永久に残る窓を
  作らないため。終端を観測した側は追随済みの状態を見る。`edits` には記録しない。巻き戻しは逆向きの
  album 全体の移動になるので同じ経路で戻る。一部だけの巻き戻しでは動かない（トラックが残る側に
  付いていく）。rename op がトラックのパスだけを所有する原則（D-43）は変えない
- **verify.log は `data/verify` のまま**（D-63 の未決を閉じる）。Library に置く同梱ファイルは rip 由来の
  3 つだけ
- **スキャナは spindle の rip.log から `source_type = 'cd_rip'` を復元する。** ディレクトリの inventory で
  `rip.log` / `rip<N>.log` を見つけたら先頭行だけ読み、`spindle rip log v1` なら印を付け、**新規挿入する
  行**だけ `cd_rip` にする（既存行は触らない。`verification` は復元せず遡及照合 §7.3 で付け直す）。
  不変条件 1（DB はキャッシュ）を出自についても保つ
- **`category` は確定フォームで選ぶ。** `GET /api/categories` / `POST /api/categories { name }`（SPEC §9 に
  あって未実装だった）を足し、`DiscDraft` / `DiscMetadata` に `category`（語彙の名前。null なら
  `_Unsorted`）を持たせる

**理由**: スキャナ経由の登録（置いてから incremental scan）だと出自と検証結果を書く相手の行が
いつ現れるか分からず、競合を避けるにはどのみち `library` の排他が要る。直接登録なら配置と登録が
1 つのジョブに閉じ、冪等性の判定も MD5 の 1 規則で済む。同梱ファイルの追随を rename ジョブに置く
のは、album 全体の移動を既に coordinator が判定していて、そこでしか「ディレクトリが空になる」
ことが分からないため。出自を rip.log から復元するのは、DB を消しても `cd_rip` の絞り込みが戻る
ように（検証は照会し直せば戻るが、出自は照会では戻らない）。

**却下**: 置いてからスキャンで拾わせる（上記）。同梱ファイルの移動を op として記録する
（rename の 2 phase に絡み、巻き戻しの対象が増える割に、ユーザが編集した値ではない）。同梱
ファイルを残して GC に回収させる（旧ディレクトリに cover.jpg だけが 30 日残る）。verify.log を
アルバムディレクトリへ移す（既存ログの移行が要り、Library に置く理由が無い）。Cover Art Archive の
取得（同人 / 国内盤は無いことが多く、埋め込みは後からできる）。`SOURCEMEDIA=CD` のようなタグで出自を
持つ（外部ツールが付けた同名タグと区別できない。rip.log は spindle の署名付き）。

**未決**: 同梱ファイルの追随が衝突で残ったときの回収（今は警告だけ）。cdrdao の `read-toc` が持つ
ISRC / pre-emphasis / CD-TEXT を `Toc` に持たせて disc.toc に書く（P2-2 のドライブ実装で決める）。

**追記**（2026-09-23。ユーザ要望。**実装は P2-5**）: **吸い出したものは Library へ直接置かず、Inbox を
通す。** CD 画面からは編集を外し、値を直す場所を Inbox の承認画面に一本化する。

- 吸い出した FLAC は `[paths].inbox` の下に 1 ディレクトリ = 1 枚で置く（複数枚組はディスクごと）
- **MusicBrainz から写した内容と検証結果はサイドカー `spindle-inbox.json` に書く**（D-70 が
  ytmusic 用に作った仕組みをそのまま使う）。これで承認画面に候補の値が最初から入り、打ち直しが要らない。
  タグにも書くが、`category` はタグに書かない（パス専用。SPEC §5）ので、サイドカーが唯一の伝え方になる
- `rip.log` / `disc.cue` / `disc.toc` も件のディレクトリに置き、承認して配置するときに Library へ運ぶ
  （既存の同梱ファイルの追随と同じ扱い）
- **AccurateRip / CTDB の照合結果は配置時に登録する。** `album_verifications` / `track_verifications` /
  `tracks.verification` は `tracks` の行が要るので、Inbox に居る間は登録できない。サイドカーに
  `RipReport` を入れておき、Inbox の配置がそれを読んで `source = 'rip'` で入れる
- `source_type = 'cd_rip'` も配置時に付ける（スキャナの rip.log 判定は残す。外から置かれた rip にも効く）

**理由**: ユーザの導線は「入力（CD / YouTube）→ Inbox → ライブラリ」で、CD だけ Library へ直行するのは
一貫しない。また CD 画面と承認画面の両方に入力欄があると、どちらで直すのか分からなくなる。承認を
挟むことで、取り込んだ直後に名前を直してから Library に入る。

**この追記で `place_disc` は作り直しになる**（宛先が Library から Inbox へ、登録が `tracks` から
`inbox_items` + サイドカーへ変わる）。既存の `tests/cd_place.rs` も書き換わる。P2-5 でまとめて行う。

**リップ開始の入力は空の名前を許す**（codex 指摘）。CD 画面から編集を外したので「どの候補も違う」
盤は名前の無いまま取り込むことになるが、いまの `DiscMetadata::validate`（`src/cd/metadata.rs`）は
`EmptyAlbum` / `EmptyAlbumArtist` を返し、web の `validateDraft` もアルバム名とアルバムアーティストを
必須にしている。このまま P2-5 で既存型を配線すると、その経路は必ず開始を拒まれる。

- リップの開始に渡すのは「**名前は空でもよい**」別の契約にする（`DiscMetadata` の検証は Library へ
  置くときのもの。Inbox の承認で `InboxDraft` の検証が必須を担保するので、二重に持たない）
- 名前が空なら Inbox の件のディレクトリ名は DiscID など盤の識別子から作る（`_Unsorted` へ落とすのは
  承認時の判断）
- web の `validateDraft`（`lib/cd.ts`）は CD 画面から使われなくなった。P2-5 で開始の前提を決めるときに
  消すか、空名を許す形に直す
- 受け入れ条件: **候補ゼロ件・アルバム名もアーティストも空のまま Inbox まで完走する**

**サイドカーは `RipReport` の各トラックをファイルに明示的に結びつける**（codex 指摘）。既存の
`cd::place::disc_record` は `RipReport` の配列順と `track_ids` の配列順を zip しているが、Inbox では
ユーザが `disc_no` / `track_no` / タイトルを直せるので、走査順や下書き順を暗黙に信じてはいけない。

- サイドカーは**ファイルの basename（不変の鍵）でトラックへ対応させる**。配置時に、件の全ファイルと
  1 対 1 で対応すること・CRC の件数・`disc_no` が揃うことを確かめてから登録する
- `source_type = 'cd_rip'` / `album_verifications`（`source = 'rip'`）/ `track_verifications` /
  `tracks.verification` は、Inbox の登録（`register_item`）と**同じトランザクション**で書く
- `album_verifications.log_path` は移動後の `rel_path`（Library 相対）にする
- CD は `album_gain = true` で提案する（SPEC 7.2 / D-74。いまは配置時に立てていた）
- サイドカー v1 は `category` / `files` しか持たず、`RipReport` も serde 型ではない。**形を足すのは
  P2-5 の明示タスク**（テストも）

**追記 2**（2026-09-23。P2-5 の Inbox 側の実装で決めたこと）:

- サイドカーは v1 のまま `rip`（`RipEntry`）を足す。無い件ではキーを出さず、旧い読み手は未知のキーを無視する
  ので版を上げない。`RipReport` / `MethodResult` はそのまま serde 型にした（列挙は snake_case）。Inbox に
  置かれたまま版をまたぎ得るので、キーの綴りはテストで固定する。モジュールは `import/ytmusic/sidecar.rs`
  から `import/sidecar.rs` へ移した（CD と共有するため）
- **検証記録の `disc_no` は下書きの値**（承認でディスク番号を直せば、記録もそのディスク番号に付く）。1 件 = 1 枚
  なので、下書きのディスク番号が揃っていなければ配置しない
- **Inbox 経由の記録は `job_id` を NULL にする**（`record_album` の `job_id` を `Option` に）。1 回の inbox ジョブが
  複数の件を置くので、ジョブ id は `(job_id, disc_no, method)` の一意性の鍵にならない（2 枚の CD を同じ回に
  置くと 2 件目が「記録済み」で落ちる）。代わりに**全トラックに `source = 'rip'` の記録が既にあれば書かない**
  （`all_have_rip_records`）。登録の commit の後・Inbox を消す前に落ちると、残った音声で走査が件を `pending`
  に戻し、再承認で同じファイルと行を採用して登録へ戻ってくるため（codex 指摘）。同じ音声の盤を吸い出し直して
  同じ行に置いた場合も書かない（Library は変わらず、最初の記録が残る。照合し直すなら遡及照合）
- **サイドカーは読んだ FD の同一性（inode / size / mtime / ctime）を登録の直前と Inbox を消す前に照合する**
  （codex 指摘）。登録前に変わっていれば置いたファイルを片付けて `Changed`（件は `pending`）、登録後に変わって
  いれば消さずに残す。外から差し替えられた記録を古いまま登録しない・新しいものを消さない（ファイルが正）
- rip.log が宛先の別内容と衝突して移せなかったときは `log_path` を NULL にする（存在しないパスを指さない）
- **Inbox への置き方**（`place_disc`）: Inbox 直下の隠しディレクトリ `.spindle-rip-<DiscID>` で組み立て、
  ディレクトリごと `CD/<albumartist - album> [<DiscID>]` へ `RENAME_NOREPLACE` する。走査は `.` で始まる
  ディレクトリを見ないので、ファイルやサイドカーが揃う前の盤が件になって承認されることがない（1 本ずつ
  置くと、途中の件を承認すると検証記録の無い `download` として Library に入る）。名前が空なら
  `CD/[<DiscID>]`: **MusicBrainz の DiscID は `.` で始まり得る**（base64 の変形で `.` `_` `-` を含む）ので
  括弧で包む（素のままだと隠しディレクトリになり、走査に見えない。テストの TOC で実際に起きた）。
  トラックのファイル名は `NN.flac`（TOC の番号）。サイドカーとの対応付けの鍵なので、承認で変わる値
  （タイトル）を入れない。Inbox の行は DB に直接書かず（D-68）、公開後に `inbox` ジョブを投入して走査に
  出させる
- **Inbox 経由で置いた CD の album に `albums.discid` は入れない**（ユーザ判断）。入れると追記先の判定
  （`destination` は DiscID を持つ album を除く）から外れ、MusicBrainz に無い複数枚組の 2 枚目が 1 枚目に
  合流できなくなる。既存曲が大量にあり、album の同一性の規則を今は動かさない。~~**未決**: スキャナは
  `MUSICBRAINZ_DISCID` から `discid` を復元するので、DB を作り直した後だけ値が付く食い違いが残る~~
  → 下の追記 3 で `albums.discid` ごと外して解消

**追記 3**（2026-09-23。ユーザ判断。上の未決を閉じる）: **`albums.discid` を落とし、DiscID を album の
同一性に使わない。** リリースの同一性は `mb_release_id`（`MUSICBRAINZ_ALBUMID`）→ album 行の 2 段にする。

- **DiscID は 1 枚ごとの値で、album の列には収まらない。** 複数枚組では album に DiscID が複数あり、スキャナは
  最頻値（どの 1 枚かは決まらない）を入れていた。しかも Inbox 経由の CD は NULL で、DB を作り直すと値が付き、
  追記先の判定・リリースキー（`disc:`）・album の照合（D-32）が作り直しの前後で変わっていた。リリースの MBID は
  全ディスクで 1 つなので、MusicBrainz にある複数枚組は MBID で自然に合流する
- マイグレーション `0024` で索引 `idx_albums_discid` と列を落とす。リリースキーは `mb:` → `album:<id>`
  （`placement::release_key` / `edit::rename` の `release_of`）、album の照合は MBID が 1 件一致 → 構成
  トラックの過半数。稼働 DB（721 album）は `discid` も `mb_release_id` も 0 件なので、失われる値は無い
- **DiscID の置き場所**: トラックのタグ `MUSICBRAINZ_DISCID`（1 枚の識別。吸い出しで付く）、
  `album_verifications`（ディスク単位の行）、rip.log / disc.cue。アルバム単位で DiscID を引く機能が要るように
  なったら、そのときに 1 枚ごとの表（`album_discs(album_id, disc_no, discid)`）を足す
- **CD とそれ以外は混ぜない。** `discid` を落とすと、MBID の無い CD の album と YouTube などの album が
  どちらも「MB キーの無い album」になり、Inbox の追記先（D-70）で互いに混ざる（D-7 の「同名 ≠ 同一
  リリース」に反する）。トラックのタグ `MUSICBRAINZ_DISCID` を **CD から来た印**にし（ファイルにあるので
  作り直しでも同じ判定）、`import::inbox::destination` は CD の件を CD の album にだけ、しかも album に
  まだ無い `disc_no` のときだけ採用する（MBID の無い複数枚組の 2 枚目を 1 枚目に合流させる。同じ番号は
  同名の別の盤なので降格）。CD でない件は CD でない album にだけ採用する。MBID のある件は従来どおり
  追記先を持たず、配置の `mb:` キーで合流 / 降格が決まる
- **同じ判定を 3 か所で通す**（codex 指摘）: (1) 追記先（`destination`）、(2) 再実行の「自分の成果物」
  （`self_album`。同じ音声の行が CD でない album にあっても、CD の件をそこへ合流させない）、(3) 登録の
  トランザクション（計画の後にタグ編集や別の配置で album が変わり得るので引き直す。MB キーの album は対象外）。
  「自分の成果物」は同じ音声の行がある album を**全部**候補に集め、件に MBID があればそのリリースの album、
  無ければ MB キーの無い album で件を入れられるものが**ちょうど 1 つ**のときだけ採る（D-29。音声の一致だけで
  別の album を自分とみなさない。件に MBID が無いのに `mb:` の album を採らない）。
  判定は `fits_plain_album`: CD かどうかは album の**全行**で見る（同じ音声の行を自分の成果物として除くと、
  他の album の行まで除いてしまう）、ディスク番号の重なりは自分の成果物の行を除いて見る（再実行）
- **購読の束ね先**（P4-16）: 束ねてある album が CD の album になっていたら同期を止める（Inbox はそこへ
  ダウンロードを追記しないので、揃えだけが進んで配置先と食い違う）。稼働 DB に CD の album は 0 件なので、
  既存の束ねの移行は要らない

**却下**: 両方の経路で `discid` を書き、合流は別の規則で救う（1 枚目だけの時点では 1 枚ものに見えるので、
2 枚目が来たときに `discid` を NULL に戻す更新が要り、規則が 3 か所に増える）。1 枚ごとの表を今作る（読む
機能が無い。遡及照合は STREAMINFO から TOC を作り直すので DiscID を使わない）。

**関連**: Inbox 経由の件への MusicBrainz 照会は、YouTube の件には行わない（曲の身元は `SOURCE_URL`、
D-70）。候補ゼロ・「どれも違う」で取り込んだ CD の引き直しだけを TASKS P4-21 に積む

---

## D-68 Inbox は 1 ディレクトリ = 1 件の承認キューにし、配置は CD と同じ経路で登録する

**決定**（2026-09-19。P2-10。D-20 の具体化）:

- **件 = 音声ファイルのあるディレクトリ。** `inbox` ジョブ（並列 1・固定キー）が Inbox を歩いて
  `inbox_items`（`rel_dir_key` で一意。状態 `pending` → `approved` → `placing` → `placed` | `failed`、
  `rejected`）と `inbox_files`（stat・コーデック・表示用タグ列・全タグ JSON）に写す。**正は Inbox の
  ファイルで行はキャッシュ**: stat が変わったファイルだけタグを読み直し、ディレクトリが消えれば行も消す
  （`placed` は 24 時間残す）。`approved` の件でファイルが変わっていたら `pending` に戻す。検出は
  `[inbox].poll_interval_secs`（既定 60、0 で自動なし）の周期投入と `POST /api/inbox/scan`
- **補正はアルバム単位とトラック単位の両方**（category / albumartist / album / date と、各トラックの
  disc_no / track_no / title / artist）。下書き（`proposal`）はタグから作る（最頻値、category は GENRE →
  `genre_category_map`）。承認の検証は CD の確定（D-65）と同じ厳しさ: album / albumartist / 各 title が
  空でない、`(disc_no, track_no)` は 1 以上で重複なし。不足のまま Library に入れない（D-20）
- **補正はファイルのタグに書く。** Library へコピーする tmp に、補正で変わるキーだけ
  `write_tag_changes` で書いてから置く。ファイルが正のまま再スキャンしても DB と一致する。Inbox の原本は
  move で消えるので、この書き込みは編集バッチ（巻き戻し）の対象にしない（Library のデータの書き換えでは
  なく、Library に入る前の整形）
- **配置は CD の配置（D-67）と同じ経路。** `pathgen::plan`（category 無しは `unsorted`、`disc_no` の
  最大 ≥ 2 なら `multi_disc`、リリースキーは MUSICBRAINZ_ALBUMID の最頻値があれば `mb:`、無ければ件ごとの
  新規キー）→ `library` の排他 → tmp + `RENAME_NOREPLACE` → 1 トランザクション登録（`source_type =
  'download'`、宛先 album のリリースキー再検証、同パス行の MD5 検証）→ 失敗時はこの呼び出しで置いた
  ファイルだけ片付ける。共通部分（1 ファイルの配置・後始末・リリースキー・album の検索 / 作成・行の
  登録）は `src/import/placement.rs` に置き、`cd::place` と `import::inbox` が使う。コピーに使う FD を
  fstat で承認時の行と照合し、コピーの後にも同じ FD を照合し、置いたファイルの音声の指紋が承認時に読んだ
  ものと一致することを確かめる（外れたら `Changed` → `pending` に戻して再承認。コピー = 検証済みの複製）。
  既知の同梱ファイル（cover 画像 / cue / toc / log）も移し、空になった Inbox のディレクトリを消す
- **状態遷移は CAS、`placing` はジョブの先頭で回復する。** `approve` / `reject` / `reopen` と worker の
  `approved → placing` は `UPDATE … WHERE state IN (…)`（`db::inbox::transition`）で、読んでから書くまでに
  他が動かした件を上書きしない。配置の途中でプロセスが落ちて `placing` のまま残った件は次の `inbox`
  ジョブが `approved` に戻して配置し直す（並列 1 なのでジョブ開始時の `placing` は必ず前の実行の残り。
  配置は冪等）。件の `placed` と normalize バッチも登録トランザクションの中で確定する（commit 後に落ちても
  `placing` が残らず、normalize の投入も欠けない。`edit::prepare_normalize_in`）。`placed` の件のディレクトリに走査で音声が見えたら `pending` に戻す（消せなかった原本や
  配置後に置かれたファイルを `placed` の裏に隠さない）
- **後続は rg / transcode に加えて normalize。** WAV / ALAC / AIFF は `[normalize].wav_to_flac` なら
  編集バッチを作って投入する（D-46 で P2-10 に先送りしていた自動投入。登録と同じトランザクション）。
  既存ライブラリの一括変換はこれまでどおり手動

**理由**: 承認キューの単位をディレクトリにするのは、購入分の zip を展開した形がそのまま 1 アルバムで、
Library と同じ「ディレクトリ = album」の規則に乗るため。トラック単位の補正を入れるのは、配信購入分の
TITLE / TRACKNUMBER の欠落や表記揺れが CD と同じくらい多く、外部ツールで直してから戻る往復が承認の
価値を削ぐため。配置を CD と共通化するのは、排他・冪等性・再検証・後始末の規則を 2 か所で別々に
守らないため。

**却下**: Inbox を直接スキャン対象にする（D-20）。補正を DB だけに持つ（再スキャンで元のタグ値に戻り、
不変条件 1 と矛盾する）。inotify（コンテナ越しに不安定）。件ごとに別のジョブ（並列 1 の固定キーで走査と
配置を 1 本にすれば、配置中の件を走査が触る競合が無い）。

**追記（2026-09-22、P4-18、D-81）**: inotify の却下理由を改める。実機は Inbox を bind mount していて同じ
カーネルなので inotify 自体は動く（動かないのは NFS / SMB をコンテナ内でマウントする構成。SMB からの投入は
Samba がホスト側で書くのでイベントになる）。それでも見送るのは、(1) ディレクトリごとの再帰 watch（zip 展開で
増えるサブディレクトリの登録競合）、(2) コピー中のイベント嵐に対する静穏期間、(3) queue overflow に備えた
ポーリングのフォールバック、が要り、フォールバックが残る以上「変化が無ければ投入しない」を別途入れる必要が
あって、それを入れると inotify の利点は検出の遅れ（最大 60 秒）だけになるため。毎分のジョブ行で一覧が埋まる
問題は指紋の比較で解く（D-81）。要るなら即時トリガとして後から足せる。

**未決**: `placed` の行を残す期間（今は 24 時間）。Inbox のサブディレクトリを 1 件にまとめる規則
（ディスクごとのサブディレクトリ `Disc 1` / `Disc 2` を 1 アルバムに束ねる。今はディレクトリごとに別の件）。

**追記**（2026-09-20、P3-4）: 配置の直後に、その album のアートワークをスキャンと同じ規則で解決し
thumbnail を投入する（`scanner::resolve_album_artwork_now`）。これまでは次のスキャン（起動時 / 手動）まで
画像が出なかった。YouTube のダウンロード（D-70）で新しい album ができる度に待たされるのが目立つため、
Inbox 配置全般の改善として入れた。失敗しても予約（`artwork_resolved_at = NULL`）が残るので従来どおり
スキャンが続きをやる。

## D-69 動画タイトルの解釈は外部のメタデータプラグインに任せ、spindle は汎用に保つ

**決定**（2026-09-19。P3-1 / P3-2）:

- **spindle はタイトルの慣習を知らない。** YouTube の動画タイトルとチャンネルからトラックのメタデータ
  （タイトル・アーティスト・アルバム・category）を決める処理は、外部コマンド（メタデータプラグイン）に
  JSON のプロトコル（SPEC §7.7）で問い合わせる。spindle が持つのは、プラグインの起動（引数配列・
  タイムアウト・終了コード検査・stderr のログ）、Response の検証、タグと `pathgen::TrackFields` への写像、
  category の語彙への自動追加だけ
- **チャンネル定義（チャンネル → artist / category、album 名の形）もプラグイン側。** spindle の設定は
  `[ytmusic].metadata_command` と `metadata_timeout_secs`（とダウンロード元の URL。P3-3）だけ
- **プロトコルは提供元に依らない形**（`source` / `op` / `item` を持ち、未知のフィールドは無視する）。
  `ok: false` の `reason` に `skip`（意図的に取り込まない）を持ち、要対応（`unmatched` /
  `unknown_channel`）と区別する。プラグインは判定できたかに関わらず終了コード 0 で返し、非ゼロ・不正な
  JSON・必須の値の欠落はプラグインの故障として扱う
- **category は無ければ語彙に追加する**（同じ canonical key があればそれを使う）。プラグインの定義が
  category の正で、初回起動の空 DB でも追加の操作なしに動く
- 参照実装は `AkashiSN/spindle-ytmusic-meta`（private）。宣言的ルール（TOML）のエンジンとフィクスチャ
  （Python 版 ytmusic のテスト 98 件から生成）とチャンネル定義を同梱した Rust のバイナリで、新パターンは
  Claude がそのリポジトリでルールとフィクスチャを 1 件ずつ足す

**理由**: spindle は公開リポジトリであり汎用に保ちたい。タイトルのパターン、チャンネル名、アーティストの
グループ、フィクスチャの実タイトルはすべて利用者固有で、ルールを TOML に外出ししてもエンジン側に
「`(from …)` / `with …` / `【合唱】` / `Live ver.`」といった慣習が残り、テストにも実タイトルが出る。
実行ファイルの境界で切れば spindle 側に固有名詞が一切入らず、プラグインは別言語（Python 版そのままでも）
でも書ける。

**却下**: spindle にルールエンジンと同梱 TOML を持つ（一度実装したが、エンジンに慣習が残るので履歴ごと
取り下げた）。ルールだけを別リポジトリに外出しして spindle が読む（同上）。GUI からのルール編集
（ルールの正が 2 か所になり、フィクスチャで守れない）。チャンネル定義を spindle の config.toml に置く
（固有名詞の置き場を 2 つに分けない）。

~~**未決**: ダウンローダ（P3-3）で `ok: false` のアイテムをどう見せるか。プラグインを Docker イメージに
どう同梱するか。~~ → D-70 で決めた（Inbox の `_unmatched` の件として見せる。プラグインは実行時マウント）。

## D-70 YouTube のダウンロードは Inbox に置くだけにし、判定の確認と既存アルバムへの追記は Inbox が担う

**決定**（2026-09-20。P3-3）:

- **ダウンローダ（`ytdl` ジョブ）は Library に触らない。** yt-dlp で音声（webm の Opus）を取り、
  ffmpeg で `.opus` に remux（再エンコードなし）し、プラグインの判定をタグに書いて **Inbox に置く**
  ところまで。配置・登録・後続ジョブは Inbox の承認と配置（D-68）がそのまま担う。プラグインが正しく
  判定したものも人が一度見てから Library に入る（自動で反映されたものを確認したい、という要求。
  D-68 の「承認キューを挟む」と同じ思想）
- **判定できなかったもの（`ok: false` の `unmatched` / `unknown_channel` / 未知の reason）も Inbox に
  置く**（`Inbox/youtube/_unmatched/<channel>/`）。TITLE に動画タイトルをそのまま書き、他は空にして
  Inbox の補正フォームで人が埋める。**受け皿を別に作らない。** `skip` はダウンロードせず終わる
- **タグに載らない情報はサイドカー `spindle-inbox.json` で渡す**（category、ファイルごとの判定・
  メッセージ・URL・チャンネル）。Inbox の行はキャッシュで DB に直接書かないという D-68 の規則を守るため、
  ダウンローダが Inbox の DB を触らない。走査は音声でないので無視し、`GET /api/inbox` が読んで件に
  付け、配置の成功時に消す（Library へは持っていかない）。人が手で置いた件には無いので従来どおり
- **Inbox は既存の album に追記できるようにする。** MUSICBRAINZ_ALBUMID が無く、宛先ディレクトリに
  active な album があり、その album にも MB キーが無ければその album を採用する（これまでは件ごとの
  新規キーで衝突 → failed）。「花譜のお歌」のように育つアルバムに曲を足すのが YouTube 取り込みの通常
  形なので、これが無いと成立しない。MB キー同士が違えば従来どおり衝突。TRACKNUMBER の無いファイルは
  採用する album の active な `track_no` の最大 + 1 から名前順に採番して提案し、承認時に採用 album の
  active なトラックと `(disc_no, track_no)` が重なれば 400 で先に直させる
- **重複取り込みは `SOURCE_URL` タグで防ぐ。** yt-dlp の `webpage_url`（正規形）をファイルに書き、
  Library（`track_tags`）か Inbox（`inbox_files.tags`）に同じ値があれば取り込まない。DB を消しても
  ファイルに残る（ファイルが正）。プロトコルの予約キーに加える
- **webm は `Archive/youtube/<id>.webm`。** Library のパスを写さない（リネームで追随できない）。
  DB に行は作らない（`archived_files` は GC の退避台帳で `eligible_after` が付く。追記のみの生データを
  GC の対象にしない）
- **プラグインは実行時マウント。** 参照実装は private で固有名詞を含むので、公開イメージに焼かず
  公開 CI からも pull しない。static バイナリをホストの `/mnt/ssd/apps/spindle/bin/` に置き、compose で
  `/usr/local/bin/spindle-ytmusic-meta:ro` にマウントする。起動時診断で実行できなければ警告（yt-dlp と同じ。
  起動は通す）
- **技術的な失敗だけがジョブの失敗。** 取り込み済み・webm の音声が無い・プラグインの故障は再試行しても
  変わらないので `JobError::Fatal`（バックオフせず `failed`）を足す。ネットワークの失敗は従来どおり再試行

**理由**: ダウンローダに配置・採番・排他を持たせる案（SPEC §7.7 の当初の記述）は、Inbox と同じことを
2 か所に持つうえ、判定を人が確認する場所が無い。Inbox に寄せれば downloader は「ファイルを置く」だけの
小さなジョブになり、確認・補正・受け皿・後続ジョブはすべて既存の経路で済む。

**却下**: `ytmusic_downloads` テーブルと専用画面（Inbox で代替できる量の UI とマイグレーションが増える）。
`ok: false` をジョブの失敗で見せる（直す場所が Jobs タブになり、補正もできない）。プラグインを
別イメージから `COPY --from`（private イメージを公開ビルドが pull できない）。webm を Library のパスに
写す（リネームで追随できない）。

**追記**（2026-09-20、実機の通し確認）: 実 URL で ダウンロード → Inbox（既存 album「花譜のお歌」への
追記、採番 246）→ 承認 → Library まで通り、ytmusic CLI の経路は置き換えられた。見つかった差: 承認画面の
下書きは `artist` を 1 値で見せるが、ファイルの `ARTIST` はプラグインの多値（「花譜」「理芽」）で、
未編集なら多値のまま Library に入る（上の規則どおり）。承認画面が「実際に書かれるもの」と違って
見えるのは良くないので、**承認画面は ARTIST の全値（`;` 区切り）と埋め込みのカバー画像を見せる**
（Inbox のファイルの `PICTURE` を返す `GET /api/inbox/:id/artwork/:hash` を足す。設計時に `:file` から
hash アドレスへ変更。下の追記）。P4-4。

**追記（2026-09-20、P4-4 の設計。codex の設計レビューで 2 往復）**:

- 下書きのトラックに **`keep_artists`**（ファイルの多値 ARTIST をそのまま保つ）を持たせ、意図を明示する。
  提案は多値なら true で、`artist` は全値を **`"; "`** で結合した表示文字列（foobar2000 等の多値の慣習。
  Library の一覧が `artist_display` に使う `", "` とは区別）。配置は true なら**現在の個数に関係なく** ARTIST に
  触れず（承認後にファイルが 1 値に変わって走査 → pending → 保存値の再表示、という経路でも安全側）、
  false なら `artist` の 1 値で置き換える（`;` で分割しない。分割規則を持ち込むと `;` を含む 1 値を
  表せない）。画面は元の値を境界の分かるチップで見せ、チェックを外すと 1 値の入力になる。チェックを
  出すのは多値のときだけで、保存値が true でもファイルが多値でなくなっていれば false に戻す
- **却下**: 結合文字列との一致を「未編集」の識別子に兼用する案（codex の指摘: `["A; B", "C"]` と
  `["A", "B; C"]` が同じに見える、1 値 `A; B; C` に畳む意図を表せない、同じ文字列を打ち直しても 1 値に
  ならない）。先頭値との一致（旧規則。画面と書かれるものが食い違う）
- 旧下書き（`keep_artists` 無し。failed / rejected にも残り、reopen 後は保存値が優先される）は旧規則で
  解釈する（`artist` が先頭値のままなら保つ）。互換を落とすと再承認で多値が 1 値に潰れる
- 画像の口は **`GET /api/inbox/:id/artwork/:hash`**（TASKS の `:file` から変更）。ハッシュアドレスなら
  Library の `/api/artwork/:hash` と同じく内容が不変で `immutable` + ETag が成り立つ。ファイル指定だと
  ファイルが書き換わったときに同じ URL で別の画像が返り、キャッシュを信用できない。`:id` で件に絞るのは
  他の件のファイルを開かせないため。**304 は実体の照合の後**（DB の `PICTURE` だけで 304 を返すと、実体が
  消えた / 差し替わった後も一致する ETag に 304 を返し「消失 / 不一致は 404」に反する）。MIME はタグの値を
  信用せず内容の sniff（D-49 と同じ規則）
- 代表画像: `inbox_files.tags` の `PICTURE` は画像種別を持たないので、走査が Library の `pick_embedded`
  （front cover 優先、無ければ先頭）で選ぶ画像を先頭に置き、画面は先頭を代表にする。件の代表は各ファイル
  代表の最頻（同数なら先に現れたもの）で、件の中の多数派を目安に見せる要約。配置後の album の代表
  （同梱カバー → disc / track 順で最初のトラックの埋め込み、追記先なら既存の代表）とは異なり得る。
  忠実なのはトラックごとのチップ / サムネイルで、見出しの画像は要約、と切り分ける
- 状態非依存で返す（rejected / failed でも reopen 前に確認したい。placed は実体が消えているので 404）。
  サムネイルは作らない（承認画面だけで使い、件は配置で消える。原寸を `<img>` で縮小して十分）
- 承認画面が見せるのは埋め込み画像だけ。同梱の `cover.jpg` は配置時に埋め込みより優先されるが、その
  表示は今回の範囲外（件のディレクトリを API のたびに歩くことになる。必要になったら走査で
  `inbox_items` に同梱カバーの有無を持たせる）
- 受け入れ: `tests/inbox_api.rs`（提案の結合と `keep_artists`、artwork の 200 / 304 / 404（不明 hash・他の件・
  実体消失で If-None-Match が一致しても 404・差し替え）/ 401、front cover が先頭）、`tests/inbox_job.rs`
  （`keep_artists` で多値を保つ・false で 1 値・旧下書きの互換）、`web/src/lib/inbox.test.ts`（`pictureOf` /
  `itemCover` / `draftFrom` の旧下書き）

**追記（2026-09-21。P4-13）**: 導線を操作タブの節から独立した YouTube 画面に移す（SPEC §12.6）。操作タブは
選択したトラックへの操作の場所で、選択と無関係なダウンロードが混ざって見つけにくく、投入後の行方も
ジョブ画面に埋もれていた。画面には ytdl ジョブの一覧（結果と「Inbox で確認」）を置き、`/youtube?url=` で
URL を受ける（ブックマークレット。同一 origin の GET なので CORS / CSRF の話が無い）。再生リストの展開時に
Library / Inbox に `SOURCE_URL` のある動画は投入しない（動画ごとのジョブが「取り込み済み」で失敗する
のは同じ結果だが、一覧が失敗 200 行で埋まる。P4-14 で既存曲に `SOURCE_URL` が付いたので、再生リストを
貼れば新しいものだけが落ちる）。`[ytmusic].ytdlp_args` で yt-dlp に毎回付ける引数を配列で渡せる
（`--extractor-args` / `--cookies`。P4-16 の同期が常用になると要る）。**UA / Referer は付けない**: yt-dlp の
YouTube 抽出は player client（web / ios / android …）の偽装で innertube API を叩くので、ブラウザ風の UA を
上書きすると client の偽装と食い違って弾かれる（yt-dlp の公式見解）。Referer も innertube には効かない。
ブロックへの対処は yt-dlp の更新（P4-12 (7)）と `ytdlp_args` の 2 つ。

**追記（2026-09-22、P4-19）**: 二重取り込みの検出は `SOURCE_URL` の一致だけでは足りないので、Inbox の承認
画面に**同名の警告**を出す。追記先の album に同じタイトル鍵（NFKD + casefold + 空白の畳み込み。`(Cover)` /
`【… Live ver.】` の注記は落とさない）の active な行があれば、そのトラックに `same_title`（track_id /
rel_path / duration_ms）を返し、画面が「⚠ Library に同名: <ファイル名>（長さ）」と出す。**承認は止めない**
（判断は人が行う、が D-70 の原則。同じ曲名の別テイクは正当）。理由: `SOURCE_URL` は補填（P4-14）で
`title-mismatch` / `no-track` になった行や、CD / 購入で入れた曲には付かず、再アップロードや切り抜きは URL が
違うので、同期は「持っていない」と判断して落としてくる。実データで測ると、同一 album 内の同名は 9,102 行中
27 グループ（119 行。多くはキャラ別 Solo ver. などの正当なもの）で、YouTube 由来の `〜のお歌` では 6 グループ。
そのうち VALIS「彷徨フォーエバー 【Live ver.】」は**長さが完全一致で URL だけ違う**（P4-16 のリハーサルで
気づかずに承認した二重取り込み）。判定を「注記まで含めて一致」に限るのは、緩めると毎回警告が出て無視される
ようになるため。長さを添えるのは、別テイクと本当の重複を人が見分ける唯一の手がかりだから。

## D-71 偽ハイレゾ検出は表示と絞り込みだけに使い、判定はカットオフの「崖」と実効ビットで下す

**決定**: 可逆かつ `sample_rate > 48000` または `bit_depth > 16` のトラックを `hirescheck` ジョブ
（読むだけ。track_id + audio_version の版付き、flaccheck と同型）で解析し、結果を `tracks.hires_check`
に記録して一覧のバッジ・固定フィルタ・DSL に出す。**判定を消費する自動処理は置かない**（P3-5、SPEC §7.10）。

- **用途は「知るため」に限る。** 具体的には、移行データに別トラックとして両方入っている `Hi-Res/`
  の 24/96 とアルバム直下の 16/44 の、どちらを残すかの判断材料。および配信ハイレゾの格付け
  （CD 由来の AccurateRip / CTDB に相当する裏付けが無い）。Derived（可逆はどのみち Opus 48 kHz）・
  配布ビュー・RG・リネームは判定に依らない
- **アップサンプリングはカットオフ周波数だけで判定しない。** `cutoff_hz ≤ 25 kHz` に加えて、
  カットオフ前後 1 kHz の落差 `cliff_db ≥ 30 dB` を条件にする。SRC のローパスは急峻な崖を作るが、
  アナログテープ起こしや静かなアコースティックは本物でも 22 kHz 前後から自然に減衰するため、
  カットオフだけでは誤検出する。崖の無いものは `inconclusive` にして人が見る
- **カットオフの探索は 1/3 オクターブで平滑化した系列で候補を決め、エッジは平滑化前の系列で
  詰める。崖も平滑化前で、同じエッジを中心に測る。** 24 kHz での 1/3 オクターブ幅は約 5.5 kHz
  あり、平滑化した系列の「最高ビン」をそのまま cutoff にすると窓の下端ぶん（最大 1/6 オクターブ、
  22.05 kHz の brickwall なら 24.75 kHz）上へずれて、主対象の 44.1 → 96 kHz を取り逃がす。
  また平滑化した系列で前後 1 kHz を比べると崖を自分でぼかして 30 dB に届かない。境界（完全ゼロの
  帯域、上端まで信号がある本物、Nyquist 直下のカットオフ、全無音）は SPEC §7.10 で有限値か NULL に
  定めてある。既定の `cutoff_hz` は 25 kHz: 48 kHz マスターの 24 kHz brickwall と Hann 窓の漏れ込み
  （数百 Hz）を拾える余裕を取る。本物の 96 kHz 録音は 30 kHz 以上まで伸びるのが普通で、崖の条件も
  あるので 25 kHz でも誤検出は増えない。
  実装時の補正（2026-09-20）: エッジは「床 + 10 dB を上回る最高ビン」ではなく **200 Hz 幅の dB 平均の
  段差が最大のビン**、探索範囲は候補の 1/3 オクターブ下まで。Hann 窓のサイドローブ（18 dB/oct）が
  しきい値を上回る範囲までエッジを押し上げるのを避ける（内容がフルスケール近くまで詰まった信号で
  数 kHz ずれ、24 kHz の brickwall を取り逃がしていた）。床は −150 dB を下限にする（完全ゼロの
  帯域で −200 dB に張り付くと遠い漏れまで拾う）
- **計測できたものが無ければ suspect より先に `inconclusive`。** 全無音は OR = 0 で実効ビットが
  求まらず（末尾ゼロが 32）、そのまま式に入れると `padded` になる。cutoff と実効ビットが
  ともに NULL なら疑いの判定に進まない
- **ビット深度は全サンプルの OR の末尾ゼロで見る。** `round(sample × 2^(bit_depth−1))` を i32 に
  戻して OR し、実効ビット = `bit_depth − 末尾ゼロ`。`≤ 16` なら `padded`。ディザやゲインを
  かけた水増しは検出できないが、それは「16 bit 相当の情報量」の証明が原理的にできない領域で、
  素の 8 bit ゼロ詰め（最も多い形）だけを確実に拾う。32 bit は f32 で正確でないので判定しない
- **計測値（`hires_cutoff_hz` / `hires_cliff_db` / `hires_effective_bits`）も保存する。** `upsampled` と
  `inconclusive` を分けるのは崖なので、崖の値が無いと 29.9 dB と 3 dB が同じに見える。 しきい値は `[hires]` に
  既定値付きで置くが、判定は検査時に確定し、しきい値を変えても既存の結果は書き換えない
  （再判定は手動投入）。計測値が残っていれば目視の判断と DSL（`%cutoff% LESS 23000`）は
  いつでもできる
- **対象は 44.1/48 kHz かつ 16 bit の可逆を含まない。** 非可逆由来の可逆（44.1 kHz の FLAC が
  16 kHz で切れている）の検出は、対象が Library の可逆全曲に広がるうえ、しきい値の誤検出リスクが
  高い（MP3 128k の 16 kHz と本物の暗い録音の区別がつかない）。要望が出るまで作らない

- **並列度は `max(1, CPU コア数 / 2)`。** ジョブの Semaphore は種別ごとに独立で、rg（コア数）・
  transcode（コア数 − 1）・flaccheck（コア数）と同時に走ると合計がコアを超える。デコード + FFT は
  `flac -t` より重く、スキャン完了時に rg / transcode と一緒に自動投入されるので、hirescheck 側を
  半分に抑える。CPU 系ジョブ共通の予算は残課題（SPEC §17）
- **`JobType::version_field` に `Hirescheck` を加える。** これが無いとワーカーの stale ゲートと
  `track_locks` が効かない（flaccheck と同じ落とし穴）

**理由**: 仕様の「任意機能」であり、自動処理に結びつけると誤検出の被害が Derived や削除に
及ぶ。表示に留めれば誤検出のコストは「人が見て無視する」だけで済む。

**却下**: カットオフだけで判定（自然なロールオフの本物を偽と言う）。計測値を残さず判定だけ保存
（しきい値を変えるたびに 24/96 の全曲を再デコードする）。判定を読み取り時にしきい値から導出
（DSL と固定フィルタが設定値に依存し、SQL に閾値を毎回バインドする割に得るものが小さい）。
重複グループ（同じ `audio_md5`）への統合表示（`Hi-Res/` と 16/44 は PCM が違うので `audio_md5` は
一致せず、別マスターを結びつける仕組みは無い。P3-5 の範囲を超える）。非可逆由来の可逆の検出
（上記）。

**追記**（2026-09-20、実機で較正）: TrueNAS の対象 124 本（ALAC 24/96 と 24/48）で観察した結果、
本物の 96 kHz（106 本）は cutoff 25.4〜43 kHz / 崖 0.8〜8 dB、疑わしいもの（複数プロデューサーの
寄せ集めコンピ 16 本と 48 kHz マスターの 2 本）は cutoff 20.2〜24.8 kHz / 崖 4〜21 dB だった。
スペクトルを見ると、SRC の段差は「エッジ直下の音楽の残り（−67 dB 程度）」と「上げた後の床
（−83 dB 程度）」の差で決まり、実 SRC では 12〜21 dB にしかならない。30 dB は帯域内フルスケールの
合成信号でしか出ない値で、17 本が全部 `inconclusive` になった。また 15 kHz からなだらかに落ちて
22 kHz 手前で床に沈む型（段差ゼロ）があり、これは崖をどう測っても拾えない。本物はすべて
25.4 kHz 以上まで伸びていたので、cutoff の絶対値で拾う。決定:
- `[hires].cliff_db` の既定を 30 → **10 dB**（本物の膝 8.1 dB は届かない）
- `[hires].hard_cutoff_hz = 22500` を追加。cutoff がこれ以下なら崖に関わらず `upsampled`
  （44.1 kHz の Nyquist + 余裕。`cutoff_hz` 以下であること）。22.5〜25 kHz（48 kHz マスター）は
  従来どおり崖で判定する
- 既存の結果は書き換えない（規則どおり）。実機は手動で再投入した

## D-72 外部メタデータは MusicBrainz を「盤の識別」にだけ使い、値は公式表記を手で入れる。Discogs / VGMdb / .fpl は作らない

**決定**（2026-09-20。SPEC §17 の残課題を閉じる）:

- **外部ソースは MusicBrainz だけ。** Discogs も VGMdb も作らない。`.fpl` 書き出しも作らない（SPEC §10 の
  「非対応」で確定）
- **候補から写すのは既定で「盤を見分けるのに要る最小限」**: `ALBUM` / `ALBUMARTIST` / `DATE` /
  `DISCNUMBER` / `DISCTOTAL` / `TRACKTOTAL` と MusicBrainz の id（`MUSICBRAINZ_ALBUMID` /
  `MUSICBRAINZ_DISCID`。同一性と遡及照合の鍵（DiscID は D-67 追記 3 で album の同一性からは外し、CD の印と
  1 枚の識別に使う）。要らなければプロパティタブで消せる）。`LABEL` /
  `CATALOGNUMBER` / `BARCODE`、トラックのタイトル / アーティスト / ISRC は既定では写さず、
  「全部写す」を選んだときだけ写る。トラック行は番号と長さだけの空行になり、公式サイトの貼り付け解析
  （D-65）で埋める。空のフィールドはタグに書かない（`tagRows` が落とす。従来どおり）
- **プロパティタブに foobar2000 の Properties と同じ「フィールドの削除」と「フィールドの追加」を置く。**
  値のダブルクリック編集（既存）に加えて、フィールド（ラベル）自体を右クリック（または行末の ×）で
  消し、新しいキーを追加できる。どちらも既存の一括編集 op（`delete` / `set`）で選択全体に効き、
  バッチとして巻き戻せる。サーバの変更は無い
- P4 のタスク: 確定フォームの「写す範囲」（既定 = 最小限、「全部写す」）、プロパティタブのフィールド
  削除 / 追加

**理由**: foobar2000 での運用実績。MusicBrainz も freedb も表記揺れが激しく（複数アーティストの並び、
キャラクター名と声優名の書き方）、結局すべて手で埋めることが多かった。そのとき外部ソースの
価値は「複数枚を取り込むときにどれがどれか分からなくならない」ための識別だけで、値は公式サイトや
ジャケット裏の表記から入れていた。Discogs の表記も登録者次第で MusicBrainz と同じ問題を持ち、
識別としては DiscID（TOC）に劣る。カタログ番号や JAN は要らず、既存ライブラリのタグもアルバム
アーティスト・アルバム名・年・トラック数程度で足りている。

**追記 2**（2026-09-23。P4-20。上の「既定で最小限」を CD 画面について覆す）: **CD 画面の写す範囲の
既定は「全部写す」**にする。「最小限」は選べるまま残す。

理由: P4-20 で CD 画面はトラック表が主役になり、「候補を選んだら表の `Track 01` が実名に変わる」のが
期待される動きになった。既定が最小限だと、候補を選んでも表が空のままで、ユーザから見ると候補を
選んだ意味が分からない。D-72 の「値は公式表記を手で入れる」という考え方自体は変えておらず、
**入り口の既定を変えただけ**（写した値は Inbox の承認画面で直せるし、「最小限」に切り替えれば
元の挙動になる）。
Inbox の承認フォームはこの範囲を使っていないので影響しない。

**却下**: Discogs をカタログ番号検索の補助に残す（値を取っても直すので効かない）。VGMdb（非公式
ミラーのスクレイピング依存）。Cover Art Archive（**→ D-82 で覆した**。当時は「MB で当たる盤の
ジャケットは取れるが、要望が無い。アートワークは埋め込み / 同梱 / 手動で足りている。D-60」と判断した
が、候補が複数出たときの見分けに要るという要望が出たので引くことにした）。AcoustID（配信音源を MB に結びつける穴を
埋めるが、要望が無い）。gnudb / iTunes Search / Deezer / Spotify（文字列照合で、識別にもならない）。

## D-73 CPU 系ジョブは種別ごとの上限に加えて共通の並列予算（= CPU コア数）を取る

**決定**（2026-09-20。D-71 の残課題。P4 で実装）:

- `worker.rs` に CPU 系（`rg` / `transcode` / `flaccheck` / `hirescheck`）が共有する Semaphore を
  1 本足し、許可数は CPU コア数。該当種別は種別ごとの Semaphore（今のまま）に加えてこの予算も取る。
  種別単独のときは今と同じ速さで、複数種別が同時に走るときだけ合計がコア数に収まる
- 予算の取得順は「種別 → 共通」。共通が取れなければ種別の許可を持ったまま待つ（ジョブは
  `running` にしない。キューの順序は変えない）
- `GET /api/jobs` の `concurrency` に `cpu_budget` を足して画面に出す
- 受け入れ: 種別ごとの上限を超えないこと（既存）に加え、4 種を同時に投入したとき実行中の合計が
  コア数を超えないこと（`tests/jobs.rs`。ハンドラを sleep にして running の数を数える）

**理由**: 種別ごとの Semaphore は rg = コア数、transcode = コア数 − 1、flaccheck = コア数、
hirescheck = コア数 / 2 で、スキャン完了時に 4 種がまとめて投入されると最悪 3.5 × コア数の重い
処理が同時に走る。NAS では I/O も競合し、同じプロセスの axum の応答が鈍る。種別の上限を下げるだけ
だと単独で走るときも遅くなる。

**却下**: 上限を下げるだけ（単独が遅くなる）。現状維持（自動投入は 1 日 1 回程度だが、初回スキャン
直後は数千件が一度に走る）。

**追記（2026-09-20。P4-1 の実装）**: 共通が取れないときは「種別の許可を持ったまま待つ」ではなく、
**種別の許可も手放して claim せず、次の周回（実行中のタスクの完了で wake が来る）で試す**。スケジューラは
1 本のループなので、そこで待つと他の種別の claim まで止まる。ジョブは queued のままで順序も変わらない
ので、決定の意図（running にしない・順序を変えない）は同じ。`cpu_budget` は `concurrency` の中ではなく
`GET /api/jobs` の兄弟フィールド（`concurrency` は種別名をキーにした表で、画面がそのまま行にするため）。
`JobType::cpu_bound` が対象種別を決める。**予算を分け合う種別は 1 件ずつラウンドロビンで claim し、
次の周は最後に claim した種別の次から始める**（種別名順に空きが尽きるまで取ると、先頭の flaccheck のキューが
尽きるまで rg / transcode / hirescheck が始まらない。codex のレビュー指摘。当初は「開始位置を周回ごとに
1 つ進める」だったが、何も取れない周回（tick・別の wake）でも進むため開始位置が実質ランダムになり、
同じ種別に続けて予算が回ることがあった。2026-09-22 に CI で観測して修正）。受け入れは `tests/jobs.rs`（コア数 2 で
rg + flaccheck を 3 本ずつで実行中の最大が 2、4 種を種別名順に固めて 3 本ずつ投入しても先頭 6 本の開始に
4 種が揃う、thumbnail は予算に縛られず 4）。

## D-74 album gain は album ごとの属性。既定 off、CD 取り込みは on、Inbox の承認画面で選ぶ

**決定**（2026-09-20。P4-5 で実装）:

- `albums.album_gain`（真偽、既定 false。マイグレーション 0017）。**true の album だけ** rg を album 単位
  （`rg:album:<id>`。全曲デコード + ゲート付き集計）で投入し、false の album は track 単位
  （`rg:track:<id>`。対象の曲だけ）。`rg_album_*` は true の album にしか付かず、書き出しも同じ
  （false なら album のキーは書かず、あれば消す。D-48 の「値の無いキーは消す」）
- **CD 取り込み**（`cd/place.rs`）で作る album は **true**。アルバムとして通して聴く単位だから
- **Inbox の承認画面**に「album gain を計算する（アルバム通し再生用）」のチェックボックス。既定 off。
  既存 album への追記なら、その album の現在値が初期値で、変えると album の属性も変わる
- アルバム画面（SPEC §12.6）で後から切り替えられる。false → true で album 単位の rg を投入、
  true → false で `rg_album_*` を NULL にして「RG 未書込」に戻す（次の書き込みで album のキーが消える）
- 既存 album はすべて false、既存の `rg_album_*` は 0017 で NULL に揃える（ファイルには書かない）

**追記（2026-09-20。P4-5 の実装）**: 切り替えの置き場は**操作タブ**の「ReplayGain / FLAC」節
（選択行が属する album ごとのチェックボックス、`PATCH /api/albums/:id`）。アルバム画面は無く、アルバム
一覧は表を絞るだけなので。CD の配置は**合流（複数枚組の 2 枚目）でも on** にする。0017 で album の値を
消した行は `rg_written_at` を NULL に戻し `rg_scanned_at` を 1 進める（Derived の `R128_ALBUM_GAIN` が
タグ上書きで追随する）。rg ハンドラは**書き込み時に属性を読み直し**、album 単位の job でも off なら
album の値を書かず、track 単位の job は on の album に属する track の album 値を据え置く（投入と切り替えの
競合で値が嘘にならない）。**スキャンは rg を自動投入しない**（D-47 のまま。新規・音声差し替えの
トラックは `POST /api/rg`（`no_rg` フィルタ）か取り込み経路で解析する）。`store` は `rg_scanned_at` を
前の値より小さくしない（off で `now + 1` へ進めた直後に同じ秒の解析が保存しても巻き戻らない）。
transcode は Derived に記録する同じトランザクションで元の世代（音声版 / tag_version / 画像 / RG 世代 /
所在）を読み直し、読んでから書く間に動いていれば同じジョブを再キューして揃え直す（属性の切り替えは
track lock を取らず、running の間の投入は dedup で弾かれるため。音声版が動いた場合は再キューした
ジョブが版のゲートで終わり、scanner が投入した新しい版のジョブが揃える）。
- `POST /api/rg { selection }`、承認後・配置後の自動投入は album の属性を見て投入単位を
  決める。設定ファイルに `[replaygain].album_gain` は置かない（album ごとに決まる）
- **RG の一致判定（`rg_written_at`）は完全一致のまま。** 既存ライブラリの RG タグ（foobar2000 / ytmusic
  が書いた track gain。9,099 本すべて track のみ、album は 0 本）は spindle の解析値と丸め・実装差で
  0.01〜0.1 dB ずれるので、初回の「解析値をタグに書く」で全ファイルが書き換わる。**これは許容する**
  （spindle の値で統一する）。以後は、書いた値が DB と完全一致して `rg_written_at` が立ち、再解析は
  音声が差し替わったときと手動だけで、同じ音声なら同じ値なので二度と書き換わらない

**理由**: 実機の通し確認で、YouTube の 1 曲を「花譜のお歌」（245 曲の育つコレクション）に追記した
だけで album 単位の rg が走り、246 曲すべてをデコードし直した。album gain は「全曲をつなげてゲート
した値」なので各曲の値から合成できず、曲が増えるたびに全曲の再解析と全ファイルの書き換えになる。
運用はアルバムをまたぐシャッフルが主で、既存ライブラリも track gain だけだった。album gain を
使うのはアルバムとして通して聴く CD だけで、それ以外は必要なときに人が選ぶ。

**却下**: 全体設定で album gain を off（CD の album gain まで消える）。育つコレクションだけ off
（判定基準が曖昧で、なぜ計算されないかが見えない）。RG の一致判定に許容差（既存の値を尊重して
書き換えを避ける案。spindle の値で統一したいので採らない）。

---

## D-75 Apple 向けに `aac` 系統の Derived を足す。RG は焼き込み + iTunNORM 0 dB、多値は結合、プレイリストは出さない

**決定**（2026-09-20。P4-7 で Derived を系統化、P4-8 で `aac` 系統を実装。仕様 SPEC §7.6）:

- Derived を**系統（variant）ごとに 1 本**にする。`opus`（Android の同期・Web 再生・配布ビュー。今まで
  どおり）と `aac`（Mac のミュージック.app へ取り込む Apple 向け）の 2 つで固定。パスは
  `Derived/<variant>/` 以下に Library のミラー、`derived_files` の主キーは `(track_id, variant)`、
  設定は `[encode.derived.<variant>]`（`enabled` / `bitrate`。`aac` は `lossy_sources` と
  `multi_value_separator` も）
- **エンコーダは ffmpeg 内蔵 `aac`**（`-b:a 256k`。ABR に近く真の VBR ではない）。追加依存を持たない
- **非可逆原本も AAC へ**（`lossy_sources = true`。D-8 の例外）。原本が AAC でも再エンコード（2 本。
  stream copy では焼き込みとリサンプルが効かない）
- **ReplayGain は track gain を音声に焼き込み**（クリップ防止に peak で上限）、タグには `iTunNORM` を
  0 dB 相当で書く。`REPLAYGAIN_*` / `R128_*` は書かない。**RG 未解析なら作らず待つ**（rg の保存で投入）。
  RG の解析世代が変わったら再エンコード（`opus` はタグ上書きで済むが、`aac` は音声に入っている）
- **多値フィールドは `" & "` で 1 値に結合**（設定で変更可）。ミュージック.app は複数値の 1 つしか
  見せないため
- 画像は 768 の JPEG（`covr` の WebP は読まれない）
- **設定の世代を行に持つ**: `audio_profile`（codec / bitrate / サンプルレート規則 / 焼き込み方式）が違えば
  Encode、`tag_profile`（区切り / iTunNORM 規則）が違えば Retag。区切りやエンコーダ引数を変えたときに
  既存の Derived が追随する（音声版だけを見る今の判定では UpToDate のまま取り残される）
- `enabled = false` は**凍結**（作らない・触らない・既存行は使い続ける）。GC は系統を区別しない
- **実装（P4-7）**: 系統の設定は起動時に `derived_variants` 表へ写し（`db::derived::sync_variants`。
  `export_profiles` の `fb2k_prefix` と同じ流儀。D-55）、投入判定（`enqueue_if_stale` / `enqueue_all_stale`）
  は表から系統ごとの設定を引く（呼び出し側は設定を持ち回らない）。0018 より前に queued だった旧 payload
  （`variant` 無し）は opus として扱う。`transcode` の `Resolved` に設定を持ち、表に無い系統のジョブは
  何もせず Done。テストのフィクスチャは `common::enable_opus_variant` で表を作る
- **配布ビューもプレイリストも `aac` には作らない。** `delivery` は `opus` 系統に固定。ミュージック.app
  へは `Derived/aac/` のファイルをそのまま取り込み、プレイリストは Apple 側で作る
- **実装（P4-8）**: `derived_variants` に `lossy_sources` / `multi_value_separator` を 0019 で足す（`eligible`
  が投入判定で前者を、ハンドラがタグ結合で後者を表から引く。`audio_profile` / `tag_profile` の文字列に
  埋めて解析し直すより素直）。`aac` の対象は `!missing && channels ∈ {1,2} && (lossless || lossy_sources)
  && RG 解析済み`（`rg_scanned_at` / `rg_track_gain` / `rg_track_peak` の 3 つが揃っている。時刻だけの行で 0 dB
  の焼き込みを確定させない）。節を省略したときの `enabled` は aac だけ false（既存 config のまま新版を
  起動しても始まらない）。エンコードは ffmpeg 1 パス（`-af volume=<gain>dB [-ar 48000] -c:a aac
  -b:a <k>k -f mp4`。opus のような中間 WAV は要らない）。焼き込み量は `min(track_gain, −20·log10(peak))`
  で、peak は true peak（1.0 超なら減衰側）。これはエンコーダ入力の上限で、AAC 再符号化後のオーバー
  シュートは保証しない（安全余裕は入れない。受け入れテストは ebur128 の測定に ±0.5 dB の許容）。
  gain が有限でなければ 0、peak は有限かつ > 0 のときだけ上限を掛ける。`iTunNORM` は 1〜2 値目 `000003E8` に加えて 3〜4 値目
  （同じ 0 dB の基準 1/2500 表現）を `000009C4` で埋める（0 のままだと読む側の解釈が不定になりうる。
  iTunes が読むのは 1〜2 値目）。MP4 のタグは Vorbis 名を lofty の `ItemKey` に写像して標準 atom へ、
  写像できないキーは `----:com.apple.iTunes:<KEY>` のフリーフォームへ直接置く（lofty の generic Tag は
  未知キーを捨てる）。`iTunNORM` だけは内部キーの大文字化に関わらず atom 名を固定する。画像は thumbnail と同じ変換の JPEG 版（`thumbs/<hex>/768.jpg`）。RG の解析世代
  （`src_rg_scanned_at`）の差分は `aac` では Encode。album gain の on / off も世代を進めるので、その album
  の aac は作り直される（track gain しか使わないが、値ベースの判定に列を足すより単純。まれな操作）。
  `lossy_sources` を true → false にしても既存の非可逆の行とファイルは消さない（Skip = 凍結と同じ）。
  ハンドラは `OpusEncoder` と `AacEncoder` の両方を持ち、系統でエンコーダ・タグ書き込み・画像の形式・
  記録する bitrate を選ぶだけで、期待パスの予約・占有・配置・退避・drift の再キューは共通
- **実機の順序（P4-8）**: 実機の 9,099 本のうち `rg_scanned_at` があるのは 270 本（残りはファイルに
  foobar 時代の track gain タグがあるだけで DB に値が無い）。「RG 未解析は作らず待つ」ので、先に
  `POST /api/rg` を全件流し（解析値は DB にしか書かない。Library のタグへの書き込みは別の
  `POST /api/rg/write` で、それは aac の前提ではない）、完了後に `[encode.derived.aac]` を有効にして
  デプロイする。ファイルの
  既存 `REPLAYGAIN_*` を暫定値に使う案は、値を spindle で統一する方針（D-74）と食い違うので採らない。
  RG 未解析でも焼き込み無しで作って解析後に作り直す案は、二度エンコードで CPU 時間が倍になるので採らない

**理由**: Apple 純正の「ミュージック」は Opus を読めず、ReplayGain タグも読まない（独自の Sound Check =
`iTunNORM` をトラック単位で、端末の設定が ON のときだけ適用）。端末設定に依らず音量を揃えるには焼き込み
が確実で、焼き込み済みの上でサウンドチェックが ON でも二重にならないよう `iTunNORM` を 0 dB で置く。
アルバムをまたぐシャッフルが主な運用なので album gain は使わない（D-74）。Apple 側の取り込みは
ファイルだけで足りるので、プロファイル `apple` の m3u8 は YAGNI。

**却下**: `fdkaac`（Debian non-free。真の VBR で品質にも定評があるが、依存が 1 つ増える。256k なら
内蔵 `aac` で実用上透過）。`iTunNORM` のみ（端末で ON にする必要があり、換算式と Apple 側の扱いが
不確か）。焼き込みのみ（サウンドチェック ON の端末で Apple が独自に計算した値が重なる）。非可逆原本を
除外する（Apple 側で YouTube 由来の 1,513 本が欠ける）。AAC 原本の stream copy（焼き込みと矛盾。
複製して実ゲインの iTunNORM を書く案は、その 2 本だけ端末設定に依存する）。`enabled = false` で
ファイルを消す・GC から除外する（消すのは再エンコードの往復、除外は孤児の意味が変わる）。系統ごとに別の root（`paths.derived_aac`。
データセットを分ける需要が無く、GC と孤児回収が二重になる）。プロファイル `apple` の m3u8
（SMB の絶対パス / 相対パス。取り込みがファイルだけなので不要）。

## D-76 終端を書けなかった `running` は再試行で救い、残ればワーカーが稼働中に回収する

**決定**（2026-09-20。P4-8 の実機で観測した取り残しの対策。P4-9）:

- **終端遷移の書き込みを再試行する。** `run_one` の終端トランザクション（未 flush の進捗・
  `track_locks` / `job_mutexes` の解放・状態遷移）が `Err` を返したら、1, 2, 4, 8, 16, 32 秒の間隔で
  再試行する（6 回、約 1 分）。ハンドラの結果は `anyhow::Error` を含むので、書く前にメッセージ化した
  Clone 可能な形（`Terminal`）にしてから閉包に渡す。再試行の間は種別の許可と CPU 予算を**持ったまま**
  （DB が書けない間に次のジョブを始めても claim から失敗する。持ったままなら詰まりが見える）。
  使い切ったら今までどおり `failed` としての記録を 1 度試み、それも駄目なら `running` のまま手放す。
  停止（shutdown）が来たら再試行の途中でも abort し、DB は `running` のまま起動時リカバリに任せる
  （今と同じ）
- **稼働中の回収。** ワーカーの周回は claim の前に `sweep_cancel_requested` を書き込みで呼んでいる。
  同じトランザクションで `state = 'running'` の id を読み、プロセス内の実行中表（`Jobs.running`。
  cancel 用の token 表）に無い行を回収する: `cancel_requested_at` が立っていれば `cancelled`、
  そうでなければ `queued`（`started_at = NULL`。`attempts` / `run_after` は触らない = 起動時リカバリと
  同じ）。その `job_id` の `track_locks` / `derived_path_locks` / `job_mutexes` を消し、`last_error` に
  「終端を記録できないまま running に残っていたので再キューした」と書き、ログと SSE にも出す
- **実行中の登録を claim の直後に移す。** 今は `execute` タスクの先頭で `track_running` を呼ぶので、
  claim の直後の周回で「DB は `running` だが表に無い」瞬間があり、回収が本物を戻してしまう。token の
  生成と登録を `claim_one` の spawn 前（スケジューラと同じタスク、claim と順序が固定）に移す。
  停止時に claim 済みを `queued` へ戻す経路では登録しない。これで `started_at` の閾値は要らない。
  単一インスタンス前提（SPEC §8）なので、表に無い `running` は「このプロセスが終端を書けなかった行」
  以外に無い
- 起動時リカバリ（`recover`）はそのまま。回収はその稼働中版で、対象を「表に無い行」に絞っただけ
- 受け入れ（`tests/jobs.rs`）: ハンドラが `PRAGMA query_only = ON` を writer に掛けて `Done` を
  返し、別タスクが数秒後に OFF にする → `done` で `attempts = 0`（再試行で完走）。行を直接 `running`
  にしてロック 3 種を入れ、ワーカーを起動 → `queued` に戻りロックが消え `last_error` に回収の旨、
  その後ハンドラが走って `done`。cancel 要求付きは `cancelled`。数秒 sleep するハンドラの本物の
  `running` は戻されず `done`

**理由**: 2026-09-20 の実機で、開発コンテナと共有する ssd プールが cargo の `target/` で満杯になり、
transcode 88 本の終端書き込みが SQLITE_FULL で失敗、`failed` としての記録も同じ理由で失敗して
`running` のまま残った（ロックも残る）。空きができた後も次回起動まで詰まったままで、docker のログも
満杯で欠けていたので、画面の「実行中 88」以外に手掛かりが無かった。一時的な障害なら再試行で仕事の
結果を失わずに済み、長い障害でも回収で再起動なしに自力で戻る。両方あると、短い障害は透過で、長い
障害は再実行（ジョブは冪等）で済む。

**却下**: 無限に再試行する（許可を永久に握り、DB 固有の恒久的な失敗で種別が止まる。上限 + 回収で
同じ結果に届く）。回収を N 秒間隔の別タイマーにする（周回ごとの書き込みに `SELECT id … WHERE state =
'running'` を足すだけで済み、索引 `idx_jobs_queue` があるので安い。タイマーを足すと停止と wake の
扱いが増える）。`started_at` に経過秒の閾値を置いて競合を避ける（登録の順序を固定すれば要らず、
閾値はテストの時間依存を増やす）。回収で `attempts` を進める（ジョブの失敗ではなく基盤の障害。
起動時リカバリも進めない）。

## D-77 MP4（ALAC / AAC）の任意キーは `----:com.apple.iTunes:<KEY>` のフリーフォーム atom。読み書きの写像は 1 か所

**決定**（2026-09-21。P4-11。仕様 SPEC §7.5「形式ごとの写像」）:

- Library の MP4 への tagwrite は lofty の generic `Tag` を経由せず、ilst を直接編集する
  （`domain::tags::apply_ilst`）。Vorbis 名が lofty の `ItemKey` に写像でき MP4 の atom を持つキーは
  標準 atom（`©nam` `trkn` `cpil` `----:com.apple.iTunes:CATALOGNUMBER` 等。値の encode は lofty の
  `Tag` → `Ilst` 変換に載せ、`trkn` / `disk` の対や bool の special-case を借りる）、写像表に無いキーは
  `----:com.apple.iTunes:<KEY>` のフリーフォーム atom（多値は 1 atom の複数値）。`iTunNORM` は内部
  キーが大文字でも綴りを固定する
- 読み側（`collect_mp4`）も同じ写像。標準 atom は今までどおり generic `Tag` 経由で Vorbis 名に、
  generic `Tag` が捨てるフリーフォーム atom は `mean = com.apple.iTunes` に限って名前を大文字化した
  キーで取り込む（`TagSet` はキーを大文字化する。標準 atom と同名になるものは標準 atom を優先）。
  他の `mean` は読み書きとも触らない
- 置き換え・削除はフリーフォームの名前を**大小文字を無視して**消してから書く。外部ツールが
  `MyKey` で書いた atom を spindle が `MYKEY` で上書きしたとき、旧綴りの atom が残ると読み戻しで
  多値に見えて op が failed に閉じる
- Derived の `aac` 系統（D-75、`write_mp4_tags`）も同じ `apply_ilst` で書く（空の ilst に
  `TransferTags` をキーで束ねて渡す）。別名キー（LABEL / ORGANIZATION、TRACKTOTAL / TOTALTRACKS）が
  両方あれば後勝ち、は変えない
- MP3 / WAV 等は generic `Tag` のまま。写像できないキーは書けず、読み戻し照合で failed に閉じる

**理由**: 2026-09-21 の実機（P4-3 の確認）で、ALAC のプロパティタブに `SPINDLETEST` を足すと tagwrite が
「書き込み結果が意図と一致しない（この形式では表現できない）」で失敗した。lofty の generic `Tag` は
`ItemKey` に無いキーを書けず、読み側でも `Ilst` → `Tag` の変換がフリーフォーム atom を捨てるので、
外部ツール（foobar2000 / Picard）が書いた独自キーが DB に載らない = ファイルが正（不変条件 1）に
反していた。P4-8 が Derived 向けに書いたフリーフォームの写像を Library の読み書きに使えば解け、
写像を 1 か所にすると Derived と Library でキーの見え方がずれない。

**却下**: 既存の ALAC は FLAC 正規化でいずれ消えるので放置（読み側の取りこぼしは残るし、Inbox に
来る m4a も同じ経路）。lofty の `preserve_format_specific_items`（companion tag）に頼る（読み側の
取り込みは解けず、書きは global option の状態に依存する）。フリーフォームの名前を元の綴りのまま
読む（`TagSet` は大文字化する規約で、FLAC のキーも大文字化して DB に載っている。綴りを残すには
DB とハッシュの規約を変える必要があり、往復の二重化は削除側を大小文字無視にすれば防げる）。

---

## D-78 再生リストの購読は album を `album_id` で束ね、同期は「列挙 → 揃え → 投入」を毎回 DB から計算し直す

**決定**（2026-09-21、P4-16。SPEC §7.7「再生リストの購読と同期」）:
- **購読 = 再生リスト 1 本 → Library の album 1 つ**（`playlist_subscriptions`）。追記先の同一性は
  `album_id`（登録時は NULL。同期か配置が Inbox の追記先と同じ規則（`destination_of`）で引けたときに
  CAS で束ねる。D-32 で album 全体の移動は id を維持する）。`albumartist` / `album` / `category` は束ねる前の
  初期値と表示用で、PATCH で変えると NULL に戻る。同じ追記先の購読は 1 つだけ（`target_key` UNIQUE と
  `album_id` の部分 UNIQUE で DB で閉じる）
- **同期ジョブの順序は列挙 → 追記先 → 照合 → 番号揃え（tags → rename）→ ytdl 投入**。購読由来の ytdl は
  `TRACKNUMBER` = 再生リストの位置を書くので、先に既存の行を揃えて隙間を空ける。揃え終わる前の失敗では
  投入しない
- **phase は永続化しない。** 各段の前に追記先の active 行に pending の op が無くなるまで待ち、差分は
  待った後の DB から計算し直す。rename の候補は「現在の `TRACKNUMBER` が目標位置と一致する対象行のうち
  ファイル名の先頭の番号が合っていないもの」で、今回のバッチの applied 集合には依らない。**改名は
  ファイル名だけ**で、ディレクトリは今のまま（リハーサル環境で album の category が未推定だったため、
  テンプレートの dir で動かすと album ごと `_Unsorted` へ移った。名前の書式だけ違う行も触らない）
- **ytdl の dedup `ytdl:<url>` は変えない。** Duplicate は満たしたと数えず「別の投入が走行中」として結果に
  出すだけ。承認されれば次の同期が Library で拾い、失敗すれば dedup が空いて購読の情報付きで再投入
- **承認の後続は latch（`sync_requested_at`）+ Requeue + dispatcher。** 配置と手動要求は latch を書いてから
  投入し、走行中の同期は終わる前に latch を見て `Outcome::Requeue`。handler の最終確認から終端までの窓に
  立った latch と失敗で残った latch は常駐の dispatcher（30 秒ごと）が回収する。時刻の比較はしない
- **取れない entry（非公開・削除）は位置を占め続ける。** Library に無ければ「取れない」一覧
  （`kind: private | deleted | unknown`。現行の yt-dlp は `title: null` で来るので区別できず unknown）。
  番号揃えもその位置を数えるので番号が飛ぶ。公開に戻れば次の同期で拾う
- **固定行が番号を塞ぐ。** `SOURCE_URL` 無し・再生リストに無い・`SOURCE_URL` 重複の行は動かさず、その
  番号への移動は「揃えられない」で報告する。対象同士の swap / cycle は可
- プラグインが skip / 判定不能でも購読由来なら投入する（再生リストは人が選んだもの）。追記先は購読の値、
  TITLE / ARTIST はプラグインの判定（無ければ動画タイトルと albumartist）
- 上限（`max_enqueue`、既定 50）を超えた分は次回に持ち越す。定期同期は `last_attempted_at`（成否を問わない
  開始時刻）を基準にする（最終 retry が failed になってもバックオフを迂回しない）
- API は `/api/ytmusic/subscriptions`（TASKS の `/api/playlists/...` から変更。`/api/playlists` は spindle
  自身の m3u8 プレイリスト）
- 実装レビュー（codex）で足した規則: 購読の id は再利用しない（AUTOINCREMENT。0022）、同期が queued / running の間は PATCH / DELETE を
  409（`sync_running`）で拒み（各段の前の `updated_at` 検査は控え）、子バッチの終端は batch_id の
  pending = 0 で待ち、同じ動画が再生リストに複数回あればその行は
  固定して投入は最初の位置だけ、子バッチが全件 applied でなければ `Failed` で投入しない（計画時の改名の
  衝突は報告だけ）、購読が消えていれば skip も通常どおり

**理由**: P4-14 で `SOURCE_URL` が揃い「再生リストのどこまで持っているか」が分かるようになったので、URL を
1 件ずつ貼る運用（P4-13）をなくせる。番号を再生リストの順に揃えたい（P4-15 の要求）が、Library に無い
動画を正しい番号に挿入して後続をずらす操作は手作業では 3 段階の一括操作になるため、同期の一部として
`SOURCE_URL` で計算する。設計レビュー（codex）で直した点: (1) 投入が揃えより先だと承認が揃えの前に進み
番号が重なる、(2) phase を持たない再計算方式は「tags 適用後・rename 前に落ちた」境界を今回の applied 集合に
頼ると取りこぼす、(3) Duplicate を満たしたと数えると購読の情報の無いジョブに項目を永久に取り違える、
(4) 承認の後続を「走行中なら Duplicate」で流すと lost wakeup になり、時刻比較は UNIX 秒精度で同秒の要求を
落とす、handler の最終確認から終端までの窓は generic worker を触らずに dispatcher で閉じる、(5) album を
毎回パスで解決すると album のタグ変更や layout 変更で別 album を作り得る、(6) `find_source_url` の LIMIT 1 は
重複 URL を隠す、(7) 定期投入を成功時刻だけで判定すると失敗後にバックオフを迂回する。

**却下**: phase / バッチ id を job payload に永続化して再開（再計算で同じ安全性が得られ、並行する手動
バッチも同じ経路で扱える）。URL 単位の download ジョブと購読の関連表（同じ動画が複数の購読に出るのは稀で、
Duplicate を「走行中」として次の同期に任せる規則で足りる）。generic worker の終端トランザクションに購読の
フックを足す（dispatcher で閉じる方が影響が狭い）。TRACKNUMBER を書かず Inbox の max+1 に任せて配置後に
揃える（毎回 2 バッチ増える。位置を先に書けば通常は承認の初期値がそのまま正しい）。

---

## D-79 イメージは CI が 1 回ビルドして起動確認に通ったものをそのまま GHCR へ push する。版は `/health` が答え、依存の更新は Renovate

**決定**（2026-09-21、P4-12。SPEC §14「イメージの配布」）:
- 置き場は GHCR `ghcr.io/akashisn/spindle`（public、`linux/amd64`）。`main` → `edge` / `sha-<7>`、
  `vX.Y.Z`（この形だけ。他の `v*` は CI が拒む）→ `X.Y.Z` / `X.Y` / `latest`（+ Release）。PR は push しない。
  書き込み権限（GHCR / Release）は push イベントだけの `publish` ジョブに限り、PR で走る `docker` ジョブは
  `contents: read`（同一リポジトリの PR や Renovate のブランチでも write token を持たない）。publish は
  docker ジョブが `docker save` した成果物を skopeo で複製する
- CI の docker ジョブが `load` でビルドして起動確認し、**同じイメージ**を `docker push` する
  （二度ビルドしない。確認した digest = 配布する digest）
- 版は `--build-arg SPINDLE_VERSION=$(git describe --tags --always)` → `build.rs` → `spindle --version` と
  `GET /health` の `version`。加えて `/health` に `ytdlp`（起動時に `yt-dlp --version` を 5 秒で 1 回）。
  「いまどの版が動いているか」を docker のログでなく `/health` で答える
- 本番は TrueNAS のカスタムアプリ（compose 相当）。自動配備は作らず、更新は UI で pull し直す手順を
  OPERATIONS に書く。マイグレーションは前進のみ、戻すときはバックアップから
- 依存の更新は **Renovate**（GitHub App）。yt-dlp は Dockerfile の `ARG` を `# renovate:` 注釈の
  custom manager（github-releases）で追う。マージは手動
- `edge` は開発用で DB の互換を約束しない。スカッシュは最初の `vX.Y.Z` の前に 1 回だけ

**理由**: `deploy/compose.yaml` が指す `ghcr.io/akashisn/spindle:latest` は存在せず、実機は `git archive`
→ ホストで `docker build` の手作業だった。YouTube の抽出は yt-dlp が古いと壊れるので、更新を仕組みに
する必要があった。Renovate は GitHub App なので、その PR で CI（`pull_request`）が走る（`GITHUB_TOKEN`
で作った PR は走らない）。自作の更新ワークフロー + PAT や Dependabot（`ADD https://…` の版を追えない）
より少ない部品で済む。

**却下**: 週 1 の自作ワークフローで yt-dlp の PR を作る（PAT が要る）。Dependabot（yt-dlp を追えない）。
`arm64` のマルチプラットフォーム（Rust のクロスビルドが遅く需要が無い。要るときに足す）。自動配備
（TrueNAS のカスタムアプリの更新は UI 操作。手動で十分）。CI で二度ビルドして push（`push: true` で
別に走らせると確認したものと digest が変わり得る）。

## D-80 一覧のキーボードは「カーソル行」を選択と別に持ち、Shift + 移動は範囲そのものに置き換える

**決定**（2026-09-22、P4-17。SPEC §12.2「キーボード」）:
- 表は選択（`lib/selection.ts` の immutable な集合）とは別に **カーソル行**（id）を持つ。クリックと移動キーが
  置き、見た目は枠だけ。選択の意味論（immutable、filter 形、anchor）は変えない
- 素の移動 = その行だけを選択、**Shift + 移動 = anchor からカーソルまでの範囲そのもの**（`rangeSelect`。
  戻れば縮み、Ctrl で足した飛び地は消える）、Ctrl + 移動 = カーソルだけ、Space = トグル。クリックの Shift は
  従来どおり「足す」（`clickRow`）で、キーとクリックで挙動が違う
- filter 形（Ctrl+A）のまま Shift + 移動すると ids 形の範囲に置き換わる。anchor が無ければ移動前のカーソル行を
  anchor にする
- カーソルは読み込み済みの行の中でだけ動く。End は「読み込み済みの末尾」であって total の末尾ではない

**理由**: foobar2000 / Explorer の Shift + 矢印は「anchor からの範囲」で、戻すと縮む。キーの Shift を
クリックと同じ「足す」にすると、行き過ぎた選択を Shift + ↑ で戻せない。クリックの Shift を「範囲そのもの」に
変えると既存の「Ctrl で飛び地を足してから Shift で別の塊を足す」使い方が壊れるので、クリック側は触らない。
未読込の行はサーバから id が届いていないので選択に入れようがなく、「読み込み済みの外へは出ない」が
仮想化（hooks/useTracks の先頭から順の積み上げ）と矛盾しない唯一の規則。

**却下**: カーソルを選択（`Selection`）の中に持つ（選択は表示に依存しない値で、カーソルは表示上の位置。
SSE や再取得で選択を書き換えない規則を守るため別に持つ）。End で total の末尾まで先に読み込む（6 万件を
一気に読む導線になる。要るなら「末尾へ」ボタンで別途）。Enter で再生（頼まれていない。要望が出たら足す）。

## D-81 ジョブ一覧の衛生: 変化が無ければ投入しない、やることが無いのは失敗でない、確認済みは消せる、古い行は GC が消す

**決定**（2026-09-22、P4-18。SPEC §7.8「検出」、§8「終端の行の片付け」、§9、§12.5、§13 `[gc]`）:
- **Inbox の周期の監視はジョブでなく常駐タスク**が行う。`[inbox].poll_interval_secs` ごとに Inbox の指紋
  （音声ファイルのパス・inode・size・mtime・ctime の集合のハッシュ。`import::inbox::fingerprint`）を取り、
  前回投入時と違うとき、配置待ち（`approved`）か期限切れの `placed` があるとき、起動直後だけ `inbox` ジョブを
  投入する。投入が Duplicate（前のジョブが未完了）なら「前回投入時の指紋」を更新せず次の周回で投入し直す
  （走査を終えた後に置かれた変化を取りこぼさない）。手動の `POST /api/inbox/scan` は常に投入。監視の状態
  （最後に確認した時刻）は `GET /api/inbox` の `watch` で画面に出す
- **取り込み済み**（同じ `SOURCE_URL` が Library / Inbox にある）の ytdl は `done`（note に所在）。失敗
  （`Fatal`）にしない。上部バーの赤丸（`summary.failed > 0`）は本当の失敗だけに付く
- **終端（failed / done / cancelled）のジョブ行は消せる**: `DELETE /api/jobs/:id`（queued / running は 409）、
  `DELETE /api/jobs?state=failed`（まとめて消せるのは failed だけ）。ジョブ行は作業記録でユーザデータでは
  ないので物理削除（`edit_ops` / `album_verifications` の `job_id` は SET NULL、ロック表は CASCADE）
- **GC が古い終端の行を消す**: done / cancelled は `[gc].jobs_done_days`（既定 7）、failed は
  `jobs_failed_days`（既定 30）。0 で消さない。`GET /api/gc/preview` に `jobs: { done, failed }`

**理由**: 実機で inbox ジョブが毎分 1 行（1 日 1,440 行）積もり、ジョブ画面の「完了」が Inbox の確認で埋まって
他の完了が見えなかった。汚染の原因は「毎分の確認」でなく「確認のたびにジョブ行を作る」ことなので、確認を
ジョブの外に出して変化があったときだけ投入する。赤丸はリハーサルで既に Library にある URL を投入した 2 件の
「取り込み済み」で点きっぱなしになっていた。やることが無いのは異常ではなく、失敗に混ぜると本当の失敗が
埋もれる。確認済みの失敗を消す口が無いと赤丸を消せない。`jobs` は 5 万行を超えていて消す仕組みが無かった。

**却下**: inotify（D-68 追記。即時トリガとして後から足せる形にしてある）。失敗に「確認済み」フラグを足す
（行を残す理由が無い。消せば十分）。done をまとめて消す API（数が多く一覧の上限で切れるので画面からは
1 件ずつ、古いものは GC）。失敗ジョブの削除イベントを SSE で流す（行が無くなるので画面は取り直す。他のタブは
次のジョブイベントで追随する）。監視の状態を DB に持つ（プロセス内の値で足りる。再起動直後は必ず 1 回投入する）。


## D-82 候補のジャケットは Cover Art Archive から引き、サーバが中継する

**決定**（2026-09-23。D-72 の却下を覆す。P4-20 で実装）:

- CD 画面の候補に**ジャケットを出す**。取得元は Cover Art Archive の `release/<mbid>/front-500`
- **サーバが中継する**（`GET /api/cd/cover/{release_id}`）。ブラウザから直に引かない
- **MB に画像がある盤だけ**。無ければ 404 で、画面は空の枠のまま。手動のアップロードはしない
  （既存の `POST /api/artwork/upload` が別にある）
- MB 本体の 1 req/s のスロットは使わない（相手が archive.org で別。画像の取得で照会が待たされない）
- **中継の境界を決めておく**: `cover_art_url` は起動時に `Url` として検証して `join` で組み立てる、
  リダイレクトは 5 回まででかつ HTTPS → HTTP のダウングレードは追わない、本文は 8 MiB まで
  逐次読みで打ち切る、`Content-Type` は `image/*` だけ、返すときは `nosniff`。
  **「画像が無い」（404）と「上流がおかしい」（502）は混ぜない**（混ぜると診断できない）
- 取り込み時に FLAC へ埋める配線は P2-8。`DiscMetadata` に画像を持たせる形は P2-5 で決める

**理由**: D-72 は「要望が無い」として却下したが、候補が複数出たときジャケットが一番早い見分けに
なるという要望が出た。サーバで中継するのは、(a) `[musicbrainz].address_family` / UA / `error_chain` の
診断をそのまま使えること（この回線は MetaBrainz の IPv4 が塞がれている。D-64 追記 2）、
(b) P2-8 で FLAC に埋めるときに結局サーバが同じ画像を取ること、の 2 つによる。

**信頼の境界（未解決のまま残す点）**: この経路はサーバがユーザの代わりに外へ出ていく
（server-side request）。利用者が入れられるのは MBID だけで、宛先は `cover_art_url` と**上流が返す
リダイレクト先**で決まる。**リダイレクト先のホストは許可リストで絞っていない**。上限 5 回・HTTPS の
ダウングレード拒否・`image/*` だけ・8 MiB までで抑え、「既定の Cover Art Archive と、管理者が
`cover_art_url` に入れた宛先は信頼する」という前提を置く。spindle は宅内の単一利用者向けで、
`cover_art_url` を書けるのは管理者自身だからこの前提で足りる。**厳密に閉じるなら**既定では
`coverartarchive.org` と `archive.org` 系だけを許可し、自前ミラーは設定した origin に限る形にする
（今回は採らない）。

**追記**（2026-09-23。実機確認で判明）: **接続に使う IP の族は `[musicbrainz].address_family` を
引き継がない**（`Auto` を渡す）。当初は UA と一緒に共用する設計だったが、実機（`address_family =
"ipv6"`）でジャケットが取れなかった。

この経路は **coverartarchive.org（MetaBrainz）→ archive.org（Internet Archive）** の二段構えで、
`archive.org` には AAAA が無い（`getent ahostsv6 archive.org` は IPv4 射影しか返さない）。ipv6 に
固定すると CAA の 307 は受け取れても飛び先へ届かず、リダイレクトを追えないまま終わる。実機での
確認: `curl -6 -L` は 307 で止まり、`curl -4 -L` は 200。族はホストごとに解決させるのが正しい。

`[musicbrainz].address_family` が要るのは musicbrainz.org のエッジがこの回線の IPv4 を落とすため
（D-64 追記 2）で、それは相手ごとの事情。CAA 用の設定項目は足さない（archive.org に AAAA が無いのは
この回線の事情ではなく Internet Archive 側の性質なので、どの環境でも `Auto` が正しい）。

**却下**: ブラウザから直接 coverartarchive.org を引く（上の 2 つの理由で不利）。原寸の取得
（数 MB あり、画面には要らない）。CAA 専用の `address_family` 設定（上の理由で `Auto` 以外に
意味のある値が無い）。

---

## D-83 読み取りオフセットは照合から見つけて学習する。同梱のオフセット表は持たない

**決定**（2026-09-23。P2-5。ユーザ判断）:

- `[rip].drive_offset = "auto"` は、ドライブの型番（INQUIRY の vendor + product。ファームウェアの版は
  含めない）で**前に覚えた値**（`drive_offsets`、マイグレーション 0023）があればそれで、無ければ 0 で吸う。
  整数を書けばその値で吸い、学習しない（`OffsetSource::Manual`）
- 吸った PCM の CRC 表で CTDB / AccurateRip と照合し（`cd::verify`）、**見つかったオフセット `r` が 0 で
  なければ PCM を `r` だけずらしてから**（`cd::rip::shift_pcm`。端の `|r|` サンプルは無音）CRC を取り直し、
  配置する。ずらした後の吸い出しのオフセットは「吸ったときの値 + `r`」で、rip.log と `RipReport` に
  `Detected` として残す。`CrcTable` の `offset` は「DB の窓が自分のデータの何サンプル後ろから始まるか」
  なので、`r` が正なら先頭を捨てて末尾を埋める（向きは `tests/cd_rip.rs` で固定）
- **照合が通ったとき（CTDB か AccurateRip のどちらかで全トラック一致）だけ覚える。** 同じドライブは最後に
  通った盤の値で置き換える。どちらにも候補が無い盤は吸ったときの値のまま（`Learned` か `Unknown`）
- 端を無音で埋めてよいのは、探索範囲（±2939）が AccurateRip の末尾の除外（2940 サンプル）と CTDB の末尾の
  除外（10 セクタ以上）に収まり、CRC が変わらないから。音声としては最初か最後のトラックの端の `|r|`
  サンプル（BDR-209M の +667 なら 15 ms）が無音になる。ドライブがリードイン / リードアウトを読めない
  ときの EAC と同じ結果

**理由**: 同梱表の出所として有名なのは AccurateRip のドライブオフセット一覧だが、D-13 のとおり AccurateRip
の DB 利用は許諾がグレーで、表を同梱して配るのはさらに踏み込む。照合側のオフセット探索は P2-9 で既にあり、
CTDB / AccurateRip に載っている盤を 1 枚吸えば正しい値が分かる（EAC の「key disc」と同じ考え方）。手元の
ドライブは 1 台で、1 回覚えれば以後は同じ値を使う。

**却下**: 同梱表（上記）。INQUIRY + 手元のドライブだけの小さな表（表の保守が要り、他のドライブでは結局 0 で
始まる）。見つけたオフセットで再リップする（10 分かかる。ずらすのと結果は同じ）。`cd-paranoia -O` を
見つけた値で掛け直す（同上）。

**追記**（2026-09-23。rip ジョブの実装で決めたこと）:

- **`cd-paranoia -O` は使わない。** 手動・学習済みの値も含めて常に 0 で読み、`shift_pcm` で当てる。
  リードイン / リードアウトの外を読みに行かないので、ドライブの overread の可否に結果が左右されない。
  読む範囲は TOC の長さから `first-last[mm:ss.ff]` で明示する（Enhanced CD の最後の音声トラックの終端を
  cd-paranoia の解釈に任せない）
- **吸い直すのは比べる相手があるときだけ。** CTDB か AccurateRip のどちらかに候補があって一致しないとき。
  照会できない・どちらの DB にも無い盤は、吸い直しても結果が変わらないので 1 回で確定する
- `TrackRead.rereads` は cd-paranoia の進捗で報告された scratch / repair / skip / read error の回数。
  cd-paranoia は C2 を使わないので `c2_errors` は 0（rip.log の C2 欄は測っていない）
- 進捗は SSE の `job` イベントに `detail`（`RipProgress`）を足して流す（DB には書かない）
- 実機（BDR-209M）で 9 分の 2 トラックの盤が 140〜160 秒。その盤は CTDB（信頼度 30、同じ TOC）と
  どのオフセットでも一致せず、2 回の吸い出しで中身が違った（傷か読みの不安定）。学習の実機確認は
  CTDB / AccurateRip に載っているきれいな盤で行う
- **「照合が通った」はオフセット 0 で全トラック一致のときだけ**（codex 指摘）。`match_*` の Verified は
  ずれたままでも成立するので、ずれを PCM に当ててから照合し直したものだけを採用する。CTDB のパリティで
  直したとき、パリティが見つけたずれ（傷のため CRC では見つからなかったもの）も同じく直した PCM に当て、
  オフセット 0 で照合が通れば「吸ったときの値 + そのずれ」を学習する
- `[rip].drive_offset` の整数は起動時に ±2939（探索範囲）へ制限する。学習済みの値が範囲外なら使わない
- 公開の前に失敗・取り消しされたら、組み立て用の隠しディレクトリを消す（走査に見えないので、残すと同じ
  盤を吸い直すまで誰も片付けない）。修復の 2 回の走査は blocking に置き、1 MB ごとに取り消しを見る
- **当てるずれは全曲一致した手法を優先する**（codex 指摘）。全トラック一致（両方なら CTDB）→ 無ければ一部
  一致（CTDB を優先）の順。部分一致を先にすると、傷のある盤で別のオフセットに引きずられる。学習に残す手法と
  信頼度は、当てた後にオフセット 0 で通った手法のもの

**追記 2**（2026-09-23。ユーザ要望。foobar2000 と同じく、ディスクが無くてもドライブを判定する）:

- **AccurateRip のドライブ別オフセット表（`DriveOffsets.bin`）を実行時に取って使う**（EAC / foobar2000 /
  dBpoweramp と同じ）。同梱はしない（D-83 の却下理由は「表を配る」ことで、実行時の取得は照会と同じ扱い。
  D-13）。置き場は `[verify].accuraterip_url` の下、保存は `data/cd/DriveOffsets.bin`（30 日で取り直す。
  取れなければ古い保存、それも無ければ表なし）。起動時に 1 回読み込み、`GET /api/cd/status` は読み込み済みの
  表だけを見る（ネットワークを待たない）
- 形式は 69 バイトのレコード（`offset i16 LE`、名前 33 バイト `"<vendor>  - <product>"`、提出数 `u32 LE`、
  予約）。照合は空白の連なり・`" - "` の区切り・大小文字を畳んだ鍵で、INQUIRY の型番（vendor と product を
  空白 1 つで結合）と突き合わせる。同じ型番が複数あれば提出数の多い方。実物で `PIONEER  - BD-RW   BDR-209M`
  は +667（提出 255）
- **吸うときのオフセットは 設定の整数 → 学習済み → 表 → 0**（`cd::rip::choose_offset`。探索範囲の外は
  使わない）。表の値も照合で確かめ、ずれていれば当てて学習する（`OffsetSource::Table`）
- 型番はポーラがデバイスを開けている間に 1 回読んで状態に持つ（ディスク不要）。`GET /api/cd/status` の
  `drive: { model, offset, offset_source }` で CD 画面がディスクを入れる前から出す
- 表の妥当性（codex 指摘）: 空は拒否。実物の全レコードで成り立つ形（名前が空でなく 33 バイト内で NUL
  終端、予約が 0）から 1 件でも外れれば表ごと拒む。壊れた保存は取り直し、壊れた応答で正しい保存を
  上書きしない。メモリの表も取得時刻を持ち、`MAX_AGE` を過ぎたら次に使うときに取り直す（常駐し続けても）。
  取り直しに失敗したら前の表で動き、1 時間は叩き直さない。範囲外の学習値は無いものとして表を引く
- 常駐中の取り直しは `spawn_refresher`（1 時間ごとに期限を見る。status は `peek` しか呼ばないので、これが
  無いと吸い出しをしない限り更新されない）。表が一度も無いままの失敗も時刻を覚え、`RETRY_AFTER` の間は
  取りに行かない（吸い出しのたびに 30 秒待たない）。古い保存は取り直しの前に公開する（stale-while-revalidate）

**追記 3**（2026-09-23。実機の確認で見つけた欠け）:

- `album_verifications.drive_offset` は `source = 'rip'` の行だけに書く。値は **PCM に当てた読み取り
  オフセット**（`RipReport.read_offset`。照合で見つけたずれを当てた後の値）で、`detected_offset` はそこからの
  残りのずれ。`source = 'retro'`（遡及照合）はどのドライブで吸われたか分からないので NULL。列は 0001 から
  あったが書く処理が無く、吸い出しでも NULL だった。既存の行は埋め直さない（値は rip.log に残っている）

---

## D-84 候補の無いまま取り込んだ CD は、承認画面のボタンで MusicBrainz を引き直す

**決定**（2026-09-23。P4-21。ユーザ判断）:

- **起点は承認画面のボタン。** CD の件（サイドカーに `rip`）にだけ「MusicBrainz で引き直す」を出す。照会は
  CD 画面と同じ `POST /api/cd/lookup`（DiscID → ISRC / バーコード → TOC 近似の段階化、10 分のキャッシュ、
  1 req/s の間隔待ちはサーバの `MusicBrainzClient`）。**ジョブにせず、結果も保存しない**（件を替えると消える）。
  自動で引くと「どれも違う」と判断した盤まで毎回引き直すことになり、レート制限の中で無駄が多い
- **吸い出しでドライブの ISRC / MCN をサイドカーに残す**（`RipEntry.isrcs` / `mcn`）。DiscID も TOC 近似も
  当たらず ISRC でだけ見つかる盤がある（嵐「Five」。D-64 追記）。旧サイドカーは空で読める
- **選んだときに写すのは、ID と空欄の名前。** リリース / リリースグループの MBID は常に写す（下書きの
  `release_id` / `release_group_id` → 配置でタグと `mb_release_id`。D-67 追記 3 のリリースキー `mb:`）。
  アルバム名・アルバムアーティスト・日付・曲名・曲のアーティストは、下書きで空のところ（曲名は吸い出しが
  付けた `Track NN` も空とみなす）だけ埋め、手で入れた値は上書きしない。曲はトラック番号で候補の曲に
  対応させ、曲のアーティストがアルバムアーティストと同じなら空のまま（配置でアルバムアーティストになる）
- **ディスク番号は候補の medium の位置にする**（名前ではなく盤の識別。2 枚組の 2 枚目を 2 枚目として置き、
  同じ MBID の 1 枚目に合流させる。1 のままだと番号が重なって配置で弾かれる）
- YouTube の件は対象外（曲の身元は `SOURCE_URL`。D-70）
- **MBID の大文字小文字は同一性に効かせない**（codex 指摘）: 外部のツールが大文字の UUID を書くので、検証は
  大小を問わず通し、値（タグと `mb_release_id`）はそのまま保つ。リリースキー `mb:` は ASCII 小文字に揃え
  （`placement::mb_key`。Inbox の配置・rename・自分の成果物の判定）、スキャナの album の照合も大小を無視する

**理由**: D-72 の「MusicBrainz は盤の識別に使い、値は公式表記を手で入れる」を保ったまま、名前の入っていない
盤では候補の表記を下敷きにできるようにした（空欄だけ埋めるので、公式表記で直した値は消えない）。

**却下**: 件が現れたら自動で照会してジョブの結果を保存する（上記）。候補の値で全部上書きする（手で直した値が
消える。CD 画面の「全部写す」は名前を入れる前の画面なので事情が違う）。

---

## D-85 CD 画面は番号付きの段で流れを見せ、ライブラリにあるかを DiscID で示す。Inbox の表は全項目を出す

**決定**（2026-09-24。ユーザ要望）:

- **CD 画面は ① 挿入中の CD → ② 認識したトラック → ③ 候補を選ぶ → ④ 取り込む の番号付きの枠。**
  アルバム名を裸の見出しで出すと何の値か分からないので、① の値はすべてラベル付きにする。長い説明文は本文から
  外し、各枠の見出しの ⓘ（ホバー / フォーカスで出るツールチップ）に畳む。「表示のみ」も注記のバッジにする
- **「この盤はライブラリにある」を最上部の帯で示す。** 新しい `POST /api/cd/library { toc, release_id? }`。
  DiscID はサーバが TOC から出し、active なトラックの `MUSICBRAINZ_DISCID` タグで引く（DiscID は 1 枚ごとの値で
  album の列には無い。D-67 追記 3）。当たらなければ、選択中の候補のリリースを `albums.mb_release_id` で引き、
  「同じリリースの album がある（この盤は未取り込み）」と区別して出す（2 枚組の 1 枚目だけある、を見分ける）。
  DB だけを読み、MusicBrainz の照会の前でも失敗しても判定できる。マイグレーションは要らない
  （`idx_track_tags_key` と `idx_albums_release` が効く）
- **Inbox の一覧の各件に代表画像のサムネイル。** 埋め込み画像だけ（既存の `/api/inbox/:id/artwork/:hash`）。
  フォルダの cover.jpg は拾わない（ユーザ判断。CD の件は画像を埋め込まないので空枠になる）
- **Inbox の承認画面のトラック表は全項目。** 直せる列・上の欄の写し・ファイル由来に加え、ファイルの全タグを
  列にする（ユーザ判断）。横に長くなるので枠の中で横スクロールし、「列」メニューで隠せる（見た目の好みなので
  localStorage）。disc / # / 画像 / タイトルは左端に固定する（右のタグを読むときに行が分かるように）。タグの列は
  読み取り専用で、配置では変えない

**理由**: CD 画面で「いま何が入っていて、どれとして取り込むか」が一目で追えず、説明文が多くて流れが読めなかった
（ユーザ指摘）。既に持っている盤を二重に吸い出す手間も、上部の帯で避けられる。

**却下**: 所持の判定を MusicBrainz の候補（リリース）だけで行う（候補を選ぶまで分からず、同じリリースの別の
盤と区別できない）。フォルダ画像の配信 API の追加（今回は不要）。

---

## D-86 Inbox の承認画面でファイルのタグと埋め込み画像も直せるようにする

**決定**（2026-09-24。ユーザ要望。モックで合意）:

- **下書きのトラックに `tags` と `picture` を足す。** `tags` はキー → 値の配列（`null` はそのタグを消す）で、
  書くのは変更したキーだけ。`picture` は差し替える画像（`<mime>:<sha256hex>`）で、無ければファイルの画像のまま。
  どちらも省略可なので、旧い下書きはそのまま読める
- **上の欄が扱うキーは `tags` で受け付けない**（TITLE / ARTIST / ALBUM / ALBUMARTIST / DATE / TRACKNUMBER /
  DISCNUMBER / DISCTOTAL / PICTURE）。直し方を二通りにしない
- **同一性の判定に使うキーには鍵をかける**（`SOURCE_URL` / `MUSICBRAINZ_*`）。`SOURCE_URL` は YouTube の
  二重取り込みの判定（D-70）、`MUSICBRAINZ_DISCID` などは CD の盤とリリースの識別（D-67 追記 3）に使うので、
  手で書き換えると判定が静かに壊れる。リリースの付け替えは MusicBrainz の引き直し（D-84）で行う
- **画像は既存のアップロード（`POST /api/artwork/upload`）で先に置き、下書きは sha256 だけを持つ。** CD の件は
  `POST /api/artwork/from-caa` で Cover Art Archive の front 画像を同じく置ける。配置は `picture` のある曲だけ
  埋め込み画像をその 1 枚にする（ライブラリの差し替え D-60 と同じく全部捨てて front cover）
- **曲ごとに画像が違う件（YouTube）は、既定で曲ごとの画像を保つ。** 画面は「画像の無い曲に入れる」と
  「全曲を 1 枚にそろえる」を明示的な操作にし、1 曲ずつの差し替えはトラック表の画像列で行う
- **下書きが参照する画像は GC しない**（区分 E の「参照」に `inbox_items.draft` の `tracks[].picture` を足す。
  下書きには任意のタグの値も入るので全文の部分一致にはせず、JSON として `picture` の値だけを照合する。
  codex 指摘）。承認後に配置が失敗して再承認を待つ間に消えないように。それでも消えていたら、承認は 400、
  配置は failed（選び直す）
- **宛先に自分の成果物（同じ音声）があっても置き換えない。代わりに今回の補正が入っているかを確かめ、
  入っていなければ失敗にする。** 途中で落ちた件の再承認では、任意のタグや画像の補正はパスを変えないので
  同じ宛先に当たる。同じ音声だからと再利用すると今回の補正が黙って捨てられ、置き換えると外部（foobar2000
  等）の変更や missing の行の実体を履歴なしに上書きしうる（codex 指摘。置き換えを孤児に限って照合する案は
  競合を原子的に閉じられなかった）。そこで再利用の前に、既存ファイルのタグと代表画像が今回の下書きの内容
  （`tag_changes` が空、`picture` の sha256 が一致）であることを確かめ、違えば Conflict（件は failed、
  Inbox と下書きは残る。Library のそのファイルを確かめて消すか、配置後にライブラリの編集で直す）
- **ALAC（MP4）を理由に止めない。** P4-11 で MP4 は写像できないキーをフリーフォームで書けるようになっており、
  配置のテストで m4a に任意のキーを書けることを確かめた（モックで出した「ALAC には書けない」は古い制限だった）
- **配置先の見込みを出す**（`POST /api/inbox/:id/preview`）。音声を読まずに配置の計画（`plan_item`）を引くので、
  途中で失敗した件の再承認（自分の成果物の再利用）では実際と食い違うことがある。画面では見込みとして出す

**理由**: 承認画面をライブラリのプロパティと同じ操作で全項目を直せる場所にし、カバーの無い件もその場で
直せるようにする（ユーザ要望）。値の保存先はこれまでどおりファイルのタグ（不変条件 1）。Inbox の原本は
書き換えず、配置の途中のコピーに書く。

**却下**: フォルダに cover.jpg を置く（spindle は同梱の cover ファイルを書かない。D-60）。同一性のキーも
自由に直させる（上記）。承認前に下書きをサーバへ保存する（今の「承認で保存」を保つ）。

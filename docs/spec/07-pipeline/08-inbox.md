### 7.8 Inbox 取り込み

取り込み対象ディレクトリを増やす方式は採らない。外部ディレクトリを直接
スキャン対象にすると、そのツリーのパス規約とタグ品質がそのまま Library に
混入するため。Inbox をステージングとして挟む。

```
Inbox/ に配置（ポーリング検出）
  → ステージング: タグ解析、コーデック判定、アルバム単位でグルーピング
  → 承認キュー:   UI で category / albumartist / album を確認・補正
  →               ※メタデータ不足のまま Library に入れない
  → 配置:         テンプレート展開 → Library へ move
  → 後続ジョブ:   normalize(WAV) → rg → transcode → thumbnail
```

承認キューを挟むことが要点で、これがないと `_Unsorted` が際限なく育つ。

実装（`src/import/inbox.rs`、`inbox` ジョブ、D-68）:

- **検出**は `inbox` ジョブ（並列 1・固定キー）。周期の**監視**が `[inbox].poll_interval_secs`（既定 60、0 で
  自動なし）ごとに Inbox の指紋（音声ファイルの集合: パス・inode・size・mtime・ctime。非音声と空ディレクトリは
  効かない）を取り、**前回投入時と違うとき**と、配置待ち（`approved`）・期限切れの `placed` があるときだけ
  投入する（変化の無い毎分のジョブ行で一覧を埋めない。投入が Duplicate なら次の周回で投入し直す。起動直後は
  必ず 1 回。P4-18、D-81。inotify は使わない: D-68 追記）。`POST /api/inbox/scan` は常に投入。
  `GET /api/inbox` の `watch` に最後に確認した時刻と間隔（Inbox 画面が「最後に確認: HH:MM:SS」を出す）。
  ジョブは Inbox を歩き、音声ファイルのあるディレクトリを
  1 つの取り込み（アルバム候補。root 直下の音声は `""` の 1 つ）として `inbox_items` / `inbox_files` に写す。
  stat（inode / size / mtime / ctime）が変わったファイルだけタグを読み直す。**正は Inbox のファイル**で、
  行はキャッシュ: ディレクトリが消えれば行も消す（`placed` は 24 時間残して結果を見せる）。`approved` の取り込みで
  ファイルが変わっていたら `pending` に戻す（再承認）
- **CD の取り込みの表の画像**（D-91）: 走査と承認済みの配置の後、承認前（`pending` / `failed`）で画像がまだ無い取り込みのうち、
  サイドカーの `rip.metadata.release_id`（吸い出しで選んだリリース）があるものは、Cover Art Archive の front
  画像を**一度だけ**取り（`POST /api/artwork/from-caa` と同じ経路・境界。`CoverArtClient` は `address_family`
  を `Auto` 固定）、`artwork` の置き場に置いて `inbox_items.caa_picture`（`<mime>:<sha256hex>`）に記録する。
  そのリリースに front が無く（404）、サイドカーに `rip.metadata.release_group_id` があれば、同じ回の中で
  リリースグループの front（`release-group/{id}/front-500`。CAA がグループの代表に選んだ版の表の画像）を 1 度だけ
  試す（D-93。通常盤に画像が無く初回限定盤・BD 付きの版にある盤。回数は 1 回と数え、グループの 404 はリリースの
  404 と同じ打ち止め、グループの上流の失敗はリリースの失敗と同じ再試行）。
  提案（`proposal`）は全曲の `picture` の初期値をその画像にする（保存した下書きがあればそちらが勝つ）。
  画像の無い盤（404）とリリースの無い取り込み（候補を選ばずに吸い出した・CD でない）は 1 回で打ち止め、上流の
  失敗は次の走査でもう 1 回だけ試す（`caa_tries`、上限 2。回数は外へ出る前に CAS で進めて永続化する）。1 回の
  実行で 4 件・90 秒まで、残りは周期の監視が次の周回で投入する（`needs_attention`）。取り消しは通信の待ちの間も
  効く。どれも取り込みは失敗させず画像なしのまま。
  承認画面を開くたび・`GET /api/inbox` のたびには外へ出ない。`caa_picture` の画像は GC しない（区分 E の参照）
- **却下した取り込みの削除**（D-90）: `rejected` の取り込みに「削除」で `discard_requested_at`（破棄待ち）を入れる。ファイルは
  すぐには消さず、GC が `[gc].retention_days` 経過後に取り込みのディレクトリの直下の**走査が写した音声**（stat が一致
  するもの）・既知の同梱ファイル・サイドカーを消し、ディレクトリが空なら消し、行を消す（サブディレクトリ = 別の取り込みと
  知らないファイルは残す）。それまでは「削除を取り消す」で `rejected` に戻る。`rejected` から出る遷移（下書きに
  戻す）でも破棄待ちは解ける。走査が破棄待ちの取り込みでファイルの変化（足された・差し替えられた）を見たら、人が
  見ていないものを消さないよう破棄待ちを解いて `rejected` のまま理由を `error` に残す（GC も確かめてから消す:
  音声は `inbox_files` と、同梱ファイル・サイドカーは ctime が破棄の要求より後でないかで照合し、合わなければ
  何も消さずに解く。消す途中で stat が変われば止めて解き、消せないものがあれば止めて行と破棄待ちを残し次の GC で
  続ける。D-90）
- **同名の警告**（P4-19、D-70 追記）: 追記先の album に**同じタイトル**の active な行があれば、そのトラックに
  `same_title: [{ track_id, rel_path, duration_ms }]` を付ける（承認は止めない。画面は「⚠ Library に同名:
  <ファイル名>（長さ）」と、取り込みの見出しに「同名 N」）。鍵は NFKD + casefold + 空白の畳み込み（全角・半角の
  揺れは同じ、`(Cover)` / `【… Live ver.】` の注記は**落とさない** = 別曲扱い）。狙いは `SOURCE_URL` の
  補填漏れや別 URL の再アップロードによる二重取り込みで、判断は人が行う（同じ曲名の別テイクは正当なので、
  長さを添えて見分けられるようにする）
- **承認キュー**は `GET /api/inbox`（`items` と監視の状態 `watch: { checked_at, poll_interval_secs }`）。取り込みごとにタグから作った下書き（`proposal`: albumartist / album / date の
  最頻値、category は GENRE → `genre_category_map`、トラックは TRACKNUMBER / DISCNUMBER / TITLE / ARTIST）と
  不足の警告を返し、UI の Inbox タブで category / albumartist / album / date と各トラックの
  disc_no / track_no / title / artist を補正して `POST /api/inbox/:id/approve { draft }`。検証（album /
  albumartist / 各 title が空でない、`(disc_no, track_no)` が 1 以上で重複なし、rel_path が取り込みのファイルと
  一致）に通らなければ 400 で、メタデータ不足のまま Library に入れない。`reject` / `reopen` で状態を戻す
- **トラックのタグの変更と画像の差し替え**（D-86）: 下書きのトラックは `tags`（キー → 値の配列、`null` で
  そのタグを消す。書くのはここにあるキーだけ）と `picture`（`<mime>:<sha256hex>`。`POST /api/artwork/upload`
  か `/from-caa` で置いた画像）を持てる（どちらも省略可。旧い下書きはそのまま読む）。キーは大文字・空でなく
  `=` と制御文字を含まない（tagops と同じ文字の規則）。上の欄が扱うキー（TITLE / ARTIST / ALBUM /
  ALBUMARTIST / DATE / TRACKNUMBER / DISCNUMBER / DISCTOTAL / PICTURE）と、曲・盤の同一性に使うキー
  （`SOURCE_URL` / `MUSICBRAINZ_*`）は受け付けない。承認は差し替える画像の `artwork` 行が無ければ 400。
  配置は `tags` の変更を補正のタグに足して書き（現在値と同じものは書かない）、`picture` のある曲だけ埋め込み
  画像を全部捨ててその 1 枚（front cover）にする。宛先に自分の成果物（同じ音声。途中で落ちた前回の配置）が
  あれば置き換えずに再利用するが、今回の補正（タグ・画像）がそのファイルに入っていなければ Conflict で failed
  （補正を黙って捨てない。外部の変更も上書きしない）。画像が store から消えていれば配置せず
  failed。入力欄は Enter / Esc / blur のどれで決着しても 1 回だけ確定か取り消しをする（`lib/editSession.ts`）。
  下書きが参照する画像は GC しない（区分 E の「参照」に Inbox の下書きを足す。D-56 / D-86）。`POST /api/inbox/:id/preview { draft }` は音声を読まずに
  配置の計画を引いて置き場所の見込みを返す（承認画面の ④）
- **配置**は `approved` の取り込みを `inbox` ジョブが順に処理する。`library` の排他（scan / gc / CD の配置と
  同じ）を取れなければ Requeue。draft から各ファイルの `TrackFields` を作り `pathgen::plan`（category 無しは
  `_Unsorted`、`disc_no` の最大 ≥ 2 なら `multi_disc`、リリースキーは MUSICBRAINZ_ALBUMID の最頻値があれば
  `mb:`、無ければ取り込みごとの新規）→ Inbox からハッシュを取りながら Library の tmp へコピー → 補正で変わる
  タグだけ `write_tag_changes` で書く（ファイルが正のまま再スキャンしても DB と一致する）→ fsync →
  `RENAME_NOREPLACE` → 読み戻して 1 トランザクション登録（`source_type = 'download'`。宛先 album の
  リリースキー再検証と同パス行の MD5 検証は §7.2 の配置と同じ）→ Inbox 側を unlink → 既知の同梱ファイル
  （cover 画像 / cue / toc / log）も移し、空になった Inbox のディレクトリを消す。コピーに使う FD を
  fstat して承認時の行（inode / size / mtime / ctime）と照合し、コピーの後にも同じ FD を照合し、置いた
  ファイルの音声の指紋が承認時に読んだものと一致することを確かめる。どれかが外れたら `Changed`（取り込みは
  `pending` に戻して再承認）。衝突・不足は取り込みを `failed` にして理由を残し、この呼び出しで置いたファイルは
  片付ける。
- **状態遷移は CAS。** `approve` / `reject` / `reopen` と worker の `approved → placing` は
  `UPDATE … WHERE id = ? AND state IN (…)` で行い（`db::inbox::transition`）、読んでから書くまでの間に
  他（API / worker / 走査）が動かした取り込みを上書きしない（API は 409 `state`、worker はその取り込みを飛ばす）。
  前のプロセスが配置の途中で落ちて `placing` のまま残った取り込みは、次の `inbox` ジョブの先頭で `approved` に
  戻して配置し直す（並列 1 なので、そこで見える `placing` は必ず前の実行の残り。配置は音声の指紋で自分の
  成果物を採用するので冪等）。取り込みの `placed` と、WAV / ALAC / AIFF の normalize バッチ（`[normalize].wav_to_flac`）も登録
  トランザクションの中で確定する（commit の直後に落ちても `placing` が残らず、投入も欠けない）。`placed` の取り込みのディレクトリに走査で音声が見えたら（消せなかった原本、
  配置の後に置かれたファイル）`pending` に戻して取り込みとして出し直す。
  Inbox は Library と別データセットなので move は実コピー（§5）
- **後続**は `rg`（album の属性が on なら album 単位、off なら登録した track ごと。D-74）と `transcode`。WAV / ALAC / AIFF は `[normalize].wav_to_flac` なら `normalize` の
  編集バッチを作って投入する（D-46 の予告）。**アートワークは配置の直後にその album だけ解決する**
  （`scanner::resolve_album_artwork_now`。同梱カバー画像 → 構成トラックの埋め込み画像というスキャンの
  Phase 5 と同じ規則・同じ DB 反映で、thumbnail もここで投入。**`library` の排他を持ったまま**行う（解放して
  からだと並行する scan の予約 → 解決をこちらの古い読み取りで上書きし得る）。始める前に
  `artwork_resolved_at = NULL` で予約するので、決められなくても（同梱カバーの探索や読み取りの I/O 失敗を
  含む）次のスキャンが拾う。P3-4）
- **既存の album への追記**（D-70）: リリースキーは MUSICBRAINZ_ALBUMID の最頻値があれば `mb:`、自分の成果物の
  album があればそれ、**無ければ宛先ディレクトリに active な album があり、その album にも MB キーが
  無ければその album を採用**（`album:<id>`）、それも無ければ取り込みごとの新規。MB キー同士が違えば従来どおり
  降格か衝突。**CD とそれ以外は混ぜない**（D-67 追記 3）: トラックのタグに `MUSICBRAINZ_DISCID` があるものを
  CD とし、CD の取り込みは CD の album にだけ、しかも album にまだ無い `disc_no` のときだけ採用する（MBID の無い
  複数枚組の 2 枚目が 1 枚目に合流する。同じ番号は同名の別の盤）。CD でない取り込みは CD でない album にだけ採用する。
  同じ判定を再実行の「自分の成果物」と登録のトランザクションでも通し、外れたら `failed`。購読（§7.7）の
  束ね先が CD の album になっていたら同期は失敗する。採用する album は `GET /api/inbox` の `destination`（`{ album_id, album, track_count,
  max_track_no, album_gain }` | null。下書きの category / albumartist / album と取り込みのファイルから引く。取り込みに
  MUSICBRAINZ_ALBUMID があれば別リリースなので null）で見せる
- **album gain**（D-74）: 下書きの `album_gain`（既定 false。承認画面のチェックボックス。追記先があればその
  現在値が初期値）を配置時に album の属性へ書く（追記先の属性も上書きし、off にすれば album の値を消す）
- **ARTIST の多値**（P4-4、D-70）: 提案のトラックは `artist`（ファイルの ARTIST の全値を `"; "` で結合した
  表示文字列。1 値なら同じ）と **`keep_artists`**（ファイルの ARTIST が多値なら true）を持つ。配置は
  `keep_artists` が true なら**現在の ARTIST の個数に関係なく** ARTIST に触れない（`artist` は表示用で、値は
  見ない。承認後にファイルが 1 値に変わっていても、そのまま）。false なら `artist`（空ならアルバムアーティスト）
  の 1 値で置き換える（`;` で分割はしない）。提案と画面が true にするのは多値のときだけ（保存済みの
  下書きを再表示するとき、ファイルが多値でなくなっていれば画面は false に戻す）。`keep_artists` を持たない
  旧下書きは旧規則（`artist` が先頭値のままなら保つ）で解釈する。保つときのパス生成の `{artist}` は
  Library の `artist_display` と同じ `", "` 結合（ARTIST が無ければ `artist` → アルバムアーティストの順）
- **埋め込み画像**（P4-4、D-70）: 走査は各ファイルの `PICTURE`（`"<mime>:<sha256>"`）を、Library の
  `pick_embedded` と同じ規則（front cover 優先、無ければ先頭）で選んだ画像が**先頭**になる順で記録する。
  各ファイルの代表画像はその先頭。取り込みの見出しに出す画像は各ファイルの代表の最頻（同数なら先に現れた
  もの）で、**取り込みの中の多数派を目安に見せる要約**。配置後の album の代表（同梱カバー → 最初のトラックの
  埋め込み、追記先なら既存の代表）と同じとは限らない。忠実なのはトラックごとのサムネイル
  `GET /api/inbox/:id/artwork/:hash` は、取り込みがあれば `PICTURE` にその sha256 を持つファイルを走査順に
  開き（openat2。symlink / 境界外 / 消失は次の候補へ）、埋め込み画像の内容の sha256 が一致するものを
  探す。見つからなければ 404（取り込みに無い hash、ファイルが消えた、画像が書き換わった）。見つかれば
  `If-None-Match` が ETag `"<hash>-orig"` に一致すれば 304、でなければ原寸を返す（MIME はタグの値ではなく
  内容の sniff。sniff できなければ 404。`Cache-Control: public, max-age=31536000, immutable`）。
  **304 の判定は実体の照合の後**（DB に `PICTURE` が残っていても実体が無ければ 404）。取り込みの状態は見ない
  （rejected / failed でも実体があれば返す。placed は実体が消えているので普通 404。**画面は配置済みの取り込みの
  画像を Library の画像置き場 `/api/artwork/:hash` から出す**（配置で同じ画像が登録されている。`lib/inbox.ts` の
  `artworkUrl` / `trackPictureUrl` / `itemThumbUrl`））。セッション必須
  （allowlist 無し）。その他の I/O 失敗は 500。サムネイルは作らない（画面で縮小）。同梱の `cover.jpg`
  等は配置時に埋め込みより優先されるが（§7.1 の規則）、承認画面が見せるのは埋め込み画像だけ
- **採番**: 下書きの提案で TRACKNUMBER の無いファイルは、採用する album の active な `track_no` の最大 + 1 から
  ファイル名順に振る（無ければ 1 から）。承認の検証に「採用する album の active なトラックと `(disc_no, track_no)`
  が重ならない」を加え、配置で失敗する前に 400 で直させる。配置の登録トランザクションでも同じ検証をする
  （承認と配置の間に足された分。外れたら `failed`）
- **サイドカー `spindle-inbox.json`**（§7.7、D-70）: 取り込みのディレクトリにあれば `GET /api/inbox` が読み、
  `category` を提案の category（語彙に同じ canonical key があるときだけ）に、`files` の `verdict` / `message` /
  `url` / `channel` を各トラックに付ける。走査は音声でないので無視し、配置の成功時に消す（Library へ
  持っていかない）。壊れていれば無いものとして扱い、取り込みの `warnings` に載せる。モジュールは `import/sidecar.rs`
  （ダウンローダと CD の吸い出しが共有する）
- **CD の吸い出しの取り込み**（D-67 追記、P2-5）: サイドカーの `rip`（`RipEntry`: CTDB 形式の `toc`、吸い出し開始時の
  `metadata`（`DiscMetadata`。名前は空でもよい）、音声トラック順のファイル名 `files`、ドライブが読んだ `isrcs` /
  `mcn`（P4-21。旧サイドカーには無い）、rip.log の名前 `log`、`report`（`RipReport`））を持つ。提案は `album_gain = true`（D-74。保存した下書きがあればそちら）。配置は
  下書きの各トラックを **basename で `files` の位置へ結びつけ**（`bind_rip`。大小文字・正規化の違いは同じ名前）、
  記録の形（件数が音声トラック数と揃う、名前の重複なし）・取り込みのファイルとの 1 対 1・下書きの `disc_no` が 1 つに
  揃うこと（1 取り込み = 1 枚）を確かめ、外れたら配置せず `failed`（提案の `warnings` にも出す）。登録は
  `register_item` と同じトランザクションで `source_type = 'cd_rip'`、`album_verifications`（`source = 'rip'`、
  `disc_no` は下書きの値、`job_id` は NULL、`drive_offset` はレポートの `read_offset`（PCM に当てた
  読み取りオフセット。遡及照合の行は NULL。D-83 追記 3）、`log_path` は移した rip.log の Library 相対。移せなければ NULL）/
  `track_verifications` / `tracks.verification`（写像は §7.3 と同じ）。全トラックに `rip` の記録が既にあれば
  書かない（commit の後に落ちて再配置したとき）。サイドカーは読んだ FD の inode / size / mtime / ctime を
  登録の直前と消す前に照合し、変わっていれば登録せず `pending`（登録後なら消さずに残す）
- **承認後にファイルが増えた取り込み**（同じ album への追加ダウンロード）は既存の規則で `pending` に戻る。そのとき
  提案は「保存した下書き（既知のファイルの分）+ 新しいファイルの提案」を merge して返す（補正をやり直させない）

- **再判定**（`inbox_rejudge` ジョブ、`src/import/ytmusic/rejudge.rs`、D-114）: プラグインのルールを直した後に、承認前の
  YouTube の件をボタン 1 つで判定し直す。`POST /api/inbox/:id/rejudge`（pending で、サイドカーに `source = youtube` の項が
  ある件だけ）が、その件の YouTube 由来の全曲を対象にジョブを投入する。出どころの無い曲は触らない。プラグインはバイナリを
  判定のたびに起動するので、`cp` で置き換えれば再起動は要らない。
  - **材料**: 項の `item`（プラグインへ渡した Item）。無い既存の曲は、ダウンローダの手順 1 と同じ引数で yt-dlp を呼んで
    取り直して項に書き足す。取れなければその曲は失敗（`rejudge_error`。タグは触らない）で、他の曲は続ける
  - **1 曲**: 初回と同じ判定と写像（`downloader::judge` / `tag_changes_for` を共有）。購読由来（購読が残る）は albumartist /
    album / category を購読の値で上書きし、`ok: false` でも初回と同じ合成 Track を書く。購読由来でない曲は `ok` →
    TITLE / ARTIST / ALBUM / ALBUMARTIST / DATE / 追加タグを書き、前回書いた追加タグ（`plugin_tags`）のうち今回の応答に無い
    ものを消す（記録の無い曲は上書きだけ）。`ok: false` → TITLE を動画タイトルに戻しプラグイン由来のタグを消す。`skip` →
    判定の記録だけでタグは触らない。プラグインの故障 → その曲は失敗でタグもサイドカーも触らない。購読が消えた購読由来の
    曲で `ok` でないときは今のタグを保ち、判定だけ記録する。ファイル名も `<YYYYMMDD> <title> [<id>].opus` で付け直し、
    TRACKNUMBER / PICTURE / SOURCE_URL は触らない。件の `category` は判定の値で書き換え（判定に無ければ消す。タグを
    触らない判定では category も触らない）、語彙に無ければ足す。サイドカーの項に `verdict` / `message` / `plugin_version` / `judged_at` / `plugin_tags` を書く
  - **移動**: 判定が ok か否かと、曲が**今いるディレクトリ**（`youtube/_unmatched/` の下か）が食い違うときだけ、初回の規則の
    宛先へ移す（unmatched → ok は `youtube/<albumartist>/<album>/`、ok → unmatched は `youtube/_unmatched/<channel>/`）。
    ok のまま宛先だけ違う場合と購読由来の曲（購読が消えていても）は移さない。移動先に件が無いか `pending` のときだけ移し、
    それ以外は移さずタグだけ直して結果に「移せなかった（移動先が <state>）」を出す。宛先のサイドカーに項を先に書き、
    元のファイルを消してから元の項を消す。空になった件は走査が消す
  - **下書きは捨てる**: ジョブの最初に件の `draft` を消す（提案でタグより勝つため）。画面は、保存済みの下書きがある件では
    「保存済みの補正は消え、判定結果で置き換わります」、画面に未送信の編集がある件では「未保存の補正は消えます」と確認し、
    どちらも無ければ確認しない
  - **冪等・排他**: 開始時と曲ごと、それに曲ごとの判定（yt-dlp・プラグインの応答）の後でファイルを書く前に件が
    `pending` かを確かめ、外れたら止める（続けると走査が承認を pending に戻す）。件の再判定ジョブが queued / running の
    間は、件の承認・却下・再開を API が 409 `rejudging` で断る（状態の検査と同じトランザクションで見るので、確かめてから
    書くまでの間に承認されることはない）。曲ごとに「tmp に写してタグ → `RENAME_NOREPLACE` で新しい名前 → 元を unlink → サイドカー」。新しい名前に同名があれば、
    その `SOURCE_URL` が同じときは前回の途中の自分の成果物として今回の書いたもので置き換える。項のファイルが無いときは
    項を持たないファイルのうち `SOURCE_URL` が項の `url` と同じものを拾う。それも無い項は消す（移動で宛先の項を書いて
    元を消した後、元の項を消す前に落ちた残り。失敗には数えない）。ファイルは排他ロックしない。サイドカーの
    読んで直して書くは、並んで走る ytdl と共有するプロセス内の鍵（`DownloaderEnv::sidecar_lock`）で直列にする。ジョブは
    `max_attempts` 1・dedup `inbox_rejudge:<item_id>`。終わったら書いたディレクトリ（移動で新しく作ったディレクトリの
    親を含む）を fsync して `inbox` の走査を投入する。キャンセル・件が pending でなくなって途中で止まっても、書き換えた
    曲があれば同じく投入する。結果（`note`）は
    `再判定: N/M 曲、移動 K 曲、失敗 F 曲` と、移せなかった曲・失敗した曲の行
  - **画面**: 件の見出しに「再判定」（YouTube 機能が無効なら出さない。実行中は無効で `再判定中 done/total`、承認・却下・
    下書きに戻すも無効）と `判定: spindle-ytmusic-meta v…`（版が混ざれば `判定: v… ほか`、記録が無ければ `判定: 版不明`）。
    ③ の判定列の詳細に版と判定時刻、失敗の理由。失敗した曲があれば件の `warnings` に「再判定できなかった曲が N 曲」。
    終わったら一覧を取り直し、押した件が消えるか曲の形（名前・判定の記録）が変わった一覧が届いたところで、全曲が別の件へ
    移ったらその件を開き、一部なら「N 曲を <移動先> へ移した」と知らせる（終わってから 30 秒届かなければ「再判定した」
    だけ）。押してから投入が受理されて一覧に載るまでも無効（二度押し）。控えは件ごとに持ち、後から失敗した投入
    （409 `duplicate` 等）は受理済みの控えを消さない。`lib/inbox.ts` の `canRejudge` / `pluginVersionLabel` /
    `rejudgeConfirmText` / `rejudgeLanding` / `rejudgeSettle` / `rejudgeOutcome` / `rejudgeBusy` / `rejudgeStarting*` /
    `rejudgeWatch*`、`hooks/useInboxRejudge`


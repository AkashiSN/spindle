# 7.7 ytmusic 統合

| 元モジュール | 移行先 | 備考 |
|---|---|---|
| `parser.py`（タイトルパーサ）/ チャンネル定義 | **外部のメタデータプラグイン**（別リポジトリ） | spindle はタイトルの慣習を知らない（[D-69](../../decisions/D-069.md)） |
| `models.py` 正規化 | Rust | 置換テーブルは [D-43](../../decisions/D-043.md) で 1 本化済み |
| `tagger.py` (mutagen) | lofty | |
| `audio.py` R128 | ebur128 | ffmpeg loudnorm より高精度 |
| `organizer.py` / `playlist.py` | Rust | m3u8 生成は継続 |
| `downloader.py` | Rust（yt-dlp を subprocess） | |
| `sync.py` (ADB) | **移植する**（端末同期として再設計） | [§7.11](11-device-delivery.md)、[D-95](../../decisions/D-095.md) |

**メタデータプラグインのプロトコル v1**（`src/import/ytmusic/metadata.rs`、P3-1 / P3-2、[D-69](../../decisions/D-069.md)）。
動画のタイトルからトラックのメタデータ（タイトル・アーティスト・アルバム・category）を決める知識は
利用者固有（チャンネル名、タイトルの慣習、パターンのルール）なので spindle には置かず、外部コマンドに
問い合わせる。spindle が知る契約はこのプロトコルだけ。

- **起動**: `[ytmusic].metadata_command`（引数配列。`sh -c` は使わない）をアイテム 1 件ごとに起動し、stdin に
  Request を 1 つ書き、stdout の Response（JSON 1 つ）を読む。`metadata_timeout_secs`（既定 30）で kill。
  stderr はログに出す。終了コードが非ゼロ・stdout が JSON（厳密な UTF-8）でない・`protocol` が違う・
  必須の値が空（`title` / `albumartist` / `album` / `artists`）・`ok: false` なのに `reason` / `message` が
  無い・`category` がディレクトリ名として不正（`POST /api/categories` と同じ規則: 前後の空白、禁止文字、
  末尾のドット、予約名は不可）・`tags` に spindle が決めるキー（TITLE / ARTIST / ALBUM / ALBUMARTIST /
  DATE / TRACKNUMBER / DISCNUMBER、画像の PICTURE / METADATA_BLOCK_PICTURE、ReplayGain の
  REPLAYGAIN_* / R128_*）や不正なキー・空の値があるものは
  **プラグインの故障**として取り込みを止める
- **Request**（未知のフィールドはプラグインが無視する。前方互換）
  ```jsonc
  { "protocol": 1, "op": "metadata",
    "item": { "source": "youtube",            // 提供元
              "channel": "<設定のチャンネル識別子>",
              "channel_title": "…" | null,    // 提供元での表示名
              "id": "<動画 id>" | null, "url": "…" | null,
              "title": "<動画タイトル>",       // 必須
              "uploaded_at": "YYYY-MM-DD" | null, "duration_ms": 123 | null } }
  ```
- **Response**（判定できたかに関わらず終了コード 0）
  ```jsonc
  { "protocol": 1, "ok": true,
    "track": { "title": "…", "artists": ["…"], "albumartist": "…", "album": "…",
               "category": "<統制語彙の名前>" | null,   // null は未分類（_Unsorted）
               "date": "YYYY[-MM[-DD]]" | null,
               "tags": [["ORIGINALARTIST", "…"]] },    // 追加のタグ（キー, 値）
    "plugin": { "name": "spindle-ytmusic-meta", "version": "v0.1.0-rc.2" } }   // 任意。判定したプラグインの版（[D-114](../../decisions/D-114.md)）
  { "protocol": 1, "ok": false,
    "reason": "unmatched" | "unknown_channel" | "skip" | "unsupported",   // 未知の値も通す（前方互換）
    "message": "…",    // unmatched / unknown_channel は要対応（メッセージをそのまま見せる）、skip は取り込まない。
                       // reason / message は必須
    "plugin": { "name": "…", "version": "…" } }   // 任意（ok の応答にも同じ）
  ```
  `plugin` は**任意**（未知の欄はプラグインが無視する規則と同じく後方互換）。無い・形が違う（name / version が空でない
  文字列でない）ときは「不明」として扱い、応答は不正にしない。spindle は応答の `plugin.version` をサイドカーに残し、
  Inbox に表示する（再判定、[§7.8](08-inbox.md)、[D-114](../../decisions/D-114.md)）。`spindle-ytmusic-meta` は `--version` と同じ値（git describe）を返す
- **spindle 側の写像**: `track` → タグ（TITLE / ARTIST 多値 / ALBUM / ALBUMARTIST / DATE / TRACKNUMBER +
  `tags`）と `pathgen::TrackFields`（category / albumartist / artist（先頭）/ album / title / track_no / year）。
  `category` は `categories` に同じ canonical key の語彙が無ければ追加する（プラグインの定義が正。
  初回起動の空 DB でも動く）。トラック番号と配置は Inbox が行う（下記、[§7.8](08-inbox.md)、[D-70](../../decisions/D-070.md)）
- 参照実装: `AkashiSN/spindle-ytmusic-meta`（private。ルール TOML + フィクスチャ + チャンネル定義を同梱した
  Rust のバイナリ。新パターンは Claude がそのリポジトリでルールとフィクスチャを 1 件ずつ足す運用）。
  イメージには同梱せず、static バイナリをホストに置いて compose でマウントする（[§14](../14-deploy.md)、[D-70](../../decisions/D-070.md)）

**ダウンローダ**（`src/import/ytmusic/downloader.rs`、`ytdl` ジョブ、P3-3、[D-70](../../decisions/D-070.md)）。Library には触らず、
**Inbox に置くところまで**。配置・登録・後続ジョブは Inbox（[§7.8](08-inbox.md)）が担い、プラグインが判定したものも
人が一度見てから Library に入る。

- **入口**: `POST /api/ytmusic/download { urls }` が URL ごとに `ytdl` ジョブ（並列 1、dedup `ytdl:<url>`）を
  投入。YouTube 画面（1 行 1 URL。[§12.6](../12-ui/06-other-screens.md)）から呼ぶ
- **手順**（ジョブ 1 件 = URL 1 件。作業領域は `[paths].data/tmp/ytdl/<job_id>/`。全部引数配列、`--` の後に URL）
  1. `yt-dlp --dump-single-json --flat-playlist --no-download -- <url>`（120 秒）。`_type` が playlist なら
     `entries` の `url` ごとに `ytdl` を投入して終わり（展開だけ）。動画なら `id` / `webpage_url` / `uploader`
     （channel）/ `channel`（channel_title）/ `title` / `upload_date` / `duration` を取る。stderr が
     `Unsupported URL` / `is not a valid URL` なら `Fatal`、他の失敗は `Failed`（再試行）
  2. プラグインに問い合わせ（Request は上記。`channel` は `uploader`）。`ok` → 宛先
     `Inbox/youtube/<albumartist>/<album>/`（`sanitize_component`）。`ok: false` の `skip` → ダウンロードせず
     `done`（ログに message）。それ以外の reason（`unmatched` / `unknown_channel` / 未知）→ 宛先
     `Inbox/youtube/_unmatched/<channel>/`（受け皿）。プラグインの故障（`ProviderError`）→ `Fatal`。
     ここで Inbox のファイル名（8.）まで決まる
  3. 取り込み済みチェック: `SOURCE_URL = webpage_url` が Library（`track_tags`）にあれば**やることが無い**ので
     `done`（note に「取り込み済み（Library）: <パス>」。失敗ではない。P4-18、[D-81](../../decisions/D-081.md)）。Inbox（`inbox_files.tags`）にあり、それが自分の宛先そのもの
     （`rel_path_key` で比べる）なら「置いた後に落ちて走査が先に拾った」再実行の可能性があるので、
     **実ファイルの `SOURCE_URL` を読み直して**同じならダウンロードせずに 8. の仕上げ（サイドカーと投入）
     だけ済ませて `done`。違えば `Fatal`（同名で別の内容）、ファイルが消えていれば普通に置き直す
     （行はキャッシュ、ファイルが正）。Inbox の別の場所なら `done`（「取り込み済み（Inbox）: <パス>」）
  4. `yt-dlp -f "ba[ext=webm]" --no-playlist --write-thumbnail --convert-thumbnails jpg -o <tmp>/%(id)s.%(ext)s -- <url>`
     （`[ytmusic].download_timeout_secs`、既定 900）。webm の音声が無ければ `Fatal`。ネットワーク等の失敗は
     `Failed`（再試行）
  5. `ffmpeg -nostdin -y -i <tmp>/<id>.webm -vn -c:a copy -map_metadata -1 <tmp>/<id>.opus`（再エンコードなし）
  6. lofty でタグ。`ok`: `Track::tags(なし)` の TITLE / ARTIST / ALBUM / ALBUMARTIST / DATE / 追加 tags
     （TRACKNUMBER は書かない。採番は Inbox）+ PICTURE（サムネイル jpg）+ `SOURCE_URL`。`ok: false`: TITLE =
     動画タイトル + PICTURE + `SOURCE_URL` だけ（他は Inbox の警告で人が埋める）
  7. webm を `Archive/youtube/<id>.webm` へ（tmp + `RENAME_NOREPLACE`。既にあれば同じ id なので採用）
  8. `.opus` を宛先へ `<YYYYMMDD> <title> [<id>].opus`（title は `sanitize_component`。名前順 = 公開順 =
     採番順。`RENAME_NOREPLACE`。既にあれば、その `SOURCE_URL` が同じときだけ自分の成果物（置いた後に
     落ちた再実行）として採用し、違えば `Fatal`）。仕上げ: 同じディレクトリの `spindle-inbox.json` を読んで
     このファイルの項を足し tmp + rename で書く → ディレクトリを fsync → `inbox` ジョブを投入（すぐ取り込みが
     出る）。仕上げの失敗は `Failed`（再試行。次の実行は 3. か 8. の採用でここへ戻る）
- **サイドカー `spindle-inbox.json`**（タグに載らない情報を Inbox へ渡す。Inbox の DB には書かない）
  ```jsonc
  { "version": 1,
    "category": "<統制語彙の名前>" | null,          // 取り込みの category（プラグインの判定。最後に書いたものが勝つ）
    "files": { "<ファイル名>": { "source": "youtube", "url": "<webpage_url>", "channel": "<uploader>",
                                 "verdict": "ok" | "unmatched" | "unknown_channel" | "skip" | "<未知の reason>",
                                 "message": "…" | null,
                                 "subscription_id": 3, "position": 17,   // 購読由来のときだけ（P4-16）
                                 "item": { … },                // プラグインへ渡した Item（再判定の材料。[D-114](../../decisions/D-114.md)）
                                 "plugin_version": "v0.1.0-rc.2" | null,   // 判定したプラグインの版（Response の plugin.version）
                                 "judged_at": 1760000000,      // 判定した時刻（UNIX epoch 秒）
                                 "plugin_tags": ["ORIGINALARTIST"],   // ok のときにプラグインが書いた追加タグのキー（大文字）
                                 "rejudge_error": "…" } } }    // 直近の再判定がこの曲で失敗した理由（成功で消す）
  ```
  `item` 以降の 4 欄は任意（無いサイドカーも読める。`version` は 1 のまま）。ダウンローダが初回の判定で `item` /
  `plugin_version` / `judged_at` / `plugin_tags` を書き、再判定（[§7.8](08-inbox.md)）が書き換える
- **失敗の区分**: `Fatal`（再試行なし）は webm の音声なし・プラグインの故障・対応していない URL・宛先の
  同名で別の内容（取り込み済みは失敗でなく `done` + note）。それ以外（yt-dlp / ffmpeg の非ゼロ終了、I/O、仕上げ）は `Failed` で指数
  バックオフ。作業領域はどの終わり方でも消す。**再実行は冪等**: Archive は同じ id なら採用、Inbox は
  `SOURCE_URL` で自分の成果物を見分けて続きから
- `POST /api/ytmusic/download` の URL は http / https でホストがあり空白・制御文字を含まないものだけ受ける
  （ホストは限定しない。対応していなければジョブが `Fatal` で伝える）
- yt-dlp は YouTube の抽出に JS ランタイムを要求する版があるため、runtime イメージに deno を同梱する（[§14](../14-deploy.md)）

**再生リストの購読と同期**（`playlist_subscriptions`、`playlist_sync` ジョブ、P4-16、[D-78](../../decisions/D-078.md)）。URL を貼る
運用をなくす。再生リスト 1 本 → Library の album 1 つ（追記先）。`SOURCE_URL`（P4-14 で補填、ytdl が
書く）で「再生リストのどこまで持っているか」が分かる。

- **購読**（`db::subscriptions`、`/api/ytmusic/subscriptions`）: `id` は再利用しない（AUTOINCREMENT。
  ジョブの payload とサイドカーが裸の id を持つので、DELETE 直後に作った購読へ旧ジョブが誤帰属しない）、
  `list_id`（URL の `list=`。YouTube のホストだけ。UNIQUE）、`albumartist` / `album` / `category`（追記先の初期値と表示用）、`album_id`
  （**追記先の同一性**。登録時は NULL。同期か配置が `import::inbox::destination_of`（Inbox の追記先と
  同じ規則）で引けたときに CAS（`album_id IS NULL` のときだけ）で束ねる。album 全体の移動は id を
  維持する（[D-32](../../decisions/D-032.md)）ので以後は album 行の値が正。album が消えれば SET NULL で再解決。albumartist /
  album / category を PATCH で変えると NULL に戻す）、`align`（番号揃えをするか。既定 on）、`enabled`
  （定期と承認の後続の対象か。手動の同期は enabled に関わらず受ける）、`max_enqueue`（1 回の同期で
  投入する上限。既定 50。誤登録した巨大なリストで数百本落とさない）。同じ追記先の購読は 1 つだけ
  （`target_key` = `sanitize_component` した albumartist / album の canonical key、UNIQUE。`album_id` も
  非 NULL の間 UNIQUE = 束ね同士の競合は DB で片方が失敗する）。Inbox の `youtube/<albumartist>/<album>`
  = 1 購読 = 1 category になるので、サイドカーの category が dir 単位でも他の購読に波及しない
- **同期ジョブ `playlist_sync`**（並列 1、dedup `playlist_sync:<id>`、payload `{ subscription_id }`）。
  順序は **列挙 → 追記先 → 照合 → 番号揃え → ytdl 投入**（購読由来の ytdl は TRACKNUMBER = 位置を書くので、
  先に既存の行を揃えて隙間を空ける）:
  1. `last_attempted_at = now`、latch（`sync_requested_at`）を NULL に。`yt-dlp --flat-playlist
     --dump-single-json`（`[ytmusic].ytdlp_args` 付き）で列挙。entry の位置（1 始まり）が再生リストの位置。
     `entries < playlist_count` なら**古い yt-dlp の取りこぼし**として `Failed`（再試行）で何も投入しない
  2. 追記先: `album_id` があればその album（missing なら `Fatal`）、無ければ引いて束ねる（別の購読が束ねて
     いれば `Fatal`。読んでから束ねるまでに配置が束ねていればそちらが正）。まだ無ければ揃えは無し（最初の
     配置で束ねる）。同期が queued / running の間は PATCH / DELETE を 409 `sync_running` で拒む（走行中の同期が
     古い追記先で揃えたり、消えた購読の album を揃えたり、死んだ id で投入したりしない。検査と UPDATE は同じ書き込み閉包 = 投入と直列）。加えて各段の
     前に購読を読み直し、消えていれば `Fatal`、`updated_at` が進んでいれば `Failed`（直接 DB を書いた場合の
     控え）
  3. entry ごとに正規 URL `https://www.youtube.com/watch?v=<id>` で Library の active 行を**全件**引く
     （`find_source_url` の LIMIT 1 でなく 0 / 1 / 2 件以上を区別）。追記先にあれば「Library」、別の album
     なら「別の album にある」（触らない、投入しない）、Inbox にあれば「取り込み中」（投入しない）
  4. **番号揃え**（`align` が on。`import::ytmusic::playlist::plan_align`）: 対象 = 追記先の active 行で
     `SOURCE_URL` が entry に一致し disc 1（NULL 含む）で単一のもの。目標番号 = 位置。**固定行** = それ
     以外（`SOURCE_URL` 無し・再生リストに無い・`SOURCE_URL` 重複）の disc 1 の行で、その番号は塞がる。
     目標番号が塞がれた対象と disc 2 以降・重複の対象は動かさず「揃えられない」一覧へ（理由付き）。
     **同じ動画が再生リストに複数回ある**ときも表現できない（1 行は 1 番号）ので、その行は固定して「揃え
     られない」（`duplicate_entry`、位置の一覧）に出し、投入は最初の位置でだけ行う。対象
     同士の swap / cycle は可（番号は UNIQUE でなく、rename は一時パス経由）。非公開・削除・未取り込みの
     位置も数えるので、それらの分は番号が飛ぶ。
     **各段の前に追記先の active 行に pending の op が無くなるまで待ち**（2 秒間隔、上限 10 分。超えたら
     `Failed`）、差分は待った後の DB から計算し直す（phase を永続化しない: 落ちて再実行しても、失敗した op は
     overlay がファイルの値に戻るので次回また差分になり、適用済みなら差分ゼロで通る）。ずれた行だけ
     `Editor::prepare_tags`（`TRACKNUMBER`。説明「再生リスト『…』に番号を揃える」）→ 終端待ち → album を
     読み直し「**現在の TRACKNUMBER が目標位置と一致する対象行のうち、ファイル名の先頭の番号が合っていない
     行**」を `plan_rename` → **ファイル名だけ**テンプレートのものにして（ディレクトリは今のまま。テンプレートの
     dir で動かすと category 未推定の album を `_Unsorted` へ移してしまう）`prepare_rename` → 終端待ち
     （今回のバッチの applied 集合には依らない = tags 適用後・rename 前に落ちた境界を再実行で拾う。名前の
     書式だけ違う行は触らない）。計画の時点の衝突（同名のファイルがある等）は続く状態なので失敗にせず
     「改名できない」として報告だけ。どちらも履歴に載り巻き戻せる。Derived（opus / aac）はタグ上書き・移動で
     追随する。子バッチの終端は **batch_id の pending = 0** で待つ（album の active 行で見ると、対象が
     走査で missing になったときに pending の op を見落とす）。**全件 applied でなければ**（pending が残る、
     外部の書き換えによる衝突・失敗・キャンセル、applied ≠ total）揃えは終わっていないので `Failed`（再試行で差分から取り直す）。揃え終わる前の失敗・
     キャンセルでは投入しない（そこまでのバッチ id と件数は結果に残す）
  5. Library / Inbox / 別 album に無く、**取れる** entry を位置順に `max_enqueue` まで `ytdl` 投入（payload
     `{ url, subscription_id, position }`、dedup は今までどおり `ytdl:<url>`）。超えた分は「次回」。
     Duplicate（手動貼り付け・別の購読の投入が走行中）は**満たしたと数えず「別の投入が走行中」**として
     結果に出すだけ（そのジョブには購読の情報が無い。承認されれば次の同期が Library で拾い、失敗すれば
     dedup が空いて購読の情報付きで再投入。同じ動画が複数の購読に出る場合も同じ規則で、先に取り込んだ側の
     album に置かれ他方には「別の album にある」）
  6. **取れない entry**（`title` が無い、旧版の `[Private video]` / `[Deleted video]`）: 投入しないが位置は
     占める。Library にあれば（補填済み、公開だった頃に取り込んだ）何もしない。無ければ「取れない」一覧
     `{ position, id, kind: private | deleted | unknown }`（現行の yt-dlp は `title: null` で来るので区別
     できず `unknown`。旧版の題名から分かるときだけ private / deleted）。後で公開に戻れば次の同期で拾う
  7. `last_result`（結果 JSON: entries / playlist_count / in_library / in_inbox / elsewhere / enqueued /
     running / deferred / unavailable / align{moved, unchanged, blocked, outsiders, unnumbered, tags, renamed,
     rename}、state = done / failed / cancelled）、成功なら `last_synced_at`、`jobs.note` に 1 行。終わる前に
     latch が立っていれば（走行中に承認の後続・手動要求が来た）`Outcome::Requeue`（試行回数を数えず同じ
     ジョブがもう一度走る）
- **ytdl の購読由来**（payload に `subscription_id`）: ALBUMARTIST / ALBUM / category は購読の追記先（束ねて
  あれば album 行の値）、プラグインの判定は TITLE / ARTIST に使う。**skip / 判定不能でも投入**（再生リストは
  人が選んだもの。TITLE = 動画タイトル、ARTIST = albumartist、verdict はサイドカーに残す）。宛先は
  `Inbox/youtube/<albumartist>/<album>/`。`align` が on なら **TRACKNUMBER = 位置**を書く（同期が先に既存の
  行を揃えて隙間を空けている。承認画面の初期値がそのまま正しい番号になり、塞がっていれば承認の既存検査
  （400）で人が直す）。off なら書かず Inbox が max+1 を振る。購読が消えていれば通常の ytdl として振る舞う（skip も通常どおり。
  購読の有無はプラグインを呼ぶ前に引く）
- **承認はそのまま**（[D-70](../../decisions/D-070.md)。判断は Inbox で人が行う）。配置（`place_item`）はサイドカーを消す前に取り込みの
  `subscription_id` を集め、inbox ジョブが配置後に追記先を束ね（CAS）、latch を立ててから `playlist_sync`
  を投入する（走行中なら Duplicate だが latch は残る）
- **dispatcher**（常駐、30 秒ごと。`[ytmusic].enabled` のとき）: (a) latch の立った購読を投入（enabled に
  関わらず。active なジョブがあれば Duplicate で何もせず、終端になった次の tick で投入される = handler の
  最終確認から終端までの窓に立った latch、Failed / Cancelled で残った latch も必ず回収される）、(b)
  `[ytmusic].sync_interval_hours`（既定 0 = 手動と承認の後続だけ）が 0 でなければ enabled で
  `last_attempted_at`（成否を問わない開始時刻）から interval 経った購読を投入（最終 retry が failed に
  なっても interval までは新しいジョブを作らない）
- yt-dlp を定期的に叩くので、ブロック時の `ytdlp_args`（P4-13）と yt-dlp の更新（P4-12）が前提

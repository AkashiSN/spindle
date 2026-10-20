# 9. HTTP API

```
GET    /health                                    認証の外。{ status: "ok" | "locked", version, ytdlp }
                                                  version = ビルド時に焼いた版（CI の git describe --tags --always。
                                                  `spindle --version` と同じ。P4-12、D-79）、ytdlp = 起動時診断で
                                                  取った yt-dlp の版（取れなければ null）
GET    /api/tracks?filter=&sort=&cursor=&limit=   カーソルページング（D-39）
                                                  filter = URL エンコードした JSON（下記）、
                                                  sort = album(既定) | title | artist | album_title |
                                                  albumartist | date | duration | codec | rel_path | id
                                                  （`-` 前置で降順）、limit = 1..=1000（既定 100）
GET    /api/tracks/:id                            セッション有りは一覧と同じ行に detail（タグ全部・物理属性・
                                                  verifications = 手法ごとの最新の照合。D-58 / D-96）を足す。
                                                  trusted_cidrs からのセッション無しは限定フィールド（D-27 / D-39）
PATCH  /api/tracks/batch                          一括編集（dry_run フラグ）
POST   /api/tracks/batch/preview                  変更プレビュー
POST   /api/rename/preview                        テンプレート適用結果（selection_token を発行）
POST   /api/rename/apply                          token の集合をリネームバッチとして記録
POST   /api/normalize/preview                     ロスレス → FLAC 正規化の宛先（selection_token を発行）
POST   /api/normalize/apply                       token の集合を正規化バッチとして記録（§7.4）
POST   /api/rg                                    { selection }。rg ジョブを投入（D-47）。`albums.album_gain` が on の
                                                  album は album 単位（rg:album:<id>）、それ以外は track 単位（rg:track:<id>。D-74）
POST   /api/rg/write                              { selection, description?, skip_pending? }。解析値を
                                                  タグとして書く編集バッチを記録（§6、D-48）。
                                                  preview 段階は無い（値は DB から決まる）。解析後は
                                                  rgwrite が自動で書くので、これは自動で書かれなかった
                                                  行（外部で書き換えた・巻き戻した等）の書き直し用（D-97）
POST   /api/flaccheck                             { selection }。active な FLAC ごとに flaccheck ジョブを
                                                  投入（§7.9、D-57。読むだけで preview は無い）
POST   /api/hirescheck                            { selection }。対象（可逆かつ >48 kHz または >16 bit）ごとに
                                                  hirescheck ジョブを投入（§7.10、D-71。読むだけで preview は無い）
                                                  → 202 { tracks, skipped, duplicates, job_ids } | 409 no_changes
POST   /api/md5fill                               { selection, description?, skip_pending? }。flac_check = md5_missing
                                                  の FLAC に md5 op の編集バッチを記録（§7.9、D-59）
                                                  → 201 { batch_id, affected, skipped, pending_excluded }
                                                  → 409 pending | no_changes | md5_fill_disabled
POST   /api/verify                                { selection }。selection のトラックが属する album ごとに
                                                  verify ジョブ（遡及照合）を投入（§7.3、D-13 / D-63。読むだけ）
                                                  → 202 { albums, duplicates, job_ids }
                                                  → 409 no_changes

GET    /api/albums / :id                         全件（ページングなし）。track_count / duration_ms は active のみ。
                                                  `?filter=`（/api/tracks と同じ JSON）で一致するトラックを持つ album だけ（P4-6）
                                                  /api/tracks の行と /api/tracks/:id には artwork_hash（トラック自身の
                                                  埋め込み画像。無ければ null。D-61）。行に album_gain（D-74）
PATCH  /api/albums/:id                            { album_gain }。album gain の属性（D-74）。true → album 単位の rg を投入、
                                                  false → rg_album_* を NULL にして未書込に戻し、Derived の追随を投入。
                                                  同じ値なら何もしない。200 { album, job_id | null, derived_jobs }。
                                                  404 not_found / 400 bad_request
GET    /api/categories, POST /api/categories        統制語彙 { "items": [{ id, name, albums, genres, subscriptions, inbox_drafts }] }
                                                  （4 つは使用数。削除できるのは全部 0 の語彙。D-106）。POST は { name }（重複は 409）。
                                                  CD 取り込みの確定フォームの category に使う（D-67）
DELETE /api/categories/:id                        使われていない語彙だけ消す（D-92）。204 / 404 / 409 `in_use`
                                                  （active な album・GENRE の写像・購読・Inbox の下書きのどれかが
                                                  使っている。`message` に理由）。missing の album は SET NULL
GET    /api/tags/keys                             ライブラリにあるタグのキーと、そのキーを持つ曲数 → { items: [{ key, tracks }] }
                                                  （曲数の多い順、同数はキーの順。多値も 1 曲、missing の曲と PICTURE は数えない）。
                                                  タグのキーの入力欄の前方一致の候補（D-113）
GET    /api/search?q=                             FTS5 trigram（3 文字未満は LIKE）。/api/tracks と同じ
                                                  レスポンス形で、filter / sort / cursor / limit も受ける

GET    /api/stream/:id                            Range 対応。原本
GET    /api/stream/:id?transcode=opus             オンザフライ変換
GET    /api/stream/:id?transcode=aac              AAC（スマホ版の試し聴きの退避。§11、§12.7、D-115）
GET    /api/artwork/:hash?size=                   size = 256 | 768 で WebP のサムネイル、無しで原画像
                                                  （元の MIME）。hash は albums.artwork_hash。未生成なら
                                                  原画像へ倒す（no-cache）。ハッシュアドレスなので immutable
POST   /api/artwork/upload                        生の画像バイト列（Content-Type: image/*、上限 32 MiB。形式は
                                                  ヘッダで判別。JPEG / PNG / WebP 以外は 400 unsupported_image）を
                                                  ArtworkStore と artwork 行に置き thumbnail ジョブを投入
                                                  → 201 { sha256, mime, width, height, bytes }（D-60）
POST   /api/artwork/embed                         { selection, sha256, description?, skip_pending? }。selection の
                                                  active 全行の埋め込み画像をその 1 枚に差し替える tags op
                                                  （PICTURE）の編集バッチを記録（§7.5、D-60）
                                                  → 201 { batch_id, affected, unchanged, pending_excluded }
POST   /api/artwork/from-caa                      { release_id } か { release_group_id }（どちらか片方。D-93）。release_id は MBID
                                                  （大小無視）か musicbrainz.org の /release/<MBID> の URL、release_group_id は
                                                  MBID。Cover Art Archive の front 画像（500px）を
                                                  取り、upload と同じく置く → 201（upload と同じ応答）。両方・どちらも無し・形が
                                                  違えば 400 bad_request。画像の無い盤は 404、
                                                  上流の失敗は 502 lookup_failed、未構成は 503 coverart_unavailable（D-86）
                                                  → 404 artwork_not_found、409 pending | no_changes

GET    /api/playlists, POST, PATCH, DELETE        プレイリストの CRUD（D-53）。DELETE は印のある端末に open な作業があれば
                                                  409 open_plan（D-102）。POST / PATCH に rule があれば
                                                  スマート（D-54。評価結果は playlist_items に書く）。並びは
                                                  /api/tracks?filter={"playlist_id":N}&sort=position
POST   /api/playlists/:id/items                   { selection, sort? } を末尾に追加（同じトラックは 1 回）
DELETE /api/playlists/:id/items                   { track_ids?, selection? } を外す
POST   /api/playlists/:id/items/move              { track_ids, before } before の直前（null は末尾）へ
POST   /api/playlists/preview                     { rule } スマートルールの検証と評価件数（保存しない。D-54）
POST   /api/playlists/:id/refresh                 スマートを今の DB で再評価（項目を書き直す）
GET    /api/playlists/:id/export?profile=         m3u8 の本文（trusted CIDR で認証スキップ）
POST   /api/playlists/:id/export?profile=         Playlists/<profile>/<name>.m3u8 へ書き出し。応答に
                                                  count / skipped_missing / stale_tags（delivery のタグ追随待ち）
GET    /api/playlists/import, POST                Playlists root 下の m3u8 の一覧 / { path, name? } で取り込み
GET    /api/playlists/:id/fb2k_query              foobar Autoplaylist 用の { query, sort, notes }（smart のみ。D-55）

GET    /api/devices                               端末の一覧 → { items: [Device] }（下の「レスポンス形」。P5-2、D-95）。ハッシュの無い送る元の
                                                  source_hash をここと差分で投入する（§7.11）
POST   /api/devices                               { name, transport: adb | agent, variant: opus | aac, selection: all | playlists, serial?, volume? }
                                                  → 201 Device。agent は行を作るだけ。adb は serial と volume（`emulated` か SD の UUID）が要り、
                                                  接続を確かめ、保存先 `Music/spindle` が無いか空（曲もプレイリストも指さない空の manifest の
                                                  前の登録の失敗の残りは片付ける）なら manifest を作って device_scan を投入する（§7.11「登録」）。
                                                  `adopt: true`（adb のみ。他の transport は 400）は既存の保存先の引き継ぎで、manifest の uuid の
                                                  行を作る（§7.11「引き継ぎ」、D-103）。400 bad_request、
                                                  409 duplicate（名前）| serial_registered | not_empty | not_connected |
                                                  adoptable（引き継げる manifest がある。引き継ぎを案内）| not_adoptable | uuid_registered |
                                                  library_busy（scan が待ち・実行中）、502 device_failed、503 adb_disabled
PATCH  /api/devices/:id                           { name?, selection?, variant? } → 200 Device。名前以外は open な計画か待ち・実行中の
                                                  device_sync がある間 409 open_plan。409 duplicate。保存先は変えない（削除して登録し直す）
DELETE /api/devices/:id                           → 204。端末上のファイルには触らない。open な計画も行と一緒に消え、待ちの端末ジョブは
                                                  取り消す。404 / 409 busy（端末のジョブが実行中か端末のロック中。D-98）
PUT    /api/devices/:id/playlists                 { playlist_ids }（印の全置換）→ 200 Device。400 bad_request（無いプレイリスト）|
                                                  cycle（on_device / device_pending を使うスマート）、409 open_plan
GET    /api/devices/:id/diff                      差分・見積もり・評価時刻と plan_token（下の「レスポンス形」）
GET    /api/devices/:id/estimate?selection=&playlist_ids=1,2
                                                  選曲タブの見積もり（保存しない）→ { tracks, bytes, estimated, unhashed }（bytes は確定と
                                                  推定の和、estimated = 推定で数えた曲数、unhashed = 推定もできず
                                                  bytes に含まない曲数。D-107）
GET    /api/devices/adb/unregistered              接続中で未登録の Android → { items: [{ serial, model, state, volumes: [{ volume, path, free,
                                                  state: missing | empty | nonempty | adoptable }], error }] }（volumes は state = device の端末だけ
                                                  調べる。adoptable = 読める manifest が曲かプレイリストを指し、volume が一致し、uuid が DB に無い）。503 adb_disabled
GET    /api/devices/adb/adopt-preview?serial=&volume=
                                                  引き継ぎのプレビュー（読むだけ。ジャーナルは回復しないので件数は目安）→ { device_uuid,
                                                  tracks_adopted, tracks_dropped, playlists_adopted, playlists_dropped, unmanaged, library_busy }
                                                  （playlists_adopted は件数）。409 not_adoptable | uuid_registered、503 adb_disabled
POST   /api/devices/:id/sync                      { plan_token }（adb のみ）。計画の確定と device_sync の投入を 1 トランザクションで行う →
                                                  202 { job_id }（同じ plan_token の open な計画があればそれで投入し直す。未接続でも 202 で、
                                                  ジョブが接続を待つ）。409 plan_changed（本文に今の plan_token）| open_plan_exists |
                                                  pending_reevaluation | plan_unreadable、400（agent の端末）、503 adb_disabled
POST   /api/devices/:id/plans/open/resume         adb の open な計画の同期を投入し直す → 202 { job_id }。404 no_open_plan、409 plan_unreadable
POST   /api/devices/:id/plans/open/abandon        { force? }。adb は接続して回復し、キャッシュを置き換えてから計画を abandoned にする →
                                                  200 Device。409 not_connected | busy、404 no_open_plan。agent は force 無しでは 400
                                                  （Mac の spindle-agent から破棄する）。force: true は端末につながず回復もせずに閉じる
                                                  （agent も可。adb_disabled でも動く。409 busy。D-98 / D-99）
POST   /api/devices/:id/verify                    adb のみ。device_verify を投入 → 202 { job_id }
POST   /api/devices/:id/pair-code                 agent のみ。10 分・1 回限り・5 回まで試せるコードを発行 → 201 { code, expires_at }
                                                  （旧トークンと旧コードは即失効）。400 not_agent、409 open_plan
// Android の操作（登録・未登録の一覧・同期・再開・通常の破棄・検証）は `[devices].adb_server` が空なら
// 503 adb_disabled（強制破棄と端末の削除は adb を使わないので動く。D-98）

// エージェントの経路（認証は下の「認証」。セッション・CSRF・trusted_cidrs は通らない。D-99）
POST   /api/agent/pair                            { code, adopt_uuid? }（Bearer なし。端末タブで発行した `<selector>.<secret>`）→ 200 { device_uuid,
                                                  device_name, token }。不一致・期限切れ・試行超過・引けないセレクタは一律 401 invalid_code。
                                                  adopt_uuid（`pair --adopt`。32 桁の小文字 16 進。形の不正はコードに触らず 400 bad_request）は
                                                  コードの消費と同じトランザクションで行の uuid を差し替える。行が端末の状態を持つか（uuid が
                                                  既に adopt_uuid なら報告か計画があるか）、uuid が他の行にあれば 409 not_adoptable（コードは消費せず、試行にも数えない。§7.11「引き継ぎ」、D-104）
GET    /api/agent/manifest                        → { device_uuid, device_name, generation, plan_token, pending_reevaluation,
                                                  items: [{ track_id, dest_path, token, size, sha256, rel_path, source }],
                                                  playlists: [{ playlist_id, name, token, tracks: [track_id] }],
                                                  diff: { items: [{ op, track_id, from, to, token, size, sha256 }],
                                                  held: [{ track_id, reason, waiting, has_copy }],
                                                  playlists: [{ op, playlist_id, from, to, token }], playlist_errors: [{ playlist_id, reason }] } }
                                                  （差分は UI と同じスナップショットから作る。D-99）
GET    /api/agent/files/:track_id                 If-Match: "<配信トークン>"（強い ETag）必須、続きは Range を足す（HEAD も可）→ 200 / 206。
                                                  428 if_match_required、412 precondition_failed（トークン不一致・弱い ETag・`*`・並記・
                                                  ハッシュ未計算）| source_changed（送る元が変わった。行を捨てて次の差分で取り直させる）、
                                                  404（その端末の desired に無い）、416、503 files_unavailable。パスはクライアントから受け取らない
POST   /api/agent/plans                           { plan_token } → 201 Plan（新しく確定）| 200 Plan（同じ plan_token の open な計画）。
                                                  409 plan_changed（本文に今の plan_token）| open_plan_exists | pending_reevaluation | plan_unreadable
GET    /api/agent/plans/open                      → 200 Plan | 404 no_open_plan | 409 plan_unreadable
POST   /api/agent/plans/:id/abandon               Report に { pending_ops, pending_batches }（件数）を足した本文 → 200 {}。報告と同じ検証を通り、
                                                  キャッシュを全置換して計画を abandoned にする。409 pending_work（どちらかが 0 でない）、
                                                  ほかは report と同じ
POST   /api/agent/report                          Report → 200 {}（計画を completed にする。同じ報告の再送も 200）。400 invalid_report
                                                  （1 件でも外れれば全体を拒否。§7.11「報告」）| bad_request、404 no_plan、409 generation_mismatch |
                                                  plan_closed | plan_unreadable。本文の上限は report と abandon と adopt だけ 64 MiB
POST   /api/agent/plans/:id/progress              { generation, done, total, bytes_done, bytes_total, rate, phase, current? } → 204。実行中の進捗を
                                                  メモリだけに置く（DB には書かず、SSE `device_progress` で流す。§7.11「進捗」、D-108）。phase は
                                                  delete | move | fetch | add | playlist、rate はバイト / 秒、current は 1,024 バイトまで。
                                                  404 no_plan（その端末の open な計画でない: 閉じた・破棄済み・別の端末・無い）、
                                                  409 generation_mismatch、400 invalid（done > total・bytes_done > bytes_total・未知の phase・
                                                  負の値・current 超過）。本文の上限は 4 KiB。エージェントは 404 / 409 で以後送らない
POST   /api/agent/adopt                           { tracks: [{ track_id, path, rel_path, source, size, sha256, stale }],
                                                  playlists: [{ playlist_id, name }] } → 200 { tracks: [{ from, to }], playlists: [{ from, to }] }
                                                  （`to` は付け替え先の id、外れたものは null）。DB 全損後の `pair --adopt` で、付け替えた状態を
                                                  キャッシュへ直接書く（`generation` と `last_synced_at` は進めない。冪等。§7.11「引き継ぎ」、D-104）。
                                                  400 bad_request（件数の上限・パスの形・未知の source・id とパスの鍵の重複・空のプレイリスト名・
                                                  sha256 が小文字 16 進 64 桁でない（空は `stale` のときだけ許す）・size が i64 を超える）、
                                                  409 library_busy（スキャン中）| open_plan | not_adoptable（既に報告を受けた端末）
// Report = { generation, plan_id, state: { tracks: [{ track_id, dest_path, token, size, sha256 }],
//            playlists: [{ playlist_id, name, token }] }, errors: [{ kind: track | playlist, ref_id, reason }] }
// Plan   = { plan_id, generation, plan_token, items: [{ op_id, op, track_id, from, to, token, size, sha256 }],
//            playlists: [{ op_id, op, playlist_id, from, to, token }] }
//          op = add | update | move | update_move | delete（プレイリストは add | update | delete）
//          ワイヤ型は agent-proto クレートで本体とエージェントが共有する

POST   /api/auth/login, POST /api/auth/logout
GET    /api/auth/session

GET    /api/cd/status                             { state: unknown | no_drive | no_disc | tray_open | not_ready | disc_ok,
                                                  toc: CTDB 形式の文字列 | null, isrcs: [音声トラック順。無ければ null],
                                                  mcn: JAN/UPC | null, error: 直近の失敗 | null, checked_at,
                                                  tracks: [{ number, length_ms }], rip_job: 進行中の rip ジョブ | null,
                                                  drive: { model, offset, offset_source: manual | learned | table | unknown } | null
                                                  （型番はディスクが無くても読む。D-83 追記 2）}
                                                  （P2-1。ポーラの状態で、ドライブは叩かない。TOC は下の lookup に渡す
                                                  文字列と同じ形。ドライブ未配線なら 503 cd_unavailable）
POST   /api/cd/lookup                             { toc, isrcs?, mcn?, release?, catno?, barcode?, refresh? }。TOC 文字列（CTDB 形式 0:13915:…:leadout か
                                                  MusicBrainz 形式 1 12 leadout+150 offset+150…）から各種 DiscID を出し、
                                                  MusicBrainz に照会（P2-3、D-21 / D-64）。isrcs / mcn は status が読んだもの
                                                  （null は捨てる）、release は貼ったリリース URL か MBID。catno / barcode は
                                                  ユーザが盤から読んで入れた品番（英数字・ハイフン・空白、32 文字まで）と
                                                  JAN / UPC（数字 8・12・13 桁。mcn が無いときだけ使う）。D-94。→ 200 { discid,
                                                  mb_toc, accuraterip_id, ctdb_toc_id, exact, candidates: [リリース × medium。
                                                  matched_by: [catno | discid | release | isrc | barcode | toc] を強い順に持ち、その順に並ぶ。
                                                  media: [{ position, format, track_count }]（リリース全体の収録構成）],
                                                  notes: [候補に入れられなかった理由], tracks: [{ number, length_ms }]（TOC の
                                                  音声トラック。手入力フォームの行。D-65）}。同じ入力の照会結果は
                                                  10 分覚えていて MusicBrainz を引き直さない（D-64 追記 3）。
                                                  refresh: true で捨てて引き直す（画面の「MusicBrainz に照会」）。
                                                  400 bad_request（TOC・ISRC・MCN・品番・JAN の形）、502 lookup_failed（届かない・応答が壊れている）、
                                                  503 musicbrainz_unavailable（再試行しても 503 の負荷制限、または未構成）
POST   /api/cd/library                            { toc, release_id? }。いまの盤がライブラリにあるか（§12.6 CD の帯、D-85）。
                                                  DiscID はサーバが TOC から出し、active なトラックの MUSICBRAINZ_DISCID
                                                  タグで引く。当たらず release_id があれば active な album の mb_release_id
                                                  （大文字小文字を区別しない。D-84）で引く。DB だけを読む（MusicBrainz は
                                                  引かない）→ 200 { discid, disc: AlbumRef | null, release: AlbumRef | null }
                                                  （AlbumRef = { album_id, rel_dir, album, albumartist }。複数あれば id の
                                                  小さいもの。disc が当たれば release は引かない）。400 bad_request（TOC）
GET    /api/cd/cover/{release_id}                 候補のジャケット。Cover Art Archive の front（500px）の中継（D-82）。
                                                  画像の無い盤は 404、上流の失敗は 502 lookup_failed、未構成は 503 coverart_unavailable
GET    /api/cd/release-group/{id}/releases        リリースグループの版（D-93）。MusicBrainz の browse
                                                  （ws/2/release?release-group=…&inc=media+labels、先頭 100 件）→ 200 { releases:
                                                  [{ release_id, title, disambiguation, date, country, status, formats: [媒体の形式],
                                                  label, catalog_number, front: CAA に表の画像があるか }]（front のある版が先、
                                                  同じなら日付の古い順）, total }。知らないグループは 404、MBID でなければ 400、
                                                  502 lookup_failed、503 musicbrainz_unavailable（負荷制限・未構成）
POST   /api/cd/rip                                { toc, metadata }（P2-5）。metadata は CD 画面の下書き（DiscMetadata。名前は空でもよい）。
                                                  盤がドライブにあり TOC が一致するときだけ rip ジョブを投入 → 202 { job_id }。
                                                  400 bad_toc / bad_metadata、409 disc_mismatch（盤が違う・無い）/ duplicate
                                                  （進行中の吸い出しがある。ドライブは 1 台）、503 cd_unavailable。進捗は SSE の
                                                  job イベントの detail（RipProgress。§7.2）、結果の一行はジョブの note
POST   /api/cd/eject                              トレイを開けて状態を見直す → 204。失敗は 500 eject_failed（理由付き）。
                                                  先に CDROM_LOCKDOOR 0 で扉のロックを外す（外さないとドライブが
                                                  CHECK CONDITION で拒む機種がある）。吸い出し中（rip が running）は 409 ripping

GET    /api/jobs, POST /api/jobs/:id/cancel, POST /api/jobs/:id/retry
DELETE /api/jobs/:id                              終端（failed / done / cancelled）のジョブ行を消す（P4-18。queued / running は 409 not_terminal）
DELETE /api/jobs?state=failed                     失敗をまとめて消す → { "deleted": N }（state=failed 以外は 400）
GET    /api/config                                読み込んだ config.toml の原文 { "path", "text" }（設定画面 §12.6。秘密は config に無い）
GET    /api/archive?state=held|restored|deleted|all&limit=&cursor=
                                                  退避台帳 { "items": [ archived_files の行 + "batch_id" ], "next_cursor", "counts" }
                                                  （新しい順。`state` 省略は held、`limit` は 1..=500・省略 100、`cursor` は
                                                  `id < cursor`。`counts` は状態ごとの件数。復元は batch の巻き戻し。D-106）
GET    /api/logs?kind=all|retro|rip&album_id=&limit=&cursor=
                                                  照合・吸い出しのログの実行ごとの一覧 { "items", "next_cursor" }。`limit` は
                                                  1..=200・省略 50、`cursor` は不透明（offset）。実行は照合なら
                                                  (album_id, source, job_id)、`job_id` が無い吸い出しなら (album_id, source,
                                                  verified_at) でまとめ、key は `retro-<album>-j<job>` / `rip-<album>-t<verified_at>`。
                                                  `log` は latest | superseded | none（照合ログは album ごとに最新 1 回分）
GET    /api/logs/{key}/text?disc=N                ログ本文（text/plain）。root の dirfd から O_NONBLOCK で開き、通常ファイル以外
                                                  （ディレクトリ・FIFO）と ENOTDIR は 404 `log_not_found`、1 MiB 超は 413
                                                  `log_too_large`、key が不正なら 400、`data/verify` を開けなければ 503
                                                  `logs_unavailable`（D-106）
POST   /api/scan                                  {"kind": "incremental" | "deep"}。scan ジョブを投入
GET    /api/gc/preview                            GC の dry-run（区分ごとの件数・バイト数・先頭 50 件、`jobs: { done, failed }` は掃除するジョブ行の数、`plans` は消す終端した計画の数。何も消さない。D-56、D-102）
GET    /api/inbox                                 承認キュー { "items": [{ id, rel_dir, state, detected_at, error, placed_album_id,
                                                  proposal, draft, warnings, destination, tracks: [{ rel_path, codec, lossless,
                                                  sample_rate, bit_depth, channels, duration_ms, tags, source,
                                                  same_title: [{ track_id, rel_path, duration_ms }] }] }],
                                                  watch: { checked_at, poll_interval_secs }, discard_retention_days, ytmusic_enabled,
                                                  subscriptions: [{ id, album, synced_at, moved, renamed, tags_batch_id,
                                                  rename_batch_id }] }   # P4-18 / P4-19 / P4-22 / D-90
                                                  （§7.8、D-68。destination と source は D-70: source は spindle-inbox.json の
                                                  項 { source, url, channel, verdict, message, subscription_id?, position? } | null。
                                                  destination.numbers は宛先の既存の番号 [[disc, track], …]（昇順）。subscriptions は
                                                  取り込みのトラックが参照する購読の直近の同期の要約（last_result の align から。1 回で読む））
                                                  承認済み（approved / placing / placed）の件には stages: [{ key, label, status: done | running |
                                                  todo | na, done, total }]（配置 → RG → 系統 → 端末の段。§12.6、P5-2）
GET    /api/inbox/:id/files/:name                 取り込みの吸い出しファイル本文（`rip*.log` / `disc*.cue` / `disc*.toc` だけ。
                                                  `GET /api/inbox` の各項目の `rip_files` が名前の一覧。D-106）
GET    /api/inbox/:id/artwork/:hash               取り込みのファイルの埋め込み画像（PICTURE の sha256 で実体を照合してから ETag / 304。
                                                  原寸、MIME は sniff、immutable。取り込みに無い / 実体消失 / 不一致は 404。状態非依存。
                                                  セッション必須。§7.8、P4-4）
POST   /api/inbox/scan                            inbox ジョブを投入（202 + job_id。queued / running があれば 409 duplicate）
POST   /api/inbox/:id/approve                     { category, albumartist, album, date, album_gain?, tracks: [{ rel_path, disc_no,
                                                  track_no, title, artist, keep_artists?, tags?, picture? }] }。検証に通らなければ 400、pending / failed 以外は 409 → approved +
                                                  ジョブ投入。件の再判定ジョブが queued / running なら 409 `rejudging`（message 付き。
                                                  YouTube 機能が無効なら見ない。§7.8、D-114）。album_gain は既定 false（D-74）。tags / picture は §7.8（D-86）。
                                                  差し替える画像の artwork 行が無ければ 400
POST   /api/inbox/:id/preview                     { draft }（approve と同じ形）。音声を読まずに配置の計画を引く →
                                                  200 { rel_dir, paths, error }（決められなければ rel_dir は null で error に
                                                  理由）。自分の成果物の再利用は見ないので見込み（D-86）
POST   /api/inbox/:id/rejudge                     pending の件の YouTube 由来の曲を今のプラグインで判定し直すジョブを投入（202 + job_id）。
                                                  `[ytmusic].enabled` でなければ 404 / 件が無ければ 404 / pending でない・YouTube 由来の曲が
                                                  無ければ 409 `state` / 同じ件のジョブがあれば 409 `duplicate` / 503 `inbox_unavailable`（§7.8、D-114）
POST   /api/inbox/:id/reject, /reopen             rejected へ / pending へ戻す（approved / rejected / failed から）。件の再判定ジョブが
                                                  queued / running なら 409 `rejudging`（approve と同じ）
POST   /api/inbox/:id/discard, /undiscard         却下した取り込みを破棄待ちにする / 取り消す（rejected だけ。消すのは GC。D-90）
POST   /api/ytmusic/download                      { urls: [string] }（1 件以上、各 1〜2048 文字）。URL ごとに ytdl ジョブを投入
POST   /api/ytmusic/lookup                        { urls: [string] }（200 件まで）→ { items: [{ url, kind: video|playlist|other|invalid,
                                                   video_url?, located?: { location: library|inbox, path }, list_id?, subscription?: { id, albumartist, album } }] }
                                                   （YouTube 画面 ① の照合。DB だけ。`list=` 付きは再生リスト。D-87）
POST   /api/ytmusic/playlist                      { url }（再生リスト）→ { list_id, title, entries, unavailable, in_library, in_inbox, new, truncated, subscription? }
                                                   | 400 | 502 ytdlp_failed（yt-dlp で列挙して所在ごとに数える。読むだけ。同時 2 本。D-87）
GET    /api/ytmusic/subscriptions                 → { items: [Subscription] }（albumartist / album 順。P4-16、D-78）
POST   /api/ytmusic/subscriptions                 { url, albumartist, album, category?, align? = true, enabled? = true, max_enqueue? = 50 }
                                                  → 201 Subscription | 400（YouTube の list= 付き URL でない、空、max_enqueue が 1〜1000 外）
                                                  | 409 { error: "duplicate_list" | "duplicate_target" }
PATCH  /api/ytmusic/subscriptions/:id             { albumartist?, album?, category? (null で消す), align?, enabled?, max_enqueue? }
                                                  → 200 Subscription | 404 | 409 { error: "duplicate_target" | "sync_running" }
                                                  追記先を変えると album_id は NULL に戻る。同期が queued / running の間は sync_running
DELETE /api/ytmusic/subscriptions/:id             → 204 | 404 | 409 sync_running（PATCH と同じ。Library のファイルは消えない）
POST   /api/ytmusic/subscriptions/:id/sync        → 202 { job_id } | 404 | 409 { error: "duplicate" }（走行中。latch は残るので終わった後に走る）
GET    /api/ytmusic/subscriptions/:id/progress    → { sync: Job | null, items: [Job] } | 404（直近の playlist_sync と、それより後に作られた（jobs.id が大きい）その購読の ytdl を position 昇順で。created_at は秒単位で前回の同期の終わった件と同じ秒になりうるので、境界は id で取る。投入が走行中の古い ytdl に dedup されて新しい行ができない同期もあるので、この購読の ytdl で queued / running のものは作られた順によらず含める。スマホ版の購読の「完了」の判定に使う。§12.7）
                                                  [ytmusic].enabled でなければすべて 404
// Subscription = { id, list_id, url, album_id, albumartist, album, category, align, enabled, max_enqueue,
//                  created_at, updated_at, last_attempted_at, last_synced_at, sync_requested_at,
//                  last_result: { state, error?, synced_at, title?, entries, playlist_count?, album_id?, in_library, in_inbox,
//                                 elsewhere: [{position, id, track_id, rel_path}], enqueued: [position], running: [position],
//                                 deferred, unavailable: [{position, id, kind}], align?: { moved, unchanged, blocked: [{track_id,
//                                 position, current_no, reason}], outsiders, unnumbered, tags?, renamed, rename? } } | null }
                                                  → 202 { job_ids }。`[ytmusic].enabled` でなければ 404（§7.7、D-70）
POST   /api/gc                                    gc ジョブを投入（未完了があれば 409）
                                                  （202 + job_id。queued / running があれば 409 duplicate）
GET    /api/events                                SSE: ジョブ進捗・ライブラリ変更

GET    /api/history, POST /api/history/:batch/revert
                                                  巻き戻しは新バッチとして記録（§7.5）。
                                                  終端状態のバッチのみ。pending 衝突・二重 revert は 409
POST   /api/history/:batch/cancel                 反映中バッチのキャンセル
```

- 一括編集は必ず preview → apply の 2 段階
- SSE は単一チャネル。イベント種別で多重化

## レスポンス形（UI が依存するもの）

```jsonc
// GET /api/tracks?filter=...&sort=title&cursor=...&limit=100
//   filter はホワイトリストのキーだけを持つ JSON（未知キーは 400。D-39）:
//   { "category": "J-Pop",            // ツリー: categories.name
//     "albumartist": "…",              // ツリー: tracks.albumartist の完全一致
//     "album_id": 12,                  // ツリー
//     "playlist_id": 3,                // プレイリスト所属
//     "flags": ["missing", "pending"], // 固定フィルタ（AND）: unverified | duplicate | missing |
//                                      //   no_rg | rg_unwritten | pending | conflict | hardlink |
//                                      //   flac_unchecked | flac_error（§7.9）| hires_unchecked | hires_suspect（§7.10）
//     "q": "情緒",                     // 検索語（3 文字以上 FTS5 / 未満 LIKE）
//     "device_pending": 2 }            // 端末に未反映（端末 id。追加・更新・移動・更新 + 移動で、前回の失敗も含む。DSL の device_pending と同じ定義。§7.11）
//   cursor は前ページの next_cursor をそのまま返す不透明文字列（キーセット）。sort が変わったら
//   捨てる（別ソートで発行したカーソルは 400）。
//   total はフィルタに一致する全件数（同じ読み取りスナップショットで数える）
{ "items": [ { "id": 1, "title": "...", "artist_display": "...", "album": "...", "albumartist": "...",
               "track_no": 1, "disc_no": 1, "date": "2024", "category": "J-Pop",
               "duration_ms": 280000, "codec": "flac", "lossless": true,
               "verification": "verified_ctdb", "rg_scanned_at": 1, "rg_written_at": 1,
               "derived": { "opus": { "codec": "opus", "stale_tags": false }, "aac": null },
                                                                       // 系統ごと。無ければ null（§7.6）
               "flac_check": { "status": "ok", "checked_at": 1700000000, "stale": false, "error": null },
                                                                       // 未検査なら null（§7.9）
               "hires_check": { "status": "upsampled", "checked_at": 1700000000, "stale": false,
                                "error": null, "cutoff_hz": 22050, "cliff_db": 48.3, "effective_bits": 16 },
                                                                       // 対象外・未検査なら null（§7.10）
               "pending_batch_id": 42,                                // または null
               "conflict_batch_id": 41,                               // または null
               "duplicate_group": "a1b2…",                            // audio_md5 hex または null
               "hardlink": false, "missing_since": null,
               "rel_path": "J-Pop/…/01 ….flac",
               "devices": [ { "device_id": 2, "state": "synced", "synced_at": 1700000000 },
                            { "device_id": 3, "state": "pending", "op": "add", "reason": null } ] } ],
                 // devices は端末ごとの状態（P5-2。その曲が端末の対象外で端末にも無いときは、その端末の要素を省き、端末が 1 台も無ければキーごと
                 // 省く）。state = synced { synced_at } | pending { op: add | update | move | update_move, reason }
                 // | waiting { reason, has_copy } | error { reason, has_copy } | removing。バッジ列とプロパティの「端末」が使う
  "next_cursor": "…", "total": 61234 }

// 選択は 2 形。Ctrl+A はフィルタ形で送る（ID 列挙にしない）
// { "ids": [1, 2, 3] }  または  { "filter": "<選択時点のフィルタ式>", "exclude_ids": [7] }
// フィルタ形は「選択した時点のフィルタ」を immutable に保持する（表示中のフィルタとは別物）

// POST /api/tracks/batch/preview   { "selection": {...}, "ops": [...], "sort": "title" }
//   サーバは selection を解決して対象 track_id と各行の tag_version / 事前条件を
//   スナップショットに保存し、selection_token（TTL 15 分）を返す（D-33）。
//   ops は上から順に適用する操作の配列（D-42）:
//     { "op": "set",     "key": "TITLE",       "value": "…" | ["…", "…"] }   // 空は削除
//     { "op": "ref",     "key": "ALBUMARTIST", "template": "%artist%" }      // 先頭値で展開
//     { "op": "replace", "key": "TITLE",       "pattern": "…", "replacement": "$1" }
//     { "op": "number",  "key": "TRACKNUMBER", "start": 1, "pad": 0 }        // sort 順に連番
//     { "op": "delete",  "key": "COMMENT" }
//     { "op": "set_rows","key": "SOURCE_URL", "rows": { "<track_id>": "…" | ["…"] } }
//         行ごとに違う値（API 専用。UI には出さない。rows に無い行は変更なし。1〜10,000 行。P4-14）
//   items は値が変わる行だけ。反映待ちの行は評価せず pending_excluded に数える
{ "selection_token": "…", "count": 1207,
  "changed": 1180, "unchanged": 24, "pending_excluded": 3,
  "items": [ { "id": 1, "changes": { "TITLE": { "old": ["…"], "new": ["…"] } } } ] }

// PATCH /api/tracks/batch   { "selection_token", "ops", "description", "skip_pending": false }
//   対象は token のスナップショット集合だけ。preview 後にスキャンで増えた行は含まれない。
//   スナップショット時と tag_version が変わった行は skipped_conflict になる
//   201 { "batch_id": 42, "affected": 1180 }
//   409 { "error": "pending", "track_ids": [ … ], "count": 3 }   // skip_pending=true で除外して続行
//   409 { "error": "preview_stale" }                             // token 期限切れ・ops 不一致
//   409 { "error": "no_changes" }                                // 値が変わる行が無い
//   409 の応答では token を消費しない（同じ token でやり直せる）。201 で消費する

// POST /api/rename/preview   { "selection": {...}, "sort"? }
//   selection を解決して固定し selection_token を返す（D-33）。各行に [layout] のテンプレートを
//   適用した宛先を返す。items は宛先が変わる行と衝突した行だけ（変更なし・反映待ちは件数）
{ "selection_token": "…", "count": 980,
  "changed": 975, "unchanged": 2, "conflict": 2, "pending_excluded": 1,
  "items": [ { "id": 1, "old": "old/a.flac", "new": "J-Pop/花譜/魔法/03 過去を喰らう.flac" },
             { "id": 2, "old": "old/b.flac", "new": null, "reason": "同名の別リリースと衝突（…）" } ] }

// POST /api/rename/apply   { "selection_token", "description"?, "skip_pending": false }
//   token の集合で計画を取り直してバッチを記録する。衝突した行と preview の後にタグが変わった行は
//   skipped_conflict の op として記録だけする（affected に含む）
//   201 { "batch_id": 43, "affected": 977, "conflict": 2 }
//   409 { "error": "pending" | "preview_stale" | "no_changes" }   // 規則は PATCH /api/tracks/batch と同じ

// POST /api/normalize/preview   { "selection": {...}, "sort"? }
//   rename と同型。items は宛先が決まる行（old / codec / new）と衝突した行（new: null, reason）。
//   既に FLAC・非可逆は unchanged に数える
{ "selection_token": "…", "count": 7572,
  "changed": 7570, "unchanged": 0, "conflict": 2, "pending_excluded": 0,
  "items": [ { "id": 1, "old": "J-Pop/…/01 ….m4a", "codec": "alac", "new": "J-Pop/…/01 ….flac" } ] }
// POST /api/normalize/apply   { "selection_token", "description"?, "skip_pending": false }
//   201 { "batch_id": 44, "affected": 7572, "conflict": 2 }
//   409 { "error": "pending" | "preview_stale" | "no_changes" | "normalize_disabled" }

// GET /api/history
{ "items": [ { "id": 42, "created_at": 1, "description": "…", "kind": "tags",
               "state": "partial", "affected": 312, "applied": 309, "conflict": 3, "failed": 0,
               "reverts_batch_id": null, "reverted_by": 39, "finished_at": 1 } ] }
//   reverted_by は reverted_at が立っているときだけ（戻した逆バッチの id）。reverted_at も返す
// GET /api/history/:id  → 上 + "ops": [ { "id", "track_id", "kind", "result", "error", "rel_path",
//                                        "edits": { "TITLE": { "old": [...], "new": [...] } },
//                                        "current": { "TITLE": [...] } } ]   // skipped_conflict のときだけ
//                                        （編集キーの現在値 = ファイルの再読込結果。rename は rel_path）
// POST /api/history/:id/revert { "description"? }  → 201 { "batch_id": 43, "affected": 50, "conflict": 1 }
//                               → 404 | 409 { "error": "not_terminal" | "already_reverted" | "pending" }
// POST /api/history/:id/cancel  → 202 | 404 | 409 { "error": "not_cancellable" }

// GET /api/jobs?type=          // type（任意）: 種別で items を絞る（`ytdl` 等。上限も種別内で数える。YouTube 画面）。
                                //   summary / by_type は全件のまま。不明な種別は 400
{ "items": [ { "id", "type", "state", "progress", "done", "total", "attempts", "last_error",
               "run_after", "edit_batch_id", "created_at", "started_at", "finished_at",
               "note",          // 完了時の結果 1 行（ハンドラが返す。ytdl: "Inbox に置いた: <path>" /
                                //   "プラグインが skip: <理由>" / "取り込み済み（Library|Inbox）: <パス>" /
                                //   "再生リストを展開した: N 件を投入、M 件は取り込み済み"。
                                //   verify: 手法ごとの結果（"CTDB 全 12 曲一致（信頼度 34） / AccurateRip
                                //   12 曲中 10 曲一致"、複数ディスクは "disc N: " 付きで「。」区切り。D-96）。
                                //   playlist_sync: 同期の要約。無ければ null）
               "subject" } ],   // subject = 対象の表示用文字列（track_id → Library のパス、album_id → ディレクトリ、
                                //   batch_id → 説明、transcode は " [<variant>]" 付き、scan は kind、ytdl は url、
                                //   thumbnail は "artwork #id"、端末ジョブは "device #id"（source_hash は track_id と同じ）。
                                //   行が消えていれば "track #id"、対象の無い種別は null）
                 // 状態ごとに上限付き（`limits`）: 実行中・待ち 1,000（実行中 → 待ち。取り出し順 = priority 降順・
                 // 作成順・id 昇順）→ 完了 300（新しい順）→ 失敗・取り消し 300（新しい順）。1 本の並びで切ると
                 // 待ちが数千件のとき完了・失敗が届かない
  "summary": { "running": 3, "queued": 12, "done": 15300, "failed": 0, "cancelled": 0, "pending_ops": 1204 },
  "by_type": { "transcode": { "queued": 6470, "running": 11, "done": 1089, "failed": 0, "cancelled": 0 }, … },
                 // 種別ごとの全件集計（0 件の種別は無し）
  "concurrency": { "scan": 1, "rg": 12, "transcode": 11, … },   // 種別ごとの並列度（§8）
  "cpu_budget": 12,                                               // CPU 系の共通予算（= コア数。D-73）
  "limits": { "active": 1000, "done": 300, "failed": 300 } }     // items の状態ごとの上限
// POST /api/jobs/:id/cancel  → 202（queued は即 cancelled、running は cancel_requested_at を立てる）
//                            → 404 | 409 { "error": "not_cancellable" }   // 既に終端
// POST /api/jobs/:id/retry   → 202（failed / cancelled を attempts=0 で queued に戻す）
//                            → 404 | 409 { "error": "not_retryable" | "duplicate" }   // D-36
// DELETE /api/jobs/:id       → 204（failed / done / cancelled の行を消す。P4-18）
//                            → 404 | 409 { "error": "not_terminal" }
// DELETE /api/jobs?state=failed → 200 { "deleted": N }。state=failed 以外は 400 { "error": "invalid_state" }

// SSE /api/events   event 種別: job | batch | library | resync | device_progress
//   job:     { "id", "state", "progress", "done", "total" }
//   batch:   { "id", "state", "applied", "conflict", "failed" }
//   library: { "scan_run_id", "kind": "ids", "track_ids": [ … ] }   // 変更が 200 行以下
//            { "scan_run_id", "kind": "bulk" }                     // それ以上。ページを無効化
//            scan の完了時に 1 回（commit 後）。変更行 0 件なら流さない（D-39）
//   device_progress: { "device_id", "progress": SyncProgress | null }   // agent 端末の同期の進捗（null は消去: 計画が閉じた）
//   resync:  { "skipped": n }   // サーバ側で取りこぼした。一覧（jobs / history / 表示ページ）を再取得
//   クライアントは SSE を開いてから一覧を取得する（逆順だと開く前のイベントを失う。D-36）
```

`library` イベントを受けたクライアントは、**表示中のページ（クエリ + カーソル）を無効化して
再取得**する。行の差し替えだけでは、変更行がフィルタに出入りしたりソートキーが変わったり
カーソル境界を跨いだときに集合と順序が壊れる。`ids` のときは表示中に含まれる id があれば
再取得、`bulk` は無条件に再取得。選択（`selection`）はイベントで変えない。

`pending_batch_id` / `conflict_batch_id` / `duplicate_group` / `hardlink` は一覧の
バッジ列（[§12.2](12-ui/02-track-list.md)）が直接使う。`GET /api/tracks` は 1 クエリで返す: `edit_ops` の pending と
最新 op は LEFT JOIN、重複は `audio_md5` 索引への相関 EXISTS で行ごとに引く
（`duplicate_groups` ビューを JOIN すると毎回 GROUP BY の実体化が走る。[D-39](../decisions/D-039.md)）。

// GET /api/albums
{ "items": [ { "id": 1, "rel_dir": "J-Pop/…/…", "category": "J-Pop", "albumartist": "…", "album": "…",
               "date": "2024", "original_date": null, "edition": null, "mb_release_id": null,
               "disc_count": null, "artwork_id": null, "artwork_hash": null,   // SHA-256 hex
               "track_count": 12, "duration_ms": 2800000, "missing_since": null } ] }
// GET /api/albums/:id  → 上の 1 要素 | 404

```jsonc
// GET /api/devices → { "items": [Device] }。POST / PATCH / PUT playlists / 破棄も Device を返す（端末タブ。§12.6）
{ "id": 2, "name": "Xperia", "transport": "adb", "variant": "opus", "selection": "playlists",
  "generation": 5,
  "connected": true,             // adb: 監視が見た状態が device か（adb 無効なら false）。agent は null
  "adb_state": "device",         // adb の生の状態（unauthorized / offline …）。見えていない・agent は null
  "adb_volume": "E1C6-6113", "adb_root": "Music/spindle",   // agent は null
  "counts": { "add": 3, "update": 0, "move": 1, "delete": 0, "waiting": 12, "error": 0, "synced": 210 },
                                 // 曲ごとの状態の件数。更新 + 移動は move に数える。ナビの黄色い件数は
                                 // add + update + move + delete + error の全端末の合計（待ちは含めない）
  "last_synced_at": 1700000000,  // adb は同期の完了、agent は報告の受理
  "playlist_ids": [1, 4],        // 印（device_playlists）
  "playlist_states": [           // プレイリストごとの同期の状況（表示用のスナップショットから。少し古いことがある）
    { "playlist_id": 1, "state": "synced", "pending": 0, "waiting": 0, "errors": 0, "reason": null },
    { "playlist_id": 4, "state": "pending", "pending": 3, "waiting": 1, "errors": 0, "reason": null } ],
                                 // 選曲の各プレイリストと、外したが端末に残るもの（removing）。state は
                                 // error（送れない収録曲があるか、ファイル自体のエラー。reason）> pending（次の同期で
                                 // 送る収録曲・準備待ちの収録曲・ファイルの追加か更新のどれかがある）> synced の順に
                                 // 決める。pending は送る曲、waiting は準備待ち（Derived 未生成など）、errors は送れない
                                 // 曲の数（ライブラリから消えた曲は数えない）
  "open_plan": false,            // open な計画か待ち・実行中の device_sync がある（設定を変えられない）
  "plan_open": false,            // open な計画がある（「続きを実行」「破棄」の対象）
  "sync_job": null,             // 待ち・実行中の device_sync { id, state } か null
  "sync_progress": null }        // agent の同期の進捗か null。open な計画の plan_id と一致するものだけ（下記）

// SyncProgress: { "plan_id", "done", "total", "bytes_done", "bytes_total", "rate", "phase", "current",
//                 "updated_at", "age" }   // age = サーバが受けてからの秒数（画面は端末の時計に頼らない）

// GET /api/devices/:id/diff
{ "generation": 5, "plan_token": "…",          // 同期（adb）と計画の確定（agent）に添える
  "pending_reevaluation": false,               // 印の付いたスマートの評価待ち（確定は 409）
  "items": [ { "op": "add", "track_id": 1, "title": "…", "artist": "…", "from": null,
               "dest_path": "J-Pop/…/1-01 群青.opus", "reason": null, "size": 8123456, "has_copy": false } ],
             // op = add | update | move | update_move | delete | adopted | waiting | error。waiting / error は hold
             // （reason に理由、has_copy は既存の写しが残るか）と、反映済みで前回の同期・報告が失敗した曲。
             // adopted = 引き継いだ写しの照合（トークンの書き換えだけで転送しない。estimate に含めない）
  "playlists": [ { "op": "update", "playlist_id": 4, "name": "通勤", "dest_path": "Playlists/通勤.m3u8",
                   "reason": null } ],         // op = add | update | delete | error
  "estimate": { "transfer_bytes": 25000000, "peak_bytes": 24000000, "free": 51200000000 },
             // 送る量、実行順に見た「今より増える量」の最大値、端末の空き（adb が最後に測った値。無ければ null）
  "evaluations": [ { "playlist_id": 4, "name": "通勤", "evaluated_at": 1700000000, "pending": false } ],
             // 印の付いたスマートプレイリストの評価時刻
  "counts": { … } }                            // Device の counts と同じ
```

## 認証

LAN 限定でも必須とする。攻撃者対策というより事故対策で、この API は 300GB の
ライブラリをリネーム・削除・タグ上書きできる。認証がないと、別タブのスクリプト、
家族の端末、うっかり有効にしたポートフォワードがそのまま破壊的操作に届く。

- 単一パスワード。argon2id でハッシュして DB に保存
- **初期パスワードは環境変数 `SPINDLE_INITIAL_PASSWORD`** で与える（[D-28](../decisions/D-028.md)）。DB に
  パスワードが無い初回起動でハッシュ化して保存し、以後は無視する。環境変数も DB も
  無い場合は**ロックモード**で起動し、SPA の配信を含めて `/health` 以外は 503 で
  短い JSON（設定手順）を返す。`/health` は `{"status":"locked"}` を返して機械可読にする。
  「先着で設定できる初回セットアップ画面」は置かない。compose のサンプルは
  `${SPINDLE_INITIAL_PASSWORD:?...}` 形式で、値を与えないと起動できないようにする
- セッション Cookie（HttpOnly / SameSite=Lax / TLS 時のみ Secure）。Cookie 値はランダム
  32 バイトで、DB には **SHA-256 のみ**保存（`sessions.token_hash`）。失敗ログインは
  IP 単位でレート制限
- **変更系リクエスト（POST / PATCH / DELETE）の CSRF 検証**: `Origin` ヘッダが**あれば**
  scheme / host / port が自分自身と完全一致することを必須にする。`Origin` が無い場合だけ
  `Sec-Fetch-Site` が `same-origin` / `none` であることを要求し、それも無ければ拒否する。
  **`Host` の一致は判定に使わない**（クロスサイト POST でも Host は送信先になるため防御に
  ならない）。reverse proxy 越しの外部 origin は `trusted_proxies` からの `X-Forwarded-Host` /
  `X-Forwarded-Proto` だけで構成する
- CORS は全面禁止（SPA は同一オリジンで配信するため不要）
- `trusted_cidrs` からの接続は **route allowlist だけ認証をスキップ**する（[D-27](../decisions/D-027.md)）:
  `GET /api/stream/:id`、`GET /api/artwork/:hash`、`GET /api/tracks/:id`（限定フィールド）、
  `GET /api/playlists/:id/export`。それ以外（一覧・検索・SSE・history・jobs・設定・session）は
  CIDR 内でもセッション必須。変更系は当然セッション必須。用途は他プレイヤーや curl からの
  ストリーム参照であり、履歴やジョブのエラー文（パスを含む）を LAN 全体に見せる理由はない。
  判定に使うのは接続元 socket のアドレスで、`X-Forwarded-For` は `trusted_proxies` に列挙した
  proxy からのものだけ採用する
- `/health` と `/api/agent/*` 以外の全ルート（SSE / stream / artwork 含む）が同じ認証ミドルウェアを通る
- **エージェントの経路**（`/api/agent/*`。Mac の `spindle-agent`、[D-99](../decisions/D-099.md)）: セッションの認証ミドルウェアの
  **外**に置き、`Authorization: Bearer <selector>.<secret>` だけで通す専用のミドルウェアを載せる。
  Cookie・CSRF・`trusted_cidrs` はどれも見ず、逆にセッション側は Bearer では通らない（同じ経路に分岐を
  足すと、どちらかの検査が緩む事故が起きやすい）。セレクタで端末の行を 1 つ引き、シークレットの SHA-256 を
  定数時間で比べる（256 bit の乱数なので argon2id は要らない）。外れれば 401 `unauthenticated` と
  `WWW-Authenticate: Bearer`。トークンは端末タブの「pair コードを発行」（`POST /api/devices/:id/pair-code`。
  セッション必須）で出したワンタイムコード（`<selector>.<secret>`、10 分・1 回限り、argon2id で保存、
  試行は検証の前に予約して 5 回まで）を `POST /api/agent/pair` で引き換えて受け取る。pair だけは Bearer を
  要らない明示的な例外。コードを発行し直すと旧トークンは即失効する。ロックモードでは pair を含めて 503。
  トークンとコードはログに出さない
- Subsonic 非対応が確定したため、salt+md5 方式との併存を考慮する必要はない

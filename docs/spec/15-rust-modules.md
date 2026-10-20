## 15. Rust モジュール構成

```
src/
├── main.rs
├── config.rs            config.toml の読み込みと検証（D-34）
├── logging.rs           tracing の初期化
├── db/
│   ├── mod.rs           コネクション管理（write 単一 / read プール）
│   ├── migrations.rs    リポジトリ直下 db/migrations/*.sql の埋め込みと適用
│   ├── tracks.rs        一覧（キーセット）・検索・selection 解決・アルバム一覧（D-39）
│   ├── archive.rs       archived_files 台帳（退避・復元・GC の根拠）
│   ├── categories.rs    統制語彙（canonical key で一意）
│   ├── inbox.rs         inbox_items / inbox_files（承認キューの状態機械。D-68）
│   ├── subscriptions.rs playlist_subscriptions（再生リストの購読。追記先の CAS 束ねと同期の latch。D-78）
│   ├── devices.rs       端末の表（§6）: マニフェストの入力の読み取りと全端末のスナップショット、計画の確定・終端、
│   │                    キャッシュの全置換、pair コードとトークン、source_hash の投入（§7.11、D-95 / D-99）
│   ├── stages.rs        Inbox の承認済みの件の段（配置 → RG → 系統 → 端末。§12.6）
│   ├── playlists.rs  jobs.rs  history.rs
├── domain/
│   ├── identity.rs      inode / audio_md5 による同一性解決
│   ├── filter.rs        一覧のフィルタ（JSON）/ ソート / カーソルの検証
│   ├── selection.rs     selection の 2 形、preview のスナップショット store（D-33）
│   ├── pathgen.rs       テンプレート展開・正規化・衝突回避
│   ├── tags.rs          lofty ラッパ、正規化、多値処理、Derived への書き出し（Opus の VorbisComments / MP4 の ilst + フリーフォーム）
│   ├── replaygain.rs    ebur128、フォーマット別変換
│   ├── device.rs        端末への配信の純粋関数（選曲・送る元・パスの検査と衝突・desired / hold / remove・トークン・
│   │                    差分・plan_token・曲ごとの状態・見積もり。§7.11、D-95）
│   └── category.rs      統制語彙、GENRE 写像
├── edit/
│   ├── mod.rs           編集バッチの coordinator（記録・DB 先行更新・反映・overlay 解消・
│   │                    キャンセル・起動時リカバリ。D-24 / D-41）
│   ├── rename.rs        一括リネームの計画・記録・2 phase 反映・album の追随（D-43）
│   ├── normalize.rs     ロスレス → FLAC 正規化の計画・記録・反映・Archive 退避（D-46）
│   ├── md5fill.rs       FLAC の MD5 補填（md5 op。D-59）
│   ├── picture.rs       埋め込み画像の差し替え（`PICTURE` の tags op。画像の読み出し・退避。D-60）
│   └── revert.rs        巻き戻し（対象集合・現在値の比較・逆バッチの記録。D-44）
├── media/
│   ├── fingerprint.rs   STREAMINFO MD5 / デコード PCM MD5 / パケット列ハッシュ
│   ├── decode.rs        symphonia / ffmpeg フォールバック
│   ├── hires.rs         偽ハイレゾ検出の解析（PcmSink: FFT の累積とサンプル OR → 計測値（cutoff / cliff / 実効ビット）→ 判定。§7.10、D-71）
│   ├── encode.rs        flac（ffmpeg デコード → flac -8）/ opus（ffmpeg デコード → opusenc）/ aac（ffmpeg 1 パス。RG 焼き込み・48 kHz 上限）
│   └── artwork.rs       同梱 / 埋め込み画像の選択、判別、ハッシュアドレスのキャッシュ（P1-3）
├── cd/
│   ├── mod.rs           TrackLayout（サンプル単位のトラック列）、照会用 HTTP クライアント
│   ├── device.rs        ioctl（CDROM_DRIVE_STATUS / READ TOC / LOCKDOOR + EJECT）、SG_IO READ SUB-CHANNEL（ISRC / MCN）、
│   │                    Drive トレイト、DriveMonitor とポーラ
│   ├── toc.rs           TOC の検証、各種 DiscID 算出、サンプル数からの再構成（§7.3）
│   ├── rip.rs           cd-paranoia、オフセット、分割
│   ├── accuraterip.rs   ARv1/v2 CRC、DB の照会（dBAR-*.bin）、オフセット表
│   ├── ctdb.rs          CRC32、照会（lookup2.php）、パリティファイルの取得
│   ├── repair.rs        CTDB の修復（GF(2^16) RS: シンドローム表、オフセット探索、復号、適用。D-66）
│   ├── crctable.rs      1 回流して任意オフセットのトラック CRC を出す表（累積和と CRC32 combine）
│   ├── verify.rs        DB の応答との照合（オフセット探索、トラックごとの一致、`tracks.verification` の写像）
│   ├── musicbrainz.rs   ws/2/discid の照会（UA、1 req/s、503 の再試行）と候補（リリース × medium）
│   ├── metadata.rs      確定フォームの DiscMetadata（検証、タグ写像 + TRACKTOTAL / MUSICBRAINZ_DISCID。D-65 / D-67）
│   ├── riplog.rs        RipReport と rip.log / disc.cue / disc.toc の描画、同梱ファイルの名前（D-67）
│   └── place.rs         配置（分割 MD5 → flac → pathgen::plan → library 排他 → tmp + RENAME_NOREPLACE →
│                        1 トランザクション登録 → rg / transcode。MD5 で冪等。D-67）
├── import/
│   ├── scanner.rs
│   ├── placement.rs     CD / Inbox 共通の配置（tmp + RENAME_NOREPLACE、album の解決と登録トランザクションでの
│   │                    リリースキー再検証、行の登録 / 採用。D-67 / D-68）
│   ├── inbox.rs         Inbox の走査（取り込み = ディレクトリ）、タグからの下書き、承認の検証、承認済みの配置
│   │                    （補正をタグに書いて pathgen::plan の宛先へ。source_type = download。D-68）
│   └── ytmusic/         metadata.rs（メタデータプラグインのプロトコル v1 と呼び出し。D-69）、
│                        downloader.rs（yt-dlp の dump / download、remux、タグ、Archive、Inbox への配置と
│                        spindle-inbox.json。D-70）、sidecar.rs（spindle-inbox.json の読み書き。Inbox と共有）、
│                        playlist.rs（再生リストの列挙の解釈と番号揃えの計画。純粋。D-78）
├── device/              端末への配信の実行側（§7.11。判定は domain::device、キャッシュは db::devices）
│   ├── remote.rs        DeviceFs（端末の root 配下の操作。試験は偽の端末 FS）
│   ├── adb.rs           adb による DeviceFs（shell v2 だけ、引数配列。ボリュームの探索）
│   ├── quote.rs         端末側のシェルへ渡すパスの検査と単引用符クォート（1 か所だけ）
│   ├── ondevice.rs  journal.rs  store.rs
│   │                    端末側の正本（manifest.json）・ジャーナル（NDJSON）・耐久化の順序
│   ├── recover.rs       接続のたびの回復（DB は見ない）
│   ├── plan.rs          確定した計画と、開始済みの計画の再開（実行する部分集合）
│   ├── sync.rs          計画の実行（削除 → パス変更のバッチ → 更新 → 追加 → プレイリスト）
│   ├── verify.rs        内容の検証（全曲の sha256sum）
│   ├── track.rs  runtime.rs
│   │                    `adb track-devices` の解析と常駐監視、端末ごとのロック・空き容量（D-98）
│   ├── credential.rs    エージェントのワンタイムコードとトークン（`<selector>.<secret>`。D-99）
│   └── report.rs        エージェントの報告の検証と正準ダイジェスト（D-99）
├── jobs/
│   ├── queue.rs  worker.rs  recovery.rs  scheduler.rs（backup / gc の周期投入。inbox は handlers/inbox.rs、
│   │                        購読の dispatcher は handlers/playlist_sync.rs）
│   └── handlers/        種別ごと（playlist_sync.rs = 購読の同期: 列挙 → 追記先 → 揃え → 投入。D-78。
│                        device.rs = device_scan / device_sync / device_verify、source_hash.rs。§7.11）
├── gc/
│   └── mod.rs           物理削除の唯一の経路。plan（判定・dry-run）と execute_*（6 区分。D-56 / D-90）
├── playlist/
│   ├── dsl.rs           pest 文法 → AST
│   ├── compile.rs       AST → パラメータ化 SQL
│   ├── fb2k.rs          AST → foobar クエリ + ソートパターン
│   └── export.rs        m3u8 / pls / パスマッピング
├── api/
│   ├── mod.rs  tracks.rs  albums.rs  categories.rs  selection.rs  batch.rs  rename.rs  normalize.rs
│   │   history.rs  stream.rs  cd.rs  inbox.rs  events.rs  ytmusic.rs  subscriptions.rs
│   ├── auth.rs          argon2id / セッション Cookie / CSRF / trusted_cidrs のミドルウェア
│   ├── devices.rs       端末 API（UI 向け。`/api/devices*`）
│   ├── agent.rs         エージェント API（`/api/agent/*`）と Bearer のミドルウェア（auth::guard の外。D-99）
│   ├── state.rs  error.rs   AppState、`{ "error": code }` 応答
└── web/                 SPA を rust-embed で同梱

agent-proto/             ワークスペースのクレート。本体とエージェントが共有する `/api/agent/*` のワイヤ型
                         （serde だけに依存する。D-99）
agent/                   ワークスペースのクレート `spindle-agent`（Mac の CLI。§7.11、D-100 / D-101）。本体には依存しない
├── src/main.rs          CLI（pair / sync / status / resolve / abandon）
├── src/sync.rs          各コマンドの段取り（state のディレクトリの lock を取り、state.json を読んでエンジンを呼ぶ）
├── src/pair.rs          pair と冪等な初期化（`.spindle-device`、「spindle」フォルダ）
├── src/adopt.rs         `pair --adopt`（DB 全損後の引き継ぎ）と、途中で落ちたときの続き（D-104）
├── src/state.rs  ctx.rs state.json（エージェントの実状態の正本）と耐久化
├── src/recover.rs  rediscover.rs  plan.rs  exec.rs  batch.rs  playlist.rs
│                        回復・再発見・再開の照合・実行・パス変更のバッチ・プレイリストの入れ替え
├── src/local.rs  pathkey.rs   ローカルの root（`~/Music/spindle`）と canonical_key
├── src/failpoint.rs     試験の中断点（state の保存・ミュージック.app の操作・配置の前後で落とす。本番は何もしない）
├── src/music.rs + music/ ミュージック.app の trait と JXA 実装（`osascript -l JavaScript`）・偽実装
├── src/music/music.js   バイナリに埋め込む JXA（要求は argv の JSON、応答は 1 行の JSON）
├── src/server.rs + server/   `/api/agent/*` のクライアント（reqwest blocking）・偽実装
├── src/secrets.rs + secrets/ トークンの保管（Keychain / ファイル）
├── tests/               エンジンの試験（偽のミュージック.app・サーバ、中断点からの回復。実機の JXA・Keychain は #[ignore]）
└── README.md            導入・前提・困ったとき
```

---


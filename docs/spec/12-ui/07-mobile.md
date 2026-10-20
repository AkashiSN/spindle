# 12.7 スマホ版（`/m/`、D-115）

外出先で眺める・探す・試し聴きする・誤字を直す・プレイリストを手直しする・購読を同期する、ための別ページ。
一括編集・巻き戻し・CD・Inbox の承認・端末同期・ジョブ・設定は PC で行う（スマホ版には置かない）。

- **PC 版に手を入れない**: コードは `web/src/mobile/`、ビルドは PC と別の vite 設定（`web/vite.mobile.config.ts`）で
  `web/dist/m/` に出す（`npm run build` が PC のビルドの後に実行）。入口を足すと共通チャンクが変わり PC の JS が
  変わるため。`web/src/components/` `hooks/` `App.tsx` `main.tsx` `index.css` は import も編集もしない。`api/` と `lib/` は
  編集せず import だけする。`dist/m/` 以外の出力が変更前と同一であることを `scripts/pc-dist-hash.sh` で確かめる。
  配色はライト / ダークとも PC の `index.css` と同じ値を持ち、テーマの保存値は PC と共有する。指で押す要素は 44px 以上
- **入口と振り分け**（`src/api/spa.rs`）: `/` を `User-Agent` に `Mobi` / `Android` / `iPhone` を含むときだけ `/m/` へ 302 で
  飛ばす（iPad は Mac の UA を名乗るので PC 版）。スマホ版のメニューの「PC 版を開く」は Cookie `spindle_view=pc` を立てて
  `/` を開き、以後は振り分けない。PC 版からの戻り導線は PC を触らないと作れないので `/m/` を直接開く。`/m/` を開くと
  Cookie を消す。`/m/` 以下の未知のパスは mobile の HTML を返す。認証と CSRF は PC と同じ（未ログインはスマホ版のログイン画面）。
  302 は UA と Cookie で応答が変わるので `Cache-Control: no-store` と `Vary: User-Agent, Cookie` を付ける（間の
  キャッシュが PC のブラウザへ 302 を返さないように。PC 向けの 200 には付けない）。`/m` は `/m/` へ 301（相対の assets を
  解決させるため）。拡張子のあるパスは静的ファイルの要求なので、無ければ HTML に倒さず 404。Cookie は
  `spindle_view=pc; path=/; max-age=31536000; samesite=lax`（1 年）、消すときは同じ属性で `max-age=0`
- **画面の位置は URL で持つ**（`mobile/route.ts`。`history.pushState` と `popstate`）: `/m/library/{artists|albums|tracks}?albumartist=&album=&q=`、
  `/m/folders/{category}/{albumartist}/{album}`（各段は `encodeURIComponent`）、`/m/playlists/{id}?edit={rule|text}`、
  `/m/subscriptions/{id}`。知らないパスはライブラリ。曲のシートは URL ではなく `history.state` に持ち、ブラウザの戻るで閉じる
- **4 タブ**（下部。再生中だけタブの上に小さな再生バー）:
  - **ライブラリ**: 検索欄（`GET /api/tracks` の検索）と「アーティスト（albumartist）/ アルバム / 曲」の切り替え（最後の見方を
    localStorage に覚える）。1 段ずつ降り、一覧は仮想スクロール。行は曲名・アーティスト・アルバムと形式のバッジ
  - **フォルダ**: パンくずと、カテゴリ → アルバムアーティスト → アルバム → 曲の木（`GET /api/albums` からクライアントで組む）
  - **プレイリスト**: 普通とスマートの一覧（曲数・長さ付き）。普通は中身の外す・並べ替え。スマートは条件を
    「フィールド / 演算子 / 値」で組み立てる（AND / OR、条件ごとの NOT、ORDER BY、LIMIT。入力のたびに
    `POST /api/playlists/preview` で検証と件数）。組み立て画面で表せない規則（入れ子、AND と OR の混在）は開いた時点から、
    それ以外も「文で編集する」でルール文の編集へ移る（組み立てから表せなければ文のまま）。値の `"` と `\\` はエスケープして
    書く（[DSL](../../user-guide/dsl.md)）。新規作成・名前の変更・削除（確認あり。409 `open_plan` はその旨を出す）ができる
  - **購読**: 先頭に Inbox の未確認の件数（0 件なら出さない。確認は PC）。購読ごとのカード（最後の同期、「同期」）。
    同期は**手動のまま**（自動にしない）。「完了」は `playlist_sync` の終了ではなく**投入した ytdl のダウンロードがすべて
    終わった時点**で、それまで進捗バー（`GET /api/ytmusic/subscriptions/:id/progress`、SSE のジョブイベントで取り直す）。
    結果は 1 行（新しい曲の数・非公開・失敗と「再試行」）、カードの詳細に曲ごとの状態。購読の追加・変更・削除は PC。
    相は `mobile/subscriptions.ts` の `syncPhase`: 同期のジョブが無ければ未同期、同期か ytdl のどれかが queued / running なら
    同期中、すべて終端で同期の失敗か ytdl の失敗・**取り消し**があれば失敗、それ以外は完了。「同期」を押して 409 `duplicate`
    （既に待ち・走行中）が返ってもエラーにせず同期中として進み具合を出す。画面を閉じても同期はサーバで続き、戻れば
    最新を出す。Inbox の件数は `GET /api/inbox/summary` の `pending`
- **曲のシート**: 曲をタップで下から出る。半分は ジャケット・曲名・再生・バッジ・主要タグ、引き上げると全タグ・ファイル情報・RG。
  「プレイリストに追加」（`POST /api/playlists/:id/items`）。戻るボタンで シート → 一覧 → 上の階層
- **タグの修正**: 1 曲だけ。値の修正のみで、フィールドの追加・削除と空にすることはしない。PC と同じ
  `batch/preview` → `apply`（`ids: [id]`）を通り、`edit_batches` に載るので PC の「履歴」から巻き戻せる。pending / conflict
  のときは保存させない（`editBlockReason`: 「反映待ちの間は編集できません」/「競合を PC で解決してから編集してください」/
  missing は「ファイルが見つかりません」）。多値は PC のプロパティ欄と同じ `;` 区切り。空にすると「空にはできません。
  フィールドの削除は PC で行ってください」で止める
- **試し聴き**: 開いている間だけ（バックグラウンド再生・ロック画面・キューは無い）。LTE + Cloudflare 経由のため原本は流さず
  `?transcode=opus`（Derived があればそれ、無ければその場で変換。非可逆の原本はそのまま）。Opus が鳴らない端末（iOS Safari の
  判定は当てにならない）では、再生前にエラーになったとき**1 回だけ** `?transcode=aac` で同じ曲を引き直す。シートに使った
  コーデックを出す。ReplayGain は掛けない

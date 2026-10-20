### 7.5 タグ書き込み（編集バッチ）

```
編集バッチ確定
  → プレビュー（dry-run）。サーバは selection のスナップショット（対象 track_id と各行の
    tag_version / 事前条件）を作り selection_token を返す（§9、D-33）
  → 適用は selection_token の集合だけを対象にする。preview 後にスキャンで増えた行は含まれない
  → 対象トラックに pending の op があれば 409 で拒否（件数と track_id を返す）。
    `skip_pending: true` なら該当トラックを除外して続行（UI の「除外して適用」）
  → 1 トランザクションで:
       edit_batches(state='prepared')
       edit_ops(result='pending')  … トラック 1 本につき 1 op。事前条件
                                    (expected_dev / inode / size / mtime_ns / ctime_ns /
                                     tag_hash / rel_path) を記録
       edits                      … op 配下にフィールドごとの旧値・新値（JSON）
       → DB（track_tags / キャッシュ列）を新値へ更新、tag_version をトラックごとに 1 回 ++
       → track 単位の tagwrite ジョブを投入（edit_batch_id で紐づけ）
  → 各 tagwrite ジョブ（並列 4）:  バッチを state='applying' に
       rel_path で open → その FD の fstat で inode/size/mtime_ns/ctime_ns を確認
       （dev は照合しない。D-62）、同じ FD からタグを読んで tag_hash を確認
         ├ いずれか不一致 → result='skipped_conflict'。ファイルは触らない。
         │                  同一トランザクションでその FD の内容から DB を戻す（下記「overlay の解消」）
         ├ rel_path のみ不一致（外部 rename。スキャナが inode で新パスを追随済み）
         │     → 新パスの rel_path_key が他の行に占有されていなければ tags op は追随して続行。
         │       占有されていれば skipped_conflict。rename op は常に skipped_conflict。
         │       rename は ctime を進めるので、このとき ctime_ns だけの不一致は許容する（D-41）
         └ 一致 → その FD の親ディレクトリに tmp を O_EXCL で作成 → 内容をコピー →
                  op 配下の全フィールドを lofty で書き換え → fsync → rename →
                  同一トランザクションで (dev, inode, size, mtime_ns, ctime_ns, tag_hash) 更新、
                  op を result='applied'
  → 全 op が終端になったらバッチを集計: applied / partial / failed / cancelled
  → Derived のタグ上書きジョブを投入（tag_version の差分で判定）
```

- **ファイル反映の単位は op（トラック）。** フィールドごとに tmp+rename すると
  1 件目で inode が変わり 2 件目以降の事前条件が必ず外れる。同一トラックの全差分を
  1 回の書き込みで反映し、`tag_version` はトラックごと・バッチごとに 1 回だけ進める。
  結果（`result`）は op にだけ持ち、**フィールド単位の部分適用はしない**
  （1 フィールドでも事前条件に合わなければ op 全体が conflict）
- **形式ごとの写像は読み書きで 1 つ。** FLAC / Opus / Ogg は Vorbis Comment に任意キー・多値を
  そのまま書く。MP4（ALAC / AAC）は Vorbis 名を lofty の `ItemKey` に写像して ilst の標準 atom
  （`©nam` `trkn` `disk` `----:com.apple.iTunes:CATALOGNUMBER` 等）に、写像表に無いキーは
  `----:com.apple.iTunes:<KEY>` のフリーフォーム atom に書く（多値は 1 atom の複数値。`iTunNORM` は
  綴りを固定。D-77）。読み側は同じ写像で、フリーフォーム atom を名前を大文字化したキーとして取り込む。
  置き換え・削除は名前の大小文字を無視して既存 atom を消す（外部ツールが `MyKey` で書いた atom を
  `MYKEY` で上書きしても二重化しない）。`trkn` / `disk` は番号と総数が同居するので、片側だけの変更は
  相方を保つ。写像と書き手（`apply_ilst`）は Derived の `aac` 系統（§7.6）と共有する。MP3 は ID3v2 の
  標準フレーム（lofty が名前を知っている TXXX を含む）に写像し、写像表に無いキーは description = キーの
  TXXX に書く（多値は NUL 区切りの 1 フレーム。ID3v2.4）。読み側は TXXX の description を大文字化した
  キーとして取り込み、置き換え・削除は description の大小文字を無視して既存の TXXX を消す。ブロック
  （ID3v2 / ID3v1 / APE）ごとに split / merge で書き、写像できないフレームを保つ（D-110）。WAV / AIFF 等は
  lofty の generic `Tag` 経由で、写像できないキーは書けない（読み戻し照合で failed に閉じる）
- **事前条件に `ctime_ns` と `tag_hash` を含める。** inode も mtime も保ったままの in-place
  更新（`touch -r` を伴うタグツール）は dev/inode/mtime では見えない。ただし外部 rename も
  ctime を進めるため、rel_path が記録時点と違う（スキャナが追随した）ときに限り ctime_ns だけの
  不一致は許容する（D-41）
- **`expected_dev` は記録するが照合しない。** dev 番号はマウントのたびに振り直されうる（ZFS は
  ホスト再起動で変わる）ので、記録の後に再起動があると全 op が外れる。実体の同一性は
  inode / size / mtime_ns / ctime_ns / tag_hash で確認する。rename / normalize の所在判定
  （`same_inode`）、flaccheck / RG / 再生の「行と FD の照合」も同じ扱い（D-62）
- **DB 先行更新 + pending 記録**を採る。DB の値は「確定した真実」ではなく
  **書き込み意図のオーバーレイ**で、ファイル反映が終わるまで暫定。UI は pending を
  バッジで見せる。DB を失うと未反映の意図も失われる（ファイルは旧値のまま残るので
  データは壊れない）。DB を後から確定する案は、6 万件の一括編集で UI が数分間
  古いままになるため却下（D-24）
- **overlay の解消**: op が `applied` 以外の終端（skipped_conflict / failed / cancelled）に
  なるときは、**同じトランザクションで**そのトラックの DB 値をファイルの現在値に戻す
  （タグ・キャッシュ列・`tag_hash`・物理属性）。次回スキャン任せにしない。`tag_version` は
  既に進んでいるので据え置く（Derived の追随ジョブはファイルの現在値で書くだけなので無害）。
  ファイルを読めない（消えている・壊れている）ときだけ `edits.old_value` と事前条件の物理属性へ
  戻す（D-41）
- **pending 中の再編集は 409 で拒否する。** `edit_ops(track_id) WHERE result='pending'` の
  UNIQUE で DB 側でも保証する。後続バッチが先行 intent を統合する方式（`superseded`）は
  予約のみで P0 では実装しない
- **tmp + rename を採用**する。電源断でファイルが壊れないことを優先。
  代償として inode が変わるので、書き込み成功時に必ず DB を追随させる
- **リカバリ**: 起動時に `prepared` / `applying` のバッチについて、`pending` の op に
  対応する track ジョブを再投入する（dedup で二重にはならない）。クラッシュ前に
  rename まで済んでいた op は inode 不一致になるので、ファイルのタグを読み
  **全フィールドが新値と一致すれば `applied` として確定**、そうでなければ
  `skipped_conflict`（overlay の解消を伴う）。この確定は再投入されたジョブが通常の反映経路で
  行う（起動時の特別処理ではない）。同じジョブを何度再実行しても結果は変わらない。
  バックオフの上限に達する失敗では、ハンドラが op を `failed` に閉じて overlay を解消する（D-41）
- **stale ジョブ**: tagwrite の `dedup_key` は `tagwrite:<track_id>:<tag_version>`。
  409 により pending 中の再編集は起きないので、通常は stale にならない。
  それでも payload の版 < 現在値なら no-op で `done` にする（防御）
- **キャンセル**: バッチ単位。子ジョブに `cancel_requested_at` を立て、未着手の op は
  `failed`（error='cancelled'、overlay の解消を伴う）、進行中の op は完了を待つ。全 op が
  終端になったらバッチを `cancelled` に。applied になった op はそのまま残り、revert で戻せる
- **巻き戻し**（§9 `/api/history/:batch/revert`）:
  - 対象は**終端状態**（applied / partial / failed / cancelled）のバッチのみ。
    `prepared` / `applying` はまずキャンセルする
  - 対象 op 集合 = 元バッチで `applied` になった op − 既存の逆バッチ（`reverts_batch_id` = 元）で
    既に `applied` になっている op。**この集合が空なら 409 `already_reverted`**
  - 各 op について**全フィールドの現在値が元バッチの `new_value` と一致する場合だけ**
    `old_value` へ戻す（op 単位。1 フィールドでも違えば `skipped_conflict`）。
    一致しないものは外部変更または後続バッチの変更なので UI に件数を出す
  - 元バッチの `reverted_at` は、**逆バッチが終端になり、対象集合が全件 `applied` になったとき**
    だけ立てる。逆バッチが partial なら立てず、再度 revert すると残りだけが対象になる
  - やり直し（redo）= 逆バッチを revert する。同じ規則で処理される
  - 巻き戻しも通常のバッチなので、対象トラックに pending があれば 409
- **埋め込み画像の差し替えも tags op**（P1-3 書き側、D-60）。キー `PICTURE`（値は `tag_hash` と同じ
  `<mime>:<sha256hex>` の配列。`track_tags` にもこの形で入っている）を `Editor::prepare_picture` が
  「全画像を捨てて上げた 1 枚を front cover にする」変更として記録する。ユーザの JSON タグ操作は
  `PICTURE` を受け付けない。実体は `ArtworkStore`（`thumbs/<hex>/orig.<ext>`）にあり、tagwrite は
  **書く前に捨てる旧画像を同じ store へ退避**してから書く（退避できなければ `failed`。新画像が store に
  無ければ `failed`）。巻き戻しは通常の逆 op（旧画像は退避済み）。applied のとき album の再解決を予約し
  増分スキャンを投入する。spindle は同梱の cover ファイルを書かない（埋め込み統一。D-60）
- リネーム（P0-11）と論理削除も同じ機構に乗せる（`kind = rename / delete`）。
  巻き戻しの対象は tag に限らない。**一括リネームは coordinator が 2 phase で行う**（D-43）:
  prepare で DB の `rel_path` を 2 段階更新で新値にし（overlay。`expected_rel_path` が記録時点の
  物理パス）、album の所属も追随させ、**バッチ 1 つに `rename` ジョブ 1 つ**を投入する。ジョブは
  phase 1 で全 op の source を同じディレクトリの一時名 `spindle-rename-<op_id>.<ext>` へ
  `RENAME_NOREPLACE` で退避（事前条件は inode / size / mtime_ns / ctime_ns。dev は照合しない、
  D-62。外れていれば
  `skipped_conflict` で触らない）、phase 2 で `ordinal` 順に最終名へ置く（宛先ディレクトリは作る。
  宛先が取られていれば source へ戻して `skipped_conflict`）。最後に 1 トランザクションで op を
  終端にし、`rel_path` をファイルの所在へ揃え、物理属性を追随し、バッチを集計する。
  swap / 循環はこれで解ける。phase 境界でのクラッシュは、再投入されたジョブが op ごとの所在
  （最終名 / 一時名 / source）を inode で判定して続きを行う。cancel は phase 2 に入るまで
  （退避済みを戻して全 op を `failed('cancelled')`）。スキャナは pending の rename op があるトラックの
  所在が source / 一時名 / 最終名のいずれかなら衝突にしない


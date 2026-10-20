# 7.1 スキャン

走査は 4 相。同一性の判定は inventory 全体が揃ってから行う（走査途中では「移動元が消えた」
を判定できない）。DB への書き込みは最終相で単一 writer トランザクションにまとめる。

```
scan_runs に行を作る（state='running'）
Phase 1  inventory:  walk(Library) を並列に stat（symlink は辿らず一覧へ）。
                     (rel_path_key, dev, inode, nlink, size, mtime_ns, ctime_ns) をメモリに固定
Phase 2  candidates: inventory の各エントリと既存行の候補を、inode → audio_md5 → rel_path_key の
                     順で列挙（§6 の採用条件。md5 段の「移動元が消えている」は inventory 全体で判定）。
                     解決が要求しうる audio_md5（inode 一致で size も mtime も違う行が md5 を持つ /
                     移動候補があるときの未決エントリ）だけを先に並列で計算し、進捗を出す。初回や
                     移動の無い増分では要求が無く、可逆のデコードは Phase 3 に回る（D-50）
Phase 3  claim:      rel_path_key 昇順で決定的に採用。取り合いに負けたエントリは次の段へ。
                     変更ありのエントリはここでタグ読込・フィンガープリント計算（並列）
Phase 4  commit:     1 トランザクションで
                       a. 採用行の rel_path / rel_path_key を「予約済み一時 key」へ全件移す
                       b. 最終値へ全件移す（swap rename A↔B や循環でも UNIQUE を踏まない）
                       c. 物理属性・タグ・版・seen_run_id・復活（missing_since=NULL）を反映
                       d. 新規行の挿入
                       e. album 照合（下記）。album の rel_dir も同じ 2 段階で更新
                       f. state='completed' なら、この run で claim されなかった active 行に
                          missing_since を立てる。failed / cancelled では立てない
                     FTS はトリガで追随
```

**アルバムの照合**（ディレクトリ rename で album id を失わないため。[D-32](../../decisions/D-032.md)）:

```
ディレクトリ D に今回見つかったトラック集合 T(D) について
  1. T(D) の行が直前まで属していた album を数える
  2. mb_release_id が一致する既存 album が**ちょうど 1 つ**で、その旧 rel_dir が
     inventory に無ければそれ（複数一致なら自動では寄せない → 3 へ）
  3. なければ、T(D) の過半数が属していた album A で、A の旧 rel_dir が inventory に無いもの
     → A.rel_dir を D に書き換える（id 維持。verifications / artwork が残る）
  4. どれにも当たらなければ新規 album
分割（一部だけ別ディレクトリへ）: 移った側は新規 album、残った側は id 維持
統合（2 ディレクトリが 1 つに）: 最多の album が id を維持、他は構成 0 になり
  albums.missing_since を立てる（行は消さない。GC が回収）
```

- 初回フルスキャンは並列度 = CPU コア数。以降は最速パス（[§6](../06-data-model.md)）で大半を飛ばす
- 外部（foobar2000 等）による書き換えを検出したら DB を上書きする。
  **ファイルが常に勝つ**（原則1）
- **例外: ファイル反映待ちのトラック（`edit_ops.result = 'pending'` がある）は、
  pending の op が所有する論理フィールドを再評価しない。** 一括編集は DB を先に更新し、
  ファイルへの反映は非同期なので（[§7.5](05-tagwrite.md)）、この窓で「ファイルが勝つ」を適用すると
  編集が DB から巻き戻され、後続の tagwrite と食い違う。
  ただし**物理的な所在は pending 中も常に追随する**: `(dev, inode)` で見つけた
  ファイルの `rel_path` / `size` / `mtime_ns` / `ctime_ns` は更新する。外部 rename は inode も
  mtime も変えないので、これを止めると tagwrite が旧パスを開けなくなる。
  抑止するのは `tags` op ならタグとキャッシュ列・`tag_version`、`rename` op なら
  `rel_path`（外部 rename と衝突したら op を `skipped_conflict` にし、同じトランザクションで
  `rel_path` を実在パスへ追随させる。[D-43](../../decisions/D-043.md)）だけ。
  外部 rename の**宛先が別の行（missing 行を含む）に占有されていた**場合は、Phase 4 で
  その行の missing 確定と key の入れ替えを同じトランザクションで行う
- 走査対象は Library のみ。Derived / Archive はスキャンしない

**アートワーク**（Phase 5、commit の後。[D-49](../../decisions/D-049.md)）:

- album のアートワークは、ディレクトリの**同梱カバー画像**（`cover` / `folder` / `front` ×
  `jpg` / `jpeg` / `png` / `webp`。名前 → 拡張子の優先順、大文字小文字は区別しない）があれば
  それ、無ければ**構成トラックを `disc_no` / `track_no` / `rel_path` 順に見て最初に見つかる埋め込み
  画像**（front cover を優先）。形式はバイト列のヘッダで判別し、拡張子やタグの MIME は信用しない
- 画像は SHA-256 でハッシュアドレスし（`artwork` 表。行は消さない）、原画像を**元の形式のまま**
  `<data>/thumbs/<hex>/orig.<ext>` に置く。サムネイル（`<size>.webp`、256 / 768。長辺、拡大なし）は
  `thumbnail` ジョブが ffmpeg で作る。Library には何も書かない
- Phase 4 は行が変わったトラックの現在の album と直前まで属していた album の
  `albums.artwork_resolved_at` を NULL にして再解決を**予約**する（同じトランザクション）。
  Phase 5 が解決し直すのは deep なら全 album、それ以外は「予約された（NULL）」「同梱画像の有無・
  stat（inode / size / mtime / ctime。`albums.cover_*`）が前回と違う」「参照中の原画像がキャッシュに
  無い」album。missing の album は触らない。決められない album（I/O 失敗、読んでいる間に同梱画像が
  変わった、構成トラックを読めない）は状態を動かさず次回やり直す。同梱画像が画像として認識できない
  ときは埋め込みへ倒し、stat は記録する（変わるまで読み直さない）
- Phase 4 の commit 後なので、Phase 5 の cancel / 失敗は run の状態（completed）と missing の確定を
  戻さない。予約が残るので次のスキャンで続きを行う
- **トラック自身の画像**（[D-61](../../decisions/D-061.md)）: Phase 3 が各トラックの埋め込み画像（front cover 優先）を読んで
  キャッシュへ置き、Phase 4 が `tracks.artwork_id` に記録する（無ければ NULL）。tagwrite の書き戻しも
  同じ。変更なしの行は読まないので、既存行は deep scan で埋まる

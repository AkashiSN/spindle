# 7.3 遡及照合（ログなし既存 FLAC）

CUETools の "verify from files" 相当。

```
アルバムをディスク（disc_no）ごとに分け、全トラックが揃っているか確認
  → 各トラックのサンプル数（STREAMINFO）から TOC 再構成
     offset = 150 + Σ(前トラックのセクタ数)
  → AccurateRip DiscID / MusicBrainz DiscID を算出
  → デコードして CRC 表を作る（1 回流すだけで ±2939 サンプルの全オフセットの CRC が出る）
  → CTDB / AccurateRip 照会 → オフセットを探して照合
  → verify.log 出力（data/verify/<album_id>.log。rip.log とは別物）
```

照合ログは album ごとに最新 1 回分しか残らない。記録画面のログタブ（[§12.4](../12-ui/04-records.md)）が `GET /api/logs` で実行ごとに
並べ、本文を `GET /api/logs/{key}/text` で読む（古い実行は「上書きされた」と出す。[D-106](../../decisions/D-106.md)）。

`verify` ジョブ（album 単位、並列 2。`POST /api/verify { selection }`）。読むだけでファイルは
書かない。ファイルは root の FD を fstat して DB の行と照合してから読む（[D-62](../../decisions/D-062.md)）。
照会の 404 は「登録なし」（0 件）として扱い、失敗にしない（AccurateRip は `dBAR-*.bin` が無い、
CTDB は `lookup2.php` が登録の無い TOC に本文 `Not Found` で返す）。それ以外の非 2xx は照会失敗で再試行。

適用条件:

- **44.1kHz / 16bit / 2ch の FLAC のみ。** それ以外を含むディスクは `unverifiable`
  （TOC が存在しない音源。品質の劣後ではない）
- 各トラックのサンプル数が **588 の倍数**（1セクタ）であること。
  端数があれば CD 由来でないか加工済みと判定して `unverifiable`
- トラック番号が 1 から連続していること（不完全なディスクは TOC を作れないので何もしない）
- **オフセットは探す**（[D-63](../../decisions/D-063.md)）。DB に登録された値は他人のドライブで吸ったもので、読み取り
  オフセットの補正が違えば同じ盤でも数十サンプルずれる。CUETools と同じ ±(5×588−1) の範囲で
  「一致したトラック数 → 信頼度の和 → 0 に近い」順に 1 つ選び、`detected_offset` に残す
  （ディスクで 1 つ。トラックごとに別のオフセットは採らない）。AccurateRip v2 はオフセットに
  対して線形でないので 0 だけ、それ以外は v1 で比べる
- CTDB は `fuzzy=1` で別リリースも返るので、音声部分の長さと音声トラック数が同じエントリだけを
  候補にする。AccurateRip の ID は CUETools / dBpoweramp 式（Enhanced CD でも実リードアウト）
- トラックごとに: CTDB 一致 → `verified_ctdb`、AccurateRip だけ一致 → `verified_ar`、
  候補はあるが不一致 → `mismatch`、どちらにも候補なし → 据え置き（`not_attempted`）。
  結果は `album_verifications`（手法 × ディスク。履歴として積む）と `track_verifications`
  （自分の CRC と一致の有無）に残す
- 照会に失敗したらジョブを失敗させて再試行し、何も記録しない（不一致を「照会できなかった」で
  汚さない）。記録は 1 トランザクション（ディスク × 手法の行、トラックの行、`tracks.verification`）で、
  照合を始めたときの `audio_version` が 1 本でも進んでいれば何も書かない。verify.log は tmp に書き、
  トランザクションの中で本来の名前に rename してから commit する。`album_verifications.job_id` で
  同じジョブの再実行（commit の後に落ちた場合）を見分け、何もしない（履歴もログも初回のまま）
- ジョブの結果 1 行（`jobs.note`）にディスクごと・手法ごとの結果（一致数と信頼度 / 登録なし /
  検証不能・照合しなかった理由）を残し、操作タブが投入したジョブを追って出す（[D-96](../../decisions/D-096.md)）。結果 1 行は記録と
  同じトランザクションで `jobs.note` に書き、commit の後に落ちた再実行はそれをそのまま返す
- **不一致は不良を意味しない。** ギャップ処理差、隠しトラック、データトラックの存在で
  普通に外れる。`mismatch` は「要確認」として扱い、警告色で表示しない

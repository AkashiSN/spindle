### 7.9 FLAC 健全性チェック（移行時 + 任意）

```
flac -t で検証
  ├ STREAMINFO MD5 が未設定（全ゼロ） → デコードした PCM MD5 を STREAMINFO に補填（再エンコードしない）
  │    一部の古いエンコーダや配信由来の FLAC で実際に起きる。
  │    未設定だと同一性解決の第 2 手段が使えず、遡及照合も不可能
  ├ デコードエラー → 要対応としてフラグ
  └ 正常 → そのまま
```

**圧縮レベルを揃えるための一括再エンコードは行わない。**
`flac -8` は圧縮率だけの違いでデコード結果は完全に同一であり、削減は
概ね 0.5〜1.5%。対価として 200GB を書き直すことになり、ZFS スナップショットが
旧ブロックを掴むため保持期間中は実効使用量が倍増する。
`flac -8` は新規生成物（CD リップ、WAV 正規化）にのみ適用する。

MD5 補填のための書き換えでは `audio_version` を据え置く
（音声内容が変わらないため Derived の再生成は不要）。inode と mtime は変わるので
DB の追随は必要。補填は再エンコードではなく STREAMINFO の MD5 16 バイトだけを書き換える
（P1-5b、D-57 / D-59）。補填は編集バッチ（`edit_ops.kind = 'md5'`、`edits.key = 'audio_md5'`、
値は hex で全ゼロ = 未設定。`new_value` は反映時に計算値を書く）として記録し、tagwrite ジョブが
tmp + rename で反映する。巻き戻しは全ゼロを書き戻す（`audio_md5` は NULL、`flac_check` は
`md5_missing` に戻る）。

検査（`flaccheck` ジョブ）は読むだけで、結果を `tracks.flac_check`（`ok` / `md5_missing` /
`decode_error`）に検査時の `audio_version` 付きで記録する。版が進めば結果は古い扱いになり、
`[normalize].flac_verify_on_import` ならスキャン完了時に結果の無い FLAC を自動で検査する。
手動は `POST /api/flaccheck { selection }`。一覧の固定フィルタ `flac_unchecked` / `flac_error`。

---


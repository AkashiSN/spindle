# 7.4 ロスレス正規化（WAV / ALAC / AIFF → FLAC）

Library のロスレスは FLAC に統一する（[D-45](../../decisions/D-045.md)）。移行で取り込んだ ALAC も、取り込まれた WAV /
AIFF も、同じ機構で FLAC にする。

```
POST /api/normalize/preview → /apply（selection。rename と同型。D-46）
  → edit_batches + edit_ops(kind='archive', pending) + edits(rel_path 旧→新, codec 旧→'flac')
    を記録し、track 単位の normalize ジョブを投入。DB は先行更新しない
normalize ジョブ（track をロック）
  → 事前条件（stat + tag_hash）を確認。外れていれば skipped_conflict
  → symphonia でデコードして PCM MD5 算出
  → ffmpeg でデコード → flac -8 --verify でエンコード（data/tmp）。タグと画像は lofty で移す
  → 生成 FLAC の STREAMINFO MD5 と突き合わせ
     ├ 一致   → Library の同じディレクトリに tmp → fsync → RENAME_NOREPLACE で <name>.flac を置く
     │          → 元ファイルを同じディレクトリの一時名 spindle-normalize-<op_id>.<ext> へ退避
     │            （inode を確認。外部の tmp + rename と分離する）
     │          → Archive/ の同じ相対パスへ実コピー（SHA-256 を照合し、edits に記録）
     │          → unlink の直前に同じ FD の stat とバイト列を再照合 → 一時名を unlink
     │          → 1 トランザクションで op を applied、rel_path / 物理属性 / codec を追随、
     │            archived_files に台帳（eligible_after = now + retention）を追加、
     │            original_codec（'wav' | 'alac' | 'aiff'）、normalized_at 記録。
     │            audio_md5 は同じ PCM なので変わらず、audio_version も上げない（Derived は据え置き）。
     │            tag_version は写したタグの tag_hash が元と違うときだけ進める
     └ 不一致 → op は failed、元ファイルを残す（生成した FLAC は捨てる）
```

宛先は拡張子を `.flac` に置き換えた同じパス。宛先を別のトラックが占有していれば計画の時点で
conflict。反映の直前に宛先へ別のファイルが現れていれば `skipped_conflict`（音声 MD5 が期待値と
一致するときだけ自分の成果物とみなして続きを行う。クラッシュ後の再投入も同じ判定）。
スキャナは pending の archive op の元・一時名・宛先のパスを「作業中」として扱い、新規登録も
missing 判定もしない。unlink の前に元ファイルが外部で更新・差し替えされていれば conflict にして
元パスへ戻す（自分が置いた宛先と Archive のコピーは消す）。cancel は Library を触る前まで
（変換中の子プロセスは止める）。

WAV は RIFF INFO / ID3 のどちらを使うかがソフトごとに異なり、ReplayGain タグの
互換性も低い。ALAC（MP4 ilst）は複数値タグが弱く、Safari 以外のブラウザで再生できない。
可逆変換なので情報は失われない。読み込み互換のため WAV / ALAC / AIFF の再生・
取り込み自体は引き続きサポートする。

**元ファイルは即時削除しない。** 物理削除はユーザデータ全般と同じく GC ジョブのみが行う
（禁止事項）。退避した元ファイルは `archived_files`（state='held'）を台帳として GC が
`eligible_after` 経過後に回収し state='deleted' にする。それまでは履歴の巻き戻しで
Library へ戻せる: 元ファイルは Archive から Library へ**コピー**で戻し（Archive の実体は残る。
state='restored'）、Library にあった FLAC は Archive へ move して台帳に `reason='restore'` の行を
足す（GC の対象）。やり直し（逆バッチの revert）はその逆で、同じパスの台帳行は作り直さず `held` に
戻す。復元元が GC 済みの op は `skipped_conflict`（[D-46](../../decisions/D-046.md)）。
編集履歴は revert / redo で状態が動くので GC の台帳には使わない。
Archive の「追記のみ」の例外はこの GC だけ。

PCM MD5 の一致が保証するのは**音声サンプルの同一性だけ**で、WAV の RIFF INFO /
ID3 / 未知チャンク / コンテナのバイト列は FLAC から再生成できない。保持期間後に
コンテナを不可逆に捨てるのは意図した決定（[D-10](../../decisions/D-010.md)）。

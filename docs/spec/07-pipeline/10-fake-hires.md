# 7.10 偽ハイレゾ検出（任意）

配信で買ったハイレゾ音源には、CD 由来の AccurateRip / CTDB のような品質の裏付けが無い
（`verification` は `unverifiable` のまま）。44.1 kHz / 16 bit のマスターをアップサンプリング・
ビット深度の水増しで「24/96」として売る例があり、移行データにも `Album/Hi-Res/01.m4a`（24/96）と
アルバム直下の 16/44 が別トラックとして両方入っている。どちらを残すかの判断材料として、
可逆かつ **`sample_rate > 48000` または `bit_depth > 16`** のトラックを解析し、結果を表示と
絞り込みに出す。**判定を消費する自動処理は無い**（Derived・配布ビュー・RG・リネームは判定に
依らない。[D-71](../../decisions/D-071.md)）。非可逆由来の可逆（44.1 kHz の FLAC が 16 kHz で切れている等）は対象外。

```
hirescheck ジョブ（読むだけ。track_id + audio_version、並列 = max(1, CPU コア数 / 2)）
  root の dirfd で開き fstat を行と照合 → media::decode で PCM を流す（PcmSink）
  ├ スペクトル（sample_rate > 48000 のとき）
  │   チャンネルごとに Hann 窓 8192 点 / ホップ 8192 の FFT（rustfft）。パワーはフルスケール正弦波が
  │   0 dB になるよう正規化（|X_k|² × (2 / Σw)²）し、線形のまま累積して有音フレーム数で割る
  │   RMS が -70 dBFS 未満のフレームは無音として捨てる
  │   終了時（有音フレームが 1 以上のとき）:
  │     P[k]        = 平均線形パワー（未平滑）。dB は 10·log10(max(P, 1e-20))（下限 -200 dB）
  │     S[k]        = 各ビンを中心に ±1/6 オクターブ（= 1/3 オクターブ幅）の P を線形平均して dB。
  │                   窓は [0, Nyquist] で切る（端は片側だけ）
  │     floor_db    = max(Nyquist 直下 5% のビンの S の中央値, −150 dB)。完全ゼロの帯域で床が
  │                   −200 dB に張り付き、遠いサイドローブまで拾うのを防ぐ（−150 dB は 24 bit の
  │                   量子化ノイズ（1 ビン約 −186 dB）より上、16 bit のディザの直下）
  │     候補 f_s    = S[k] > floor_db + 10 dB を満たす最高ビンの中心周波数（S は窓の下端
  │                   f_s / 2^(1/6) まで信号を引きずるので、これ自体は上へ最大 1/6 オクターブずれる）。
  │                   該当ビンが無ければ（上端まで信号がある） cutoff_hz = Nyquist、cliff_db = NULL
  │     cutoff_hz   = 平滑化前の P_dB に戻り、[f_s / 2^(1/3), f_s] の範囲で 200 Hz 幅の dB 平均の段差
  │                   mean(P_dB[k−m, k)) − mean(P_dB[k, k+m)) が最大のビンの周波数（推定エッジ）。
  │                   brickwall では段差が真のエッジで最大になり、Hann 窓の漏れの量に依らない
  │                   （合成信号で 22.05 kHz → 22,090 Hz、24 kHz → 24,035 Hz）
  │     cliff_db    = 10·log10(mean(P[cutoff−1 kHz, cutoff)) / mean(P[cutoff, cutoff+1 kHz]))。
  │                   **平滑化前の P で、cutoff と同じ推定エッジを中心に測る**（S で測ると崖を自分で
  │                   ぼかす）。帯域は [0, Nyquist] で切り、どちらかの帯域にビンが無ければ（cutoff が
  │                   Nyquist 直下・1 kHz 未満）NULL。分母が 0 なら +200 dB に飽和
  │   チャンネルが複数なら cutoff が最大のチャンネルを採り、**cliff もその同じチャンネルの値**を対で
  │   採る（片側だけ本物なら本物。cutoff と cliff を別々に最大化しない）
  │   有音フレームが 0 なら cutoff_hz / cliff_db は NULL
  ├ ビット（bit_depth ≤ 24 のとき）
  │   round(sample × 2^(bit_depth−1)) を i32 に戻して全チャンネル・全サンプルを OR
  │   effective_bits = bit_depth − OR の末尾ゼロビット数。OR = 0（全無音）なら NULL
  │   32 bit は f32 で正確でないので NULL
  └ 判定（上から順に最初に当たったもの。計測しなかった側は NULL）
      decode_error : デコード失敗（hires_check_error に stderr / メッセージの先頭 500 文字）
      inconclusive : 計測できたものが無い（cutoff_hz と effective_bits がともに NULL。全無音、
                     32 bit かつ ≤ 48 kHz）
      both         : upsampled かつ padded
      upsampled    : cutoff_hz ≤ [hires].cutoff_hz かつ（cliff_db ≥ [hires].cliff_db または
                     cutoff_hz ≤ [hires].hard_cutoff_hz）。後者は 44.1 kHz の Nyquist + 余裕で、
                     >48 kHz のファイルでそこから上が空なら崖の有無を問わない（D-71 追記）
      padded       : effective_bits ≤ 16
      inconclusive : cutoff_hz ≤ [hires].cutoff_hz だが崖が無い（cliff_db が NULL か閾値未満。
                     自然なロールオフ。本物の可能性あり）
      ok           : いずれでもない
```

カットオフだけで判定しない理由: 本物でも録音や マスタリングの都合で自然に減衰する帯域がある。
SRC のローパスは 1 kHz 以内に段差（実測 12〜21 dB。エッジ直下の音楽の残りと上げた後の床の差で
決まり、30 dB には届かない）を作るので、それを条件に加えて誤検出を避ける。ただし 44.1 kHz の
Nyquist（22.05 kHz）の下で床に沈むものは、段差が無くても本物の >48 kHz 録音ではあり得ない
（実機の 106 本の本物はすべて 25.4 kHz 以上まで伸びていた）ので `hard_cutoff_hz` で拾う。
22.5〜25 kHz の帯（48 kHz マスター）で崖の無いものは `inconclusive` として人が見る。

結果は `tracks.hires_check` に検査時の `audio_version` 付きで記録し（`hires_checked_at` /
`hires_check_version` / `hires_check_error`）、**計測値 `hires_cutoff_hz` / `hires_cliff_db` /
`hires_effective_bits` も残す**（しきい値を変えたときや目視の判断に使う。判定は検査時に確定し、しきい値の変更は既存の
結果を書き換えない。再判定は手動投入）。版が進めば結果は古い扱い（`stale`）。
`[hires].check_on_import` ならスキャン完了時に結果の無い対象を自動で投入する（flaccheck と同型）。
手動は `POST /api/hirescheck { selection }`（対象外・missing は `skipped`）。
一覧の固定フィルタ `hires_unchecked` / `hires_suspect`（`upsampled` / `padded` / `both`）。
DSL は `hirescheck`（文字列）、`cutoff`（数値、Hz）、`cliff`（数値、dB）、`effectivebits`（数値）。

開いた FD の fstat が行と一致しない・missing なら何も書かず `Outcome::Done` で終える（flaccheck と
同じ。API の `skipped` は投入時に対象外だった数で、ジョブの終端とは別）。次のスキャンで版が進めば
再投入される。`record` は `WHERE audio_version = ?` で、検査中に版が進んでいれば書かない。

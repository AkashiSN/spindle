### 7.6 Derived 生成

Derived は**系統（variant）**ごとに 1 本ずつ作る。系統は `opus`（Android の同期・Web 再生・配布ビュー）と
`aac`（Mac のミュージック.app へ取り込む Apple 向け）の 2 つで固定（P4-7 で系統化、P4-8 で `aac` の実体。
D-9 追記、D-75）。系統の設定は `config.toml` が正で、起動時に `derived_variants` 表へ写す（投入判定は
どこからでも同じ接続で設定を引ける。D-55 の `fb2k_prefix` と同じ流儀）。設定に無い系統は投入も生成も
しない。

| 項目 | 規則 |
|---|---|
| パス | `Derived/<variant>/` 以下に Library と完全ミラー（拡張子だけ `.opus` / `.m4a`）。`paths.derived` は 1 つ |
| 設定 | `[encode.derived.<variant>]` に `enabled` と `bitrate`（下記）。節を省略したときの `enabled` は **`opus` が true、`aac` が false**（既存の config をそのまま新版で起動しても aac は始まらない。RG 全件解析 → 有効化の順序を崩さない）。`enabled = false` の系統は**凍結**: 新しく作らず、既存の行とファイルは Move / Retag / Encode のどれも行わず、配布ビュー・バッジ・`has_derived` は既存行をそのまま使う。GC は系統を区別せず `Derived/` 全体で「行に無いファイル = 孤児」を回収する（凍結でも行は残るので消えない） |
| 出力仕様の世代 | 系統ごとに 2 つの文字列を設定から作り、行に保存する。**`audio_profile`**（音声に効く設定: codec / bitrate / サンプルレート規則 / RG 焼き込み方式の版。例 `opus:256:v1`、`aac:256:48k:bake1`）と **`tag_profile`**（タグに効く設定: `multi_value_separator` / iTunNORM 規則の版。例 `aac:sep= & :itunnorm0:v1`）。エンコーダの引数や規則を変えるときは版を上げる |
| 判定 | `audio_version` 差分・**行の `audio_profile` が設定と食い違う** → 再エンコード / `tag_version`・埋めた画像（`src_artwork_id`）・RG の解析世代（`src_rg_scanned_at`）・**`tag_profile`** の差分のみ → タグ上書き（**`aac` は RG を音声に焼き込むので RG 世代の差分は再エンコード**）/ パスの差分のみ → rename。優先順はこの順（再エンコードは残りを兼ねる） |
| RG の前提 | **どの系統も RG が揃ってから作る**: 解析済み（`rg_scanned_at` と `rg_track_gain` / `rg_track_peak`）で、`[replaygain].write_tags = true` なら Library のタグへの書き込みも確認済み（`rg_written_at >= rg_scanned_at`）。揃っていない間は凍結と同じく既存の行とファイルに触らない。順序は rg → rgwrite → tagwrite の applied → transcode（D-97） |
| 投入 | scan ジョブの完了時に食い違う全トラック × 系統、tagwrite / rename の applied、RG 解析の保存（D-51）。ジョブは `transcode`（`(track_id, variant)` 単位、`audio_version` で dedup）で、ハンドラが現在値から必要な処理を決める。決めた処理（エンコード / タグ更新 / 移動 / 移動 + タグ更新）を実行中から `jobs.note` に書き、完了時は実際に行った処理（移動・タグ更新が Derived の消失でエンコードに倒れたら「エンコード」、何もしなければ「変更なし」「対象外」）を残す。種別はどれも「Derived 生成」なので、ジョブ画面の結果列で見分ける（D-111）。P4-1 の共通並列予算の対象 |
| 追随 | Library の移動に追随（Derived を rename）。削除には追随せず（missing は可逆）、`retention_days` 超の回収と孤児（行に無いファイル。`Derived/` 全体）は GC ジョブ |
| マルチch | どの系統も既定で対象外（チャンネル数不明も対象外）。トラック単位の `-ac 2` ダウンミックスは需要が出たら（D-51。実データは全件 2ch） |
| 配布ビュー | `delivery` は **`opus` 系統に固定**（Android の m3u8、`?transcode=opus`、`has_derived`）。`aac` は配布ビューを持たない。端末へは、Android に `opus` 系統、iPhone に `aac` 系統を既定で送る（端末ごとに選べる）。iPhone へは Mac のエージェントが `aac` を取り込み、曲は `~/Music/spindle` に置き、プレイリストはミュージック.app の「spindle」フォルダに作る（§7.11、D-75、D-95） |

**`opus` 系統**

| 項目 | 規則 |
|---|---|
| 対象 | Library 内の可逆のみ（flac / alac / wav）。opus / aac / mp3 は原本をそのまま配布（D-8） |
| 出力 | `opusenc --vbr --music --bitrate <bitrate>`。既定 **256 kbps**（D-9 追記。可逆 237 GB で約 58 GB） |
| RG | 再解析しない。Library 側の解析値を `R128_*` へ変換して埋める（`REPLAYGAIN_*` は書かない）。RG が揃うまで作らない（上の「RG の前提」） |
| 画像 | トラック自身の埋め込み画像（`tracks.artwork_id`。D-61）、無ければ album のアートワーク（§7.1）の長辺 768 の WebP を 1 枚だけ埋める（D-51） |

**`aac` 系統**（Apple 向け。D-75）

| 項目 | 規則 |
|---|---|
| 対象 | 可逆（flac / alac / wav）に加え、`lossy_sources = true` なら**非可逆も**（opus / ogg / mp3 / aac → AAC。世代劣化は承知の上で、ミュージック.app が Opus を読めないため。**D-8 の例外**）。原本が AAC でも同じ経路で再エンコードする（stream copy では RG の焼き込みとリサンプルができない。D-75）。`lossy_sources` を true → false にしても既存の非可逆の行とファイルは消さない（対象外 = Skip で凍結と同じ扱い。物理削除は GC のみ） |
| 出力 | ffmpeg **1 パス**（中間 WAV なし。FD を stdin に繋ぐのは opus と同じ）: `-i /dev/stdin -map 0:a:0 -vn -map_metadata -1 -af volume=<gain>dB [-ar 48000] -c:a aac -b:a <bitrate>k -f mp4`（内蔵エンコーダ。既定 256 kbps）。48 kHz 超は 48 kHz へ落とす（`-ar 48000`）、44.1 / 48 は据え置き。`audio_profile` は `aac:<bitrate>:48k:bake1`、`tag_profile` は `aac:sep=<区切り>:itunnorm0:v1` |
| RG | **track gain を音声に焼き込む**（`-af volume=<gain>dB`。gain は `min(rg_track_gain, −20·log10(rg_track_peak))` で**エンコーダ入力を 0 dBTP 以下に抑える**（peak は true peak なので 1.0 超なら減衰側に倒れる。AAC 再符号化後のオーバーシュートまでは保証しない。gain が有限でなければ 0、peak は有限かつ > 0 のときだけ上限を掛ける）。album gain は使わない）。**RG 未解析のトラックは作らず待つ**（「解析済み」= `rg_scanned_at` と `rg_track_gain` / `rg_track_peak` の 3 つが揃っていること。時刻だけ残った行は対象外。`write_tags` ならタグへの書き込みも待つ（上の「RG の前提」。D-97）。書き込みの tagwrite の applied で投入される。二度エンコードの回避）。RG の解析世代（`src_rg_scanned_at`）の差分は**再エンコード**（album gain の on / off も世代を進めるので、その album の aac は作り直される。track gain しか使わないが値ベースの判定に列を足すより単純で、まれな操作なので許容。D-75）。タグには `iTunNORM` を **0 dB 相当**で書き、端末のサウンドチェック ON でも二重に掛からないようにする。値は 10 個の 8 桁 16 進を空白区切り（先頭にも空白）で、1〜2 値目（基準 1/1000）は `000003E8`、3〜4 値目（同じ量の基準 1/2500 の表現）は `000009C4`、残り 6 値は `00000000`: `" 000003E8 000003E8 000009C4 000009C4 00000000 00000000 00000000 00000000 00000000 00000000"`。`REPLAYGAIN_*` / `R128_*` は書かない |
| タグ | Library のタグを写す（`REPLAYGAIN_*` / `R128_*` / 既存の `ITUNNORM` は落とす）。**同じキーの複数値は出現順に `multi_value_separator`（既定 `" & "`）で 1 値に結合**（ミュージック.app は複数値の 1 つしか見せない。ARTIST / ALBUMARTIST / GENRE / COMPOSER など多値になり得る全フィールド）。写像は Library の tagwrite と同じ（§7.5「形式ごとの写像」）: Vorbis 名を lofty の `ItemKey` に写像して ilst の標準 atom（`©ART` `trkn` `disk` `©gen` 等）に書き、写像できないキーは `----:com.apple.iTunes:<KEY>` のフリーフォーム。`iTunNORM` は内部キーが大文字化されても atom 名を `iTunNORM` に固定する（大小文字を special-case） |
| 画像 | `opus` と同じ選び方で、長辺 768 の **JPEG**（ミュージック.app は `covr` の WebP を読まない）。`thumbs/<hex>/768.jpg` をキャッシュに足す（thumbnail ジョブと同じ変換に形式を足したもの。`-pix_fmt yuvj420p -q:v 2`） |

```toml
[encode]
flac_compression = 8

[encode.derived.opus]
enabled = true
bitrate = 256                   # opusenc --vbr --music --bitrate

[encode.derived.aac]
enabled = true
bitrate = 256                   # ffmpeg -c:a aac -b:a
lossy_sources = true            # 非可逆原本も AAC へ（D-8 の例外。aac 原本も再エンコード）
multi_value_separator = " & "   # 多値フィールドの結合
```

`derived_variants` の `lossy_sources` / `multi_value_separator` は aac 系統の設定（`eligible` が前者を、ハンドラが後者を
表から引く。設定は config が正で、起動時に両系統を写す）。

`derived_files` の主キーは `(track_id, variant)` で、配布ビュー `delivery` は `variant = 'opus'` の行を見る。
系統を持つ前の行（ルート直下・`audio_profile = 'opus:128:v1'`）は、既定の 256k では次の transcode が
`audio_profile` の差分で再エンコードして `opus/` 配下へ置き、旧ファイルは transcode の退避経路で消える。
`bitrate = 128` のまま（`audio_profile` が一致）ならパスの差分だけなので Move で `opus/` 配下へ移る（D-75）。

`has_derived`（DSL / フラグ / バッジ）は **`opus` 系統の行**の有無（= `opus` 系統を生成済みか。音声版が古い間は
配布ビューが原本へ倒れるが、`has_derived` は行の有無のまま）。
`aac` しか無いトラック（非可逆原本）は `has_derived` にならない。`GET /api/tracks` の `derived` は系統ごとに
集約して返す（2 系統あっても行は 1 つ）。

容量逼迫時のみ、トラック単位で `force_transcode` を手動指定可能（**元が 256kbps 以上の場合のみ**）
という `opus` 系統の例外は、需要が出るまで実装しない（D-51。実データの非可逆は Opus が大半で削減にならない）。


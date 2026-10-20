# 11. 再生とトランスコード

| コーデック | ブラウザ | 方針 |
|---|---|---|
| FLAC | Chrome / Firefox / Safari 対応 | 直送 |
| Opus | Chrome / Firefox / Safari 18.4+ 対応 | 直送（Opus 不可のブラウザ向けの AAC 変換は作らない。[D-52](../decisions/D-052.md)。スマホ版の試し聴きは例外で `?transcode=aac` を使う。[D-115](../decisions/D-115.md)） |
| AAC (m4a) | 全対応 | 直送 |
| WAV | 全対応 | 直送（サイズ大） |
| **ALAC** | **Safari のみ** | **既定で Opus へ変換** |
| ハイレゾ FLAC | 再生可だが帯域大 | 既定の Derived（Opus 48 kHz）再生で吸収。「原本」を選んだときはそのまま送る（[D-52](../decisions/D-052.md)） |

- `GET /api/stream/:id` は原本を Range で直送。`?transcode=opus` は `delivery` が Derived を指せば
  Derived を Range で直送し、無いときだけ ffmpeg を `stdout` パイプで起動して chunked で返す（[D-52](../decisions/D-052.md)）
- `?transcode=aac` は `aac` 系統の Derived（m4a）が現在値なら Range で直送し、無いときは ffmpeg で AAC を ADTS として
  chunked で返す（`audio/aac`。Range なし。シークは `?start=`）。非可逆の原本は `opus` / `aac` のどちらでも変換せずそのまま
  返す（世代劣化させない）。スマホ版の試し聴きが、Ogg/Opus を再生できない端末で使う（[§12.7](12-ui/07-mobile.md)、[D-115](../decisions/D-115.md)）
- 変換中のシークは該当位置から ffmpeg を再起動（`?start=` → `-ss`）
- 変換結果はキャッシュしない（Derived がそのキャッシュ）
- クライアント能力は起動時に `canPlayType()` で判定し、再生時に URL で選ぶ（サーバへは通知しない）。
  可逆はブラウザが再生できても既定で Derived の Opus（設定「原本」で直送）

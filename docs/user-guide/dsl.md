# スマートプレイリスト DSL

foobar2000 のクエリ構文に寄せた記法。`pest` でパースして AST(JSON) に変換し、
DB に保存する。実行時に AST からパラメータ化 SQL を生成する。

```
%albumartist% IS ヰ世界情緒 AND %verification% IS verified_ctdb
  AND NOT %category% IS _Unsorted
ORDER BY %date% DESC LIMIT 100
```

## 文法

簡略形。正は `src/playlist/dsl.pest`（キーワードは `^"IS"` のように大小文字を無視し、語の途中では切らない）。

```pest
query      = { SOI ~ expr ~ order? ~ limit? ~ EOI }

expr       = { term ~ (or_op ~ term)* }
term       = { factor ~ (and_op ~ factor)* }
factor     = { not_op? ~ (group | compare | presence) }
group      = { "(" ~ expr ~ ")" }

compare    = { field ~ op ~ value }
presence   = { (present_op | missing_op) ~ field }

field      = { "%" ~ ident ~ "%" }
op         = { "IS" | "HAS" | "GREATER" | "LESS" | "MATCHES" }
present_op = { "PRESENT" }
missing_op = { "MISSING" }
and_op     = { "AND" }
or_op      = { "OR" }
not_op     = { "NOT" }

order      = { "ORDER" ~ "BY" ~ (field | "random") ~ ("ASC" | "DESC")? }
limit      = { "LIMIT" ~ integer }

value      = { quoted | bare }
quoted     = @{ "\"" ~ (("\\" ~ ANY) | (!"\"" ~ ANY))* ~ "\"" }
bare       = @{ !"\"" ~ (!(WHITESPACE | ")") ~ ANY)+ }
ident      = { (ASCII_ALPHANUMERIC | "_" | " ")+ }
```

- キーワード（`AND` `IS` など）は大文字小文字を区別しない。`ISLAND` のような語を `IS` + `LAND` とは読まない
- フィールド名は空白を除いて小文字にしてから引く（`%Album Artist%` も `%ALBUMARTIST%` も `albumartist`）
- 値に空白か `)` を含める場合は二重引用符で囲む。引用符の中では `\"` が `"`、`\\` が `\` を表す（それ以外の `\` はそのまま）。
  値に `"` や `\` を含めるときはこのエスケープで書く（スマホ版のルール組み立てもこれで書き出す）
- 演算子の優先順位: `NOT` > `AND` > `OR`。括弧で明示できる
- `ORDER BY` は既定で昇順（`ASC`）。値の無い曲は向きにかかわらず末尾に回る。`ORDER BY random` は無作為順
- `LIMIT` は 1 以上の整数

## 演算子

| 演算子 | 意味 | 備考 |
|---|---|---|
| `IS` | 完全一致 | 文字列は大小文字を区別しない（ASCII の範囲）。数値は等値、`duration` は秒に丸めて比べる、`added` はその日（UTC）に含まれるか |
| `HAS` | 部分一致 | 大小文字を区別しない（ASCII の範囲）。foobar も部分一致（実機で確認。[D-55](../decisions/D-055.md)）。整数のフィールドは数字の文字列として比べる |
| `GREATER` / `LESS` | 数値・日付の比較 | 数値のフィールドと `date`（`YYYY-MM-DD` の文字列順）、`added`（`YYYY-MM-DD` か epoch 秒。`GREATER` はその日の終わりより後、`LESS` はその日の始まりより前）。それ以外（任意タグを含む）はエラー |
| `MATCHES` | 正規表現 | **独自拡張。** foobar には無い。Rust の `fancy-regex` の構文で、大小文字を区別する（`(?i)` で無視） |
| `PRESENT` / `MISSING` | 値の有無 | 値を取らない。任意タグはキーの有無、標準タグの文字列は空文字も無いとみなす。真偽のフィールドは常に PRESENT、端末のフィールドには使えない |

真偽のフィールドは `IS true` / `IS false` だけ（`1` / `0`、`yes` / `no`、`on` / `off` も可）。型に合わない演算子と値は
保存の前にエラーになる（例: `%duration% HAS 3`、`%samplerate% IS abc`）。

## フィールド

### 標準タグ

`title` `artist` `albumartist` `album` `date` `tracknumber`（整数）`discnumber`（整数）は
キャッシュ列を引く。`artist` は多値を `, ` で結合した表示用の値なので、多値のアーティストの 1 つを探すときは `HAS` を使う。

それ以外（`genre` `composer` `comment` `track_type` `playlist_exclude` など）は、ファイルに存在する任意のタグ名として
`track_tags` を引く（キーは大文字にして比べる。多値ならどれか 1 つが一致すればよい）。任意タグは文字列なので
`GREATER` / `LESS` は使えない。

### 拡張フィールド（spindle 固有）

| フィールド | 型 | 値 |
|---|---|---|
| `category` | 文字列 | 統制語彙のカテゴリ名 |
| `verification` | 文字列 | `verified_ar` / `verified_ctdb` / `mismatch` / `unverifiable` / `not_attempted` |
| `source_type`（`sourcetype`） | 文字列 | `cd_rip` / `download` / `youtube` / `unknown` |
| `lossless` | 真偽 | |
| `codec` | 文字列 | `flac` / `opus` / `alac` / `aac` / `mp3` / `wav` / `aiff` / `ogg` / `wv` / `ape` |
| `samplerate` | 数値 | Hz |
| `bitdepth` | 数値 | |
| `channels` | 数値 | |
| `bitrate` | 数値 | kbps |
| `duration` | 数値 | 秒（小数可） |
| `added` | 日付 | 初回スキャン日時。値は `YYYY-MM-DD`（UTC）か epoch 秒 |
| `has_derived`（`hasderived`） | 真偽 | Opus の Derived が生成済みか（AAC だけの曲は偽） |
| `missing` | 真偽 | 論理削除されているか |
| `hirescheck`（`hires_check`） | 文字列 | `ok` / `upsampled` / `padded` / `both` / `inconclusive` / `decode_error`（未検査は MISSING） |
| `cutoff` | 数値 | 偽ハイレゾ検出のカットオフ周波数（Hz） |
| `cliff` | 数値（小数） | 偽ハイレゾ検出のカットオフ前後 1 kHz の落差（dB）。`HAS` / `MATCHES` は不可 |
| `effectivebits`（`effective_bits`） | 数値 | 偽ハイレゾ検出の実効ビット数 |
| `on_device` | 端末名 | その端末に反映済みの写しがある（`device_items` に行がある）。`IS` だけ |
| `device_pending` | 端末名 | その端末のマニフェストに載るが未反映（追加・更新・移動・更新 + 移動）。前回の同期・報告で失敗したものも含む。削除・待ち・エラー（パス衝突などの保留と、反映済みの写しの失敗）は含まない。`IS` だけ |

端末名は大小文字・Unicode の正規化を無視して比べる（`canonical_key`）。知らない端末名は一致なし。
端末を改名すると、旧名を書いたルールは保存し直さなくても一致なし（空）になる（ルールは書き換えない）。
`ORDER BY` にも使えない（400）。
これらを使うスマートプレイリストは端末の選曲に載せられない（循環の禁止。[D-95](../decisions/D-095.md)）。
`device_pending` は差分の計算結果を使うので、ライブラリの変更の直後は古い結果になりうる。

## AST

```json
{
  "where": {
    "op": "and",
    "args": [
      { "op": "cmp", "field": "albumartist", "cmp": "is", "value": "ヰ世界情緒" },
      { "op": "not",
        "args": [{ "op": "cmp", "field": "category", "cmp": "is", "value": "_Unsorted" }] }
    ]
  },
  "order": { "field": "date", "dir": "desc" },
  "limit": 100
}
```

`playlists.rule_ast` に JSON として保存する。`playlists.rule_source` には
入力された DSL の原文をそのまま残す（AST から逆生成すると整形が変わり、
ユーザの記述意図が失われるため）。

## SQL 生成

- **列名は必ずホワイトリスト経由で解決する。** AST のフィールド名を
  そのまま SQL に埋め込んではならない
- **値は必ずバインドパラメータ。** 文字列連結は禁止
- 標準タグのうちキャッシュ列があるもの（`title` `artist_display` `album`
  `albumartist` `track_no` `disc_no` `date`）は `tracks` を直接引く
- それ以外のタグは `EXISTS (SELECT 1 FROM track_tags tt WHERE tt.track_id = t.id
  AND tt.key = ?1 AND tt.value = ?2)` に展開する
- `MATCHES` は SQLite のユーザ定義関数として `regexp` を登録して使う
- `ORDER BY random` は `ORDER BY RANDOM()`
- `missing` の指定がない限り、暗黙に `t.missing_since IS NULL` を付ける

## foobar2000 へのエクスポート

Autoplaylist はファイルとして保存できないため、クエリ文字列を出力して
ユーザが foobar 側に貼り付ける（`GET /api/playlists/:id/fb2k_query` →
`{ query, sort, notes }`。UI はコピーボタン付きのダイアログで見せる）。変換時の注意が 3 点ある。

**1. フィールド名の写像。** foobar はスペース区切りを使う。

| spindle | foobar |
|---|---|
| `albumartist` | `%album artist%` |
| `tracknumber` | `%tracknumber%` |
| `discnumber` | `%discnumber%` |
| `date` | `%date%` |
| `codec` / `samplerate` / `bitrate` / `channels` | `%__codec%` / `%__samplerate%` / `%__bitrate%` / `%__channels%` |
| `bitdepth` | `%__bitspersample%` |
| `duration` | `%length_seconds%`（`PRESENT` / `MISSING` は変換不能） |
| その他の標準タグ・任意タグ | 同名 |

技術情報は生の値を返す `%__…%` を使う。`%channels%` は mono / stereo の表示文字列に
なり、`%channels% IS 2` が当たらない。`%length_seconds%` は技術情報でなく特殊フィールド
なので有無は問えない。

spindle 固有のフィールド（`verification` `category` `source_type` `lossless`
`added` `has_derived` `missing` `hirescheck` `cutoff` `cliff` `effectivebits`
`on_device` `device_pending`）は foobar 側に存在しないため、その項を落として
`notes` に出す。`MATCHES` も同様。落ちて空になった `AND` / `OR` と、子が落ちた
`NOT` も落とす（`OR` の 1 項が落ちると結果は狭まる。`notes` を見て手で直す）。

演算子は `PRESENT %f%` / `MISSING %f%` を foobar の後置形 `%f% PRESENT` / `%f% MISSING` に、
`date` の `GREATER` / `LESS` を `AFTER` / `BEFORE` に写す。値は空白・括弧・`"` を含むか
予約語と同じなら二重引用符で囲む（`"` 自体は foobar 側でエスケープできないので警告）。
複合式の子は常に括弧で囲み、foobar 側の優先順位に依存しない。

**2. `ORDER BY` は分離する。** foobar の Autoplaylist はソートをクエリに書かず、
別欄のタイトルフォーマット文字列で指定する。エクスポートは
「クエリ」と「ソートパターン」の 2 本を出力する。ソートパターンは文字列比較なので、
数値フィールド（`tracknumber` `discnumber` `samplerate` `bitrate` `channels` `bitdepth`
`duration`）は `$num(%__bitrate%,10)` のようにゼロ埋めして桁を揃える。降順は
ソートパターンで表せない。

**3. `LIMIT` と `random` は変換不能。** `notes` に明示する。

```
%albumartist% IS ヰ世界情緒 AND %verification% IS verified_ctdb
  AND NOT %category% IS _Unsorted
ORDER BY %date% DESC LIMIT 100
```

↓

```json
{
  "query": "%album artist% IS ヰ世界情緒",
  "sort": "%date%",
  "notes": [
    "%verification% IS verified_ctdb … spindle 固有フィールド（foobar に無い）",
    "NOT %category% IS _Unsorted … spindle 固有フィールド（foobar に無い）",
    "ORDER BY %date% DESC … ソートパターンでは降順を表せない（foobar 側で並びを反転する）",
    "LIMIT 100 … foobar の Autoplaylist に相当機能なし"
  ]
}
```

`HAS` は foobar でも部分一致（2026-09-19 に実機で確認。[D-55](../decisions/D-055.md)）。

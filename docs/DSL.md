# スマートプレイリスト DSL

foobar2000 のクエリ構文に寄せた記法。`pest` でパースして AST(JSON) に変換し、
DB に保存する。実行時に AST からパラメータ化 SQL を生成する。

```
%albumartist% IS ヰ世界情緒 AND %verification% IS verified_ctdb
  AND NOT %category% IS _Unsorted
ORDER BY %date% DESC LIMIT 100
```

## 文法

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
quoted     = { "\"" ~ (!"\"" ~ ANY)* ~ "\"" }
bare       = { (!(WHITESPACE | ")") ~ ANY)+ }
ident      = { (ASCII_ALPHANUMERIC | "_" | " ")+ }
```

- キーワード（`AND` `IS` など）は大文字小文字を区別しない
- 値に空白を含める場合は二重引用符で囲む
- 演算子の優先順位: `NOT` > `AND` > `OR`。括弧で明示できる

## 演算子

| 演算子 | 意味 | 備考 |
|---|---|---|
| `IS` | 完全一致 | 文字列は大小文字を区別しない |
| `HAS` | 部分一致 | 語境界の扱いは foobar とわずかに異なる可能性がある |
| `GREATER` / `LESS` | 数値・日付の比較 | 対象フィールドが数値型でなければエラー |
| `MATCHES` | 正規表現 | **独自拡張。** foobar には無い |
| `PRESENT` / `MISSING` | タグの有無 | 値を取らない |

## フィールド

### 標準タグ

`title` `artist` `albumartist` `album` `date` `genre` `tracknumber`
`discnumber` `composer` `comment` — およびファイルに存在する任意のタグ名。
ホワイトリストにない名前は `track_tags` への `EXISTS` サブクエリに展開する。

### 拡張フィールド（spindle 固有）

| フィールド | 型 | 値 |
|---|---|---|
| `category` | 文字列 | 統制語彙のカテゴリ名 |
| `verification` | 文字列 | `verified_ar` / `verified_ctdb` / `mismatch` / `unverifiable` / `not_attempted` |
| `source_type` | 文字列 | `cd_rip` / `download` / `youtube` / `unknown` |
| `lossless` | 真偽 | |
| `codec` | 文字列 | `flac` / `opus` / `alac` / `aac` / `mp3` / `wav` |
| `samplerate` | 数値 | Hz |
| `bitdepth` | 数値 | |
| `channels` | 数値 | |
| `bitrate` | 数値 | kbps |
| `duration` | 数値 | 秒 |
| `added` | 日付 | 初回スキャン日時 |
| `has_derived` | 真偽 | Derived が生成済みか |
| `missing` | 真偽 | 論理削除されているか |

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
`added` `has_derived` `missing`）は foobar 側に存在しないため、その項を落として
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

`HAS` の語境界の扱いなど演算子の細部は foobar のバージョンで差があるため、
実装時に実機で一度突き合わせること。

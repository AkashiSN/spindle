## 10. プレイリストとエクスポート

### スマートプレイリスト DSL

foobar2000 風の文法を採用し、`pest` でパースして AST(JSON) を DB に保存、
実行時にパラメータ化 SQL へ変換する。生 SQL を保存しないのは、スキーマ変更で
全ルールが壊れることと、インジェクション面を抱え込むことを避けるため。

```
%albumartist% IS ヰ世界情緒 AND %verification% IS verified_ctdb
  AND NOT %category% IS _Unsorted
ORDER BY %date% DESC LIMIT 100
```

- 演算子: `IS` / `HAS`(部分一致) / `GREATER` / `LESS` / `MATCHES`(正規表現) /
  `MISSING` / `PRESENT`
- 論理: `AND` / `OR` / `NOT` / 括弧
- 拡張フィールド: `verification` `lossless` `codec` `samplerate` `bitdepth`
  `channels` `category` `added` `duration` `has_derived` `missing` `hirescheck` `cutoff` `cliff`
  `effectivebits`、端末の状態を引く `on_device` / `device_pending`（値は端末名、`IS` だけ。端末名はバインドし、
  未反映の集合は JSON 配列をバインドして `json_each` で展開する。これらを使うスマートプレイリストは端末の
  選曲に載せられない。§7.11、docs/DSL.md）
- 独自拡張（foobar に無い）: `MATCHES` / `LIMIT` / `ORDER BY random`
- SQL 生成はホワイトリスト列へのマッピング。任意タグは
  `EXISTS (SELECT 1 FROM track_tags ...)` に展開。値は全てバインドパラメータ
- 評価結果（ORDER BY / LIMIT 適用後）は `playlist_items` に書く（D-54）。表・書き出しは手動と同じ経路。
  ライブラリ変更をトリガに常駐タスクがデバウンス後に再評価する

### エクスポート形式

| 出力先 | 形式 | 内容 |
|---|---|---|
| foobar（静的） | `.m3u8` | 評価結果のトラック一覧。UTF-8 / BOM なし |
| foobar（動的） | クリップボード | **クエリ文字列とソートパターン**。Autoplaylist 作成時に貼る（`.txt` は作らない。D-55） |
| Android | `.m3u8` | 配布ビュー（Derived 優先）で解決したパス。タグ追随待ちの Derived は件数（`stale_tags`）で示す |
| 汎用 | `.pls` | 任意（未実装。`export_profiles.format` に列挙だけしてある） |

`.fpl` は非対応。非公開のバイナリ形式で foobar 1.x と 2.x で構造が異なり、
書き損じると foobar 側の状態を壊す。

### foobar クエリへの変換

1. **フィールド名の写像。** foobar はスペース区切り: `ALBUMARTIST` → `%album artist%`、
   `TRACKNUMBER` → `%tracknumber%`。写像表を持つ（docs/DSL.md）。技術情報（`codec` `samplerate`
   `bitrate` `channels` `bitdepth` `duration`）は foobar の技術フィールドへ、spindle 固有
   （`verification` `category` `source_type` `lossless` `added` `has_derived` `missing` `hirescheck`
   `cutoff` `cliff` `effectivebits` `on_device` `device_pending`）と `MATCHES` は
   変換不能としてその項を落とし `notes` に出す
2. **`ORDER BY` は分離。** foobar の Autoplaylist はソートをクエリに書かず、
   別欄のタイトルフォーマット文字列で指定する。「クエリ」「ソートパターン」の
   2 本を出力する
3. **`LIMIT` と `random`、降順は変換不能。** `notes` に明示する

`HAS` は foobar でも部分一致で、技術情報フィールドの `PRESENT` / `MISSING` も効く
（2026-09-19 に実機で確認。D-55）。

### パスマッピング（エクスポートプロファイル）

NAS 上の `/library/...` をそのまま書いても foobar からは開けない。
出力先ごとにプロファイルを持つ（`export_profiles` テーブル）。3 つは固定で、CRUD の API は
持たない。`foobar` の `prefix` だけ `[export].fb2k_prefix` を正として起動時に揃える（D-55）。

| プロファイル | source | path_style | prefix | sep |
|---|---|---|---|---|
| `foobar` | master (Library) | absolute | `\\TRUENAS\music\` | `\` |
| `android` | delivery (Derived 優先) | relative | — | `/` |
| `internal` | master | relative | — | `/` |

書き出し先は `Playlists/<profile>/<name>.m3u8`（プロファイルごとにディレクトリを分ける。D-53）。
`Playlists/<profile>/` は `Library/` と同じ深さなので、相対パスは `../../Library/...` で
正しく解決される。旧ライブラリから移した `Playlists/m3u8/` は取り込み元として残す。
スマートプレイリストはライブラリ変更をトリガに、
デバウンス（既定 30 秒）を挟んで自動再エクスポートする。

---


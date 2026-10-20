# 7.2 CD 取り込み

```
[ディスク検出]  CDROM_DRIVE_STATUS ioctl を 2 秒間隔ポーリング（`cd/device.rs`）
   ↓            （udev がコンテナに届かないため）。デバイスを開けなければ no_drive として起動は続ける
[TOC 取得]      CDROMREADTOCHDR / CDROMREADTOCENTRY ioctl（LBA 形式。中身は READ TOC コマンドで、
   ↓            /dev/sg* も外部プロセスも要らない）。DiscOk になってから読めるまで毎周回試し、
   ↓            読めたらディスクが抜かれるまで読み直さない。`GET /api/cd/status` が CTDB 形式の
   ↓            文字列で返し、UI は新しいディスクの TOC が出たら照会を自動で始める
   ↓
[ID 算出]       MusicBrainz DiscID / AccurateRip id1,id2 / FreeDB ID
   ↓            すべて TOC からの整数演算。libdiscid FFI 不要
[メタデータ照会] MusicBrainz → 候補提示 → ユーザ確認・手動補正
   ↓            候補は「リリース × medium」（DiscID を持つ medium は exact、トラック数の合う medium は
   ↓            近似。D-64）。**経路は 3 段で、上の段で候補が残れば下は引かない**（D-64 追記 4）:
   ↓
   ↓              段        引くもの                              次の段へ進む条件
   ↓              discid    ws/2/discid/<id>                      404、または 200 でも候補 0 件
   ↓              ids       ISRC（recording 検索）+ MCN = バーコード  候補 0 件
   ↓                        （release 検索）
   ↓              toc       ws/2/discid/<id>?toc=（TOC 近似）      —
   ↓
   ↓            「候補」はトラック数で絞ったあとの数で数える。DiscID が 200 でも候補 0 件なら次の段へ
   ↓            落とすが、exact は真のままにする（DiscID は登録済みなので登録を勧めない）。ユーザが
   ↓            貼ったリリース URL / MBID は段に関係なく常に足し、toc へ進むかの判定では候補に数える
   ↓            （ids は引く）。ユーザが盤の帯・背から読んで入れた品番（`ws/2/release?query=catno:"…"`）も
   ↓            段に関係なく常に引き、カタログ番号が正規化して一致するリリースだけを経路 catno の候補にして
   ↓            先頭に並べる（DiscID は収録が同じ版を区別できないため。D-94）。手入力の JAN は盤の MCN が
   ↓            無いときだけ ids の段のバーコード検索に使う。
   ↓            同じ medium は 1 件に束ねて経路（matched_by）を付ける（D-64 追記）。
   ↓            TOC 近似はトラック長の近い別の盤を大量に返すので最後の手段。応答の stage / can_widen で
   ↓            画面に段を見せ、「さらに広げて探す」（widen）で明示的に下の段まで引ける。候補を選んだら DiscID の登録リンク
   ↓            ※同人・VTuber・インディーズ国内盤は MusicBrainz 未登録が常態。
   ↓              照会結果ゼロでもウィザードが完走できることを必須要件とする。
   ↓              候補を写す先は 1 つのフォーム（D-65。写す範囲は既定で「全部写す」、「識別用の
   ↓              最小限」を選べる。D-72 追記 2、P4-2 / P4-20）。**CD 画面では直せない**（D-67 追記。
   ↓              補正とトラックリスト貼り付けは Inbox の承認画面）。例外は手入力の品番と JAN（D-94）で、
   ↓              写す範囲に関わらず下書きに重ねる。品番が選んだ候補と食い違えば（手元の版が MusicBrainz に
   ↓              無い）品番は入力値にし、候補の JAN は写さない。MB の id は近い版を指す値として残す。これが
   ↓              DiscMetadata（album / album_artist / date / label / catalog_number / barcode /
   ↓              disc_no / disc_count / tracks[{ number, title, artist, mb }]、source）になる。
   ↓              トラックリスト貼り付け（通販ページ等からのテキストを行解析して
   ↓              トラック番号・タイトル・アーティストへ割り付け。web/src/lib/tracklist.ts）は
   ↓              **Inbox の承認画面の**一級の入力経路とする（D-67 追記。CD 画面からは外した）。
   ↓              行は TOC の音声トラックと 1:1 で、番号と長さは TOC から
   ↓
[オフセット決定] INQUIRY でドライブ型番取得 → 学習済みの値（drive_offsets）→ AccurateRip の
   ↓            ドライブ表（DriveOffsets.bin を実行時に取得・保存）→ 0（D-83 / 追記 2）
   ↓            `[rip].drive_offset` の整数で上書き可。照合で見つかったずれは PCM に当てて学習する
[吸い出し]      cd-paranoia -e -r -d <dev> -- <first>-<last>[mm:ss.ff] で音声トラック全体を 1 本の PCM に
   ↓            （オフセット 0 で読み、範囲は TOC の長さで明示。`cd::rip::read_disc`。D-83）
   ↓            ※トラック単位で吸うとオフセット補正が境界をまたげない
[オフセット適用 → トラック分割]
   ↓
[CRC 計算]      ARv1 / ARv2 / CTDB CRC32
   ↓            先頭トラック冒頭・末尾トラック終端の除外規則を厳守
[照合]          CTDB を主、AccurateRip を補助
   ↓
[エンコード]    FLAC (master) を **Inbox へ**（D-67 追記。Library へ直行しない）。
   ↓            tmp の PCM は検証完了まで保持
[ログ出力]      rip.log / disc.cue / disc.toc を取り込みのディレクトリへ。MusicBrainz から写した内容と
   ↓            RipReport（照合結果）は サイドカー spindle-inbox.json に書く（D-70 の仕組み）
[承認]          Inbox の承認画面で名前と category を直して「承認して配置」
   ↓            → 配置が Library へ運び、source_type = cd_rip と検証記録を入れる
[後続ジョブ投入] rg（album 単位。配置した album は album gain on。D-74）→ transcode(Derived) → thumbnail
```

Inbox への配置（`src/cd/place.rs` の `place_disc`、[D-67](../../decisions/D-067.md) 追記、P2-5）: rip ジョブの最終段。
入力は `Toc`、吸い出しを始めたときの `DiscMetadata`（**名前は空でもよい**。`validate` は TOC との
行の対応・日付・ディスク番号・category だけを見る。必須の検証は Inbox の承認が担う）、オフセット
適用済みの tmp の PCM、`RipReport`。PCM を `TrackLayout` で切り、raw のまま `flac -N --verify` で
エンコード（トラックごとの PCM の MD5 と STREAMINFO の MD5 が一致するときだけ成果物）、タグは
下書きの写像（[D-65](../../decisions/D-065.md)）+ `TRACKTOTAL` / `MUSICBRAINZ_DISCID`。空のタイトルは `Track NN`、空の名前は
タグに書かない。`category` はタグに書かず（パス専用。[SPEC §5](../05-library-layout.md)）サイドカーで渡す。

- **組み立ててから公開する。** Inbox 直下の隠しディレクトリ `.spindle-rip-<DiscID>` に `NN.flac`
  （TOC の番号。名前での対応付けの鍵なので承認で変わる値を入れない）、同梱ファイル、サイドカー
  （`category` と `rip`。[§7.8](08-inbox.md)）を置き、`CD/<albumartist - album> [<DiscID>]`（名前が空なら
  `CD/[<DiscID>]`。DiscID は `.` で始まり得るので括弧で包む）へディレクトリごと `RENAME_NOREPLACE`。
  走査は `.` で始まるディレクトリを見ないので、揃う前の盤が取り込みとして見えて承認されることはない。
  公開したら `inbox` ジョブを投入する。複数枚組は DiscID が違うのでディスクごとに別の取り込みになる
- **冪等性。** 公開先が既にあり、全トラックの STREAMINFO の MD5 が自分の PCM と一致し、サイドカーの
  記録が同じファイル名を持てば自分の成果物（公開の後に落ちた再実行）として組み立てを飛ばす。違えば
  衝突。前の実行の組み立ての残骸は消して作り直し、公開が衝突したら組み立てたものを消す
- `albums` / `tracks` / 検証記録の登録、パスの計画（`[layout]`）、`library` の排他、`rg` / `transcode` の
  投入は承認後の Inbox の配置（[§7.8](08-inbox.md)）が行う

同梱ファイルは 1 枚なら `disc.cue` /
`disc.toc` / `rip.log`、複数枚組は `disc<N>.cue` / `disc<N>.toc` / `rip<N>.log`。`disc.cue` は
EAC 流の複数ファイル cue（ギャップは前トラック末尾。INDEX 00 は書かない）、`disc.toc` は `Toc` から
cdrdao 構文で生成、`rip.log` は先頭行 `spindle rip log v1` の自前形式でドライブ・オフセット・
トラックごとの CRC と照合結果を持つ。トラック表の「ずれ」は、cd-paranoia が読み取り位置のずれ（ドライブの
ジッター）を検出・補正した回数（drift / dropped / duped。`TrackRead.slips`。補正の通知なので回数だけでは誤りを
意味しないが、多発と照合の不一致が併発した）。1 回でもあれば表の下に合計と
意味を、試行ごとの合計（`RipReport.attempt_slips`）を「試行ごとのずれ」に出す。照合が通らずずれがある取り込みは、
Inbox の承認画面の警告にも出る（P2-5 の調査。TASKS）。スキャナはこの rip.log のあるディレクトリで新規に登録する行を
`cd_rip` にする（DB を消しても出自が戻る。検証は [§7.3](03-retro-verify.md) で付け直す）。

同梱ファイル（cover / disc.cue / disc.toc / rip.log）は album 全体の一括リネームに追随する: rename
ジョブが phase 2 の後、commit トランザクションの中で既知の名前のファイルを新ディレクトリへ移し、空になった
旧ディレクトリを消す（[D-67](../../decisions/D-067.md)。`edits` には記録せず、巻き戻しは逆向きの移動で戻る）。

不一致時の既定動作: 自動再リップ（最大2回）→ CTDB 修復データ適用 →
それでも不一致なら `mismatch` フラグ付きで取り込み、UI で要確認表示。

CTDB の修復（`src/cd/repair.rs`、[D-66](../../decisions/D-066.md)）: ディスクの 16 bit 語を 11760 語ずつの行に並べ、列ごとに
GF(2^16) の Reed-Solomon 符号語とみなす（データ行は先頭 1 行と末尾 1 行 + 端数を除く。CTDB の
ディスク CRC の範囲と同じ）。1 回目の走査でシンドローム表（`SyndromeSampler` → `SyndromeTable`。
80 分で 3〜5 秒）を作り、エントリの `syndrome`（列 0）でオフセットを探す（`find_offset`、±2939）。
誤りがあれば `hasparity` のパリティファイル（各列のシンドローム。`CtdbClient::fetch_syndromes` が
Range で先頭 npar 面だけ取る）と突き合わせ、列ごとに Berlekamp-Massey → Chien → Forney で位置と値を
出す（列あたり npar/2 個まで。1 セクタ丸ごとの傷は 1176 列に 1 個ずつなので直る）。直した後の
ディスク CRC がエントリの値に一致するときだけ計画（`RepairPlan`）を採用し、2 回目の走査で
`RepairApplier` が語を XOR する。直せなければ再リップ / `mismatch` へ。吸い出しジョブへの配線は
P2-5 で行う

**ドライブは物理的に1台なので rip キューの並列度は 1 に固定する。**

**進捗**（P4-20 で形を決め、P2-5 で確定）: rip ジョブは SSE の `job` イベントの `detail` に `RipProgress`
（`src/cd/rip.rs`。DB には書かない）を流す。

```
{ "phase": "read|verify|repair|encode|place", "attempt": 1, "disc_no": 1, "track_no": 3 | null,
  "done": 2, "total": 12 }
```

- `phase` は相。`read` はドライブから全ディスクを 1 本で読む（`done` / `total` はセクタ、`track_no` は読んでいる
  位置のトラック）、`verify` は CRC を取って照合（バイト。トラックに分かれない）、`repair` は CTDB のパリティで直す
  （バイト）、`encode` は FLAC にする（トラック。`track_no` は**エンコードし終えた**トラックで、`done` が 0 の最初の
  報告はまだどれも終えていない）、`place` は Inbox に置く
- `attempt` は何回目の吸い出しか（1 始まり。照合が通らず吸い直すと `read` から増える）。`disc_no` は複数枚組の
  ディスク番号（`rip<N>.log` / `disc<N>.cue` と揃える。[D-67](../../decisions/D-067.md)）
- CD 画面のトラック表（[§12.6](../12-ui/06-other-screens.md)）は進捗が来ている間だけ右端に `進捗` 列を出す（`lib/cdRip.ts`）。
  読み取り中は読み終えた行に「読んだ」・読んでいる行に「読み取り中」、照合・修復は全行に「照合中」/「修復中」、
  エンコードは終えた行に「FLAC 済」・次の行に「エンコード中」、置いたら全行に「Inbox へ」。状態の一行は
  「読み取り 45%」「エンコード 60% 3/5 曲」の形で、2 回目以降は吸い直している旨を添える

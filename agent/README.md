# spindle-agent

Mac のミュージック.app を、spindle サーバの計画どおりに同期する小さなコマンドです。spindle の「端末」タブで選んだ曲とプレイリストを、`~/Music/spindle` とミュージック.app の「spindle」フォルダへ写します。そのあと Finder で iPhone を同期すれば、選んだプレイリストが iPhone に入ります。

ファイルが正で、サーバの状態はキャッシュです。エージェントは自分が置いた曲（state に記録したもの）だけを触り、管理外のファイルやプレイリストには手を出しません。

## 動作環境

- macOS（Apple Silicon、arm64）
- ミュージック.app
- spindle サーバに届く LAN。`https://` が基本です。LAN 内で https にできないときだけ `--insecure-http` を付けます

署名・公証はしていません。導入のときに隔離属性を外します（下記）。

## 導入

GitHub の Release から `spindle-agent-<版>-aarch64-apple-darwin.tar.gz` と `.sha256` を落とします。

```sh
shasum -a 256 -c spindle-agent-*-aarch64-apple-darwin.tar.gz.sha256
tar xzf spindle-agent-*-aarch64-apple-darwin.tar.gz
cd spindle-agent-*-aarch64-apple-darwin
xattr -d com.apple.quarantine spindle-agent
mkdir -p ~/.local/bin && cp spindle-agent ~/.local/bin/
```

`~/.local/bin` が PATH に無ければ、フルパスで呼ぶか PATH に足してください。

## 前提

- ミュージック > 設定 > ファイル の「ファイルを［ミュージック］フォルダにコピー」を**オフ**にする。ON のままだと、ミュージック.app が曲を複製してしまい、spindle が置いた 1 ファイルと対応しなくなります。この設定は外から読めないため、エージェントは `add` の後で複製に気づいて止まります
- ミュージックの「メディアフォルダの場所」を `~/Music/spindle` にも、それを含むフォルダ（`~/Music` など）にもしない。メディアフォルダの中の曲は、削除するとファイルまで消え、コピー設定が ON でも複製されません。エージェントは pair と sync の最初に、root とその祖先のフォルダを検査して止まります
- 初回の操作でミュージック.app への「オートメーション」の許可を求められたら許可する。pair の最初に読み取りだけの操作をして、ここで許可を済ませます

## 使い方

1. spindle の端末タブで「iPhone（Mac 経由）」の端末の「pair コードを発行」を押す。発行すると、その Mac が今持っているトークンは使えなくなります（pair し直すまで sync できません）。同期の計画が途中の間は押せません
2. ペアリングする

   ```sh
   spindle-agent pair https://spindle.example.com <コード>
   ```

   LAN 内の http のサーバなら `--insecure-http` を付けます。トークンは Keychain に保存されます。コードは 1 回限りなので、エージェントはコードを使う前に、トークンを保存できるか（Keychain に書けるか）を確かめます。`SPINDLE_AGENT_SECRETS=file` で pair したら、以後の `sync` などにも同じ指定を付けてください（指定が違うとトークンを見つけられません）
3. 同期する

   ```sh
   spindle-agent sync
   ```

   サーバの計画と差分を表示します。確認すると、`~/Music/spindle` に曲を置き、ミュージック.app の「spindle」フォルダにプレイリストを作ります

   実行中は標準エラーに進捗を 1 行で出します（`[12/92] 85.3 MB / 634.8 MB  12.4 MB/s  残り 0:42  受信中: …`）。端末でなければ 10 秒か 10 件ごとに 1 行ずつ出し、終わると要約を 1 行出します。同じ進捗は spindle の端末タブにも出ます。サーバへの送信に失敗しても同期は止まらず（間隔を空けて送り直し、続けて失敗すればその回は送るのをやめます）、古いサーバでは送らないだけです
4. Finder で iPhone を同期し、ミュージックの「spindle」フォルダのプレイリストを選ぶ

選曲を変えたときは、端末タブで変えてからもう一度 `sync` します。

### サーバの DB を失った後（`pair --adopt`）

spindle の DB を失って作り直したとき（端末の登録が消え、Library を走査し直したとき）は、この Mac の `~/Music/spindle` とミュージック.app の track を残したまま、新しい登録へ引き継げます。

1. 端末タブで「iPhone（Mac 経由）」の端末を新しく作り、「pair コードを発行」を押す
2. 引き継ぐ

   ```sh
   spindle-agent pair --adopt https://spindle.example.com <コード>
   ```

   前の登録の state（`~/Library/Application Support/spindle-agent`）と root の `.spindle-device` がそのまま要ります。コードを使う前に、前の登録があること・root の印が一致すること・途中の操作が残っていないことを確かめます。サーバの行を前の登録と同じ端末として扱わせ、state の曲を Library の `rel_path` で、プレイリストを名前で新しい id へ付け替えます
3. いつもどおり `sync` する。中身が同じ曲は受け取り直さず、ミュージック.app の track（再生回数など）もそのまま使います。プレイリストは同じ名前のまま中身を入れ直します

新しい Library で見つからなかった曲・プレイリストは管理外になります。ファイルもミュージック.app の track も消しませんが、以後エージェントは触りません（要らなければ手で消してください）。途中で止まっても、次の `sync` か `pair --adopt` のやり直しが続きを行います。DB を失っていない（端末タブに今の Mac の登録が残っている）ときは `--adopt` を付けずに pair します

### その他のコマンド

```
spindle-agent pair --adopt <URL> <コード> [--insecure-http]
spindle-agent status
spindle-agent resolve <op_id> (--delete-track <persistent_id> | --no-copy-created)
spindle-agent abandon
```

- `pair --adopt`: サーバの DB を失った後、この Mac の保存先を新しい登録へ引き継ぎます（上記）
- `status`: 登録状況と、保留中の操作を表示します
- `resolve`: 保留した追加の操作を解決します。`sync` が表示した候補を見て、`--delete-track` でその曲を spindle の複製として消すか、複製は作られなかったときに `--no-copy-created` で add の意図を捨てます
- `abandon`: 開いている計画を破棄します。保留中の操作が無いときだけ送れます

## 置き場所

| もの | 場所 |
|---|---|
| state | `~/Library/Application Support/spindle-agent` |
| トークン | Keychain（サービス `spindle-agent`、アカウント `token`） |
| root（曲の置き場） | `~/Music/spindle`（起動時にシンボリックリンクを解いたパスにする） |

root の下に `.spindle-device` と、バッチ中だけ `.moving/` ができます。root の中を手で並べ替えないでください。

ミュージック.app は曲の場所をシンボリックリンクを解いたパスで持つので、エージェントも root（とまだ無い部分の手前の、在る祖先）をシンボリックリンクを解いて扱います。`~/Music` や `~/Music/spindle` 自体がリンクでも構いません。root の**中**のリンクは辿りません（下記）。起動した後で root やその途中のフォルダがリンクに差し替えられたときは止まります。

## 困ったとき

- **コピー設定が ON で止まった**: 設定 > ファイル で「ファイルを［ミュージック］フォルダにコピー」を切り、`sync` し直します。できてしまった複製の track は、通常の削除経路でエージェントが消してから止まります。メディアフォルダ側に残った複製ファイルは、ミュージック.app で確認して消してください
- **重複で止まった**（「管理外と衝突」、または移動の行き先に管理外のファイルがある）: エージェントは管理外のファイルを上書きしません。該当のファイルを別の場所へ移してから `sync` し直します。追加の途中で止まったときは `sync` が候補を表示するので、案内に従って `resolve` します
- **「上書きしない rename」に対応していないと言われた**: `~/Music/spindle` が一部のネットワーク共有などの、上書きしない rename に対応しないファイルシステムにあります。管理外のファイルを上書きしないことを保証できないので止まります。Mac の内蔵ディスクなどのローカルのボリューム（APFS）へ移してください
- **symlink で止まった**: root の下のシンボリックリンクは辿りません。`~/Music/spindle` の中のリンクを実体に置き換えるか外してください
- **メディアフォルダで止まった**: ミュージックのメディアフォルダが `~/Music/spindle` か、それを含むフォルダ（`~/Music` など）になっています。ミュージック > 設定 > ファイル の「ミュージックメディアフォルダの場所」を `~/Music/spindle` を含まない別のフォルダに変えてから、やり直します
- **ssh 越しで Keychain が使えない**（`User interaction is not allowed`）: ssh のセッションではログイン Keychain に触れません。Mac の Terminal.app で実行するか、環境変数 `SPINDLE_AGENT_SECRETS=file` を付けてトークンを state のディレクトリのファイル（0600）に置きます。pair はコードを使う前にこれを確かめて止まるので、同じコードでやり直せます。`SPINDLE_AGENT_SECRETS=file` は pair の後も毎回付けてください
- **「トークンがありません」で止まった**: pair をまだしていないか、pair のときと `SPINDLE_AGENT_SECRETS` の指定が違います。同じ指定で実行するか、pair し直します
- **ミュージック.app が応答しない**: `osascript` は 1 回 120 秒で打ち切ります（プレイリストの中身の入れ替えだけは曲数に応じて延ばします。1 曲あたり 100 ms）。ミュージック.app を起動して落ち着いてから `sync` し直します。途中で止まっても、次の `sync` が回復して続きを行います

環境変数 `SPINDLE_AGENT_STATE_DIR` / `SPINDLE_AGENT_ROOT` で state と root の場所を変えられます（試験用）。

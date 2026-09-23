#!/bin/bash
# テスト用の偽の cd-paranoia（tests/cd_rip.rs）。テストが書いてから実行すると、並列の別テストが fork した
# 子が書き込みの FD を持ったままで ETXTBSY になり得るので、実行するファイルはリポジトリに置いて固定し、
# 振る舞いは出力先と同じディレクトリの fake.conf（BYTES / CODE）と fake.stderr から読む。
# 引数は同じディレクトリの args に 1 行ずつ残す
out="${@: -1}"
dir="$(dirname -- "$out")"
# shellcheck source=/dev/null
. "$dir/fake.conf"
printf '%s\n' "$@" > "$dir/args"
head -c "$BYTES" /dev/zero > "$out"
cat "$dir/fake.stderr" >&2
exit "$CODE"

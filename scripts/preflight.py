#!/usr/bin/env python3
"""
spindle — 移行前チェック

casesensitivity=insensitive / normalization=formD / utf8only=on の
データセットへコピーする前に、コピーが失敗する（または黙って上書きされる）
条件を検出する。

  usage:
    ./preflight.py /mnt/ssd/musics/Opus
    ./preflight.py /mnt/ssd/musics/Opus --dest /mnt/ssd --plan rename.sh
    ./preflight.py /mnt/ssd/musics/Original --manifest before.tsv --full
    ./preflight.py /mnt/ssd/media/Library --manifest after.tsv --audio-only

  exit code:
    0  ブロッカーなし（移行して良い）
    1  ブロッカーあり（コピー前に解消が必要）
    2  実行エラー
"""

from __future__ import annotations

import argparse
import collections
import json
import os
import shlex
import sys
import unicodedata

# ZFS の 1 コンポーネント上限（バイト）
NAME_MAX = 255

# Windows/SMB クライアントが扱えない文字
WIN_FORBIDDEN = set('"*:<>?|\\')
WIN_RESERVED = {
    "CON", "PRN", "AUX", "NUL",
    *(f"COM{i}" for i in range(1, 10)),
    *(f"LPT{i}" for i in range(1, 10)),
}

AUDIO_EXT = {".flac", ".opus", ".m4a", ".mp3", ".wav", ".aac", ".ogg",
             ".alac", ".aiff", ".aif", ".wv", ".ape", ".webm", ".mka"}


def collision_key(name: str) -> str:
    """
    ZFS の比較キーを再現する。
    normalization=formD は比較時に NFD 正規化し、
    casesensitivity=insensitive はさらに大小文字を畳む。
    この値が一致する 2 ファイルは同一データセット内に共存できない。
    """
    return unicodedata.normalize("NFD", name).casefold()


class Report:
    def __init__(self) -> None:
        # ブロッカー: これがあるとコピーが失敗するか上書きされる、
        # spindle の同一性解決（1 トラック = 1 ファイル、(dev,inode)）が成立しない、
        # またはスキャナが対象外にするため移行後にトラック数が一致しない
        self.collisions: list[dict] = []
        self.invalid_utf8: list[str] = []
        self.too_long: list[tuple[str, int]] = []
        self.symlinks: list[str] = []      # spindle は symlink を辿らない → 消えたように見える
        self.hardlinks: list[str] = []     # 同じ inode を 2 パスが共有 → タグ書き込みが他方を壊す（D-26）
        self.win_unsafe: list[tuple[str, str]] = []   # SMB 禁止文字。スキャナが対象外にする
        self.trailing: list[str] = []                 # 末尾ドット/スペース。同上
        self.reserved: list[str] = []                 # Windows 予約名。同上
        self.special: list[str] = []                  # 通常ファイルでない。rsync が飛ばす
        self.unreadable: list[tuple[str, str]] = []   # 読めない subtree は判定そのものが欠ける
        self.dest_insufficient: bool = False          # --dest の空きが不足
        # 統計
        self.files = 0
        self.dirs = 0
        self.total_bytes = 0
        self.ext = collections.Counter()
        self.ext_bytes = collections.Counter()
        self.nfc = 0
        self.nfd = 0
        self.manifest: list[tuple[str, int, int, str]] = []  # (rel, size, mtime, ext)

    @property
    def blocking(self) -> int:
        return (len(self.collisions) + len(self.invalid_utf8) + len(self.too_long)
                + len(self.symlinks) + len(self.hardlinks)
                + len(self.win_unsafe) + len(self.trailing) + len(self.reserved)
                + len(self.special) + len(self.unreadable)
                + (1 if self.dest_insufficient else 0))


def scan(root: bytes, rep: Report, collect_manifest: bool) -> None:
    stack = [root]
    while stack:
        d = stack.pop()
        try:
            entries = list(os.scandir(d))
        except OSError as e:
            rep.unreadable.append((os.fsdecode(d), str(e)))
            continue

        rep.dirs += 1
        groups: dict[str, list[str]] = collections.defaultdict(list)

        for e in entries:
            raw: bytes = e.name  # bytes（root が bytes のため）
            try:
                name = raw.decode("utf-8")
                valid = True
            except UnicodeDecodeError:
                name = raw.decode("utf-8", "replace")
                valid = False

            disp = os.path.join(os.fsdecode(d), name)

            if not valid:
                # utf8only=on のデータセットには作成できない
                rep.invalid_utf8.append(disp)
                continue

            if len(raw) > NAME_MAX:
                rep.too_long.append((disp, len(raw)))

            groups[collision_key(name)].append(name)

            # 正規化形の分布（情報）
            if name != unicodedata.normalize("NFC", name):
                rep.nfd += 1
            else:
                rep.nfc += 1

            bad = sorted(WIN_FORBIDDEN & set(name))
            if bad:
                rep.win_unsafe.append((disp, "".join(bad)))
            # Windows は末尾のスペース・ドットを落とすため、
            # ディレクトリでも同じ問題が起きる（むしろこちらが多い）
            if name != name.rstrip(" ."):
                rep.trailing.append(disp)
            stem = name.split(".")[0].upper()
            if stem in WIN_RESERVED:
                rep.reserved.append(disp)

            try:
                if e.is_symlink():
                    rep.symlinks.append(disp)
                    continue
                st = e.stat(follow_symlinks=False)
                if e.is_dir(follow_symlinks=False):
                    stack.append(e.path)
                elif e.is_file(follow_symlinks=False):
                    rep.files += 1
                    rep.total_bytes += st.st_size
                    ext = os.path.splitext(name)[1].lower()
                    rep.ext[ext] += 1
                    rep.ext_bytes[ext] += st.st_size
                    if st.st_nlink > 1:
                        rep.hardlinks.append(disp)
                    if collect_manifest:
                        rel = os.path.relpath(disp, os.fsdecode(root))
                        rep.manifest.append((rel, st.st_size, int(st.st_mtime), ext))
                else:
                    rep.special.append(disp)
            except OSError as err:
                rep.unreadable.append((disp, str(err)))

        for key, names in groups.items():
            if len(names) > 1:
                rep.collisions.append({
                    "dir": os.fsdecode(d),
                    "names": sorted(names),
                    "reason": classify(names),
                })


def classify(names: list[str]) -> str:
    # 同一ディレクトリ内に完全一致の名前は存在しないため、
    # 「NFD 正規化すると一致する」= 正規化形の違いだけ、と判定できる。
    if len({unicodedata.normalize("NFD", n) for n in names}) == 1:
        return "unicode"             # NFC / NFD の違い
    if len({n.casefold() for n in names}) == 1:
        return "case"                # 大小文字のみ違う
    return "case+unicode"


def human(n: int) -> str:
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if abs(n) < 1024:
            return f"{n:.1f}{unit}"
        n /= 1024
    return f"{n:.1f}PiB"


def write_plan(rep: Report, path: str) -> None:
    """衝突を解消するリネーム案。必ず目視してから実行すること。"""
    lines = [
        "#!/bin/bash",
        "# spindle preflight — 衝突解消のリネーム案",
        "# 自動生成。どちらを残すかは人間が判断すること。",
        "# 中身が同一ファイルの重複なら、リネームではなく削除が正しい。",
        "set -euo pipefail",
        "",
    ]
    for c in rep.collisions:
        lines.append(f"# {c['dir']}  [{c['reason']}]")
        if c["reason"] != "case":
            # NFC/NFD の差は画面上で完全に同じに見える。
            # 人間が判別できるようエスケープ表記を併記する。
            lines.append("#   ↓ 見た目は同一だが別ファイル（正規化形が違う）")
            for n in c["names"]:
                lines.append(f"#     {n.encode('unicode_escape').decode()}")
        keep, *rest = c["names"]
        lines.append(f"#   keep: {keep}")
        for i, n in enumerate(rest, 1):
            src = os.path.join(c["dir"], n)
            stem, ext = os.path.splitext(n)
            dst = os.path.join(c["dir"], f"{stem}__dup{i}{ext}")
            lines.append(f"mv -n -- {shlex.quote(src)} {shlex.quote(dst)}")
        lines.append("")
    with open(path, "w") as f:
        f.write("\n".join(lines))
    os.chmod(path, 0o755)


def show(title: str, items, limit: int, fmt=str) -> None:
    if not items:
        return
    print(f"\n  {title}: {len(items)}")
    for x in items[:limit]:
        print(f"    {fmt(x)}")
    if len(items) > limit:
        print(f"    ... 他 {len(items) - limit} 件（--full で全件表示）")


def main() -> int:
    ap = argparse.ArgumentParser(description="spindle 移行前チェック")
    ap.add_argument("source", help="旧ライブラリのパス")
    ap.add_argument("--dest", help="コピー先。空き容量を確認する")
    ap.add_argument("--plan", help="衝突解消のリネーム案を書き出す")
    ap.add_argument("--manifest", help="移行後の照合用マニフェスト(TSV)")
    ap.add_argument("--audio-only", action="store_true",
                    help="マニフェストを音声ファイル(AUDIO_EXT)に限定する。DB のトラック数との照合用")
    ap.add_argument("--json", dest="json_out", help="結果を JSON で書き出す")
    ap.add_argument("--full", action="store_true", help="全件表示")
    args = ap.parse_args()

    if not os.path.isdir(args.source):
        print(f"error: ディレクトリがありません: {args.source}", file=sys.stderr)
        return 2

    rep = Report()
    print(f"走査中: {args.source} ...", file=sys.stderr)
    scan(os.fsencode(os.path.abspath(args.source)), rep, bool(args.manifest))

    limit = 10**9 if args.full else 20
    print("=" * 68)
    print("spindle 移行前チェック")
    print("=" * 68)
    print(f"\n対象      : {os.path.abspath(args.source)}")
    print(f"ファイル  : {rep.files:,}  /  ディレクトリ: {rep.dirs:,}")
    print(f"合計容量  : {human(rep.total_bytes)}")
    print(f"正規化形  : NFC {rep.nfc:,} / NFD {rep.nfd:,}")

    print("\n拡張子別:")
    for ext, n in rep.ext.most_common(12):
        tag = " *" if ext in AUDIO_EXT else "  "
        print(f"  {tag}{ext or '(なし)':<10} {n:>8,}  {human(rep.ext_bytes[ext]):>10}")

    print("\n" + "-" * 68)
    print("ブロッカー（コピー前に解消が必要）")
    print("-" * 68)

    show("名前の衝突（insensitive/formD で共存不可）", rep.collisions, limit,
         lambda c: f"[{c['reason']}] {c['dir']}\n      " + "\n      ".join(c["names"]))
    show("不正な UTF-8 名（utf8only=on で作成不可）", rep.invalid_utf8, limit)
    show(f"名前が {NAME_MAX} バイト超", rep.too_long, limit,
         lambda t: f"{t[1]}B  {t[0]}")
    show("シンボリックリンク（spindle は辿らない。実体に置き換えるか除外する）",
         rep.symlinks, limit)
    show("ハードリンク（同じ inode を複数パスが共有。片方を実コピーにするか削除する）",
         rep.hardlinks, limit)
    show("SMB で使えない文字（spindle のスキャナが対象外にする）", rep.win_unsafe, limit,
         lambda t: f"[{t[1]}] {t[0]}")
    show("末尾がスペースまたはドット（同上）", rep.trailing, limit)
    show("Windows 予約名（同上）", rep.reserved, limit)
    show("通常ファイルでない（rsync が飛ばす）", rep.special, limit)
    show("読み取れない（未走査の subtree があると判定が欠ける）", rep.unreadable, limit,
         lambda t: f"{t[0]}: {t[1]}")

    if args.dest:
        print("\n" + "-" * 68)
        try:
            st = os.statvfs(args.dest)
            free = st.f_bavail * st.f_frsize
            print(f"空き容量: {human(free)}  /  必要: {human(rep.total_bytes)}")
            if free < rep.total_bytes:
                print("  不足しています。コピーは完走しません（ブロッカー）")
                rep.dest_insufficient = True
            elif free < rep.total_bytes * 1.1:
                print("  余裕がありません。スナップショット分を見込むこと")
            else:
                print("  十分です")
        except OSError as e:
            print(f"コピー先を確認できません: {e}（ブロッカー）")
            rep.dest_insufficient = True

    if rep.blocking == 0:
        print("\n  なし")

    if args.plan and rep.collisions:
        write_plan(rep, args.plan)
        print(f"\nリネーム案を書き出しました: {args.plan}")
        print("  そのまま実行せず、必ず中身を確認すること。")
        print("  中身が同一の重複なら、リネームではなく削除が正しい。")

    if args.manifest:
        rows = [m for m in rep.manifest if not args.audio_only or m[3] in AUDIO_EXT]
        with open(args.manifest, "w") as f:
            f.write("path\tsize\tmtime\n")
            for rel, size, mtime, _ext in sorted(rows):
                f.write(f"{rel}\t{size}\t{mtime}\n")
        kind = "音声ファイルのみ" if args.audio_only else "全ファイル"
        print(f"\nマニフェストを書き出しました: {args.manifest}  ({kind} {len(rows):,} 行)")
        print("  移行後に同じコマンドをコピー先で実行し、diff で照合する。")
        if args.audio_only:
            print("  この行数が初回スキャン後の DB トラック数と一致すること。")

    if args.json_out:
        with open(args.json_out, "w") as f:
            json.dump({
                "source": os.path.abspath(args.source),
                "files": rep.files, "dirs": rep.dirs,
                "total_bytes": rep.total_bytes,
                "nfc": rep.nfc, "nfd": rep.nfd,
                "ext": dict(rep.ext),
                "collisions": rep.collisions,
                "invalid_utf8": rep.invalid_utf8,
                "too_long": rep.too_long,
                "win_unsafe": rep.win_unsafe,
                "trailing": rep.trailing,
                "reserved": rep.reserved,
                "symlinks": rep.symlinks,
                "hardlinks": rep.hardlinks,
                "special": rep.special,
                "unreadable": rep.unreadable,
            }, f, ensure_ascii=False, indent=2)
        print(f"JSON を書き出しました: {args.json_out}")

    print("\n" + "=" * 68)
    if rep.blocking:
        print(f"判定: 移行不可。ブロッカー {rep.blocking} 件を解消してください")
        print("=" * 68)
        return 1
    print("判定: 移行可。rsync に進めます")
    print("=" * 68)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(2)

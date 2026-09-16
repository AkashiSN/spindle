#!/usr/bin/env python3
"""
spindle — 移行の振り分け計画（docs/MIGRATION.md §1、docs/DECISIONS.md D-45）

旧ライブラリ（`Opus/` と `Original/` が 1:1 で対になっている構成）を、役割別の
Library / Derived / Archive に振り分ける rsync 用のファイル一覧を作る。
判定は「Original 側の原本の形式」で行う:

  Original の原本          Library（master）            Archive
  ------------------------ ---------------------------- ------------------
  ロスレス (m4a=ALAC/flac) Original の原本              —
  webm（YouTube 生データ） Opus/ の .opus（remux）      Original の .webm
  非可逆原本 (mp3 等)      Original の原本              —
  aiff + 同名の m4a        m4a（整数 PCM）              .aiff

  対応する Opus/ の .opus は、ロスレス由来なら Derived で再生成するので移さない。
  非可逆原本由来のものは非可逆→非可逆の世代劣化品なので捨てる。
  `.exclude` / `.noimage` などの旧パイプラインのマーカー、`AAC/` ディレクトリ、
  foobar の `.fpl` は移さない。画像（png / jpg）は Library へ同じ相対パスで移す。

出力（--out ディレクトリ、すべて NUL 区切りで rsync --files-from --from0 用）:
  library-original.list  Original/ を root に Library へ
  library-opus.list      Opus/ を root に Library へ
  archive-original.list  Original/ を root に Archive へ
  playlists.list         Opus/Playlists/ を root に Playlists/m3u8 へ
  skip.tsv               移さないもの（理由付き。人が読む）
  summary.json           件数と容量

  usage:
    ./migrate_plan.py /mnt/ssd/musics --out /tmp/plan

  exit code:
    0  対応が取れた（rsync に進める）
    1  対応が取れない stem がある（skip.tsv の理由を見て判断する）
    2  実行エラー
"""

from __future__ import annotations

import argparse
import collections
import json
import os
import sys

# preflight.py の AUDIO_EXT と揃える。スキャナが受け付ける拡張子の定義はあちらが正
AUDIO_EXT = {".flac", ".opus", ".m4a", ".mp3", ".wav", ".aac", ".ogg",
             ".alac", ".aiff", ".aif", ".wv", ".ape", ".webm", ".mka"}
LOSSLESS_EXT = {".flac", ".m4a", ".wav", ".alac", ".aiff", ".aif", ".wv", ".ape"}
RAW_EXT = {".webm", ".mka"}
IMAGE_EXT = {".png", ".jpg", ".jpeg", ".webp"}
MARKERS = {".exclude", ".noimage"}
# Original 側でアルバム直下に切られている入れ子。親アルバムの stem に畳む
NESTED = {"Original", "Opus", "Hi-Res", "AAC", "Artwork"}


def walk(base: str) -> dict[str, list[tuple[str, int]]]:
    """base 配下の全ファイルを (相対パス, サイズ) で返す。key は拡張子を除いた相対パス"""
    out: dict[str, list[tuple[str, int]]] = collections.defaultdict(list)
    for d, dirs, files in os.walk(base):
        dirs.sort()
        for f in sorted(files):
            p = os.path.join(d, f)
            rel = os.path.relpath(p, base)
            stem, _ = os.path.splitext(rel)
            out[stem].append((rel, os.lstat(p).st_size))
    return out


def fold(stem: str) -> tuple[str, str]:
    """`Album/Hi-Res/01. X` → (`Album/01. X`, `Hi-Res`)。入れ子でなければタグは空"""
    parts = stem.split(os.sep)
    if len(parts) >= 2 and parts[-2] in NESTED:
        return os.sep.join(parts[:-2] + parts[-1:]), parts[-2]
    return stem, ""


class Plan:
    def __init__(self) -> None:
        self.library_original: list[tuple[str, int]] = []
        self.library_opus: list[tuple[str, int]] = []
        self.archive_original: list[tuple[str, int]] = []
        self.playlists: list[tuple[str, int]] = []
        self.skip: list[tuple[str, str, str]] = []  # (root, rel, reason)
        self.unmatched: list[str] = []

    def summary(self) -> dict:
        def agg(items):
            return {"files": len(items), "bytes": sum(s for _, s in items)}
        return {
            "library_original": agg(self.library_original),
            "library_opus": agg(self.library_opus),
            "archive_original": agg(self.archive_original),
            "playlists": agg(self.playlists),
            "library_audio_total": sum(
                1 for rel, _ in self.library_original + self.library_opus
                if os.path.splitext(rel)[1].lower() in AUDIO_EXT
            ),
            "skipped": len(self.skip),
            "unmatched": len(self.unmatched),
        }


def build(root: str) -> Plan:
    opus_base = os.path.join(root, "Opus")
    orig_base = os.path.join(root, "Original")
    plan = Plan()

    opus_all = walk(opus_base)
    orig_all = walk(orig_base)

    # Opus/Playlists は m3u8 だけ別扱い
    opus_audio: dict[str, list[tuple[str, int]]] = {}
    for stem, files in opus_all.items():
        for rel, size in files:
            ext = os.path.splitext(rel)[1].lower()
            top = rel.split(os.sep)[0]
            if top == "Playlists":
                if ext == ".m3u8":
                    # Opus/Playlists/ を root にする（Playlists/m3u8/ へ平置き）
                    plan.playlists.append((os.path.relpath(rel, "Playlists"), size))
                else:
                    plan.skip.append(("Opus", rel, "playlists: m3u8 以外"))
            elif os.path.basename(rel) in MARKERS:
                plan.skip.append(("Opus", rel, "旧パイプラインのマーカー"))
            elif ext in AUDIO_EXT:
                opus_audio.setdefault(stem, []).append((rel, size))
            else:
                plan.skip.append(("Opus", rel, "音声でも m3u8 でもない"))

    # Original 側: stem（入れ子を畳んだもの）→ [(rel, size, ext, tag)]
    orig_audio: dict[str, list[tuple[str, int, str, str]]] = collections.defaultdict(list)
    for stem, files in orig_all.items():
        for rel, size in files:
            ext = os.path.splitext(rel)[1].lower()
            top = rel.split(os.sep)[0]
            key, tag = fold(stem)
            if top == "Playlists":
                plan.skip.append(("Original", rel, "playlists: Opus/Playlists の m3u8 を採用"))
            elif os.path.basename(rel) in MARKERS:
                plan.skip.append(("Original", rel, "旧パイプラインのマーカー"))
            elif tag == "AAC":
                plan.skip.append(("Original", rel, "AAC コピー（iTunes 用配布物）"))
            elif ext in IMAGE_EXT:
                plan.library_original.append((rel, size))
            elif ext in AUDIO_EXT:
                orig_audio[key].append((rel, size, ext, tag))
            else:
                plan.skip.append(("Original", rel, "音声でも画像でもない"))

    for key in sorted(set(opus_audio) | set(orig_audio)):
        opus_files = opus_audio.get(key, [])
        orig_files = orig_audio.get(key, [])
        if not orig_files:
            # 原本の無い opus は由来が分からないので止める（人が判断する）
            plan.unmatched.append(key)
            for rel, _ in opus_files:
                plan.skip.append(("Opus", rel, "unmatched: Original 側に原本が無い"))
            continue

        # 原本の中で master を選ぶ。整数 PCM のロスレス > 非可逆原本。aiff は m4a があれば Archive
        lossless = [f for f in orig_files if f[2] in LOSSLESS_EXT]
        raw = [f for f in orig_files if f[2] in RAW_EXT]
        lossy = [f for f in orig_files if f[2] not in LOSSLESS_EXT and f[2] not in RAW_EXT]

        if lossless:
            m4a = [f for f in lossless if f[2] != ".aiff" and f[2] != ".aif"]
            aiff = [f for f in lossless if f[2] in {".aiff", ".aif"}]
            masters = m4a if m4a else aiff
            # 入れ子の Hi-Res も、アルバム直下の 16/44 も両方 Library（audio_md5 が違う別トラック）
            for rel, size, _, _ in masters:
                plan.library_original.append((rel, size))
            if m4a:
                for rel, size, _, _ in aiff:
                    plan.archive_original.append((rel, size))
            for rel, size, ext, tag in raw:
                plan.archive_original.append((rel, size))
            for rel, size, ext, tag in lossy:
                plan.skip.append(("Original", rel, f"ロスレス原本があるので非可逆 {ext} は不要"))
            for rel, _ in opus_files:
                plan.skip.append(("Opus", rel, "ロスレス由来: Derived で再生成"))
        elif raw:
            # webm 由来: Opus/ の .opus が master（remux）、webm は Archive
            if not opus_files:
                plan.unmatched.append(key)
                for rel, _, _, _ in raw:
                    plan.skip.append(("Original", rel, "unmatched: webm に対応する Opus/ が無い"))
                continue
            for rel, size in opus_files:
                plan.library_opus.append((rel, size))
            for rel, size, _, _ in raw:
                plan.archive_original.append((rel, size))
            for rel, size, ext, tag in lossy:
                # 入れ子の Opus/ 等（Opus/ 側と同内容）は不要
                plan.skip.append(("Original", rel, f"webm 由来の {ext} は Opus/ 側を採用"))
        else:
            # 非可逆原本（mp3 等）: 原本が master。opus は世代劣化品
            for rel, size, _, _ in lossy:
                plan.library_original.append((rel, size))
            for rel, _ in opus_files:
                plan.skip.append(("Opus", rel, "非可逆原本由来の opus は捨てる（非可逆→非可逆）"))
    return plan


def write_list(path: str, items: list[tuple[str, int]]) -> None:
    with open(path, "wb") as f:
        for rel, _ in sorted(items):
            f.write(rel.encode("utf-8", "surrogateescape") + b"\0")


def human(n: int) -> str:
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if n < 1024:
            return f"{n:.1f}{unit}" if unit != "B" else f"{n}{unit}"
        n /= 1024
    return f"{n:.1f}PiB"


def main() -> int:
    ap = argparse.ArgumentParser(description="spindle 移行の振り分け計画")
    ap.add_argument("source", help="旧ライブラリのパス（Opus/ と Original/ を持つ）")
    ap.add_argument("--out", required=True, help="一覧の出力先ディレクトリ")
    args = ap.parse_args()

    for sub in ("Opus", "Original"):
        if not os.path.isdir(os.path.join(args.source, sub)):
            print(f"{args.source}/{sub} がない", file=sys.stderr)
            return 2
    os.makedirs(args.out, exist_ok=True)

    plan = build(args.source)
    write_list(os.path.join(args.out, "library-original.list"), plan.library_original)
    write_list(os.path.join(args.out, "library-opus.list"), plan.library_opus)
    write_list(os.path.join(args.out, "archive-original.list"), plan.archive_original)
    write_list(os.path.join(args.out, "playlists.list"), plan.playlists)
    with open(os.path.join(args.out, "skip.tsv"), "w", encoding="utf-8", errors="surrogateescape") as f:
        f.write("root\treason\tpath\n")
        for root, rel, reason in sorted(plan.skip, key=lambda t: (t[0], t[2], t[1])):
            f.write(f"{root}\t{reason}\t{rel}\n")
    summary = plan.summary()
    with open(os.path.join(args.out, "summary.json"), "w", encoding="utf-8") as f:
        json.dump(summary, f, ensure_ascii=False, indent=2)

    print("振り分け:")
    for k in ("library_original", "library_opus", "archive_original", "playlists"):
        v = summary[k]
        print(f"  {k:18} {v['files']:6,} files  {human(v['bytes'])}")
    print(f"  Library の音声合計   {summary['library_audio_total']:6,}  ← 初回スキャン後のトラック数と一致すること")
    reasons = collections.Counter(reason for _, _, reason in plan.skip)
    print("移さないもの:")
    for reason, n in reasons.most_common():
        print(f"  {n:6,}  {reason}")
    if plan.unmatched:
        print(f"対応が取れない stem: {len(plan.unmatched)}（skip.tsv の unmatched を見る）")
        for k in plan.unmatched[:20]:
            print(f"    {k}")
        return 1
    print(f"出力: {args.out}/")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(2)

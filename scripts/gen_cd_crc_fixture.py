#!/usr/bin/env python3
"""AccurateRip / CTDB の CRC の参照値を生成する（P2-6 のテストフィクスチャ）。

Rust 実装（src/cd/accuraterip.rs, src/cd/ctdb.rs）とは独立に、CUETools
（CUETools.AccurateRip/AccurateRip.cs, CUETools.AccurateRip/CDRepair.cs）の定義から
書き起こしたもの。両者が同じ値を出すことを tests/cd_crc.rs で確認する。

サンプル列は LCG で決定的に生成し、Rust 側でも同じ式で再現する
（x = x * 1103515245 + 12345 mod 2^32、サンプル = x >> 16 の下位 16 bit を符号付きで）。
L, R の順に生成する。

出力: tests/fixtures/cd_crc_reference.json

定義（CUETools と同じ）:
- AccurateRip の「サンプル」は 32 bit 語 = L(下位 16 bit) | R(上位 16 bit)
- v1 = Σ (n × word) mod 2^32、n はトラック内の 1 始まりの位置
- v2 = v1 + Σ ((n × word) >> 32) mod 2^32
- 先頭トラックは先頭 5×588−1 サンプルを、末尾トラックは末尾 5×588 サンプルを除外する
  （位置 n は除外分も数える）
- crc450 = トラック先頭から 450 セクタ目（サンプル 450×588 から 588 個）の v1 を
  位置 1 から数え直したもの。トラックが 451 セクタ未満なら無し
- CTDB の CRC32 は zlib 互換（IEEE、初期値 0xFFFFFFFF、最終 XOR）。
  ディスク CRC は先頭 10×588 サンプルと末尾 (10×588 + 総サンプル数 mod (10×588)) を除いた
  範囲。トラック CRC は同じ除外を先頭トラックの頭と末尾トラックの尻に適用し、中間は全体
"""

import json
import struct
import sys
import zlib
from pathlib import Path

SECTOR = 588
AR_SKIP_HEAD = 5 * SECTOR - 1
AR_SKIP_TAIL = 5 * SECTOR
CRC450_START = 450 * SECTOR
CTDB_STRIDE = 10 * SECTOR


def lcg_samples(seed: int, count: int) -> list[tuple[int, int]]:
    """(L, R) の符号付き 16 bit を count 組"""
    x = seed & 0xFFFFFFFF
    out = []
    for _ in range(count):
        pair = []
        for _ in range(2):
            x = (x * 1103515245 + 12345) & 0xFFFFFFFF
            v = (x >> 16) & 0xFFFF
            if v >= 0x8000:
                v -= 0x10000
            pair.append(v)
        out.append((pair[0], pair[1]))
    return out


def word(l: int, r: int) -> int:
    return (l & 0xFFFF) | ((r & 0xFFFF) << 16)


def accuraterip(track: list[tuple[int, int]], first: bool, last: bool) -> dict:
    n = len(track)
    lo_from = AR_SKIP_HEAD if first else 0
    hi_to = n - AR_SKIP_TAIL if last else n
    v1 = 0
    hi = 0
    for i in range(max(lo_from, 0), max(hi_to, 0)):
        p = word(*track[i]) * (i + 1)
        v1 = (v1 + (p & 0xFFFFFFFF)) & 0xFFFFFFFF
        hi = (hi + (p >> 32)) & 0xFFFFFFFF
    crc450 = None
    if n >= CRC450_START + SECTOR:
        crc450 = 0
        for j in range(SECTOR):
            crc450 = (crc450 + (j + 1) * word(*track[CRC450_START + j])) & 0xFFFFFFFF
    return {"v1": v1, "v2": (v1 + hi) & 0xFFFFFFFF, "crc450": crc450}


def pcm_bytes(samples: list[tuple[int, int]]) -> bytes:
    return b"".join(struct.pack("<hh", l, r) for l, r in samples)


def ctdb(tracks: list[list[tuple[int, int]]]) -> dict:
    disc = [s for t in tracks for s in t]
    total = len(disc)
    head = CTDB_STRIDE
    tail = CTDB_STRIDE + total % CTDB_STRIDE
    disc_crc = zlib.crc32(pcm_bytes(disc[head : total - tail])) & 0xFFFFFFFF
    track_crcs = []
    for i, t in enumerate(tracks):
        lo = head if i == 0 else 0
        hi = len(t) - tail if i == len(tracks) - 1 else len(t)
        track_crcs.append(zlib.crc32(pcm_bytes(t[lo:hi])) & 0xFFFFFFFF)
    return {"disc": disc_crc, "tracks": track_crcs}


CASES = [
    # (名前, seed, 各トラックのサンプル数)
    # 3 トラック。2 本目は 451 セクタ未満で crc450 無し。総数 mod 5880 = 588 で末尾除外が伸びる
    ("three_tracks", 0x5EED, [460 * SECTOR, 301 * SECTOR, 480 * SECTOR]),
    # 1 トラック（先頭かつ末尾）。crc450 の窓が末尾除外の中に入るが crc450 には影響しない
    ("single_track", 0xC0FFEE, [451 * SECTOR]),
    # 2 トラック。総数が 5880 の倍数で末尾除外が最小
    ("two_tracks", 42, [500 * SECTOR, 500 * SECTOR]),
]


def main() -> None:
    root = Path(__file__).resolve().parent.parent
    out_path = root / "tests" / "fixtures" / "cd_crc_reference.json"
    cases = []
    for name, seed, lengths in CASES:
        all_samples = lcg_samples(seed, sum(lengths))
        tracks = []
        pos = 0
        for n in lengths:
            tracks.append(all_samples[pos : pos + n])
            pos += n
        ar = [
            accuraterip(t, i == 0, i == len(tracks) - 1) for i, t in enumerate(tracks)
        ]
        cases.append(
            {
                "name": name,
                "seed": seed,
                "track_samples": lengths,
                "accuraterip": ar,
                "ctdb": ctdb(tracks),
            }
        )
    doc = {
        "_comment": [
            "scripts/gen_cd_crc_fixture.py が生成する。手で編集しない。",
            "AccurateRip v1/v2/crc450 と CTDB CRC32 の参照値。tests/cd_crc.rs が読む。",
        ],
        "cases": cases,
    }
    out_path.write_text(json.dumps(doc, ensure_ascii=False, indent=2) + "\n")
    print(f"wrote {out_path.relative_to(root)}", file=sys.stderr)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""
spindle — 既存の webm 由来トラックへの SOURCE_URL 補填（docs/TASKS.md P4-14、D-70）

旧パイプラインは YouTube の再生リストごとに「再生リスト名 = アルバム名、リスト内の位置 =
TRACKNUMBER」でファイルを並べていた。その再生リストを yt-dlp で列挙し、位置と番号で Library の
行に対応付けて `SOURCE_URL = https://www.youtube.com/watch?v=<id>` を書く。書き込みは通常の
編集バッチ（`set_rows` op。アルバムごとに 1 バッチ、巻き戻し可）。

  usage:
    ./backfill_source_url.py --playlists ~/.local/share/spindle/backfill/playlists.tsv \
        --spindle http://truenas:8080 --out ~/.local/share/spindle/backfill/plan
    ./backfill_source_url.py ... --apply                 # verified / verified-by-title / position-only を書く
    ./backfill_source_url.py ... --apply --include-inferred   # plan.csv の verified-by-neighbors を見てから
    ./backfill_source_url.py ... --only "存流のお歌"      # 1 アルバムだけ

  入力 TSV（リポジトリには置かない）: albumartist<TAB>album<TAB>再生リスト URL。`#` 行は無視。
  パスワードは環境変数 SPINDLE_PASSWORD。yt-dlp の dump は --out に JSON で保存し、再実行は
  それを読む（--refresh で取り直す）。

  判定（plan.csv の status）:
    verified           位置が合い、動画タイトルに Library のタイトルが含まれる
    verified-by-title  位置は合わないが、未割り当ての行の中でタイトルが合う（順の入れ替え・欠落によるずれ。
                       候補が複数なら期待位置に最も近い行）
    verified-by-neighbors  タイトルでは判定できない（英題など）が、両隣が同じずれ幅で対応し区間の行数も合う。
                       区間内の入れ替えは検出できないので既定では書かず、--include-inferred で明示する
    position-only      非公開 / 削除でタイトルが無い（位置だけで対応付け）
    title-mismatch     どれにも当てはまらない。人が見る（--include-mismatch は位置の行に書く。ずれの
                       後ろでは誤るので、plan.csv を見てから）
    already            既に同じ SOURCE_URL（位置に関わらず URL で照合）
    kept               既に別の SOURCE_URL を持つ行。上書きしない（P3 以降の取り込みや前回の補填）
    no-track           その動画の行が Library に無い（欠番、または未ダウンロード）
    extra-track     行はあるが再生リストにその位置が無い
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import re
import shlex
import subprocess
import sys
import unicodedata
import urllib.error
import urllib.parse
import urllib.request
from http.cookiejar import CookieJar

WATCH = "https://www.youtube.com/watch?v="
UNAVAILABLE_TITLES = {"[private video]", "[deleted video]", "[unavailable video]"}
PLAN_FIELDS = [
    "album",
    "track_no",
    "track_id",
    "rel_path",
    "video_id",
    "video_title",
    "library_title",
    "status",
    "source_url",
]


# ---------------------------------------------------------------- 入力


def parse_playlists_tsv(text: str) -> list[tuple[str, str, str]]:
    out = []
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split("\t")
        if len(parts) != 3:
            raise ValueError(f"TSV は 3 列: {line!r}")
        out.append((parts[0].strip(), parts[1].strip(), parts[2].strip()))
    return out


def parse_flat_playlist(dump: dict) -> list[dict]:
    """`yt-dlp --flat-playlist --dump-single-json` の entries を順に。非公開 / 削除は available=False"""
    out = []
    for e in dump.get("entries") or []:
        entry = {"id": e.get("id") or "", "title": e.get("title") or ""}
        entry["available"] = is_available(entry)
        out.append(entry)
    return out


def is_available(entry: dict) -> bool:
    """タイトルが取れている（非公開 / 削除でない）か"""
    if "available" in entry:
        return bool(entry["available"])
    title = (entry.get("title") or "").strip()
    return bool(entry.get("id")) and bool(title) and title.lower() not in UNAVAILABLE_TITLES


def playlist_truncated(dump: dict) -> bool:
    """yt-dlp が continuation を取りこぼした（古い版で黙って 100 件前後で止まる）"""
    count = dump.get("playlist_count")
    return isinstance(count, int) and len(dump.get("entries") or []) < count


def watch_url(video_id: str) -> str:
    return WATCH + video_id


# ---------------------------------------------------------------- タイトル照合

# 末尾の括弧書き（種類ごとに対にする。`【怪歌(再) Live ver.】` のように中に別種の括弧があってもよい）
_SUFFIX = re.compile(r"\s*(?:\([^()]*\)|（[^（）]*）|\[[^\[\]]*\]|【[^【】]*】)\s*$")
# 共演者の付記（`with X` / `feat. X` / `ft. X`）。動画側の表記が揺れるので落として比べる
_TAIL = re.compile(r"\s+(?:with|feat\.?|ft\.?)\s+.*$", re.IGNORECASE)


def normalize_title(s: str) -> str:
    """NFKC（全角英数 → 半角）→ 小文字 → 波ダッシュ / 全角チルダを `~` に → 空白を 1 つに"""
    s = unicodedata.normalize("NFKC", s).lower()
    for a, b in (("\u301c", "~"), ("\uff5e", "~"), ("\u2019", "'"), ("\u2018", "'"), ("\u201c", '"'), ("\u201d", '"')):
        s = s.replace(a, b)
    return re.sub(r"\s+", " ", s).strip()


def match_length(video_title: str, library_title: str) -> int:
    """Library のタイトル（末尾の括弧書き・共演者を 1 段ずつ外しながら）が動画タイトルに含まれるなら、
    含まれた部分の長さ（空白抜き）。含まれなければ 0。長いほど確かな一致（`ゲシュタルト` より
    `ゲシュタルト -崩壊Remix-` を優先する）。比較は空白を無視する（表記が揺れる）"""
    video = normalize_title(video_title).replace(" ", "")
    cand = normalize_title(library_title)
    while cand:
        compact = cand.replace(" ", "")
        if compact and _bounded_in(compact, video):
            return len(compact)
        stripped = _SUFFIX.sub("", cand).strip()
        if stripped == cand:
            stripped = _TAIL.sub("", cand).strip()
        if stripped == cand:
            break
        cand = stripped
    return 0


def _is_word_char(c: str) -> bool:
    """ASCII 英数字と数字（全角は NFKC で半角になっている）。日本語は境界を要求しない"""
    return c.isascii() and c.isalnum()


def _bounded_in(needle: str, hay: str) -> bool:
    """`needle` が `hay` に含まれ、両端が ASCII 英数字なら隣も英数字でない（`曲1` が `曲10` に、
    `a` が `abc` に一致しない）"""
    start = 0
    while True:
        i = hay.find(needle, start)
        if i < 0:
            return False
        end = i + len(needle)
        left_ok = not (_is_word_char(needle[0]) and i > 0 and _is_word_char(hay[i - 1]))
        right_ok = not (_is_word_char(needle[-1]) and end < len(hay) and _is_word_char(hay[end]))
        if left_ok and right_ok:
            return True
        start = i + 1


def title_matches(video_title: str, library_title: str) -> bool:
    return match_length(video_title, library_title) > 0


# ---------------------------------------------------------------- 対応付け


def _row(album: str, no: int, t: dict | None, e: dict | None, status: str) -> dict:
    return {
        "album": album,
        "track_no": no,
        "track_id": t["id"] if t else None,
        "rel_path": t["rel_path"] if t else "",
        "video_id": e["id"] if e else "",
        "video_title": e["title"] if e else "",
        "library_title": (t.get("title") or "") if t else "",
        "status": status,
        "source_url": watch_url(e["id"]) if e and e["id"] else "",
    }


def build_plan(album: str, entries: list[dict], tracks: list[dict]) -> list[dict]:
    """位置 i+1 ↔ track_no で対応付けて、行ごとの判定を返す（track_no 順）。

    1. 位置どおりの行とタイトルが合えば verified（非公開 / 削除は position-only、同じ URL なら already）
    2. 合わなかった entry は未割り当ての行からタイトルで探す（再生リストの順の入れ替え・欠落によるずれ）。
       候補が複数なら期待位置に最も近い行（同距離なら決めない）→ verified-by-title
    3. それでも残った entry の連続区間は、両端の隣が位置どおりに対応していて、区間の行数が
       entry 数と同じなら位置で採る（英題の動画など、タイトルで判定できないがずれも無い）
       → verified-by-neighbors
    残りは title-mismatch（人が見る）。行に対応する entry が無ければ extra-track"""
    by_no: dict[int, dict] = {}
    for t in tracks:
        if t.get("track_no") is not None:
            by_no.setdefault(int(t["track_no"]), t)
    n = len(entries)
    # 位置 → (track_no, status)。位置は 1 始まり
    assigned: dict[int, tuple[int, str]] = {}
    claimed: set[int] = set()
    rows: list[dict] = []

    def take(pos: int, no: int, status: str) -> None:
        assigned[pos] = (no, status)
        claimed.add(no)

    # 0. 既に SOURCE_URL を持つ行は別の URL で上書きしない（P3 以降の取り込みや前回の補填を守る）。
    #    同じ URL の entry があれば位置に関わらず already、無ければ kept
    url_index: dict[str, int] = {}
    for no, t in by_no.items():
        if t.get("source_url"):
            url_index.setdefault(t["source_url"], no)
            claimed.add(no)
    for i, e in enumerate(entries, start=1):
        url = watch_url(e["id"]) if e["id"] else ""
        if url and url in url_index and url_index[url] not in {a[0] for a in assigned.values()}:
            take(i, url_index[url], "already")

    for i, e in enumerate(entries, start=1):
        t = by_no.get(i)
        if t is None or i in assigned or i in claimed:
            continue
        if not is_available(e):
            take(i, i, "position-only")
        elif title_matches(e["title"], t.get("title") or ""):
            take(i, i, "verified")
    # 2. タイトルによる救済（一致の長い行 → 期待位置に近い行 の順で選ぶ。同点なら決めない）
    for i, e in enumerate(entries, start=1):
        if i in assigned or not is_available(e):
            continue
        cands = []
        for no, t in by_no.items():
            if no in claimed:
                continue
            n_match = match_length(e["title"], t.get("title") or "")
            if n_match:
                cands.append((-n_match, abs(no - i), no))
        if not cands:
            continue
        cands.sort()
        if len(cands) == 1 or cands[0][:2] != cands[1][:2]:
            no = cands[0][2]
            same = by_no[no].get("source_url") == (watch_url(e["id"]) if e["id"] else "")
            take(i, no, "already" if same else "verified-by-title")
    # 3. 両隣が同じずれ幅で対応している区間は、そのずれで位置から採る。先頭の区間は右隣が
    #    ずれ 0 のとき、末尾の区間は左隣がずれ 0 で行が余っていないときだけ（欠落を見落とさない）
    max_no = max(by_no) if by_no else 0
    i = 1
    while i <= n:
        if i in assigned:
            i += 1
            continue
        a = i
        while i <= n and i not in assigned:
            i += 1
        b = i - 1
        left = assigned.get(a - 1)
        right = assigned.get(b + 1)
        d_left = left[0] - (a - 1) if left else None
        d_right = right[0] - (b + 1) if right else None
        if a == 1 and b == n:
            d = None
        elif a == 1:
            d = 0 if d_right == 0 else None
        elif b == n:
            d = 0 if d_left == 0 and max_no == n else None
        else:
            d = d_left if d_left is not None and d_left == d_right else None
        if d is None:
            continue
        if all((no + d) in by_no and (no + d) not in claimed for no in range(a, b + 1)):
            for pos in range(a, b + 1):
                e = entries[pos - 1]
                same = by_no[pos + d].get("source_url") == (watch_url(e["id"]) if e["id"] else "")
                take(pos, pos + d, "already" if same else "verified-by-neighbors")
    # 行にする
    shown: set[int] = set()  # 行として出した track_no
    for i, e in enumerate(entries, start=1):
        if i in assigned:
            no, status = assigned[i]
            shown.add(no)
            rows.append(_row(album, no, by_no[no], e, status))
        elif by_no.get(i) is None or i in claimed:
            # 位置に行が無い、または位置の行を別の entry がタイトルで取った = この動画は Library に無い
            rows.append(_row(album, i, None, e, "no-track"))
        else:
            # 位置の行が空いていて、タイトルも合わない（人が見る。--include-mismatch の対象）
            claimed.add(i)
            shown.add(i)
            rows.append(_row(album, i, by_no[i], e, "title-mismatch"))
    for no, t in sorted(by_no.items()):
        if no in shown:
            continue
        if t.get("source_url"):
            rows.append(_row(album, no, t, None, "kept"))
        else:
            rows.append(_row(album, no, t, None, "extra-track"))
    rows.sort(key=lambda r: (r["track_no"], r["status"]))
    return rows


def rows_to_apply(rows: list[dict], include_mismatch: bool, include_inferred: bool = False) -> dict[int, str]:
    """書く行。verified-by-neighbors（位置推定）は区間内の入れ替えや欠落 + 追加の相殺を検出できないので、
    plan.csv を見てから --include-inferred で明示したときだけ書く"""
    ok = {"verified", "verified-by-title", "position-only"}
    if include_inferred:
        ok.add("verified-by-neighbors")
    if include_mismatch:
        ok.add("title-mismatch")
    return {
        int(r["track_id"]): r["source_url"]
        for r in rows
        if r["status"] in ok and r["track_id"] is not None and r["source_url"]
    }


# ---------------------------------------------------------------- spindle API


class Spindle:
    def __init__(self, base: str, password: str):
        self.base = base.rstrip("/")
        self.opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(CookieJar()))
        self._call("POST", "/api/auth/login", {"password": password})

    def _call(self, method: str, path: str, body=None):
        data = None
        headers = {"Sec-Fetch-Site": "same-origin", "Origin": self.base}
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        req = urllib.request.Request(self.base + path, data=data, method=method, headers=headers)
        try:
            with self.opener.open(req, timeout=120) as res:
                raw = res.read()
        except urllib.error.HTTPError as e:
            raise SystemExit(f"{method} {path}: HTTP {e.code}: {e.read().decode(errors='replace')[:300]}")
        return json.loads(raw) if raw else None

    def albums(self) -> list[dict]:
        return self._call("GET", "/api/albums")["items"]

    def tracks_of_album(self, album_id: int) -> list[dict]:
        out, cursor = [], None
        while True:
            q = {"filter": json.dumps({"album_ids": [album_id]}), "limit": "1000"}
            if cursor:
                q["cursor"] = cursor
            page = self._call("GET", "/api/tracks?" + urllib.parse.urlencode(q))
            out.extend(page["items"])
            cursor = page.get("next_cursor")
            if not cursor:
                return out

    def source_url_of(self, track_id: int) -> str | None:
        d = self._call("GET", f"/api/tracks/{track_id}")
        v = (d.get("detail") or {}).get("tags", {}).get("SOURCE_URL") or []
        return v[0] if v else None

    def preview(self, ids: list[int], ops: list) -> dict:
        return self._call("POST", "/api/tracks/batch/preview", {"selection": {"ids": ids}, "ops": ops})

    def apply(self, token: str, ops: list, description: str) -> dict:
        return self._call(
            "PATCH",
            "/api/tracks/batch",
            {"selection_token": token, "ops": ops, "description": description},
        )


# ---------------------------------------------------------------- 実行


def dump_playlist(ytdlp: str, url: str, path: str, refresh: bool) -> dict:
    if os.path.exists(path) and not refresh:
        with open(path, encoding="utf-8") as f:
            dump = json.load(f)
        _reject_truncated(dump)
        return dump
    # `--ytdlp` は前置き付きでもよい（例 "ssh host sudo docker exec spindle yt-dlp"）。`sh -c` は使わない。
    # ssh 越しは相手のシェルがもう一度解釈する（URL の `?` が glob になる）ので引数を quote する
    prefix = shlex.split(ytdlp)
    args = ["--dump-single-json", "--flat-playlist", "--no-download", "--no-update", "--", url]
    if prefix and os.path.basename(prefix[0]) == "ssh":
        args = [shlex.quote(a) for a in args]
    cmd = [*prefix, *args]
    res = subprocess.run(cmd, capture_output=True, text=True, timeout=300, check=False)
    if res.returncode != 0:
        raise SystemExit(f"yt-dlp が失敗 ({res.returncode}): {res.stderr.strip()[-500:]}")
    dump = json.loads(res.stdout)
    _reject_truncated(dump)
    with open(path, "w", encoding="utf-8") as f:
        json.dump(dump, f, ensure_ascii=False, indent=1)
    return dump


def _reject_truncated(dump: dict) -> None:
    if playlist_truncated(dump):
        raise SystemExit(
            f"yt-dlp が再生リストを途中までしか列挙できなかった（{len(dump.get('entries') or [])}/{dump.get('playlist_count')}）。"
            " yt-dlp を更新するか --ytdlp で新しい版を指す（保存済みの dump なら --refresh で取り直す）"
        )


def find_album(albums: list[dict], albumartist: str, album: str) -> dict | None:
    key = (normalize_title(albumartist), normalize_title(album))
    for a in albums:
        if (normalize_title(a.get("albumartist") or ""), normalize_title(a.get("album") or "")) == key:
            return a
    return None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--playlists", required=True, help="albumartist / album / URL の TSV")
    ap.add_argument("--spindle", required=True, help="例 http://truenas:8080")
    ap.add_argument("--out", required=True, help="dump JSON と plan.csv の置き場")
    ap.add_argument("--ytdlp", default="yt-dlp", help="yt-dlp のコマンド。前置き付き可（ssh … docker exec … yt-dlp）")
    ap.add_argument("--only", help="この album だけ（TSV の album 列）")
    ap.add_argument("--refresh", action="store_true", help="保存済みの dump を使わず取り直す")
    ap.add_argument("--apply", action="store_true", help="計画を編集バッチとして投入する")
    ap.add_argument("--include-inferred", action="store_true", help="verified-by-neighbors（位置推定）も書く。plan.csv を見てから")
    ap.add_argument("--include-mismatch", action="store_true", help="title-mismatch も書く（位置の行に）")
    args = ap.parse_args()

    password = os.environ.get("SPINDLE_PASSWORD")
    if not password:
        print("環境変数 SPINDLE_PASSWORD が要る", file=sys.stderr)
        return 2
    with open(args.playlists, encoding="utf-8") as f:
        playlists = parse_playlists_tsv(f.read())
    if args.only:
        playlists = [p for p in playlists if p[1] == args.only]
        if not playlists:
            print(f"--only に一致する album が TSV に無い: {args.only}", file=sys.stderr)
            return 2
    os.makedirs(args.out, exist_ok=True)

    api = Spindle(args.spindle, password)
    albums = api.albums()
    all_rows: list[dict] = []
    summary: list[str] = []
    for albumartist, album, url in playlists:
        a = find_album(albums, albumartist, album)
        if a is None:
            print(f"[{album}] Library に album が無い（albumartist={albumartist!r}）", file=sys.stderr)
            continue
        list_id = urllib.parse.parse_qs(urllib.parse.urlparse(url).query).get("list", ["playlist"])[0]
        dump = dump_playlist(args.ytdlp, url, os.path.join(args.out, f"{list_id}.json"), args.refresh)
        entries = parse_flat_playlist(dump)
        tracks = api.tracks_of_album(int(a["id"]))
        # 既存の SOURCE_URL は detail にしか無い（1 行 1 回。数百件なら数秒）
        for t in tracks:
            t["source_url"] = api.source_url_of(int(t["id"]))
        rows = build_plan(album, entries, tracks)
        counts: dict[str, int] = {}
        for r in rows:
            counts[r["status"]] = counts.get(r["status"], 0) + 1
        summary.append(f"[{album}] entries={len(entries)} tracks={len(tracks)} " + " ".join(f"{k}={v}" for k, v in sorted(counts.items())))
        all_rows.extend(rows)

        if args.apply:
            targets = rows_to_apply(rows, args.include_mismatch, args.include_inferred)
            if not targets:
                summary.append(f"[{album}] 書くものが無い")
                continue
            ops = [{"op": "set_rows", "key": "SOURCE_URL", "rows": {str(k): v for k, v in targets.items()}}]
            pv = api.preview(sorted(targets), ops)
            if pv.get("pending_excluded"):
                # apply は反映待ちがあると 409 で拒む（skip_pending で除外もできるが、補填は急がないので
                # 全部そろってからやり直す）
                summary.append(f"[{album}] 反映待ちの行が {pv['pending_excluded']} 件あるので見送り（終わってからやり直す）")
                continue
            if not pv.get("changed"):
                summary.append(f"[{album}] preview で変更なし（既に同じ値）")
                continue
            res = api.apply(pv["selection_token"], ops, f"SOURCE_URL 補填 ({album})")
            summary.append(f"[{album}] batch #{res['batch_id']} affected={res['affected']} (preview changed={pv['changed']})")

    plan_path = os.path.join(args.out, "plan.csv")
    with open(plan_path, "w", encoding="utf-8", newline="") as f:
        w = csv.DictWriter(f, fieldnames=PLAN_FIELDS)
        w.writeheader()
        for r in all_rows:
            w.writerow({k: ("" if r.get(k) is None else r.get(k)) for k in PLAN_FIELDS})
    print("\n".join(summary))
    print(f"plan: {plan_path} ({len(all_rows)} 行)")
    return 0


if __name__ == "__main__":
    sys.exit(main())

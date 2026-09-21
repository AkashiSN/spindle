"""backfill_source_url.py の対応付けと判定（P4-14）。`python3 -m unittest scripts/test_backfill_source_url.py`"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from backfill_source_url import (  # noqa: E402
    build_plan,
    normalize_title,
    parse_playlists_tsv,
    parse_flat_playlist,
    rows_to_apply,
    title_matches,
    watch_url,
)


def entry(id_, title):
    return {"id": id_, "title": title}


def track(id_, no, title, source_url=None, rel_path=None):
    return {
        "id": id_,
        "track_no": no,
        "title": title,
        "rel_path": rel_path or f"X/A/A のお歌/{no:02} {title}.opus",
        "source_url": source_url,
    }


class ParseInputs(unittest.TestCase):
    def test_tsv_skips_comments_and_blank_lines(self):
        text = "# c\n\nA\tA のお歌\thttps://x/1\nB\tB のお歌\thttps://x/2\n"
        self.assertEqual(
            parse_playlists_tsv(text),
            [("A", "A のお歌", "https://x/1"), ("B", "B のお歌", "https://x/2")],
        )

    def test_flat_playlist_keeps_entry_order_and_marks_unavailable(self):
        dump = {
            "title": "A のお歌",
            "entries": [
                {"id": "v1", "title": "曲 1 / A"},
                {"id": "v2", "title": "[Private video]"},
                {"id": "v3", "title": "[Deleted video]"},
            ],
        }
        dump["entries"].append({"id": "v4", "title": ""})
        got = parse_flat_playlist(dump)
        self.assertEqual([e["id"] for e in got], ["v1", "v2", "v3", "v4"])
        self.assertEqual([e["available"] for e in got], [True, False, False, False])

    def test_truncated_playlist_is_detected(self):
        from backfill_source_url import playlist_truncated

        self.assertFalse(playlist_truncated({"playlist_count": 3, "entries": [{}, {}, {}]}))
        self.assertTrue(playlist_truncated({"playlist_count": 244, "entries": [{}] * 102}))
        # playlist_count が無い版でも entries だけなら判定しない
        self.assertFalse(playlist_truncated({"entries": [{}]}))

    def test_watch_url_is_canonical(self):
        self.assertEqual(watch_url("abc"), "https://www.youtube.com/watch?v=abc")


class TitleMatching(unittest.TestCase):
    def test_normalize_folds_width_case_and_spaces(self):
        self.assertEqual(normalize_title("Ｓｎｏｗ  Jam（Cover）"), "snow jam(cover)")

    def test_wave_dash_and_tilde_are_folded(self):
        self.assertTrue(title_matches("Departures 〜あなたにおくるアイの歌〜 - EGOIST covered by 存流", "Departures ~あなたにおくるアイの歌~ (Cover)"))
        self.assertEqual(normalize_title("a〜b～c~d"), "a~b~c~d")

    def test_with_and_feat_tails_are_dropped_progressively(self):
        self.assertTrue(title_matches("【歌ってみた】ワールド・コーリング covered by ヰ世界情緒 & 春猿火", "ワールド・コーリング with 春猿火 (Cover)"))
        self.assertTrue(title_matches("【音楽的同位体星界】回想 / ヰ世界情緒 feat. 星界", "回想 feat. 星界"))
        self.assertTrue(title_matches("【V.W.P】ヰ世界情緒×花譜「深淵」【派生曲】", "深淵 with 花譜"))
        # 英題 ↔ 邦題は判定できない（人が見る）
        self.assertFalse(title_matches("Moonflower Confession covered by Isekaijoucho", "夜顔の告白 (Cover)"))

    def test_nested_brackets_in_suffix_and_curly_quotes(self):
        self.assertTrue(title_matches("花譜「何者 -みんなで創る神椿幕張戦線MV-」【「怪歌(再)」Live Ver.】", "何者 【怪歌(再) Live ver.】"))
        self.assertTrue(title_matches("Can’t Wait ’Til Christmas - 宇多田ヒカル Covered by 理芽", "Can't Wait' Til Christmas (Cover)"))
        self.assertEqual(normalize_title("Can’t “x” ‘y’"), "can't \"x\" 'y'")

    def test_spaces_are_ignored_when_comparing(self):
        self.assertTrue(title_matches("花譜 # 140「ゲシュタルト-崩壊Remix-」【オリジナルMV】", "ゲシュタルト -崩壊Remix-"))

    def test_ascii_and_digit_matches_need_word_boundaries(self):
        # 「曲 1」が「曲 10」に、「a」が「abc」に一致してはいけない
        self.assertFalse(title_matches("曲 10 / A", "曲 1"))
        self.assertFalse(title_matches("abc", "a"))
        self.assertFalse(title_matches("Rumor2 covered by X", "Rumor"))
        self.assertTrue(title_matches("曲 1 / A", "曲 1"))
        self.assertTrue(title_matches("Rumor - ポリスピカデリー covered by 存流", "Rumor (Cover)"))
        # 日本語は境界を要求しない（1 文字の題名もある）
        self.assertTrue(title_matches("花譜 # 112「糸」【Live Ver.】", "糸"))

    def test_library_title_without_suffix_is_substring_of_video_title(self):
        self.assertTrue(title_matches("【歌ってみた】新世界ピグマリオン / VALIS", "新世界ピグマリオン"))
        self.assertTrue(title_matches("モザイクロール (Reloaded) covered by 春猿火", "モザイクロール (Reloaded) (Cover)"))
        self.assertTrue(title_matches("snow jam - Rin音 (cover) 春猿火", "snow jam (Cover)"))
        self.assertFalse(title_matches("全然違う曲 / A", "新世界ピグマリオン"))


class Plan(unittest.TestCase):
    def test_position_maps_to_track_number_with_statuses(self):
        entries = [
            entry("v1", "曲 1 / A"),          # 1: 一致
            entry("v2", "[Private video]"),   # 2: 位置のみ
            entry("v3", "別の曲 / A"),        # 3: タイトル不一致
            entry("v4", "曲 4 / A"),          # 4: 行が無い（欠番）
            entry("v5", "曲 5 / A"),          # 5: 既に同じ URL
        ]
        tracks = [
            track(11, 1, "曲 1"),
            track(12, 2, "曲 2"),
            track(13, 3, "曲 3"),
            track(15, 5, "曲 5", source_url="https://www.youtube.com/watch?v=v5"),
            track(16, 6, "曲 6"),             # entry が無い
        ]
        rows = build_plan("A のお歌", entries, tracks)
        by_no = {r["track_no"]: r for r in rows}
        self.assertEqual(by_no[1]["status"], "verified")
        self.assertEqual(by_no[1]["track_id"], 11)
        self.assertEqual(by_no[1]["source_url"], "https://www.youtube.com/watch?v=v1")
        self.assertEqual(by_no[2]["status"], "position-only")
        self.assertEqual(by_no[3]["status"], "title-mismatch")
        self.assertEqual(by_no[4]["status"], "no-track")
        self.assertIsNone(by_no[4]["track_id"])
        self.assertEqual(by_no[5]["status"], "already")
        self.assertEqual(by_no[6]["status"], "extra-track")
        self.assertEqual(by_no[6]["video_id"], "")
        self.assertEqual(len(rows), 6)

    def test_swapped_positions_are_rescued_by_title(self):
        # 再生リストの順を後から入れ替えた: 位置 2 と 3 が Library の番号と逆
        entries = [entry("v1", "曲 1 / A"), entry("v2", "曲 3 / A"), entry("v3", "曲 2 / A")]
        tracks = [track(11, 1, "曲 1"), track(12, 2, "曲 2"), track(13, 3, "曲 3")]
        rows = build_plan("A のお歌", entries, tracks)
        by_id = {r["track_id"]: r for r in rows}
        self.assertEqual(by_id[11]["status"], "verified")
        self.assertEqual(by_id[12]["status"], "verified-by-title")
        self.assertEqual(by_id[12]["video_id"], "v3")
        self.assertEqual(by_id[13]["status"], "verified-by-title")
        self.assertEqual(by_id[13]["video_id"], "v2")
        self.assertEqual(len(rows), 3)

    def test_title_rescue_prefers_the_track_nearest_to_the_position(self):
        # 位置 5 の entry が消えて以降が 1 ずれた。「曲 6」と「曲 6 【Live ver.】」の両方が候補に
        # なるので、期待位置（5 → 6 付近）に近い方を採る
        entries = [entry("v1", "曲 1"), entry("v2", "曲 2"), entry("v3", "曲 3"), entry("v4", "曲 4"),
                   entry("v6", "曲 6 / A"), entry("v7", "曲 7 / A"), entry("v8", "曲 6 【Live】/ A")]
        tracks = [track(1, 1, "曲 1"), track(2, 2, "曲 2"), track(3, 3, "曲 3"), track(4, 4, "曲 4"),
                  track(5, 5, "曲 5"), track(6, 6, "曲 6"), track(7, 7, "曲 7"), track(8, 8, "曲 6 【Live ver.】")]
        rows = build_plan("A のお歌", entries, tracks)
        by_id = {r["track_id"]: r for r in rows if r["track_id"] is not None}
        self.assertEqual((by_id[6]["status"], by_id[6]["video_id"]), ("verified-by-title", "v6"))
        self.assertEqual((by_id[7]["status"], by_id[7]["video_id"]), ("verified-by-title", "v7"))
        self.assertEqual((by_id[8]["status"], by_id[8]["video_id"]), ("verified-by-title", "v8"))
        self.assertEqual(by_id[5]["status"], "extra-track")

    def test_title_rescue_prefers_the_longest_matching_title(self):
        # 「ゲシュタルト」（原曲）と「ゲシュタルト -崩壊Remix-」の両方が候補になる。長い方が正しい
        entries = [entry("v1", "曲 1 / A"), entry("v2", "KAF #137 - Gestalt [MV]"), entry("v3", "曲 3 / A"),
                   entry("v4", "花譜 # 140「ゲシュタルト-崩壊Remix-」【オリジナルMV】")]
        tracks = [track(1, 1, "曲 1"), track(2, 2, "ゲシュタルト"), track(3, 3, "曲 3"), track(4, 4, "ゲシュタルト -崩壊Remix-")]
        rows = build_plan("A のお歌", entries, tracks)
        by_no = {r["track_no"]: r for r in rows}
        self.assertEqual((by_no[4]["status"], by_no[4]["video_id"]), ("verified", "v4"))
        self.assertEqual((by_no[2]["status"], by_no[2]["video_id"]), ("verified-by-neighbors", "v2"))

    def test_playlist_video_without_a_library_row_is_no_track(self):
        # 位置 2 の動画は Library に無く、位置の行は別の entry がタイトルで取った
        entries = [entry("v1", "曲 1 / A"), entry("v2", "未DL / A"), entry("v3", "曲 2 / A")]
        tracks = [track(1, 1, "曲 1"), track(2, 2, "曲 2")]
        rows = build_plan("A", entries, tracks)
        st = {(r["track_no"], r["video_id"]): r["status"] for r in rows}
        self.assertEqual(st[(2, "v2")], "no-track")
        self.assertEqual(st[(2, "v3")], "verified-by-title")

    def test_title_rescue_with_equidistant_candidates_stays_mismatch(self):
        entries = [entry("v1", "違う / A"), entry("v2", "同じ / A"), entry("v3", "別 / A")]
        tracks = [track(11, 1, "同じ"), track(12, 2, "x"), track(13, 3, "同じ")]
        rows = build_plan("A のお歌", entries, tracks)
        by_no = {r["track_no"]: r for r in rows}
        self.assertEqual(by_no[2]["status"], "title-mismatch")

    def test_positional_mismatch_between_consistent_neighbours_is_accepted(self):
        # 英題の動画: タイトルでは判定できないが、両隣が位置どおりなら位置で採る
        entries = [entry("v1", "曲 1 / A"), entry("v2", "Yarn [Music Video]"), entry("v3", "曲 3 / A"),
                   entry("v4", "Witch [MV]"), entry("v5", "Chicks [MV]")]
        tracks = [track(1, 1, "曲 1"), track(2, 2, "糸"), track(3, 3, "曲 3"), track(4, 4, "魔女"), track(5, 5, "雛鳥")]
        rows = build_plan("A のお歌", entries, tracks)
        by_no = {r["track_no"]: r for r in rows}
        self.assertEqual((by_no[2]["status"], by_no[2]["video_id"]), ("verified-by-neighbors", "v2"))
        # 末尾の 4, 5 も、左隣がずれ 0 で行が余っていないので位置で採る
        self.assertEqual(by_no[4]["status"], "verified-by-neighbors")
        self.assertEqual(by_no[5]["status"], "verified-by-neighbors")

    def test_leading_run_is_accepted_when_the_right_anchor_is_positional(self):
        entries = [entry("v1", "Deathly Loneliness Attacks"), entry("v2", "Sayonara Midnight"), entry("v3", "曲 3 / A")]
        tracks = [track(1, 1, "猛独が襲う (Cover)"), track(2, 2, "さよならミッドナイト (Cover)"), track(3, 3, "曲 3")]
        rows = build_plan("A のお歌", entries, tracks)
        by_no = {r["track_no"]: r for r in rows}
        self.assertEqual((by_no[1]["status"], by_no[1]["video_id"]), ("verified-by-neighbors", "v1"))
        self.assertEqual((by_no[2]["status"], by_no[2]["video_id"]), ("verified-by-neighbors", "v2"))

    def test_shifted_run_is_accepted_when_both_anchors_share_the_offset(self):
        # 位置 2 の entry が消えて以降 +1。3 (Gestalt) は英題で判定できないが、両隣が +1 で対応
        entries = [entry("v1", "曲 1 / A"), entry("v3", "曲 3 / A"), entry("v4", "Gestalt [MV]"), entry("v5", "曲 5 / A")]
        tracks = [track(1, 1, "曲 1"), track(2, 2, "曲 2"), track(3, 3, "曲 3"), track(4, 4, "ゲシュタルト"), track(5, 5, "曲 5")]
        rows = build_plan("A のお歌", entries, tracks)
        by_no = {r["track_no"]: r for r in rows}
        self.assertEqual((by_no[3]["status"], by_no[3]["video_id"]), ("verified-by-title", "v3"))
        self.assertEqual((by_no[4]["status"], by_no[4]["video_id"]), ("verified-by-neighbors", "v4"))
        self.assertEqual((by_no[5]["status"], by_no[5]["video_id"]), ("verified-by-title", "v5"))
        self.assertEqual(by_no[2]["status"], "extra-track")

    def test_trailing_run_needs_equal_counts(self):
        # 末尾の英題: 行が余っていなければずれ 0 で採る。余っていれば決めない
        entries = [entry("v1", "曲 1 / A"), entry("v2", "Yarn [MV]")]
        rows = build_plan("A", entries, [track(1, 1, "曲 1"), track(2, 2, "糸")])
        self.assertEqual({r["track_no"]: r["status"] for r in rows}, {1: "verified", 2: "verified-by-neighbors"})
        rows = build_plan("A", entries, [track(1, 1, "曲 1"), track(2, 2, "糸"), track(3, 3, "余り")])
        self.assertEqual({r["track_no"]: r["status"] for r in rows}, {1: "verified", 2: "title-mismatch", 3: "extra-track"})

    def test_existing_source_url_is_never_overwritten(self):
        # Library の 3 は既に LiSA 版の URL を持つ（明透×琶舞）。再生リストで Vaundy 版を 3 に置いても
        # 上書きしない: LiSA 版の entry は URL で already、Vaundy 版は no-track
        entries = [entry("v1", "曲 1 / A"), entry("v2", "曲 2 / A"), entry("vaundy", "再会 - Vaundy covered by A"),
                   entry("lisa", "再会 - LiSA,Uru covered by A×B")]
        tracks = [track(1, 1, "曲 1"), track(2, 2, "曲 2"),
                  track(3, 3, "再会 (Cover)", source_url="https://www.youtube.com/watch?v=lisa")]
        rows = build_plan("A", entries, tracks)
        st = {(r["track_no"], r["video_id"]): r["status"] for r in rows}
        self.assertEqual(st[(3, "lisa")], "already")
        self.assertEqual(st[(3, "vaundy")], "no-track")
        self.assertNotIn("kept", {r["status"] for r in rows})
        self.assertEqual(rows_to_apply(rows, include_mismatch=True), {1: "https://www.youtube.com/watch?v=v1", 2: "https://www.youtube.com/watch?v=v2"})

    def test_track_with_a_url_from_elsewhere_is_kept(self):
        entries = [entry("v1", "曲 1 / A")]
        tracks = [track(1, 1, "曲 1"), track(2, 2, "別経路 (Cover)", source_url="https://www.youtube.com/watch?v=p3")]
        rows = build_plan("A", entries, tracks)
        self.assertEqual({r["track_no"]: r["status"] for r in rows}, {1: "verified", 2: "kept"})
        self.assertEqual(rows_to_apply(rows, include_mismatch=True), {1: "https://www.youtube.com/watch?v=v1"})

    def test_inferred_rows_are_opt_in_because_swaps_inside_a_run_are_invisible(self):
        # 区間内で English Two / English Three が入れ替わっていても両端のずれ幅は同じで、位置推定は
        # 気づけない。だから verified-by-neighbors は既定では書かず --include-inferred で明示する
        entries = [entry("v1", "曲 1 / A"), entry("v3", "English Three"), entry("v2", "English Two"), entry("v4", "曲 4 / A")]
        tracks = [track(1, 1, "曲 1"), track(2, 2, "邦題二"), track(3, 3, "邦題三"), track(4, 4, "曲 4")]
        rows = build_plan("A", entries, tracks)
        by_no = {r["track_no"]: r for r in rows}
        self.assertEqual(by_no[2]["status"], "verified-by-neighbors")
        self.assertEqual(rows_to_apply(rows, include_mismatch=False), {1: "https://www.youtube.com/watch?v=v1", 4: "https://www.youtube.com/watch?v=v4"})
        self.assertEqual(
            rows_to_apply(rows, include_mismatch=False, include_inferred=True),
            {1: "https://www.youtube.com/watch?v=v1", 2: "https://www.youtube.com/watch?v=v3", 3: "https://www.youtube.com/watch?v=v2", 4: "https://www.youtube.com/watch?v=v4"},
        )

    def test_cached_dump_is_also_checked_for_truncation(self):
        import json
        import tempfile

        from backfill_source_url import dump_playlist

        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "x.json")
            with open(path, "w", encoding="utf-8") as f:
                json.dump({"playlist_count": 244, "entries": [{"id": "a"}] * 102}, f)
            with self.assertRaises(SystemExit):
                dump_playlist("yt-dlp", "https://x/list", path, refresh=False)

    def test_apply_set_selects_verified_and_position_only_by_default(self):
        rows = [
            {"track_id": 1, "status": "verified", "source_url": "u1"},
            {"track_id": 2, "status": "position-only", "source_url": "u2"},
            {"track_id": 3, "status": "title-mismatch", "source_url": "u3"},
            {"track_id": None, "status": "no-track", "source_url": "u4"},
            {"track_id": 5, "status": "already", "source_url": "u5"},
        ]
        rows.append({"track_id": 6, "status": "verified-by-title", "source_url": "u6"})
        rows.append({"track_id": 7, "status": "verified-by-neighbors", "source_url": "u7"})
        self.assertEqual(rows_to_apply(rows, include_mismatch=False), {1: "u1", 2: "u2", 6: "u6"})
        self.assertEqual(rows_to_apply(rows, include_mismatch=True), {1: "u1", 2: "u2", 3: "u3", 6: "u6"})
        self.assertEqual(rows_to_apply(rows, include_mismatch=False, include_inferred=True), {1: "u1", 2: "u2", 6: "u6", 7: "u7"})


if __name__ == "__main__":
    unittest.main()

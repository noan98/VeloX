#!/usr/bin/env python3
"""`scripts/bench/serve.py` の beacon 集計の単体テスト (Issue #247)。

集計ロジックだけを対象にする。ソケットを立てないのは、**この計測が
答えようとしている問いがカウンタの進み方だから**で、HTTP そのものは
`SimpleHTTPRequestHandler` に任せている。
"""

import os
import sys
import unittest

sys.path.insert(
    0,
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "scripts", "bench"),
)

from serve import BeaconCounts  # noqa: E402


class BeaconCountsTest(unittest.TestCase):
    def test_counts_per_state_and_reports_how_far_counters_advanced(self):
        counts = BeaconCounts()
        counts.record("state=hidden&f=10&t=20")
        counts.record("state=hidden&f=35&t=61")
        counts.record("state=visible&f=0&t=0")

        summary = counts.summary()
        self.assertEqual(sorted(summary), ["hidden", "visible"])
        self.assertEqual(summary["hidden"]["count"], 2)
        # **これが本題。** 隠れているタブでカウンタが進んだかどうか。
        self.assertEqual(summary["hidden"]["advanced_frames"], 25)
        self.assertEqual(summary["hidden"]["advanced_ticks"], 41)
        self.assertEqual(summary["visible"]["count"], 1)

    def test_a_single_beacon_advances_nothing(self):
        # 1 件しか届かなければ「進んだ」とは言えない。最初と最後が同じ
        # なので差は 0 になる — これを「止まっている」と読み違えない
        # ために、`count` も一緒に出している。
        counts = BeaconCounts()
        counts.record("state=hidden&f=7&t=9")
        summary = counts.summary()
        self.assertEqual(summary["hidden"]["count"], 1)
        self.assertEqual(summary["hidden"]["advanced_frames"], 0)
        self.assertEqual(summary["hidden"]["advanced_ticks"], 0)

    def test_counters_that_never_move_are_visible_as_zero_advance(self):
        # 背景タブで rAF が完全に止まると f はこうなる (t だけ進む)。
        # この形が読めることが #247 の目的そのもの。
        counts = BeaconCounts()
        for ticks in (5, 10, 15):
            counts.record(f"state=hidden&f=42&t={ticks}")
        summary = counts.summary()
        self.assertEqual(summary["hidden"]["advanced_frames"], 0)
        self.assertEqual(summary["hidden"]["advanced_ticks"], 10)

    def test_malformed_beacons_are_dropped_without_raising(self):
        # 集計の欠落より計測の継続を優先する: ここで例外を投げると
        # ベンチ実行そのものが落ちる。
        counts = BeaconCounts()
        for query in ("", "state=hidden", "state=hidden&f=x&t=1", "f=1&t=2&state=", "state=h&f=-1&t=2"):
            counts.record(query)
        self.assertEqual(counts.summary().get("hidden", {}).get("count", 0), 0)

    def test_state_is_recorded_even_when_the_page_does_not_send_one(self):
        # `state` が無くても f/t が読めれば数える。捨てると
        # 「beacon が来ていない」と区別がつかなくなる。
        counts = BeaconCounts()
        counts.record("f=1&t=2")
        counts.record("f=4&t=9")
        summary = counts.summary()
        self.assertEqual(summary["unknown"]["count"], 2)
        self.assertEqual(summary["unknown"]["advanced_frames"], 3)


if __name__ == "__main__":
    unittest.main()

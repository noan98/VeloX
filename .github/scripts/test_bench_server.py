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


class PerInstanceTest(unittest.TestCase):
    """§42.4 が壊れていた理由を、そのまま回帰テストにする。

    初版は state ごとに first/last だけを持っていた。`background_cpu` は
    12 個の背景タブを開き、velox は 1 run で 8 回起動し直す — どれも
    カウンタを 0 から数え直すので、**別々の実体の値を引き算していた。**
    実測で `advanced_frames` が -5 になった。
    """

    def test_two_tabs_in_the_same_state_do_not_subtract_across_each_other(self):
        counts = BeaconCounts()
        # A は 10 -> 20 (+10)、B は 100 -> 140 (+40)。合計 50。
        counts.record("state=hidden&f=10&t=5&id=A")
        counts.record("state=hidden&f=100&t=50&id=B")
        counts.record("state=hidden&f=20&t=9&id=A")
        counts.record("state=hidden&f=140&t=70&id=B")

        summary = counts.summary()
        self.assertEqual(summary["hidden"]["count"], 4)
        self.assertEqual(summary["hidden"]["instances"], 2)
        self.assertEqual(summary["hidden"]["advanced_frames"], 50)
        self.assertEqual(summary["hidden"]["advanced_ticks"], 24)

    def test_a_reloaded_page_never_makes_the_advance_negative(self):
        """velox の再起動でカウンタが 0 に戻っても負にならない。

        **これが §42.4 で実際に出た -5 の正体である。**
        """
        counts = BeaconCounts()
        counts.record("state=hidden&f=100&t=156&id=first")
        # 起動し直し。新しいページ実体なので id が変わり、値は小さい。
        counts.record("state=hidden&f=3&t=4&id=second")
        counts.record("state=hidden&f=95&t=162&id=second")

        summary = counts.summary()
        self.assertEqual(summary["hidden"]["instances"], 2)
        self.assertGreaterEqual(summary["hidden"]["advanced_frames"], 0)
        # first は 1 件だけなので 0、second が 3 -> 95 で +92。
        self.assertEqual(summary["hidden"]["advanced_frames"], 92)

    def test_a_smaller_value_arriving_later_is_reordering_not_corruption(self):
        """同一インスタンスでカウンタが減ることは**原理的に無い**。

        `frames` / `ticks` は単調増加しかしない。したがって小さい値が
        後から届いたら、それは「カウンタが戻った」のではなく
        **beacon の到着順が入れ替わった**ということである
        (`fetch` は投げっぱなしで、応答も順序も待たない)。

        そこで min/max で範囲を取る。**進んだ量は 0 ではなく 40 が
        正しい** — 送信側は 10 から 50 まで確かに進んでいる。
        """
        counts = BeaconCounts()
        counts.record("state=hidden&f=50&t=50&id=A")
        counts.record("state=hidden&f=10&t=10&id=A")
        summary = counts.summary()
        self.assertEqual(summary["hidden"]["advanced_frames"], 40)
        self.assertEqual(summary["hidden"]["advanced_ticks"], 40)
        # 負にはならない。これは min/max を取る限り構造的に保証される。
        self.assertGreaterEqual(summary["hidden"]["advanced_frames"], 0)

    def test_out_of_order_arrival_does_not_shrink_the_advance(self):
        """`fetch` は投げっぱなしなので、到着順は送信順と一致しない。

        PR #255 のレビュー指摘。先に届いたほうを first と決め打つと、
        順序が入れ替わっただけで進んだ量が縮む。**last を `max` で
        守って first を守らないのは非対称である。**
        """
        counts = BeaconCounts()
        # 送信順は 10 -> 30 -> 50 だが、30 が最初に届いた。
        counts.record("state=hidden&f=30&t=30&id=A")
        counts.record("state=hidden&f=10&t=10&id=A")
        counts.record("state=hidden&f=50&t=50&id=A")
        summary = counts.summary()
        # 到着順によらず 10 -> 50 で +40。
        self.assertEqual(summary["hidden"]["advanced_frames"], 40)
        self.assertEqual(summary["hidden"]["advanced_ticks"], 40)

    def test_beacons_without_an_id_collapse_into_one_instance(self):
        """`id=` を送らない古いページは 1 実体として扱う。

        初版と同じ壊れ方をするが、**`instances` が 1 であることから
        混ざっていると分かる**ので、黙って誤らせない。
        """
        counts = BeaconCounts()
        counts.record("state=hidden&f=10&t=1")
        counts.record("state=hidden&f=20&t=2")
        summary = counts.summary()
        self.assertEqual(summary["hidden"]["instances"], 1)

    def test_instances_is_reported_so_a_missing_id_is_visible(self):
        # 背景タブが 12 個あるのに instances が 1 なら、id= が
        # 届いていないということ。読み手がそれに気づけるようにする。
        counts = BeaconCounts()
        for tab in range(12):
            counts.record(f"state=hidden&f=1&t=1&id=tab{tab}")
        self.assertEqual(counts.summary()["hidden"]["instances"], 12)


class ResetTest(unittest.TestCase):
    """A/B の腕を分ける仕掛け (§42.5 / D127 決定5)。"""

    def test_reset_returns_what_it_clears(self):
        counts = BeaconCounts()
        counts.record("state=hidden&f=1&t=1&id=A")
        counts.record("state=hidden&f=9&t=5&id=A")

        cleared = counts.reset()
        self.assertEqual(cleared["hidden"]["count"], 2)
        self.assertEqual(cleared["hidden"]["advanced_frames"], 8)
        # 返した分は消えている。次の腕の beacon と混ざらない。
        self.assertEqual(counts.summary(), {})

    def test_the_second_arm_starts_from_zero(self):
        counts = BeaconCounts()
        counts.record("state=hidden&f=100&t=100&id=lowarm")
        counts.reset()

        counts.record("state=hidden&f=5&t=5&id=normalarm")
        counts.record("state=hidden&f=8&t=7&id=normalarm")
        summary = counts.summary()
        self.assertEqual(summary["hidden"]["count"], 2)
        self.assertEqual(summary["hidden"]["instances"], 1)
        # 前の腕の 100 を引きずらない。
        self.assertEqual(summary["hidden"]["advanced_frames"], 3)

    def test_reset_on_an_empty_server_is_not_an_error(self):
        # 腕の切り替えは beacon が 1 本も来ていなくても起きうる。
        self.assertEqual(BeaconCounts().reset(), {})


if __name__ == "__main__":
    unittest.main()

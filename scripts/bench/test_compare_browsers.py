#!/usr/bin/env python3
"""compare_browsers.py の T2 判定ロジックのユニットテスト (Issue #197)。

実行方法:
    python3 -m unittest discover -s scripts/bench -p 'test_*.py' -v

テスト方針: ブラウザを実際に起動する部分は CI では回せないので、**判定ロジック
だけを切り出して固定する。** Windows では真の PSS が取れず区間でしか言えないため
(`proctree.py` 参照)、「判定できない場合に判定できないと言えるか」がこの実装の
肝になる。区間が重なっているのに `met` や `missed` を返す退行は、
**測れていないものを測れたことにする**バグなので、明示的に押さえておく。
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from compare_browsers import T2_THRESHOLD, compare_bounds  # noqa: E402

MIB = 1024 * 1024


class CompareBoundsTest(unittest.TestCase):
    def test_threshold_matches_the_documented_target(self):
        """T2 は「Chromium 比 +10% 以内」(docs/performance-targets.md)。"""
        self.assertAlmostEqual(T2_THRESHOLD, 0.10)

    def test_met_when_velox_upper_fits_under_baseline_lower_plus_threshold(self):
        """VeloX の上界が相手の下界 +10% 以下なら、真値がどこでも達成。"""
        verdict, detail = compare_bounds(
            velox_lower=90 * MIB, velox_upper=100 * MIB,
            base_lower=100 * MIB, base_upper=140 * MIB,
        )
        self.assertEqual(verdict, "met")
        self.assertIn("達成", detail)

    def test_missed_when_velox_lower_exceeds_baseline_upper_plus_threshold(self):
        """VeloX の下界が相手の上界 +10% 超なら、真値がどこでも未達。"""
        verdict, detail = compare_bounds(
            velox_lower=200 * MIB, velox_upper=300 * MIB,
            base_lower=100 * MIB, base_upper=150 * MIB,
        )
        self.assertEqual(verdict, "missed")
        self.assertIn("未達", detail)

    def test_inconclusive_when_the_intervals_overlap(self):
        """区間が重なるなら「判定不能」。**片方の端で断定しない。**"""
        verdict, detail = compare_bounds(
            velox_lower=90 * MIB, velox_upper=200 * MIB,
            base_lower=100 * MIB, base_upper=150 * MIB,
        )
        self.assertEqual(verdict, "inconclusive")
        self.assertIn("判定不能", detail)

    def test_exactly_at_the_threshold_counts_as_met(self):
        """ちょうど +10% は達成側 (「+10% 以内」なので境界を含む)。"""
        verdict, _ = compare_bounds(
            velox_lower=110 * MIB, velox_upper=110 * MIB,
            base_lower=100 * MIB, base_upper=100 * MIB,
        )
        self.assertEqual(verdict, "met")

    def test_linux_zero_width_intervals_behave_like_plain_pss_comparison(self):
        """Linux は下界 = 上界 = PSS なので、ふつうの PSS 比較に退化する。

        Windows 対応で判定を区間化したことが、**既存の Linux の評価方法を
        変えていない**ことを固定する。
        """
        met, _ = compare_bounds(100 * MIB, 100 * MIB, 100 * MIB, 100 * MIB)
        self.assertEqual(met, "met")
        missed, _ = compare_bounds(200 * MIB, 200 * MIB, 100 * MIB, 100 * MIB)
        self.assertEqual(missed, "missed")

    def test_unknown_when_values_are_missing(self):
        """値が欠けていたら判定しない (0 として扱って達成にしない)。"""
        verdict, _ = compare_bounds(None, 100 * MIB, 100 * MIB, 100 * MIB)
        self.assertEqual(verdict, "unknown")

    def test_unknown_when_baseline_is_zero(self):
        """相手のメモリが 0 は計測失敗。**0 除算より前に弾く。**"""
        verdict, _ = compare_bounds(100 * MIB, 100 * MIB, 0, 0)
        self.assertEqual(verdict, "unknown")


if __name__ == "__main__":
    unittest.main()

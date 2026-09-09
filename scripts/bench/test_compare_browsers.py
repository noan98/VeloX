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

import os
import subprocess
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from compare_browsers import (  # noqa: E402
    T2_THRESHOLD,
    compare_bounds,
    format_trial_line,
)

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


def trial_result(pss=None, private=None, rss=100 * MIB, unreadable=0):
    """`run_trial` が返す形。Linux は pss が、Windows は private が埋まる。"""
    lower = pss if pss is not None else private
    return {
        "startup_to_load_ms": 1234.5,
        "rss_bytes": rss,
        "pss_bytes": pss,
        "private_bytes": private,
        "lower_bytes": lower,
        "upper_bytes": pss if pss is not None else rss,
        "process_count": 8,
        "unreadable_count": unreadable,
    }


class FormatTrialLineTest(unittest.TestCase):
    """試行ごとの進捗表示 (Issue #197、run 34364659960 の回帰)。

    **サマリ表だけ Windows 対応にして、この 1 行を直し忘れた。** 計測は最後まで
    成功していたのに、結果を表示する段で `pss_bytes` (Windows では常に `None`)
    を割り算して `TypeError` になり、run 全体が失敗した。両 OS 分の形をここで
    踏んでおく。
    """

    def test_windows_shape_has_no_pss_and_must_not_crash(self):
        """Windows: `pss_bytes` は `None`。区間として表示する。"""
        line = format_trial_line(1, "velox", trial_result(private=40 * MIB))
        self.assertIn("mem=[40.0, 100.0]MiB", line)
        self.assertIn("procs=8", line)

    def test_linux_shape_shows_a_single_value(self):
        """Linux: 下界 = 上界なので区間ではなく 1 つの値で出す。"""
        line = format_trial_line(2, "chromium", trial_result(pss=60 * MIB))
        self.assertIn("mem=60.0MiB", line)

    def test_missing_lower_bound_is_shown_not_crashed(self):
        """下界が取れなくても落ちない。**取れなかったことを表示する。**"""
        result = trial_result(private=None)
        result["lower_bytes"] = None
        line = format_trial_line(3, "velox", result)
        self.assertIn("—", line)

    def test_unreadable_processes_are_surfaced(self):
        """読めなかったプロセスがあれば合計は過小評価。黙って出さない。"""
        line = format_trial_line(4, "velox", trial_result(private=40 * MIB, unreadable=2))
        self.assertIn("2 プロセスは読めず", line)


class OutputEncodingTest(unittest.TestCase):
    """日本語の出力で計測が落ちないこと (Issue #197、run 34364067651 の回帰)。

    Windows の Python は標準出力が cp1252 になることがあり、**日本語を
    `print` しただけで `UnicodeEncodeError` で落ちる。** Windows 対応の初回
    実行がまさにこれで失敗した — 比較相手の自動検出も計測ロジックも正しく
    動いていたのに、**「自動検出しました」と表示する行だけで計測全体が
    落ちた。**

    Linux でも `PYTHONIOENCODING=cp1252` を与えれば同じ条件を作れるので、
    Windows 実機なしで回帰を止められる。
    """

    def test_runs_under_a_non_utf8_stdout_encoding(self):
        script = Path(__file__).resolve().parent / "compare_browsers.py"
        result = subprocess.run(
            [sys.executable, str(script), "--help"],
            capture_output=True,
            text=True,
            # 日本語を表現できないコードページを強制する。修正前はここで
            # UnicodeEncodeError になっていた。
            env={**os.environ, "PYTHONIOENCODING": "cp1252"},
        )
        self.assertEqual(
            result.returncode,
            0,
            f"cp1252 の標準出力で失敗しました:\n{result.stderr}",
        )
        self.assertNotIn("UnicodeEncodeError", result.stderr)


if __name__ == "__main__":
    unittest.main()

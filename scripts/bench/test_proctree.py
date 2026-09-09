#!/usr/bin/env python3
"""proctree.py のユニットテスト (Issue #197)。

実行方法:
    python3 -m unittest discover -s scripts/bench -p 'test_*.py' -v
または (このディレクトリから):
    python3 -m unittest test_proctree -v

テスト方針: **Windows 実機はこの環境に無い** (docs/decisions.md D61/D88)。
そこで OS 依存の収集 (`/proc` 走査・Toolhelp32 スナップショット) と、木の走査・
集計ロジックを `collect_tree` で分離してある。ここで固定できるのは後者だけだが、
**バグが入りやすいのも後者**なので (親子関係の取り違え・循環・取得失敗の扱い)、
そこを全 OS で回帰させられる形にしておく価値がある。

Windows の FFI 部分 (`_windows_nodes`) はここでは検証できない。実機で確かめる
までは「未検証」と扱うこと — D88 が同じ理由で `QueryWorkingSetEx` の実装を
見送った経緯がある。
"""

from __future__ import annotations

import os
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from proctree import (  # noqa: E402
    WINDOWS_SHARE_COUNT_MAX,
    TreeMemory,
    _ProcMemory,
    collect_tree,
    process_tree_memory,
    supported,
)


def node(ppid: int, rss: int, pss: int | None = None, private: int | None = None):
    return (ppid, _ProcMemory(rss_bytes=rss, pss_bytes=pss, private_bytes=private))


class CollectTreeTest(unittest.TestCase):
    def test_sums_only_the_subtree_under_root(self):
        """兄弟のツリーを巻き込まない。

        ブラウザ比較では VeloX と Chromium を同じマシンで交互に起動するので、
        **他方のプロセスを数えてしまうと比較そのものが壊れる。**
        """
        nodes = {
            1: node(0, 100),
            10: node(1, 1000, pss=500),  # 対象ブラウザの親
            11: node(10, 2000, pss=700),  # その子
            12: node(11, 4000, pss=900),  # 孫まで辿る
            20: node(1, 8000, pss=8000),  # 別ブラウザ。含めてはいけない
        }
        result = collect_tree(10, nodes)
        self.assertEqual(result.rss_bytes, 1000 + 2000 + 4000)
        self.assertEqual(result.pss_bytes, 500 + 700 + 900)
        self.assertEqual(result.process_count, 3)

    def test_missing_root_yields_empty_result(self):
        """根が既に終了していたら、0 件として返す (例外にしない)。"""
        result = collect_tree(999, {1: node(0, 100)})
        self.assertEqual(result.rss_bytes, 0)
        self.assertEqual(result.process_count, 0)
        self.assertIsNone(result.pss_bytes)

    def test_parent_pid_cycle_terminates(self):
        """PID が再利用されて循環しても止まる。

        走査中にプロセスが入れ替わると、親子関係が循環した表を受け取りうる。
        無限ループになると計測が固まって原因も分かりにくいので、必ず止める。
        """
        nodes = {10: node(11, 100), 11: node(10, 200)}
        result = collect_tree(10, nodes)
        self.assertEqual(result.process_count, 2)
        self.assertEqual(result.rss_bytes, 300)

    def test_unreadable_processes_are_counted_but_not_summed(self):
        """権限不足などで読めなかったプロセスは、数には入るが合計には入らない。

        黙って落とすとプロセス数だけが辻褄の合わない値になる。`unreadable_count`
        を添えて「この合計は過小評価だ」と読み手に分かるようにしている。
        """
        nodes = {10: node(1, 1000, pss=500), 11: (10, None), 12: node(11, 300, pss=200)}
        result = collect_tree(10, nodes)
        self.assertEqual(result.process_count, 3)
        self.assertEqual(result.unreadable_count, 1)
        self.assertEqual(result.rss_bytes, 1300)
        self.assertEqual(result.pss_bytes, 700)

    def test_pss_stays_none_when_no_process_reported_it(self):
        """PSS を 1 つも取れなければ `None`。**0 と混同させない。**

        Windows は常にこの経路 (PSS が存在しない)。ここで 0 を返すと
        「メモリを使っていない」と読めてしまい、比較表で致命的に誤解を招く。
        """
        nodes = {10: node(1, 1000, private=400), 11: node(10, 2000, private=800)}
        result = collect_tree(10, nodes)
        self.assertIsNone(result.pss_bytes)
        self.assertEqual(result.private_bytes, 1200)


class BoundsTest(unittest.TestCase):
    """真の PSS を挟む上下界の扱い (Windows で T2 を評価するための土台)。"""

    def test_windows_bounds_bracket_the_true_pss(self):
        """Windows では private が下界、working set 合計が上界になる。"""
        result = TreeMemory(
            rss_bytes=1000,
            pss_bytes=None,
            private_bytes=400,
            process_count=3,
            unreadable_count=0,
        )
        self.assertEqual(result.lower_bytes, 400)
        self.assertEqual(result.upper_bytes, 1000)
        self.assertLessEqual(result.lower_bytes, result.upper_bytes)

    def test_linux_pss_is_both_bounds(self):
        """Linux は PSS がカーネル計算値なので、上下界が一致する (幅ゼロ)。"""
        result = TreeMemory(
            rss_bytes=1000,
            pss_bytes=600,
            private_bytes=None,
            process_count=3,
            unreadable_count=0,
        )
        self.assertEqual(result.lower_bytes, 600)
        self.assertEqual(result.upper_bytes, 600)

    def test_share_count_saturation_constant_is_three_bits(self):
        """`ShareCount` が 3 bit であることを定数として固定しておく。

        近似 PSS を作らない判断の根拠そのものなので、値が変わるなら
        判断も見直す必要がある (docs/decisions.md D88/D99)。
        """
        self.assertEqual(WINDOWS_SHARE_COUNT_MAX, 7)


@unittest.skipUnless(sys.platform.startswith("linux"), "Linux でのみ実測できる")
class LinuxLiveTest(unittest.TestCase):
    """実際に自プロセスを測って、値が正気か確かめる。"""

    def test_self_tree_reports_plausible_memory(self):
        result = process_tree_memory(os.getpid())
        self.assertTrue(supported())
        self.assertGreaterEqual(result.process_count, 1)
        # Python インタプリタが 1 MiB 未満ということはない。
        self.assertGreater(result.rss_bytes, 1024 * 1024)
        if result.pss_bytes is not None:
            # PSS は RSS を超えない (共有ページを割るのだから当然)。
            self.assertLessEqual(result.pss_bytes, result.rss_bytes)


if __name__ == "__main__":
    unittest.main()

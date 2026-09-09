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
    working_set_pages,
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
            pss_upper_bytes=650,
            process_count=3,
            unreadable_count=0,
        )
        self.assertEqual(result.lower_bytes, 400)
        # Working Set 合計 (1000) ではなく、締まった上界 (650) を使う。
        self.assertEqual(result.upper_bytes, 650)
        self.assertLessEqual(result.lower_bytes, result.upper_bytes)

    def test_linux_pss_is_both_bounds(self):
        """Linux は PSS がカーネル計算値なので、上下界が一致する (幅ゼロ)。"""
        result = TreeMemory(
            rss_bytes=1000,
            pss_bytes=600,
            private_bytes=None,
            pss_upper_bytes=None,
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


PAGE = 4096


def block(shared: bool, share_count: int = 0) -> int:
    """`PSAPI_WORKING_SET_BLOCK` を組み立てる。

    ビット配置は Protection:5, ShareCount:3, Shared:1 — つまり `ShareCount` は
    bit 5-7、`Shared` は bit 8。**この配置を間違えると数字が静かに狂う**ので、
    テスト側でも同じ規則で組み立てて往復させる。
    """
    value = (share_count & 0x7) << 5
    if shared:
        value |= 1 << 8
    return value


class WorkingSetPagesTest(unittest.TestCase):
    """`ShareCount` 由来の上界 (Issue #197、D99 決定1)。

    **D88 はこれを「近似値」として使うことを検討して見送った。** その懸念
    (`ShareCount` が 3 bit で 7 に飽和する) は事実だが、**上界として使うなら
    飽和は破綻しない** — 報告値 c に対して実際の共有数 n は必ず n >= c なので、
    `page_size / n <= page_size / c` が常に成り立つ。ここではその性質を固定する。
    """

    def test_private_pages_are_counted_in_full(self):
        pages = [block(shared=False)] * 10
        private, upper = working_set_pages(pages, PAGE)
        self.assertEqual(private, 10 * PAGE)
        # 共有ページが無ければ上界 = 私有 = 真の PSS。
        self.assertEqual(upper, 10 * PAGE)

    def test_shared_pages_are_excluded_from_the_lower_bound(self):
        """下界は私有ページのみ。共有ページは 1 枚も数えない。"""
        pages = [block(shared=True, share_count=2)] * 8
        private, upper = working_set_pages(pages, PAGE)
        self.assertEqual(private, 0)
        self.assertEqual(upper, 4 * PAGE)  # 8 ページ × 1/2

    def test_upper_bound_divides_by_share_count(self):
        """私有 2 + 共有 4 (c=4) → 上界 = 2 + 1 = 3 ページ分。"""
        pages = [block(shared=False)] * 2 + [block(shared=True, share_count=4)] * 4
        private, upper = working_set_pages(pages, PAGE)
        self.assertEqual(private, 2 * PAGE)
        self.assertEqual(upper, 3 * PAGE)

    def test_saturated_share_count_still_yields_a_valid_upper_bound(self):
        """**飽和しても上界であり続ける** — D88 の懸念への直接の回答。

        c=7 と報告されたページが実際には 20 プロセスで共有されていたとする。
        真の寄与は 1/20 だが、こちらは 1/7 で数えるので**多めに見積もる**。
        上界としては正しい (緩いだけ)。
        """
        pages = [block(shared=True, share_count=WINDOWS_SHARE_COUNT_MAX)] * 70
        _, upper = working_set_pages(pages, PAGE)
        true_pss_if_20_sharers = 70 / 20 * PAGE
        self.assertEqual(upper, 10 * PAGE)  # 70 × 1/7
        self.assertGreater(upper, true_pss_if_20_sharers)

    def test_upper_bound_never_exceeds_the_working_set_total(self):
        """上界は Working Set 合計 (c=1 と置いたのと同じ) を超えない。"""
        pages = [block(shared=False)] * 3 + [
            block(shared=True, share_count=c) for c in (1, 2, 3, 7)
        ]
        _, upper = working_set_pages(pages, PAGE)
        self.assertLessEqual(upper, len(pages) * PAGE)

    def test_zero_share_count_does_not_divide_by_zero(self):
        """共有ページで c=0 は本来ありえないが、落ちずに上界を保つ。"""
        pages = [block(shared=True, share_count=0)] * 5
        _, upper = working_set_pages(pages, PAGE)
        self.assertEqual(upper, 5 * PAGE)  # 1 とみなす = 最も緩い上界

    def test_empty_working_set(self):
        self.assertEqual(working_set_pages([], PAGE), (0, 0))


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

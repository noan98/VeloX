#!/usr/bin/env python3
"""`serve.py` の beacon 集計の単体テスト (Issue #247 / #128 の続き)。

## なぜこれが要るのか

`BeaconCounts` の doc コメント自身が「ソケットを持たない純粋なデータなので、
単体テストから直接叩ける」と書いているのに、**テストが無かった。**

そしてこの集計は、既に 2 回、Windows CI の計測を壊している:

- `docs/performance-targets.md` **§41** — 配線が繋がっておらず、beacon を
  数えたつもりで診断ステップの単発起動を数えていた。run ごと撤回
- **§42.4** — state ごとに first/last だけを持ち、12 個のタブと 8 回の
  起動を混ぜて引き算した。`advanced_frames` が **-5** になった

windows-latest の run は安くない。**集計が壊れていても run は緑で終わる**
(数字が出てしまう) ので、壊れたことは結果を読むまで分からない。ここが
その手前の網である。

`ci.yml` は `git ls-files '*/test_*.py'` で見つけたディレクトリを
すべて discover するので、このファイルは置くだけで CI に載る。
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from serve import BeaconCounts  # noqa: E402


def beacon(state: str, frames: int, ticks: int, instance: str, heap: int | None = None) -> str:
    """`busy.html` が実際に投げるクエリ文字列を組み立てる。

    形は `busy.html` の `fetch(...)` そのまま —
    `state=..&f=..&t=..&id=..[&h=..]`。テストが実物とずれないよう、
    ここ 1 箇所に寄せる。
    """
    query = f"state={state}&f={frames}&t={ticks}&id={instance}"
    if heap is not None:
        query += f"&h={heap}"
    return query


class RecordsPerInstance(unittest.TestCase):
    """§42.4 の壊れ方 (インスタンスをまたいで引き算する) が戻らないこと。"""

    def test_two_tabs_in_one_state_do_not_subtract_across_each_other(self):
        # 背景タブ 2 つ。片方は 100→110、もう片方は 5→9 まで進んだ。
        # インスタンスをまたいで引くと 9-100 = -91 のような負が出る。
        counts = BeaconCounts()
        counts.record(beacon("hidden", 100, 200, "tab-a"))
        counts.record(beacon("hidden", 5, 10, "tab-b"))
        counts.record(beacon("hidden", 110, 220, "tab-a"))
        counts.record(beacon("hidden", 9, 18, "tab-b"))

        hidden = counts.summary()["hidden"]
        self.assertEqual(hidden["instances"], 2)
        self.assertEqual(hidden["count"], 4)
        # (110-100) + (9-5) = 14、(220-200) + (18-10) = 28
        self.assertEqual(hidden["advanced_frames"], 14)
        self.assertEqual(hidden["advanced_ticks"], 28)

    def test_a_restarted_page_is_a_new_instance_and_never_goes_negative(self):
        # velox は 1 run で何度も起動し直し、そのたびにページのカウンタは
        # 0 から始まる。id が変わるので別インスタンスとして数えられる。
        counts = BeaconCounts()
        counts.record(beacon("hidden", 500, 900, "run-1"))
        counts.record(beacon("hidden", 520, 940, "run-1"))
        counts.record(beacon("hidden", 0, 0, "run-2"))
        counts.record(beacon("hidden", 3, 7, "run-2"))

        hidden = counts.summary()["hidden"]
        self.assertEqual(hidden["instances"], 2)
        self.assertEqual(hidden["advanced_frames"], 20 + 3)
        self.assertGreaterEqual(hidden["advanced_frames"], 0)
        self.assertGreaterEqual(hidden["advanced_ticks"], 0)

    def test_out_of_order_arrival_does_not_shrink_the_advance(self):
        # ページは `fetch` を投げっぱなしにし、応答も順序も待たない。
        # 先に届いたほうを first と決め打つと、順序が入れ替わっただけで
        # 進んだ量が縮む (最悪、負になる)。min/max で取ることの確認。
        counts = BeaconCounts()
        counts.record(beacon("hidden", 50, 80, "tab"))
        counts.record(beacon("hidden", 10, 20, "tab"))  # 遅れて届いた古い分
        counts.record(beacon("hidden", 30, 50, "tab"))

        hidden = counts.summary()["hidden"]
        self.assertEqual(hidden["advanced_frames"], 40)  # 50 - 10
        self.assertEqual(hidden["advanced_ticks"], 60)  # 80 - 20

    def test_beacons_without_an_id_are_pooled_and_visible_as_one_instance(self):
        # 古い `busy.html` は `id=` を送らない。初版と同じ壊れ方をするが、
        # **`instances` が 1 であること**からそれと分かる — 黙って
        # 誤らせないための設計 (`BeaconCounts` の doc コメント)。
        counts = BeaconCounts()
        counts.record("state=hidden&f=1&t=2")
        counts.record("state=hidden&f=9&t=9")

        hidden = counts.summary()["hidden"]
        self.assertEqual(hidden["instances"], 1)
        self.assertEqual(hidden["count"], 2)


class SeparatesStates(unittest.TestCase):
    """前景 / 背景を混ぜないこと。§43 の結論はこの分離に乗っている。"""

    def test_visible_and_hidden_are_counted_apart(self):
        counts = BeaconCounts()
        counts.record(beacon("visible", 0, 0, "fg"))
        counts.record(beacon("visible", 900, 100, "fg"))
        counts.record(beacon("hidden", 0, 0, "bg"))
        counts.record(beacon("hidden", 0, 110, "bg"))

        summary = counts.summary()
        # §43.4 が数字で出した形: 背景では描画が止まり、タイマーは動く。
        self.assertEqual(summary["visible"]["advanced_frames"], 900)
        self.assertEqual(summary["hidden"]["advanced_frames"], 0)
        self.assertEqual(summary["hidden"]["advanced_ticks"], 110)

    def test_a_beacon_without_a_state_lands_in_its_own_bucket(self):
        counts = BeaconCounts()
        counts.record("f=1&t=1&id=x")
        self.assertIn("unknown", counts.summary())


class ResetSeparatesTheArms(unittest.TestCase):
    """§42.5 の「両腕が同じバケットに積み上がる」が戻らないこと。

    ワークフローは A/B の腕の切り替えで `/beacon-reset` を叩く。
    **これが効かないと A/B 比較そのものが解釈不能になる** — しかも run は
    緑で終わるので、結果を読むまで気付けない。
    """

    def test_reset_returns_what_it_clears(self):
        counts = BeaconCounts()
        counts.record(beacon("hidden", 0, 0, "arm-a"))
        counts.record(beacon("hidden", 10, 20, "arm-a"))

        taken = counts.reset()
        self.assertEqual(taken["hidden"]["advanced_frames"], 10)
        self.assertEqual(taken["hidden"]["count"], 2)
        # 返した分は消えている — 次の腕に持ち越さない。
        self.assertEqual(counts.summary(), {})

    def test_the_second_arm_starts_from_zero(self):
        counts = BeaconCounts()
        counts.record(beacon("hidden", 0, 0, "arm-a"))
        counts.record(beacon("hidden", 100, 100, "arm-a"))
        counts.reset()

        counts.record(beacon("hidden", 0, 0, "arm-b"))
        counts.record(beacon("hidden", 3, 4, "arm-b"))

        hidden = counts.summary()["hidden"]
        self.assertEqual(hidden["instances"], 1, "A 腕のインスタンスが残っている")
        self.assertEqual(hidden["advanced_frames"], 3)
        self.assertEqual(hidden["advanced_ticks"], 4)

    def test_reset_on_an_empty_counter_is_harmless(self):
        self.assertEqual(BeaconCounts().reset(), {})


class HeapColumns(unittest.TestCase):
    """`h=` は Chromium 系にしか無い。**欠けていることと 0 は違う。**"""

    def test_heap_columns_are_dropped_entirely_when_nothing_reports_one(self):
        # 列が残って 0 が入っていると「ヒープが 0」と読めてしまう。
        counts = BeaconCounts()
        counts.record(beacon("hidden", 1, 1, "tab"))
        hidden = counts.summary()["hidden"]
        for key in ("heap_count", "heap_sum", "heap_max", "heap_mean"):
            self.assertNotIn(key, hidden)

    def test_the_mean_divides_by_the_beacons_that_carried_a_heap(self):
        # `count` で割ると、`performance.memory` の無い環境で値が薄まる。
        counts = BeaconCounts()
        counts.record(beacon("hidden", 1, 1, "tab", heap=100))
        counts.record(beacon("hidden", 2, 2, "tab"))  # h 無し
        counts.record(beacon("hidden", 3, 3, "tab", heap=300))

        hidden = counts.summary()["hidden"]
        self.assertEqual(hidden["count"], 3)
        self.assertEqual(hidden["heap_count"], 2)
        self.assertEqual(hidden["heap_max"], 300)
        self.assertEqual(hidden["heap_mean"], 200)  # 400 // 2、3 では割らない


class DropsBrokenBeaconsWithoutFailing(unittest.TestCase):
    """壊れた値で計測そのものを落とさないこと。

    `record` が例外を投げると、計測中のサーバが倒れる。集計の欠落より
    継続を優先する — ただし**黙って数に入れない**。
    """

    def test_missing_or_unparsable_counters_are_dropped(self):
        counts = BeaconCounts()
        for query in (
            "state=hidden&id=x",  # f / t が無い
            "state=hidden&f=abc&t=1&id=x",  # 数にならない
            "state=hidden&f=1&t=zzz&id=x",
            "state=hidden&f=-1&t=1&id=x",  # 負のカウンタはありえない
            "",
        ):
            counts.record(query)
        self.assertEqual(counts.summary(), {})

    def test_a_valid_beacon_still_lands_after_broken_ones(self):
        counts = BeaconCounts()
        counts.record("state=hidden&f=abc&t=1&id=x")
        counts.record(beacon("hidden", 1, 2, "x"))
        self.assertEqual(counts.summary()["hidden"]["count"], 1)


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""report.py のユニットテスト (Issue #211 項目2 / docs/decisions.md D104)。

実行方法:
    python3 -m unittest discover -s scripts/dashboard -p 'test_*.py' -v

テスト方針: `velox-bench` バイナリは CI に無い前提で、`build_report_model`
に `velox_bench_bin=None` を渡し、`common.classify_fallback` 経由の簡易判定
だけを固定する (D82 が元から想定しているフォールバック経路)。

固定したい不変条件は 1 つ: **`session_id` が同じでも機種が違えば、隣接
エントリを連結しない・差分を計算しない・同じ色にしない。** これは D82 の
「同一 session_id の隣接エントリだけを比較する」に、Issue #211 が「かつ
同一機種」を足した部分そのもの。
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from common import HistoryEntry  # noqa: E402
from report import assign_series_colors, build_report_model, series_key  # noqa: E402


def make_entry(
    session_id: str,
    generated_at: str,
    cpu_model: str | None = "AMD EPYC 9V74",
    cpu_count: int = 4,
    total_memory_bytes: int | None = 8 * 1024**3,
    line_no: int = 1,
    metric_value: float = 600.0,
    path: Path | None = None,
) -> HistoryEntry:
    env = {"os": "windows", "cpu_count": cpu_count, "generated_at": generated_at}
    if cpu_model is not None:
        env["cpu_model"] = cpu_model
    if total_memory_bytes is not None:
        env["total_memory_bytes"] = total_memory_bytes
    raw = {
        "schema_version": 1,
        "ingested_at": generated_at,
        "session_id": session_id,
        "source": "test",
        "branch": None,
        "pr_number": None,
        "note": None,
        "result": {
            "scenario": "cold_startup",
            "environment": env,
            "metrics": {"startup_window_created_ms": {"median": metric_value}},
        },
    }
    return HistoryEntry(
        os_name="windows",
        scenario="cold_startup",
        path=path or Path("results/history/windows/cold_startup.jsonl"),
        line_no=line_no,
        raw=raw,
    )


class SeriesKeyTest(unittest.TestCase):
    def test_same_session_and_machine_share_series_key(self):
        e1 = make_entry("S1", "2026-09-01T00:00:00Z", line_no=1)
        e2 = make_entry("S1", "2026-09-02T00:00:00Z", line_no=2)
        self.assertEqual(series_key(e1), series_key(e2))

    def test_same_session_different_machine_are_different_series(self):
        """D104 の核心: `session_id` だけでは同一系列と認めない。"""
        e1 = make_entry("S1", "2026-09-01T00:00:00Z", cpu_model="AMD EPYC 9V74", line_no=1)
        e2 = make_entry("S1", "2026-09-02T00:00:00Z", cpu_model="Intel Xeon 8573C", line_no=2)
        self.assertNotEqual(series_key(e1), series_key(e2))


class BuildReportModelTest(unittest.TestCase):
    def test_same_session_and_machine_connects_and_diffs(self):
        entries = [
            make_entry("S1", "2026-09-01T00:00:00Z", metric_value=600.0, line_no=1),
            make_entry("S1", "2026-09-02T00:00:00Z", metric_value=660.0, line_no=2),
        ]
        model = build_report_model(entries, velox_bench_bin=None)
        rows = model["by_os"]["windows"]["cold_startup"]["rows"]
        self.assertTrue(rows[0]["series_boundary"], "先頭のエントリは常に境界")
        self.assertFalse(rows[1]["series_boundary"], "同一機種・同一セッションは境界にならない")
        self.assertTrue(rows[1]["has_prev"])
        self.assertIn("startup_window_created_ms", rows[1]["diffs"])

    def test_same_session_different_machine_never_connects_or_diffs(self):
        """Issue #208 が観測した「windows-latest が run ごとに別機種を割り
        当てる」状況を、同じ session_id を使い回した場合として再現する。"""
        entries = [
            make_entry("S1", "2026-09-01T00:00:00Z", cpu_model="AMD EPYC 9V74", line_no=1),
            make_entry("S1", "2026-09-02T00:00:00Z", cpu_model="Intel Xeon 8573C", line_no=2),
        ]
        model = build_report_model(entries, velox_bench_bin=None)
        rows = model["by_os"]["windows"]["cold_startup"]["rows"]
        self.assertTrue(rows[1]["series_boundary"], "機種が変われば境界にならなければならない")
        self.assertFalse(rows[1]["has_prev"])
        self.assertEqual(rows[1]["diffs"], {})

    def test_unknown_machine_entries_never_connect_to_each_other(self):
        """機種不明どうしも安全側に倒して連結しない (D104)。"""
        entries = [
            make_entry("S1", "2026-09-01T00:00:00Z", cpu_model=None, line_no=1),
            make_entry("S1", "2026-09-02T00:00:00Z", cpu_model=None, line_no=2),
            make_entry("S1", "2026-09-03T00:00:00Z", cpu_model=None, line_no=3),
        ]
        model = build_report_model(entries, velox_bench_bin=None)
        rows = model["by_os"]["windows"]["cold_startup"]["rows"]
        for row in rows:
            self.assertTrue(row["series_boundary"])
            self.assertFalse(row["has_prev"])
            self.assertEqual(row["diffs"], {})

    def test_different_session_and_same_machine_still_does_not_connect(self):
        """機種を足しても、既存の D82 の規則 (session_id が違えば繋がない)
        を緩めてはいけない。"""
        entries = [
            make_entry("S1", "2026-09-01T00:00:00Z", line_no=1),
            make_entry("S2", "2026-09-02T00:00:00Z", line_no=2),
        ]
        model = build_report_model(entries, velox_bench_bin=None)
        rows = model["by_os"]["windows"]["cold_startup"]["rows"]
        self.assertTrue(rows[1]["series_boundary"])
        self.assertFalse(rows[1]["has_prev"])


class AssignSeriesColorsTest(unittest.TestCase):
    def test_same_session_different_machine_get_different_colors(self):
        """「同じ色の点だけが比較可能」という UI 上の約束を機種軸でも保つ:
        session_id が同じでも機種が違えば別の色になる。"""
        e1 = make_entry("S1", "2026-09-01T00:00:00Z", cpu_model="AMD EPYC 9V74", line_no=1)
        e2 = make_entry("S1", "2026-09-02T00:00:00Z", cpu_model="Intel Xeon 8573C", line_no=2)
        colors = assign_series_colors([e1, e2])
        self.assertNotEqual(colors[series_key(e1)], colors[series_key(e2)])


if __name__ == "__main__":
    unittest.main()

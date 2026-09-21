"""`perf_log_timeline.py` の単体テスト。

**なぜこのテストが要るか** (D148)

このスクリプトが出す「要求タブ数」は、`browser::suspension` の
`memory_budget_for_ram` / `tabs_to_free` の**複製**である。Rust 側と
ずれると、§46.5 の仮説 2 (要求が小さく出ている) を「確かめた」つもりで
別の式を見ていることになる。ここでは §46 で実測した機械
(15.99 GiB → 予算 1023 MiB) と、`tabs_to_free` のテストが固定している
性質 (切り上げ・超過があれば必ず 1 以上・見込み解放量 0 なら 0) を
そのまま固定する。

スイープのまとめ方も固定する: 1 回の判定で休止された 4 タブ (数 ms 差)
は 1 行に、5 秒後の次の判定は別の行になること。ここが崩れると
「5 秒に 4 タブ」が「5 秒に 1 タブ × 4 行」に見えて、問いそのものが
消える。
"""

from __future__ import annotations

import io
import json
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from perf_log_timeline import (  # noqa: E402
    ESTIMATED_BYTES_PER_TAB,
    MIB,
    build_timeline,
    expand_inputs,
    main,
    memory_budget_for_ram,
    parse_jsonl,
    render_markdown,
    tabs_to_free,
)

SCRIPT = Path(__file__).resolve().parent / "perf_log_timeline.py"


BROWSER_MIB = 27.0


# 私的コミット (D150) はテストでは rss の 8 割とする。ワーキングセットと
# 別の量として列に出ていることが分かればよい。
PRIVATE_RATIO = 0.8


def _rss(ts_ms: float, mib: float, processes: int = 10) -> dict:
    # 実際のレコードと同じく browser + engine = total を守る。
    private = int(mib * PRIVATE_RATIO * MIB)
    browser_private = int(BROWSER_MIB * PRIVATE_RATIO * MIB)
    return {
        "event": "rss",
        "ts_ms": ts_ms,
        "total_rss_bytes": int(mib * MIB),
        "total_pss_bytes": None,
        "process_count": processes,
        "browser_rss_bytes": int(BROWSER_MIB * MIB),
        "engine_rss_bytes": int((mib - BROWSER_MIB) * MIB),
        "total_private_bytes": private,
        "private_process_count": processes,
        "browser_private_bytes": browser_private,
        "engine_private_bytes": private - browser_private,
    }


def _suspend(ts_ms: float, tab_id: int, reason: str = "memory") -> dict:
    return {"event": "tab_suspend", "ts_ms": ts_ms, "tab_id": tab_id, "reason": reason}


def _lines(events: list[dict]) -> str:
    return "\n".join(json.dumps(e) for e in events) + "\n"


# §46 の run 2 と同じ形: mark の 1.7 秒後から 5 秒周期で 4 タブずつ。
SAMPLE_EVENTS = [
    {"event": "startup", "ts_ms": 1.0},
    _rss(9_000.0, 1200.0),
    {"event": "measure_start", "ts_ms": 10_000.0},
    _rss(11_000.0, 1150.0),
    _suspend(11_700.0, 5),
    _suspend(11_705.0, 6),
    _suspend(11_712.0, 7),
    _suspend(11_720.0, 8),
    _rss(12_000.0, 1090.0, processes=9),
    {"event": "tab_resume", "ts_ms": 13_000.0, "tab_id": 3},
    _rss(16_500.0, 1100.0),
    _suspend(16_700.0, 9),
    _suspend(16_704.0, 10),
    _suspend(16_709.0, 11),
    _suspend(16_715.0, 12),
    _rss(17_000.0, 1040.0, processes=8),
]


class BudgetFormulaTest(unittest.TestCase):
    def test_matches_section_46_machine(self) -> None:
        # 15.99 GiB (§46 のランナー) → 1023 MiB。Rust 側の
        # `memory_budget_for_ram` と同じ答えでなければならない。
        ram = int(15.99 * 1024 * MIB)
        self.assertEqual(memory_budget_for_ram(ram) // MIB, 1023)

    def test_clamps_to_min_and_max(self) -> None:
        self.assertEqual(memory_budget_for_ram(4 * 1024 * MIB), 700 * MIB)
        self.assertEqual(memory_budget_for_ram(64 * 1024 * MIB), 2048 * MIB)
        self.assertEqual(memory_budget_for_ram(None), 700 * MIB)


class TabsToFreeTest(unittest.TestCase):
    def test_rounds_up_and_never_returns_zero_when_over(self) -> None:
        per_tab = ESTIMATED_BYTES_PER_TAB
        budget = 1023 * MIB
        self.assertEqual(tabs_to_free(budget, budget, per_tab), 0)
        self.assertEqual(tabs_to_free(budget + 1, budget, per_tab), 1)
        self.assertEqual(tabs_to_free(budget + per_tab, budget, per_tab), 1)
        self.assertEqual(tabs_to_free(budget + per_tab + 1, budget, per_tab), 2)
        self.assertEqual(tabs_to_free(budget + 200 * MIB, budget, per_tab), 4)

    def test_zero_per_tab_means_zero_tabs(self) -> None:
        self.assertEqual(tabs_to_free(10_000 * MIB, 700 * MIB, 0), 0)


class TimelineTest(unittest.TestCase):
    def test_groups_one_sweep_per_decision(self) -> None:
        tl = build_timeline(Path("x-trial-1.jsonl"), parse_jsonl(_lines(SAMPLE_EVENTS)))
        self.assertEqual(tl.mark_ms, 10_000.0)
        self.assertEqual(len(tl.sweeps), 2)
        self.assertEqual([s.count for s in tl.sweeps], [4, 4])
        self.assertEqual(tl.suspended_total, 8)
        self.assertEqual(len(tl.sweeps_after_mark()), 2)
        self.assertEqual(len(tl.resumes), 1)

    def test_picks_the_rss_sample_just_before_and_after(self) -> None:
        tl = build_timeline(Path("x.jsonl"), parse_jsonl(_lines(SAMPLE_EVENTS)))
        first, second = tl.sweeps
        self.assertEqual(first.rss_before.total_rss_bytes, 1150 * MIB)
        self.assertEqual(first.rss_after.total_rss_bytes, 1090 * MIB)
        self.assertEqual(second.rss_before.total_rss_bytes, 1100 * MIB)
        self.assertEqual(second.rss_after.total_rss_bytes, 1040 * MIB)

    def test_uses_the_last_measure_start(self) -> None:
        events = [
            {"event": "measure_start", "ts_ms": 1_000.0},
            _suspend(2_000.0, 1),
            {"event": "measure_start", "ts_ms": 3_000.0},
            _suspend(4_000.0, 2),
        ]
        tl = build_timeline(Path("x.jsonl"), parse_jsonl(_lines(events)))
        self.assertEqual(tl.mark_ms, 3_000.0)
        self.assertEqual(len(tl.sweeps), 2)
        self.assertEqual(len(tl.sweeps_after_mark()), 1)

    def test_skips_broken_lines_and_records_without_ts(self) -> None:
        text = 'not json\n{"event":"rss"}\n' + _lines([_suspend(5.0, 1)])
        events = parse_jsonl(text)
        self.assertEqual(len(events), 1)


class RenderTest(unittest.TestCase):
    def test_markdown_shows_demand_next_to_actual(self) -> None:
        tl = build_timeline(Path("tabs_hold_bounce_50-windows-baseline-1-trial-1.jsonl"), parse_jsonl(_lines(SAMPLE_EVENTS)))
        budget = 1023 * MIB
        text = render_markdown([tl], budget, ESTIMATED_BYTES_PER_TAB, only_with_suspends=True)
        # 1150 MiB - 1023 MiB = 127 MiB 超過 → ceil(127 / 64) = 2 要求に対して 4 休止。
        # 末尾はプロセス数 / engine / browser / engine 私的 の直前→直後
        # (§47.4 の切り分け用、私的は D150)。engine 私的 = 0.8 × total − 0.8 × 27。
        self.assertIn(
            "| 1 | +1.7 | 4 | memory×4 | 1150.0 | 700 | 127.0 | 2 ⚠️ | 1090.0 | 10→9 | 1123.0→1063.0 | 27.0→27.0 "
            "| 898.4→850.4 |",
            text,
        )
        # 1100 - 1023 = 77 MiB → 2 要求、4 休止。
        self.assertIn(
            "| 2 | +6.7 | 4 | memory×4 | 1100.0 | 200 | 77.0 | 2 ⚠️ | 1040.0 | 10→8 | 1073.0→1013.0 | 27.0→27.0 "
            "| 858.4→810.4 |",
            text,
        )
        self.assertIn("予算 **1023 MiB**", text)

    def test_budget_input_private_judges_on_private_commit(self) -> None:
        # D151: `--budget-input private` は超過量と要求を私的コミット
        # (rss の 0.8 倍) から出す。1150 × 0.8 = 920 MiB は予算 900 MiB を
        # 20 MiB 超え → 要求 1。rss 直前の列そのものは変えない。
        from perf_log_timeline import judged_bytes

        tl = build_timeline(Path("b-trial-1.jsonl"), parse_jsonl(_lines(SAMPLE_EVENTS)))
        budget = 900 * MIB
        text = render_markdown([tl], budget, ESTIMATED_BYTES_PER_TAB, False, budget_input="private")
        self.assertIn("判定の入力 **`private`**", text)
        self.assertIn("| 1 | +1.7 | 4 | memory×4 | 1150.0 | 700 | 20.0 | 1 ⚠️ | 1090.0 |", text)
        # 既定 (resident) は rss で判定: 1150 − 900 = 250 → ceil(250/64) = 4。
        text = render_markdown([tl], budget, ESTIMATED_BYTES_PER_TAB, False)
        self.assertIn("| 1 | +1.7 | 4 | memory×4 | 1150.0 | 700 | 250.0 | 4 | 1090.0 |", text)
        # 私的コミットの無いログ (Linux / 古いログ) では private も rss に落ちる。
        old = tl.rss[0]
        old.total_private_bytes = None
        self.assertEqual(judged_bytes(old, "private"), old.total_rss_bytes)

    def test_detail_columns_degrade_to_dash_on_old_logs(self) -> None:
        # Issue #176 Stage 1 より前のログには browser / engine の内訳が無い。
        # 列ごと消すのではなく `-` にして、行の形を変えない。
        old = {"event": "rss", "ts_ms": 500.0, "total_rss_bytes": 1100 * MIB, "process_count": 12}
        events = [{"event": "measure_start", "ts_ms": 100.0}, old, _suspend(900.0, 1)]
        tl = build_timeline(Path("old.jsonl"), parse_jsonl(_lines(events)))
        text = render_markdown([tl], None, ESTIMATED_BYTES_PER_TAB, only_with_suspends=False)
        self.assertIn("| 1 | +0.8 | 1 | memory | 1100.0 | 400 | - | 12→- | -→- | -→- | -→- |", text)

    def test_private_columns_degrade_to_dash_without_the_field_or_when_null(self) -> None:
        # Linux / macOS の VeloX は `total_private_bytes: null` を書く (D150)。
        # 無い (古いログ) のと null なのは同じく `-` になる。
        linux = {**_rss(500.0, 1100.0), "total_private_bytes": None, "engine_private_bytes": None}
        events = [{"event": "measure_start", "ts_ms": 100.0}, linux, _suspend(900.0, 1), _rss(1_200.0, 1000.0)]
        tl = build_timeline(Path("linux.jsonl"), parse_jsonl(_lines(events)))
        self.assertIsNone(tl.rss[0].total_private_bytes)
        self.assertIsNone(tl.rss[0].engine_private_bytes)
        text = render_markdown([tl], None, ESTIMATED_BYTES_PER_TAB, only_with_suspends=False)
        self.assertIn("| 1073.0→973.0 | 27.0→27.0 | -→778.4 |", text)

    def test_markdown_omits_empty_logs_when_asked(self) -> None:
        empty = build_timeline(Path("cold_startup-trial-1.jsonl"), parse_jsonl(_lines([_rss(100.0, 300.0)])))
        text = render_markdown([empty], None, ESTIMATED_BYTES_PER_TAB, only_with_suspends=True)
        self.assertNotIn("cold_startup-trial-1.jsonl", text)
        self.assertIn("1 本は省略", text)
        text = render_markdown([empty], None, ESTIMATED_BYTES_PER_TAB, only_with_suspends=False)
        self.assertIn("(休止なし)", text)


class RssTrackTest(unittest.TestCase):
    def test_tracks_rss_from_mark_in_steps_and_counts_suspends_per_window(self) -> None:
        from perf_log_timeline import render_rss_track

        tl = build_timeline(Path("b-trial-1.jsonl"), parse_jsonl(_lines(SAMPLE_EVENTS)))
        text = render_rss_track([tl], 5000.0, markdown=True)
        # mark = 10.0s。+0 は 9.0s のサンプル (1200 MiB)、+5 は 12.0s の
        # サンプル (1090 MiB、その区間に 4 休止)。最後のサンプルは 17.0s
        # なので +10 (20.0s) には追いついておらず、行にしない — 最後の
        # サンプルは quit の途中で採られうるため (run 35601333889)。
        # 末尾 3 列は私的コミット (D150): 総量 / 直前の行からの差 / engine 側。
        self.assertIn("| +0 | 1200.0 | - | 0 | 10 | 1173.0 | 27.0 | 960.0 | - | 938.4 |", text)
        self.assertIn("| +5 | 1090.0 | -110.0 | 4 | 9 | 1063.0 | 27.0 | 872.0 | -88.0 | 850.4 |", text)
        self.assertNotIn("| +10 |", text)
        # 20.0s 以降にサンプルがあれば +10 の行が出て、16.7s の 4 休止を数える。
        longer = build_timeline(Path("b.jsonl"), parse_jsonl(_lines(SAMPLE_EVENTS + [_rss(20_500.0, 1041.0, 8)])))
        text = render_rss_track([longer], 5000.0, markdown=True)
        self.assertIn("| +10 | 1040.0 | -50.0 | 4 | 8 | 1013.0 | 27.0 | 832.0 | -40.0 | 810.4 |", text)

    def test_track_private_delta_is_dash_when_either_side_is_missing(self) -> None:
        from perf_log_timeline import render_rss_track

        # +0 の行は私的コミットあり、+5 の行は無し (null) → 差は `-`、
        # 次に戻っても直前が無いので `-`。
        events = [
            _rss(9_000.0, 1200.0),
            {"event": "measure_start", "ts_ms": 10_000.0},
            {**_rss(14_500.0, 1100.0), "total_private_bytes": None, "engine_private_bytes": None},
            _rss(19_500.0, 1050.0),
            _rss(21_000.0, 1050.0),
        ]
        tl = build_timeline(Path("b.jsonl"), parse_jsonl(_lines(events)))
        text = render_rss_track([tl], 5000.0, markdown=True)
        self.assertIn("| +0 | 1200.0 | - | 0 | 10 | 1173.0 | 27.0 | 960.0 | - | 938.4 |", text)
        self.assertIn("| +5 | 1100.0 | -100.0 | 0 | 10 | 1073.0 | 27.0 | - | - | - |", text)
        self.assertIn("| +10 | 1050.0 | -50.0 | 0 | 10 | 1023.0 | 27.0 | 840.0 | - | 818.4 |", text)

    def test_logs_without_mark_are_skipped_and_counted(self) -> None:
        from perf_log_timeline import render_rss_track

        no_mark = build_timeline(Path("cold-trial-1.jsonl"), parse_jsonl(_lines([_rss(100.0, 300.0)])))
        text = render_rss_track([no_mark], 5000.0, markdown=True)
        self.assertNotIn("cold-trial-1.jsonl", text)
        self.assertIn("1 本は省略", text)

    def test_rejects_non_positive_step(self) -> None:
        # step が 0 以下だと刻みが進まず無限ループになる (PR #286 のレビュー
        # 指摘)。関数と CLI の両方で弾く。
        from perf_log_timeline import render_rss_track

        tl = build_timeline(Path("b.jsonl"), parse_jsonl(_lines(SAMPLE_EVENTS)))
        with self.assertRaises(ValueError):
            render_rss_track([tl], 0.0, markdown=True)
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "b-trial-1.jsonl"
            log.write_text(_lines(SAMPLE_EVENTS), encoding="utf-8")
            with redirect_stdout(io.StringIO()), self.assertRaises(SystemExit) as raised:
                saved, sys.stderr = sys.stderr, io.StringIO()
                try:
                    main([str(log), "--rss-track", "--track-step-ms", "0"])
                finally:
                    sys.stderr = saved
            self.assertEqual(raised.exception.code, 2)

    def test_cli_switch_selects_the_track_view(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "b-trial-1.jsonl"
            log.write_text(_lines(SAMPLE_EVENTS), encoding="utf-8")
            out = io.StringIO()
            with redirect_stdout(out):
                code = main([str(log), "--rss-track", "--markdown"])
            self.assertEqual(code, 0)
            self.assertIn("秒刻みの rss の推移", out.getvalue())
            self.assertNotIn("休止スイープの時系列", out.getvalue())


class CliTest(unittest.TestCase):
    def test_reads_a_directory_and_computes_budget_from_ram(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "b-trial-1.jsonl").write_text(_lines(SAMPLE_EVENTS), encoding="utf-8")
            (root / "a-trial-1.jsonl").write_text(_lines([_rss(1.0, 100.0)]), encoding="utf-8")
            (root / "ignored.json").write_text("{}", encoding="utf-8")
            self.assertEqual([p.name for p in expand_inputs([tmp])], ["a-trial-1.jsonl", "b-trial-1.jsonl"])
            out = io.StringIO()
            with redirect_stdout(out):
                code = main([tmp, "--ram-bytes", str(int(15.99 * 1024 * MIB)), "--markdown", "--only-with-suspends"])
            self.assertEqual(code, 0)
            self.assertIn("予算 **1023 MiB**", out.getvalue())
            self.assertIn("b-trial-1.jsonl", out.getvalue())
            self.assertNotIn("#### `a-trial-1.jsonl`", out.getvalue())

    def test_writes_utf8_even_when_stdout_is_a_legacy_code_page(self) -> None:
        # perf-windows の初回 (run 35561470137) は、Windows の Python が
        # stdout を cp1252 で開いたために表の日本語で `UnicodeEncodeError`
        # になった。`PYTHONIOENCODING=cp1252` で同じ状況を作り、それでも
        # UTF-8 で書けることを固定する。
        import os
        import subprocess

        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "b-trial-1.jsonl"
            log.write_text(_lines(SAMPLE_EVENTS), encoding="utf-8")
            env = {**os.environ, "PYTHONIOENCODING": "cp1252"}
            env.pop("PYTHONUTF8", None)
            proc = subprocess.run(
                [sys.executable, str(SCRIPT), str(log), "--budget-bytes", str(1023 * MIB), "--markdown"],
                capture_output=True,
                env=env,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr.decode("utf-8", "replace"))
            self.assertIn("休止スイープの時系列", proc.stdout.decode("utf-8"))

    def test_returns_1_when_nothing_could_be_read(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            err = io.StringIO()
            with redirect_stdout(io.StringIO()):
                sys.stderr, saved = err, sys.stderr
                try:
                    code = main([str(Path(tmp) / "missing.jsonl")])
                finally:
                    sys.stderr = saved
            self.assertEqual(code, 1)


if __name__ == "__main__":
    unittest.main()

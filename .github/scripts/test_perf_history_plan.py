"""`perf_history_plan.py` の単体テスト。

**なぜこのテストが要るか**

取り込みが走るのは週 1 回の Windows schedule run だけで、しかも
**間違っても CI は緑のまま**である。壊れ方は 2 通りあり、どちらも
静かに起きる:

1. 取り込みが 0 件 — 「貯めているつもりで何も貯まっていない」
2. 別条件の数値が同じ系列に入る — **比較してはならない数値が繋がる**

2 のほうが有害である (1 は気付けば取り返せるが、2 は誤った結論を作る)。
D82 / D96 / D106 が一貫して塞いできたのはこの穴なので、系列 ID の決め方を
ここで固定する。
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from perf_history_plan import (  # noqa: E402
    IngestionPlan,
    ingested_run_ids,
    main,
    plan_ingestion,
    slugify_condition,
)

SCRIPT = Path(__file__).resolve().parent / "perf_history_plan.py"

AB_FILES = [
    "results/cold_startup-windows-baseline-1.json",
    "results/cold_startup-windows-baseline-2.json",
    "results/cold_startup-windows-compare-1.json",
    "results/cold_startup-windows-compare-2.json",
]


def _plan(paths: list[str], **kwargs) -> IngestionPlan:
    kwargs.setdefault("run_id", "1")
    return plan_ingestion(paths, **kwargs)


class ArmSeparationTest(unittest.TestCase):
    def test_the_two_arms_never_share_a_session_id(self) -> None:
        plan = _plan(AB_FILES, compare_env="VELOX_MEMORY_BUDGET_MB=0")
        self.assertEqual(len(plan.groups), 2)
        sessions = [g.session_id for g in plan.groups]
        self.assertEqual(len(set(sessions)), 2, sessions)

    def test_each_arm_gets_only_its_own_files(self) -> None:
        plan = _plan(AB_FILES, compare_env="VELOX_MEMORY_BUDGET_MB=0")
        by_session = {g.session_id: g.results for g in plan.groups}
        for session, results in by_session.items():
            marker = "baseline" if session.endswith("-A") else "compare"
            for path in results:
                self.assertIn(marker, path, f"{session} に別の腕のファイルが入っている: {path}")

    def test_a_single_arm_run_is_its_own_series(self) -> None:
        plan = _plan(["results/cold_startup-windows.json"])
        self.assertEqual(len(plan.groups), 1)
        self.assertIn("single", plan.groups[0].session_id)


class ConditionInTheSessionIdTest(unittest.TestCase):
    """条件が変われば系列が割れることを固定する (このモジュールの核心)。"""

    def _b_session(self, compare_env: str, common_env: str = "") -> str:
        plan = _plan(AB_FILES, compare_env=compare_env, common_env=common_env)
        return [g.session_id for g in plan.groups if "compare" in g.results[0]][0]

    def test_changing_the_compare_condition_splits_the_series(self) -> None:
        self.assertNotEqual(
            self._b_session("VELOX_MEMORY_BUDGET_MB=0"),
            self._b_session("VELOX_MEMORY_BUDGET_MB=512"),
        )

    def test_whitespace_only_differences_do_not_split_the_series(self) -> None:
        self.assertEqual(
            self._b_session("VELOX_MEMORY_BUDGET_MB=0"),
            self._b_session("  VELOX_MEMORY_BUDGET_MB = 0 ; "),
        )

    def test_common_env_also_splits_both_arms(self) -> None:
        plain = _plan(AB_FILES, compare_env="X=1")
        with_common = _plan(AB_FILES, compare_env="X=1", common_env="VELOX_LOW_MEMORY=1")
        self.assertEqual(len(plain.groups), len(with_common.groups))
        for a, b in zip(plain.groups, with_common.groups):
            self.assertNotEqual(a.session_id, b.session_id)

    def test_the_condition_is_kept_verbatim_in_the_note(self) -> None:
        plan = _plan(AB_FILES, compare_env="VELOX_MEMORY_BUDGET_MB=0")
        notes = " ".join(g.note for g in plan.groups)
        self.assertIn("compare_env=VELOX_MEMORY_BUDGET_MB=0", notes)

    def test_the_run_id_is_always_in_every_note(self) -> None:
        plan = _plan(AB_FILES, compare_env="X=1", run_id="4242")
        for group in plan.groups:
            self.assertIn("run_id=4242", group.note)


class RefusalTest(unittest.TestCase):
    def test_compare_results_without_a_condition_are_not_ingested(self) -> None:
        """何を測ったか分からない数値を既定の系列に混ぜない。"""
        plan = _plan(AB_FILES, compare_env="")
        sessions = [g.session_id for g in plan.groups]
        self.assertEqual(len(sessions), 1, sessions)
        self.assertTrue(any("compare_env" in w for w in plan.warnings), plan.warnings)

    def test_unrecognised_file_names_are_reported_not_dropped(self) -> None:
        plan = _plan(["results/something-else.txt.json"])
        self.assertTrue(plan.is_empty)
        self.assertTrue(any("判定できない" in w for w in plan.warnings), plan.warnings)


class SlugTest(unittest.TestCase):
    def test_separators_become_safe_characters(self) -> None:
        self.assertEqual(slugify_condition("A=1;B=2"), "A=1+B=2")

    def test_an_empty_condition_is_an_empty_slug(self) -> None:
        self.assertEqual(slugify_condition("  ;  "), "")

    def test_shell_metacharacters_are_flattened(self) -> None:
        self.assertNotIn("$", slugify_condition("A=$(whoami)"))


class IngestedRunIdsTest(unittest.TestCase):
    def _history(self, notes: list[str | None]) -> Path:
        tmp = Path(tempfile.mkdtemp())
        target = tmp / "windows" / "cold_startup.jsonl"
        target.parent.mkdir(parents=True)
        with target.open("w", encoding="utf-8") as handle:
            for note in notes:
                handle.write(json.dumps({"note": note}) + "\n")
        return tmp

    def test_it_finds_the_run_ids(self) -> None:
        history = self._history(["perf-windows A / run_id=777 / x", "perf-windows B / run_id=778"])
        self.assertEqual(ingested_run_ids(history), {"777", "778"})

    def test_a_missing_directory_is_simply_empty(self) -> None:
        self.assertEqual(ingested_run_ids(Path(tempfile.mkdtemp()) / "nope"), set())

    def test_broken_lines_do_not_stop_the_scan(self) -> None:
        history = self._history(["perf-windows A / run_id=777"])
        target = history / "windows" / "cold_startup.jsonl"
        target.write_text("{not json\n" + target.read_text(encoding="utf-8"), encoding="utf-8")
        self.assertEqual(ingested_run_ids(history), {"777"})

    def test_entries_without_a_note_are_ignored(self) -> None:
        self.assertEqual(ingested_run_ids(self._history([None])), set())


class CliTest(unittest.TestCase):
    """配線が壊れているときは落とす (D131 決定5 と同じ扱い)。"""

    def test_a_missing_results_directory_fails(self) -> None:
        code = main(["--results-dir", "/nonexistent/x", "--run-id", "1", "--dry-run"])
        self.assertEqual(code, 1)

    def test_an_empty_results_directory_fails(self) -> None:
        tmp = Path(tempfile.mkdtemp())
        code = main(["--results-dir", str(tmp), "--run-id", "1", "--dry-run"])
        self.assertEqual(code, 1)

    def test_an_already_ingested_run_is_skipped_without_error(self) -> None:
        tmp = Path(tempfile.mkdtemp())
        results = tmp / "results"
        results.mkdir()
        (results / "cold_startup-windows.json").write_text("{}", encoding="utf-8")
        history = tmp / "history" / "windows"
        history.mkdir(parents=True)
        (history / "cold_startup.jsonl").write_text(
            json.dumps({"note": "perf-windows 単独計測 / run_id=55"}) + "\n", encoding="utf-8"
        )
        code = main(
            [
                "--results-dir",
                str(results),
                "--history-dir",
                str(tmp / "history"),
                "--run-id",
                "55",
                "--dry-run",
            ]
        )
        self.assertEqual(code, 0)

    def test_the_script_runs_as_a_program(self) -> None:
        """import できるだけでなく、workflow が呼ぶ形でも動くこと。"""
        completed = subprocess.run(
            [sys.executable, str(SCRIPT), "--results-dir", "/nonexistent/x", "--run-id", "1"],
            capture_output=True,
            text=True,
        )
        self.assertEqual(completed.returncode, 1)
        self.assertIn("::error::", completed.stdout)


if __name__ == "__main__":
    unittest.main()

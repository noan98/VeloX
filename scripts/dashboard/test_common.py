#!/usr/bin/env python3
"""common.py のユニットテスト (Issue #211 項目2 / docs/decisions.md D106)。

実行方法:
    python3 -m unittest discover -s scripts/dashboard -p 'test_*.py' -v

テスト方針: 「機種」を比較のもう一段の単位にする変更の核心は
`derive_machine_key` (純粋関数) と、それを使う `HistoryEntry.machine_key`
にある。ここでは:

  1. 既知の機種同士は os/cpu_model/cpu_count/メモリ量だけで決まり、
     salt に依存しないこと (同じ機種なら常に同じ key)。
  2. **機種不明 (cpu_model 無し) のエントリは、salt が異なれば必ず
     異なる key になり、決して他のエントリと同一視されないこと**
     (D106 の「安全側に倒す」を固定する — ここが壊れると、比較しては
     いけない古いエントリ同士が誤って連結される)。
  3. `results/history/` の v1 エントリ (`environment` に `cpu_model` 等が
     無い) が引き続き問題なく読み込めること (後方互換)。

を固定する。
"""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from common import (  # noqa: E402
    HistoryEntry,
    append_jsonl,
    derive_machine_key,
    history_file_for,
    machine_label,
    read_history,
)


class DeriveMachineKeyTest(unittest.TestCase):
    def test_same_known_machine_yields_the_same_key_regardless_of_salt(self):
        """既知の機種は salt を無視する — 同じ機種は「いつ計測しても」同じ
        key になってほしい (D96 が観測した4機種を判別できることの裏返し)。"""
        env = {
            "os": "windows",
            "cpu_model": "AMD EPYC 9V74 80-Core Processor",
            "cpu_count": 2,
            "total_memory_bytes": 8 * 1024**3,
        }
        key_a = derive_machine_key(env, salt="line-1")
        key_b = derive_machine_key(env, salt="line-999")
        self.assertEqual(key_a, key_b)

    def test_different_cpu_model_yields_different_key(self):
        """Issue #208 で観測された4機種 (AMD EPYC 9V74 / Intel Xeon 8573C /
        Intel Xeon 6973P-C / AMD EPYC 7763) は、それぞれ別の key になる
        必要がある — これが「同一機種の run 同士でのみ比較する」の土台。"""
        base = {"os": "windows", "cpu_count": 4, "total_memory_bytes": 16 * 1024**3}
        models = [
            "AMD EPYC 9V74",
            "Intel Xeon Platinum 8573C",
            "Intel Xeon 6973P-C",
            "AMD EPYC 7763",
        ]
        keys = {derive_machine_key({**base, "cpu_model": m}, salt="s") for m in models}
        self.assertEqual(len(keys), len(models), "4機種すべてが別の key になっていない")

    def test_different_cpu_count_yields_different_key(self):
        """D96 は同じ `windows-latest` でも論理コア数が 2 と 4 で割れることを
        観測している。cpu_model が仮に同じでも core 数が違えば別機種扱い。"""
        env_2core = {"os": "windows", "cpu_model": "Same Model", "cpu_count": 2}
        env_4core = {"os": "windows", "cpu_model": "Same Model", "cpu_count": 4}
        self.assertNotEqual(
            derive_machine_key(env_2core, salt="s"),
            derive_machine_key(env_4core, salt="s"),
        )

    def test_memory_is_bucketed_to_the_nearest_gib_to_absorb_os_reporting_jitter(self):
        """OS が報告する総メモリはバイト単位で微妙にブレうる (予約領域等)。
        同じ物理機械なら GiB に丸めた後は一致してほしい。"""
        env = {"os": "windows", "cpu_model": "M", "cpu_count": 4}
        # 8 GiB ちょうど周辺の微小なブレ (数 MiB 差) は同じバケツに丸まる。
        key_a = derive_machine_key({**env, "total_memory_bytes": 8 * 1024**3}, salt="s")
        key_b = derive_machine_key(
            {**env, "total_memory_bytes": 8 * 1024**3 - 4 * 1024 * 1024}, salt="s"
        )
        self.assertEqual(key_a, key_b)

    def test_unknown_machine_never_equals_another_unknown_machine_with_different_salt(self):
        """D106 の核心: cpu_model が無い「機種不明」は、salt が違えば必ず
        別の key になる。**これが崩れると、比較してはいけない機種不明の
        エントリ同士が誤って同一機種として連結される。**"""
        env = {"os": "windows", "cpu_count": 4}  # cpu_model が無い
        key_a = derive_machine_key(env, salt="path.jsonl:3")
        key_b = derive_machine_key(env, salt="path.jsonl:4")
        self.assertNotEqual(key_a, key_b)

    def test_unknown_machine_with_the_same_salt_is_stable(self):
        """同じ salt (= 同じエントリ) を渡せば毎回同じ key になる — 純粋関数
        であることの確認 (report.py を複数回実行しても結果が揺れない)。"""
        env = {"os": "linux", "cpu_count": 8}
        self.assertEqual(
            derive_machine_key(env, salt="x:1"),
            derive_machine_key(env, salt="x:1"),
        )

    def test_missing_cpu_model_key_entirely_is_treated_as_unknown(self):
        """v1 エントリのように `cpu_model` キー自体が無い場合も「機種不明」
        として扱う (KeyError にならないこと、None と同じ扱いになること)。"""
        env_no_key = {"os": "linux", "cpu_count": 4}
        env_none = {"os": "linux", "cpu_count": 4, "cpu_model": None}
        self.assertEqual(
            derive_machine_key(env_no_key, salt="s"),
            derive_machine_key(env_none, salt="s"),
        )


class MachineLabelTest(unittest.TestCase):
    def test_unknown_when_cpu_model_missing(self):
        self.assertEqual(machine_label({"os": "linux"}), "機種不明")

    def test_shows_model_and_core_count_when_present(self):
        label = machine_label({"cpu_model": "AMD EPYC 9V74", "cpu_count": 2})
        self.assertIn("AMD EPYC 9V74", label)
        self.assertIn("2", label)


class HistoryEntryMachineKeyTest(unittest.TestCase):
    """`HistoryEntry.machine_key` は `path:line_no` を salt に使う。実際に
    ファイルへ書いて `read_history` 経由で読み戻し、行番号ベースの salt が
    実際に機能することを確認する。"""

    def _write(self, tmp: Path, entries: list[dict]) -> Path:
        dest = history_file_for(tmp, "windows", "cold_startup")
        for e in entries:
            append_jsonl(dest, e)
        return dest

    def _entry(self, session_id: str, cpu_model: str | None, **env_extra) -> dict:
        env = {"os": "windows", "cpu_count": 4, "generated_at": "2026-09-01T00:00:00Z"}
        if cpu_model is not None:
            env["cpu_model"] = cpu_model
        env.update(env_extra)
        return {
            "schema_version": 1,
            "ingested_at": "2026-09-01T00:00:00Z",
            "session_id": session_id,
            "source": "test",
            "branch": None,
            "pr_number": None,
            "note": None,
            "result": {
                "scenario": "cold_startup",
                "environment": env,
                "metrics": {"startup_window_created_ms": {"median": 600.0}},
            },
        }

    def test_two_unknown_machine_entries_in_the_same_file_get_different_keys(self):
        with tempfile.TemporaryDirectory() as d:
            tmp = Path(d)
            self._write(
                tmp,
                [
                    self._entry("S1", cpu_model=None),
                    self._entry("S1", cpu_model=None),
                ],
            )
            entries = read_history(tmp)
            self.assertEqual(len(entries), 2)
            self.assertNotEqual(entries[0].machine_key, entries[1].machine_key)

    def test_two_known_same_machine_entries_get_the_same_key(self):
        with tempfile.TemporaryDirectory() as d:
            tmp = Path(d)
            self._write(
                tmp,
                [
                    self._entry("S1", cpu_model="AMD EPYC 9V74", total_memory_bytes=8 * 1024**3),
                    self._entry("S1", cpu_model="AMD EPYC 9V74", total_memory_bytes=8 * 1024**3),
                ],
            )
            entries = read_history(tmp)
            self.assertEqual(entries[0].machine_key, entries[1].machine_key)

    def test_v1_entry_without_environment_machine_fields_still_parses(self):
        """後方互換の固定: item1 (機種メタデータ追加) 以前に書かれた v1
        エントリ (cpu_model/total_memory_bytes/os_version/webview_runtime が
        存在しない) を読んでも例外にならず、「機種不明」として扱われる。"""
        with tempfile.TemporaryDirectory() as d:
            tmp = Path(d)
            legacy = self._entry("S1", cpu_model=None)
            # 明示的に item1 のフィールドを一切含まない、正真正銘の v1 形。
            self.assertNotIn("cpu_model", legacy["result"]["environment"])
            self._write(tmp, [legacy])
            entries = read_history(tmp)
            self.assertEqual(len(entries), 1)
            self.assertEqual(entries[0].machine_display, "機種不明")
            # 例外を出さずに一意な key が引ける。
            self.assertTrue(entries[0].machine_key.startswith("unknown:"))


if __name__ == "__main__":
    unittest.main()

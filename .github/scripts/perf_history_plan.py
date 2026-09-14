#!/usr/bin/env python3
"""perf-windows.yml の計測結果を `results/history/` に取り込む計画を立てる。

Issue #211 項目4 の後半 / `docs/decisions.md` D106 Revisit condition (1)。

## なぜ「計画」を別モジュールに切り出すのか

取り込みの本体は `scripts/dashboard/record.py` (既存) と git/PR 操作
(シェル) だが、**どの結果ファイルをどの `--session-id` でまとめるか**は
純粋な判断であり、ここを間違えると「比較してはならない数値が 1 本の系列
として繋がる」という、D82 / D96 / D106 が一貫して塞いできた事故になる。

その判断だけを純粋関数として切り出し、Linux 上の単体テストで固定する
(workflow の中に埋めると **Windows run を 1 回使い切るまで誰も気付けない**
— `test_perf_windows_inputs.py` の冒頭が書いているのと同じ理由)。

## 系列 (`session_id`) の決め方

`report.py` は **`session_id` と `machine_key` の両方が一致する隣接エントリ
だけ**を線でつなぐ (D106)。機種差は `machine_key` が既に担保しているので、
ここで担保すべきなのは残りの軸 — **「何を測ったか」** である。

| 軸 | 誰が担保するか |
| --- | --- |
| 機種 (CPU/コア数/RAM/OS) | `machine_key` (D106) |
| 計測条件 (A 腕 / B 腕、環境変数) | **`session_id` (このモジュール)** |
| シナリオ | `results/history/<os>/<scenario>.jsonl` のファイル分割 |

したがって A 腕と B 腕は必ず別系列にする。さらに **B 条件 (`compare_env`)
の文字列そのものを `session_id` に埋める** — 将来 `compare_env` の既定値が
変わったとき、**同じ系列の中で測っているものが静かに入れ替わる**のを防ぐ
ため。条件が変われば系列 ID も変わり、線は切れる (D106 が機種について
採ったのと同じ「変わったら繋がない」方向)。`common_env` も同様に扱う。

## 冪等性

`record.py` は単純な追記なので、同じ run を 2 度取り込めば 2 行入る
(`workflow_dispatch` の re-run で普通に起こりうる)。各エントリの `note` に
`run_id=<GitHub Actions の run id>` を必ず入れ、既に同じ run id が履歴に
あれば取り込みを行わない。
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from pathlib import Path

# 腕を表すファイル名の接尾辞。perf-windows.yml の `Invoke-Bench` が
# 書き出す名前と対応する:
#   単独計測  results/<scenario>-windows.json
#   A 腕      results/<scenario>-windows-baseline-<r>.json
#   B 腕      results/<scenario>-windows-compare-<r>.json
_BASELINE_RE = re.compile(r"-windows-baseline-\d+\.json$")
_COMPARE_RE = re.compile(r"-windows-compare-\d+\.json$")
_SINGLE_RE = re.compile(r"-windows\.json$")

# `session_id` に埋め込んでよい文字。`report.py` は session_id を HTML の
# 系列ラベルにも使うため、記号は最小限に潰す (値そのものは `note` に
# 生のまま残るので、情報は失われない)。
_SLUG_UNSAFE_RE = re.compile(r"[^A-Za-z0-9._=+-]+")

SOURCE = "ci-perf-windows-schedule"


def slugify_condition(spec: str) -> str:
    """`KEY=VALUE;KEY2=VALUE2` を `session_id` に埋められる形にする。

    **空白の差だけで系列が割れないように**前後を刈り、区切りを正規化する。
    `VELOX_MEMORY_BUDGET_MB=0` と ` VELOX_MEMORY_BUDGET_MB = 0 ` は同じ
    条件なので、同じ slug にならなければならない。
    """
    parts = []
    for entry in spec.split(";"):
        entry = entry.strip()
        if not entry:
            continue
        if "=" in entry:
            key, _, value = entry.partition("=")
            entry = f"{key.strip()}={value.strip()}"
        parts.append(entry)
    joined = "+".join(parts)
    return _SLUG_UNSAFE_RE.sub("-", joined)


@dataclass(frozen=True)
class IngestionGroup:
    """`record.py` の 1 回の呼び出しに対応する。"""

    session_id: str
    results: tuple[str, ...]
    note: str

    def command(self, *, history_dir: str | None = None) -> list[str]:
        """`record.py` に渡す argv (プログラム名を除く) を組み立てる。"""
        argv = ["--source", SOURCE, "--session-id", self.session_id, "--branch", "main", "--note", self.note]
        for path in self.results:
            argv += ["--result", path]
        if history_dir is not None:
            argv += ["--history-dir", history_dir]
        return argv


@dataclass(frozen=True)
class IngestionPlan:
    groups: tuple[IngestionGroup, ...] = ()
    warnings: tuple[str, ...] = field(default=())

    @property
    def is_empty(self) -> bool:
        return not self.groups


def _note(arm: str, *, run_id: str, run_url: str | None, compare_env: str, common_env: str) -> str:
    bits = [f"perf-windows {arm}", f"run_id={run_id}"]
    if compare_env:
        bits.append(f"compare_env={compare_env}")
    if common_env:
        bits.append(f"common_env={common_env}")
    if run_url:
        bits.append(run_url)
    return " / ".join(bits)


def plan_ingestion(
    result_paths: list[str],
    *,
    compare_env: str = "",
    common_env: str = "",
    run_id: str,
    run_url: str | None = None,
) -> IngestionPlan:
    """結果ファイル一覧から `record.py` の呼び出し計画を作る。

    ファイル名だけで腕を判定する。**判定できないファイルは黙って捨てず、
    警告として返す** — 「取り込んだつもりで 0 件」という壊れ方 (CI は緑の
    まま目的を達成しない、D102/D103/D106 が繰り返し踏んできた形) を
    避けるため。
    """
    baseline: list[str] = []
    compare: list[str] = []
    single: list[str] = []
    warnings: list[str] = []

    for path in sorted(result_paths):
        name = Path(path).name
        if _BASELINE_RE.search(name):
            baseline.append(path)
        elif _COMPARE_RE.search(name):
            compare.append(path)
        elif _SINGLE_RE.search(name):
            single.append(path)
        else:
            warnings.append(f"腕を判定できないため取り込みません: {path}")

    common_slug = slugify_condition(common_env)
    compare_slug = slugify_condition(compare_env)

    def arm_session(arm: str, extra: str = "") -> str:
        parts = [f"perf-windows-schedule-{arm}"]
        if extra:
            parts.append(extra)
        if common_slug:
            parts.append(f"common={common_slug}")
        return ":".join(parts)

    groups: list[IngestionGroup] = []
    if single:
        groups.append(
            IngestionGroup(
                session_id=arm_session("single"),
                results=tuple(single),
                note=_note("単独計測", run_id=run_id, run_url=run_url, compare_env="", common_env=common_env),
            )
        )
    if baseline:
        groups.append(
            IngestionGroup(
                session_id=arm_session("A"),
                results=tuple(baseline),
                note=_note("A (既定)", run_id=run_id, run_url=run_url, compare_env="", common_env=common_env),
            )
        )
    if compare:
        if not compare_slug:
            # B 腕のファイルがあるのに条件が分からない状態。**ここで
            # 既定の系列に混ぜるのが最悪の選択** — 何を測ったか分からない
            # 数値が A 腕の系列に紛れ込む。取り込まずに警告する。
            warnings.append(
                "B 腕の結果がありますが compare_env が空です。"
                "何を測った数値か決められないため取り込みません "
                f"({len(compare)} 件)"
            )
        else:
            groups.append(
                IngestionGroup(
                    session_id=arm_session("B", compare_slug),
                    results=tuple(compare),
                    note=_note(
                        "B (比較条件)", run_id=run_id, run_url=run_url, compare_env=compare_env, common_env=common_env
                    ),
                )
            )

    return IngestionPlan(groups=tuple(groups), warnings=tuple(warnings))


def ingested_run_ids(history_dir: Path) -> set[str]:
    """履歴に既に入っている run id を集める (冪等性の判定に使う)。

    壊れた行は無視する — 履歴の 1 行が壊れていることを理由に取り込みを
    止めても、誰も得をしない。
    """
    found: set[str] = set()
    if not history_dir.is_dir():
        return found
    for path in sorted(history_dir.rglob("*.jsonl")):
        try:
            text = path.read_text(encoding="utf-8")
        except OSError:
            continue
        for line in text.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                entry = json.loads(line)
            except json.JSONDecodeError:
                continue
            note = entry.get("note")
            if not isinstance(note, str):
                continue
            for match in re.finditer(r"\brun_id=(\S+)", note):
                found.add(match.group(1))
    return found


def main(argv: list[str] | None = None) -> int:
    """結果ディレクトリを読み、`record.py` を実際に呼ぶ。

    **「取り込んだつもりで 0 件」は必ず落とす。** このジョブは
    `needs: perf-windows` が成功した後にしか走らないので、結果ファイルが
    1 つも無いのは計測の失敗ではなく**配線の失敗**である
    (`check_closing_keywords.py` の D131 決定5 と同じ扱い)。
    """
    import argparse
    import subprocess
    import sys

    parser = argparse.ArgumentParser(description="perf-windows の結果を results/history に取り込む")
    parser.add_argument("--results-dir", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--run-url")
    parser.add_argument("--compare-env", default="")
    parser.add_argument("--common-env", default="")
    parser.add_argument("--history-dir")
    parser.add_argument("--record-script", default="scripts/dashboard/record.py")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args(argv)

    results_dir = Path(args.results_dir)
    if not results_dir.is_dir():
        print(f"::error::結果ディレクトリがありません: {results_dir}。取り込みの配線が壊れています。")
        return 1

    result_paths = [str(p) for p in sorted(results_dir.glob("*.json"))]
    if not result_paths:
        print(
            f"::error::{results_dir} に結果 JSON が 1 件もありません。"
            "計測ジョブは成功しているので、これは配線の失敗です。"
        )
        return 1

    history_dir = Path(args.history_dir) if args.history_dir else Path("results/history")
    if args.run_id in ingested_run_ids(history_dir):
        print(f"run_id={args.run_id} は既に取り込み済みです。何もしません。")
        return 0

    plan = plan_ingestion(
        result_paths,
        compare_env=args.compare_env,
        common_env=args.common_env,
        run_id=args.run_id,
        run_url=args.run_url,
    )
    for warning in plan.warnings:
        print(f"::warning::{warning}")

    if plan.is_empty:
        print("::error::取り込める結果がありませんでした (腕を判定できるファイルが 0 件)。")
        return 1

    for group in plan.groups:
        argv_rest = group.command(history_dir=str(history_dir))
        command = [sys.executable, args.record_script, *argv_rest]
        print(f"$ {' '.join(command)}")
        if args.dry_run:
            continue
        completed = subprocess.run(command, check=False)
        if completed.returncode != 0:
            print(f"::error::record.py が失敗しました (session_id={group.session_id})")
            return completed.returncode

    return 0


if __name__ == "__main__":  # pragma: no cover
    import sys

    sys.exit(main())

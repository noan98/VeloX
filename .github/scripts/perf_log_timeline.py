#!/usr/bin/env python3
"""perf ログ (JSON Lines) から、メモリ判定のスイープごとの休止を時系列で並べる。

**何のためにあるか** (Issue #176 Stage 3 / D145 Revisit condition (1) / D147 / D148)

`velox-bench run --output` の結果 JSON は集計値 (中央値・サンプル一覧) しか
持たない。`docs/performance-targets.md` §46.5 が残した問い —
「50 タブ + `LOW` で `mark` の後も 5 秒ごとに 4 タブずつ休止が続くのは、
1 タブあたりの見込み解放量 (`ESTIMATED_BYTES_PER_TAB` = 64 MiB) が
Windows の実態より大きく、1 回の要求が小さく出ているからか」— は、
**1 回のスイープが何タブを休止させ、その直前のメモリが予算をどれだけ
超えていたか**を並べて初めて答えられる。

このスクリプトは `--keep-logs` (D147) で残した試行ごとの生ログを読み、
trial ごとに次を表にする:

- `tab_suspend` を**スイープ**にまとめる (`app::sweep_tabs` は 1 回の
  判定で複数タブを休止し、その `ts_ms` はほぼ同じになる。ここでは
  間隔 `SWEEP_GAP_MS` 以内の連続した `tab_suspend` を 1 スイープとみなす)
- スイープ直前の `rss` サンプル (`total_rss_bytes`) と、その予算超過量
- 超過量から `suspension::tabs_to_free` と同じ算術で求めた**要求タブ数**
  (`ceil(超過 / 見込み解放量)`) — 実際の休止数と並べると、「要求が小さく
  出ている」のか「要求どおりに取れていない」のかが分かれる

**計算の前提と限界**

- 予算は perf ログに書かれていない。`--budget-bytes` で直接与えるか、
  `--ram-bytes` から `suspension::memory_budget_for_ram` と同じ式
  (`clamp(RAM / 16, 700 MiB, 2048 MiB)`) で求める。式の定数はここに
  複製しているので、Rust 側を変えたらここも変えること
  (`test_perf_log_timeline.py` が §46 の実測値 15.99 GiB → 1023 MiB で
  固定している)。
- `rss` レコードは perf の RSS サンプラ (`VELOX_PERF_RSS_INTERVAL_MS`)、
  メモリ判定は別のサンプラ (`VELOX_MEMORY_CHECK_INTERVAL_MS`、既定 5 秒)
  が採るので、「直前の `rss`」は判定が見た値そのものではなく、その近似
  である。表には `rss` の古さ (スイープまでの経過 ms) を出す。
- Windows では PSS が採れないため、判定は `total_rss_bytes` を使う
  (`app.rs` の `total_pss_bytes.unwrap_or(total_rss_bytes)`)。Linux では
  判定は PSS を見るので、この表の「超過量」は Linux では過大になる。

使い方:

    perf_log_timeline.py <file.jsonl | dir> [...] [--ram-bytes N | --budget-bytes N]
        [--per-tab-bytes N] [--markdown] [--only-with-suspends]

ディレクトリを渡すとその直下の `*.jsonl` を名前順に読む。終了コードは
読めたファイルが 1 つも無いときだけ 1、それ以外は 0 (診断ツールなので
CI を落とさない)。
"""

from __future__ import annotations

import json
import math
import sys
from dataclasses import dataclass, field
from pathlib import Path

# 1 スイープとみなす `tab_suspend` 同士の最大間隔。`sweep_tabs` は 1 回の
# 判定の中で連続して休止するので実際の間隔は数 ms〜数十 ms、判定と判定の
# 間は既定 5 秒 (`VELOX_MEMORY_CHECK_INTERVAL_MS`)。この間のどこに置いても
# 結果は変わらないが、周期を 500ms に詰めた計測 (Issue #197) でも
# 隣の判定と混ざらないよう、その半分より小さく取る。
SWEEP_GAP_MS = 200.0

MIB = 1024 * 1024

# `browser::suspension` の複製 (docstring 参照)。
ESTIMATED_BYTES_PER_TAB = 64 * MIB
MIN_MEMORY_BUDGET_BYTES = 700 * MIB
MAX_MEMORY_BUDGET_BYTES = 2048 * MIB
MEMORY_BUDGET_RAM_DIVISOR = 16


def memory_budget_for_ram(ram_bytes: int | None) -> int:
    """`suspension::memory_budget_for_ram` と同じ式。"""
    if ram_bytes is None:
        return MIN_MEMORY_BUDGET_BYTES
    return min(max(ram_bytes // MEMORY_BUDGET_RAM_DIVISOR, MIN_MEMORY_BUDGET_BYTES), MAX_MEMORY_BUDGET_BYTES)


def tabs_to_free(total: int, budget: int, per_tab: int) -> int:
    """`suspension::tabs_to_free` と同じ式 (0 除算を拒む点も含めて)。"""
    if per_tab <= 0:
        return 0
    excess = max(total - budget, 0)
    if excess == 0:
        return 0
    return max(math.ceil(excess / per_tab), 1)


@dataclass
class RssSample:
    ts_ms: float
    total_rss_bytes: int
    process_count: int | None


@dataclass
class Sweep:
    """1 回のメモリ判定で休止されたタブの束。"""

    start_ms: float
    end_ms: float
    tab_ids: list[int] = field(default_factory=list)
    reasons: dict[str, int] = field(default_factory=dict)
    rss_before: RssSample | None = None
    rss_after: RssSample | None = None

    @property
    def count(self) -> int:
        return len(self.tab_ids)

    def reason_text(self) -> str:
        return ", ".join(f"{name}×{n}" if n > 1 else name for name, n in sorted(self.reasons.items()))


@dataclass
class Timeline:
    path: Path
    records: int
    mark_ms: float | None
    sweeps: list[Sweep]
    resumes: list[float]

    @property
    def suspended_total(self) -> int:
        return sum(s.count for s in self.sweeps)

    def sweeps_after_mark(self) -> list[Sweep]:
        if self.mark_ms is None:
            return []
        return [s for s in self.sweeps if s.start_ms > self.mark_ms]


def parse_jsonl(text: str) -> list[dict]:
    """壊れた行は読み飛ばす (`benchmark::parse_jsonl` と同じ寛容さ)。"""
    events: list[dict] = []
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and isinstance(value.get("ts_ms"), (int, float)):
            events.append(value)
    return events


def build_timeline(path: Path, events: list[dict]) -> Timeline:
    events = sorted(events, key=lambda e: e["ts_ms"])
    rss: list[RssSample] = []
    suspends: list[dict] = []
    resumes: list[float] = []
    mark_ms: float | None = None
    for event in events:
        kind = event.get("event")
        ts = float(event["ts_ms"])
        if kind == "rss":
            total = event.get("total_rss_bytes")
            if isinstance(total, (int, float)):
                count = event.get("process_count")
                rss.append(RssSample(ts, int(total), int(count) if isinstance(count, int) else None))
        elif kind == "tab_suspend":
            suspends.append(event)
        elif kind == "tab_resume":
            resumes.append(ts)
        elif kind == "measure_start":
            # 最後の `measure_start` を採る (D145 決定2 と同じ規約)。
            mark_ms = ts

    sweeps: list[Sweep] = []
    for event in suspends:
        ts = float(event["ts_ms"])
        if sweeps and ts - sweeps[-1].end_ms <= SWEEP_GAP_MS:
            sweep = sweeps[-1]
            sweep.end_ms = ts
        else:
            sweep = Sweep(start_ms=ts, end_ms=ts)
            sweeps.append(sweep)
        tab_id = event.get("tab_id")
        sweep.tab_ids.append(int(tab_id) if isinstance(tab_id, int) else -1)
        reason = str(event.get("reason", "?"))
        sweep.reasons[reason] = sweep.reasons.get(reason, 0) + 1

    for sweep in sweeps:
        before = [s for s in rss if s.ts_ms < sweep.start_ms]
        after = [s for s in rss if s.ts_ms > sweep.end_ms]
        sweep.rss_before = before[-1] if before else None
        sweep.rss_after = after[0] if after else None

    return Timeline(path=path, records=len(events), mark_ms=mark_ms, sweeps=sweeps, resumes=resumes)


def expand_inputs(inputs: list[str]) -> list[Path]:
    paths: list[Path] = []
    for raw in inputs:
        path = Path(raw)
        if path.is_dir():
            paths.extend(sorted(p for p in path.iterdir() if p.suffix == ".jsonl" and p.is_file()))
        else:
            paths.append(path)
    return paths


def _mib(value: int | None) -> str:
    return "-" if value is None else f"{value / MIB:.1f}"


def _rel(ts_ms: float, mark_ms: float | None) -> str:
    """`mark` からの相対秒。`mark` が無ければプロセス開始からの絶対秒。"""
    if mark_ms is None:
        return f"{ts_ms / 1000:.1f}"
    return f"{(ts_ms - mark_ms) / 1000:+.1f}"


def render_markdown(
    timelines: list[Timeline],
    budget: int | None,
    per_tab: int,
    only_with_suspends: bool,
) -> str:
    lines: list[str] = []
    lines.append("### 休止スイープの時系列 (D147 / D148、§46.5 の検証用)")
    lines.append("")
    if budget is None:
        lines.append(
            "予算が与えられていないため、超過量と要求タブ数は出していない "
            "(`--ram-bytes` か `--budget-bytes` を渡す)。"
        )
    else:
        lines.append(
            f"予算 **{budget / MIB:.0f} MiB**、見込み解放量 **{per_tab / MIB:.0f} MiB/タブ** として、"
            "各スイープの直前の `rss` から `tabs_to_free` と同じ式で「要求タブ数」を求めている。"
            "**要求 = 実際なら、判定は要求どおりに取れていて、収束が遅いのは要求が小さく出ているから** "
            "(§46.5 仮説 2)。実際 > 要求なら疑似プロセスグループの丸ごと回収 (仮説 1) の寄与がある。"
        )
    lines.append("")
    lines.append(
        "時刻は最後の `measure_start` (`mark`) からの相対秒 (無ければプロセス開始から)。"
        "`rss` は perf のサンプラの値で、判定が見た値そのものではない (「古さ」列はスイープまでの経過 ms)。"
    )
    lines.append("")

    skipped = 0
    for tl in timelines:
        if only_with_suspends and not tl.sweeps:
            skipped += 1
            continue
        after = tl.sweeps_after_mark()
        lines.append(f"#### `{tl.path.name}`")
        lines.append("")
        mark_text = "-" if tl.mark_ms is None else f"{tl.mark_ms / 1000:.1f}s"
        lines.append(
            f"レコード {tl.records} 件 / `mark` {mark_text} / 休止 {tl.suspended_total} タブ "
            f"({len(tl.sweeps)} スイープ、うち `mark` 後 {sum(s.count for s in after)} タブ・{len(after)} スイープ) / "
            f"復帰 {len(tl.resumes)} 回"
        )
        lines.append("")
        if not tl.sweeps:
            lines.append("(休止なし)")
            lines.append("")
            continue
        if budget is None:
            lines.append("| # | t (s) | 休止 | 理由 | rss 直前 (MiB) | 古さ (ms) | プロセス | rss 直後 (MiB) |")
            lines.append("| ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: |")
        else:
            lines.append(
                "| # | t (s) | 休止 | 理由 | rss 直前 (MiB) | 古さ (ms) | 超過 (MiB) | 要求 | rss 直後 (MiB) |"
            )
            lines.append("| ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: |")
        for index, sweep in enumerate(tl.sweeps, start=1):
            before = sweep.rss_before
            after_rss = sweep.rss_after
            age = "-" if before is None else f"{sweep.start_ms - before.ts_ms:.0f}"
            cells = [
                str(index),
                _rel(sweep.start_ms, tl.mark_ms),
                str(sweep.count),
                sweep.reason_text(),
                _mib(before.total_rss_bytes if before else None),
                age,
            ]
            if budget is None:
                cells.append("-" if before is None or before.process_count is None else str(before.process_count))
            else:
                if before is None:
                    cells.extend(["-", "-"])
                else:
                    over = max(before.total_rss_bytes - budget, 0)
                    demand = tabs_to_free(before.total_rss_bytes, budget, per_tab)
                    marker = "" if demand == sweep.count else (" ⚠️" if sweep.count > demand else " ↓")
                    cells.extend([f"{over / MIB:.1f}", f"{demand}{marker}"])
            cells.append(_mib(after_rss.total_rss_bytes if after_rss else None))
            lines.append("| " + " | ".join(cells) + " |")
        lines.append("")
    if skipped:
        lines.append(f"(休止が 1 件も無いログ {skipped} 本は省略)")
        lines.append("")
    return "\n".join(lines)


def render_plain(timelines: list[Timeline], budget: int | None, per_tab: int, only_with_suspends: bool) -> str:
    lines: list[str] = []
    for tl in timelines:
        if only_with_suspends and not tl.sweeps:
            continue
        mark_text = "-" if tl.mark_ms is None else f"{tl.mark_ms / 1000:.1f}s"
        lines.append(
            f"{tl.path.name}: records={tl.records} mark={mark_text} suspended={tl.suspended_total} "
            f"sweeps={len(tl.sweeps)} resumes={len(tl.resumes)}"
        )
        for index, sweep in enumerate(tl.sweeps, start=1):
            before = sweep.rss_before
            parts = [
                f"  #{index} t={_rel(sweep.start_ms, tl.mark_ms)}s tabs={sweep.count} ({sweep.reason_text()})",
                f"rss_before={_mib(before.total_rss_bytes if before else None)}MiB",
            ]
            if budget is not None and before is not None:
                parts.append(f"over={max(before.total_rss_bytes - budget, 0) / MIB:.1f}MiB")
                parts.append(f"demand={tabs_to_free(before.total_rss_bytes, budget, per_tab)}")
            parts.append(f"rss_after={_mib(sweep.rss_after.total_rss_bytes if sweep.rss_after else None)}MiB")
            lines.append(" ".join(parts))
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    import argparse

    parser = argparse.ArgumentParser(description="perf ログから休止スイープの時系列を出す")
    parser.add_argument("inputs", nargs="+", help="perf ログ (.jsonl) かそれを含むディレクトリ")
    budget_group = parser.add_mutually_exclusive_group()
    budget_group.add_argument("--ram-bytes", type=int, help="搭載 RAM (bytes)。予算を RAM 相対の式で求める")
    budget_group.add_argument("--budget-bytes", type=int, help="メモリ予算 (bytes) を直接与える")
    parser.add_argument(
        "--per-tab-bytes",
        type=int,
        default=ESTIMATED_BYTES_PER_TAB,
        help=f"1 タブあたりの見込み解放量 (bytes、既定 {ESTIMATED_BYTES_PER_TAB})",
    )
    parser.add_argument("--markdown", action="store_true", help="Job Summary 向けの Markdown で出す")
    parser.add_argument(
        "--only-with-suspends",
        action="store_true",
        help="tab_suspend が 1 件も無いログは省略する (cold_startup などで表が空にならないように)",
    )
    args = parser.parse_args(argv)

    budget: int | None
    if args.budget_bytes is not None:
        budget = args.budget_bytes
    elif args.ram_bytes is not None:
        budget = memory_budget_for_ram(args.ram_bytes)
    else:
        budget = None

    timelines: list[Timeline] = []
    for path in expand_inputs(args.inputs):
        try:
            text = path.read_text(encoding="utf-8")
        except OSError as err:
            print(f"perf_log_timeline: {path} を読めません: {err}", file=sys.stderr)
            continue
        timelines.append(build_timeline(path, parse_jsonl(text)))
    if not timelines:
        print("perf_log_timeline: 読めた perf ログがありません", file=sys.stderr)
        return 1

    if args.markdown:
        print(render_markdown(timelines, budget, args.per_tab_bytes, args.only_with_suspends))
    else:
        print(render_plain(timelines, budget, args.per_tab_bytes, args.only_with_suspends))
    return 0


if __name__ == "__main__":
    sys.exit(main())

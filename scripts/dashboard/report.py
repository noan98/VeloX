#!/usr/bin/env python3
"""Performance Dashboard (Issue #71) — 履歴ストアから静的レポートを生成する。

`results/history/<os>/<scenario>.jsonl` (record.py が追記したもの) を読み、
OS ごと・シナリオごとに時系列を並べた HTML (既定) / Markdown (任意) の
レポートを生成する。外部サービスへの依存は増やさない — 生成物はローカルの
静的ファイルで、ブラウザで直接開ける。グラフは matplotlib 等を使わず、素の
SVG を文字列として組み立てている (新規依存なし)。

## 「異なるセッション/マシンの数値を比較してはならない」原則の担保

`common.py` のモジュール docstring に設計の全体像がある。このスクリプトが
UI 側で守っている規則は 1 つ (Issue #211 項目2 / docs/decisions.md D106 で
「機種」の軸を追加した):

**同一 `session_id` かつ同一機種 (`machine_key`) の隣接エントリ同士だけを
線でつなぎ、`velox-bench gate` で差分の重大度 (OK/WARN/FAIL) を計算する。
`session_id` または機種のどちらかが異なる点は、時系列グラフ上には
(どちらも) 表示するが、線ではつながず、差分バッジも出さない。**

これにより「グラフ上に複数セッション/機種の点が乗っていても、線が切れている
箇所は比較不可能」であることが一目でわかるようにしてある。「機種不明」
(`environment.cpu_model` が無い、item1 以前の古いエントリなど) は安全側に
倒し、他のどのエントリとも同一機種として連結しない
(`common.derive_machine_key` 参照)。

## threshold (閾値) 表示について

差分バッジの OK/WARN/FAIL は、可能な限り `velox-bench gate`
(`benchmark::evaluate_gate`、`GateThresholds` 既定値: warn 20% / fail 60% +
メトリクスごとの最小絶対差) をそのまま呼び出して計算する — CI の
回帰ゲート (`.github/workflows/perf-gate.yml`) と**同じロジック**。
`velox-bench` バイナリが見つからない場合のみ、`common.classify_fallback`
による簡易判定 (絶対差フロアなし) にフォールバックし、その旨を明示する。

使い方
------
    python3 scripts/dashboard/report.py \\
      --output results/history/report.html \\
      --markdown-output results/history/report.md
"""

from __future__ import annotations

import argparse
import html
import json
import subprocess
import sys
import tempfile
from collections import defaultdict
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))
from common import (  # noqa: E402
    DEFAULT_HISTORY_DIR,
    FALLBACK_FAIL_PCT,
    FALLBACK_WARN_PCT,
    METRIC_GROUPS,
    HistoryEntry,
    classify_fallback,
    metric_display_value,
    metric_label,
    metric_unit,
    read_history,
    short_sha,
)

# Issue #211 項目2 / D106。「同一 session_id かつ同一機種」だけが比較可能
# な系列 — `session_id` (無ければ "unknown-session") と `machine_key` の
# 組。session_id だけの D82 の単位に、機種という軸を 1 つ足したもの。
SeriesKey = tuple[str, str]


def series_key(entry: HistoryEntry) -> SeriesKey:
    return (entry.session_id or "unknown-session", entry.machine_key)

DEFAULT_REPO_URL = "https://github.com/noan98/VeloX"

PALETTE = [
    "#4C72B0",
    "#DD8452",
    "#55A868",
    "#C44E52",
    "#8172B2",
    "#937860",
    "#DA8BC3",
    "#8C8C8C",
    "#CCB974",
    "#64B5CD",
]

SEVERITY_CLASS = {"OK": "sev-ok", "WARN": "sev-warn", "FAIL": "sev-fail", "N/A": "sev-na"}


def parse_time(value: str) -> datetime:
    if not value:
        return datetime.min
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return datetime.min


def find_velox_bench_bin(explicit: str | None) -> Path | None:
    if explicit:
        p = Path(explicit)
        return p if p.exists() else None
    from common import REPO_ROOT

    for candidate in ("target/release/velox-bench", "target/debug/velox-bench"):
        p = REPO_ROOT / candidate
        if p.exists():
            return p
    return None


@dataclass
class Diff:
    """1 メトリクスぶんの「前の同一セッション内エントリとの差分」。"""

    pct_change: float
    severity: str
    approximate: bool  # True = velox-bench gate を呼べず簡易判定した


def run_gate(
    bin_path: Path,
    baseline_result: dict,
    candidate_result: dict,
    workdir: Path,
) -> dict | None:
    baseline_path = workdir / "baseline.json"
    candidate_path = workdir / "candidate.json"
    out_path = workdir / "gate.json"
    baseline_path.write_text(json.dumps(baseline_result), encoding="utf-8")
    candidate_path.write_text(json.dumps(candidate_result), encoding="utf-8")
    try:
        proc = subprocess.run(
            [
                str(bin_path),
                "gate",
                "--baseline",
                str(baseline_path),
                "--candidate",
                str(candidate_path),
                "--output",
                str(out_path),
            ],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        print(f"警告: velox-bench gate の実行に失敗しました ({exc})", file=sys.stderr)
        return None
    # 終了コード 0=OK/1=FAIL/3=WARN はすべて「評価に成功した」ことを示す
    # (docs/benchmarking.md)。2 やクラッシュだけが異常。
    if proc.returncode not in (0, 1, 3):
        print(
            f"警告: velox-bench gate が異常終了しました (code={proc.returncode}): "
            f"{proc.stderr.strip()}",
            file=sys.stderr,
        )
        return None
    try:
        return json.loads(out_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        print(f"警告: gate 結果を読み込めません ({exc})", file=sys.stderr)
        return None


def compute_diff_via_gate(
    gate_report: dict, metric_name: str
) -> Diff | None:
    verdict = gate_report.get("metrics", {}).get(metric_name)
    if not verdict:
        return None
    pct_changes = verdict.get("pct_changes") or []
    if not pct_changes:
        return None
    return Diff(pct_change=pct_changes[0], severity=verdict["severity"].upper(), approximate=False)


def compute_diff_fallback(prev: HistoryEntry, curr: HistoryEntry, metric_name: str) -> Diff | None:
    prev_v = prev.metric_median(metric_name)
    curr_v = curr.metric_median(metric_name)
    if prev_v is None or curr_v is None:
        return None
    if prev_v == 0:
        pct = 0.0 if curr_v == 0 else float("inf")
    else:
        pct = (curr_v - prev_v) / prev_v * 100.0
    severity = classify_fallback(pct, FALLBACK_WARN_PCT, FALLBACK_FAIL_PCT)
    return Diff(pct_change=pct, severity=severity, approximate=True)


def assign_series_colors(entries_time_sorted: list[HistoryEntry]) -> dict[SeriesKey, str]:
    """系列 (session_id + machine_key) ごとに色を割り当てる。機種が違えば
    `session_id` が同じでも別系列 = 別色になる (D106) — 「同じ色の点だけが
    比較可能」という UI 上の約束を、機種軸でも保つため。"""
    colors: dict[SeriesKey, str] = {}
    for e in entries_time_sorted:
        skey = series_key(e)
        if skey not in colors:
            colors[skey] = PALETTE[len(colors) % len(PALETTE)]
    return colors


def render_svg_chart(
    points: list[tuple[HistoryEntry, float]],
    metric_name: str,
    series_colors: dict[SeriesKey, str],
) -> str:
    """`points` は時系列順 (古い→新しい) の (エントリ, 生の値)。同一系列
    (session_id かつ machine_key が同じ、D106) の点だけを線でつなぐ —
    異なる系列の点は marker のみ描画し、線を引かない (モジュール docstring
    の規則)。"""
    if not points:
        return "<p class='muted'>データがありません。</p>"

    width, height = 720, 200
    pad_left, pad_right, pad_top, pad_bottom = 56, 16, 16, 28
    plot_w = width - pad_left - pad_right
    plot_h = height - pad_top - pad_bottom

    values = [metric_display_value(metric_name, v) for _, v in points]
    v_min, v_max = min(values), max(values)
    if v_min == v_max:
        v_min -= 1
        v_max += 1
    span = v_max - v_min
    v_min -= span * 0.08
    v_max += span * 0.08
    span = v_max - v_min

    n = len(points)

    def x_at(i: int) -> float:
        if n == 1:
            return pad_left + plot_w / 2
        return pad_left + plot_w * i / (n - 1)

    def y_at(v: float) -> float:
        return pad_top + plot_h * (1 - (v - v_min) / span)

    # 系列 (session_id + machine_key) ごとに折れ線を分ける — 同一系列の
    # 点だけをつなぐ (D106)。
    by_series: dict[SeriesKey, list[int]] = defaultdict(list)
    for i, (entry, _) in enumerate(points):
        by_series[series_key(entry)].append(i)

    parts: list[str] = [
        f"<svg viewBox='0 0 {width} {height}' role='img' "
        f"aria-label='{html.escape(metric_label(metric_name))} の推移' "
        "xmlns='http://www.w3.org/2000/svg' class='chart'>"
    ]
    # 目盛り (上端・中央・下端)
    for frac, label_v in ((0.0, v_max), (0.5, (v_min + v_max) / 2), (1.0, v_min)):
        y = pad_top + plot_h * frac
        parts.append(
            f"<line x1='{pad_left}' y1='{y:.1f}' x2='{width - pad_right}' y2='{y:.1f}' "
            "class='gridline'/>"
        )
        parts.append(
            f"<text x='{pad_left - 6}' y='{y + 4:.1f}' text-anchor='end' class='axis-label'>"
            f"{label_v:.1f}</text>"
        )

    for skey, idxs in by_series.items():
        color = series_colors.get(skey, "#888")
        if len(idxs) >= 2:
            path = " ".join(
                f"{x_at(i):.1f},{y_at(metric_display_value(metric_name, points[i][1])):.1f}"
                for i in idxs
            )
            parts.append(f"<polyline points='{path}' fill='none' stroke='{color}' stroke-width='2'/>")
        for i in idxs:
            entry, raw_v = points[i]
            dv = metric_display_value(metric_name, raw_v)
            x, y = x_at(i), y_at(dv)
            sid, _machine_key = skey
            tooltip = (
                f"{entry.generated_at} | commit={short_sha(entry.commit)} | "
                f"session={sid} | machine={entry.machine_display} | "
                f"source={entry.source} | "
                f"{dv:.2f}{metric_unit(metric_name)}"
            )
            parts.append(
                f"<circle cx='{x:.1f}' cy='{y:.1f}' r='4' fill='{color}' stroke='#fff' "
                f"stroke-width='1'><title>{html.escape(tooltip)}</title></circle>"
            )
    parts.append(
        f"<text x='{pad_left}' y='{height - 6}' class='axis-label'>"
        f"{html.escape(points[0][0].generated_at[:10])}</text>"
    )
    parts.append(
        f"<text x='{width - pad_right}' y='{height - 6}' text-anchor='end' class='axis-label'>"
        f"{html.escape(points[-1][0].generated_at[:10])}</text>"
    )
    parts.append("</svg>")
    return "".join(parts)


def html_escape(v: Any) -> str:
    return html.escape(str(v)) if v is not None else ""


def build_report_model(
    entries: list[HistoryEntry],
    velox_bench_bin: Path | None,
) -> dict:
    """os -> scenario -> {"rows": [...], "charts": {...}} の入れ子構造を作る。"""
    by_os: dict[str, dict[str, list[HistoryEntry]]] = defaultdict(lambda: defaultdict(list))
    for e in entries:
        by_os[e.os_name][e.scenario].append(e)

    gate_calls_made = 0
    gate_calls_available = velox_bench_bin is not None

    model: dict[str, Any] = {}
    with tempfile.TemporaryDirectory(prefix="velox-dashboard-gate-") as tmp:
        tmp_path = Path(tmp)
        for os_name, scenarios in sorted(by_os.items()):
            model[os_name] = {}
            for scenario, scenario_entries in sorted(scenarios.items()):
                scenario_entries.sort(key=lambda e: parse_time(e.generated_at))
                series_colors = assign_series_colors(scenario_entries)

                # 系列 (session_id + machine_key, D106) ごとに「直前の
                # 同一系列のエントリ」を求め、差分を計算する。機種が違えば
                # session_id が同じでも別系列として扱う。
                last_by_series: dict[SeriesKey, HistoryEntry] = {}
                rows = []
                prev_series: SeriesKey | None = None
                for e in scenario_entries:
                    skey = series_key(e)
                    prev_entry = last_by_series.get(skey)
                    diffs: dict[str, Diff] = {}
                    if prev_entry is not None:
                        gate_report = None
                        if velox_bench_bin is not None:
                            gate_report = run_gate(
                                velox_bench_bin, prev_entry.result, e.result, tmp_path
                            )
                            gate_calls_made += 1
                        metric_names = set(prev_entry.result.get("metrics", {})) | set(
                            e.result.get("metrics", {})
                        )
                        for m in metric_names:
                            d = None
                            if gate_report is not None:
                                d = compute_diff_via_gate(gate_report, m)
                            if d is None:
                                d = compute_diff_fallback(prev_entry, e, m)
                            if d is not None:
                                diffs[m] = d
                    rows.append(
                        {
                            "entry": e,
                            "series_boundary": skey != prev_series,
                            "color": series_colors.get(skey, "#888"),
                            "diffs": diffs,
                            "has_prev": prev_entry is not None,
                        }
                    )
                    last_by_series[skey] = e
                    prev_series = skey

                charts = {}
                for group_key, group_label, candidates in METRIC_GROUPS:
                    metric_name = next(
                        (
                            m
                            for m in candidates
                            if any(e.metric_median(m) is not None for e in scenario_entries)
                        ),
                        None,
                    )
                    if metric_name is None:
                        continue
                    points = [
                        (e, e.metric_median(metric_name))
                        for e in scenario_entries
                        if e.metric_median(metric_name) is not None
                    ]
                    charts[group_key] = {
                        "label": group_label,
                        "metric_name": metric_name,
                        "svg": render_svg_chart(points, metric_name, series_colors),
                    }

                model[os_name][scenario] = {
                    "rows": rows,
                    "charts": charts,
                    "series_colors": series_colors,
                }
    return {
        "by_os": model,
        "gate_calls_made": gate_calls_made,
        "gate_calls_available": gate_calls_available,
    }


PRIMARY_TABLE_METRICS = [
    "startup_first_load_ms",
    "startup_toolbar_ready_ms",
    "page_load_ms",
    "pss_total_bytes",
    "cpu_percent",
    "tab_switch_ms",
    "tab_create_ms",
]


def format_metric_cell(entry: HistoryEntry, diff_map: dict[str, Diff], metric_name: str) -> str:
    v = entry.metric_median(metric_name)
    if v is None:
        return "—"
    dv = metric_display_value(metric_name, v)
    unit = metric_unit(metric_name)
    cell = f"{dv:.2f}{unit}"
    d = diff_map.get(metric_name)
    if d is not None:
        sign = "+" if d.pct_change >= 0 else ""
        approx = "≈" if d.approximate else ""
        cell += f" ({approx}{sign}{d.pct_change:.1f}%)"
    return cell


def render_html(model: dict, thresholds_note: str, repo_url: str) -> str:
    generated_at = datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%SZ")
    parts: list[str] = []
    parts.append("<!doctype html><html lang='ja'><head><meta charset='utf-8'>")
    parts.append("<meta name='viewport' content='width=device-width, initial-scale=1'>")
    parts.append("<title>VeloX Performance Dashboard</title>")
    parts.append(
        """
<style>
:root { color-scheme: light dark; }
body { font-family: -apple-system, "Segoe UI", "Hiragino Sans", sans-serif;
       margin: 0; padding: 1.5rem 2rem 4rem; background: #fafafa; color: #1a1a1a; line-height: 1.6; }
@media (prefers-color-scheme: dark) {
  body { background: #14161a; color: #e8e8e8; }
  table { background: #1c1f24; }
  th { background: #23262c !important; }
  td, th { border-color: #33373f !important; }
  .card { background: #1c1f24; border-color: #33373f; }
  .gridline { stroke: #33373f; }
  .axis-label { fill: #9aa0a6; }
  a { color: #7fb3ff; }
}
h1 { font-size: 1.4rem; }
h2 { margin-top: 2.5rem; border-bottom: 2px solid #ccc; padding-bottom: .25rem; }
h3 { margin-top: 2rem; }
.note, .muted { color: #666; font-size: .9rem; }
.card { border: 1px solid #ddd; border-radius: 8px; padding: 1rem; margin: .75rem 0; background: #fff; }
table { border-collapse: collapse; width: 100%; font-size: .85rem; margin: .5rem 0 1.5rem; }
th, td { border: 1px solid #ddd; padding: .35rem .5rem; text-align: left; white-space: nowrap; }
th { background: #f0f0f0; position: sticky; top: 0; }
tr.series-boundary td { border-top: 3px solid #999; }
.session-tag { display: inline-block; width: .7em; height: .7em; border-radius: 50%; margin-right: .35em; }
.sev-ok { color: #2e7d32; }
.sev-warn { color: #b26a00; font-weight: 600; }
.sev-fail { color: #c62828; font-weight: 700; }
.sev-na { color: #999; }
.chart-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(340px, 1fr)); gap: 1rem; }
.chart { width: 100%; height: auto; }
.gridline { stroke: #e0e0e0; stroke-dasharray: 3 3; }
.axis-label { font-size: 10px; fill: #666; }
code { background: rgba(127,127,127,.15); padding: .1em .3em; border-radius: 3px; }
</style>
</head><body>
"""
    )
    parts.append("<h1>VeloX Performance Dashboard</h1>")
    parts.append(f"<p class='note'>生成日時 (UTC): {generated_at}</p>")
    parts.append(
        "<div class='card'><strong>読み方</strong>: "
        "この表・グラフの各行/点は「1回の計測セッション・1つの機種」の"
        "組に属します。<strong>同じ色・線でつながっている点だけが比較可能"
        "</strong>です。線が途切れている、あるいは色が変わっている箇所は"
        "<strong>別のセッション、または別の機種で採った数値</strong>であり、"
        "並べて表示はしますが自動では差分を計算しません "
        "(<code>docs/performance-targets.md</code> §10、"
        "<code>docs/decisions.md</code> D46 — "
        "同一バイナリでもセッションを跨ぐと最大 +78.9% 動くことが実測済み。"
        "<code>docs/decisions.md</code> D96/D106 — "
        "<code>windows-latest</code> は run ごとに機種の異なるマシンを"
        "割り当てるため、機種が違えば同一セッション扱いでも比較しません。"
        "「機種不明」(CPU 情報の無い古い結果) は安全側に倒し、他のどの"
        "エントリとも比較しません)。"
        f"<br>{html_escape(thresholds_note)}"
        "<br>メモリはプロセス数の異なるブラウザ間で比較できる "
        "<strong>PSS</strong> を優先表示します "
        "(<code>docs/performance-targets.md</code> §3.1)。"
        "</div>"
    )

    by_os = model["by_os"]
    if not by_os:
        parts.append("<p>履歴データがありません。scripts/dashboard/record.py で記録してください。</p>")

    parts.append("<nav class='card'><strong>目次</strong><ul>")
    for os_name in sorted(by_os):
        parts.append(f"<li><a href='#os-{html_escape(os_name)}'>{html_escape(os_name)}</a></li>")
    parts.append("</ul></nav>")

    for os_name, scenarios in sorted(by_os.items()):
        parts.append(f"<h2 id='os-{html_escape(os_name)}'>OS: {html_escape(os_name)}</h2>")
        for scenario, data in sorted(scenarios.items()):
            parts.append(f"<h3>シナリオ: <code>{html_escape(scenario)}</code></h3>")

            charts = data["charts"]
            if charts:
                parts.append("<div class='chart-grid'>")
                for group_key, chart in charts.items():
                    parts.append(
                        f"<div><strong>{html_escape(chart['label'])}</strong> "
                        f"(<code>{html_escape(chart['metric_name'])}</code>)"
                        f"{chart['svg']}</div>"
                    )
                parts.append("</div>")
            else:
                parts.append("<p class='muted'>このシナリオでは startup/memory/cpu/tab のいずれの代表メトリクスも記録されていません。</p>")

            present_metrics = [
                m
                for m in PRIMARY_TABLE_METRICS
                if any(r["entry"].metric_median(m) is not None for r in data["rows"])
            ]
            parts.append("<div style='overflow-x:auto'><table><thead><tr>")
            for col in ("日時", "セッション/ソース", "機種", "コミット", "ブランチ", "PR", "試行数"):
                parts.append(f"<th>{col}</th>")
            for m in present_metrics:
                parts.append(f"<th>{html_escape(metric_label(m))}</th>")
            parts.append("</tr></thead><tbody>")
            for row in data["rows"]:
                e: HistoryEntry = row["entry"]
                cls = " class='series-boundary'" if row["series_boundary"] else ""
                parts.append(f"<tr{cls}>")
                parts.append(f"<td>{html_escape(e.generated_at)}</td>")
                sid_short = (e.session_id or "unknown")[:28]
                parts.append(
                    f"<td><span class='session-tag' style='background:{row['color']}'></span>"
                    f"{html_escape(sid_short)}<br><span class='muted'>{html_escape(e.source)}"
                    f"{' / ' + html_escape(e.note) if e.note else ''}</span></td>"
                )
                machine_cls = " class='muted'" if not e.cpu_model else ""
                parts.append(
                    f"<td{machine_cls}>{html_escape(e.machine_display)}</td>"
                )
                if e.commit:
                    commit_url = f"{repo_url}/commit/{e.commit}"
                    parts.append(f"<td><a href='{html_escape(commit_url)}'>{short_sha(e.commit)}</a></td>")
                else:
                    parts.append("<td>—</td>")
                parts.append(f"<td>{html_escape(e.branch) if e.branch else '—'}</td>")
                if e.pr_number:
                    pr_url = f"{repo_url}/pull/{e.pr_number}"
                    parts.append(f"<td><a href='{html_escape(pr_url)}'>#{e.pr_number}</a></td>")
                else:
                    parts.append("<td>—</td>")
                parts.append(f"<td>{e.trials if e.trials is not None else '—'}</td>")
                for m in present_metrics:
                    cell = format_metric_cell(e, row["diffs"], m)
                    d = row["diffs"].get(m)
                    sev_cls = SEVERITY_CLASS.get(d.severity, "") if d else ""
                    parts.append(f"<td class='{sev_cls}'>{html_escape(cell)}</td>")
                parts.append("</tr>")
            parts.append("</tbody></table></div>")

    parts.append("</body></html>")
    return "".join(parts)


def render_markdown(model: dict, thresholds_note: str, repo_url: str) -> str:
    generated_at = datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%SZ")
    lines = ["# VeloX Performance Dashboard", "", f"生成日時 (UTC): {generated_at}", ""]
    lines.append(
        "> 同じセッション ID **かつ同じ機種**の隣接エントリ同士だけが比較可能"
        "です。セッションまたは機種が変わる箇所は差分を計算していません "
        "(docs/performance-targets.md §10 / docs/decisions.md D46/D96/D106)。"
        "「機種不明」(CPU 情報の無い古い結果) は安全側に倒し、他のどの"
        "エントリとも比較しません。"
    )
    lines.append(f"> {thresholds_note}")
    lines.append("")

    by_os = model["by_os"]
    for os_name, scenarios in sorted(by_os.items()):
        lines.append(f"## OS: {os_name}")
        lines.append("")
        for scenario, data in sorted(scenarios.items()):
            lines.append(f"### シナリオ: `{scenario}`")
            lines.append("")
            present_metrics = [
                m
                for m in PRIMARY_TABLE_METRICS
                if any(r["entry"].metric_median(m) is not None for r in data["rows"])
            ]
            header = ["日時", "セッション", "機種", "ソース", "commit", "PR", "試行数"] + [
                metric_label(m) for m in present_metrics
            ]
            lines.append("| " + " | ".join(header) + " |")
            lines.append("|" + "|".join(["---"] * len(header)) + "|")
            for row in data["rows"]:
                e: HistoryEntry = row["entry"]
                boundary_mark = "**↓新系列 (セッション/機種)** " if row["series_boundary"] else ""
                sid_short = (e.session_id or "unknown")[:20]
                pr_cell = f"#{e.pr_number}" if e.pr_number else "—"
                commit_cell = short_sha(e.commit) if e.commit else "—"
                cells = [
                    e.generated_at,
                    f"{boundary_mark}{sid_short}",
                    e.machine_display,
                    e.source,
                    commit_cell,
                    pr_cell,
                    str(e.trials) if e.trials is not None else "—",
                ]
                for m in present_metrics:
                    cells.append(format_metric_cell(e, row["diffs"], m))
                lines.append("| " + " | ".join(cells) + " |")
            lines.append("")
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--history-dir", default=str(DEFAULT_HISTORY_DIR))
    parser.add_argument("--output", default=str(DEFAULT_HISTORY_DIR / "report.html"))
    parser.add_argument("--markdown-output")
    parser.add_argument("--os", action="append", dest="os_filter", help="このOSに絞り込む (繰り返し指定可)")
    parser.add_argument("--scenario", action="append", dest="scenario_filter", help="このシナリオに絞り込む (繰り返し指定可)")
    parser.add_argument("--velox-bench-bin", help="velox-bench バイナリのパス (省略時は target/release または target/debug から自動検出)")
    parser.add_argument("--repo-url", default=DEFAULT_REPO_URL)
    args = parser.parse_args()

    history_dir = Path(args.history_dir)
    os_filter = set(args.os_filter) if args.os_filter else None
    scenario_filter = set(args.scenario_filter) if args.scenario_filter else None

    entries = read_history(history_dir, os_filter, scenario_filter)
    if not entries:
        print(f"警告: {history_dir} に履歴データが見つかりません。空のレポートを生成します。", file=sys.stderr)

    velox_bench_bin = find_velox_bench_bin(args.velox_bench_bin)
    if velox_bench_bin is None:
        print(
            "警告: velox-bench バイナリが見つかりません。差分の重大度判定は "
            "簡易フォールバック (絶対差フロアなし) になります。"
            "--velox-bench-bin で明示するか、cargo build 後に再実行してください。",
            file=sys.stderr,
        )
    else:
        print(f"velox-bench: {velox_bench_bin} を使って差分を計算します。")

    model = build_report_model(entries, velox_bench_bin)

    if velox_bench_bin is not None:
        thresholds_note = (
            f"閾値判定: velox-bench gate ({model['gate_calls_made']} 回呼び出し, "
            "GateThresholds 既定値 warn=20% / fail=60% + メトリクスごとの最小絶対差)"
        )
    else:
        thresholds_note = (
            f"閾値判定: 簡易フォールバック (warn={FALLBACK_WARN_PCT}% / fail={FALLBACK_FAIL_PCT}%、"
            "絶対差フロアなし。velox-bench gate の判定より神経質になりえます)"
        )

    output_path = Path(args.output)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(render_html(model, thresholds_note, args.repo_url), encoding="utf-8")
    print(f"HTML レポートを書き出しました: {output_path}")

    if args.markdown_output:
        md_path = Path(args.markdown_output)
        md_path.parent.mkdir(parents=True, exist_ok=True)
        md_path.write_text(render_markdown(model, thresholds_note, args.repo_url), encoding="utf-8")
        print(f"Markdown レポートを書き出しました: {md_path}")

    return 0


if __name__ == "__main__":
    sys.exit(main())

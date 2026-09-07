"""Performance Dashboard (Issue #71) の record.py / report.py が共有する
データモデルとユーティリティ。

## 保存形式

`velox-bench run`/`aggregate` が書き出す `BenchmarkResult` JSON
(`docs/benchmarking.md` 参照) は一切変更しない。このモジュールはそれを
そのまま 1 エントリの `"result"` フィールドに包んで、以下のメタデータを
付け足した上で `results/history/<os>/<scenario>.jsonl` に 1 行 1 エントリで
追記する:

```json
{
  "schema_version": 1,
  "ingested_at": "<RFC3339>",
  "session_id": "<string>",
  "source": "manual | ci-perf-gate | baseline-committed | ...",
  "branch": "<string|null>",
  "pr_number": "<int|null>",
  "note": "<string|null>",
  "result": { ...BenchmarkResult (変更なし)... }
}
```

`velox-bench gate` が読む形式 (`BenchmarkResult`) と保存形式 (この
ラッパー) を意図的に分けているのは、`.github/workflows/perf-gate.yml` や
`velox-bench run --output` が生成する既存ファイルを**そのまま** `--result`
に渡して追記できるようにするため — 変換や再フォーマットを挟まない。

## 「異なるセッション/マシンの数値を比較してはならない」原則との両立

`docs/performance-targets.md` §10 (`docs/decisions.md` D46) は、この環境が
無変更バイナリでも +78.9% 動くほどノイズが大きく、**異なるセッションで採った
数値を並べて比較してはならない**と明記している。

このモジュールは「セッション」を**明示的な単位**として扱う:

- `session_id` は既定では呼び出しごとに一意な値を自動生成する
  (`generate_session_id`) — つまり**何もしなければ、2 回の記録は別セッション
  として扱われ、自動的には比較されない**。
- 複数の記録を「同一マシン・同一セッションで採った、比較してよい系列」と
  して扱いたい場合は、呼び出し側が **同じ `--session-id` を明示的に** 渡す
  必要がある (例: `.github/workflows/perf-gate.yml` が同一ジョブ内で
  baseline/candidate を計測するのと同じ粒度)。
- `report.py` は同一 `session_id` の隣接エントリ間だけを線でつなぎ、
  `velox-bench gate` で差分の重大度 (OK/WARN/FAIL) を計算する。
  `session_id` が変わる境界では線を引かず、差分も計算しない — 数値は
  並べて表示するが「参考程度」であることを明示する。

このルールはデータ構造 (`session_id` フィールドが無ければ比較できない) と
UI (session_id が変わる箇所は線を切り、差分バッジを出さない) の両方で
強制される。
"""

from __future__ import annotations

import json
import secrets
import socket
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterator

SCHEMA_VERSION = 1

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
DEFAULT_HISTORY_DIR = REPO_ROOT / "results" / "history"

# `velox-bench gate` の既定値 (`GateThresholds::default()`,
# `src/browser/benchmark.rs`) と同じ数値。`report.py` が `velox-bench` を
# 呼べない場合の簡易表示にのみ使う参考値であり、実際の判定は極力
# `velox-bench gate` の出力 (`GateReport.thresholds`) をそのまま使う
# — 二重管理を避けるため、ここでは「呼べないときのフォールバック」の
# 位置付けに留める。
FALLBACK_WARN_PCT = 20.0
FALLBACK_FAIL_PCT = 60.0

# report.py が「startup / memory / CPU / tab switching」の 4 グラフ
# (Issue #71 の実装内容) として拾う代表メトリクス。
# 値は (グループの日本語ラベル, そのグループで優先して使うメトリクス名を
# 前から順に試す候補リスト) — scenario によって存在するメトリクスが違う
# ため、存在する最初のものを使う。
METRIC_GROUPS: list[tuple[str, str, list[str]]] = [
    ("startup", "起動", ["startup_first_load_ms", "startup_window_created_ms"]),
    ("memory", "メモリ (PSS)", ["pss_total_bytes", "rss_total_bytes"]),
    ("cpu", "CPU", ["cpu_percent"]),
    ("tab", "タブ切替/生成", ["tab_switch_ms", "tab_create_ms", "tab_resume_ms"]),
]

# メトリクス名 -> (表示ラベル, 単位, 生値 -> 表示値への変換)
_MS = ("ms", lambda v: v)
_BYTES_MIB = ("MiB", lambda v: v / 1024 / 1024)
_PCT = ("%", lambda v: v)
_COUNT = ("個", lambda v: v)

METRIC_META: dict[str, tuple[str, str, Any]] = {
    "startup_window_created_ms": ("window_created", *_MS),
    "startup_rust_setup_done_ms": ("rust_setup_done", *_MS),
    "startup_toolbar_script_started_ms": ("toolbar_script_started", *_MS),
    "startup_toolbar_ready_ms": ("toolbar_ready", *_MS),
    "startup_first_load_ms": ("first_load", *_MS),
    "page_load_ms": ("page_load", *_MS),
    "tab_create_ms": ("tab_create", *_MS),
    "tab_switch_ms": ("tab_switch", *_MS),
    "tab_resume_ms": ("tab_resume", *_MS),
    "rss_total_bytes": ("RSS 合計", *_BYTES_MIB),
    "rss_process_count": ("RSS プロセス数", *_COUNT),
    "pss_total_bytes": ("PSS 合計", *_BYTES_MIB),
    "pss_process_count": ("PSS プロセス数", *_COUNT),
    "cpu_percent": ("CPU", *_PCT),
}


def metric_label(name: str) -> str:
    meta = METRIC_META.get(name)
    return meta[0] if meta else name


def metric_unit(name: str) -> str:
    meta = METRIC_META.get(name)
    return meta[1] if meta else ""


def metric_display_value(name: str, raw: float) -> float:
    meta = METRIC_META.get(name)
    return meta[2](raw) if meta else raw


def now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def generate_session_id(source: str) -> str:
    """呼び出しごとに一意な session_id を作る。**意図的に** 毎回変わる —
    複数の記録を同一セッション扱いにしたい場合は、呼び出し側が
    `--session-id` で明示的に同じ値を指定すること (モジュール docstring
    参照)。"""
    ts = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S")
    host = socket.gethostname() or "unknown-host"
    return f"{source}-{host}-{ts}-{secrets.token_hex(3)}"


def short_sha(sha: str | None, length: int = 12) -> str:
    if not sha:
        return "unknown"
    return sha[:length]


def history_file_for(history_dir: Path, os_name: str, scenario: str) -> Path:
    safe_os = os_name or "unknown"
    safe_scenario = scenario or "unknown"
    return history_dir / safe_os / f"{safe_scenario}.jsonl"


def append_jsonl(path: Path, entry: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as f:
        f.write(json.dumps(entry, ensure_ascii=False, sort_keys=True))
        f.write("\n")


@dataclass
class HistoryEntry:
    """`results/history/<os>/<scenario>.jsonl` の 1 行を読み込んだもの。"""

    os_name: str
    scenario: str
    path: Path
    line_no: int
    raw: dict = field(repr=False)

    @property
    def result(self) -> dict:
        return self.raw.get("result", {})

    @property
    def environment(self) -> dict:
        return self.result.get("environment", {})

    @property
    def session_id(self) -> str | None:
        return self.raw.get("session_id")

    @property
    def source(self) -> str:
        return self.raw.get("source") or "unknown"

    @property
    def commit(self) -> str | None:
        return self.environment.get("git_commit")

    @property
    def branch(self) -> str | None:
        return self.raw.get("branch")

    @property
    def pr_number(self) -> int | None:
        return self.raw.get("pr_number")

    @property
    def note(self) -> str | None:
        return self.raw.get("note")

    @property
    def generated_at(self) -> str:
        # `generated_at` (計測時刻) を優先し、無ければ取り込み時刻で代替する
        # — 手動で組み立てた BenchmarkResult など、まれに欠けるケースの保険。
        return self.environment.get("generated_at") or self.raw.get("ingested_at") or ""

    @property
    def trials(self) -> int | None:
        return self.environment.get("trials")

    @property
    def cpu_count(self) -> int | None:
        return self.environment.get("cpu_count")

    def metric_median(self, name: str) -> float | None:
        stats = self.result.get("metrics", {}).get(name)
        if not stats:
            return None
        return stats.get("median")


def iter_history_files(history_dir: Path) -> Iterator[Path]:
    if not history_dir.exists():
        return
    for path in sorted(history_dir.glob("*/*.jsonl")):
        yield path


def read_history(
    history_dir: Path,
    os_filter: set[str] | None = None,
    scenario_filter: set[str] | None = None,
) -> list[HistoryEntry]:
    entries: list[HistoryEntry] = []
    for path in iter_history_files(history_dir):
        os_name = path.parent.name
        scenario = path.stem
        if os_filter and os_name not in os_filter:
            continue
        if scenario_filter and scenario not in scenario_filter:
            continue
        with path.open("r", encoding="utf-8") as f:
            for line_no, line in enumerate(f, start=1):
                line = line.strip()
                if not line:
                    continue
                try:
                    raw = json.loads(line)
                except json.JSONDecodeError as exc:
                    print(
                        f"警告: {path}:{line_no} の行を JSON として解釈できません"
                        f"でした ({exc})。読み飛ばします。"
                    )
                    continue
                entries.append(
                    HistoryEntry(
                        os_name=os_name,
                        scenario=scenario,
                        path=path,
                        line_no=line_no,
                        raw=raw,
                    )
                )
    return entries


def classify_fallback(pct_change: float, warn_pct: float, fail_pct: float) -> str:
    """`velox-bench gate` バイナリが手元に無いときの簡易判定。
    `MetricKey::min_significant_delta` の絶対値フロアを適用しないぶん、
    実際の `velox-bench gate` より神経質になりうる — 必ず「簡易判定」と
    明記して表示すること。"""
    if pct_change > fail_pct:
        return "FAIL"
    if pct_change > warn_pct:
        return "WARN"
    return "OK"

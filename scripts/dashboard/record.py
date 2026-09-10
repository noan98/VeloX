#!/usr/bin/env python3
"""Performance Dashboard (Issue #71) — 計測結果を履歴ストアに追記する。

`velox-bench run --output <path>` / `velox-bench aggregate --output <path>`
が書き出した `BenchmarkResult` JSON (`docs/benchmarking.md` 参照)、および
`results/baseline/*.json` のようにコミット済みの同形式ファイルを、そのまま
`results/history/<os>/<scenario>.jsonl` に 1 行追記する。**`BenchmarkResult`
自体は一切変換しない** — 保存形式の詳細は `common.py` のモジュール
docstring を参照。

## 「セッション」の指定について (重要)

`--session-id` を省略すると、呼び出しごとに一意な ID が自動生成される —
つまり**このコマンドを 2 回呼ぶだけでは、2 つの記録は別セッション扱いに
なり、report.py 上で自動的には比較されない**。

複数の記録を「同一マシン・同一セッションで採った、比較してよい系列」として
扱いたい場合 (例: baseline 1 回 + candidate 2 回を同一 CI ジョブ内で測った
`.github/workflows/perf-gate.yml` の運用) は、それらの呼び出し全てに
**同じ `--session-id`** を明示的に渡すこと。1 回目の呼び出しの出力に
`session_id=...` が表示されるので、それを後続の呼び出しにそのまま渡せば
よい。

## 「機種」について (Issue #211 項目2 / docs/decisions.md D104)

`--session-id` が同じでも、`report.py` は **機種 (`environment.cpu_model`
等から導出する `machine_key`) が一致する場合にしか隣接エントリを比較しない**
(`common.py` の該当節参照)。`environment.cpu_model` が無い結果は「機種不明」
として記録され、安全側に倒して他のどのエントリとも自動比較されない —
このコマンドはその旨を記録のたびに表示する。

使い方
------
    # 1 回だけ記録する (session_id は自動生成される)
    python3 scripts/dashboard/record.py \\
      --result results/cold_startup.json --source manual

    # 同一セッションとして複数ファイルをまとめて記録する
    python3 scripts/dashboard/record.py \\
      --result results/baseline.json \\
      --result results/candidate-1.json \\
      --result results/candidate-2.json \\
      --source ci-perf-gate --session-id pr123-run456 \\
      --branch claude/issue-71-perf-dashboard-bnmj1w --pr 71
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from common import (  # noqa: E402
    DEFAULT_HISTORY_DIR,
    SCHEMA_VERSION,
    append_jsonl,
    generate_session_id,
    history_file_for,
    machine_label,
    now_iso,
)

REQUIRED_KEYS = ("scenario", "environment", "metrics")


def load_result(path: Path) -> dict | None:
    try:
        result = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        print(f"エラー: {path} を読み込めません ({exc})", file=sys.stderr)
        return None
    missing = [key for key in REQUIRED_KEYS if key not in result]
    if missing:
        print(
            f"エラー: {path} は BenchmarkResult 形式ではありません "
            f"(欠けているフィールド: {', '.join(missing)})",
            file=sys.stderr,
        )
        return None
    if "os" not in result.get("environment", {}):
        print(f"エラー: {path} の environment に os がありません", file=sys.stderr)
        return None
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "--result",
        action="append",
        required=True,
        metavar="PATH",
        help="velox-bench run/aggregate が書き出した BenchmarkResult JSON。繰り返し指定可",
    )
    parser.add_argument(
        "--source",
        default="manual",
        help="計測元の自由記述ラベル (manual / ci-perf-gate / baseline-committed 等、既定: manual)",
    )
    parser.add_argument(
        "--session-id",
        help="同一セッションとして扱うグループの ID。省略すると自動生成される (docstring 参照)",
    )
    parser.add_argument("--branch", help="計測対象コミットのブランチ名 (任意)")
    parser.add_argument("--pr", type=int, help="関連する PR 番号 (任意)")
    parser.add_argument("--note", help="自由記述のメモ (任意)")
    parser.add_argument(
        "--history-dir",
        default=str(DEFAULT_HISTORY_DIR),
        help=f"履歴ストアのルート (既定: {DEFAULT_HISTORY_DIR})",
    )
    args = parser.parse_args()

    session_id = args.session_id or generate_session_id(args.source)
    history_dir = Path(args.history_dir)

    written: list[Path] = []
    for result_path_str in args.result:
        result_path = Path(result_path_str)
        result = load_result(result_path)
        if result is None:
            return 2
        os_name = result["environment"]["os"]
        scenario = result["scenario"]
        entry = {
            "schema_version": SCHEMA_VERSION,
            "ingested_at": now_iso(),
            "session_id": session_id,
            "source": args.source,
            "branch": args.branch,
            "pr_number": args.pr,
            "note": args.note,
            "result": result,
        }
        dest = history_file_for(history_dir, os_name, scenario)
        append_jsonl(dest, entry)
        written.append(dest)
        print(
            f"追記しました: {dest} "
            f"(scenario={scenario} os={os_name} "
            f"commit={result['environment'].get('git_commit', 'unknown')[:12] if result['environment'].get('git_commit') else 'unknown'})"
        )
        # Issue #211 項目2 / docs/decisions.md D104: report.py は session_id
        # に加えて機種 (machine_key) が一致する隣接エントリ同士だけを比較
        # する。cpu_model が無い結果は「機種不明」として記録され、
        # report.py 上では他のどのエントリとも自動比較されない (安全側)。
        environment = result["environment"]
        print(f"  機種: {machine_label(environment)}")
        if not environment.get("cpu_model"):
            print(
                "  警告: environment.cpu_model が無いため「機種不明」として"
                "記録されます。report.py はこのエントリを他のどのエントリとも"
                "つなぎません (docs/decisions.md D104)。"
            )

    print(f"session_id={session_id}")
    print(
        "同じ計測セッションの続きを記録する場合は、上の session_id を "
        "次回の --session-id にそのまま渡してください。"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""プロセスツリー全体の RSS/PSS を時系列でサンプリングし CSV に落とす (Issue #70)。

なぜこのスクリプトが要るのか
----------------------------
`browser::metrics::sample_process_tree_rss` (D16/D42) と
`scripts/bench/compare_browsers.py` はどちらも「ある一時点」のスナップショット
しか撮らない。だが実際に調べたいのは大抵「タブを 5 個開いたら増える」
「10 分放置したら漏れていないか」といった**時間方向の変化**であり、それには
時系列サンプルが要る。このスクリプトは同じ `/proc` の読み方
(`docs/performance-targets.md` §3.1、D16/D42 参照) を使って、指定した間隔で
繰り返しサンプリングするだけの薄いラッパーである。

PSS を使う理由そのものはここでは繰り返さない。
`docs/performance-targets.md` §3.1 と D41/D42 を参照すること。

使い方
------
既に動いている VeloX (や他プロセス) の root PID にアタッチする:

    python3 scripts/profile/pss_sampler.py --pid 12345 \\
        --interval-ms 1000 --duration-secs 60 \\
        --output /tmp/velox-mem-timeline.csv

自分でプロセスを起動してから追跡する (起動直後からサンプルしたいとき):

    python3 scripts/profile/pss_sampler.py \\
        --interval-ms 500 --duration-secs 30 \\
        --output /tmp/velox-mem-timeline.csv \\
        --launch -- ./target/release/velox --homepage http://127.0.0.1:8731/minimal.html

`--launch` を使った場合、計測終了時 (デュレーション経過、または対象プロセスが
自然終了したとき) にこのスクリプトが SIGTERM で後始末する。

出力 CSV の列
-------------
`elapsed_ms,timestamp_iso,rss_bytes,pss_bytes,process_count,pss_process_count`

`pss_bytes` はツリー中 1 プロセスも smaps_rollup を読めなければ空欄になる
(scripts/bench/compare_browsers.py の `pss_bytes` が 0 を返すのとは違い、
「欠損」と「実測 0」を区別する — D42 と同じ方針)。`pss_process_count` が
`process_count` より小さければ、読めたプロセスの分だけの部分合計であることを
意味する。

このスクリプトは Python 標準ライブラリのみを使う (scripts/bench/ と同じ方針)。
"""

from __future__ import annotations

import argparse
import csv
import os
import signal
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path


def _pss_bytes(entry: Path) -> int | None:
    """`smaps_rollup` の `Pss:` 行 (kB) をバイトで返す。読めなければ `None`。

    `scripts/bench/compare_browsers.py::_pss_bytes` と同一のロジック
    (D42 が言うとおり、同じ読み方を複数箇所で独立に書いて数値がずれることを
    避けるため、意図的に揃えている)。
    """
    try:
        for line in (entry / "smaps_rollup").read_text().splitlines():
            if line.startswith("Pss:"):
                return int(line.split()[1]) * 1024
    except (OSError, IndexError, ValueError):
        return None
    return None


def process_tree_memory(root_pid: int) -> tuple[int, int | None, int, int]:
    """`root_pid` を根とするプロセスツリーの
    (RSS 合計, PSS 合計 or None, プロセス数, PSS を読めたプロセス数)。

    `scripts/bench/compare_browsers.py::process_tree_memory` と同じ考え方。
    そちらは (rss, pss, count) の 3 値タプルで PSS 欠損を暗黙に 0 として
    返しているが、このスクリプトは時系列データとして残すため、欠損を
    `None` として明示的に区別する (`browser::metrics::RssSample` の
    `total_pss_bytes: Option<u64>` に合わせている)。
    """
    children: dict[int, list[int]] = {}
    rss: dict[int, int] = {}
    pss: dict[int, int] = {}
    page_size = os.sysconf("SC_PAGE_SIZE")
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        try:
            fields = (entry / "stat").read_text().rsplit(") ", 1)[1].split()
            ppid = int(fields[1])
            resident_pages = int((entry / "statm").read_text().split()[1])
        except (OSError, IndexError, ValueError):
            continue
        children.setdefault(ppid, []).append(pid)
        rss[pid] = resident_pages * page_size
        proportional = _pss_bytes(entry)
        if proportional is not None:
            pss[pid] = proportional

    rss_total = pss_total = count = pss_count = 0
    stack = [root_pid]
    seen: set[int] = set()
    while stack:
        pid = stack.pop()
        if pid in seen or pid not in rss:
            continue
        seen.add(pid)
        rss_total += rss[pid]
        count += 1
        if pid in pss:
            pss_total += pss[pid]
            pss_count += 1
        stack.extend(children.get(pid, []))

    pss_result = pss_total if pss_count > 0 else None
    return rss_total, pss_result, count, pss_count


def pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


def sample_loop(
    root_pid: int, interval_ms: int, duration_secs: float, writer: csv.writer
) -> int:
    """`duration_secs` 経過するか対象プロセスが終了するまでサンプリングする。

    実際に書けた行数を返す。
    """
    start = time.monotonic()
    deadline = start + duration_secs if duration_secs > 0 else None
    rows = 0
    while True:
        now = time.monotonic()
        if deadline is not None and now >= deadline:
            break
        if not pid_alive(root_pid):
            print(f"pid={root_pid} は既に終了しています。サンプリングを終了します。",
                  file=sys.stderr)
            break
        rss_total, pss_total, count, pss_count = process_tree_memory(root_pid)
        elapsed_ms = (now - start) * 1000.0
        writer.writerow([
            f"{elapsed_ms:.1f}",
            datetime.now(timezone.utc).isoformat(),
            rss_total,
            "" if pss_total is None else pss_total,
            count,
            pss_count,
        ])
        rows += 1
        time.sleep(max(0.0, interval_ms / 1000.0))
    return rows


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                      formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--pid", type=int, help="追跡するプロセスツリーの root PID")
    parser.add_argument("--launch", action="store_true",
                         help="`--` の後のコマンドを自分で起動してから追跡する")
    parser.add_argument("--interval-ms", type=int, default=1000,
                         help="サンプリング間隔 (既定 1000ms)")
    parser.add_argument("--duration-secs", type=float, default=60.0,
                         help="サンプリングを続ける秒数。0 以下なら対象プロセスが"
                              "終了するまで無期限に続ける (既定 60)")
    parser.add_argument("--output", required=True, help="出力 CSV のパス")
    parser.add_argument("command", nargs=argparse.REMAINDER,
                         help="--launch と併用: `--` の後に起動するコマンド")
    args = parser.parse_args()

    if args.launch:
        if not args.command or args.command[0] != "--":
            print("--launch には `-- <コマンド> [引数...]` が必要です。", file=sys.stderr)
            return 2
        argv = args.command[1:]
        if not argv:
            print("--launch の後に起動するコマンドがありません。", file=sys.stderr)
            return 2
        proc = subprocess.Popen(argv)
        root_pid = proc.pid
        print(f"起動しました: pid={root_pid} ({' '.join(argv)})", file=sys.stderr)
    elif args.pid:
        root_pid = args.pid
        proc = None
        if not pid_alive(root_pid):
            print(f"pid={root_pid} は存在しません。", file=sys.stderr)
            return 1
    else:
        print("--pid か --launch のどちらかを指定してください。", file=sys.stderr)
        return 2

    out_path = Path(args.output)
    out_path.parent.mkdir(parents=True, exist_ok=True)

    rows = 0
    try:
        with out_path.open("w", newline="", encoding="utf-8") as f:
            writer = csv.writer(f)
            writer.writerow([
                "elapsed_ms", "timestamp_iso", "rss_bytes", "pss_bytes",
                "process_count", "pss_process_count",
            ])
            rows = sample_loop(root_pid, args.interval_ms, args.duration_secs, writer)
    finally:
        if proc is not None and proc.poll() is None:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=10)

    if rows == 0:
        print("1 行もサンプリングできませんでした (対象プロセスが即終了した"
              "可能性があります)。", file=sys.stderr)
        return 1

    print(f"{rows} 行を {out_path} に書き出しました。", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

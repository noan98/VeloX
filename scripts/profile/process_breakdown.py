#!/usr/bin/env python3
"""プロセスツリーを 1 プロセスずつ (comm, RSS, PSS) に分解して出す (Issue #61)。

なぜ要るのか
------------
`scripts/bench/compare_browsers.py` と `scripts/profile/pss_sampler.py` は
どちらも「ツリー全体の合計」しか出さない。Issue #61 の切り分けに必要なのは
合計ではなく **どのプロセス (UI プロセス本体 / WebKitWebProcess /
WebKitNetworkProcess / その他ヘルパー) が PSS のどれだけを持っているか**の
内訳であり、既存スクリプトにはこの粒度が無い。本スクリプトは
`process_tree_memory` と同じ `/proc` の読み方 (D16/D41/D42 と揃えている) を
使い、合計する代わりにプロセスごとの行を返す。

`comm` は `/proc/<pid>/comm` (15 文字で切り詰められることがある。例:
`WebKitWebProces`) をそのまま使う。VeloX/Chromium のどちらでも追加の
コマンドライン解析なしで役割が推測できる程度の粒度で十分なため、あえて
`cmdline` の全展開はしていない (見づらくなるため)。

使い方
------
起動済みプロセスの root PID にスナップショットを 1 回撮る:

    python3 scripts/profile/process_breakdown.py --pid 12345

自分で起動してから、load 相当を待つ時間 (`--settle-secs`) を置いてから撮る:

    python3 scripts/profile/process_breakdown.py \\
        --settle-secs 3 --output /tmp/velox-breakdown.json \\
        --launch -- ./target/release/velox --homepage http://127.0.0.1:8731/minimal.html

出力
----
標準出力に PSS 降順の表 (comm, pid, ppid, rss_mib, pss_mib)。`--output` を
指定すると同じ内容を JSON でも書き出す (`process_tree_memory` 由来の合計と
個々のプロセス一覧の両方を含む)。PSS が読めなかったプロセスは `pss_bytes`
が `null` になる (D42 と同じく「欠損」と「実測 0」を区別する)。

このスクリプトは Python 標準ライブラリのみを使う (scripts/bench/,
scripts/profile/ の既存スクリプトと同じ方針)。
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path


def _pss_bytes(entry: Path) -> int | None:
    """`smaps_rollup` の `Pss:` 行 (kB) をバイトで返す。読めなければ `None`。

    `scripts/bench/compare_browsers.py::_pss_bytes` /
    `scripts/profile/pss_sampler.py::_pss_bytes` と同一ロジック (D42 の
    「同じ読み方を複数箇所で独立に書いて数値がずれることを避ける」方針を踏襲)。
    """
    try:
        for line in (entry / "smaps_rollup").read_text().splitlines():
            if line.startswith("Pss:"):
                return int(line.split()[1]) * 1024
    except (OSError, IndexError, ValueError):
        return None
    return None


def _comm(entry: Path) -> str:
    try:
        return (entry / "comm").read_text().strip()
    except OSError:
        return "?"


def process_tree_breakdown(root_pid: int) -> list[dict]:
    """`root_pid` を根とするツリーの、プロセスごとの内訳を返す (PSS 降順)。

    各要素: `{pid, ppid, comm, rss_bytes, pss_bytes}`。`pss_bytes` は
    `smaps_rollup` を読めなかった場合 `None`。
    """
    children: dict[int, list[int]] = {}
    info: dict[int, dict] = {}
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
        info[pid] = {
            "pid": pid,
            "ppid": ppid,
            "comm": _comm(entry),
            "rss_bytes": resident_pages * page_size,
            "pss_bytes": _pss_bytes(entry),
        }

    rows: list[dict] = []
    stack = [root_pid]
    seen: set[int] = set()
    while stack:
        pid = stack.pop()
        if pid in seen or pid not in info:
            continue
        seen.add(pid)
        rows.append(info[pid])
        stack.extend(children.get(pid, []))

    rows.sort(key=lambda r: (r["pss_bytes"] is None, -(r["pss_bytes"] or 0)))
    return rows


def pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--pid", type=int, help="スナップショットを撮る root PID")
    parser.add_argument("--launch", action="store_true",
                         help="`--` の後のコマンドを自分で起動してから撮る")
    parser.add_argument("--settle-secs", type=float, default=3.0,
                         help="--launch 時、起動してからスナップショットを撮るまでの"
                              "待ち時間 (既定 3秒)")
    parser.add_argument("--output", help="JSON の出力先 (省略可)")
    parser.add_argument("command", nargs=argparse.REMAINDER,
                         help="--launch と併用: `--` の後に起動するコマンド")
    args = parser.parse_args()

    proc = None
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
        time.sleep(args.settle_secs)
    elif args.pid:
        root_pid = args.pid
        if not pid_alive(root_pid):
            print(f"pid={root_pid} は存在しません。", file=sys.stderr)
            return 1
    else:
        print("--pid か --launch のどちらかを指定してください。", file=sys.stderr)
        return 2

    try:
        rows = process_tree_breakdown(root_pid)
        if not rows:
            print(f"pid={root_pid} 配下のプロセスが見つかりませんでした。",
                  file=sys.stderr)
            return 1

        rss_total = sum(r["rss_bytes"] for r in rows)
        pss_rows = [r for r in rows if r["pss_bytes"] is not None]
        pss_total = sum(r["pss_bytes"] for r in pss_rows) if pss_rows else None

        print(f"{'comm':<20}{'pid':>8}{'ppid':>8}{'rss_mib':>12}{'pss_mib':>12}")
        for r in rows:
            pss_str = f"{r['pss_bytes'] / 1024 / 1024:.1f}" if r["pss_bytes"] is not None else "n/a"
            print(
                f"{r['comm']:<20}{r['pid']:>8}{r['ppid']:>8}"
                f"{r['rss_bytes'] / 1024 / 1024:>12.1f}{pss_str:>12}"
            )
        print("-" * 60)
        pss_total_str = f"{pss_total / 1024 / 1024:.1f}" if pss_total is not None else "n/a"
        print(
            f"{'TOTAL':<20}{'':>8}{'':>8}{rss_total / 1024 / 1024:>12.1f}"
            f"{pss_total_str:>12}  ({len(pss_rows)}/{len(rows)} プロセスで PSS 取得)"
        )

        if args.output:
            out = Path(args.output)
            out.parent.mkdir(parents=True, exist_ok=True)
            out.write_text(
                json.dumps(
                    {
                        "root_pid": root_pid,
                        "processes": rows,
                        "rss_total_bytes": rss_total,
                        "pss_total_bytes": pss_total,
                        "process_count": len(rows),
                        "pss_process_count": len(pss_rows),
                    },
                    indent=2,
                ),
                encoding="utf-8",
            )
            print(f"\n結果を {out} に保存しました。", file=sys.stderr)
        return 0
    finally:
        if proc is not None and proc.poll() is None:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=10)


if __name__ == "__main__":
    raise SystemExit(main())

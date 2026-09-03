#!/usr/bin/env python3
"""タブ数を変えたときの PSS の増え方を VeloX / Chromium で比較する (Issue #61)。

なぜ `velox-bench run --scenario tabs_N` をそのまま使わなかったか
------------------------------------------------------------------
`velox-bench` の RSS/PSS サンプラ (`spawn_rss_sampler`, `src/app.rs`) は
`VELOX_PERF_RSS_INTERVAL_MS` (既定 5000ms, `config::DEFAULT_PERF_RSS_INTERVAL`)
間隔で「起動直後から」定期的にサンプリングする一方、`tabs_N` シナリオの自動操作
スクリプト (`browser::automation::generate_bench_script` の `TabCountMemory`
ブランチ) は `open` を待ち時間なしで連続実行し、末尾の 1 回の `wait` (3000ms)
の後に `quit` する。**シナリオ全体の所要時間がサンプリング間隔の 5000ms 未満の
ことが多く (`tabs_1`/`tabs_5` は 3〜4 秒程度で終わる)、この場合ループの
「起動直後の 1 回目」のサンプルしか記録に残らない。** 実際にこのコンテナで
`tabs_1`/`tabs_5`/`tabs_10`/`tabs_20` を既定設定 (`--rss-interval-ms` 省略) で
実行して確認したところ、`pss_total_bytes` の中央値はタブ数に関係なくほぼ一定
(約 134〜147 MiB) だった — これは「タブを N 個開いた定常状態」ではなく
「まだタブを開き始める前 (またはごく初期) の PSS」を測っていたことを意味する。
`--rss-interval-ms` を短くする手もあるが、そうすると 1 試行の中に「タブを開いて
いる途中の低い値」と「開き終わった後の値」が混ざり、`velox-bench aggregate` の
中央値がどちらの値でもない不明瞭な数字になる。

このスクリプトは `docs/profiling.md` §2.4 が推奨する方針
(「タブライフサイクルには `pss_sampler.py` のような高頻度サンプリングを使う」)
を踏まえ、**タブを全部開き終えて安定させた後の 1 点**を明示的に狙って
サンプリングする。VeloX には `browser::automation` と同じ形式の自動操作
スクリプトを自前で生成して渡し (`open` を `--settle-per-open-ms` ずつ空けて
確実に 1 つずつ開かせる)、Chromium にはコマンドライン引数に URL を複数渡す
(Chromium は追加の URL 引数それぞれを新しいタブとして開く)。どちらも
`--stabilize-secs` 秒待ってから `scripts/bench/compare_browsers.py` /
`scripts/profile/pss_sampler.py` と同じ `/proc` の読み方でプロセスツリー全体
の PSS を採る。

使い方
------
    xvfb-run -a --server-args="-screen 0 1280x900x24" dbus-run-session -- \\
      python3 scripts/bench/tab_scaling.py \\
        --velox ./target/release/velox \\
        --chromium /opt/pw-browsers/chromium \\
        --page minimal.html --tab-counts 1,5,10,20 --trials 3 \\
        --output results/tab-scaling.json

`--chromium` を省略すると VeloX だけを測る。

出力
----
標準出力に `browser,tabs,trial,pss_mib,rss_mib,process_count` の表。
`--output` を指定すると同じ内容を JSON でも書き出す (中央値も含む)。

このスクリプトは Python 標準ライブラリのみを使う。
"""

from __future__ import annotations

import argparse
import http.server
import json
import os
import shutil
import signal
import socket
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

PAGES_DIR = Path(__file__).resolve().parent / "pages"


def _pss_bytes(entry: Path) -> int | None:
    """`smaps_rollup` の `Pss:` 行 (kB) をバイトで返す。読めなければ `None`。

    `compare_browsers.py`/`pss_sampler.py`/`process_breakdown.py` と同一ロジック。
    """
    try:
        for line in (entry / "smaps_rollup").read_text().splitlines():
            if line.startswith("Pss:"):
                return int(line.split()[1]) * 1024
    except (OSError, IndexError, ValueError):
        return None
    return None


def process_tree_memory(root_pid: int) -> tuple[int, int | None, int]:
    """`root_pid` を根とするツリーの (RSS 合計, PSS 合計 or None, プロセス数)。"""
    children: dict[int, list[int]] = {}
    rss: dict[int, int] = {}
    pss: dict[int, int] = {}
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
        rss[pid] = resident_pages * os.sysconf("SC_PAGE_SIZE")
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
    return rss_total, (pss_total if pss_count > 0 else None), count


class _QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):  # 計測中の標準エラー出力を汚さない
        pass


def serve_pages(port: int) -> socketserver.TCPServer:
    handler = lambda *a, **kw: _QuietHandler(*a, directory=str(PAGES_DIR), **kw)  # noqa: E731
    httpd = socketserver.ThreadingTCPServer(("127.0.0.1", port), handler)
    httpd.daemon_threads = True
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def measure_velox(binary: str, url: str, tabs: int, data_dir: Path,
                   settle_per_open_ms: int, stabilize_secs: float,
                   timeout: float) -> dict | None:
    """VeloX を自動操作スクリプトで `tabs` 個のタブまで開かせ、安定後に 1 回採る。"""
    lines = [f"open {url}" for _ in range(tabs - 1)]
    # 1 個ずつ確実に開かせるため、各 open の間に短い wait を挟む
    # (`browser::automation::TabCountMemory` は間隔なしで連続実行するが、それが
    # 本スクリプトを書く動機になった測定ギャップの一因なので、ここでは意図的に
    # 空ける)。
    spaced: list[str] = []
    for line in lines:
        spaced.append(line)
        spaced.append(f"wait {settle_per_open_ms}")
    # 開き終わった後、長めに待ってから (この待ち時間の途中でこちらから kill
    # するので実際にはここまで到達しない) quit。
    spaced.append("wait 120000")
    spaced.append("quit")
    script_text = "\n".join(spaced) + "\n"

    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as f:
        f.write(script_text)
        script_path = f.name

    env = {**os.environ, "VELOX_DATA_DIR": str(data_dir),
           "VELOX_AUTOMATION_SCRIPT": script_path, "VELOX_HOMEPAGE": url}
    proc = subprocess.Popen([binary], env=env,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        # タブを 1 つずつ開き終えるのにかかる見込み時間 + 安定待ち。
        open_wait = (tabs - 1) * settle_per_open_ms / 1000.0
        deadline = time.monotonic() + timeout
        time.sleep(min(open_wait + stabilize_secs, max(0.0, deadline - time.monotonic())))
        if proc.poll() is not None:
            return None
        rss, pss, count = process_tree_memory(proc.pid)
        return {"rss_bytes": rss, "pss_bytes": pss, "process_count": count}
    finally:
        os.unlink(script_path)
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=10)


def measure_chromium(binary: str, base_url: str, tabs: int, profile: Path,
                      stabilize_secs: float, timeout: float) -> dict | None:
    """Chromium に `tabs` 個の URL 引数を渡し (それぞれ新規タブになる)、安定後に採る。"""
    urls = [f"{base_url}&tab={i}" for i in range(tabs)]
    argv = [
        binary, "--no-sandbox", "--disable-gpu", "--no-first-run",
        "--no-default-browser-check", "--disable-features=Translate,MediaRouter",
        f"--user-data-dir={profile}", *urls,
    ]
    proc = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + timeout
        time.sleep(min(stabilize_secs, max(0.0, deadline - time.monotonic())))
        if proc.poll() is not None:
            return None
        rss, pss, count = process_tree_memory(proc.pid)
        return {"rss_bytes": rss, "pss_bytes": pss, "process_count": count}
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=10)


def median(values: list[float]) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    n = len(ordered)
    return ordered[n // 2] if n % 2 else (ordered[n // 2 - 1] + ordered[n // 2]) / 2.0


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--velox", required=True)
    parser.add_argument("--chromium")
    parser.add_argument("--page", default="minimal.html")
    parser.add_argument("--tab-counts", default="1,5,10,20",
                         help="カンマ区切りのタブ数リスト (既定 1,5,10,20)")
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--settle-per-open-ms", type=int, default=300,
                         help="VeloX: 各 open の後に空ける待ち時間 (既定 300ms)")
    parser.add_argument("--stabilize-secs", type=float, default=3.0,
                         help="全タブを開き終えてから PSS を採るまでの待ち時間")
    parser.add_argument("--timeout", type=float, default=120.0)
    parser.add_argument("--output")
    args = parser.parse_args()

    if not os.environ.get("DISPLAY"):
        print("DISPLAY が未設定です。xvfb-run 経由で実行してください。", file=sys.stderr)
        return 2

    tab_counts = [int(x) for x in args.tab_counts.split(",") if x.strip()]
    port = free_port()
    httpd = serve_pages(port)

    workdir = Path(tempfile.mkdtemp(prefix="velox-tabscaling-"))
    rows: list[dict] = []
    try:
        for tabs in tab_counts:
            for trial in range(1, args.trials + 1):
                url = f"http://127.0.0.1:{port}/{args.page}?run=v-{tabs}-{trial}"
                data_dir = workdir / f"velox-{tabs}-{trial}"
                data_dir.mkdir(parents=True, exist_ok=True)
                result = measure_velox(
                    args.velox, url, tabs, data_dir,
                    args.settle_per_open_ms, args.stabilize_secs, args.timeout,
                )
                if result is None:
                    print(f"  velox tabs={tabs} trial={trial}: 計測失敗 (早期終了)")
                    continue
                row = {"browser": "velox", "tabs": tabs, "trial": trial, **result}
                rows.append(row)
                pss = row["pss_bytes"]
                pss_s = f"{pss / 1024 / 1024:.1f}" if pss is not None else "n/a"
                print(f"  velox   tabs={tabs:>3} trial={trial}: "
                      f"pss={pss_s}MiB rss={row['rss_bytes'] / 1024 / 1024:.1f}MiB "
                      f"procs={row['process_count']}")

            if args.chromium:
                for trial in range(1, args.trials + 1):
                    url = f"http://127.0.0.1:{port}/{args.page}?run=c-{tabs}-{trial}"
                    profile = workdir / f"chromium-{tabs}-{trial}"
                    profile.mkdir(parents=True, exist_ok=True)
                    result = measure_chromium(
                        args.chromium, url, tabs, profile,
                        args.stabilize_secs, args.timeout,
                    )
                    if result is None:
                        print(f"  chromium tabs={tabs} trial={trial}: 計測失敗 (早期終了)")
                        continue
                    row = {"browser": "chromium", "tabs": tabs, "trial": trial, **result}
                    rows.append(row)
                    pss = row["pss_bytes"]
                    pss_s = f"{pss / 1024 / 1024:.1f}" if pss is not None else "n/a"
                    print(f"  chromium tabs={tabs:>3} trial={trial}: "
                          f"pss={pss_s}MiB rss={row['rss_bytes'] / 1024 / 1024:.1f}MiB "
                          f"procs={row['process_count']}")
    finally:
        httpd.shutdown()
        shutil.rmtree(workdir, ignore_errors=True)

    summary: dict[str, dict[int, dict]] = {}
    for browser in {r["browser"] for r in rows}:
        summary[browser] = {}
        for tabs in tab_counts:
            subset = [r for r in rows if r["browser"] == browser and r["tabs"] == tabs]
            pss_vals = [r["pss_bytes"] / 1024 / 1024 for r in subset
                        if r["pss_bytes"] is not None]
            rss_vals = [r["rss_bytes"] / 1024 / 1024 for r in subset]
            summary[browser][tabs] = {
                "n": len(subset),
                "pss_mib_median": median(pss_vals),
                "rss_mib_median": median(rss_vals),
                "process_count_median": median([float(r["process_count"]) for r in subset]),
            }

    print(f"\n{'browser':<10}{'tabs':>6}{'n':>4}{'pss median(MiB)':>18}"
          f"{'rss median(MiB)':>18}{'procs median':>14}")
    for browser, per_tabs in summary.items():
        for tabs, s in sorted(per_tabs.items()):
            pss_s = f"{s['pss_mib_median']:.1f}" if s["pss_mib_median"] is not None else "n/a"
            print(f"{browser:<10}{tabs:>6}{s['n']:>4}{pss_s:>18}"
                  f"{s['rss_mib_median']:>18.1f}{s['process_count_median']:>14.1f}")

    if args.output:
        out = Path(args.output)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(
            json.dumps({"rows": rows, "summary": summary}, indent=2, ensure_ascii=False),
            encoding="utf-8",
        )
        print(f"\n結果を {out} に保存しました。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

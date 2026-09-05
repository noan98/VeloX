#!/usr/bin/env python3
"""バックグラウンドタブの CPU 消費を外側から測る (Issue #64, docs/decisions.md D58)。

なぜ `velox-bench run --scenario background_cpu` だけで足りないか
----------------------------------------------------------------
`background_cpu` シナリオは VeloX 自身の RSS/CPU サンプラ
(`app::spawn_rss_sampler`) が出す `cpu_percent` を集計する。これは回帰ゲート
に掛けられる反面、**サンプラ自身が /proc を歩く分の CPU が数値に混ざる**
(既定 2 秒間隔で、この環境では数 % に相当する)。同一シナリオの before/after
比較では両方に等しく乗るので打ち消し合うが、「バックグラウンドタブは実際に
何 % 使っているのか」という絶対値を出すには使えない。

このスクリプトは VeloX の外から `/proc/<pid>/stat` の utime+stime を
**2 点だけ**読んで差を取るので、測定自体のコストが被測定側に乗らない。
`--settle-secs` 待ってから `--window-secs` の窓で測るため、起動とページ
読み込みのコストも窓の外に出る。

使い方
------
    # busy.html がアクティブタブのとき
    printf 'wait 60000\nquit\n' > /tmp/active.txt
    xvfb-run -a --server-args="-screen 0 1280x900x24" dbus-run-session -- \
      python3 scripts/profile/cpu_usage.py --velox ./target/release/velox \
        --script /tmp/active.txt --homepage file://$PWD/scripts/bench/pages/busy.html \
        --label active

    # busy.html をバックグラウンドに送ったとき
    P=$PWD/scripts/bench/pages
    printf 'open file://%s/busy.html\nwait 1500\nopen file://%s/minimal.html\nwait 60000\nquit\n' \
      $P $P > /tmp/bg.txt
    ... --script /tmp/bg.txt --homepage file://$P/minimal.html --label background

`VELOX_MAX_TABS_PER_PROCESS=1` を付けるとタブごとに web プロセスが分かれる
ので、プロセス別の内訳からどのタブが使っているかを読み取れる (D54/D57)。

出力
----
1 行目に合計 (`cpu_secs` / `cpu_pct` / プロセス数)、続けて 0.05 秒以上
CPU を使ったプロセスの内訳。Python 標準ライブラリのみを使う。
"""

import argparse, os, subprocess, sys, time, tempfile

CLK = os.sysconf("SC_CLK_TCK")

def proc_tree(root):
    """root の子孫 pid の集合 (root 自身を含む)。"""
    kids = {}
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat") as f:
                parts = f.read().rsplit(") ", 1)[1].split()
            kids.setdefault(int(parts[1]), []).append(int(entry))
        except (OSError, IndexError):
            continue
    out, stack = set(), [root]
    while stack:
        pid = stack.pop()
        if pid in out:
            continue
        out.add(pid)
        stack.extend(kids.get(pid, []))
    return out

def cpu_ticks(pids):
    """pid 集合の utime+stime 合計 (秒) と、プロセス別の内訳。"""
    total, per = 0.0, {}
    for pid in pids:
        try:
            with open(f"/proc/{pid}/stat") as f:
                raw = f.read()
            comm = raw.split("(", 1)[1].rsplit(")", 1)[0]
            parts = raw.rsplit(") ", 1)[1].split()
            secs = (int(parts[11]) + int(parts[12])) / CLK  # utime, stime
        except (OSError, IndexError, ValueError):
            continue
        total += secs
        per[pid] = (comm, secs)
    return total, per

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--velox", required=True)
    ap.add_argument("--script", required=True, help="自動操作スクリプト")
    ap.add_argument("--homepage", required=True)
    ap.add_argument("--settle-secs", type=float, default=6.0)
    ap.add_argument("--window-secs", type=float, default=8.0)
    ap.add_argument("--data-dir", default=None)
    ap.add_argument("--label", default="")
    args = ap.parse_args()

    data_dir = args.data_dir or tempfile.mkdtemp(prefix="velox-cpu-")
    env = {**os.environ, "VELOX_DATA_DIR": data_dir,
           "VELOX_AUTOMATION_SCRIPT": args.script, "VELOX_HOMEPAGE": args.homepage}
    proc = subprocess.Popen([args.velox], env=env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        time.sleep(args.settle_secs)
        if proc.poll() is not None:
            print(f"{args.label}: velox が早期終了しました", file=sys.stderr)
            return 1
        pids = proc_tree(proc.pid)
        t0, per0 = cpu_ticks(pids)
        wall0 = time.monotonic()
        time.sleep(args.window_secs)
        pids |= proc_tree(proc.pid)
        t1, per1 = cpu_ticks(pids)
        wall = time.monotonic() - wall0
        used = t1 - t0
        print(f"{args.label}\tcpu_secs={used:.2f}\twall={wall:.1f}\tcpu_pct={100*used/wall:.1f}\tprocs={len(per1)}")
        rows = []
        for pid, (comm, secs) in per1.items():
            delta = secs - per0.get(pid, (comm, 0.0))[1]
            if delta > 0.05:
                rows.append((delta, comm, pid))
        for delta, comm, pid in sorted(rows, reverse=True):
            print(f"    {comm:<20} pid={pid:<7} {delta:6.2f}s ({100*delta/wall:5.1f}%)")
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill(); proc.wait(timeout=10)
    return 0

sys.exit(main())

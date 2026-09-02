#!/usr/bin/env python3
"""VeloX と他ブラウザを同一条件で比較する計測ハーネス (Issue #58)。

なぜ velox-bench とは別なのか
------------------------------
`velox-bench` は VeloX 自身が `VELOX_PERF_OUTPUT` に書き出す計測ログを集計する。
それは VeloX の内部イベント (window created / toolbar ready / LoadFinished) を
直接読める代わりに、**他のブラウザには一切適用できない**。競合比較には、どの
ブラウザにも同じ意味で当てはまる外形的な指標が要る。

このスクリプトが測るもの
------------------------
1. `startup_to_load_ms` — プロセスを spawn した瞬間から、**ページ自身の `load`
   イベントが発火するまで**の実時間。ページに注入した beacon が
   `GET /loaded?...` を叩き、その到着時刻をこのプロセスが記録する。ブラウザの
   内部 API に一切依存しないので、VeloX と Chromium に同じ定義で当てはまる。
2. `rss_bytes` — ロード完了から `--settle-secs` 秒後の、プロセスツリー全体の
   RSS 合計。`/proc` を辿るだけで、`browser::metrics::sample_process_tree_rss`
   と同じ考え方 (D16)。
3. `pss_bytes` — 同じ時点の PSS (Proportional Set Size) 合計。**RSS 合計だけで
   比較してはいけない**: RSS は共有メモリをプロセスごとに丸ごと数えるため、
   プロセス数の多いブラウザほど二重計上で不利に出る (この環境では VeloX が 5
   プロセス、Chromium が 9 プロセス)。PSS は共有ページを共有者数で割るので、
   マルチプロセスブラウザ同士のメモリ比較にはこちらが適切。

公平性のために揃えていること
----------------------------
- 同一マシン・同一 Xvfb ディスプレイ・同一のローカル固定ページ (ネットワーク
  非依存)。
- beacon スクリプトは両ブラウザに**同じものを**注入する。`load` 後に 1 回
  fetch するだけなので、どちらか一方だけが不利にならない。
- 試行ごとにプロファイルディレクトリを捨てる (Chromium)。VeloX は
  `VELOX_DATA_DIR` を試行ごとに変える。

正直に言えないこと
------------------
- **これはブラウザの「速さ」の総合評価ではない。** 測っているのは起動から
  最初の 1 ページが load するまでと、その直後の RSS だけ。レンダリング品質、
  JS 実行性能、複数タブ時の挙動、省電力性は一切見ていない。
- GPU の無い環境ではどちらもソフトウェアレンダリングになり、**RSS は実機と
  乖離する**。絶対値ではなく同一環境での相対比較として読むこと。
- VeloX はシステム WebView (Linux では WebKitGTK) を使う。したがってこの比較は
  「VeloX 対 Chromium」であると同時に「WebKitGTK 対 Blink」でもある。VeloX 側の
  オーバーヘッドとエンジン差を分離するものではない (Epic #57 の原則)。

使い方
------
    xvfb-run -a --server-args="-screen 0 1280x900x24" \\
      python3 scripts/bench/compare_browsers.py \\
        --velox ./target/release/velox \\
        --chromium /opt/pw-browsers/chromium \\
        --page minimal.html --trials 5 --output results/compare.json
"""

from __future__ import annotations

import argparse
import http.server
import json
import os
import shutil
import socket
import socketserver
import subprocess
import tempfile
import threading
import time
from pathlib import Path
from urllib.parse import urlparse

PAGES_DIR = Path(__file__).resolve().parent / "pages"

# `load` の後に 1 回だけ beacon を送る。`load` にしているのは、DOMContentLoaded
# より「利用者が待たされ終わる時点」に近く、かつどのブラウザでも同じ意味を持つ
# ため。keepalive を付けるのは、直後にプロセスを落としても取りこぼさないため。
BEACON = """
<script>
window.addEventListener('load', function () {
  try { fetch('/loaded?run=' + encodeURIComponent(RUN_ID), { keepalive: true }); }
  catch (e) { new Image().src = '/loaded?run=' + encodeURIComponent(RUN_ID); }
});
</script>
"""


class Harness(socketserver.ThreadingMixIn, http.server.HTTPServer):
    """固定ページを配信し、beacon の到着時刻を記録するだけのサーバ。"""

    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, addr, page_html: str):
        super().__init__(addr, _Handler)
        self.page_html = page_html
        self.loaded: dict[str, float] = {}
        self.lock = threading.Lock()


class _Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):  # noqa: N802 (http.server's API)
        parsed = urlparse(self.path)
        if parsed.path == "/loaded":
            now = time.monotonic()
            run = parsed.query.partition("run=")[2]
            with self.server.lock:
                self.server.loaded.setdefault(run, now)
            self._respond(b"ok", "text/plain")
            return
        if parsed.path in ("/", "/index.html"):
            run = parsed.query.partition("run=")[2] or "unknown"
            body = self.server.page_html.replace("RUN_ID", json.dumps(run))
            self._respond(body.encode("utf-8"), "text/html; charset=utf-8")
            return
        self.send_error(404)

    def _respond(self, body: bytes, content_type: str) -> None:
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        # 試行間でキャッシュが効くと「2 回目以降だけ速い」が混ざるため無効化する。
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):  # 計測中の標準エラー出力を汚さない
        pass


def build_page(page: str) -> str:
    """固定ページに beacon を差し込んだ HTML を返す。

    元の fixture ファイルは書き換えない。両ブラウザに同じ加工を施すので、
    beacon 自体は比較の公平性を損なわない。
    """
    html = (PAGES_DIR / page).read_text(encoding="utf-8")
    if "</body>" in html:
        return html.replace("</body>", BEACON + "</body>", 1)
    return html + BEACON


def _pss_bytes(entry: Path) -> int | None:
    """`smaps_rollup` の Pss 行 (kB)。読めなければ `None`。"""
    try:
        for line in (entry / "smaps_rollup").read_text().splitlines():
            if line.startswith("Pss:"):
                return int(line.split()[1]) * 1024
    except (OSError, IndexError, ValueError):
        return None
    return None


def process_tree_memory(root_pid: int) -> tuple[int, int, int]:
    """`root_pid` を根とするプロセスツリーの (RSS 合計, PSS 合計, プロセス数)。

    `browser::metrics::sample_process_tree_rss` (D16) と同じ考え方で `/proc` を
    直接読む。ブラウザはマルチプロセスなので、親プロセスだけを見ても意味が無い。

    RSS 合計は共有ページを各プロセスで重複して数えるため、プロセス数の多い
    ブラウザほど大きく出る。異なるプロセス構成のブラウザ同士を比べるときは
    PSS 合計を見ること。PSS が読めなかったプロセスは PSS 合計から落ちるので、
    その場合は PSS を過小評価する (RSS 側は常に読める)。
    """
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
            # statm の 2 列目 (resident) はページ数。
            resident_pages = int((entry / "statm").read_text().split()[1])
        except (OSError, IndexError, ValueError):
            continue
        children.setdefault(ppid, []).append(pid)
        rss[pid] = resident_pages * os.sysconf("SC_PAGE_SIZE")
        proportional = _pss_bytes(entry)
        if proportional is not None:
            pss[pid] = proportional

    rss_total = pss_total = count = 0
    stack = [root_pid]
    seen: set[int] = set()
    while stack:
        pid = stack.pop()
        if pid in seen or pid not in rss:
            continue
        seen.add(pid)
        rss_total += rss[pid]
        pss_total += pss.get(pid, 0)
        count += 1
        stack.extend(children.get(pid, []))
    return rss_total, pss_total, count


def velox_command(binary: str, url: str, data_dir: Path) -> tuple[list[str], dict]:
    return [binary, "--homepage", url], {"VELOX_DATA_DIR": str(data_dir)}


def chromium_command(binary: str, url: str, profile: Path) -> tuple[list[str], dict]:
    # 比較を成立させるための最小限のフラグだけを渡す。速度に効く最適化フラグは
    # 足さない (どちらかを有利にしないため)。--no-sandbox はこのコンテナが root
    # で動くため、--disable-gpu は GPU が無い環境で毎回フォールバックする分の
    # ばらつきを避けるため。
    return (
        [
            binary,
            "--no-sandbox",
            "--disable-gpu",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-features=Translate,MediaRouter",
            f"--user-data-dir={profile}",
            url,
        ],
        {},
    )


def run_trial(name: str, argv: list[str], env_extra: dict, server: Harness,
              run_id: str, timeout: float, settle: float) -> dict | None:
    env = {**os.environ, **env_extra}
    started = time.monotonic()
    proc = subprocess.Popen(
        argv, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )
    try:
        deadline = started + timeout
        loaded_at = None
        while time.monotonic() < deadline:
            with server.lock:
                loaded_at = server.loaded.get(run_id)
            if loaded_at is not None:
                break
            if proc.poll() is not None:
                break
            time.sleep(0.01)
        if loaded_at is None:
            return None
        time.sleep(settle)
        rss_total, pss_total, proc_count = process_tree_memory(proc.pid)
        return {
            "browser": name,
            "startup_to_load_ms": (loaded_at - started) * 1000.0,
            "rss_bytes": rss_total,
            "pss_bytes": pss_total,
            "process_count": proc_count,
        }
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=10)


def summarize(values: list[float]) -> dict:
    if not values:
        return {"n": 0}
    ordered = sorted(values)
    n = len(ordered)
    median = (
        ordered[n // 2] if n % 2 else (ordered[n // 2 - 1] + ordered[n // 2]) / 2.0
    )
    return {
        "n": n,
        "min": ordered[0],
        "median": median,
        "max": ordered[-1],
    }


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--velox", required=True)
    parser.add_argument("--chromium")
    parser.add_argument("--page", default="minimal.html")
    parser.add_argument("--trials", type=int, default=5)
    parser.add_argument("--timeout", type=float, default=45.0)
    parser.add_argument(
        "--settle-secs",
        type=float,
        default=3.0,
        help="load 後、RSS を採るまでの待ち時間",
    )
    parser.add_argument("--output")
    args = parser.parse_args()

    if not os.environ.get("DISPLAY"):
        print("DISPLAY が未設定です。xvfb-run 経由で実行してください。")
        return 2

    port = free_port()
    server = Harness(("127.0.0.1", port), build_page(args.page))
    threading.Thread(target=server.serve_forever, daemon=True).start()

    browsers: list[tuple[str, str]] = [("velox", args.velox)]
    if args.chromium:
        browsers.append(("chromium", args.chromium))

    samples: dict[str, list[dict]] = {name: [] for name, _ in browsers}
    workdir = Path(tempfile.mkdtemp(prefix="velox-compare-"))
    try:
        for trial in range(1, args.trials + 1):
            for name, binary in browsers:
                run_id = f"{name}-{trial}"
                url = f"http://127.0.0.1:{port}/?run={run_id}"
                profile = workdir / run_id
                profile.mkdir(parents=True, exist_ok=True)
                argv, env_extra = (
                    velox_command(binary, url, profile)
                    if name == "velox"
                    else chromium_command(binary, url, profile)
                )
                result = run_trial(
                    name, argv, env_extra, server, run_id, args.timeout,
                    args.settle_secs,
                )
                if result is None:
                    print(f"  試行 {trial}: {name}: load を検知できませんでした")
                    continue
                samples[name].append(result)
                print(
                    f"  試行 {trial}: {name}: "
                    f"{result['startup_to_load_ms']:.1f}ms "
                    f"pss={result['pss_bytes'] / 1024 / 1024:.1f}MiB "
                    f"rss={result['rss_bytes'] / 1024 / 1024:.1f}MiB "
                    f"procs={result['process_count']}"
                )
    finally:
        server.shutdown()
        shutil.rmtree(workdir, ignore_errors=True)

    report = {
        "page": args.page,
        "trials_requested": args.trials,
        "settle_secs": args.settle_secs,
        "browsers": {
            name: {
                "startup_to_load_ms": summarize(
                    [s["startup_to_load_ms"] for s in samples[name]]
                ),
                "rss_bytes": summarize([float(s["rss_bytes"]) for s in samples[name]]),
                "pss_bytes": summarize([float(s["pss_bytes"]) for s in samples[name]]),
                "process_count": summarize(
                    [float(s["process_count"]) for s in samples[name]]
                ),
            }
            for name, _ in browsers
        },
    }

    print(f"\npage={args.page} trials={args.trials}")
    print(
        f"{'browser':<12}{'n':>3}{'load median(ms)':>18}"
        f"{'PSS median(MiB)':>18}{'RSS median(MiB)':>18}{'procs':>8}"
    )
    for name, _ in browsers:
        b = report["browsers"][name]
        if not b["startup_to_load_ms"]["n"]:
            print(f"{name:<12}{0:>3}{'—':>18}{'—':>18}{'—':>18}{'—':>8}")
            continue
        print(
            f"{name:<12}{b['startup_to_load_ms']['n']:>3}"
            f"{b['startup_to_load_ms']['median']:>18.1f}"
            f"{b['pss_bytes']['median'] / 1024 / 1024:>18.1f}"
            f"{b['rss_bytes']['median'] / 1024 / 1024:>18.1f}"
            f"{b['process_count']['median']:>8.0f}"
        )
    print(
        "\n注: RSS 合計は共有メモリを二重計上するため、プロセス数の多いブラウザが\n"
        "不利に出る。メモリの比較は PSS を見ること。"
    )

    if args.output:
        out = Path(args.output)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(json.dumps(report, indent=2, ensure_ascii=False), encoding="utf-8")
        print(f"\n結果を {out} に保存しました。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

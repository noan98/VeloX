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

Windows では何が違うか (Issue #197)
-----------------------------------
T2 の評価は Linux でしか行えていなかった — このスクリプトが `/proc` 前提だった
ためである。CLAUDE.md は Windows を最優先と定めているので、**Windows で評価
できない目標を Stage 1 (Issue #176) の完了条件に据えることはできない**
(docs/decisions.md D97 Revisit condition (3))。差分は 3 点:

1. **メモリの採り方**。`/proc` の代わりに Toolhelp32 + `QueryWorkingSet` を使う
   (`proctree.py`)。**Windows に PSS は無い** (D88)。代わりに真の PSS を挟む
   上下界 — Private Working Set 合計 (下界) と Working Set 合計 (上界) — を
   採り、区間が重なる間は「判定不能」と言う (`compare_bounds`)。
2. **仮想ディスプレイが要らない**。`DISPLAY` の確認は Linux でのみ行う。
3. **比較相手**。Windows の VeloX は WebView2 (= Edge と同じ Chromium エンジン)
   を使うので、**Edge と比べると「エンジン差」が消えて VeloX 自身のオーバー
   ヘッドだけが残る。** Linux (WebKitGTK 対 Blink) では不可能だった切り分けが
   Windows では可能になる。Chrome と Edge の両方を自動検出し、どちらを測ったかを
   結果に記録する。

使い方
------
Linux:

    xvfb-run -a --server-args="-screen 0 1280x900x24" \\
      python3 scripts/bench/compare_browsers.py \\
        --velox ./target/release/velox \\
        --chromium /opt/pw-browsers/chromium \\
        --page minimal.html --trials 5 --output results/compare.json

Windows (`--chromium` を省くと Chrome/Edge を自動検出する):

    python scripts\\bench\\compare_browsers.py ^
      --velox .\\target\\release\\velox.exe ^
      --page minimal.html --trials 5 --output results\\compare.json
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
import sys
import tempfile
import threading
import time
from pathlib import Path
from urllib.parse import urlparse

sys.path.insert(0, str(Path(__file__).resolve().parent))

from proctree import TreeMemory, process_tree_memory, supported  # noqa: E402

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


def velox_command(binary: str, url: str, data_dir: Path) -> tuple[list[str], dict]:
    return [binary, "--homepage", url], {"VELOX_DATA_DIR": str(data_dir)}


def chromium_command(binary: str, url: str, profile: Path) -> tuple[list[str], dict]:
    # 比較を成立させるための最小限のフラグだけを渡す。速度に効く最適化フラグは
    # 足さない (どちらかを有利にしないため)。--disable-gpu は GPU が無い環境で
    # 毎回フォールバックする分のばらつきを避けるため。
    argv = [
        binary,
        "--disable-gpu",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-features=Translate,MediaRouter",
        f"--user-data-dir={profile}",
        url,
    ]
    if not sys.platform == "win32":
        # --no-sandbox はこのコンテナが root で動くため。Windows では不要で、
        # **サンドボックスを切ると子プロセス構成が変わってメモリ比較が歪む**
        # ので付けない (プロセス数は §28.8 のとおり合計 RSS に直結する)。
        argv.insert(1, "--no-sandbox")
    return argv, {}


#: Windows で Chromium 系ブラウザを探す既定の場所。**Edge を先に探す** —
#: VeloX は WebView2 (= Edge と同じエンジン) を使うので、Edge との比較だけが
#: 「エンジン差を除いた VeloX 自身のオーバーヘッド」を示す。Chrome との比較は
#: エンジン差を含む (Linux で WebKitGTK 対 Blink を測っていたのと同じ性質)。
WINDOWS_BROWSER_CANDIDATES = (
    ("edge", r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"),
    ("edge", r"C:\Program Files\Microsoft\Edge\Application\msedge.exe"),
    ("chrome", r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
    ("chrome", r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe"),
)


def discover_chromium() -> tuple[str, str] | None:
    """比較相手の Chromium 系ブラウザを探して (名前, パス) を返す。

    見つからなければ `None` — **黙って比較を省略せず、呼び出し側でその旨を
    出力する。** 「比較相手が居ないまま VeloX だけ測れてしまった」結果を
    T2 の評価と取り違えないため。
    """
    if sys.platform == "win32":
        for name, path in WINDOWS_BROWSER_CANDIDATES:
            if Path(path).exists():
                return name, path
        return None
    for name, path in (("chromium", "/opt/pw-browsers/chromium"),):
        if Path(path).exists():
            return name, path
    for candidate in ("chromium", "chromium-browser", "google-chrome"):
        found = shutil.which(candidate)
        if found:
            return "chromium", found
    return None


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
        mem = process_tree_memory(proc.pid)
        return {
            "browser": name,
            "startup_to_load_ms": (loaded_at - started) * 1000.0,
            "rss_bytes": mem.rss_bytes,
            # Linux のみ。Windows では常に None (D88 — PSS 相当は存在しない)。
            "pss_bytes": mem.pss_bytes,
            # Windows のみ。Private Working Set 合計 = 真の PSS の下界。
            "private_bytes": mem.private_bytes,
            # 比較に使う区間。Linux では下界 = 上界 = PSS になる。
            "lower_bytes": mem.lower_bytes,
            "upper_bytes": mem.upper_bytes,
            "process_count": mem.process_count,
            "unreadable_count": mem.unreadable_count,
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


#: T2 の判定しきい値 (`docs/performance-targets.md` の「Chromium 比 +10% 以内」)。
T2_THRESHOLD = 0.10


def compare_bounds(
    velox_lower: float | None,
    velox_upper: float | None,
    base_lower: float | None,
    base_upper: float | None,
    threshold: float = T2_THRESHOLD,
) -> tuple[str, str]:
    """T2 (Chromium 比 +10% 以内) を、上下界の区間比較で判定する。

    Linux では PSS がカーネル計算値なので下界 = 上界となり、ふつうの
    「PSS 同士の比較」に退化する。Windows では PSS が存在しないため区間に幅が
    出る (`proctree.py` 参照)。**幅がある以上、判定できない場合が必ずある** —
    そのときに片方の端を代表値として選んで断定するのは、測れていないものを
    測れたことにする行為なので、明示的に「判定不能」を返す。

    - `met`          — VeloX の**上界**が Chromium の**下界** × (1+閾値) 以下。
                        どちらの真値を取っても達成しているので、確実に達成。
    - `missed`       — VeloX の**下界**が Chromium の**上界** × (1+閾値) 超。
                        どちらの真値を取っても超過するので、確実に未達。
    - `inconclusive` — 区間が重なる。真値の位置次第で結論が変わる。
    """
    if None in (velox_lower, velox_upper, base_lower, base_upper):
        return "unknown", "比較に必要な値が揃っていません"
    if base_lower <= 0 or base_upper <= 0:
        return "unknown", "比較相手のメモリが 0 でした (計測失敗)"

    pct = threshold * 100
    if velox_upper <= base_lower * (1 + threshold):
        return "met", (
            f"達成。VeloX の上界 ({velox_upper / 1024 / 1024:.1f} MiB) が "
            f"比較相手の下界 ({base_lower / 1024 / 1024:.1f} MiB) +{pct:.0f}% 以下"
        )
    if velox_lower > base_upper * (1 + threshold):
        excess = velox_lower / base_upper - 1.0
        return "missed", (
            f"未達。VeloX の下界 ({velox_lower / 1024 / 1024:.1f} MiB) が "
            f"比較相手の上界 ({base_upper / 1024 / 1024:.1f} MiB) を "
            f"+{excess * 100:.1f}% 超過 (許容 +{pct:.0f}%)"
        )
    return "inconclusive", (
        f"判定不能。VeloX [{velox_lower / 1024 / 1024:.1f}, "
        f"{velox_upper / 1024 / 1024:.1f}] MiB と比較相手 "
        f"[{base_lower / 1024 / 1024:.1f}, {base_upper / 1024 / 1024:.1f}] MiB の "
        f"区間が重なるため、真値の位置次第で結論が変わる"
    )


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--velox", required=True)
    parser.add_argument(
        "--chromium",
        help="比較相手のブラウザ。省略すると自動検出する "
        "(Windows: Edge → Chrome の順、Linux: chromium)",
    )
    parser.add_argument(
        "--baseline-name",
        help="結果に記録する比較相手の名前 (既定: chromium / 自動検出名)",
    )
    parser.add_argument(
        "--no-baseline",
        action="store_true",
        help="VeloX 単独で測る。**T2 の評価にはならない**",
    )
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

    if not supported():
        print(f"{sys.platform} ではプロセスツリーのメモリを採れません。")
        return 2
    # 仮想ディスプレイが要るのは Linux だけ。Windows のランナーは対話セッションを
    # 持つので、ここで弾くと Windows 計測がそもそも動かない (Issue #197)。
    if sys.platform.startswith("linux") and not os.environ.get("DISPLAY"):
        print("DISPLAY が未設定です。xvfb-run 経由で実行してください。")
        return 2

    port = free_port()
    server = Harness(("127.0.0.1", port), build_page(args.page))
    threading.Thread(target=server.serve_forever, daemon=True).start()

    browsers: list[tuple[str, str]] = [("velox", args.velox)]
    baseline_name: str | None = None
    if args.chromium:
        baseline_name = args.baseline_name or "chromium"
        browsers.append((baseline_name, args.chromium))
    elif not args.no_baseline:
        found = discover_chromium()
        if found is None:
            # 黙って VeloX だけ測ると、T2 の評価と取り違えられかねない。
            print(
                "比較相手の Chromium 系ブラウザが見つかりませんでした。"
                "--chromium でパスを指定するか、--no-baseline を付けてください。"
            )
            return 2
        detected_name, baseline_path = found
        # `--baseline-name` は自動検出時にも尊重する。渡された指定を黙って
        # 捨てると、結果 JSON の名前と実際に測ったブラウザがずれる。
        baseline_name = args.baseline_name or detected_name
        print(f"比較相手を自動検出しました: {detected_name} ({baseline_path})")
        browsers.append((baseline_name, baseline_path))

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
        # Epic #57 の絶対ルール5「OS ごとに結果を分ける」。どの OS で採った
        # 数字かが分からない結果は、後から並べて誤読される。
        "platform": sys.platform,
        "baseline": baseline_name,
        "memory_metric": (
            "pss (smaps_rollup)" if sys.platform.startswith("linux")
            else "working set の上下界 (PSS は Windows に存在しない — D88)"
        ),
        "browsers": {
            name: {
                "startup_to_load_ms": summarize(
                    [s["startup_to_load_ms"] for s in samples[name]]
                ),
                "rss_bytes": summarize([float(s["rss_bytes"]) for s in samples[name]]),
                # Linux のみ値が入る。Windows は全試行 None なので n=0 になる。
                "pss_bytes": summarize(
                    [float(s["pss_bytes"]) for s in samples[name]
                     if s["pss_bytes"] is not None]
                ),
                # Windows のみ値が入る (Private Working Set 合計)。
                "private_bytes": summarize(
                    [float(s["private_bytes"]) for s in samples[name]
                     if s["private_bytes"] is not None]
                ),
                "lower_bytes": summarize(
                    [float(s["lower_bytes"]) for s in samples[name]
                     if s["lower_bytes"] is not None]
                ),
                "upper_bytes": summarize(
                    [float(s["upper_bytes"]) for s in samples[name]]
                ),
                "process_count": summarize(
                    [float(s["process_count"]) for s in samples[name]]
                ),
                "unreadable_count": summarize(
                    [float(s["unreadable_count"]) for s in samples[name]]
                ),
            }
            for name, _ in browsers
        },
    }

    print(f"\npage={args.page} trials={args.trials} platform={sys.platform}")
    print(
        f"{'browser':<12}{'n':>3}{'load median(ms)':>18}"
        f"{'mem lower(MiB)':>17}{'mem upper(MiB)':>17}{'procs':>8}"
    )
    for name, _ in browsers:
        b = report["browsers"][name]
        if not b["startup_to_load_ms"]["n"]:
            print(f"{name:<12}{0:>3}{'—':>18}{'—':>17}{'—':>17}{'—':>8}")
            continue
        lower = b["lower_bytes"]
        print(
            f"{name:<12}{b['startup_to_load_ms']['n']:>3}"
            f"{b['startup_to_load_ms']['median']:>18.1f}"
            f"{(lower['median'] / 1024 / 1024) if lower['n'] else float('nan'):>17.1f}"
            f"{b['upper_bytes']['median'] / 1024 / 1024:>17.1f}"
            f"{b['process_count']['median']:>8.0f}"
        )

    if sys.platform.startswith("linux"):
        print(
            "\n注: メモリの上下界は Linux では両方 PSS (カーネル計算値) なので"
            "一致する。\nRSS 合計は共有メモリを二重計上するため、"
            "プロセス数の多いブラウザが不利に出る。"
        )
    else:
        print(
            "\n注: Windows に PSS は無い (docs/decisions.md D88)。lower は"
            " Private Working Set 合計、\nupper は Working Set 合計で、"
            "真の値はこの区間のどこかにある。**区間の端を代表値として"
            "\n引用しないこと。** OS をまたいだ数値比較も成立しない (Epic #57)。"
        )

    # --- T2 の判定 --------------------------------------------------------
    if baseline_name and report["browsers"][baseline_name]["upper_bytes"]["n"]:
        velox = report["browsers"]["velox"]
        base = report["browsers"][baseline_name]
        verdict, detail = compare_bounds(
            velox["lower_bytes"]["median"] if velox["lower_bytes"]["n"] else None,
            velox["upper_bytes"]["median"] if velox["upper_bytes"]["n"] else None,
            base["lower_bytes"]["median"] if base["lower_bytes"]["n"] else None,
            base["upper_bytes"]["median"] if base["upper_bytes"]["n"] else None,
        )
        report["t2"] = {
            "threshold": T2_THRESHOLD,
            "baseline": baseline_name,
            "verdict": verdict,
            "detail": detail,
        }
        print(f"\nT2 ({baseline_name} 比 +{T2_THRESHOLD * 100:.0f}% 以内): {detail}")
        if baseline_name == "edge":
            print(
                "  ※ VeloX は WebView2 (Edge と同じエンジン) を使うため、"
                "この比較にエンジン差は含まれない。\n"
                "     ここに残る差は VeloX 自身のオーバーヘッドである。"
            )
    else:
        report["t2"] = {"verdict": "unknown", "detail": "比較相手を測れていません"}
        print("\nT2: 比較相手を測れていないため評価していません。")

    if args.output:
        out = Path(args.output)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(json.dumps(report, indent=2, ensure_ascii=False), encoding="utf-8")
        print(f"\n結果を {out} に保存しました。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

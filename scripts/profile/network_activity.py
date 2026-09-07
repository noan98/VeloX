#!/usr/bin/env python3
"""バックグラウンドタブのネットワーク活動を外側から測る (Issue #65)。

なぜ VeloX 自身のメトリクスでは測れないか
------------------------------------------
`scripts/profile/cpu_usage.py` (Issue #64) は `/proc/<pid>/stat` を読むので
VeloX の外から絶対値の CPU 使用率が取れる。ネットワークにはそれに相当する
「外から読める既製のカウンタ」が存在しない — `/proc/net/dev` はインター
フェース単位の合計で、どのタブのどのリクエストかを一切区別できない。

さらに `docs/decisions.md` D17/D59 が確認したとおり、**wry
0.56 はサブリソース単位のリクエスト横取りフックを一切公開しておらず、
唯一の例外 (`ICoreWebView2::WebResourceRequested` 経由) も Windows
(WebView2) だけで、macOS (WKWebView) / Linux (WebKitGTK) には同等の
拡張トレイトが無い**。つまりこの開発環境 (Linux/WebKitGTK) では VeloX
自身がリクエスト単位のイベントを一切観測できない。

このスクリプトは Issue #64 の `?beacon=1` と同じ発想を全パターンに広げ、
**VeloX の外に置いたローカル HTTP/WebSocket サーバのアクセスログ**で
数える。測定コストは VeloX の中には一切乗らない。

パターンの分類 (`scripts/bench/pages/network_activity.html` が発生させる)
--------------------------------------------------------------------------
| パス | パターン | Issue #65 の分類 |
| --- | --- | --- |
| `GET /poll` | 2 秒おきの `fetch`/`XMLHttpRequest` | polling / periodic fetch・XHR |
| `GET /pixel.gif` | 3 秒おきの `<img>` src 張り替え | background resource loading |
| `GET /prefetch-target` | `<link rel=prefetch>` を 1 回だけ挿入 | prefetch |
| `GET /prefetch-armed` | 上の挿入直後に無条件で 1 回 (スクリプト自体が動いた確認用、Issue #65 の分類には数えない) | — |
| `ws /ws` (open/message/close) | 2 秒おきの WebSocket 心拍 | 保護対象 (壊してはいけない) |

使い方
------
    # active: network_activity.html がアクティブタブ
    printf 'wait 60000\nquit\n' > /tmp/active.txt
    xvfb-run -a --server-args="-screen 0 1280x900x24" dbus-run-session -- \
      python3 scripts/profile/network_activity.py --velox ./target/release/velox \
        --script /tmp/active.txt --label active --window-secs 16

    # background: network_activity.html をバックグラウンドに送ってから測る
    printf 'wait 1500\nopen about:blank\nwait 60000\nquit\n' > /tmp/bg.txt
    ... --script /tmp/bg.txt --label background --window-secs 16

    # suspension: 加えて短い auto-suspend を効かせる (#63 との連携)
    ... --label suspended --suspend-after-ms 3000 --window-secs 16

`--homepage` を渡さない場合は `network_activity.html` を自動生成した
URL に差し替える (アクティブ計測用)。バックグラウンド計測ではスクリプト側で
先に `network_activity.html` を開いてから別タブを開く形にすること —
`cpu_usage.py` と同じ役割分担。
"""

from __future__ import annotations

import argparse
import http.server
import json
import os
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
from base64 import b64encode
from hashlib import sha1
from pathlib import Path
from urllib.parse import urlparse

PAGES_DIR = Path(__file__).resolve().parent.parent / "bench" / "pages"
FIXTURE = "network_activity.html"
WS_MAGIC = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

# サーバログの各パスがどの Issue #65 分類に属するか。`GET /network_activity.html`
# 自身と `favicon.ico` はページ読み込みの一部であって「バックグラウンド
# ネットワーク活動」ではないので分類から除く。
CLASSIFICATION = {
    "poll": "polling_periodic_fetch_xhr",
    "pixel.gif": "background_resource_loading",
    "prefetch-target": "prefetch",
    # Fired unconditionally right after the `<link rel=prefetch>` element is
    # inserted (see network_activity.html) so a result can tell "the script
    # never ran" apart from "the script ran but the engine never actually
    # requested the prefetch target" — not itself one of Issue #65's
    # patterns, so it stays out of `prefetch`'s own count.
    "prefetch-armed": "prefetch_script_ran",
    "ws:open": "websocket_protected",
    "ws:message": "websocket_protected",
    "ws:close": "websocket_protected",
}


class LogEntry:
    __slots__ = ("t", "path")

    def __init__(self, t: float, path: str):
        self.t = t
        self.path = path


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, addr):
        super().__init__(addr, _Handler)
        self.log: list[LogEntry] = []
        self.lock = threading.Lock()

    def record(self, path: str) -> None:
        with self.lock:
            self.log.append(LogEntry(time.monotonic(), path))


class _Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):  # noqa: N802
        parsed = urlparse(self.path)
        path = parsed.path.lstrip("/")

        if path == "" or path == FIXTURE:
            self._serve_fixture()
            return
        if path == "ws":
            self._maybe_upgrade_websocket()
            return
        if path in ("poll", "pixel.gif", "prefetch-target", "prefetch-armed"):
            self.server.record(path)
            body = b"{}" if path == "poll" else b"\x00"
            content_type = "application/json" if path == "poll" else "application/octet-stream"
            self._respond(body, content_type)
            return
        self.send_error(404)

    def _serve_fixture(self) -> None:
        html = (PAGES_DIR / FIXTURE).read_text(encoding="utf-8")
        self._respond(html.encode("utf-8"), "text/html; charset=utf-8")

    def _respond(self, body: bytes, content_type: str) -> None:
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _maybe_upgrade_websocket(self) -> None:
        """`ws://.../ws` への最小限の RFC 6455 ハンドシェイクと、その後の
        テキストフレーム受信ループ。WebSocket 用ライブラリを新規に依存
        追加しない (D6) ための自前実装 — 標準ライブラリの `socket`/
        `hashlib`/`base64` だけで足りるほど RFC 6455 のハンドシェイクは
        小さい。心拍メッセージを受け取るだけで応答は返さない (fire-and-
        forget の心拍を模しているだけなので、往復させる意味が無い)。
        """
        key = self.headers.get("Sec-WebSocket-Key")
        if not key or self.headers.get("Upgrade", "").lower() != "websocket":
            self.send_error(400, "not a websocket upgrade")
            return
        accept = b64encode(sha1((key + WS_MAGIC).encode("ascii")).digest()).decode("ascii")
        self.send_response(101, "Switching Protocols")
        self.send_header("Upgrade", "websocket")
        self.send_header("Connection", "Upgrade")
        self.send_header("Sec-WebSocket-Accept", accept)
        self.end_headers()
        self.server.record("ws:open")
        try:
            self._read_ws_frames()
        except (OSError, ConnectionError):
            pass
        finally:
            self.server.record("ws:close")

    def _read_ws_frames(self) -> None:
        # 心拍メッセージは小さいテキストフレームのみ (マスク付き、
        # クライアント→サーバは RFC 6455 で必須)。フラグメント化・
        # 制御フレームの厳密な扱いは、この固定ページが送るものの範囲外
        # なので実装しない — 最小限のテキストフレームだけを読む。
        conn = self.connection
        conn.settimeout(1.0)
        while True:
            try:
                header = self._recv_exact(conn, 2)
            except TimeoutError:
                # 心拍間隔より短い周期でポーリングし、外側の window-secs
                # が尽きたらソケットごと閉じられるので、ここは単に
                # ループを続ける。
                continue
            if not header:
                return
            second_byte = header[1]
            masked = second_byte & 0x80
            length = second_byte & 0x7F
            if length == 126:
                length = int.from_bytes(self._recv_exact(conn, 2), "big")
            elif length == 127:
                length = int.from_bytes(self._recv_exact(conn, 8), "big")
            mask_key = self._recv_exact(conn, 4) if masked else b"\x00\x00\x00\x00"
            payload = self._recv_exact(conn, length)
            if masked:
                payload = bytes(b ^ mask_key[i % 4] for i, b in enumerate(payload))
            opcode = header[0] & 0x0F
            if opcode == 0x8:  # close
                return
            if opcode == 0x1:  # text frame
                self.server.record("ws:message")

    @staticmethod
    def _recv_exact(conn, n: int) -> bytes:
        if n == 0:
            return b""
        buf = b""
        conn.settimeout(1.0)
        deadline = time.monotonic() + 30
        while len(buf) < n:
            if time.monotonic() > deadline:
                raise ConnectionError("timed out reading websocket frame")
            try:
                chunk = conn.recv(n - len(buf))
            except TimeoutError:
                continue
            if not chunk:
                raise ConnectionError("peer closed")
            buf += chunk
        return buf

    def log_message(self, *args):  # 計測中の標準エラー出力を汚さない
        pass


def run_velox(velox: str, script_path: str, homepage: str, data_dir: str,
              settle_secs: float, window_secs: float,
              suspend_after_ms: int | None, max_tabs_per_process: int | None):
    env = {**os.environ, "VELOX_DATA_DIR": data_dir,
           "VELOX_AUTOMATION_SCRIPT": script_path, "VELOX_HOMEPAGE": homepage}
    if suspend_after_ms is not None:
        env["VELOX_AUTO_SUSPEND_AFTER_MS"] = str(suspend_after_ms)
    if max_tabs_per_process is not None:
        env["VELOX_MAX_TABS_PER_PROCESS"] = str(max_tabs_per_process)
    proc = subprocess.Popen([velox], env=env,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(settle_secs)
    early_exit = proc.poll() is not None
    time.sleep(window_secs)
    return proc, early_exit


def summarize(log: list[LogEntry], window_start: float, window_end: float) -> dict:
    windowed = [e for e in log if window_start <= e.t <= window_end]
    counts: dict[str, int] = {}
    for entry in windowed:
        cls = CLASSIFICATION.get(entry.path, entry.path)
        counts[cls] = counts.get(cls, 0) + 1
    wall = window_end - window_start
    return {
        "counts": counts,
        "rates_per_sec": {k: round(v / wall, 3) for k, v in counts.items()},
        "wall_secs": round(wall, 1),
        "total_requests": len(windowed),
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                  formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--velox", required=True)
    ap.add_argument("--script", required=True, help="自動操作スクリプト")
    ap.add_argument("--homepage", default=None,
                     help="省略時は network_activity.html をサーバから配信して使う")
    ap.add_argument("--fixture-query", default="",
                     help="--homepage を省略したとき、自動生成する URL に付ける"
                          " クエリ文字列 (例: 'poll_ms=200')。ポート番号は実行時にしか"
                          " 決まらないため、--homepage 自身には書けない")
    ap.add_argument("--settle-secs", type=float, default=6.0)
    ap.add_argument("--window-secs", type=float, default=16.0)
    ap.add_argument("--data-dir", default=None)
    ap.add_argument("--label", default="")
    ap.add_argument("--suspend-after-ms", type=int, default=None,
                     help="VELOX_AUTO_SUSPEND_AFTER_MS を設定する (Issue #63 との連携を測るとき)")
    ap.add_argument("--max-tabs-per-process", type=int, default=None)
    ap.add_argument("--json", action="store_true", help="機械可読な JSON で出力する")
    args = ap.parse_args()

    server = Server(("127.0.0.1", 0))
    port = server.server_address[1]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    query = f"?{args.fixture_query}" if args.fixture_query else ""
    homepage = args.homepage or f"http://127.0.0.1:{port}/{FIXTURE}{query}"
    data_dir = args.data_dir or tempfile.mkdtemp(prefix="velox-network-")

    proc, early_exit = run_velox(
        args.velox, args.script, homepage, data_dir,
        args.settle_secs, args.window_secs,
        args.suspend_after_ms, args.max_tabs_per_process,
    )
    window_end = time.monotonic()
    window_start = window_end - args.window_secs

    if early_exit:
        print(f"{args.label}: velox が早期終了しました", file=sys.stderr)
    result = summarize(server.log, window_start, window_end)
    result["label"] = args.label

    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=10)
    server.shutdown()

    if args.json:
        print(json.dumps(result, ensure_ascii=False))
    else:
        counts_str = " ".join(f"{k}={v}" for k, v in sorted(result["counts"].items()))
        print(f"{args.label}\ttotal={result['total_requests']}\twall={result['wall_secs']}"
              f"\t{counts_str}")
    return 1 if early_exit else 0


if __name__ == "__main__":
    sys.exit(main())

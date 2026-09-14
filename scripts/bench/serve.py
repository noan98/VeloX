#!/usr/bin/env python3
"""固定ベンチページを配信し、`busy.html` の beacon を数えるサーバ (Issue #247)。

## なぜ `python -m http.server` ではないのか

ふだんの計測は `python -m http.server` で足りている。これが要るのは
**`?beacon=1` を数えたいときだけ**で、perf-windows も `beacon` 入力が
真のときしかこちらを使わない。既定の計測環境を黙って変えないためである。

## なぜ beacon なのか — タイトルを読むのではなく

`busy.html` は rAF / `setInterval` のカウンタを `<title>` に書き出す。
しかし**それを VeloX 側から読むには、計測対象のタブで JS を評価する
必要がある**。背景タブに `MemoryUsageTargetLevel(LOW)` を伝えた状態で
JS を走らせれば、**測ろうとしている当のものを乱しうる** (docs/decisions.md
D122 決定4 が `IsSuspended` について書いたのと同じ危険)。

`?beacon=1` はページが自分で HTTP リクエストを投げるので、
**ブラウザの外側で数えられる。** Issue #64 がこのオプションを用意した
理由がまさにこれで、`busy.html` のコメントはこう書いている —
「CPU 使用量は『少ない』ことしか示せず、0 との区別がつかない」。

## リクエストごとのログは出さない

`http.server` は**リクエストごとに stderr へ 1 行書く**。
`docs/performance-targets.md` §26.4 は、その出力をファイルへ
リダイレクトした 2 つの run で `page_load_ms` が 54ms → 2077ms と
**38 倍**になったことを記録している (`http.server` はシングルスレッドで、
書き込み先がファイルだと応答をブロックしうる)。

ここでは `log_message` を握り潰すので、**今より I/O が減ることはあっても
増えることはない。** beacon の集計はメモリ上のカウンタで、ディスクに
触らない。
"""

from __future__ import annotations

import argparse
import json
import os
import threading
from functools import partial
from http.server import HTTPServer, SimpleHTTPRequestHandler
from urllib.parse import parse_qs, urlsplit

#: beacon を受け取るパス。`busy.html` の `fetch("beacon?...")` は
#: ページと同じディレクトリからの相対なので `/beacon` に来る。
BEACON_PATH = "/beacon"
#: 集計結果を JSON で返すパス。ワークフローが計測後に 1 回だけ叩く。
SUMMARY_PATH = "/beacon-summary"


class BeaconCounts:
    """`state` (`visibilityState`) ごとの beacon の集計。

    数えたいのは**回数だけではない**。「隠れているタブでカウンタが進んだか」
    が問いなので、`f=` (rAF の回数) と `t=` (`setInterval` の発火回数) の
    **最初と最後**を覚えておく。進んでいなければ最初と最後が同じになる。

    ソケットを持たない純粋なデータなので、単体テストから直接叩ける。
    """

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._states: dict[str, dict[str, int]] = {}

    def record(self, query: str) -> None:
        """`state=hidden&f=12&t=34` の形のクエリを 1 件取り込む。

        壊れた値は黙って捨てる。**ここで例外を投げると計測そのものが
        落ちる**ので、集計の欠落より継続を優先する。
        """
        params = parse_qs(query)
        state = (params.get("state") or ["unknown"])[0]
        frames = _first_int(params.get("f"))
        ticks = _first_int(params.get("t"))
        if frames is None or ticks is None:
            return
        with self._lock:
            entry = self._states.get(state)
            if entry is None:
                self._states[state] = {
                    "count": 1,
                    "first_frames": frames,
                    "first_ticks": ticks,
                    "last_frames": frames,
                    "last_ticks": ticks,
                }
                return
            entry["count"] += 1
            entry["last_frames"] = frames
            entry["last_ticks"] = ticks

    def summary(self) -> dict[str, dict[str, int]]:
        """集計を JSON にできる形で返す。

        `advanced_*` は「最後 − 最初」で、**これが 0 なら、その状態の
        タブではカウンタが一度も進まなかった**ことを意味する。
        """
        with self._lock:
            out: dict[str, dict[str, int]] = {}
            for state, entry in sorted(self._states.items()):
                out[state] = dict(entry)
                out[state]["advanced_frames"] = entry["last_frames"] - entry["first_frames"]
                out[state]["advanced_ticks"] = entry["last_ticks"] - entry["first_ticks"]
            return out


def _first_int(values: list[str] | None) -> int | None:
    """クエリの先頭の値を非負整数として読む。読めなければ `None`。"""
    if not values:
        return None
    try:
        parsed = int(values[0])
    except ValueError:
        return None
    return parsed if parsed >= 0 else None


class BenchHandler(SimpleHTTPRequestHandler):
    """固定ページの配信 + beacon の集計。"""

    def __init__(self, *args, counts: BeaconCounts, **kwargs) -> None:
        self._counts = counts
        super().__init__(*args, **kwargs)

    def do_GET(self) -> None:  # noqa: N802 - http.server の命名に合わせる
        path = urlsplit(self.path).path
        if path == BEACON_PATH:
            self._counts.record(urlsplit(self.path).query)
            # 本文を返さない。ページ側は応答を読まないし
            # (`.catch(() => {})`)、返すだけ無駄な仕事になる。
            self.send_response(204)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if path == SUMMARY_PATH:
            body = json.dumps(self._counts.summary()).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        super().do_GET()

    def log_message(self, *args, **kwargs) -> None:
        """リクエストごとのログを出さない (モジュール冒頭の §26.4 参照)。"""


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=8731)
    parser.add_argument(
        "--directory",
        default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "pages"),
    )
    args = parser.parse_args()

    counts = BeaconCounts()
    handler = partial(BenchHandler, counts=counts, directory=args.directory)
    HTTPServer(("127.0.0.1", args.port), handler).serve_forever()


if __name__ == "__main__":
    main()

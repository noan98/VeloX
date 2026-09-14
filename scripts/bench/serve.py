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
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

#: beacon を受け取るパス。`busy.html` の `fetch("beacon?...")` は
#: ページと同じディレクトリからの相対なので `/beacon` に来る。
BEACON_PATH = "/beacon"
#: 待ち行列を深めに取る。`busy.html` は 500ms ごとに beacon を投げ、
#: `background_cpu` は 2 タブ分がそれをやる。**ページ側は
#: `.catch(() => {})` で失敗を握り潰すので、接続が拒否されても
#: 何も言わずに消える** — つまり取りこぼしは「タイマーが止まった」
#: ように見える。既定の 5 では足りうるので広げておく。
REQUEST_QUEUE_SIZE = 128
#: 集計結果を JSON で返すパス。ワークフローが計測後に 1 回だけ叩く。
SUMMARY_PATH = "/beacon-summary"
#: 集計を返したうえで空にするパス (Issue #247)。**A/B の腕の切り替え時に
#: これを叩かないと、両腕の beacon が同じバケットに積み上がる。**
#: §42.5 はそれで `low` と `normal` を分離できなかった。
RESET_PATH = "/beacon-reset"


class BeaconCounts:
    """`state` (`visibilityState`) ごとの beacon の集計。

    数えたいのは**回数だけではない**。「隠れているタブでカウンタが進んだか」
    が問いなので、`f=` (rAF の回数) と `t=` (`setInterval` の発火回数) が
    どれだけ進んだかを数える。

    ## なぜページ「インスタンス」ごとに持つのか (Issue #247)

    初版は state ごとに「最初の値」と「最後の値」だけを持ち、その差を
    進んだ量としていた。**それは壊れていた。**

    1. **1 つの state に複数のタブが入る。** `background_cpu` は 12 個の
       背景タブを開き、全部が `state=hidden` で報告する。別々のタブの
       `f=` を並べて引き算しても意味がない。
    2. **velox は 1 run で何度も起動し直す。** そのたびにページは
       読み込み直され、カウンタは 0 から始まる。

    結果、`advanced_frames` が **-5** のような負の値になった
    (`docs/performance-targets.md` §42.4)。負の「進んだ量」は、指標が
    壊れている合図である。

    そこで `id=` (ページが読み込みごとに作る乱数) ごとに最初と最後を
    持ち、**その差を合計する。** インスタンスをまたいで引き算しない。

    `id=` が無い beacon (古い `busy.html` など) は、すべて同じ
    `"-"` インスタンスとして扱う。初版と同じ壊れ方をするが、**混ざって
    いることが `instances` の数から見える**ので、黙って誤らせない。

    ソケットを持たない純粋なデータなので、単体テストから直接叩ける。
    """

    #: `id=` を持たない beacon をまとめる先。
    UNKNOWN_INSTANCE = "-"

    def __init__(self) -> None:
        self._lock = threading.Lock()
        # state -> instance id -> {count, first_frames, last_frames, ...}
        self._states: dict[str, dict[str, dict[str, int]]] = {}

    def record(self, query: str) -> None:
        """`state=hidden&f=12&t=34&id=abc` の形のクエリを 1 件取り込む。

        壊れた値は黙って捨てる。**ここで例外を投げると計測そのものが
        落ちる**ので、集計の欠落より継続を優先する。
        """
        params = parse_qs(query)
        state = (params.get("state") or ["unknown"])[0]
        frames = _first_int(params.get("f"))
        ticks = _first_int(params.get("t"))
        if frames is None or ticks is None:
            return
        instance = (params.get("id") or [self.UNKNOWN_INSTANCE])[0]
        # JS ヒープ (Issue #247)。Chromium 以外では送られてこないので
        # 欠けていてもよい。**欠けていることと 0 は違う**ので、
        # 集計側も届いた beacon の数を別に数える。
        heap = _first_int(params.get("h"))
        with self._lock:
            instances = self._states.setdefault(state, {})
            entry = instances.get(instance)
            if entry is None:
                instances[instance] = {
                    "count": 1,
                    "first_frames": frames,
                    "first_ticks": ticks,
                    "last_frames": frames,
                    "last_ticks": ticks,
                    "heap_count": 0 if heap is None else 1,
                    "heap_sum": 0 if heap is None else heap,
                    "heap_max": 0 if heap is None else heap,
                }
                return
            entry["count"] += 1
            # **同一インスタンスでカウンタが減ることは原理的に無い。**
            # `frames` / `ticks` は単調増加しかしないので、小さい値が
            # 後から届いたら「カウンタが戻った」のではなく
            # **beacon の到着順が入れ替わった**ということである
            # (ページは `fetch` を投げっぱなしにし、応答も順序も待たない)。
            #
            # そこで min/max で範囲を取る。先に届いたほうを first と
            # 決め打つと、順序が入れ替わっただけで進んだ量が縮む。
            # この形なら **進んだ量は構造的に負にならない。**
            entry["first_frames"] = min(entry["first_frames"], frames)
            entry["first_ticks"] = min(entry["first_ticks"], ticks)
            entry["last_frames"] = max(entry["last_frames"], frames)
            entry["last_ticks"] = max(entry["last_ticks"], ticks)
            if heap is not None:
                entry["heap_count"] += 1
                entry["heap_sum"] += heap
                entry["heap_max"] = max(entry["heap_max"], heap)

    def summary(self) -> dict[str, dict[str, int]]:
        """集計を JSON にできる形で返す。

        `advanced_*` は**インスタンスごとの「最後 − 最初」の合計**で、
        **これが 0 なら、その状態のタブではカウンタが一度も進まなかった**
        ことを意味する。インスタンスをまたいで引き算しないので、
        負にはならない。

        `instances` はその state に何個のページ実体が居たか。
        `background_cpu` なら背景タブの数 × 起動回数に近い値になるはずで、
        **1 なら `id=` が届いていない**ことを疑う。

        `heap_*` は `performance.memory.usedJSHeapSize` (Issue #247)。
        `MemoryUsageTargetLevel(LOW)` が何を縮めたのかの 3 候補
        (JS ヒープ / 描画バッファ / キャッシュ) のうち、**ページから
        見えるのはこれだけ**である (D129 決定6)。Chromium 系にしか
        無いので、**1 件も届かなければ列ごと落とす** — 0 を
        「ヒープが 0」と読ませないため。
        """
        with self._lock:
            return self._summary_locked()

    def reset(self) -> dict[str, dict[str, int]]:
        """集計を返したうえで空にする (Issue #247)。

        **A/B の腕の切り替え時に呼ぶ。** 呼ばないと両腕の beacon が同じ
        バケットに積み上がり、`low` と `normal` を分離できない (§42.5)。

        返すのは「これから捨てる分」なので、呼び出し側はそれを腕の
        結果として記録できる。取得と初期化が 1 回のロックの中で起きる
        ので、その間に来た beacon が**どちらにも入らない / 両方に入る**
        ということはない。
        """
        with self._lock:
            out = self._summary_locked()
            self._states.clear()
            return out

    def _summary_locked(self) -> dict[str, dict[str, int]]:
        """`summary()` の中身。**呼び出し側がロックを持っていること。**"""
        out: dict[str, dict[str, int]] = {}
        for state, instances in sorted(self._states.items()):
            total = {
                "count": 0,
                "instances": len(instances),
                "advanced_frames": 0,
                "advanced_ticks": 0,
                "heap_count": 0,
                "heap_sum": 0,
                "heap_max": 0,
            }
            for entry in instances.values():
                total["count"] += entry["count"]
                total["advanced_frames"] += entry["last_frames"] - entry["first_frames"]
                total["advanced_ticks"] += entry["last_ticks"] - entry["first_ticks"]
                total["heap_count"] += entry["heap_count"]
                total["heap_sum"] += entry["heap_sum"]
                total["heap_max"] = max(total["heap_max"], entry["heap_max"])
            # 平均は**届いた beacon の数で割る。** count で割ると、
            # `performance.memory` の無い環境で値が薄まる。
            # 0 件のときは列ごと落とす — 0 を「ヒープが 0」と読ませない。
            if total["heap_count"]:
                total["heap_mean"] = total["heap_sum"] // total["heap_count"]
            else:
                for key in ("heap_count", "heap_sum", "heap_max"):
                    del total[key]
            out[state] = total
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


class BenchServer(ThreadingHTTPServer):
    """スレッド化した HTTP サーバ。

    **1 リクエストずつ捌く `HTTPServer` だと beacon を取りこぼしうる。**
    ページ側は `fetch` の失敗を握り潰すので、取りこぼしは
    「beacon が来ていない」= 「タイマーが止まっている」と**区別が
    つかない見え方**をする。計測の結論を左右する取り違えなので、
    サーバ側で起こりにくくしておく。

    §26.4 が記録した「I/O が応答をブロックして page_load_ms が 38 倍」
    という失敗とは逆方向の変更である (捌く側を増やしている)。
    """

    daemon_threads = True
    request_queue_size = REQUEST_QUEUE_SIZE


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
        if path in (SUMMARY_PATH, RESET_PATH):
            # reset は「返してから空にする」。返す中身は summary と同じなので、
            # 呼び出し側は腕ごとの結果としてそのまま記録できる。
            counts = self._counts.reset() if path == RESET_PATH else self._counts.summary()
            body = json.dumps(counts).encode("utf-8")
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
    server = BenchServer(("127.0.0.1", args.port), handler)
    server.serve_forever()


if __name__ == "__main__":
    main()

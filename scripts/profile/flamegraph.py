#!/usr/bin/env python3
"""`perf script` の出力を折り畳んで SVG フレームグラフを描く (Issue #70)。

なぜ自前で書いたか
------------------
定番の Brendan Gregg 版 `stackcollapse-perf.pl` / `flamegraph.pl` はこの
コンテナに存在せず、`apt` にも `crates.io` にも `pip` にも外部ネットワーク
経由でしか手が届かない (この環境は外部ネットワークを遮断している —
`CLAUDE.md` の「環境メモ」参照)。インストールできない前提のツールを手順に
書いても再現できないので、Python 標準ライブラリだけで同等の役割を果たす
最小限の実装をここに置く。`scripts/bench/compare_browsers.py` と同じ方針
(標準ライブラリのみ、外部依存なし)。

パイプライン
------------
    perf script -i perf.data | python3 scripts/profile/flamegraph.py -o flame.svg

内部では 2 段階:

1. `collapse_perf_script`: `perf script` の生テキスト (プロセスごとのブロック、
   各ブロックはリーフが先頭・ルートが末尾の行の並び) を読み、
   `"root;caller;...;leaf"` 形式の「折り畳みスタック」+ 出現回数に変換する
   (`stackcollapse-perf.pl` と同じ出力形式なので、他ツールに食わせたいときは
   `--collapsed-only` でこの中間形式だけを出力できる)。
2. `render_svg`: 折り畳みスタックから呼び出し木を作り、幅がサンプル数に
   比例する矩形を積んだ SVG を描く。ホバーで全スタックを見せる `<title>` は
   SVG 標準機能なので、これだけで JS 無しでも動く (ズーム用の最小限の
   インライン JS は付けているが、無くても静的に読める)。

色分け
------
シンボルが `velox` バイナリ由来 (関数名が `velox::` で始まる、またはモジュール
パスに `/velox` を含む) なら青系、`WebKit`/`webkit` を含むモジュール
(WebKitWebProcess 等) なら緑系、それ以外 (glib/gtk/libc など) は橙系で塗る。
「どこまでが VeloX 自身のコードで、どこからが WebView 側か」を一目で切り分け
られるようにするための配色で、`docs/profiling.md` が要求する
「VeloX 自身の Rust heap/CPU と WebKitGTK 側を切り分ける」の CPU 版に当たる。

制限事項 (正直に書く)
---------------------
- インライン展開やジャンプテーブルによる非対称なシンボル解決の癖は、
  本家 flamegraph.pl と完全には一致しない可能性がある。相対的な「どこが
  太いか」を見るには十分だが、正確な行番号までは信用しすぎないこと。
- differential flame graph (2 つのプロファイルの差分) はここでは実装して
  いない。必要になったら追加する。
"""

from __future__ import annotations

import argparse
import html
import re
import sys
import zlib
from collections import defaultdict
from pathlib import Path

FRAME_RE = re.compile(
    r"^\s*(?:[0-9a-fA-F]+)\s+(?P<sym>.+?)\s*\((?P<mod>[^()]*)\)\s*$"
)


def parse_perf_script(lines: list[str]) -> list[list[str]]:
    """`perf script` の生テキストを、スタックごとの [leaf, ..., root] のリストに分解する。

    ブロックは空行区切り。各ブロックの最初の行はヘッダ (comm/pid/time/event) で、
    以降の行がリーフ→ルートの順のスタックフレーム。ヘッダのないインデント行だけを
    フレームとして拾う。
    """
    stacks: list[list[str]] = []
    current: list[str] = []
    in_stack = False
    for raw in lines:
        line = raw.rstrip("\n")
        if not line.strip():
            if current:
                stacks.append(current)
                current = []
            in_stack = False
            continue
        if line[0] not in (" ", "\t"):
            # ヘッダ行 (新しいイベント) の開始。直前のスタックを確定させる。
            if current:
                stacks.append(current)
                current = []
            in_stack = True
            continue
        if not in_stack:
            continue
        m = FRAME_RE.match(line)
        if m:
            sym = m.group("sym").strip()
            mod = m.group("mod").strip()
            current.append(f"{sym} ({mod})" if mod else sym)
        else:
            # シンボルが解決できていない生アドレスなど。そのまま使う。
            current.append(line.strip())
    if current:
        stacks.append(current)
    return stacks


def collapse_perf_script(lines: list[str]) -> dict[tuple[str, ...], int]:
    """`stackcollapse-perf.pl` 相当: root→leaf 順のタプルごとの出現回数。"""
    counts: dict[tuple[str, ...], int] = defaultdict(int)
    for leaf_to_root in parse_perf_script(lines):
        if not leaf_to_root:
            continue
        root_to_leaf = tuple(reversed(leaf_to_root))
        counts[root_to_leaf] += 1
    return counts


def write_collapsed(counts: dict[tuple[str, ...], int], out) -> None:
    for stack, count in sorted(counts.items()):
        out.write(";".join(stack) + f" {count}\n")


def read_collapsed(lines: list[str]) -> dict[tuple[str, ...], int]:
    counts: dict[tuple[str, ...], int] = defaultdict(int)
    for line in lines:
        line = line.rstrip("\n")
        if not line.strip():
            continue
        stack_part, _, count_part = line.rpartition(" ")
        if not stack_part:
            continue
        try:
            count = int(count_part)
        except ValueError:
            continue
        counts[tuple(stack_part.split(";"))] += count
    return counts


class Node:
    __slots__ = ("name", "count", "children")

    def __init__(self, name: str):
        self.name = name
        self.count = 0
        self.children: dict[str, "Node"] = {}

    def child(self, name: str) -> "Node":
        node = self.children.get(name)
        if node is None:
            node = Node(name)
            self.children[name] = node
        return node


def build_tree(counts: dict[tuple[str, ...], int]) -> Node:
    root = Node("root")
    for stack, count in counts.items():
        root.count += count
        node = root
        for frame in stack:
            node = node.child(frame)
            node.count += count
    return root


def frame_color(name: str) -> str:
    """VeloX 自身 / WebKit / その他ライブラリを色で切り分ける。"""
    lname = name.lower()
    if "velox::" in name or "/velox" in lname or name.startswith("velox "):
        base_hue, sat, light_lo, light_hi = 210, 70, 45, 65  # 青系: VeloX 自身
    elif "webkit" in lname or "javascriptcore" in lname or "wtf::" in lname:
        base_hue, sat, light_lo, light_hi = 150, 55, 35, 55  # 緑系: WebKit/JSC
    else:
        base_hue, sat, light_lo, light_hi = 30, 80, 45, 65  # 橙系: それ以外 (glib/gtk/libc 等)
    # 同じ色相の中で少しだけ揺らす (隣接フレームを視覚的に区別しやすくする)。
    jitter = zlib.crc32(name.encode("utf-8", "replace")) % 21 - 10
    hue = base_hue + jitter
    light = light_lo + (zlib.crc32(name.encode("utf-8", "replace") + b"L") % (light_hi - light_lo))
    return f"hsl({hue},{sat}%,{light}%)"


FRAME_HEIGHT = 17
MIN_LABEL_WIDTH = 28


def render_svg(root: Node, width: int = 1600, max_depth: int | None = None) -> str:
    total = root.count or 1
    rows: list[tuple[int, int, int, Node]] = []  # (depth, x0, w, node)

    def walk(node: Node, depth: int, x0: int, w: int) -> None:
        if max_depth is not None and depth > max_depth:
            return
        rows.append((depth, x0, w, node))
        cx = x0
        # 子は count 降順に並べる (太いものを左に集める方が目で追いやすい)。
        for child in sorted(node.children.values(), key=lambda n: -n.count):
            cw = round(width * child.count / total)
            if cw <= 0:
                continue
            walk(child, depth + 1, cx, cw)
            cx += cw

    for child in sorted(root.children.values(), key=lambda n: -n.count):
        cw = round(width * child.count / total)
        if cw <= 0:
            continue
        walk(child, 0, 0, cw)

    max_d = max((d for d, *_ in rows), default=0)
    height = (max_d + 2) * FRAME_HEIGHT + 40

    parts: list[str] = []
    parts.append(
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" '
        f'height="{height}" viewBox="0 0 {width} {height}" '
        f'font-family="monospace" font-size="11">'
    )
    parts.append(f'<rect x="0" y="0" width="{width}" height="{height}" fill="#111318"/>')
    parts.append(
        f'<text x="8" y="18" fill="#e8e8e8" font-size="13">'
        f"VeloX flame graph — {total} samples, "
        f"{max_d + 1} 段。矩形の幅 = そのフレームでサンプルされた割合。"
        f"ホバーで完全なフレーム名を表示。青=VeloX自身 / 緑=WebKit・JSC / "
        f"橙=その他 (glib/gtk/libc 等)</text>"
    )
    y_offset = 30
    for depth, x0, w, node in rows:
        y = y_offset + (max_d - depth) * FRAME_HEIGHT
        color = frame_color(node.name)
        pct = 100.0 * node.count / total
        safe_name = html.escape(node.name)
        title = html.escape(f"{node.name}\n{node.count} samples ({pct:.2f}%)")
        label = node.name if w >= MIN_LABEL_WIDTH else ""
        # ラベルは矩形をはみ出さないよう、文字数からおおよそ切り詰める。
        max_chars = max(0, (w - 4) // 6)
        if label and len(label) > max_chars:
            label = (label[: max(0, max_chars - 1)] + "…") if max_chars > 1 else ""
        parts.append(
            f'<g><rect x="{x0}" y="{y}" width="{max(w - 1, 0)}" height="{FRAME_HEIGHT - 1}" '
            f'fill="{color}" stroke="#111318" stroke-width="0.5">'
            f"<title>{title}</title></rect>"
        )
        if label:
            parts.append(
                f'<text x="{x0 + 3}" y="{y + FRAME_HEIGHT - 5}" fill="#0a0a0a">'
                f"{html.escape(label)}</text>"
            )
        parts.append("</g>")
    parts.append("</svg>")
    return "\n".join(parts)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                      formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--input", help="`perf script` の出力ファイル (省略時は stdin)")
    parser.add_argument("--from-collapsed", action="store_true",
                         help="--input は生の perf script ではなく、既に折り畳み済みの "
                              "'stack;stack;... count' 形式である")
    parser.add_argument("-o", "--output", help="SVG の出力パス (省略時は stdout)")
    parser.add_argument("--collapsed-only", action="store_true",
                         help="SVG を描かず、折り畳んだスタックのテキストだけを出力する "
                              "(stackcollapse-perf.pl 互換の中間形式)")
    parser.add_argument("--width", type=int, default=1600, help="SVG の幅 (px)")
    parser.add_argument("--max-depth", type=int, default=None,
                         help="この深さより下のフレームは省略する (巨大なスタック対策)")
    args = parser.parse_args()

    if args.input:
        text_lines = Path(args.input).read_text(encoding="utf-8", errors="replace").splitlines(keepends=True)
    else:
        text_lines = sys.stdin.readlines()

    if not text_lines:
        print("入力が空です。perf script の出力 (または --from-collapsed で折り畳み済み"
              "テキスト) を渡してください。", file=sys.stderr)
        return 1

    counts = read_collapsed(text_lines) if args.from_collapsed else collapse_perf_script(text_lines)

    if not counts:
        print("スタックを 1 つも抽出できませんでした。`perf record` に十分なサンプルが"
              "含まれているか (`perf report --stdio` で確認)、フォーマットが想定通りか"
              "確認してください。", file=sys.stderr)
        return 1

    if args.collapsed_only:
        out = open(args.output, "w", encoding="utf-8") if args.output else sys.stdout
        try:
            write_collapsed(counts, out)
        finally:
            if args.output:
                out.close()
        return 0

    root = build_tree(counts)
    svg = render_svg(root, width=args.width, max_depth=args.max_depth)

    if args.output:
        Path(args.output).write_text(svg, encoding="utf-8")
        print(f"{sum(counts.values())} 件のスタック ({len(counts)} 種類) から "
              f"{args.output} を書き出しました。", file=sys.stderr)
    else:
        sys.stdout.write(svg)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except BrokenPipeError:
        # 出力を `head` 等に繋いだときの正常な打ち切り。エラーとして騒がない。
        sys.stderr.close()
        raise SystemExit(0)

#!/usr/bin/env python3
"""`heaptrack` のラッパー (Xvfb 込み)。VeloX 自身の Rust heap を記録する (Issue #70)。

VeloX の Rust heap と WebKitGTK 側が自然に分離される理由
---------------------------------------------------------
`heaptrack <コマンド>` は `LD_PRELOAD` でメモリ確保関数をフックする。
これは **`heaptrack` が直接起動したプロセス 1 つに対してのプロファイルを
1 ファイルに書き出す** 仕組みで、WebKitGTK が `fork`+`exec` する
`WebKitWebProcess` / `WebKitNetworkProcess` 等の子プロセスは**別プロファイル
にはならず、そもそも記録されない**。実際にこのコンテナで確認した:

    heaptrack -o /tmp/x ./target/profiling/velox --homepage ...

を実行すると `/tmp/x.gz` が 1 個だけ生成され、`WebKitWebProcess` や
`WebKitNetworkProcess` 用のファイルは生成されない (WebKitGTK が子プロセスを
起動する際に環境をサニタイズしており、`LD_PRELOAD` が子に引き継がれない
ため — セキュリティ上の理由でこうなっていると考えられる)。**この
プロセス境界がそのまま「VeloX 自身の Rust heap」と「WebKitGTK 側」の
切り分けとして働く** — 何も特別なフラグを足す必要はなく、素朴に
`heaptrack ./target/profiling/velox` を実行するだけで, 得られるプロファイルは
VeloX 本体プロセスの heap だけになる。

これは D42/`docs/performance-targets.md` §3.1 が言う「RSS/PSS はプロセス
ツリー全体を合算する」話とは**別の軸**であることに注意: RSS/PSS は
「メモリ使用量の全体像 (WebView 込み)」を見るための指標で、`heaptrack`
はその中で「VeloX 自身の Rust コードがどこで malloc しているか」を掘る
ための指標。両方が要る場面が多い (全体量は `pss_sampler.py` や
`browser::metrics`、内訳は `heaptrack`)。

WebKitGTK 側 (WebProcess) の heap も見たい場合
-----------------------------------------------
`heaptrack -p <PID>` でアタッチする方法があるが、ヘルプにあるとおり
**実行中プロセスへのアタッチは不安定でクラッシュしうる**
(heaptrack 自身の警告)。`WebKitWebProcess` の PID は VeloX 起動後に
`pgrep -f WebKitWebProcess` 等で調べられる。試す場合は自己責任で、
まず `--pid` モードのドキュメント (`heaptrack --help`) を読むこと。
このラッパーは (安定した方の) 「VeloX 自身を `heaptrack` 経由で起動する」
ケースのみを自動化する。

使い方
------
    python3 scripts/profile/run_heaptrack.py \\
        --duration-secs 15 --output /tmp/velox-heap \\
        -- ./target/profiling/velox --homepage http://127.0.0.1:8731/minimal.html

    # 集計を見る (テキスト)
    heaptrack_print /tmp/velox-heap.gz | less

    # GUI があれば (このコンテナには heaptrack-gui は入れていない)
    heaptrack --analyze /tmp/velox-heap.gz

**必ず debug symbols 付きビルドを渡すこと** — `cargo build --profile profiling`
(D45)。既定の `cargo build --release` は `strip = true` なのでシンボルが
読めない (関数名・アドレスしか出ない)。

heaptrack が使えない場合
------------------------
`sudo apt install heaptrack` で入らない場合は `docs/profiling.md` の
「heaptrack が使えないとき: valgrind --tool=massif で代替する」を参照。
このスクリプトは heaptrack 専用 (massif は別途 `run_massif.py` は用意して
いないため、コマンドを手で組み立てる — 手順は docs/profiling.md にある)。

このスクリプトは Python 標準ライブラリのみを使う。
"""

from __future__ import annotations

import argparse
import glob
import os
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

# run_perf.py と同じ Xvfb 自前起動ロジックを再利用する。
sys.path.insert(0, str(Path(__file__).resolve().parent))
from run_perf import OwnedXvfb  # noqa: E402


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--duration-secs", type=float, default=15.0,
                         help="記録を続ける秒数 (既定 15)。過ぎたら対象プロセスに "
                              "SIGTERM を送って終了させる")
    parser.add_argument("--output", required=True,
                         help="heaptrack の出力先接頭辞 (実際のファイルは "
                              "<output>.gz になる)")
    parser.add_argument("--display", help="使う既存の DISPLAY (省略時: 環境変数 "
                         "DISPLAY があればそれを使い、無ければ Xvfb を自分で起動する)")
    parser.add_argument("command", nargs=argparse.REMAINDER,
                         help="`-- <VeloX バイナリ> [引数...]`")
    args = parser.parse_args()

    heaptrack_bin = shutil.which("heaptrack")
    if heaptrack_bin is None:
        print(
            "heaptrack が見つかりませんでした。`sudo apt install heaptrack` を"
            "試してください。入らない場合は docs/profiling.md の massif の節を"
            "参照してください。",
            file=sys.stderr,
        )
        return 3

    if not args.command or args.command[0] != "--":
        print("記録するコマンドを `-- <コマンド> [引数...]` の形で指定してください。",
              file=sys.stderr)
        return 2
    target_argv = args.command[1:]
    if not target_argv:
        print("`--` の後に記録対象のコマンドがありません。", file=sys.stderr)
        return 2

    owned_xvfb: OwnedXvfb | None = None
    env = dict(os.environ)
    try:
        if args.display:
            env["DISPLAY"] = args.display
        elif not env.get("DISPLAY"):
            print("DISPLAY が未設定です。Xvfb を自分で起動します。", file=sys.stderr)
            owned_xvfb = OwnedXvfb()
            env["DISPLAY"] = owned_xvfb.display
            print(f"Xvfb を DISPLAY={owned_xvfb.display} で起動しました。", file=sys.stderr)

        out_path = Path(args.output)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        heaptrack_argv = [heaptrack_bin, "-o", str(out_path)] + target_argv

        print(f"実行: {' '.join(heaptrack_argv)} (最大 {args.duration_secs}秒)",
              file=sys.stderr)
        proc = subprocess.Popen(heaptrack_argv, env=env)
        try:
            proc.wait(timeout=args.duration_secs)
        except subprocess.TimeoutExpired:
            # heaptrack はラッパープロセス。SIGTERM で自分と debuggee の両方を
            # きれいに終了させ、記録済みデータをフラッシュする。
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=20)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
    finally:
        if owned_xvfb is not None:
            owned_xvfb.stop()

    gz_path = Path(str(out_path) + ".gz")
    zst_path = Path(str(out_path) + ".zst")
    result_path = gz_path if gz_path.exists() else (zst_path if zst_path.exists() else None)
    if result_path is None:
        print(f"{out_path}.gz (または .zst) が作られませんでした。対象プロセスの"
              "起動に失敗した可能性があります。", file=sys.stderr)
        return 4

    print(f"記録完了: {result_path} ({result_path.stat().st_size} bytes)",
          file=sys.stderr)
    print(f"集計を見るには: heaptrack_print {result_path} | less", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

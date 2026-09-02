#!/usr/bin/env python3
"""`perf record` のラッパー (Xvfb 込み)。VeloX の CPU プロファイルを 1 コマンドで取る (Issue #70)。

やること
--------
1. (`--display` 未指定かつ `DISPLAY` 未設定なら) 自前で `Xvfb` を立てる。
2. `perf record -g -e <event> -F <freq> -o <out>.data -- <コマンド...>` を、
   `--duration-secs` 秒だけ実行して SIGINT で止める (`perf` は SIGINT で
   記録を終了し、自分が起動した子プロセス — VeloX 本体 — も道連れに終了させる)。
3. `perf script` でシンボルを解決したテキストに変換 (`<out>.script`)。
4. `scripts/profile/flamegraph.py` に通して折り畳みスタック (`<out>.folded`)
   と SVG フレームグラフ (`<out>.svg`) を書き出す。

`perf` の見つけ方について (このコンテナで実際にハマった問題)
--------------------------------------------------------------
`/usr/bin/perf` はカーネルのバージョンに応じたラッパースクリプトで、
**動いているカーネルとちょうど同じバージョンの `linux-tools-<version>`
パッケージが入っていないと、警告を出して即座に (exit code 2 で) 失敗する。**
このコンテナのカーネル (`6.18.44-fc-v22` のようなカスタムビルド) に対応する
`linux-tools` パッケージは `apt` に存在しないため、`/usr/bin/perf` は常に
このパターンで失敗する。回避策として `apt install linux-tools-generic` で
入る **近いバージョンの `perf` バイナリ** (`/usr/lib/linux-tools/<version>/perf`)
を直接指定すれば動く — `perf_event` の ABI はこの程度のバージョン差では
互換であることが多く、実際にこのコンテナでは `6.8.0-138-generic` 版の
`perf` で `cpu-clock` サンプリングと `velox::app::run` 等の Rust シンボル
解決の両方が動作した (`docs/profiling.md` に検証記録がある)。このスクリプトは
`/usr/bin/perf` を試し、ダメなら `/usr/lib/linux-tools/*/perf` を機械的に
探して使う。

ハードウェアイベントについて
-----------------------------
このコンテナは (Firecracker 等の) 仮想化環境で、ハードウェア PMU が
パススルーされていない。実際に確認した症状: `perf stat -e cycles,instructions`
が `<not supported>` を返す。そのため既定のイベントは `cpu-clock`
(ソフトウェアイベント、CPU 上で実行されている時間を一定間隔でサンプリング —
ハードウェア PMU 不要) にしている。実機や PMU が使える環境では `--event
cycles` を指定するとハードウェアサイクルベースの、より正確なプロファイルが
取れる。

使い方
------
    xvfb-run 無しでそのまま (このスクリプトが Xvfb を起動する):

    python3 scripts/profile/run_perf.py \\
        --duration-secs 5 --output-prefix results/profile/cpu-startup \\
        -- ./target/profiling/velox --homepage http://127.0.0.1:8731/minimal.html

    既存の DISPLAY (既に xvfb-run の中など) を使う場合は `--display` は不要で、
    環境変数 `DISPLAY` があればそのまま使う。

**必ず debug symbols 付きビルドを渡すこと** — `cargo build --profile profiling`
(`Cargo.toml`、D45)。既定の `cargo build --release` は `strip = true` なので
シンボルが読めない。

このスクリプトは Python 標準ライブラリのみを使う。
"""

from __future__ import annotations

import argparse
import glob
import os
import shutil
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent


def find_perf() -> str | None:
    """使える `perf` バイナリのパスを探す。無ければ `None`。

    `/usr/bin/perf` (カーネルバージョン別ラッパー) を先に試し、それが
    「このカーネル用の perf が無い」で失敗する場合は
    `/usr/lib/linux-tools/*/perf` にフォールバックする (上記 docstring 参照)。
    """
    candidate = shutil.which("perf")
    if candidate:
        try:
            result = subprocess.run(
                [candidate, "--version"], capture_output=True, text=True, timeout=10
            )
            if result.returncode == 0 and result.stdout.strip():
                return candidate
        except (OSError, subprocess.TimeoutExpired):
            pass
    for path in sorted(glob.glob("/usr/lib/linux-tools/*/perf"), reverse=True):
        try:
            result = subprocess.run(
                [path, "--version"], capture_output=True, text=True, timeout=10
            )
            if result.returncode == 0 and result.stdout.strip():
                return path
        except (OSError, subprocess.TimeoutExpired):
            continue
    return None


def free_display_number() -> int:
    """使われていない X ディスプレイ番号を探す (`/tmp/.X11-unix/X<N>` が無いもの)。"""
    for n in range(99, 199):
        if not Path(f"/tmp/.X11-unix/X{n}").exists():
            return n
    raise RuntimeError("空いている X ディスプレイ番号が見つかりませんでした")


class OwnedXvfb:
    """このスクリプトが自前で起動・停止する Xvfb インスタンス。"""

    def __init__(self, screen: str = "1280x900x24"):
        xvfb_bin = shutil.which("Xvfb")
        if xvfb_bin is None:
            raise RuntimeError(
                "Xvfb が見つかりません。`sudo apt install xvfb` でインストールする"
                "か、既に DISPLAY が使えるセッションで --display を指定してください。"
            )
        self.display_num = free_display_number()
        self.display = f":{self.display_num}"
        self.proc = subprocess.Popen(
            [xvfb_bin, self.display, "-screen", "0", screen, "-nolisten", "tcp"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        deadline = time.monotonic() + 10
        sock_path = Path(f"/tmp/.X11-unix/X{self.display_num}")
        while time.monotonic() < deadline:
            if sock_path.exists():
                break
            if self.proc.poll() is not None:
                raise RuntimeError("Xvfb の起動に失敗しました")
            time.sleep(0.05)
        else:
            self.stop()
            raise RuntimeError("Xvfb の起動待ちがタイムアウトしました")
        # ソケットができてから実際に接続を受けられるまで、ごく短い猶予を置く。
        time.sleep(0.2)

    def stop(self) -> None:
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5)


def run_timed(argv: list[str], env: dict, duration_secs: float) -> int:
    """`argv` を起動し、`duration_secs` 後に SIGINT を送って正常終了させる。

    `perf record` は SIGINT を受けると記録を fsync して終了し、自分が
    `--` の後で起動した子プロセス (VeloX 本体) も終了させる — Ctrl-C で
    普通に `perf record` を止めたときと同じ経路なので、記録済みのデータは
    壊れない。戻り値は `perf` の終了コード。
    """
    proc = subprocess.Popen(argv, env=env)
    try:
        proc.wait(timeout=duration_secs)
    except subprocess.TimeoutExpired:
        proc.send_signal(signal.SIGINT)
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
    return proc.returncode if proc.returncode is not None else -1


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--duration-secs", type=float, default=8.0,
                         help="記録する秒数 (既定 8)。起動プロファイルなら "
                              "起動〜初回ロードが収まる長さにすること")
    parser.add_argument("--event", default="cpu-clock",
                         help="perf のイベント (既定 cpu-clock — ハードウェア PMU "
                              "の無い環境向け。実機では 'cycles' も使える)")
    parser.add_argument("--freq", type=int, default=999,
                         help="サンプリング周波数 Hz (既定 999)")
    parser.add_argument("--output-prefix", required=True,
                         help="出力ファイルの接頭辞。<prefix>.data / .script / "
                              ".folded / .svg を書き出す")
    parser.add_argument("--display", help="使う既存の DISPLAY (省略時: 環境変数 "
                         "DISPLAY があればそれを使い、無ければ Xvfb を自分で起動する)")
    parser.add_argument("--perf-bin", help="使う perf バイナリを明示的に指定 "
                         "(省略時は自動検出)")
    parser.add_argument("--no-flamegraph", action="store_true",
                         help="perf script / 折り畳み / SVG 生成を省略し、"
                              "<prefix>.data の記録だけ行う")
    parser.add_argument("command", nargs=argparse.REMAINDER,
                         help="`-- <VeloX バイナリ> [引数...]`")
    args = parser.parse_args()

    if not args.command or args.command[0] != "--":
        print("記録するコマンドを `-- <コマンド> [引数...]` の形で指定してください。",
              file=sys.stderr)
        return 2
    target_argv = args.command[1:]
    if not target_argv:
        print("`--` の後に記録対象のコマンドがありません。", file=sys.stderr)
        return 2

    perf_bin = args.perf_bin or find_perf()
    if perf_bin is None:
        print(
            "perf バイナリが見つかりませんでした。\n"
            "  sudo apt install linux-tools-generic linux-tools-common\n"
            "を試してください。それでも見つからない場合、このカーネル向けの\n"
            "linux-tools パッケージが存在しない可能性があります —\n"
            "docs/profiling.md の「perf が見つからない/使えないとき」を参照してください。",
            file=sys.stderr,
        )
        return 3

    out_prefix = Path(args.output_prefix)
    out_prefix.parent.mkdir(parents=True, exist_ok=True)
    data_path = out_prefix.with_suffix(out_prefix.suffix + ".data") if out_prefix.suffix else Path(str(out_prefix) + ".data")

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

        perf_argv = [
            perf_bin, "record", "-g",
            "-e", args.event,
            "-F", str(args.freq),
            "-o", str(data_path),
            "--",
        ] + target_argv

        print(f"実行: {' '.join(perf_argv)} (最大 {args.duration_secs}秒)", file=sys.stderr)
        rc = run_timed(perf_argv, env, args.duration_secs)
        if rc not in (0, None) and not data_path.exists():
            print(f"perf record が失敗しました (exit={rc})。", file=sys.stderr)
            return 4
    finally:
        if owned_xvfb is not None:
            owned_xvfb.stop()

    if not data_path.exists() or data_path.stat().st_size == 0:
        print(f"{data_path} が空か作られませんでした。記録対象のプロセスが "
              "起動できなかった可能性があります (WebKitGTK の依存関係、"
              "DISPLAY 等を確認してください)。", file=sys.stderr)
        return 5

    print(f"記録完了: {data_path} ({data_path.stat().st_size} bytes)", file=sys.stderr)

    if args.no_flamegraph:
        return 0

    script_path = Path(str(out_prefix) + ".script")
    folded_path = Path(str(out_prefix) + ".folded")
    svg_path = Path(str(out_prefix) + ".svg")

    with script_path.open("w", encoding="utf-8") as f:
        result = subprocess.run(
            [perf_bin, "script", "-i", str(data_path)],
            stdout=f, stderr=subprocess.PIPE, text=True,
        )
    if result.returncode != 0:
        print(f"perf script に失敗しました: {result.stderr}", file=sys.stderr)
        return 6
    if script_path.stat().st_size == 0:
        print(f"{data_path} にサンプルがありませんでした (記録時間が短すぎた、"
              "または対象プロセスがほぼアイドルだった可能性)。"
              "--duration-secs を増やすか、記録中に実際に負荷のかかる操作 "
              "(ページ読み込み・タブ切り替え等) を行ってください。", file=sys.stderr)
        return 7

    flamegraph_script = SCRIPT_DIR / "flamegraph.py"
    with folded_path.open("w", encoding="utf-8") as f:
        subprocess.run(
            [sys.executable, str(flamegraph_script), "--input", str(script_path),
             "--collapsed-only"],
            stdout=f, check=False,
        )
    subprocess.run(
        [sys.executable, str(flamegraph_script), "--input", str(script_path),
         "-o", str(svg_path)],
        check=False,
    )

    print(f"folded: {folded_path}", file=sys.stderr)
    print(f"flamegraph: {svg_path}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

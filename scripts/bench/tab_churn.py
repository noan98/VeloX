#!/usr/bin/env python3
"""タブの開閉を大量に繰り返したときの PSS の増え方 (Issue #62)。

何を測るか
----------
`scripts/bench/tab_scaling.py` (#61) は「タブを N 個開いたまま保持したときの
定常状態の PSS」を測る。本スクリプトが測るのはそれとは別の軸で、**タブ数は
毎回 1 個 (ホームページのみ) に戻しているのに、開閉を繰り返すだけで PSS が
じわじわ増えていかないか** — 長時間使い続けたときの retention/leak の
シグネチャそのものである。

やり方: VeloX を 1 プロセスだけ起動し、`browser::automation` と同じ形式の
自動操作スクリプトで **「K 個のタブを開く → 落ち着かせる → K 個とも閉じて
1 タブに戻す → 落ち着かせる」を 1 ラウンドとして R 回繰り返す**。各ラウンドの
「閉じ終わった直後 (=常に同じタブ数 1 に戻った瞬間)」に
`scripts/bench/tab_scaling.py` と同じ `/proc` の読み方でプロセスツリー全体の
PSS/RSS を 1 回採る。同一タブ数のはずの点が右肩上がりなら、それは
「タブ数がそのままなのに増えている」= 何かが解放されずに残っている、という
直接の証拠になる。

VeloX は自動操作スクリプトを起動時に一括で読み込み、`wait <ms>` は
そのミリ秒だけ実スリープしてから次のコマンドに進む
(`browser::automation::AutomationCommand::Wait`)。したがってラウンド境界の
実時刻はスクリプトに書いた wait の合計から高い精度で計算できる —
本スクリプトはその予測時刻まで外側から `time.sleep` した上でスナップショット
を撮る。ラウンドの途中を誤って掴まないよう、各 `wait` には十分な余裕
(既定 300ms/150ms + ラウンド末尾 800ms) を持たせている。

**「1 時間以上の連続利用」の扱いについて (Issue #62 の受け入れ条件)。** CI や
この検証環境で実際に 1 時間回すのは非現実的なので、本スクリプトは既定で
数十ラウンドという短い反復回数だけを実際に流し、そこから得られた
「ラウンドあたりの増分 (MiB/round)」を実測値として報告した上で、
`--extrapolate-minutes` (既定 60) 分の連続利用に外挿した推定値を別途表示する
— 外挿はあくまで推定であり実測ではないことを出力に明記する。

使い方
------
    xvfb-run -a --server-args="-screen 0 1280x900x24" dbus-run-session -- \\
      python3 scripts/bench/tab_churn.py \\
        --velox ./target/release/velox \\
        --page minimal.html --rounds 20 --tabs-per-round 8 \\
        --output results/tab-churn.json

出力
----
標準出力にラウンドごとの表 (round, pss_mib, rss_mib, process_count) と、
最小二乗法による傾き (MiB/round, MiB/tab-open) および 1 時間換算の外挿値。
`--output` を指定すると同じ内容を JSON でも書き出す。

このスクリプトは Python 標準ライブラリのみを使う。
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from proctree import process_tree_memory as proctree_memory  # noqa: E402

PAGES_DIR = Path(__file__).resolve().parent / "pages"


def process_tree_memory(root_pid: int) -> tuple[int, int | None, int]:
    """`root_pid` を根とするツリーの (RSS 合計, PSS 合計 or None, プロセス数)。

    実装は `proctree.py` に集約した (Issue #197)。**同じ `/proc` 走査が 3 つの
    スクリプトに重複していた**ため、Windows 対応を足すにあたって 1 つにまとめた。
    このスクリプト自身は Linux 専用のままなので、戻り値の形は変えていない。
    """
    mem = proctree_memory(root_pid)
    return mem.rss_bytes, mem.pss_bytes, mem.process_count


def build_script(
    url: str,
    rounds: int,
    tabs_per_round: int,
    settle_per_open_ms: int,
    settle_per_close_ms: int,
    round_settle_ms: int,
) -> tuple[str, list[float]]:
    """自動操作スクリプトのテキストと、各ラウンドが閉じ終わって落ち着いた
    はずの「起動からの経過秒数」のリストを返す。

    1 タブ (ホームページ) から始まり、各ラウンドは
    `open ×tabs_per_round` (index 1..tabs_per_round が新規タブ) →
    `close ×tabs_per_round` (index を降順に、index 1 まで) → ラウンド末の
    settle という構成。降順で閉じるのは、`close <index>` が 0-based の
    その時点のタブ strip 位置を指すため (`browser::automation`) —
    昇順だと 2 個目を閉じた時点で以降の index がずれる。
    """
    lines: list[str] = []
    boundaries: list[float] = []
    elapsed_ms = 0.0
    for _ in range(rounds):
        for _ in range(tabs_per_round):
            lines.append(f"open {url}")
            lines.append(f"wait {settle_per_open_ms}")
            elapsed_ms += settle_per_open_ms
        for index in range(tabs_per_round, 0, -1):
            lines.append(f"close {index}")
            lines.append(f"wait {settle_per_close_ms}")
            elapsed_ms += settle_per_close_ms
        lines.append(f"wait {round_settle_ms}")
        elapsed_ms += round_settle_ms
        boundaries.append(elapsed_ms / 1000.0)
    # 最後のスナップショットの後、python 側が kill する前に quit が先に
    # 実行されてしまわないよう長めの余裕を持たせる。
    lines.append("wait 30000")
    lines.append("quit")
    return "\n".join(lines) + "\n", boundaries


def least_squares_slope(xs: list[float], ys: list[float]) -> tuple[float, float]:
    """単純な最小二乗法で (傾き, 切片) を返す。numpy 不使用 (標準ライブラリ方針)。"""
    n = len(xs)
    mean_x = sum(xs) / n
    mean_y = sum(ys) / n
    num = sum((x - mean_x) * (y - mean_y) for x, y in zip(xs, ys))
    den = sum((x - mean_x) ** 2 for x in xs)
    if den == 0:
        return 0.0, mean_y
    slope = num / den
    intercept = mean_y - slope * mean_x
    return slope, intercept


def run_churn(
    binary: str,
    url: str,
    data_dir: Path,
    rounds: int,
    tabs_per_round: int,
    settle_per_open_ms: int,
    settle_per_close_ms: int,
    round_settle_ms: int,
    timeout: float,
    extra_env: dict[str, str] | None = None,
) -> list[dict]:
    """1 プロセスの VeloX を起動し、各ラウンド境界で PSS/RSS を採る。

    戻り値は `[{"round": int, "elapsed_s": float, "pss_bytes": int|None,
    "rss_bytes": int, "process_count": int}, ...]`。
    """
    script_text, boundaries = build_script(
        url, rounds, tabs_per_round, settle_per_open_ms, settle_per_close_ms, round_settle_ms
    )
    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as f:
        f.write(script_text)
        script_path = f.name

    env = {
        **os.environ,
        "VELOX_DATA_DIR": str(data_dir),
        "VELOX_AUTOMATION_SCRIPT": script_path,
        "VELOX_HOMEPAGE": url,
        **(extra_env or {}),
    }
    rows: list[dict] = []
    start = time.monotonic()
    proc = subprocess.Popen([binary], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = start + timeout
        for round_index, boundary in enumerate(boundaries, start=1):
            target = start + boundary
            sleep_for = max(0.0, min(target, deadline) - time.monotonic())
            time.sleep(sleep_for)
            if proc.poll() is not None:
                print(
                    f"  round {round_index}: velox が既に終了していました "
                    f"(exit={proc.returncode})。それ以降のラウンドは計測できません。",
                    file=sys.stderr,
                )
                break
            rss, pss, count = process_tree_memory(proc.pid)
            rows.append(
                {
                    "round": round_index,
                    "elapsed_s": time.monotonic() - start,
                    "rss_bytes": rss,
                    "pss_bytes": pss,
                    "process_count": count,
                }
            )
            pss_s = f"{pss / 1024 / 1024:.1f}" if pss is not None else "n/a"
            print(
                f"  round {round_index:>3}/{rounds}: pss={pss_s:>7}MiB "
                f"rss={rss / 1024 / 1024:>7.1f}MiB procs={count}"
            )
    finally:
        os.unlink(script_path)
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=10)
    return rows


def summarize(rows: list[dict], extrapolate_minutes: float, tabs_per_round: int) -> dict:
    pss_rows = [r for r in rows if r["pss_bytes"] is not None]
    if len(pss_rows) < 2:
        return {"ok": False, "reason": "PSS を読めたラウンドが2点未満で、傾きを計算できません"}

    xs = [float(r["round"]) for r in pss_rows]
    ys_mib = [r["pss_bytes"] / 1024 / 1024 for r in pss_rows]
    slope_mib_per_round, intercept = least_squares_slope(xs, ys_mib)
    slope_mib_per_open = slope_mib_per_round / tabs_per_round if tabs_per_round else 0.0

    # 実測した範囲の平均ラウンド所要時間から、外挿対象の分数に何ラウンド
    # 相当するかを見積もる。
    elapsed_span = rows[-1]["elapsed_s"] - (rows[0]["elapsed_s"] - (rows[0]["elapsed_s"] / rows[0]["round"]))
    avg_round_s = rows[-1]["elapsed_s"] / rows[-1]["round"] if rows[-1]["round"] else 0.0
    rounds_in_window = (extrapolate_minutes * 60.0) / avg_round_s if avg_round_s > 0 else 0.0
    extrapolated_growth_mib = slope_mib_per_round * rounds_in_window

    first_pss_mib = ys_mib[0]
    last_pss_mib = ys_mib[-1]

    return {
        "ok": True,
        "measured_rounds": len(pss_rows),
        "measured_tabs_opened_total": len(pss_rows) * tabs_per_round,
        "measured_wall_time_s": rows[-1]["elapsed_s"],
        "first_round_pss_mib": first_pss_mib,
        "last_round_pss_mib": last_pss_mib,
        "raw_delta_first_to_last_mib": last_pss_mib - first_pss_mib,
        "slope_mib_per_round_least_squares": slope_mib_per_round,
        "slope_mib_per_tab_open_close": slope_mib_per_open,
        "intercept_mib": intercept,
        "avg_round_wall_time_s": avg_round_s,
        "extrapolate_minutes": extrapolate_minutes,
        "extrapolated_rounds_in_window": rounds_in_window,
        "extrapolated_growth_mib": extrapolated_growth_mib,
    }


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--velox", required=True)
    parser.add_argument("--page", default="minimal.html")
    parser.add_argument("--rounds", type=int, default=20,
                         help="開閉を繰り返す回数 (既定 20)")
    parser.add_argument("--tabs-per-round", type=int, default=8,
                         help="1 ラウンドで開いて閉じるタブ数 (既定 8)")
    parser.add_argument("--settle-per-open-ms", type=int, default=300)
    parser.add_argument("--settle-per-close-ms", type=int, default=150)
    parser.add_argument("--round-settle-ms", type=int, default=800,
                         help="ラウンド末尾、全タブを閉じ終えてからスナップショットまでの追加待ち")
    parser.add_argument("--extrapolate-minutes", type=float, default=60.0,
                         help="外挿する連続利用時間 (分、既定 60 = 受け入れ条件の1時間)")
    parser.add_argument("--timeout", type=float, default=600.0)
    parser.add_argument("--output")
    args = parser.parse_args()

    if not os.environ.get("DISPLAY"):
        print("DISPLAY が未設定です。xvfb-run 経由で実行してください。", file=sys.stderr)
        return 2

    page_path = PAGES_DIR / args.page
    if not page_path.exists():
        print(f"ページが見つかりません: {page_path}", file=sys.stderr)
        return 2
    url = f"file://{page_path}"

    workdir = Path(tempfile.mkdtemp(prefix="velox-tabchurn-"))
    data_dir = workdir / "velox-data"
    data_dir.mkdir(parents=True, exist_ok=True)

    print(
        f"tab_churn: rounds={args.rounds} tabs_per_round={args.tabs_per_round} "
        f"(のべ {args.rounds * args.tabs_per_round} 回の open/close)"
    )
    rows = run_churn(
        args.velox, url, data_dir,
        args.rounds, args.tabs_per_round,
        args.settle_per_open_ms, args.settle_per_close_ms, args.round_settle_ms,
        args.timeout,
    )
    workdir_cleanup_failed = False
    try:
        import shutil
        shutil.rmtree(workdir, ignore_errors=True)
    except Exception:
        workdir_cleanup_failed = True

    if not rows:
        print("計測できたラウンドがありませんでした。", file=sys.stderr)
        return 1

    summary = summarize(rows, args.extrapolate_minutes, args.tabs_per_round)

    print(f"\n{'round':>6}{'pss(MiB)':>12}{'rss(MiB)':>12}{'procs':>8}{'elapsed(s)':>12}")
    for r in rows:
        pss_s = f"{r['pss_bytes'] / 1024 / 1024:.1f}" if r["pss_bytes"] is not None else "n/a"
        print(f"{r['round']:>6}{pss_s:>12}{r['rss_bytes'] / 1024 / 1024:>12.1f}"
              f"{r['process_count']:>8}{r['elapsed_s']:>12.1f}")

    if summary.get("ok"):
        print(
            f"\n実測 {summary['measured_rounds']} ラウンド "
            f"({summary['measured_tabs_opened_total']} 回の open/close、"
            f"所要 {summary['measured_wall_time_s']:.1f} 秒) の結果:\n"
            f"  最初のラウンド: {summary['first_round_pss_mib']:.1f} MiB\n"
            f"  最後のラウンド: {summary['last_round_pss_mib']:.1f} MiB\n"
            f"  最小二乗法の傾き: {summary['slope_mib_per_round_least_squares']:+.3f} "
            f"MiB/round ({summary['slope_mib_per_tab_open_close']:+.4f} MiB/open-close)\n"
            f"\n"
            f"  [外挿・実測ではない] 実測区間の平均ラウンド所要時間 "
            f"({summary['avg_round_wall_time_s']:.2f} 秒/round) から "
            f"{summary['extrapolate_minutes']:.0f} 分の連続利用は約 "
            f"{summary['extrapolated_rounds_in_window']:.0f} round 相当と仮定すると、\n"
            f"  推定増分 = {summary['extrapolated_growth_mib']:+.1f} MiB "
            f"(この推定は短い実測区間の傾きを線形に伸ばしただけで、実際に"
            f"{summary['extrapolate_minutes']:.0f}分間流して確認した値ではない)"
        )
    else:
        print(f"\n傾きを計算できませんでした: {summary.get('reason')}")

    if args.output:
        out = Path(args.output)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(
            json.dumps(
                {
                    "args": vars(args),
                    "rows": rows,
                    "summary": summary,
                },
                indent=2,
                ensure_ascii=False,
            ),
            encoding="utf-8",
        )
        print(f"\n結果を {out} に保存しました。")

    if workdir_cleanup_failed:
        print(f"警告: 作業ディレクトリ {workdir} の削除に失敗しました。", file=sys.stderr)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())

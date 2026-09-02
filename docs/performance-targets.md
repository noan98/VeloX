# VeloX 性能目標と競合ベンチマーク条件

Issue #58 / Epic #57。**Phase 3 のすべての最適化はこの文書の条件と目標値を基準に
評価する。** ここが決まっていない状態での最適化は、Epic #57 の絶対ルール
「ベンチマークなしの最適化をしない」に違反する。

---

## 1. 測定環境 (固定)

比較は**必ず同一マシン・同一セッション内**で行う。異なる日・異なるマシンで取った
数値を並べて比較しない。

| 項目 | 値 |
| --- | --- |
| OS | Ubuntu 24.04.4 LTS |
| カーネル | 6.18.44 |
| CPU | Intel Xeon @ 2.80GHz / 4 コア |
| メモリ | 15 GiB |
| ディスプレイ | Xvfb `-screen 0 1280x900x24` |
| GPU | **なし** (ソフトウェアレンダリング) |
| ネットワーク | 使わない (loopback 上の固定ページのみ) |
| WebKitGTK | 2.52.6 |
| rustc | 1.94.1 |
| VeloX ビルド | `cargo build --release` |

> **この環境の重大な制約**: GPU が無いため、両ブラウザともソフトウェア
> レンダリングにフォールバックする (`libEGL warning: DRI3 error`)。**メモリと
> 描画に関わる数値は実機と乖離する。** 同一環境での相対比較としてのみ読むこと。
> 実機での再測定は #70 (CPU / Memory Profiling Workflow) の課題とする。

## 2. 比較対象

| ブラウザ | バージョン | エンジン | 備考 |
| --- | --- | --- | --- |
| VeloX | `88c9356` | WebKitGTK 2.52.6 | システム WebView (wry 0.56) |
| Chromium | 141.0.7390.37 | Blink | Playwright 同梱版 |

**Firefox / Safari / Edge は未測定。** この環境に無く、追加もしていない。Safari は
Linux に存在せず、Edge は実質 Chromium と同一エンジンなので、**次に足す価値が
高いのは Firefox (Gecko)** である。

> **重要**: VeloX はシステム WebView を使うため、この比較は「VeloX 対 Chromium」で
> あると同時に「**WebKitGTK 対 Blink**」でもある。**VeloX 自身のオーバーヘッドと
> エンジン差は分離できていない。** Epic #57 の「WebView をブラックボックスとして
> 扱う」という原則どおり、VeloX が改善できるのは WebView の周囲だけである。

## 3. メトリクス定義

### 3.1 競合比較に使うもの (`scripts/bench/compare_browsers.py`)

ブラウザの内部 API に依存しない外形指標のみを使う。内部イベントは VeloX にしか
無く、比較に使えない。

| メトリクス | 定義 |
| --- | --- |
| `startup_to_load_ms` | プロセス spawn の瞬間から、**ページ自身の `load` イベント**が発火するまでの実時間。ページに注入した beacon が loopback の HTTP サーバを叩き、その到着時刻で測る |
| `pss_bytes` | load から `--settle-secs` 秒後の、プロセスツリー全体の **PSS** 合計 |
| `rss_bytes` | 同時点の RSS 合計 (**比較には使わない**。下記参照) |
| `process_count` | 同時点のプロセス数 |

> ### ⚠️ メモリ比較には PSS を使うこと
>
> **RSS 合計は共有メモリをプロセスごとに丸ごと数えるため、プロセス数の多い
> ブラウザほど大きく出る。** この環境では VeloX が 5 プロセス、Chromium が 9
> プロセスなので、RSS 合計で比較すると VeloX が不当に有利になる。実際、同じ
> 測定で結論が逆転する (§4)。
>
> **これは VeloX 自身の計測にも影響する。** `browser::metrics::sample_process_tree_rss`
> (D16) は RSS 合計を採るため、**VeloX のメモリ優位を過大評価する。** #61 / #63 が
> この値を改善の指標に使う前に PSS を足すべき — フォローアップは #108。

### 3.2 VeloX 内部の計測 (`velox-bench`)

`docs/benchmarking.md` を参照。`startup_window_created_ms` / `startup_toolbar_ready_ms` /
`startup_first_load_ms` / `page_load_ms` / `rss_*`。これらは VeloX の内部を分解する
ためのもので、**競合比較には使えない**。

## 4. 初回 baseline (2026-09-02)

`xvfb-run -a --server-args="-screen 0 1280x900x24" python3 scripts/bench/compare_browsers.py`
を各ページで実行した中央値。

### minimal.html (7 試行)

| ブラウザ | load 到達 (ms) | PSS (MiB) | RSS 合計 (MiB) | プロセス数 |
| --- | ---: | ---: | ---: | ---: |
| **VeloX** | **435.6** | 423.6 | 775.2 | 5 |
| Chromium | 561.4 | **328.9** | 854.8 | 9 |

### dom_heavy.html (5 試行)

| ブラウザ | load 到達 (ms) | PSS (MiB) | RSS 合計 (MiB) | プロセス数 |
| --- | ---: | ---: | ---: | ---: |
| **VeloX** | **491.4** | 467.4 | 825.4 | 5 |
| Chromium | 723.9 | **364.6** | 895.5 | 9 |

### VeloX 内部の startup 内訳 (`velox-bench`, cold_startup, minimal.html)

| メトリクス | median (ms) |
| --- | ---: |
| `startup_window_created_ms` | 224.4 |
| `startup_toolbar_ready_ms` | 528.4 |
| `startup_first_load_ms` | 665.3 |
| `page_load_ms` | 77.5 |

## 5. 勝っている領域 / 負けている領域

### ✅ 勝っている: 起動から最初のページ表示まで

**VeloX は Chromium より 22〜32% 速い** (minimal 435.6ms vs 561.4ms、dom_heavy
491.4ms vs 723.9ms)。ページが重くなるほど差が広がっており、7 試行/5 試行で一貫
している。プロセス数が 5 対 9 と少ないことも起動コストに効いていると考えられる。

### ❌ 負けている: メモリ

**PSS で見ると VeloX は Chromium より 28〜29% 多い** (minimal 423.6 vs 328.9 MiB、
dom_heavy 467.4 vs 364.6 MiB)。

**これは VeloX の掲げる「低メモリ」という位置付けと矛盾する。** RSS 合計で見ると
逆に見えるが、それは §3.1 のとおり測り方の問題であり、実態ではない。

Phase 3 のメモリ最適化 (#61 / #62 / #63) は「Chromium より軽い」を出発点にできない。
**まず「なぜ WebKitGTK ベースの VeloX が Blink より PSS で重いのか」を切り分ける
必要がある** — VeloX 側のオーバーヘッドなのか、WebKitGTK と Blink の差なのか。
後者なら Epic #57 の原則上 VeloX には手が出せない領域になる。

### 未測定

タブ生成/切替、複数タブ時のメモリ、バックグラウンド CPU、ページロードの内訳
(DNS/TLS/レンダリング)、バッテリー/アイドル消費。これらは自動駆動の仕組みが
まだ無い (`Scenario::is_unattended()` が `false`)。#106 で起動 URL の指定までは
入ったが、「N タブ開く」「起動後に遷移する」フックは未実装。

## 6. VeloX の性能目標

上記 baseline を踏まえた Phase 3 の目標。**すべて同一環境・同一ページでの中央値**で
評価する。

| # | 目標 | 現在値 | 目標値 | 根拠 |
| --- | --- | ---: | ---: | --- |
| T1 | 起動〜load の優位を**維持**する | Chromium 比 -22〜-32% | **Chromium 比 -20% 以内を維持** | 既に勝っている領域を最適化で失わないことが最優先。回帰ゲート (#72) の対象 |
| T2 | メモリ (PSS) で Chromium と**同等**まで詰める | Chromium 比 +28〜29% | **Chromium 比 +10% 以内** | 「低メモリ」を名乗る最低条件。まず #61/#62 で VeloX 側の寄与を特定する |
| T3 | `startup_toolbar_ready_ms` を短縮する | 528.4ms | **300ms 以下** | 内部内訳で window_created (224ms) からツールバー ready まで 300ms かかっている。ここは VeloX 自身のコードであり、エンジン差ではない = **確実に手が出せる** |
| T4 | 20 タブ時に操作不能な遅延を出さない | 未測定 | タブ切替 median **100ms 以下** | #60 の受け入れ条件。まず計測手段が必要 |

**T3 が Phase 3 で最初に着手すべき項目**である。エンジン差の影響を受けず、VeloX の
コードだけで改善でき、かつ内訳上いちばん大きい (起動時間の約半分)。

## 7. baseline の保存形式

- 競合比較: `scripts/bench/compare_browsers.py --output <path>` が出力する JSON
- VeloX 内部: `velox-bench` の `BenchmarkResult` JSON (`docs/benchmarking.md` §出力形式)

いずれも OS・CPU コア数・試行回数・実行日時を含む。**git commit と対応付けて
保存すること** — どのコードの数値かが分からない baseline は比較に使えない。

## 8. 再現手順

```sh
# 依存 (Debian/Ubuntu)
sudo apt install -y libwebkit2gtk-4.1-dev xvfb

cargo build --release

# 競合比較
xvfb-run -a --server-args="-screen 0 1280x900x24" \
  python3 scripts/bench/compare_browsers.py \
    --velox ./target/release/velox \
    --chromium /path/to/chromium \
    --page minimal.html --trials 7 \
    --output results/compare-minimal.json

# VeloX 内部の内訳
(cd scripts/bench/pages && python3 -m http.server 8731 &)
xvfb-run -a --server-args="-screen 0 1280x900x24" \
  ./target/release/velox-bench run \
    --scenario cold_startup --trials 10 \
    --url http://127.0.0.1:8731/minimal.html \
    --output results/cold_startup.json
```

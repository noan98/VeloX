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
> **これは VeloX 自身の計測にも影響していた。** `browser::metrics::sample_process_tree_rss`
> (D16) はもともと RSS 合計のみを採っており、**VeloX のメモリ優位を過大評価
> していた。** #108 (D42) でこの関数自体に PSS 合計 (`total_pss_bytes`) と
> `pss_process_count` (読めたプロセス数) を追加済み。`velox-bench` の
> `MetricKey` にも `pss_total_bytes` / `pss_process_count` が追加されている
> ので、**#61 / #62 / #63 のメモリ最適化は `rss_total_bytes` ではなく
> `pss_total_bytes` を改善の指標に使うこと。** `smaps_rollup` が読めない環境
> (古いカーネル・権限不足・非 Linux) では `total_pss_bytes` は `null`
> (欠損として扱われ `velox-bench` の集計からは丸ごと省かれる) になり、RSS 側
> は従来どおり必ず取得できる。詳細は `docs/decisions.md` D42、フィールドの
> 意味は `docs/benchmarking.md` の対応表を参照。

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

> **⚠️ 2026-09-03 (#61) の実測で切り分けが完了した。詳細は
> [docs/memory-analysis.md](memory-analysis.md) を参照。**
>
> 「VeloX 側のオーバーヘッドなのか、WebKitGTK と Blink の差なのか」という
> 上記の問いへの答え: **主因は WebKitGTK と Blink のエンジン差ではなく、
> VeloX 自身の実装 (toolbar を独立 webview にしている設計 + webview を
> 作るたびに独立した `WebContext` を作っている wry の呼び出し方) だった。**
> 単一 webview だけの最小 WebKitGTK アプリを作って計測すると PSS は約
> 296〜299 MiB で、これは Chromium (318〜328 MiB) より**軽い** — WebKitGTK
> というエンジン自体が重いという証拠はこの環境では見つからなかった。VeloX
> が実際に重いのは、1 タブでも toolbar 用 + content 用の 2 つの webview
> (=2 組の `WebKitWebProcess`+`WebKitNetworkProcess`) を同時に持っている
> ためで、この超過分だけで 1 タブ時の Chromium 超過分のほぼ全部を説明できる。
> さらにタブが増えるたびに VeloX は正確に +2 プロセスするのに対し
> Chromium は概ね +1 プロセスで済んでおり (タブごとに独立した `WebContext`
> を作っているため)、20 タブでは VeloX は Chromium の約 5.2 倍の PSS になる。
> VeloX 自身の Rust heap (heaptrack 実測 32.08 MiB、タブ数やページの重さに
> 依らず一定) はツリー全体 PSS の 1〜8% 程度に過ぎず、削っても全体にはほぼ
> 効かないことも確認済み。**Epic #57 の「エンジンをブラックボックスとして
> 扱う」原則には反しない** (WebKit 内部ではなく wry への webview の作り方の
> 話) が、**未実装・未検証の仮説**であることに変わりはない — 実装・計測は
> #62 に引き継ぐ (`docs/decisions.md` D48)。

Phase 3 のメモリ最適化 (#61 / #62 / #63) は「Chromium より軽い」を出発点にできない。
上記のとおり、原因の切り分け自体は #61 で完了した。

> **⚠️ 2026-09-03 (#118) で toolbar/タブ間の `WebContext` 共有を実装・計測
> した。詳細は [docs/memory-analysis.md](memory-analysis.md) §9、
> `docs/decisions.md` D49 を参照。**
>
> 結論: **効果は実測できる程度に有意だが、部分的。** `WebKitNetworkProcess`
> はタブ数に関わらず 1 個に統合できた (タブ 1 個あたりの増分プロセス数が
> +2→+1 に、Chromium と同じ増分パターンになった) が、PSS の大半を占める
> `WebKitWebProcess` は webview ごとに独立したまま統合されなかった
> (WebKitGTK の `WebContext` は `NetworkProcess` の生成単位ではあるが
> `WebProcess` の生成単位ではないと判明)。結果として PSS は 1/5/10/20 タブ
> で -2.5%〜-11.0% 減少したが、**T2 (Chromium 比 +10% 以内) には遠く届いて
> いない** (1 タブで +45.5%、20 タブで +402.2%、変更前はそれぞれ +48.6%/
> +428.1%)。プライベートモードは D15/D48 の予測どおり wry の制約で共有が
> 効かず、変更前と同一の挙動 (webview ごとに独立した `WebProcess`+
> `NetworkProcess`) のまま。**効果がゼロではなく measurable なため実装は
> 残したが、T2 達成には別の手段 (`WebProcess` 自体の共有) が必要。**

> **⚠️ 2026-09-04 (#124) でその「別の手段」— `with_related_view` による
> タブ間の `WebKitWebProcess` 共有 — を実装・計測した。詳細は
> [docs/memory-analysis.md](memory-analysis.md) §10、`docs/decisions.md`
> D54 を参照。**
>
> 結論: **効果は大きいが T2 には届かない。** content タブの `WebProcess`
> を最大 4 タブごとに 1 つへ統合し (読み込み中のタブがいるプロセスには
> 相乗りしない)、PSS は 5/10/20 タブで -16.8% / -22.6% / -25.6%、1 タブ
> あたりの増分は 92.4 → 63.3 MiB/タブ。Chromium 比は 20 タブで +365.0% →
> +246.2%。1 タブ時は共有相手が無いため +45% のまま。全タブを 1 プロセス
> に乗せる案は burst オープン時のページロードが直列化 (`tab_switch` の
> `page_load_ms` +638%) して Epic #57 ルール 4 に抵触したため不採用。
> 残りの超過 (同一プロセス内でもページ 1 枚あたり 54 MiB、Chromium の
> 5.6 倍) の切り分けは #63 (Adaptive Tab Suspension) に引き継ぐ。

### タブ数に対する増え方 (2026-09-03、#61 で測定 / #118 で before/after 追記 / #124 で再測定)

1/5/10/20 タブで `minimal.html` を計測 (`scripts/bench/tab_scaling.py`、
各 3 試行の中央値、詳細は `docs/memory-analysis.md` §4/§9)。

| タブ数 | before (#61時点) VeloX PSS (MiB) | after (#118: WebContext共有後) VeloX PSS (MiB) | after (#124: WebProcess共有後) VeloX PSS (MiB) | Chromium PSS (MiB) |
| ---: | ---: | ---: | ---: | ---: |
| 1  | 409.6  | 408.5  | 409.0  | 276.7 |
| 5  | 777.7  | 790.9  | 655.7  | 315.6 |
| 10 | 1387.0 | 1234.5 | 989.5  | 366.0 |
| 20 | 2421.9 | 2322.5 | 1612.3 | 464.8 |

（#124 列は 2026-09-04 の別セッションの計測。同一セッション内の before/after
比較は `docs/memory-analysis.md` §10.2 を参照 — before 409.2/787.9/1277.7/
2165.8 MiB → after 409.0/655.7/989.5/1612.3 MiB、同時測定した Chromium は
281.2/320.4/367.3/465.8 MiB。）

（#61 と #118 は別セッションの計測のため、上表の Chromium 列は #61 時点の
参考値。#118 は同一セッション内の before/after 比較を別途行っており、その
数値は `docs/memory-analysis.md` §9.2 を参照— before 419.0/834.8/1386.4/
2456.6 MiB → after 408.5/790.9/1234.5/2322.5 MiB、-2.5%〜-11.0%。）

1 タブあたりの増分は before 約 107 MiB/タブ → after 約 101 MiB/タブ
(#118、同一セッション比較)。Chromium は約 9.9 MiB/タブ。原因はプロセス数の
増え方 (before: VeloX +2/タブ → after: VeloX +1/タブ。Chromium は +1/タブ
程度) と一致する — `WebKitNetworkProcess` の統合分だけ増分が縮んだ
(`docs/memory-analysis.md` §9.3)。

### 未測定

バックグラウンド CPU、ページロードの内訳 (DNS/TLS/レンダリング)、バッテリー/
アイドル消費。タブ生成/切替のレイテンシと複数タブ時のメモリは #61/#112 で
測定可能になった (`velox-bench run --scenario tab_create|tab_switch|tabs_N`、
`scripts/bench/tab_scaling.py`) — メモリ側は上表のとおり測定済み。ただし
`velox-bench run --scenario tabs_N` 自体の `pss_total_bytes`/`rss_total_bytes`
は既定の RSS サンプリング間隔 (5000ms) がシナリオの所要時間より長いことが
多く、タブ数に対する増え方の指標としては現状使えない
(`docs/memory-analysis.md` §4.1、`docs/decisions.md` D48)。

## 6. VeloX の性能目標

上記 baseline を踏まえた Phase 3 の目標。**すべて同一環境・同一ページでの中央値**で
評価する。

| # | 目標 | 現在値 | 目標値 | 根拠 |
| --- | --- | ---: | ---: | --- |
| T1 | 起動〜load の優位を**維持**する | Chromium 比 -22〜-32% | **Chromium より速い状態を維持** (目安 -20%) | 既に勝っている領域を最適化で失わないことが最優先。回帰ゲート (#72) の対象 — **判定方式は §10 で確定**。単発の測定値で -20% を割ったことを回帰と判定してはならない (§10 参照) |
| T2 | メモリ (PSS) で Chromium と**同等**まで詰める | Chromium 比 +45.4% (1 タブ)、タブ数が増えるほど拡大 (20 タブで +246.2%、§11・`docs/memory-analysis.md` §10.6) | **Chromium 比 +10% 以内** | 「低メモリ」を名乗る最低条件。**#61 で主因を特定 (webview/`WebContext` の使い方、エンジン差ではない) → #118 で `WebContext` 共有 (`NetworkProcess` 統合、-2.5〜-11%) → #124 で `with_related_view` によるタブ間 `WebKitWebProcess` 共有 (最大 4 タブ/プロセス、読み込み中のプロセスには相乗りしない) を実装し、5/10/20 タブで -16.8/-22.6/-25.6% (D54)。1 タブ時は共有相手が無く変化なし。T2 は未達だが、残りの超過は「同一プロセス内でもページ 1 枚あたり 54 MiB」であり、次の一手は非表示タブのリソース解放 (#63 Adaptive Tab Suspension) — `docs/memory-analysis.md` §10.6 参照** |
| T3 | `startup_toolbar_ready_ms` を短縮する | 528.4ms | **300ms 以下** | ⚠️ **保留**。当初「この区間は VeloX 自身のコードでエンジン差ではないから確実に手が出せる」と設定したが、**#59 の実測でこの前提は誤りと判明した** (支配的なのは tao/GTK の初期化と WebKitGTK の webview 生成)。目標値は据え置くが、達成手段は現時点で不明。§9 参照 |
| T4 | 20 タブ時に操作不能な遅延を出さない | 未測定 | タブ切替 median **100ms 以下** | #60 の受け入れ条件。まず計測手段が必要 |

> **⚠️ この節の当初の記述は #59 の実測で覆っている。**
>
> #58 の時点では「T3 が Phase 3 で最初に着手すべき項目である。エンジン差の影響を
> 受けず、VeloX のコードだけで改善でき、かつ内訳上いちばん大きい」と書いていた。
> **これは内訳を細分化する前の推測であり、誤りだった。** #59 が
> `window_created → toolbar_ready` を 3 区間に分解して実測した結果、この区間は
> `tao`/GTK のイベントループ初期化と WebKitGTK の最初の webview 生成が支配的で、
> **VeloX の Rust 側起動処理 (history/bookmarks の読み込み、`AppState` 構築) は
> 約 0.1ms、ツールバー自身の JS 実行はほぼ 0ms** だった。`toolbar.html` を 49KB
> から `<script>` 2 行に差し替えても変化しない。詳細は §9 と D43。
>
> **したがって Phase 3 で次に着手すべきは T2 (メモリ) である。** T3 は目標として
> 残すが、エンジン側のコストである以上、Epic #57 の「WebView をブラックボックス
> として扱う」原則の下では VeloX 側から短縮する手段が現時点で無い。

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

## 9. T3 の調査結果 (Issue #59, 2026-09-02)

**結論を先に**: `window_created → toolbar_ready` を細分化して実測した結果、
このギャップは `tao`/GTK のイベントループ初期化と、WebKitGTK が最初の
webview を生成する際のエンジン側コストが支配的で、**`toolbar.html` の内容量
にも VeloX の Rust 側起動処理にもほとんど依存しないことが実測で確認できた**。
49KB のフル `toolbar.html` を `<script>` 2 行だけの最小版に一時的に差し替えて
再計測しても、このギャップはほとんど変化しなかった（詳細・生データは
`docs/decisions.md` D43 を参照）。そのため **本 Issue では有効な最適化を
適用していない** — 効果がゼロと分かっている変更を数値のために入れることは
Epic #57 の「ベンチマークなしの最適化をしない」に反するため。

### 追加した計測

`browser::metrics::StartupTimestamps` に 2 つの中間チェックポイントを追加し
（`rust_setup_done`／`toolbar_script_started`）、`startup` イベントの JSON に
`rust_setup_done_ms`／`toolbar_script_started_ms` フィールドが増えた。
`velox-bench` の `MetricKey` にも対応する
`startup_rust_setup_done_ms`／`startup_toolbar_script_started_ms` を追加した。
計測オフ時のオーバーヘッドは増えていない（既存の `Option` パターンを維持、
D19）。

### VeloX 内部の同一セッション before/after (`velox-bench`, cold_startup, minimal.html, 各 10 試行)

計測を追加する前 (`e8f1250`, before) と、計測を追加した後 (after, 機能的な
実行パスの変更なし) を同一セッション内で比較。

| メトリクス | before median (ms) | after median (ms) | 変化率 |
| --- | ---: | ---: | ---: |
| `startup_window_created_ms` | 209.70 | 191.50 | -8.7% |
| `startup_rust_setup_done_ms` | (未計測) | 191.65 | — |
| `startup_toolbar_script_started_ms` | (未計測) | 378.60 | — |
| `startup_toolbar_ready_ms` | 397.50 | 380.20 | -4.4% |
| `startup_first_load_ms` | 426.35 | 404.70 | -5.1% |

`velox-bench compare --baseline before.json --candidate after.json` は
**回帰なし**（すべての既存メトリクスが閾値 10% 以内、`rss_*` も含む）。
`toolbar_ready` の -4.4% はセッション内変動の範囲内であり、計測追加による
実質的な改善ではない（実行パスを一切変えていないので当然の結果）。

新しく分かったのは内訳: `rust_setup_done`(191.65ms) は `window_created`
(191.50ms) とほぼ同時刻 — history/bookmarks/input_history の読み込みと
`AppState` 構築は無視できるコスト（約 0.1ms）。一方
`toolbar_script_started`(378.60ms) は `toolbar_ready`(380.20ms) とほぼ同時刻
— ツールバー自身の JS 実行はほぼ 0ms。つまり `window_created` →
`toolbar_ready` の約 190ms は、ほぼ全て `rust_setup_done →
toolbar_script_started` の区間（エンジンがツールバーのドキュメントをパース
し終えるまで）に集中している。この区間が `toolbar.html` のサイズに依存しな
いことは `docs/decisions.md` D43 に記載した実験（フル版 vs 最小版の比較）で
確認済み。

### 競合比較 (`compare_browsers.py`) の after — T1 が保たれているか

| ブラウザ | load 到達 median (ms, 7 試行) | PSS median (MiB) |
| --- | ---: | ---: |
| **VeloX** | **453.9** | 423.9 |
| Chromium | 560.6 | 330.3 |

VeloX は Chromium より **-19.0%** 速い（§4 の初回 baseline は -22.4%
〔minimal〕）。

**この測定値は当初 T1 に書いていた -20% の線をわずかに下回っている。** 本 Issue は
実行パスを一切変更していない（計測点を増やしただけ）ので、コード変更による回帰では
なく、セッション間のノイズと考えるのが自然である — このマシンは他セッションと共有
されており、並行するビルドの有無で ±10〜20% 程度変動する。

ただし「回帰ではない」と「目標を達成した」は別である。**-19.0% は -20% の線を
満たしていない**ので、ここでは達成とは書かない。むしろこの結果は、**単発の中央値と
固定閾値で T1 を判定する運用自体が、この環境では成立しないことを示している。**
#72 (Performance Regression Gate) は、複数回測定・統計処理・許容分散を前提に
設計する必要がある（#72 の本文にも同じ注意書きがある）。T1 の閾値はその設計が
決まった時点で見直すこと。優位そのもの（VeloX の方が明確に速い）は保たれている。

### T3 (`startup_toolbar_ready_ms` ≤ 300ms) は未達

**達成できなかった。** 実測により、このメトリクスを支配しているのは
VeloX 自身のコードではなく `tao`/GTK の初期化と WebKitGTK の webview 生成
コストであることが分かったため、本 Issue のスコープ（新規依存を避ける、
`unsafe` 原則禁止、correctness を壊さない）の中では安全に短縮する手段が
見つからなかった。参考実験として `LIBGL_ALWAYS_SOFTWARE=1
WEBKIT_DISABLE_COMPOSITING_MODE=1` を設定すると `rust_setup_done →
toolbar_script_started` がおよそ半分になったが、これは本評価環境が GPU を
持たないために発生する `DRI3`/`EGL` ネゴシエーション失敗のリトライコストを
スキップしているだけであり、実 GPU を持つ利用者の環境には当てはまらない
（`docs/decisions.md` D43 参照）。したがって本 Issue のコードには含めていない。

## 10. 回帰ゲートの判定方式 (Issue #72, 2026-09-02)

§9 で「単発の中央値と固定閾値で T1 を判定する運用自体が、この環境では成立
しない」と書いた問題への回答。**T1 の判定方式はここで確定する。**

### 追加で実測したノイズ

`cold_startup` を 10 試行 × 6 セット、**同一バイナリ・同一コミット
(`b81b1fc`)・コード変更なし**で連続実行し、セット間のばらつきを 2 通りの
見方で計測した（生データ・コマンドは `docs/decisions.md` D46 参照）。

- **隣接セット同士の比較**（CI の同一ジョブ内比較が近似する条件）:
  `startup_first_load_ms` 最大 19.0%、`startup_toolbar_ready_ms` 最大
  16.0%、`pss_total_bytes` 最大 28.8%。
- **セット 1 とセット 6 の比較**（約 9 分離れた測定。古い baseline
  ファイルとの比較が近似する条件）: `page_load_ms` +77.0%/+78.9%、
  `startup_toolbar_ready_ms` +52.2%/+53.4%、`startup_first_load_ms`
  +49.2%/+52.4%。**同一バイナリでここまで動く。**

この 2 つ目の数字が、**固定 baseline ファイルとの比較を CI のブロッキング
判定に使ってはならない**という結論の直接の根拠である。

### 採用した判定方式

`src/browser/benchmark.rs` の `evaluate_gate`（純粋ロジック、`cargo test`
で境界値・同着・試行数不足・baseline 欠損を含めて検証済み）が実装する:

1. **2 段階の重大度**: `warn_pct`(既定 20%) と `fail_pct`(既定 60%)。
   `fail_pct` は隣接セットの最大ノイズ (28.8%) に約 31pt、セット 1/6 間の
   最悪ノイズ (53.4%、絶対値フロアで吸収できないメトリクスに限る) にも
   約 7pt のマージンを残す。
2. **メトリクスごとの最小絶対差**: 相対閾値だけでは吸収できない
   (`page_load_ms` の +78.9% は絶対では 16.2ms の変化でしかなかった)。
3. **複数候補測定の多数決**: CI では候補 (PR head) を 2 回測定し、**両方**
   が `fail_pct` を超えたときのみ Fail とする — 1 回だけの悪化は Warn に
   留める。
4. **試行数不足の検出**: baseline/candidate のいずれかが 5 試行未満なら
   `low_confidence` を立て、Fail への昇格を禁止する。

固定閾値 1 本 + 単発比較という素朴な方式を採らなかった理由は、上記の実測
そのもの — セット 1/6 間で無変更のバイナリが 50〜79% 動く環境では、固定
閾値だけでは「ノイズか回帰か」を区別できない。

### CI での運用: baseline は都度その場で作る

**`results/baseline/cold_startup-linux-xvfb.json`（この dev/agent コンテナ
で採取、§7 の形式で commit 紐付け済み）は、CI のブロッキング判定には
使わない。** 上記のとおり、機械が変わる/セッションが変わる比較はこの
環境で意味をなさないと実測で分かっているため。**代わりに
`.github/workflows/perf-gate.yml` は、PR の merge-base コミットと PR
head コミットを同一ジョブ内で連続してビルド・計測し、その場で作った
baseline と candidate を `velox-bench gate` に渡す。** これにより機械差・
セッション差そのものを比較から除去する。コミット済みの baseline
ファイルは、経時トレンドを人が目視で追うための参考情報として残す
(`docs/benchmarking.md` 参照)。

### 結果として T1 はどう判定されるか

- **CI (ブロッキング)**: 上記の `evaluate_gate` — Chromium との比較ではなく
  **VeloX 自身の PR head vs merge-base** の `startup_*`/`page_load_ms` を
  対象にする。Chromium 比 -20% という数値自体は目安として §6 に残すが、
  Chromium バイナリを CI に持ち込む仕組みはまだ無い (#58 の競合比較は手動
  実行のまま) ため、**CI が自動でブロックするのは「Chromium との差」では
  なく「直前の VeloX からの回帰」である。**
- Chromium との相対優位 (§4〜§5) は、引き続き手動での `compare_browsers.py`
  実行で確認する。自動化は将来 Issue の対象。

### CI で GUI ベンチを回すかどうか

**回す。** Xvfb でこの環境の `velox-bench run` が実測できることは #106 /
本 Issue で確認済みで、GUI を回さない代替 (純粋ロジックのマイクロベンチ等)
では `startup_*`/`page_load_ms` という T1 が問題にしている指標そのものを
測れない。コストは同一ジョブ内で 2 回ビルド + 3 回計測 (baseline 1 回 +
candidate 2 回) が掛かるが、`ci.yml` の本体 CI とは別ワークフローに分離
してあるため、通常の fmt/clippy/test/build のフィードバック速度には影響
しない。

## 11. T2 (メモリ) の切り分け結果 (Issue #61, 2026-09-03)

**詳細な測定データ・再現手順は [docs/memory-analysis.md](memory-analysis.md)
を参照。ここでは§5〜6 の「なぜ負けているか / T2 は達成可能か」に対応する
結論だけを記録する。**

**結論を先に**: §5 の「VeloX 側のオーバーヘッドなのか、WebKitGTK と Blink
の差なのか」という問いに対する答えは、**主因は VeloX 自身の実装**だった。
実測の柱は 3 つ:

1. **プロセス別 PSS 内訳**: VeloX (1 タブ) は `velox` 本体 (約 70.5 MiB) +
   `WebKitWebProcess`×2 (約 310 MiB) + `WebKitNetworkProcess`×2 (約 35.5
   MiB) の 5 プロセス、合計約 416 MiB。webview が 1 タブしかないのに
   `WebKitWebProcess`/`WebKitNetworkProcess` が 2 個ずつあるのは、VeloX が
   toolbar 用と content 用の 2 つの webview を常に同時に持つ設計
   (`docs/architecture.md` D3) の帰結。
2. **VeloX 自身の Rust heap (`heaptrack` 実測)**: 32.08 MiB で、ページの
   重さ (`minimal.html`/`dom_heavy.html`) にもタブ数 (1〜5 タブ) にも
   依存せず一定。ツリー全体 PSS の 1〜8% に過ぎず、削っても全体にはほぼ
   効かないことを確認した。
3. **エンジンだけの比較**: VeloX の Rust コードを一切含まない、webview 1
   個だけの最小 WebKitGTK C アプリを書いて計測すると、合計 PSS は約
   296〜299 MiB — これは **Chromium (318〜328 MiB) より軽い**。WebKitGTK
   というエンジン自体が Blink より PSS で重いという証拠はこの環境では
   見つからなかった。VeloX (415〜420 MiB) との差 (約 116〜125 MiB) は、
   §4.3 で計測した「webview 1 個あたりの増分 (約 92〜122 MiB)」とほぼ
   同じ大きさで、toolbar 用の 2 個目の webview 1 個分にほぼ対応する。

**タブ数を増やすとさらに悪化する**: 1/5/10/20 タブで計測すると、VeloX の
1 タブあたりの PSS 増分 (約 106 MiB/タブ) は Chromium (約 9.9 MiB/タブ) の
約 10.7 倍で、20 タブでは VeloX は Chromium の約 5.2 倍の PSS になる。原因
はプロセス数の増え方の違い (VeloX は追加タブ 1 個ごとに正確に +2 プロセス、
Chromium は概ね +1 プロセス) と一致しており、ソースコード上も VeloX が
webview を作るたび (`wry::WebViewBuilder::new()`) に `.web_context(...)`
で既存の `WebContext` を共有させておらず、毎回新しい `WebContext` を
作っていることと符合する (wry 0.56.1 の WebKitGTK バックエンドは
`attributes.context` が渡されなければ新しい `WebContext` を作り、
WebKitGTK は `WebContext` ごとに独立した `WebProcess`/`NetworkProcess` の
プールを持つ)。

**T2 (+10% 以内) は「達成不能」ではない。** #59/T3 (D43) とは逆の結果に
なった — あちらは「VeloX 側で手が出せるはず」の区間が実測するとエンジン
側だったが、こちらは「エンジン差だろう」と予想されていた超過分の大半が、
実測すると VeloX 自身の webview/`WebContext` の使い方に起因していた。
toolbar/content 間で `WebContext` を共有する変更は Epic #57 の「エンジンを
ブラックボックスとして扱う」原則に反しない (WebKit の内部ではなく、
wry への webview の作り方の話)。ただし**これは実装・計測していない仮説で
あり**、実際に効果があるかは #62 で実装して計測するまで分からない
(`docs/decisions.md` D48 に条件を記録した)。

**そのまま使えなかった既存の仕組み**: `velox-bench run --scenario
tabs_1/5/10/20` の `pss_total_bytes` はタブ数によらずほぼ一定 (134〜147
MiB) という、明らかに誤った値を返すことが分かった。原因は RSS/PSS サンプラ
(既定間隔 5000ms、起動直後から即座にサンプリングを開始) がシナリオ全体の
所要時間 (3〜4 秒程度で終わることが多い) より長い間隔で動いているため、
「起動直後の 1 回目」の値しか記録に残らないこと。本 Issue では代わりに
`scripts/bench/tab_scaling.py` を新規に書いて計測した
(`docs/memory-analysis.md` §4.1)。

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

ページロードの内訳 (DNS/TLS/レンダリング)、バッテリー/
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
| T2 | メモリ (PSS) で Chromium と**同等**まで詰める | Chromium 比 +45% (1 タブ)。**自動休止を有効にすると** 10/20 タブで +14% / +20% (`VELOX_MAX_LIVE_TABS=4`)、既定 (無効) では +171% / +245% (§12・`docs/memory-analysis.md` §11) | **Chromium 比 +10% 以内** | 「低メモリ」を名乗る最低条件。**#61 で主因を特定 → #118 で `WebContext` 共有 (-2.5〜-11%) → #124 で `WebKitWebProcess` 共有 (5/10/20 タブで -17/-23/-26%、D54) → #63 で Adaptive Tab Suspension (D56): 空にできるプロセスグループを丸ごと休止する適応ポリシーで、有効時は 20 タブ -65% (1612 → 560 MiB、Chromium 比 +245% → +20%)。既定は無効 (D9) のため、T2 の「現在値」は設定次第。残りは 1 タブ時の toolbar 用 `WebProcess` (+45%) と生存タブ分 — §12 参照**
| T3 | `startup_toolbar_ready_ms` を短縮する | 528.4ms | **300ms 以下** | ⚠️ **保留**。当初「この区間は VeloX 自身のコードでエンジン差ではないから確実に手が出せる」と設定したが、**#59 の実測でこの前提は誤りと判明した** (支配的なのは tao/GTK の初期化と WebKitGTK の webview 生成)。目標値は据え置くが、達成手段は現時点で不明。§9 参照 |
| T4 | 20 タブ時に操作不能な遅延を出さない | **20 タブでタブ切替 0.40ms** (`tab_switch_20`、48 サンプル)、休止タブへの切替 `tab_resume` 2.7ms + 再読み込み 10ms | タブ切替 median **100ms 以下** | ✅ **達成** (#60、§13)。目標を 2 桁下回る。ただしこの指標はメインスレッドのハンドラが返るまでで、描画完了までではない (D57 の「残る限界」) |

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

## 12. T2 — Adaptive Tab Suspension の結果 (Issue #63, 2026-09-04)

**詳細な測定データ・再現手順は [docs/memory-analysis.md](memory-analysis.md)
§11 を、設計判断は `docs/decisions.md` D56 を参照。**

`browser::suspension` に 3 つの独立したシグナル (アイドル時間
`VELOX_AUTO_SUSPEND_AFTER_MS` / 生存タブ上限 `VELOX_MAX_LIVE_TABS` / メモリ
予算 `VELOX_MEMORY_BUDGET_MB`) を持つ適応ポリシーを実装した。**既定はすべて
無効** (D9 のオプトインを維持) で、その場合の挙動と数値は before と同一
(回帰ゲート 3 シナリオ OK、`tab_scaling.py` も誤差範囲)。

| タブ数 | before (MiB) | `VELOX_MAX_LIVE_TABS=4` | `VELOX_MEMORY_BUDGET_MB=700` | Chromium (MiB) | Chromium 比 (上限 4) |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1  | 409.0  | 409.3  | 407.3  | 281.9 | +45.2% |
| 5  | 653.6  | 553.7  | 659.6  | 320.3 | +72.9% |
| 10 | 990.6  | 419.8  | 476.2  | 366.9 | **+14.4%** |
| 20 | 1612.1 | 560.4  | 615.1  | 466.7 | **+20.1%** |

**本 Issue で分かった、今後の最適化すべてに効く事実**: 同一 `WebKitWebProcess`
内で webview を破棄しても、解放されたヒープはプロセスに残り PSS はほとんど
下がらない (生存 3 ページで 496 MiB の例)。**メモリが確実に OS へ返るのは
プロセスが終了したときだけ**なので、休止はタブ単位ではなくプロセスグループ
単位で行う (空にできる LRU グループを丸ごと落とす) のが正しく、v2 → v3 で
20 タブ 904 → 560 MiB の差になった。これは D54 の「案 A (全タブ 1 プロセス)
でもページ 1 枚あたり 54 MiB」の観測とも整合する: 1 プロセスに集めるほど、
そのプロセスは終了できなくなる。

**復帰コスト** (`tab_resume` シナリオ、`minimal.html`): webview の再構築
2.7ms (中央値) + 再読み込み 10.1ms。D54 の共有プロセスに related view と
して作り直すため、新プロセスの起動を伴わない。

**T2 は未達** (+10% 以内に対し、最良で 10 タブ +14% / 20 タブ +20%)。残りは
(1) 1 タブ時の toolbar 用 `WebProcess` (+45%、休止では届かない)、(2) 生存
タブ分 (上限を下げるほど減るが復帰が増える)、(3) 本計測が `minimal.html`
であること (実サイト未評価)。D56 の Revisit condition を参照。

## 13. タブ生成・切替のベースラインとボトルネック (Issue #60, 2026-09-05)

**設計判断と考察は `docs/decisions.md` D57 を参照。** ここでは数値だけ記録する。

### 13.1 ベースライン (既定設定、`minimal.html`、各 6 試行 = 48 サンプル)

`velox-bench run --scenario tab_create_N|tab_switch_N` で、**すべてのサンプルを
タブ数 N ちょうどで**採ったもの (`mark` による warm-up 切り捨て、D57)。

| タブ数 | `tab_create_ms` | `tab_switch_ms` | `page_load_ms` (新しいタブ) | プロセスに空きがあるか |
| ---: | ---: | ---: | ---: | --- |
| 1  | 2.00 | 0.10 | 6.95  | あり |
| 5  | 2.25 | 0.50 | 6.00  | あり |
| 10 | 2.20 | 0.50 | 6.70  | あり |
| 20 | 2.60 | 0.40 | **14.60** | **無し (20 は上限 4 の倍数)** |

`tab_switch_1` の 0.10ms は「自分自身への切替」で、下限の目安。

### 13.2 ボトルネック: web プロセスを起こすかどうか

20 タブでの `VELOX_MAX_TABS_PER_PROCESS` スイープ (各 3 試行):

| 上限 | グループ構成 | `page_load_ms` | `tab_create_ms` | プロセス数 |
| ---: | --- | ---: | ---: | ---: |
| 4  | 4×5 (満杯)      | 15.8 | 2.75 | 8 |
| 5  | 5×4 (満杯)      | 15.1 | 2.90 | 8 |
| 6  | 6+6+6+2         | 7.3  | 2.15 | – |
| 7  | 7+7+6           | 9.2  | 2.00 | 6 |
| 8  | 8+8+4           | 7.8  | 2.10 | – |
| 9  | 9+9+2           | 6.5  | 2.00 | 6 |
| 10 | 10+10 (満杯)    | 16.1 | 2.70 | 6 |
| 12 | 12+8            | 8.2  | 2.05 | – |

**上限 9 と 10 はプロセス数が同じ 6 なのに 6.5ms と 14.6〜16.1ms に分かれる。**
効いているのはプロセス数ではなく、新しいタブが既存プロセスに相乗りできるか
(空きがあるか) である。相乗りできれば 6.5〜9.2ms、新しいプロセスを起こすなら
14.6〜16.1ms と約 2 倍。

### 13.3 上限 8 との比較 (既定を変えなかった根拠)

| 指標 (20 タブ) | 上限 4 (既定) | 上限 8 | 判定 |
| --- | ---: | ---: | --- |
| PSS | 1652.0 MiB | 1543.7 MiB | 上限 8 が -6.6% |
| プロセス数 | 8 | 6 | 上限 8 が少ない |
| `tab_switch` シナリオ (バースト) `page_load_ms` | 17.6 | 19.8 | 誤差範囲 |
| `tab_resume` シナリオ `page_load_ms` | 7.6 | 8.5 | 誤差範囲 |

D54 が上限を設けた理由 (バースト時のページロード直列化) は、上限ではなく
「読み込み中のプロセスには相乗りしない」規則が抑えていることが確認できた。
それでも既定を 4 のままにした理由は D57 を参照。

### 13.4 再現手順

```sh
S=/path/to/scratch
cargo build --release
(cd scripts/bench/pages && python3 -m http.server 8731 &)
URL=http://127.0.0.1:8731/minimal.html
XV='xvfb-run -a --server-args=-screen 0 1280x900x24 dbus-run-session --'

# 13.1 ベースライン
for n in 1 5 10 20; do
  $XV target/release/velox-bench run --scenario tab_create_$n --trials 6 \
    --velox-bin target/release/velox --url $URL --output $S/create-$n.json
  $XV target/release/velox-bench run --scenario tab_switch_$n --trials 6 \
    --velox-bin target/release/velox --url $URL --output $S/switch-$n.json
done

# 13.2 上限スイープ (順序の影響を避けるため上限をラウンドロビンで回す)
for trial in 1 2 3; do for cap in 4 5 6 7 8 9 10 12; do
  VELOX_MAX_TABS_PER_PROCESS=$cap $XV target/release/velox-bench run \
    --scenario tab_create_20 --trials 1 --velox-bin target/release/velox \
    --url $URL --output $S/cap-$cap-$trial.json
done; done

# 13.3 メモリ
for cap in 4 8; do
  VELOX_MAX_TABS_PER_PROCESS=$cap $XV python3 scripts/bench/tab_scaling.py \
    --velox target/release/velox --page minimal.html --tab-counts 1,5,10,20 --trials 3
done
```

### 13.5 計測上の注意 (実際に踏んだ罠)

**同一バイナリ・同一スクリプトでも、実行順で結果が 3 倍変わる。** 最初の調査で
`page_load_ms` が 20 タブで 25.4ms と出たが、同じ設定を後から測ると 7.8ms
だった。差はタブ数ではなく「その run がその日の 1 本目か」で、ページキャッシュ
などのウォームアップが効いている。**条件をまとめて連続実行すると、最初に測った
条件だけが不当に遅く出る。** 条件はラウンドロビンで回し、1 回目は捨てること。

## 14. バックグラウンドタブの CPU (Issue #64, 2026-09-05)

**設計判断と考察は `docs/decisions.md` D58 を参照。**

### 14.1 実測 (各 3 試行、16 秒窓、`scripts/profile/cpu_usage.py`)

負荷源は `scripts/bench/pages/busy.html` (`requestAnimationFrame` ループ +
10ms タイマー + CSS アニメーションで実際に CPU を焼く)。1 コアを 100% と
した比率。

| 状態 | CPU | 内訳 |
| --- | ---: | --- |
| busy がアクティブタブ | **100.0〜101.2%** | WebProcess 92〜93% + `velox` 本体 7.7〜7.9% |
| busy がバックグラウンドタブ | **0.4〜0.6%** | WebProcess 0.3% |
| busy を休止 (#63) | 0.0〜0.2% | — |
| 静的ページ 2 タブ (対照) | 0.1% | — |

**バックグラウンド化だけで 99.4% 減る。** VeloX 側の実装によるものではなく、
タブ切替の `set_visible(false)` が GTK ウィジェットを hide し、WebKitGTK が
そのページを「隠れている」と扱う結果である (D58)。

### 14.2 止まってはいない (`?beacon=1` による外形計測)

| 状態 | ビーコン到達 | 元の間隔 |
| --- | ---: | --- |
| visible | 2.17 件/秒 | 500ms タイマー = 2 件/秒 |
| hidden  | 1.10 件/秒 | 約 1000ms に間引き |

タイマーは約 1/2 の頻度で回り続け、バックグラウンドからのネットワーク
リクエストも通る。`document.visibilityState` が `hidden` を返していることも
同時に確認できる。

### 14.3 2 つの計測手段の使い分け

| 手段 | 出せるもの | 注意 |
| --- | --- | --- |
| `scripts/profile/cpu_usage.py` | **絶対値** (「何 % 使っているか」) | VeloX の外から `/proc` を 2 点だけ読むので測定コストが被測定側に乗らない |
| `velox-bench run --scenario background_cpu` の `cpu_percent` | **回帰検知** (before/after 比較) | VeloX 自身のサンプラが /proc を歩くコストが乗る (この環境で数 %)。同一シナリオ同士なら打ち消し合う |

この環境での `background_cpu` の `cpu_percent` は中央値 5.45% (3 試行、
12 サンプル)。上表の 0.5% と食い違うのはサンプラ自身のコストで、**絶対値を
語るときは `cpu_usage.py` の数字を使うこと。**

### 14.4 再現手順

```sh
P=$PWD/scripts/bench/pages
XV='xvfb-run -a --server-args=-screen 0 1280x900x24 dbus-run-session --'

# 14.1 アクティブ / バックグラウンド
printf 'wait 60000\nquit\n' > /tmp/active.txt
printf 'open file://%s/busy.html\nwait 1500\nopen file://%s/minimal.html\nwait 60000\nquit\n' \
  $P $P > /tmp/bg.txt
VELOX_MAX_TABS_PER_PROCESS=1 $XV python3 scripts/profile/cpu_usage.py \
  --velox target/release/velox --script /tmp/active.txt \
  --homepage file://$P/busy.html --label active --window-secs 16
VELOX_MAX_TABS_PER_PROCESS=1 $XV python3 scripts/profile/cpu_usage.py \
  --velox target/release/velox --script /tmp/bg.txt \
  --homepage file://$P/minimal.html --label background --window-secs 16

# 14.3 回帰検知用のシナリオ
(cd scripts/bench/pages && python3 -m http.server 8731 &)
$XV target/release/velox-bench run --scenario background_cpu --trials 3 \
  --velox-bin target/release/velox --url http://127.0.0.1:8731/busy.html
```

## 15. バックグラウンドタブのネットワーク活動 (Issue #65, 2026-09-07)

**設計判断と考察は `docs/decisions.md` D80 を参照。** 測定環境は §1 のとおり
(このリポジトリの Linux/WebKitGTK 環境。Windows/macOS の実力値ではない)。

負荷源は `scripts/bench/pages/network_activity.html` (新規、Issue #65)。
サーバ側のアクセスログで数える `scripts/profile/network_activity.py` を
使う — VeloX 自身にはリクエスト単位のイベントが存在しない理由は
スクリプト冒頭のコメントと D80 を参照。

### 15.1 典型的なポーリング間隔 (2 秒) — 各 3 試行、16 秒窓

| 状態 | polling (件/16s) | background resource (件/16s) | websocket (件/16s) | 合計 |
| --- | ---: | ---: | ---: | ---: |
| アクティブタブ | 5, 5, 5 | 4, 4, 4 | 6, 6, 6 | 15, 15, 15 |
| バックグラウンドタブ | 5, 6, 5 | 4, 4, 4 | 6, 7, 6 | 15, 17, 15 |

有意差なし。

### 15.2 高頻度ポーリング (`?poll_ms=200`) — 各 3 試行、8 秒窓

| 状態 | polling (件/8s) | background resource (件/8s) | websocket (件/8s) |
| --- | ---: | ---: | ---: |
| アクティブタブ | 31, 31, 31 | 2, 2, 2 | 4, 4, 4 |
| バックグラウンドタブ | 8, 8, 8 | 2, 2, 2 | 5, 5, 5 |

polling のみ 74% 減 (3.9→1.0 件/秒)。1 秒以上の間隔を持つ他 2 系統は
このケースでも間引かれていない。

### 15.3 prefetch — 全試行で 0 件

`GET /prefetch-target` は §15.1/§15.2 のどの試行 (アクティブ・
バックグラウンドとも計 12 試行) でも 1 件も記録されなかった。一方
「`<link rel=prefetch>` を挿入した直後」に無条件で送る `GET
/prefetch-armed` は毎回 1 件記録されている — スクリプト自体は実行された
が、WebKitGTK 2.52.6 がこの prefetch ヒントを実行しない。

### 15.4 Adaptive Tab Suspension (#63) との連携 — 各 3 試行、16 秒窓 (`--suspend-after-ms 3000`, `--settle-secs 8`)

| 状態 | 合計イベント数 (16s窓) |
| --- | ---: |
| バックグラウンド、suspension 無効 (§15.1 と同条件) | 15, 17, 15 |
| バックグラウンド、suspension 有効・休止後 | **0, 0, 0** |

### 15.5 再現手順

```sh
XV='xvfb-run -a --server-args=-screen 0 1280x900x24 dbus-run-session --'

# 15.1: 通常のポーリング間隔、アクティブ vs バックグラウンド
printf 'wait 16000\nquit\n' > /tmp/na_active.txt
printf 'wait 1500\nopen about:blank\nwait 16000\nquit\n' > /tmp/na_bg.txt
$XV python3 scripts/profile/network_activity.py --velox target/release/velox \
  --script /tmp/na_active.txt --label active --settle-secs 6 --window-secs 16
$XV python3 scripts/profile/network_activity.py --velox target/release/velox \
  --script /tmp/na_bg.txt --label background --settle-secs 6 --window-secs 16

# 15.2: 高頻度ポーリング (?poll_ms=200)
printf 'wait 10000\nquit\n' > /tmp/na_fast_active.txt
printf 'wait 1500\nopen about:blank\nwait 10000\nquit\n' > /tmp/na_fast_bg.txt
$XV python3 scripts/profile/network_activity.py --velox target/release/velox \
  --script /tmp/na_fast_active.txt --label fast_active --fixture-query "poll_ms=200" \
  --settle-secs 4 --window-secs 8
$XV python3 scripts/profile/network_activity.py --velox target/release/velox \
  --script /tmp/na_fast_bg.txt --label fast_background --fixture-query "poll_ms=200" \
  --settle-secs 4 --window-secs 8

# 15.4: suspension との連携 (idle_after を意図的に短くする)
printf 'wait 1500\nopen about:blank\nwait 26000\nquit\n' > /tmp/na_suspend.txt
$XV python3 scripts/profile/network_activity.py --velox target/release/velox \
  --script /tmp/na_suspend.txt --label suspended --settle-secs 8 --window-secs 16 \
  --suspend-after-ms 3000
## 16. メモリ/リソースライフタイム監査 (Issue #62, 2026-09-07)

**詳細な調査・実測データ・再現手順は [docs/memory-analysis.md](memory-analysis.md)
§12 を、判断の根拠は `docs/decisions.md` D79 を参照。要点のみここに残す。**

Issue #62 は #61/#63/#64 のどの計測にも無かった軸 — **タブ数を一定に保った
まま開閉「だけ」を繰り返す (churn) と PSS がじわじわ増えないか** — を新設の
`scripts/bench/tab_churn.py` で測った (`browser::automation` スクリプトで
「K 個開く→落ち着かせる→K 個とも閉じて 1 タブに戻す→落ち着かせる」を 1
ラウンドとして繰り返し、ラウンド境界ごとに PSS を採る)。

**コード上のリソースライフタイム監査で 1 件、本物のバグを見つけて修正した**:
`app::run` の `page_load_timers` (タブ latency 計測用マップ、`perf_metrics`
有効時のみ書き込まれる) が、タブを閉じてもエントリを一度も削除しておらず、
`WindowId`/`TabId` は再利用されないため**開いたタブの延べ数に比例して
無制限に増え続けていた**。`LoadFinished` でエントリを `remove` し、
`close_tab`/`close_window_by_tao_id` でも明示的に破棄するよう修正
(`src/app.rs`)。回帰ゲート (`cold_startup`/`tab_create`/`tab_switch`、
baseline 8 試行 + candidate 2×8 試行) は 3 シナリオとも総合判定 OK。ただし
このエントリ 1 件は高々 100 バイト程度で、本 Issue で実際に流せた churn の
規模 (最大 72 回の open/close) では `/proc` 経由の PSS/RSS 計測でこの修正の
効果を独立に検出できるだけの大きさにならなかった (このコンテナには
`heaptrack` が無く、malloc 単位の独立検証もできなかった) — 正しさの根拠は
単体/統合テストによる直接確認であり、PSS 計測で改善を実測したわけではない。

**churn そのものによる PSS 増加は観測されたが、無制限ではなく有界だった**:
`minimal.html`・6 タブ/ラウンド・12 ラウンド (のべ 72 回の open/close、42.1
秒) では round 1 (558.3 MiB) → round 2 (638.7 MiB) で跳ねた後、round 3〜12
は 581〜693 MiB の範囲で**増加が止まる** (round 3〜12 の最小二乗傾きは
-0.756 MiB/round)。プロセス数は全ラウンドを通じて一定 (4)。#63 (D56) が
「休止でも解放ヒープはプロセスに残り、プロセス終了だけが確実に OS へ返す」
と結論した現象が、休止だけでなく本当の tab close (相乗りしているプロセス
の一部だけ閉じる場合) にも及ぶことを示す一方、**アロケータのアリーナが
一度ピーク相当まで育った後は使い回されるだけで、無制限には育たない**
(いわゆる leak ではなく、有界な 1 回きりのウォームアップコスト) ことも
同時に確認できた。よって本 Issue ではプロセスグループの寿命ベース recycle
のような新規の設計変更には着手せず、D79 に Revisit condition として残した。

**「1 時間以上の連続利用」の外挿について**: 実際に流せたのは最大 15
ラウンド (60 回の open/close、約 39 秒) までで、1 時間相当 (1000+ ラウンド)
は本 Issue でも実測していない。`tab_churn.py` は実測区間の平均ラウンド
所要時間から単純な線形外挿も出力するが、実測データ自体が「数ラウンドで
頭打ちになる非線形な形」を示しているため、この線形外挿の数値そのものは
信頼できる予測として使わない — 出力にもその旨を明記している。

再現コマンド:

```sh
xvfb-run -a --server-args="-screen 0 1280x900x24" dbus-run-session -- \
  python3 scripts/bench/tab_churn.py --velox target/release/velox \
    --page minimal.html --rounds 12 --tabs-per-round 6 \
    --output results/tab-churn.json
```
## 17. Performance Dashboard (Issue #71, 2026-09-07)

**設計・実装の詳細は [docs/performance-dashboard.md](performance-dashboard.md)
を、設計判断は `docs/decisions.md` D82 を参照。** ここでは §10 (回帰ゲート)
との関係だけを記す。

`velox-bench gate` (§10) は CI が PR をブロックするかどうかを**同一ジョブ内で
測った baseline/candidate** から判定する。それに対し Performance Dashboard
(`scripts/dashboard/`) は、**過去の計測結果を `results/history/` に追記保存し、
人間が経時トレンドを一覧できる静的レポートを生成する**別の役目を持つ —
CI のブロッキング判定を置き換えるものではない。

保存形式は `velox-bench run`/`aggregate`/`gate` が使う `BenchmarkResult`
JSON をそのまま包んだものにしてあり (§7 の保存形式と地続き)、perf-gate の
出力形式とダッシュボードの保存形式が食い違わないようにしてある。

**この文書 §10/D46 が明らかにした「異なるセッション/マシンの数値を比較して
はならない」という原則は、ダッシュボードの設計そのものに組み込まれている**:
比較 (差分の計算・折れ線での接続) は明示的に同一とマークされた
「セッション」の中でだけ行われ、セッションを跨いだ点は時系列上に並べて
表示はするが、線ではつながず、差分も計算しない。判定ロジックそのものも
独自実装ではなく `velox-bench gate` (`evaluate_gate`) をそのまま呼び出す
ため、CI と同じ閾値 (`GateThresholds` 既定値 warn=20%/fail=60% + メトリクス
ごとの最小絶対差) が二重管理にならない。詳細は
`docs/performance-dashboard.md` §2/§5 を参照。
## 18. IPC (WebView ↔ Rust) の計測結果 (Issue #66, 2026-09-07)

**設計判断は `docs/decisions.md` D81 を参照。** ここでは実測データと
結論だけを記録する。この節の数値はすべて §1 の環境 (Ubuntu 24.04.4 /
WebKitGTK 2.52.6 / Xvfb、GPU なし) での計測であり、**Windows
(WebView2) の実力値ではない** — IPC の実装 (wry の `evaluate_script`/
`with_ipc_handler`) は OS 間でほぼ同じコードパスだが、未計測の OS へ
そのまま外挿しないこと。

### 18.1 何を計測できるようにしたか

`metrics::PerfRecord::Ipc` (Issue #66) が JS → Rust (`direction=in`、
`ToolbarCommand`) と Rust → JS (`direction=out`、
`ui::window::BrowserWindow::eval_toolbar` の呼び出し元名) の両方向を
1 メッセージ単位で記録する。`velox-bench ipc-summary` がそれを
`(direction, name)` ごとに件数・合計バイト数・`duration_ms` 分布へ集計する
(`docs/benchmarking.md` §6)。**`duration_ms` は Rust 側のコスト
(`parse_command`/`evaluate_script` の呼び出し) のみで、JS 実行や DOM
更新は含まない** — Epic #57 ルール 3 (WebView をブラックボックスとして
扱う) のとおり、VeloX 側から測れるのはここまで。

### 18.2 セッション実測 (`velox-bench ipc-summary`)

自動操作スクリプトでタブ 20 個 (`minimal.html`) を開き、10 回切替 + 2 回
ナビゲーション + 2 回クローズを行う「20 タブセッション」と、タブ 3 個で
同種の操作を行う「3 タブセッション」を 1 回ずつ実行 (各 1 試行、
`VELOX_MAX_TABS_PER_PROCESS=4`、既定)。再現手順は §18.5。

**20 タブセッション、修正前 (this issue 着手前のコード) の内訳
(上位 5、`total_bytes` 降順)**:

| dir | name | count | total_bytes | median_ms | p95_ms |
| --- | --- | ---: | ---: | ---: | ---: |
| out | `set_tabs` | 120 | 256,648 | 0.000 | 0.105 |
| out | `set_history` | 67 | 15,723 | 0.000 | 0.100 |
| out | `set_url` | 91 | 4,536 | 0.000 | 0.200 |
| out | `set_bookmark_active` | 91 | 2,730 | 0.000 | 0.100 |
| out | `set_loading` | 91 | 2,033 | 0.000 | 0.100 |
| — | 合計 | 502 | 284,268 | — | — |

`in` 側は `script_started`(24 bytes)/`ready`(15 bytes) の 2 件のみ —
16.4 で理由を説明する。

### 18.3 高頻度イベントの特定と結論

- **`set_tabs` (タブストリップの全件再送信) が量・回数とも最大**
  (20 タブセッションで合計バイト数の 90%)。1 タブ操作 (open/navigate/
  close/switch) につき最大 3 回 (`NavigationStarted`/`LoadFinished`/
  `FaviconResolved` それぞれが `sync_tab_strip` を呼ぶ) 送られており、
  120 件 / 約 34 回のタブ影響操作という比率もこれと整合する。**しかし
  実測コストは無視できる規模だった**: 20 タブという本プロジェクトで
  最も重いケースでも `duration_ms` の中央値は 0.000ms、p95 でも
  0.105ms、120 件中の最悪値でも 3.5ms (1 件のみ、他はすべて 2ms 未満)。
  1 メッセージの最大ペイロードも 3,585 bytes (20 タブ分の
  `TabSummary` 配列) で、JSON 化・`evaluate_script` 呼び出しという
  Rust 側の処理は sub-millisecond。タブストリップは**常時表示**の UI
  であり、3 回の送信はそれぞれ「読込中インジケータ」「URL/タイトル」
  「favicon」という実際に変化した状態を反映しているため、**意図的な
  設計であり、バッチ化・削減の実測上の必要性は見つからなかった。**
  T4 (タブ切替 100ms 以下、#60 で 0.40ms 達成済み) を脅かす要素はない。
- **`set_history` (履歴パネルの全件再送信) は不要イベントだった。**
  `refresh_history_panel` は履歴パネルが**閉じていても**
  `LoadFinished`/`PageTitleResolved`/`FaviconResolved` の 3 箇所から
  無条件に呼ばれており、閲覧のたびに `config.history_panel_limit`
  (既定 200 件) 分の履歴を JSON 化して送っていた。履歴パネルは
  ほとんどの時間閉じているため、この送信の大部分は**誰にも見られない
  DOM 更新**だった。§18.4 で before/after を示す。
- それ以外の `out` イベント (`set_url`/`set_bookmark_active`/
  `set_loading`/`set_block_count` など) は 1 件あたり数十バイト、
  `duration_ms` はほぼ 0 — 削減の対象にならない規模。

### 18.4 実施した削減と before/after (同一自動操作スクリプトでの比較)

`app::refresh_history_panel_if_open` を追加し、`LoadFinished`/
`PageTitleResolved`/`FaviconResolved` の 3 箇所を
`refresh_history_panel`(無条件) から `refresh_history_panel_if_open`
(`window.open_panel() == Some(Panel::History)` のときだけ実際に送信)
に変更した。パネルを開く操作 (`ToolbarCommand::TogglePanel`) は
既存のまま無条件に `refresh_history_panel` を呼ぶので、**パネルを
開いた瞬間に最新データが表示される挙動は変わらない** — 変わるのは
「閉じている間、誰も見ない更新を送り続けない」点のみ。

| セッション | 指標 | before | after | 変化 |
| --- | --- | ---: | ---: | ---: |
| 20 タブ | `set_history` 件数 | 67 | 1 | **-98.5%** |
| 20 タブ | `set_history` bytes | 15,723 | 555 | **-96.5%** |
| 20 タブ | ipc イベント総数 | 502 | 430 | -14.3% |
| 20 タブ | ipc 総バイト数 | 284,268 | 269,277 | -5.3% |
| 3 タブ | `set_history` 件数 | 13 | 1 | **-92.3%** |
| 3 タブ | `set_history` bytes | 7,614 | 887 | **-88.4%** |
| 3 タブ | ipc イベント総数 | 96 | 84 | -12.5% |
| 3 タブ | ipc 総バイト数 | 20,262 | 13,917 | -31.3% |

(`set_tabs` は before/after で変化なし — 120 件 → 120 件、256,648 →
257,023 bytes。誤差は自動操作スクリプトの実行ごとの URL/タイトル文字数の
揺れによるもので、意図的な変更はしていない。各 1 試行の実測であり、
セッション間ノイズ〔§10〕を統計的に切り分けられるほどの試行数ではない
ことに注意。)

`cargo test` は本変更後も全件成功 (946 ユニットテスト + 10 統合テスト、
xvfb-run + dbus-run-session)。履歴パネル自体の動作 (開いたときに最新の
履歴が表示される、検索、削除、クリア) は `ToolbarCommand::TogglePanel`
/`DeleteHistoryEntry`/`ClearHistory`/`SearchHistory` の呼び出し経路を
変更していないため影響を受けない。

### 18.5 結論

- **IPC の回数・サイズ・時間を継続的に計測できる仕組み**: `metrics::
  PerfRecord::Ipc` + `browser::benchmark::summarize_ipc` +
  `velox-bench ipc-summary` (`docs/benchmarking.md` §6)。#67/#68/#69 は
  このまま (追加の型を作らず) 使える。
- **高頻度イベントの特定**: `set_tabs`(常時表示、意図的、コスト無視できる)、
  `set_history`(閉じたパネルへの無駄な送信、削減済み)。
- **不要イベントの削減**: `set_history` の無条件送信を撤廃 (§18.4)。
- **batching の要否**: **導入しなかった。** `set_tabs` を含むすべての
  `out` イベントの `duration_ms` が sub-millisecond (worst case でも
  3.5ms) であり、複数イベントをまとめて 1 回の `evaluate_script`
  呼び出しに合成する batching は、計測上のボトルネックが無い状態で
  タイマー・デバウンスロジックという複雑さとステイル状態のリスクだけを
  持ち込む — Epic #57 ルール 1 (ベンチマークなしの最適化をしない) に
  照らして見送った。
- **`direction=in` (JS → Rust) の限界**: 自動操作スクリプト
  (`open`/`switch`/`navigate`/`close`) は `AutomationCommand` として
  `app.rs` のハンドラを直接呼ぶため、実際の `window.ipc.postMessage`
  を経由しない (`docs/decisions.md` D81 に理由を記録)。したがって
  `in` 側の実測は起動時ハンドシェイク (`ready`/`script_started`、
  15〜24 bytes、`duration_ms` は測定分解能以下) に限られる。**実際の
  ユーザ操作 (クリック・キー入力) が送る `navigate`/`activate_tab`
  等の `in` メッセージは `ui::toolbar::ToolbarCommand` の定義上、数十
  バイトの固定形状 JSON であることがソースコードから明らかであり**
  (例: `{"cmd":"activate_tab","id":42}` は 27 bytes)、`out` 側で
  実測した「同程度サイズのメッセージは sub-millisecond」という結果と
  合わせて、`in` 側もボトルネックである根拠は無いと判断した。ただし
  これは実測ではなく推論であることを明記する — オムニボックスの
  1 キー入力ごとの `omnibox_input`/`omnibox_close` 往復のような、
  自動操作スクリプトの文法 (`browser::automation`) では今のところ
  再現できない高頻度パスは**未計測**として残す (`docs/decisions.md`
  D81)。

### 18.6 再現手順

```sh
S=/path/to/scratch
cargo build --release
(cd scripts/bench/pages && python3 -m http.server 8731 &)
XV='xvfb-run -a --server-args=-screen 0 1280x900x24 dbus-run-session --'

cat > $S/session.txt << 'SCRIPT'
open http://127.0.0.1:8731/minimal.html
wait 300
(... 18 回繰り返して合計 20 タブ ...)
mark
switch 0
wait 200
(... switch/navigate/close を繰り返す ...)
quit
SCRIPT

$XV env VELOX_PERF_METRICS=1 VELOX_PERF_FORMAT=json \
  VELOX_PERF_OUTPUT=$S/session.jsonl \
  VELOX_HOMEPAGE=http://127.0.0.1:8731/minimal.html \
  VELOX_AUTOMATION_SCRIPT=$S/session.txt \
  target/release/velox

target/release/velox-bench ipc-summary --input $S/session.jsonl \
  --output $S/ipc-summary.json
```

## 21. Windows (WebView2) の実測 (Issue #136)

**設計判断・実装方針は `docs/decisions.md` D88 を参照。** この節は Windows
側の実測結果を記録する場所として用意した — **現時点ではまだ 1 つも数値が
入っていない。** 理由は D88 のとおり: この節を書いている環境は Linux
コンテナで Windows 実機が無く、`.github/workflows/perf-windows.yml`
(`workflow_dispatch` 限定) を実際に `windows-latest` ランナー上で実行して
初めて数値が取れる。以下は**その実行後に埋めるプレースホルダ**であり、
架空の数値は一切書いていない。

> ⚠️ **この節の数値を、§1〜§20 の Linux (WebKitGTK/Xvfb) の数値と直接比較
> しないこと。** OS が異なれば WebView 実装 (WebView2 vs WebKitGTK) も
> プロセスモデルも別物であり、Epic #57 絶対ルール5「OS ごとに結果を分ける」
> のとおり比較は成立しない。特に **PSS 相当は Windows では実装していない
> (D88)** — `pss_total_bytes` は Windows の結果には常に含まれず、
> `total_rss_bytes` のみが入る。将来 Windows 側に PSS 相当の値を追加した
> としても、Linux の PSS (`smaps_rollup` 由来) とは算出方法が全く異なるため
> 直接比較してはならない (D88 参照)。

### 21.1 測定環境 (`perf-windows.yml` 実行後に記入)

| 項目 | 値 |
| --- | --- |
| ランナー | `windows-latest` (GitHub-hosted) |
| OS ビルド番号 | *(TBD — `perf-windows.yml` の「実行環境の情報を記録」ステップのログ参照)* |
| CPU | *(TBD)* |
| メモリ | *(TBD)* |
| WebView2 Runtime バージョン | *(TBD)* |
| rustc | *(TBD)* |
| VeloX ビルド | `cargo build --release` (`velox.exe` / `velox-bench.exe`) |
| ディスプレイ | GitHub-hosted Windows ランナーの対話セッション (Xvfb 相当の仕組みは無い。GUI/WebView2 ウィンドウが起動できるか自体が未検証 — D88 参照) |

### 21.2 シナリオ別の結果 (`perf-windows.yml` 実行後に記入)

`velox-bench run` の `--scenario`/`--trials`/`--url` を workflow_dispatch の
入力パラメータとして指定した実行結果を、シナリオごとに追記していく。形式は
§4/§13 等の既存の節にならい、`velox-bench` の `metrics` キー
(`docs/benchmarking.md` 「計測される生データとの対応」表) をそのまま使う。

*(まだ実行結果が無いため、表は未作成。初回実行後、このセクションに
`cold_startup` を最初のシナリオとして追記する想定。)*

### 21.3 GUI 起動可否の検証結果 (`perf-windows.yml` 実行後に記入)

D88 が「最大のリスク」と位置付けた、`windows-latest` ランナー上で VeloX
(WebView2 ウィンドウ) が起動できるかどうかの結果をここに記録する。
`perf-windows.yml` の診断ステップ (`velox.exe` を直接起動し、プロセス一覧と
perf ログの有無を確認する) の結果を貼ること。

- 起動できた場合: 何回目の実行で確認できたか、`startup` perf レコードが
  実際に書けたかを記録する。
- 起動できなかった場合: 何を試して、どう失敗したか (プロセスが即終了した/
  ハングした/perf レコードが 1 件も書けなかった、等) を記録し、セルフホスト
  ランナー等の方式再検討が必要という結論をここに残す。**無理に通そうとした
  形跡 (数値の捏造・失敗の隠蔽) を残さないこと** — D88 および #59 が同じ
  方針を採っている。
